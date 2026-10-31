//! Fixed base-accelerometer power ownership. No device path or register is
//! supplied by the journal; recovery rediscovers the same hardware profile.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

const RECORD: &str = "sensor.lease";
const SETTLE: Duration = Duration::from_millis(500);
const MAX_RECORD: u64 = 96;

/// A record can survive failed discovery without a live Sensors object.
/// Invalid records and metadata errors still mean recovery is incomplete.
pub(super) fn recovery_pending(dir: &Path) -> bool {
    !matches!(fs::symlink_metadata(dir.join(RECORD)), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

pub(super) trait SensorIo {
    /// Also recheck the fixed adapter, unclaimed address and chip identity.
    fn control(&self) -> io::Result<u8>;
    fn write_control(&self, value: u8) -> Result<(), WriteFailure>;
}

pub(super) struct WriteFailure {
    pub attempted: bool,
    pub error: io::Error,
}
impl WriteFailure {
    pub(super) fn before(error: io::Error) -> Self {
        Self {
            attempted: false,
            error,
        }
    }
    pub(super) fn attempted(error: io::Error) -> Self {
        Self {
            attempted: true,
            error,
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invocation(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn trusted_directory(dir: &Path, owner: u32) -> io::Result<()> {
    let m = fs::symlink_metadata(dir)?;
    if !m.is_dir() || m.uid() != owner || m.mode() & 0o022 != 0 {
        return Err(invalid("untrusted sensor recovery directory"));
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct Lease {
    dir: PathBuf,
    id: String,
    owner: u32,
}

/// Keep attribution when automatic cleanup is unsafe, without making a later
/// explicit restore look successful merely because Drop was suppressed.
pub(super) struct Ownership {
    lease: Option<Lease>,
    restore_on_drop: bool,
}
impl Ownership {
    pub(super) fn new(lease: Option<Lease>) -> Self {
        Self {
            lease,
            restore_on_drop: true,
        }
    }
    pub(super) fn retain(&mut self) {
        self.restore_on_drop = false;
    }
    pub(super) fn restore_on_drop(&self) -> bool {
        self.restore_on_drop
    }
    pub(super) fn restore(&mut self, sensor: &impl SensorIo) -> io::Result<()> {
        if let Some(lease) = &self.lease {
            lease.restore(sensor)?;
            self.lease = None;
        }
        self.restore_on_drop = true;
        Ok(())
    }
}

impl Lease {
    fn text(id: &str) -> String {
        // Version, invocation, fixed profile, original/applied CONTROL bytes.
        format!("v1\n{id}\nU300-I2C3-15-ID02\n01\n00\n")
    }

    pub(super) fn read(
        dir: &Path,
        expected_id: Option<&str>,
        owner: u32,
    ) -> io::Result<Option<Self>> {
        trusted_directory(dir, owner)?;
        let path = dir.join(RECORD);
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // A dangling symlink is a conflicting record, not absence.
                if fs::symlink_metadata(path).is_ok() {
                    return Err(invalid("untrusted sensor recovery record"));
                }
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let m = file.metadata()?;
        if !m.is_file() || m.uid() != owner || m.mode() & 0o7777 != 0o600 || m.nlink() != 1 {
            return Err(invalid("untrusted sensor recovery record"));
        }
        let mut text = String::new();
        file.take(MAX_RECORD + 1).read_to_string(&mut text)?;
        if text.len() as u64 > MAX_RECORD {
            return Err(invalid("oversized sensor recovery record"));
        }
        let fields: Vec<_> = text.lines().collect();
        let ["v1", id, "U300-I2C3-15-ID02", "01", "00"] = fields.as_slice() else {
            return Err(invalid("invalid sensor recovery record"));
        };
        if !invocation(id) || text != Self::text(id) {
            return Err(invalid("invalid sensor recovery invocation"));
        }
        if expected_id.is_some_and(|expected| expected != *id) {
            return Err(invalid("foreign sensor recovery invocation"));
        }
        Ok(Some(Self {
            dir: dir.into(),
            id: (*id).into(),
            owner,
        }))
    }

    pub(super) fn acquire(
        sensor: &impl SensorIo,
        dir: &Path,
        id: &str,
        owner: u32,
        settle: impl FnOnce(Duration),
    ) -> io::Result<Option<Self>> {
        Self::acquire_with_writer(sensor, dir, id, owner, settle, |file, bytes| {
            file.write_all(bytes)
        })
    }

    fn acquire_with_writer(
        sensor: &impl SensorIo,
        dir: &Path,
        id: &str,
        owner: u32,
        settle: impl FnOnce(Duration),
        write: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
    ) -> io::Result<Option<Self>> {
        trusted_directory(dir, owner)?;
        if !invocation(id) {
            return Err(invalid("invalid sensor invocation"));
        }
        if fs::symlink_metadata(dir.join(RECORD)).is_ok() {
            return Err(invalid("pending sensor ownership requires recovery"));
        }
        match sensor.control()? {
            0 => return Ok(None),
            1 => (),
            _ => {
                return Err(invalid(
                    "unsupported secondary sensor CONTROL; expected 00 or 01",
                ));
            }
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(dir.join(RECORD))?;
        let recorded = (|| {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            write(&mut file, Self::text(id).as_bytes())?;
            file.sync_all()?;
            File::open(dir)?.sync_all()
        })();
        if let Err(error) = recorded {
            // create_new succeeded, but no sensor write has been attempted.
            // A partial record must not turn into permanent recovery ownership.
            fs::remove_file(dir.join(RECORD)).map_err(|cleanup| {
                io::Error::other(format!("sensor journal: {error}; cleanup: {cleanup}"))
            })?;
            File::open(dir)?.sync_all()?;
            return Err(error);
        }
        let lease = Self {
            dir: dir.into(),
            id: id.into(),
            owner,
        };
        // Recheck immediately before changing only the known PD state.
        if let Err(error) = sensor.control().and_then(|value| {
            if value == 1 {
                Ok(())
            } else {
                Err(invalid("secondary sensor changed before wake"))
            }
        }) {
            // We know no write was attempted. Do not claim a third party's
            // independently changed state as this controller's wake.
            lease.retire()?;
            return Err(error);
        }
        if let Err(failure) = sensor.write_control(0) {
            if !failure.attempted {
                lease.retire()?;
            }
            return Err(failure.error);
        }
        if sensor.control()? != 0 {
            return Err(invalid("secondary sensor wake readback failed"));
        }
        // MXC6655XA specifies 400 ms maximum after leaving power-down. Keep
        // input OFF and discard all readings during a conservative 500 ms.
        settle(SETTLE);
        Ok(Some(lease))
    }

    pub(super) fn restore(&self, sensor: &impl SensorIo) -> io::Result<()> {
        // Validate the on-disk attribution again, including its invocation.
        if Self::read(&self.dir, Some(&self.id), self.owner)?.is_none() {
            return Err(invalid("sensor recovery attribution disappeared"));
        }
        match sensor.control()? {
            0 => {
                sensor.write_control(1).map_err(|failure| failure.error)?;
                if sensor.control()? != 1 {
                    return Err(invalid("secondary sensor power-down readback failed"));
                }
            }
            1 => (), // A failed wake may never have changed the original state.
            _ => {
                return Err(invalid(
                    "foreign secondary sensor CONTROL; retaining recovery record",
                ));
            }
        }
        self.retire()
    }

    fn retire(&self) -> io::Result<()> {
        fs::remove_file(self.dir.join(RECORD))?;
        File::open(&self.dir)?.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        os::unix::fs::{PermissionsExt, symlink},
        sync::atomic::{AtomicU64, Ordering},
    };

    const ID: &str = "0123456789abcdef0123456789abcdef";

    #[derive(Clone, Copy, PartialEq)]
    enum Fault {
        None,
        BeforeValidation,
        BeforeWrite,
        AfterWrite,
        IgnoreWrite,
        ReadAfterWake,
        Read,
    }
    struct Fake {
        dir: PathBuf,
        value: Cell<u8>,
        fault: Cell<Fault>,
        writes: RefCell<Vec<u8>>,
        reads: Cell<usize>,
        external_change: Cell<bool>,
    }
    impl SensorIo for Fake {
        fn control(&self) -> io::Result<u8> {
            if self.fault.get() == Fault::Read {
                return Err(io::Error::other("identity/read failure"));
            }
            self.reads.set(self.reads.get() + 1);
            if self.external_change.get() && self.reads.get() == 2 {
                self.value.set(0);
            }
            Ok(self.value.get())
        }
        fn write_control(&self, value: u8) -> Result<(), WriteFailure> {
            assert!(value <= 1);
            // This is the mutation boundary: durable attribution must exist.
            assert!(Lease::read(&self.dir, Some(ID), owner()).unwrap().is_some());
            if self.fault.get() == Fault::BeforeValidation {
                return Err(WriteFailure::before(io::Error::other(
                    "identity changed before write",
                )));
            }
            self.writes.borrow_mut().push(value);
            match self.fault.get() {
                Fault::BeforeWrite => {
                    return Err(WriteFailure::attempted(io::Error::other("write refused")));
                }
                Fault::IgnoreWrite => return Ok(()),
                _ => self.value.set(value),
            }
            if self.fault.get() == Fault::AfterWrite {
                return Err(WriteFailure::attempted(io::Error::other(
                    "write accepted but reported failure",
                )));
            }
            if self.fault.get() == Fault::ReadAfterWake {
                self.fault.set(Fault::Read);
            }
            Ok(())
        }
    }
    fn owner() -> u32 {
        unsafe { libc::geteuid() }
    }
    struct Fixture {
        dir: PathBuf,
        sensor: Fake,
    }
    impl Fixture {
        fn new(value: u8) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "minibookx-sensor-lease-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                sensor: Fake {
                    dir: dir.clone(),
                    value: Cell::new(value),
                    fault: Cell::new(Fault::None),
                    writes: RefCell::new(vec![]),
                    reads: Cell::new(0),
                    external_change: Cell::new(false),
                },
                dir,
            }
        }
        fn wake(&self) -> io::Result<Option<Lease>> {
            Lease::acquire(&self.sensor, &self.dir, ID, owner(), |duration| {
                assert_eq!(duration, Duration::from_millis(500));
                assert_eq!(self.sensor.value.get(), 0);
            })
        }
        fn record(&self) -> PathBuf {
            self.dir.join(RECORD)
        }
        fn saved(&self) -> Lease {
            Lease::read(&self.dir, None, owner()).unwrap().unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn cold_power_down_is_journaled_before_wake_and_restored_after_settling() {
        let f = Fixture::new(1);
        let settled = Cell::new(false);
        let lease = Lease::acquire(&f.sensor, &f.dir, ID, owner(), |delay| {
            assert_eq!(delay, Duration::from_millis(500));
            assert_eq!(f.sensor.value.get(), 0);
            assert_eq!(&*f.sensor.writes.borrow(), &[0]);
            assert!(f.record().exists());
            settled.set(true);
        })
        .unwrap()
        .unwrap();
        assert!(settled.get());
        lease.restore(&f.sensor).unwrap();
        assert_eq!(&*f.sensor.writes.borrow(), &[0, 1]);
        assert_eq!(f.sensor.value.get(), 1);
        assert!(!f.record().exists());
        // Resume claims a new baseline; no elapsed pre-suspend time is reused.
        let resumed = f.wake().unwrap().unwrap();
        resumed.restore(&f.sensor).unwrap();
        assert_eq!(&*f.sensor.writes.borrow(), &[0, 1, 0, 1]);
    }

    #[test]
    fn recovery_pending_includes_orphaned_and_unreadable_records_without_hardware() {
        let f = Fixture::new(1);
        assert!(!recovery_pending(&f.dir));
        fs::write(f.record(), "invalid attribution").unwrap();
        assert!(recovery_pending(&f.dir));
        // Metadata errors are not proof that attribution has disappeared.
        assert!(recovery_pending(&f.record()));
        fs::remove_file(f.record()).unwrap();
        symlink("missing", f.record()).unwrap();
        assert!(recovery_pending(&f.dir));
        fs::remove_file(f.record()).unwrap();
        assert!(!recovery_pending(&f.dir));
        assert_eq!(f.sensor.reads.get(), 0);
        assert!(f.sensor.writes.borrow().is_empty());
        let lease = f.wake().unwrap().unwrap();
        let before = f.sensor.reads.get();
        assert!(recovery_pending(&f.dir));
        assert_eq!(f.sensor.reads.get(), before);
        lease.restore(&f.sensor).unwrap();
        let before = f.sensor.reads.get();
        assert!(!recovery_pending(&f.dir));
        assert_eq!(f.sensor.reads.get(), before);
        assert_eq!(&*f.sensor.writes.borrow(), &[0, 1]);
    }

    #[test]
    fn retained_ownership_still_restores_or_reports_the_real_error() {
        let f = Fixture::new(1);
        let mut ownership = Ownership::new(f.wake().unwrap());
        ownership.retain();
        assert!(!ownership.restore_on_drop());
        assert!(f.record().exists());
        f.sensor.fault.set(Fault::Read);
        assert!(ownership.restore(&f.sensor).is_err());
        assert!(f.record().exists());
        assert_eq!(&*f.sensor.writes.borrow(), &[0]);
        f.sensor.fault.set(Fault::None);
        ownership.restore(&f.sensor).unwrap();
        assert_eq!(&*f.sensor.writes.borrow(), &[0, 1]);
        assert!(ownership.restore_on_drop());
        assert!(Lease::read(&f.dir, None, owner()).unwrap().is_none());
    }

    #[test]
    fn failed_partial_journal_write_leaves_no_record_or_sensor_mutation() {
        let f = Fixture::new(1);
        let result = Lease::acquire_with_writer(
            &f.sensor,
            &f.dir,
            ID,
            owner(),
            |_| panic!("must not settle"),
            |file, bytes| {
                file.write_all(&bytes[..7])?;
                Err(io::Error::from_raw_os_error(libc::ENOSPC))
            },
        );
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ENOSPC));
        assert!(f.sensor.writes.borrow().is_empty());
        assert_eq!(f.sensor.value.get(), 1);
        assert!(Lease::read(&f.dir, None, owner()).unwrap().is_none());
    }

    #[test]
    fn already_awake_baseline_has_no_write_ownership_or_delay() {
        let f = Fixture::new(0);
        let lease = Lease::acquire(&f.sensor, &f.dir, ID, owner(), |_| {
            panic!("active baseline must not wait")
        });
        assert!(lease.unwrap().is_none());
        assert!(f.sensor.writes.borrow().is_empty());
        assert!(!f.record().exists());
    }

    #[test]
    fn unknown_control_or_failed_identity_is_not_reconfigured() {
        for value in [2, 0x20, 0x40, 0x80, 0xff] {
            let f = Fixture::new(value);
            assert!(f.wake().is_err());
            assert!(!f.record().exists());
            assert!(f.sensor.writes.borrow().is_empty());
        }
        let f = Fixture::new(1);
        f.sensor.fault.set(Fault::Read);
        assert!(f.wake().is_err());
        assert!(!f.record().exists());
    }

    #[test]
    fn failed_or_indeterminate_wake_keeps_independent_recovery_without_module_lease() {
        for fault in [
            Fault::BeforeWrite,
            Fault::AfterWrite,
            Fault::IgnoreWrite,
            Fault::ReadAfterWake,
        ] {
            let f = Fixture::new(1);
            f.sensor.fault.set(fault);
            assert!(f.wake().is_err());
            assert!(f.record().exists());
            assert!(!f.dir.join("module.lease").exists());
            f.sensor.fault.set(Fault::None);
            f.saved().restore(&f.sensor).unwrap();
            assert_eq!(f.sensor.value.get(), 1);
            assert!(!f.record().exists());
        }
    }

    #[test]
    fn a_known_prewrite_failure_never_claims_an_external_wake() {
        let f = Fixture::new(1);
        f.sensor.external_change.set(true);
        assert!(f.wake().is_err());
        assert_eq!(f.sensor.value.get(), 0);
        assert!(f.sensor.writes.borrow().is_empty());
        assert!(!f.record().exists());
        let f = Fixture::new(1);
        f.sensor.fault.set(Fault::BeforeValidation);
        assert!(f.wake().is_err());
        assert!(f.sensor.writes.borrow().is_empty());
        assert!(!f.record().exists());
    }

    #[test]
    fn readback_failure_and_foreign_control_retain_attribution_for_retry() {
        let f = Fixture::new(1);
        let lease = f.wake().unwrap().unwrap();
        f.sensor.fault.set(Fault::IgnoreWrite);
        assert!(lease.restore(&f.sensor).is_err());
        assert!(f.record().exists());
        f.sensor.fault.set(Fault::None);
        f.sensor.value.set(0x20);
        let before = f.sensor.writes.borrow().len();
        assert!(lease.restore(&f.sensor).is_err());
        assert_eq!(before, f.sensor.writes.borrow().len());
        assert!(f.record().exists());
        f.sensor.value.set(0);
        lease.restore(&f.sensor).unwrap();
    }

    #[test]
    fn pending_lease_cannot_be_replaced_or_claimed_by_another_invocation() {
        let f = Fixture::new(1);
        f.wake().unwrap();
        let bytes = fs::read(f.record()).unwrap();
        assert!(f.wake().is_err());
        assert_eq!(fs::read(f.record()).unwrap(), bytes);
        assert!(Lease::read(&f.dir, Some("fedcba9876543210fedcba9876543210"), owner()).is_err());
        assert_eq!(&*f.sensor.writes.borrow(), &[0]);
    }

    #[test]
    fn recovery_rejects_nonprivate_linked_malformed_or_oversized_records() {
        let f = Fixture::new(1);
        f.wake().unwrap();
        fs::set_permissions(f.record(), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Lease::read(&f.dir, None, owner()).is_err());
        fs::set_permissions(f.record(), fs::Permissions::from_mode(0o600)).unwrap();
        let hard = f.dir.join("hard");
        fs::hard_link(f.record(), &hard).unwrap();
        assert!(Lease::read(&f.dir, None, owner()).is_err());
        fs::remove_file(hard).unwrap();
        for bytes in [
            "v1\nshort\nU300-I2C3-15-ID02\n01\n00\n".to_owned(),
            "x".repeat(200),
            format!("{}extra\n", Lease::text(ID)),
        ] {
            fs::write(f.record(), bytes).unwrap();
            assert!(Lease::read(&f.dir, None, owner()).is_err());
        }
        fs::remove_file(f.record()).unwrap();
        symlink("missing", f.record()).unwrap();
        assert!(Lease::read(&f.dir, None, owner()).is_err());
        assert!(f.wake().is_err());
        fs::remove_file(f.record()).unwrap();
        fs::write(f.record(), Lease::text(ID)).unwrap();
        fs::set_permissions(f.record(), fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&f.dir, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(Lease::read(&f.dir, None, owner()).is_err());
    }
}
