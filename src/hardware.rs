pub use crate::paths::RUN_DIR;
use crate::{
    ec::{self, EcIo, Fan, FileEc},
    platform,
};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
};

pub const EC_PATH: &str = "/sys/kernel/debug/ec/ec0/io";
pub const EC_ALIAS_PATH: &str = "/run/minibookx-ctl/hardware/debugfs/ec/ec0/io";
const SUPPORT: &str = "/sys/module/ec_sys/parameters/write_support";

fn prepare_private_alias(dir: &Path) -> io::Result<()> {
    let private = dir.join("hardware");
    match fs::create_dir(&private) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e),
    }
    let m = fs::symlink_metadata(&private)?;
    if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
        return Err(error("untrusted private hardware directory"));
    }
    fs::set_permissions(private, fs::Permissions::from_mode(0o700))
}

fn ec_path_at(private: &Path, canonical: &Path, owner: u32) -> io::Result<PathBuf> {
    let alias = private.join("debugfs");
    let mounted = match fs::symlink_metadata(&alias) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(canonical.join("ec/ec0/io")),
        Err(e) => return Err(e),
    };
    let parent = fs::symlink_metadata(private)?;
    if !parent.is_dir()
        || parent.uid() != owner
        || parent.mode() & 0o777 != 0o700
        || !mounted.is_dir()
    {
        return Err(error("untrusted private debugfs alias"));
    }
    let source = fs::symlink_metadata(canonical)?;
    if !source.is_dir() {
        return Err(error("debugfs source is not a directory"));
    }
    // A namespace bind has the source directory's identity. The empty mount
    // target left outside that namespace must never replace standalone recovery.
    if mounted.dev() == source.dev() && mounted.ino() == source.ino() {
        Ok(alias.join("ec/ec0/io"))
    } else {
        Ok(canonical.join("ec/ec0/io"))
    }
}

fn open_ec(writable: bool) -> io::Result<FileEc> {
    let path = ec_path_at(
        &Path::new(RUN_DIR).join("hardware"),
        Path::new("/sys/kernel/debug"),
        0,
    )?;
    FileEc::open(&path, writable)
}

fn error(s: &str) -> io::Error {
    io::Error::other(s)
}
fn root() -> io::Result<()> {
    // SAFETY: geteuid has no arguments or memory access requirements.
    if unsafe { libc::geteuid() } != 0 {
        return Err(error("system controller requires root"));
    }
    Ok(())
}
pub fn check_profile() -> io::Result<()> {
    let bios = fs::read_to_string("/sys/class/dmi/id/bios_version")?;
    let model = fs::read_to_string("/sys/class/dmi/id/product_name")?;
    let vendor = fs::read_to_string("/sys/class/dmi/id/sys_vendor")?;
    let cpu = fs::read_to_string("/proc/cpuinfo")?;
    if !platform::supported_profile(&vendor, &model, &bios, &cpu) {
        return Err(error("unsupported hardware/BIOS; refusing EC access"));
    }
    Ok(())
}
fn invocation() -> io::Result<String> {
    let id = env::var("INVOCATION_ID")
        .map_err(|_| error("run --prepare and --daemon through the systemd unit"))?;
    if !valid_invocation(&id) {
        return Err(error("invalid systemd invocation ID"));
    }
    Ok(id)
}
fn valid_invocation(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}
fn directory() -> io::Result<PathBuf> {
    let p = PathBuf::from(RUN_DIR);
    let m = fs::symlink_metadata(&p)?;
    if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
        return Err(error("untrusted runtime directory"));
    }
    Ok(p)
}
pub struct Lock(File);
impl Lock {
    fn take(dir: &Path) -> io::Result<Self> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("control.lock"))?;
        // SAFETY: fd belongs to a live File; flock changes its advisory lock only.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(f))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: this object still owns the fd.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// One shared ownership guard for every cooperating hardware write. Created
/// after both children fork; sensor cleanup retains it even if EC setup fails.
#[derive(Clone)]
pub struct Control(Rc<ControlState>);
struct ControlState {
    dir: PathBuf,
    id: String,
    _lock: Lock,
}
impl Control {
    pub fn open() -> io::Result<Self> {
        root()?;
        check_profile()?;
        let dir = directory()?;
        let lock = Lock::take(&dir)?;
        require_no_unowned_records(&dir)?;
        for name in ["module.lease", "sensor.lease"] {
            match fs::symlink_metadata(dir.join(name)) {
                Ok(_) => return Err(error("pending hardware ownership requires recovery")),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
        }
        Ok(Self(Rc::new(ControlState {
            dir,
            id: invocation()?,
            _lock: lock,
        })))
    }
    pub fn directory(&self) -> &Path {
        &self.0.dir
    }
    pub fn invocation(&self) -> &str {
        &self.0.id
    }
}
struct Lease {
    id: String,
    fan_control: bool,
    was_loaded: bool,
    was_writable: bool,
}
impl Lease {
    fn encode(&self) -> String {
        format!(
            "v2\n{}\n{}\n{}\n{}\n",
            self.id, self.fan_control, self.was_loaded, self.was_writable
        )
    }
    fn read(path: &Path) -> io::Result<Self> {
        let s = fs::read_to_string(path)?;
        let v: Vec<_> = s.lines().collect();
        let flag = |s: &str| s.parse::<bool>().map_err(|_| error("invalid lease flag"));
        let lease = match v.as_slice() {
            // The old service always prepared EC access before creating its lease.
            ["v1", id, loaded, writable] => Self {
                id: (*id).into(),
                fan_control: true,
                was_loaded: flag(loaded)?,
                was_writable: flag(writable)?,
            },
            ["v2", id, fan, loaded, writable] => Self {
                id: (*id).into(),
                fan_control: flag(fan)?,
                was_loaded: flag(loaded)?,
                was_writable: flag(writable)?,
            },
            _ => return Err(error("invalid module lease")),
        };
        if !valid_invocation(&lease.id) {
            return Err(error("invalid module lease invocation ID"));
        }
        if (!lease.fan_control && (lease.was_loaded || lease.was_writable))
            || (lease.was_writable && !lease.was_loaded)
        {
            return Err(error("inconsistent module lease"));
        }
        Ok(lease)
    }
}
fn create_private(path: &Path, text: &str) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    f.write_all(text.as_bytes())
}
fn modprobe(remove: bool) -> io::Result<()> {
    let mut c = Command::new(platform::system_tool("modprobe")?);
    if remove {
        c.arg("-r");
    }
    c.arg("ec_sys");
    if !remove {
        c.arg("write_support=1");
    }
    if !c.status()?.success() {
        return Err(error("modprobe ec_sys failed"));
    }
    Ok(())
}
pub fn prepare() -> io::Result<()> {
    root()?;
    check_profile()?;
    prepare_private_alias(&directory()?)?;
    // systemd retries this same unit after a failed start or runtime fault.
    // Restore any attributable orphaned state before creating a new lease.
    recover()?;
    for module in ["uinput", "i2c-dev"] {
        if !Command::new(platform::system_tool("modprobe")?)
            .arg(module)
            .status()?
            .success()
        {
            return Err(error("required stock input/I2C module is unavailable"));
        }
    }
    eprintln!("prepared known U300 input/I2C prerequisites; fan mode awaits policy");
    Ok(())
}
pub struct Session<T: EcIo = FileEc> {
    pub fan: Option<Fan<T>>,
    dir: PathBuf,
    id: String,
    _control: Control,
}
impl Session<FileEc> {
    pub fn open(fan_control: bool) -> io::Result<Self> {
        Self::open_locked(fan_control, Control::open()?)
    }
    pub fn open_locked(fan_control: bool, control: Control) -> io::Result<Self> {
        let dir = control.directory().to_owned();
        let id = control.invocation().to_owned();
        let fan = open_leased_fan(
            &dir,
            &id,
            fan_control,
            || {
                let loaded = Path::new(SUPPORT).exists();
                let writable = loaded && fs::read_to_string(SUPPORT)?.trim() == "Y";
                Ok((loaded, writable))
            },
            |loaded| {
                if !loaded {
                    modprobe(false)?;
                }
                let baseline = open_ec(false).map_err(|e| {
                    error(&format!(
                        "fan control requires ec_sys and mounted debugfs at {EC_PATH}: {e}"
                    ))
                })?;
                Fan::new(baseline).baseline()?;
                if fs::read_to_string(SUPPORT)?.trim() != "Y" {
                    fs::write(SUPPORT, b"Y\n")?;
                }
                let fan = Fan::new(open_ec(true)?);
                fan.baseline()?;
                Ok(fan)
            },
        )?;
        Ok(Self {
            fan,
            dir,
            id,
            _control: control,
        })
    }
}

/// Capture module ownership before any EC setup. Disabled fan control must not
/// even inspect the module or debugfs interface, including if they are present.
fn open_leased_fan<T: EcIo>(
    dir: &Path,
    id: &str,
    fan_control: bool,
    module_baseline: impl FnOnce() -> io::Result<(bool, bool)>,
    prepare_fan: impl FnOnce(bool) -> io::Result<Fan<T>>,
) -> io::Result<Option<Fan<T>>> {
    let (was_loaded, was_writable) = if fan_control {
        module_baseline()?
    } else {
        (false, false)
    };
    let lease = Lease {
        id: id.into(),
        fan_control,
        was_loaded,
        was_writable,
    };
    create_private(&dir.join("module.lease"), &lease.encode())?;
    if fan_control {
        prepare_fan(was_loaded).map(Some)
    } else {
        Ok(None)
    }
}

impl<T: EcIo> Session<T> {
    pub fn activate(&mut self) -> io::Result<()> {
        let Some(fan) = &mut self.fan else {
            return require_no_manual_owner(&self.dir);
        };
        fan.baseline()?;
        let marker = self.dir.join("manual.owner");
        match fs::read_to_string(&marker) {
            Ok(id) if id.trim() == self.id => (),
            Ok(_) => return Err(error("foreign manual owner marker")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                create_private(&marker, &(self.id.clone() + "\n"))?
            }
            Err(e) => return Err(e),
        }
        let result = fan.activate();
        if result.is_err() && !fan.may_be_manual() {
            fs::remove_file(marker)?;
        }
        result
    }

    /// A successful release ends fan ownership, including across suspend.
    /// Keep the marker when automatic-mode verification or removal fails.
    pub fn restore(&mut self) -> io::Result<()> {
        let Some(fan) = &mut self.fan else {
            return require_no_manual_owner(&self.dir);
        };
        let marker = self.dir.join("manual.owner");
        let owned = match fs::read_to_string(&marker) {
            Ok(id) if id.trim() == self.id => true,
            Ok(_) => return Err(error("foreign manual owner marker")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        fan.restore()?;
        if owned {
            fs::remove_file(marker)?;
        }
        Ok(())
    }
}
impl<T: EcIo> Drop for Session<T> {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            eprintln!("fan ownership release failed in Drop: {e}");
        }
    }
}
/// Root-only orphan recovery. The exclusive lock proves no cooperating runtime
/// is alive; persistent root-owned lease/marker identify this controller's state.
pub fn recover() -> io::Result<()> {
    cleanup_owned(None)
}
fn cleanup_owned(expected_id: Option<String>) -> io::Result<()> {
    root()?;
    let dir = match directory() {
        Ok(p) => p,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let _lock = Lock::take(&dir)?;
    // This journal may precede module.lease, including when sensor wake or EC
    // setup failed. It is an independent domain even if another lease is bad.
    recover_independently(
        || cleanup_module(&dir, expected_id.as_deref()),
        || crate::tablet::recover_sensor(&dir, expected_id.as_deref()),
    )
}

fn recover_independently(
    other_hardware: impl FnOnce() -> io::Result<()>,
    sensor: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    // Give thermal/power restoration priority over a potentially slow I2C
    // transfer, but always attempt the sensor even when another domain fails.
    let first = other_hardware();
    let second = sensor();
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(second)) => Err(error(&format!("{first}; {second}"))),
    }
}

fn cleanup_module(dir: &Path, expected_id: Option<&str>) -> io::Result<()> {
    let lease = match Lease::read(&dir.join("module.lease")) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            require_no_unowned_records(dir)?;
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    if expected_id.is_some_and(|id| lease.id != id) {
        eprintln!("foreign lease: cleanup performs no writes");
        return Ok(());
    }
    recover_components(
        || {
            if lease.fan_control {
                restore_owned_fan(dir, &lease.id)
            } else {
                require_no_manual_owner(dir)
            }
        },
        crate::power::restore_journal,
        || {
            if lease.fan_control {
                if Path::new(SUPPORT).exists() {
                    fs::write(SUPPORT, if lease.was_writable { b"Y\n" } else { b"N\n" })?;
                }
                if !lease.was_loaded
                    && let Err(e) = modprobe(true)
                {
                    eprintln!("auto restored; leaving busy ec_sys loaded: {e}");
                }
            }
            fs::remove_file(dir.join("module.lease"))?;
            write_status("{\"active\":false,\"manual\":false}\n")
        },
    )
}

fn require_no_manual_owner(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir.join("manual.owner")) {
        Ok(_) => Err(error(
            "manual fan ownership conflicts with a fan-disabled session; retain records for recovery",
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn require_no_unowned_records(dir: &Path) -> io::Result<()> {
    for name in ["manual.owner", "power.lease"] {
        match fs::symlink_metadata(dir.join(name)) {
            Ok(_) => {
                return Err(error(
                    "hardware recovery record has no module ownership lease; retain records for review",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Independent hardware domains must not prevent each other's recovery.
/// Keep the module lease and write access for a retry if either domain fails.
fn recover_components(
    restore_fan: impl FnOnce() -> io::Result<()>,
    restore_power: impl FnOnce() -> io::Result<()>,
    release_module: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let fan = restore_fan();
    let power = restore_power();
    match (fan, power) {
        (Ok(()), Ok(())) => release_module(),
        (Err(fan), Ok(())) => Err(error(&format!("fan recovery failed: {fan}"))),
        (Ok(()), Err(power)) => Err(error(&format!("power recovery failed: {power}"))),
        (Err(fan), Err(power)) => Err(error(&format!(
            "fan recovery failed: {fan}; power recovery failed: {power}"
        ))),
    }
}

fn restore_owned_fan(dir: &Path, id: &str) -> io::Result<()> {
    let marker = dir.join("manual.owner");
    if marker.exists() {
        if fs::read_to_string(&marker)?.trim() != id {
            return Err(error("foreign manual owner: cleanup refused"));
        }
        check_profile()?;
        let mut result = Err(error("restore not attempted"));
        for _ in 0..3 {
            result = open_ec(true).and_then(|mut ec| ec::restore_auto(&mut ec));
            if result.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        result?;
        fs::remove_file(marker)?;
        eprintln!("independent cleanup verified EC4F manual=0");
    }
    Ok(())
}
pub fn write_status(text: &str) -> io::Result<()> {
    let dir = directory()?;
    let temp = dir.join("status.tmp");
    fs::write(&temp, text)?;
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o644))?;
    fs::rename(temp, dir.join("status.json"))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        sync::atomic::{AtomicU64, Ordering},
    };

    #[derive(Clone)]
    struct FakeEc(Rc<RefCell<FakeState>>);

    struct FakeState {
        registers: [u8; 256],
        writes: Vec<(u8, u8)>,
        fail_release: bool,
        fail_activation_after_write: bool,
    }

    impl EcIo for FakeEc {
        fn read(&self, register: u8) -> io::Result<u8> {
            Ok(self.0.borrow().registers[usize::from(register)])
        }
        fn write(&mut self, register: u8, value: u8) -> io::Result<()> {
            let mut state = self.0.borrow_mut();
            if register == 0x4f && value & ec::MANUAL == 0 && state.fail_release {
                return Err(error("simulated release failure"));
            }
            state.registers[usize::from(register)] = value;
            state.writes.push((register, value));
            if register == 0x4f && value & ec::MANUAL != 0 && state.fail_activation_after_write {
                state.fail_activation_after_write = false;
                return Err(error("activation accepted but reported an error"));
            }
            Ok(())
        }
    }

    struct FanFixture {
        dir: PathBuf,
        ec: FakeEc,
    }

    impl FanFixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "minibookx-fan-lease-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join("module.lease"), "module access remains owned").unwrap();
            let mut registers = [0; 256];
            registers[0x4a] = 91;
            registers[0x4f] = 0x05;
            Self {
                dir,
                ec: FakeEc(Rc::new(RefCell::new(FakeState {
                    registers,
                    writes: vec![],
                    fail_release: false,
                    fail_activation_after_write: false,
                }))),
            }
        }
        fn session(&self) -> Session<FakeEc> {
            Session {
                fan: Some(Fan::new(self.ec.clone())),
                dir: self.dir.clone(),
                id: "0123456789abcdef0123456789abcdef".into(),
                _control: self.control(),
            }
        }
        fn control(&self) -> Control {
            Control(Rc::new(ControlState {
                dir: self.dir.clone(),
                id: "0123456789abcdef0123456789abcdef".into(),
                _lock: Lock::take(&self.dir).unwrap(),
            }))
        }
        fn marker(&self) -> PathBuf {
            self.dir.join("manual.owner")
        }
    }
    impl Drop for FanFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn shared_control_keeps_exclusive_ownership_until_the_last_sensor_guard_drops() {
        let fixture = FanFixture::new();
        let control = fixture.control();
        let sensor_guard = control.clone();
        drop(control);
        assert!(Lock::take(&fixture.dir).is_err());
        drop(sensor_guard);
        assert!(Lock::take(&fixture.dir).is_ok());
    }

    #[test]
    fn independent_sensor_recovery_runs_after_other_domains_even_when_they_fail() {
        let events = RefCell::new(Vec::new());
        let failure = recover_independently(
            || {
                events.borrow_mut().push("fan and power");
                Err(error("fan restore failed"))
            },
            || {
                events.borrow_mut().push("sensor without module lease");
                Err(error("sensor restore failed"))
            },
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            &*events.borrow(),
            &["fan and power", "sensor without module lease"]
        );
        assert!(
            failure.contains("fan restore failed") && failure.contains("sensor restore failed")
        );
        assert!(recover_independently(|| Ok(()), || Err(error("sensor conflict"))).is_err());
        assert!(recover_independently(|| Err(error("power conflict")), || Ok(())).is_err());
    }

    #[test]
    fn private_ec_alias_requires_identity_and_private_parent_and_recovery_can_fall_back() {
        use std::os::unix::fs::symlink;
        let fixture = FanFixture::new();
        let private = fixture.dir.join("hardware");
        let source = fixture.dir.join("source");
        fs::create_dir(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(&source).unwrap();
        let owner = unsafe { libc::geteuid() };
        let canonical = source.join("ec/ec0/io");
        assert_eq!(ec_path_at(&private, &source, owner).unwrap(), canonical);
        let alias = private.join("debugfs");
        fs::create_dir(&alias).unwrap();
        // An unmounted target left between unit invocations is not EC access.
        assert_eq!(ec_path_at(&private, &source, owner).unwrap(), canonical);
        // Using the same directory here isolates the identity check. The VM
        // separately verifies a real namespace bind of two distinct paths.
        assert_eq!(
            ec_path_at(&private, &alias, owner).unwrap(),
            alias.join("ec/ec0/io")
        );
        fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ec_path_at(&private, &source, owner).is_err());
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(ec_path_at(&private, &source, owner.wrapping_add(1)).is_err());
        fs::remove_dir(&alias).unwrap();
        symlink(&source, &alias).unwrap();
        assert!(ec_path_at(&private, &source, owner).is_err());
    }

    #[test]
    fn fan_disabled_session_never_prepares_ec_or_creates_manual_ownership() {
        let fixture = FanFixture::new();
        fs::remove_file(fixture.dir.join("module.lease")).unwrap();
        let fan = open_leased_fan::<FakeEc>(
            &fixture.dir,
            "0123456789abcdef0123456789abcdef",
            false,
            || panic!("disabled mode must not inspect EC module state"),
            |_| panic!("disabled mode must not open or prepare EC access"),
        )
        .unwrap();
        assert!(fan.is_none());
        let lease = Lease::read(&fixture.dir.join("module.lease")).unwrap();
        assert!(!lease.fan_control && !lease.was_loaded && !lease.was_writable);
        let mut session = Session {
            fan,
            dir: fixture.dir.clone(),
            id: "0123456789abcdef0123456789abcdef".into(),
            _control: fixture.control(),
        };
        // Start, suspend, resume and stop must all remain fan-free.
        session.activate().unwrap();
        session.restore().unwrap();
        session.activate().unwrap();
        drop(session);
        assert!(!fixture.marker().exists());
        assert!(fixture.ec.0.borrow().writes.is_empty());
        assert!(fixture.dir.join("module.lease").exists());
    }

    #[test]
    fn fan_module_baseline_is_persisted_before_even_failed_preparation() {
        let fixture = FanFixture::new();
        let path = fixture.dir.join("module.lease");
        fs::remove_file(&path).unwrap();
        let result = open_leased_fan::<FakeEc>(
            &fixture.dir,
            "0123456789abcdef0123456789abcdef",
            true,
            || Ok((true, false)),
            |was_loaded| {
                assert!(was_loaded);
                let lease = Lease::read(&path).unwrap();
                assert!(lease.fan_control && lease.was_loaded);
                assert!(!lease.was_writable);
                Err(error("EC interface unavailable"))
            },
        );
        assert!(result.is_err());
        assert!(
            path.exists(),
            "retain recovery attribution after setup failure"
        );
    }

    #[test]
    fn fan_disabled_session_retains_conflicting_manual_owner_and_recovers_power() {
        let fixture = FanFixture::new();
        let mut session = Session::<FakeEc> {
            fan: None,
            dir: fixture.dir.clone(),
            id: "0123456789abcdef0123456789abcdef".into(),
            _control: fixture.control(),
        };
        // A same-invocation marker is still inconsistent: this session never
        // prepared the fan and must not claim or silently erase manual ownership.
        fs::write(fixture.marker(), "0123456789abcdef0123456789abcdef\n").unwrap();
        assert!(session.activate().is_err());
        assert!(session.restore().is_err());
        let attempted_power = Cell::new(false);
        let failure = recover_components(
            || require_no_manual_owner(&fixture.dir),
            || {
                attempted_power.set(true);
                Ok(())
            },
            || panic!("inconsistent fan ownership must retain its lease"),
        )
        .unwrap_err();
        assert!(attempted_power.get());
        assert!(failure.to_string().contains("fan-disabled"));
        assert_eq!(
            fs::read_to_string(fixture.marker()).unwrap(),
            "0123456789abcdef0123456789abcdef\n"
        );
        assert!(fixture.ec.0.borrow().writes.is_empty());
        fs::remove_file(fixture.marker()).unwrap();
        std::os::unix::fs::symlink("missing-target", fixture.marker()).unwrap();
        assert!(session.restore().is_err());
        fs::remove_file(fixture.marker()).unwrap();
    }

    #[test]
    fn legacy_module_leases_recover_as_prepared_fan_sessions() {
        let fixture = FanFixture::new();
        let path = fixture.dir.join("module.lease");
        for (loaded, writable) in [(false, false), (true, false), (true, true)] {
            fs::write(
                &path,
                format!("v1\nfedcba9876543210fedcba9876543210\n{loaded}\n{writable}\n"),
            )
            .unwrap();
            let lease = Lease::read(&path).unwrap();
            assert!(lease.fan_control);
            assert_eq!(lease.id, "fedcba9876543210fedcba9876543210");
            assert_eq!((lease.was_loaded, lease.was_writable), (loaded, writable));
            fs::write(&path, lease.encode()).unwrap();
            let reread = Lease::read(&path).unwrap();
            assert!(reread.fan_control);
            assert_eq!((reread.was_loaded, reread.was_writable), (loaded, writable));
        }
        for text in [
            "v2\n0123456789abcdef0123456789abcdef\nfalse\ntrue\nfalse\n",
            "v2\n0123456789abcdef0123456789abcdef\nfalse\nfalse\ntrue\n",
            "v2\n0123456789abcdef0123456789abcdef\ntrue\nfalse\ntrue\n",
            "v2\n0123456789abcdef0123456789abcdef\nunknown\nfalse\nfalse\n",
            "v3\n0123456789abcdef0123456789abcdef\nfalse\nfalse\nfalse\n",
            "v2\n0123456789abcdef0123456789abcdef\nfalse\nfalse\nfalse\nextra\n",
        ] {
            fs::write(&path, text).unwrap();
            assert!(Lease::read(&path).is_err(), "accepted {text:?}");
        }
        for version in ["v1", "v2"] {
            for id in [
                "",
                "short",
                "0123456789abcdef0123456789abcdeg",
                "0123456789abcdef0123456789abcdef0",
            ] {
                let flags = if version == "v1" {
                    "false\nfalse\n"
                } else {
                    "false\nfalse\nfalse\n"
                };
                fs::write(&path, format!("{version}\n{id}\n{flags}")).unwrap();
                assert!(Lease::read(&path).is_err(), "accepted {version} {id:?}");
            }
        }
    }

    #[test]
    fn verified_release_retires_fan_owner_and_resume_acquires_a_fresh_marker() {
        let fixture = FanFixture::new();
        let mut session = fixture.session();
        session.activate().unwrap();
        assert!(fixture.marker().exists());
        session.restore().unwrap();
        assert!(!fixture.marker().exists());
        assert!(fixture.dir.join("module.lease").exists());
        assert_eq!(fixture.ec.read(0x4f).unwrap(), 0x05);
        session.activate().unwrap();
        assert!(fixture.marker().exists());
        drop(session);
        assert!(!fixture.marker().exists());
        assert_eq!(fixture.ec.read(0x4f).unwrap(), 0x05);
    }

    #[test]
    fn another_controller_after_release_is_not_claimed_or_cleared() {
        let fixture = FanFixture::new();
        let mut session = fixture.session();
        session.activate().unwrap();
        session.restore().unwrap();
        {
            let mut state = fixture.ec.0.borrow_mut();
            state.registers[0x4f] |= ec::MANUAL;
            state.writes.clear();
        }
        assert!(session.activate().is_err());
        assert!(!fixture.marker().exists());
        drop(session);
        assert!(fixture.ec.0.borrow().writes.is_empty());
        assert_ne!(fixture.ec.read(0x4f).unwrap() & ec::MANUAL, 0);
    }

    #[test]
    fn failed_release_keeps_the_marker_and_can_be_retried() {
        let fixture = FanFixture::new();
        let mut session = fixture.session();
        session.activate().unwrap();
        fixture.ec.0.borrow_mut().fail_release = true;
        assert!(session.restore().is_err());
        assert!(fixture.marker().exists());
        assert!(session.fan.as_ref().unwrap().may_be_manual());
        fixture.ec.0.borrow_mut().fail_release = false;
        session.restore().unwrap();
        assert!(!fixture.marker().exists());
        assert_eq!(fixture.ec.read(0x4f).unwrap(), 0x05);
    }

    #[test]
    fn indeterminate_activation_still_has_recovery_attribution() {
        let fixture = FanFixture::new();
        let mut session = fixture.session();
        fixture.ec.0.borrow_mut().fail_activation_after_write = true;
        assert!(session.activate().is_err());
        assert!(fixture.marker().exists());
        assert_ne!(fixture.ec.read(0x4f).unwrap() & ec::MANUAL, 0);
        drop(session);
        assert!(!fixture.marker().exists());
        assert_eq!(fixture.ec.read(0x4f).unwrap(), 0x05);
    }

    #[test]
    fn missing_module_lease_must_not_hide_or_overwrite_other_recovery_records() {
        let dir = std::env::temp_dir().join(format!("minibookx-orphan-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        require_no_unowned_records(&dir).unwrap();
        for name in ["power.lease", "manual.owner"] {
            let path = dir.join(name);
            fs::write(&path, "preserve recovery evidence").unwrap();
            assert!(require_no_unowned_records(&dir).is_err());
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                "preserve recovery evidence"
            );
            fs::remove_file(path).unwrap();
        }
        std::os::unix::fs::symlink("missing-target", dir.join("manual.owner")).unwrap();
        assert!(require_no_unowned_records(&dir).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    struct TestEc(u8);
    impl ec::EcIo for TestEc {
        fn read(&self, register: u8) -> io::Result<u8> {
            assert_eq!(register, 0x4f);
            Ok(self.0)
        }
        fn write(&mut self, register: u8, value: u8) -> io::Result<()> {
            assert_eq!(register, 0x4f);
            self.0 = value;
            Ok(())
        }
    }

    #[test]
    fn failed_power_recovery_still_clears_real_manual_bit_and_retains_retry_access() {
        let mut ec = TestEc(ec::MANUAL | 0x08);
        let module_released = Cell::new(false);
        let failure = recover_components(
            || ec::restore_auto(&mut ec),
            || Err(error("foreign RAPL limit")),
            || {
                module_released.set(true);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(ec.0, 0x08);
        assert!(!module_released.get());
        assert!(failure.to_string().contains("foreign RAPL limit"));
    }

    #[test]
    fn failed_fan_recovery_does_not_skip_power_and_reports_both_failures() {
        let attempted_power = Cell::new(false);
        let failure = recover_components(
            || Err(error("EC unavailable")),
            || {
                attempted_power.set(true);
                Err(error("RAPL unavailable"))
            },
            || panic!("must retain recovery access"),
        )
        .unwrap_err()
        .to_string();
        assert!(attempted_power.get());
        assert!(failure.contains("EC unavailable") && failure.contains("RAPL unavailable"));
    }
}
