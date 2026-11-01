//! Development-only VM executable. Production control flow, fixed fake hardware.
//! EC, temperatures, posture inputs and switch events use files or constants.
//! The VM keeps optional RAPL writes disabled; no hardware is passed through.
#![allow(dead_code)]
pub use minibookx_ctl::{ipc, paths, power, status};
use std::{fs, io, path::Path};

#[path = "../../src/presets.rs"]
mod actual_presets;
#[path = "../../src/broker.rs"]
mod broker;
#[path = "../../src/policy.rs"]
mod policy;
mod presets {
    pub use super::actual_presets::*;
    pub fn load(path: &std::path::Path) -> std::io::Result<Loaded> {
        if std::env::var_os("NOTIFY_SOCKET").is_some() {
            return Err(std::io::Error::other(
                "worker inherited the notifier environment",
            ));
        }
        let loaded = super::actual_presets::load(path)?;
        let watchdog = std::path::Path::new("/run/minibookx-ctl/spoof-watchdog");
        if watchdog.exists() {
            let endpoint = std::fs::read_to_string("/run/minibookx-ctl/notify-endpoint")?;
            // SAFETY: the policy child is single-threaded; nothing reads the
            // environment concurrently.
            unsafe {
                std::env::set_var("NOTIFY_SOCKET", endpoint);
            }
            let notify = super::systemd::Notifier::from_environment()?;
            // SAFETY: as above.
            unsafe {
                std::env::remove_var("NOTIFY_SOCKET");
            }
            // Deliberately stop policy IPC while continuing forged manager
            // notifications from the actual unprivileged service child.
            while watchdog.exists() {
                notify.send("WATCHDOG=1")?;
                super::count("spoof-watchdog")?;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        if std::path::Path::new("/run/minibookx-ctl/spoof-notify").exists() {
            // Simulate a compromised worker knowing the manager's endpoint.
            // NotifyAccess=main must reject it even without environment secrecy.
            let endpoint = std::fs::read_to_string("/run/minibookx-ctl/notify-endpoint")?;
            // SAFETY: the policy child is single-threaded; nothing reads the
            // environment concurrently.
            unsafe {
                std::env::set_var("NOTIFY_SOCKET", endpoint);
            }
            super::systemd::Notifier::from_environment()?.send(&format!(
                "MAINPID={}\nSTATUS=forged by worker\nWATCHDOG=1",
                std::process::id()
            ))?;
            // SAFETY: as above.
            unsafe {
                std::env::remove_var("NOTIFY_SOCKET");
            }
            super::count("spoof-notify")?;
        }
        Ok(loaded)
    }
}
#[path = "../../src/runtime.rs"]
mod runtime;
#[path = "../../src/tablet/power.rs"]
mod sensor_power;
#[path = "../../src/service.rs"]
mod service;
#[path = "../../src/systemd.rs"]
mod systemd;

const ROOT: &str = "/run/minibookx-ctl";
const REJECT_START: &[u8] = b"MINIBOOKX_VM_REJECT_START";

fn count(operation: &str) -> io::Result<()> {
    use std::io::Write;
    // The broker has dropped DAC capabilities and cannot write in the worker's
    // directory. Each process records in its own writable ownership domain.
    // SAFETY: geteuid has no arguments and cannot fail.
    let directory = if unsafe { libc::geteuid() } == 0 {
        ROOT.to_owned()
    } else {
        format!("{ROOT}/policy")
    };
    writeln!(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{directory}/reads-{}", std::process::id()))?,
        "{operation}"
    )
}

// Pollable stand-in only: production lid.rs tests cover the evdev protocol.
mod lid {
    use super::{ROOT, fs, io};
    use std::os::{
        fd::{AsRawFd, RawFd},
        unix::net::UnixDatagram,
    };

    pub struct Lid {
        socket: UnixDatagram,
        closed: Option<bool>,
    }
    impl Lid {
        pub fn discover() -> io::Result<Self> {
            let path = format!("{ROOT}/lid-events");
            match fs::remove_file(&path) {
                Ok(()) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
            let socket = UnixDatagram::bind(path)?;
            socket.set_nonblocking(true)?;
            let mut lid = Self {
                socket,
                closed: None,
            };
            lid.resync()?;
            Ok(lid)
        }
        pub fn fd(&self) -> RawFd {
            self.socket.as_raw_fd()
        }
        pub fn closed(&self) -> Option<bool> {
            self.closed
        }
        pub fn drain(&mut self, revents: libc::c_short) -> io::Result<()> {
            if revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                self.closed = None;
                return Err(io::Error::other("fake lid disconnected"));
            }
            let mut byte = [0];
            loop {
                match self.socket.recv(&mut byte) {
                    Ok(_) => (),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e),
                }
            }
            self.resync()
        }
        pub fn resync(&mut self) -> io::Result<()> {
            self.closed = None;
            self.closed = Some(match fs::read_to_string(format!("{ROOT}/lid-state")) {
                Ok(value) if value.trim() == "closed" => true,
                Ok(value) if value.trim() == "open" => false,
                Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                _ => return Err(io::Error::other("invalid fake lid state")),
            });
            Ok(())
        }
    }
}

mod ec {
    pub use minibookx_ctl::ec::*;
    use std::{fs::File, io, os::unix::fs::FileExt};
    pub struct FileEc(pub File);
    impl EcIo for FileEc {
        fn read(&self, register: u8) -> io::Result<u8> {
            let mut value = [0];
            if self.0.read_at(&mut value, u64::from(register))? != 1 {
                return Err(io::Error::other("short fake EC read"));
            }
            Ok(value[0])
        }
        fn write(&mut self, register: u8, value: u8) -> io::Result<()> {
            if register == 0x4b {
                super::count(&format!("pwm={value}"))?;
            }
            if register == 0x4f
                && value & MANUAL == 0
                && std::path::Path::new("/run/minibookx-ctl/fail-restore").exists()
            {
                return Err(io::Error::other("injected restoration failure"));
            }
            if self.0.write_at(&[value], u64::from(register))? != 1 {
                return Err(io::Error::other("short fake EC write"));
            }
            Ok(())
        }
    }
}

mod hardware {
    use super::{Path, ROOT, ec, fs, io};
    use std::{
        fs::OpenOptions,
        os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        path::PathBuf,
    };
    fn ec_path() -> io::Result<PathBuf> {
        let source = Path::new("/sys/kernel/debug");
        let alias = Path::new("/run/minibookx-ctl/hardware/debugfs");
        let a = fs::metadata(alias)?;
        let s = fs::metadata(source)?;
        Ok(if a.dev() == s.dev() && a.ino() == s.ino() {
            alias
        } else {
            source
        }
        .join("ec/ec0/io"))
    }
    #[derive(Clone)]
    pub struct Control(String);
    impl Control {
        pub fn open() -> io::Result<Self> {
            check_profile()?;
            Ok(Self(
                std::env::var("INVOCATION_ID").map_err(io::Error::other)?,
            ))
        }
        pub fn directory(&self) -> &Path {
            Path::new(ROOT)
        }
        pub fn invocation(&self) -> &str {
            &self.0
        }
    }
    pub fn check_profile() -> io::Result<()> {
        if fs::read_to_string("/sys/class/dmi/id/product_name")?.trim()
            != "MiniBookX Systemd Fixture"
        {
            return Err(io::Error::other("fixture requires the isolated QEMU guest"));
        }
        Ok(())
    }
    fn open() -> io::Result<ec::FileEc> {
        if Path::new(&format!("{ROOT}/forbid-ec")).exists() {
            return Err(io::Error::other("test forbids EC access"));
        }
        Ok(ec::FileEc(
            OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(ec_path()?)?,
        ))
    }
    pub fn prepare() -> io::Result<()> {
        check_profile()?;
        fs::set_permissions(
            format!("{ROOT}/hardware"),
            fs::Permissions::from_mode(0o700),
        )?;
        recover_fan()?;
        if let Err(error) = super::tablet::recover_sensor(Path::new(ROOT)) {
            eprintln!("VM sensor recovery quarantined: {error}");
        }
        Ok(())
    }
    fn recover_fan() -> io::Result<()> {
        check_profile()?;
        eprintln!("VM_RECOVER invoked");
        let fan = match fs::read_to_string(format!("{ROOT}/module.lease")) {
            Ok(value) => match value.trim() {
                "fan=true" => true,
                "fan=false" => false,
                _ => return Err(io::Error::other("invalid fake module lease")),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        if fan && Path::new(&format!("{ROOT}/manual.owner")).exists() {
            ec::restore_auto(&mut open()?)?;
        } else if !fan && Path::new(&format!("{ROOT}/manual.owner")).exists() {
            return Err(io::Error::other("unowned fake manual marker"));
        }
        for name in ["manual.owner", "module.lease"] {
            match fs::remove_file(format!("{ROOT}/{name}")) {
                Ok(()) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    pub fn recover() -> io::Result<()> {
        let fan = recover_fan();
        let sensor = super::tablet::recover_sensor(Path::new(ROOT));
        fan?;
        sensor?;
        eprintln!("VM_RECOVER completed");
        Ok(())
    }
    pub struct Session {
        pub fan: Option<ec::Fan<ec::FileEc>>,
    }
    impl Session {
        pub fn open_locked(fan_control: bool, _: Control) -> io::Result<Self> {
            Self::open(fan_control)
        }
        pub fn open(fan_control: bool) -> io::Result<Self> {
            fs::write(
                format!("{ROOT}/module.lease"),
                if fan_control {
                    "fan=true\n"
                } else {
                    "fan=false\n"
                },
            )?;
            let fan = if fan_control {
                if Path::new(&format!("{ROOT}/forbid-ec")).exists() {
                    return Err(io::Error::other("test forbids EC setup"));
                }
                let ec_path = ec_path()?;
                if !ec_path.exists() {
                    fs::create_dir_all(ec_path.parent().unwrap())?;
                    let mut bytes = [0; 256];
                    bytes[0x4a] = 91;
                    bytes[0x70] = 60;
                    bytes[0x76] = 9;
                    bytes[0x77] = 196;
                    fs::write(ec_path, bytes)?;
                }
                let fan = ec::Fan::new(open()?);
                fan.baseline()?;
                Some(fan)
            } else {
                None
            };
            Ok(Self { fan })
        }
        pub fn activate(&mut self) -> io::Result<()> {
            let Some(fan) = &mut self.fan else {
                return Ok(());
            };
            fs::write(format!("{ROOT}/manual.owner"), "fixture broker\n")?;
            fan.activate()
        }
        pub fn restore(&mut self) -> io::Result<()> {
            if let Some(fan) = &mut self.fan {
                fan.restore()?;
            }
            match fs::remove_file(format!("{ROOT}/manual.owner")) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        }
    }
}

mod sensor {
    use std::{io, path::Path};
    pub struct CpuSensor;
    impl CpuSensor {
        pub fn discover(_: &Path) -> io::Result<Self> {
            if Path::new("/run/minibookx-ctl/forbid-ec").exists() {
                return Err(io::Error::other("test forbids coretemp discovery"));
            }
            Ok(Self)
        }
        pub fn read_mc(&self) -> io::Result<i32> {
            super::count("cpu")?;
            match std::fs::read_to_string("/run/minibookx-ctl/temperature") {
                Ok(value) => value.trim().parse().map_err(io::Error::other),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(60_000),
                Err(e) => Err(e),
            }
        }
    }
}

mod tablet {
    use super::{ROOT, sensor_power};
    pub use minibookx_ctl::tablet::{
        Estimate, EstimateReason, HingeEstimator, SAMPLE_INTERVAL, Sample,
    };
    use std::{
        fs::{File, OpenOptions},
        io::{self, Write},
        os::unix::fs::OpenOptionsExt,
    };
    struct Sensor;
    impl sensor_power::SensorIo for Sensor {
        fn control(&self) -> io::Result<u8> {
            std::fs::read_to_string(format!("{ROOT}/sensor-control"))?
                .trim()
                .parse()
                .map_err(io::Error::other)
        }
        fn write_control(&self, value: u8) -> Result<(), sensor_power::WriteFailure> {
            if std::path::Path::new(&format!("{ROOT}/fail-sensor-restore")).exists() && value == 1 {
                return Err(sensor_power::WriteFailure::before(io::Error::other(
                    "injected sensor restore failure",
                )));
            }
            std::fs::write(format!("{ROOT}/sensor-control"), value.to_string())
                .map_err(sensor_power::WriteFailure::attempted)
        }
    }
    pub struct Sensors(sensor_power::Ownership);
    impl Sensors {
        pub fn discover(control: &super::hardware::Control) -> io::Result<Self> {
            if std::path::Path::new(&format!("{ROOT}/slow-discovery")).exists() {
                std::thread::sleep(std::time::Duration::from_millis(2300));
            }
            let once = std::path::Path::new("/run/minibookx-ctl/fail-discovery-once");
            if once.exists() {
                std::fs::remove_file(once)?;
                return Err(io::Error::other("injected one-shot discovery failure"));
            }
            if !std::path::Path::new(&format!("{ROOT}/sensor-control")).exists() {
                std::fs::write(format!("{ROOT}/sensor-control"), "1")?;
            }
            let lease = sensor_power::Lease::acquire(
                &Sensor,
                control.directory(),
                control.invocation(),
                0,
                std::thread::sleep,
            )?;
            Ok(Self(sensor_power::Ownership::new(lease)))
        }
        pub fn restore(&mut self) -> io::Result<()> {
            super::count("sensor-restore")?;
            self.0.restore(&Sensor)
        }
        pub fn retain_for_recovery(&mut self) {
            self.0.retain();
        }
        pub fn sample(&self) -> io::Result<Sample> {
            super::count("posture")?;
            if std::path::Path::new("/run/minibookx-ctl/fail-posture").exists() {
                return Err(io::Error::other("injected posture read failure"));
            }
            if std::path::Path::new("/run/minibookx-ctl/posture-rotated").exists() {
                // Same hinge angle after rotating both gravity vectors.
                return Ok(Sample {
                    primary: [0.0, 0.0, -1000.0],
                    secondary: [-1000.0, 0.0, 0.0],
                });
            }
            // Synthetic 270-degree hinge, fixed numerical input only.
            Ok(Sample {
                primary: [1000.0, 0.0, 0.0],
                secondary: [0.0, 0.0, -1000.0],
            })
        }
    }
    impl Drop for Sensors {
        fn drop(&mut self) {
            if self.0.restore_on_drop() {
                let _ = self.restore();
            }
        }
    }
    pub fn recover_sensor(dir: &std::path::Path) -> io::Result<()> {
        if let Some(lease) = sensor_power::Lease::read(dir, None, 0)? {
            lease.restore(&Sensor)?;
        }
        Ok(())
    }
    pub fn sensor_recovery_pending(dir: &std::path::Path) -> bool {
        sensor_power::recovery_pending(dir)
    }
    pub struct VirtualSwitch(File);
    impl VirtualSwitch {
        pub fn into_fd(self) -> std::os::fd::OwnedFd {
            self.0.into()
        }
        pub fn from_fd(fd: std::os::fd::OwnedFd) -> Self {
            Self(fd.into())
        }
        pub fn force_off(&mut self) -> io::Result<()> {
            self.send(false)
        }
        pub fn create() -> io::Result<Self> {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o644)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open("/run/minibookx-ctl/switch-events")?;
            Ok(Self(file))
        }
        pub fn send(&mut self, tablet: bool) -> io::Result<()> {
            // The writer PID lets the guest prove which process sent OFF.
            let line = format!("{} {}\n", u8::from(tablet), std::process::id());
            self.0.write_all(line.as_bytes())?;
            self.0.flush()
        }
    }
}

fn run() -> io::Result<()> {
    hardware::check_profile()?;
    let mode = std::env::args().nth(1);
    if mode.as_deref() == Some("--check-config-stdin") {
        return minibookx_ctl::install::check_config_stdin()
            .map_err(|e| io::Error::other(e.to_string()));
    }
    // SAFETY: geteuid has no arguments and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::other("fixture requires guest root"));
    }
    match mode.as_deref() {
        Some("--install") => minibookx_ctl::install::run(true, std::env::args().skip(2))
            .map_err(|e| io::Error::other(e.to_string())),
        Some("--prepare") => hardware::prepare(),
        Some("--recover") => {
            hardware::recover()?;
            // Distinct from --prepare's recovery: proves ExecStopPost ran.
            eprintln!("VM_EXECSTOPPOST recovery completed");
            Ok(())
        }
        Some("--daemon") => {
            // Only the guest test's candidate copy has this ELF trailer. Its
            // installer and recovery work, but startup must fail and roll back.
            if fs::read("/proc/self/exe")?.ends_with(REJECT_START) {
                return Err(io::Error::other("injected new-version startup failure"));
            }
            if let Some(endpoint) = std::env::var_os("NOTIFY_SOCKET") {
                fs::write(
                    format!("{ROOT}/notify-endpoint"),
                    endpoint.as_encoded_bytes(),
                )?;
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(
                    format!("{ROOT}/notify-endpoint"),
                    fs::Permissions::from_mode(0o644),
                )?;
            }
            broker::run(
                "/etc/minibookx-ctl/config.conf".into(),
                !Path::new(&format!("{ROOT}/no-tablet")).exists(),
            )
        }
        Some("--sandbox-probe") => {
            // This node exists only in the diskless test guest; no host block
            // device is passed through. An open alone must be denied by cgroup.
            let denied = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/minibookx-test-block")
                .err();
            if !denied.is_some_and(|e| e.raw_os_error() == Some(libc::EPERM)) {
                return Err(io::Error::other("block-device access was not denied"));
            }
            // An open tests the actual uinput access without creating a second
            // switch or bypassing production's U300 identity guard.
            let _uinput = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/uinput")?;
            let input_read = fs::OpenOptions::new()
                .read(true)
                .open("/dev/minibookx-test-input")
                .err();
            if !input_read
                .is_some_and(|e| matches!(e.raw_os_error(), Some(libc::ENODEV | libc::ENXIO)))
            {
                return Err(io::Error::other("input read did not reach the fake device"));
            }
            let input_write = fs::OpenOptions::new()
                .write(true)
                .open("/dev/minibookx-test-input")
                .err();
            if !input_write.is_some_and(|e| e.raw_os_error() == Some(libc::EPERM)) {
                return Err(io::Error::other("input injection was not denied"));
            }
            match fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/i2c-0")
            {
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENODEV | libc::ENXIO)) => (),
                _ => return Err(io::Error::other("I2C open did not reach the fake bus")),
            }
            // Unit sandbox. perf_event_open is outside @system-service: the
            // filter returns EPERM, while the kernel would reject NULL attr.
            // SAFETY: the NULL attr is never dereferenced in user space; the
            // filter or the kernel rejects it, and no descriptor is created.
            let perf = unsafe {
                libc::syscall(
                    libc::SYS_perf_event_open,
                    std::ptr::null::<u8>(),
                    0,
                    -1,
                    -1,
                    0,
                )
            };
            if perf != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
                return Err(io::Error::other("system call filter is not active"));
            }
            // SAFETY: socket takes integers only.
            let netlink =
                unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, 0) };
            if netlink >= 0 {
                // SAFETY: closes the descriptor this probe has just created.
                unsafe { libc::close(netlink) };
                return Err(io::Error::other("netlink socket was not denied"));
            }
            if std::net::UdpSocket::bind("127.0.0.1:0").is_ok() {
                return Err(io::Error::other("IP socket was not denied"));
            }
            if fs::write("/dev/shm/minibookx-probe", b"x").is_ok() {
                return Err(io::Error::other("/dev/shm is writable"));
            }
            Ok(())
        }
        _ => Err(io::Error::other("fixture lifecycle command required")),
    }
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("VM_FIXTURE_ERROR {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
