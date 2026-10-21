//! Bounded diagnostics across the worker/root ownership boundary.
use serde_json::{Value, json};
use std::{
    ffi::CStr,
    fs::{File, OpenOptions},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

const MAX_RECORD: u64 = 16 * 1024;

/// Milliseconds since boot, including suspend. This provides a common clock for
/// bounded status records and read-only clients without exposing a wall clock.
pub fn boottime_ms() -> io::Result<u64> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: CLOCK_BOOTTIME is a read-only clock and value is a writable buffer.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut value) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if value.tv_sec < 0 || !(0..1_000_000_000).contains(&value.tv_nsec) {
        return Err(io::Error::other("invalid boot clock"));
    }
    (value.tv_sec as u64)
        .checked_mul(1000)
        .and_then(|v| v.checked_add(value.tv_nsec as u64 / 1_000_000))
        .ok_or_else(|| io::Error::other("boot clock overflow"))
}

fn check_directory(file: &File, owner: u32) -> io::Result<()> {
    let m = file.metadata()?;
    if !m.is_dir() || m.uid() != owner || m.mode() & 0o022 != 0 {
        return Err(io::Error::other("untrusted status directory"));
    }
    Ok(())
}

fn open_at(directory: &File, name: &CStr, is_directory: bool) -> io::Result<File> {
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | libc::O_NOFOLLOW
        | if is_directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: name is a fixed relative leaf and directory is an owned live FD.
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat transfers this new FD to File exactly once.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn read_record(directory: &File, name: &CStr, owner: u32) -> io::Result<Value> {
    let file = open_at(directory, name, false)?;
    let m = file.metadata()?;
    if !m.is_file()
        || m.uid() != owner
        || m.nlink() != 1
        || m.mode() & 0o022 != 0
        || m.len() > MAX_RECORD
    {
        return Err(io::Error::other("untrusted or oversized status record"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD {
        return Err(io::Error::other("status record grew beyond limit"));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if !value.is_object() {
        return Err(io::Error::other("status record must be a JSON object"));
    }
    Ok(value)
}

fn optional_record(directory: &File, name: &CStr, owner: u32) -> io::Result<Value> {
    match read_record(directory, name, owner) {
        Ok(value) => Ok(value),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Value::Null),
        Err(error) => Err(error),
    }
}

fn snapshot(root: &File, root_owner: u32, policy_owner: u32) -> io::Result<Value> {
    let policy_dir = open_at(root, c"policy", true)?;
    check_directory(&policy_dir, policy_owner)?;
    let policy = read_record(&policy_dir, c"status.json", policy_owner)?;
    let fan = optional_record(root, c"fan.json", root_owner)?;
    let tablet = optional_record(root, c"tablet.json", root_owner)?;
    let power = optional_record(root, c"power.json", root_owner)?;
    Ok(json!({"policy": policy, "fan": fan, "tablet": tablet, "power": power}))
}

pub fn current(root: &Path) -> io::Result<Value> {
    let directory = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(json!({"active": false, "state": "not-running"}));
        }
        Err(error) => return Err(error),
    };
    check_directory(&directory, 0)?;
    // This root-owned marker needs only stat, so an ordinary user can run
    // --status without gaining access to the private marker's contents.
    let mut marker = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: valid directory FD, static leaf, and writable stat result buffer.
    let result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            c"module.lease".as_ptr(),
            marker.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(json!({"active": false, "state": "not-running"}));
        }
        return Err(error);
    }
    // SAFETY: successful fstatat initialized the stat result.
    let marker = unsafe { marker.assume_init() };
    if marker.st_mode & libc::S_IFMT != libc::S_IFREG
        || marker.st_uid != 0
        || marker.st_nlink != 1
        || marker.st_mode & 0o077 != 0
        || marker.st_size > 256
    {
        return Err(io::Error::other("untrusted runtime lease marker"));
    }
    // SAFETY: CLI startup is single-threaded and the static name is NUL-terminated.
    let account = unsafe { libc::getpwnam(c"minibookx-ctl".as_ptr()) };
    if account.is_null() {
        return Err(io::Error::other("service account is missing"));
    }
    // SAFETY: copy the uid before another libc account lookup can occur.
    let owner = unsafe { (*account).pw_uid };
    if owner == 0 {
        return Err(io::Error::other("policy account must be unprivileged"));
    }
    match snapshot(&directory, 0, owner) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(json!({"active": false, "state": "starting-or-recovering"}))
        }
        value => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        time::{SystemTime, UNIX_EPOCH},
    };
    #[test]
    fn snapshot_rejects_directory_substitution_and_unsafe_worker_files() {
        let path = std::env::temp_dir().join(format!(
            "minibookx-status-snapshot-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let policy = path.join("policy");
        fs::create_dir(&policy).unwrap();
        fs::set_permissions(&policy, fs::Permissions::from_mode(0o700)).unwrap();
        let record = policy.join("status.json");
        fs::write(&record, r#"{"active":true,"preset":"quiet"}"#).unwrap();
        fs::set_permissions(&record, fs::Permissions::from_mode(0o644)).unwrap();
        // SAFETY: geteuid has no memory arguments or side effects.
        let owner = unsafe { libc::geteuid() };
        let root = File::open(&path).unwrap();
        let value = snapshot(&root, owner, owner).unwrap();
        assert_eq!(value["policy"]["preset"], "quiet");
        assert_eq!(value["fan"], Value::Null);
        assert_eq!(value["tablet"], Value::Null);
        assert_eq!(value["power"], Value::Null);
        let power = path.join("power.json");
        fs::write(
            &power,
            r#"{"source":"broker-power-state","baseline_uw":[12000000,15000000]}"#,
        )
        .unwrap();
        fs::set_permissions(&power, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            snapshot(&root, owner, owner).unwrap()["power"]["baseline_uw"][0],
            12000000
        );
        fs::remove_file(&power).unwrap();
        symlink(&record, &power).unwrap();
        assert!(snapshot(&root, owner, owner).is_err());
        fs::remove_file(&power).unwrap();

        fs::set_permissions(&record, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(snapshot(&root, owner, owner).is_err());
        fs::set_permissions(&record, fs::Permissions::from_mode(0o644)).unwrap();
        let moved = path.join("saved-policy");
        fs::rename(&policy, &moved).unwrap();
        symlink(&moved, &policy).unwrap();
        assert!(snapshot(&root, owner, owner).is_err());
        fs::remove_file(&policy).unwrap();
        fs::rename(&moved, &policy).unwrap();
        fs::set_permissions(&policy, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(snapshot(&root, owner, owner).is_err());
        fs::set_permissions(&policy, fs::Permissions::from_mode(0o700)).unwrap();
        let outside = path.join("private-fixture");
        fs::write(&outside, "must not be parsed or included in an error").unwrap();
        symlink(&outside, path.join("fan.json")).unwrap();
        let error = snapshot(&root, owner, owner).unwrap_err().to_string();
        assert!(!error.contains("must not be parsed"));
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn diagnostics_never_follow_worker_links_or_read_devices_unbounded_data() {
        let dir = std::env::temp_dir().join(format!(
            "minibookx-status-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let owner = unsafe { libc::geteuid() };
        let root = File::open(&dir).unwrap();
        check_directory(&root, owner).unwrap();
        let status = dir.join("status.json");
        let secret = dir.join("benign-private-fixture");
        fs::write(&secret, "not-json-private-data").unwrap();
        symlink(&secret, &status).unwrap();
        assert!(read_record(&root, c"status.json", owner).is_err());
        fs::remove_file(&status).unwrap();
        fs::hard_link(&secret, &status).unwrap();
        assert!(read_record(&root, c"status.json", owner).is_err());
        fs::remove_file(&status).unwrap();
        let leaf = std::ffi::CString::new(status.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: test-owned temporary FIFO, never a real system device.
        assert_eq!(unsafe { libc::mkfifo(leaf.as_ptr(), 0o600) }, 0);
        assert!(read_record(&root, c"status.json", owner).is_err());
        fs::remove_file(&status).unwrap();
        fs::write(&status, vec![b' '; MAX_RECORD as usize + 1]).unwrap();
        assert!(read_record(&root, c"status.json", owner).is_err());
        fs::write(&status, "[]").unwrap();
        assert!(read_record(&root, c"status.json", owner).is_err());
        fs::write(&status, r#"{"state":"ok"}"#).unwrap();
        assert_eq!(
            read_record(&root, c"status.json", owner).unwrap()["state"],
            "ok"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
