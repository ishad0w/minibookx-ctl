//! Fixed, read-only recognition of the package RAPL sysfs ABI.
//!
//! Constraint numbers are not identities: match the exported names. This
//! module deliberately exposes no write, MSR, enable or time-window operation.
use std::{
    fs::OpenOptions,
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadError {
    Absent,
    Permission,
    Invalid,
    Other,
}
impl From<io::Error> for ReadError {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => Self::Absent,
            io::ErrorKind::PermissionDenied => Self::Permission,
            io::ErrorKind::InvalidData => Self::Invalid,
            _ => Self::Other,
        }
    }
}
impl From<ReadError> for io::Error {
    fn from(error: ReadError) -> Self {
        let kind = match error {
            ReadError::Absent => io::ErrorKind::NotFound,
            ReadError::Permission => io::ErrorKind::PermissionDenied,
            ReadError::Invalid => io::ErrorKind::InvalidData,
            ReadError::Other => io::ErrorKind::Other,
        };
        Self::new(kind, "unavailable or unrecognized package RAPL data")
    }
}

pub(crate) fn read_file(path: &Path) -> Result<String, ReadError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(ReadError::from)?;
    if !file.metadata().map_err(ReadError::from)?.is_file() {
        return Err(ReadError::Invalid);
    }
    let mut value = String::new();
    file.take(257)
        .read_to_string(&mut value)
        .map_err(ReadError::from)?;
    if value.len() > 256 {
        return Err(ReadError::Invalid);
    }
    Ok(value)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    indices: [usize; 2],
}
impl Layout {
    pub(crate) fn limit_paths(&self, root: &Path) -> [PathBuf; 2] {
        self.indices
            .map(|index| root.join(format!("constraint_{index}_power_limit_uw")))
    }
    fn window_paths(&self, root: &Path) -> [PathBuf; 2] {
        self.indices
            .map(|index| root.join(format!("constraint_{index}_time_window_us")))
    }
}

pub(crate) fn discover(
    root: &Path,
    read: &mut impl FnMut(&Path) -> Result<String, ReadError>,
) -> Result<Layout, ReadError> {
    if read(&root.join("name"))?.trim() != "package-0" {
        return Err(ReadError::Invalid);
    }
    let mut indices = [None; 2];
    // The tested Intel interface has long_term, short_term and optional
    // peak_power. An unreadable name could conceal a duplicate: fail closed.
    for index in 0..3 {
        let name = match read(&root.join(format!("constraint_{index}_name"))) {
            Ok(name) => name,
            Err(ReadError::Absent) => continue,
            Err(error) => return Err(error),
        };
        let slot = match name.trim() {
            "long_term" => 0,
            "short_term" => 1,
            _ => continue,
        };
        if indices[slot].replace(index).is_some() {
            return Err(ReadError::Invalid);
        }
    }
    Ok(Layout {
        indices: [
            indices[0].ok_or(ReadError::Absent)?,
            indices[1].ok_or(ReadError::Absent)?,
        ],
    })
}

fn number(value: Result<String, ReadError>) -> Result<u64, ReadError> {
    value?.trim().parse().map_err(|_| ReadError::Invalid)
}

pub(crate) struct Observation {
    pub(crate) limits_uw: [Result<u64, ReadError>; 2],
    pub(crate) windows_us: [Result<u64, ReadError>; 2],
    pub(crate) package_enabled: Result<bool, ReadError>,
}

pub(crate) fn observe(
    root: &Path,
    read: &mut impl FnMut(&Path) -> Result<String, ReadError>,
) -> Observation {
    let package_enabled = number(read(&root.join("enabled"))).and_then(|value| match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(ReadError::Invalid),
    });
    let (limits_uw, windows_us) = match discover(root, read) {
        Ok(layout) => (
            layout.limit_paths(root).map(|path| {
                number(read(&path)).and_then(|value| {
                    (value <= 1_000_000_000)
                        .then_some(value)
                        .ok_or(ReadError::Invalid)
                })
            }),
            layout.window_paths(root).map(|path| number(read(&path))),
        ),
        Err(error) => ([Err(error); 2], [Err(error); 2]),
    };
    Observation {
        limits_uw,
        windows_us,
        package_enabled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn observations_keep_optional_abi_fields_unknown_and_never_assume_constraint_order() {
        let root = Path::new("/fixture");
        let mut files = BTreeMap::from([
            ("name", "package-0"),
            ("constraint_0_name", "short_term"),
            ("constraint_1_name", "long_term"),
            ("constraint_0_power_limit_uw", "15000000"),
            ("constraint_1_power_limit_uw", "12000000"),
            ("constraint_1_time_window_us", "28000000"),
        ]);
        let observation = |files: &BTreeMap<&str, &str>| {
            observe(root, &mut |path| {
                files
                    .get(path.file_name().unwrap().to_str().unwrap())
                    .map(|value| (*value).to_owned())
                    .ok_or(ReadError::Absent)
            })
        };
        let observed = observation(&files);
        assert_eq!(observed.limits_uw, [Ok(12_000_000), Ok(15_000_000)]);
        assert_eq!(
            observed.windows_us,
            [Ok(28_000_000), Err(ReadError::Absent)]
        );
        assert_eq!(observed.package_enabled, Err(ReadError::Absent));
        files.insert("enabled", "2");
        files.insert("constraint_0_power_limit_uw", "-1");
        let observed = observation(&files);
        assert_eq!(observed.package_enabled, Err(ReadError::Invalid));
        assert_eq!(observed.limits_uw[1], Err(ReadError::Invalid));
        files.insert("constraint_2_name", "long_term");
        assert_eq!(observation(&files).limits_uw, [Err(ReadError::Invalid); 2]);
    }

    #[test]
    fn unreadable_optional_names_invalidate_the_whole_layout() {
        let mut read = |path: &Path| match path.file_name().unwrap().to_str().unwrap() {
            "name" => Ok("package-0".into()),
            "constraint_0_name" => Ok("long_term".into()),
            "constraint_1_name" => Ok("short_term".into()),
            _ => Err(ReadError::Permission),
        };
        assert_eq!(
            discover(Path::new("/fixture"), &mut read),
            Err(ReadError::Permission)
        );
    }

    #[test]
    fn file_reader_bounds_data_and_rejects_special_or_redirected_leaves() {
        use std::os::unix::fs::symlink;
        let root =
            std::env::temp_dir().join(format!("minibookx-rapl-reader-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let value = root.join("value");
        std::fs::write(&value, "12000000\n").unwrap();
        assert_eq!(read_file(&value).unwrap(), "12000000\n");
        symlink(&value, root.join("redirect")).unwrap();
        assert!(read_file(&root.join("redirect")).is_err());
        assert_eq!(read_file(Path::new("/dev/null")), Err(ReadError::Invalid));
        std::fs::write(&value, vec![b'1'; 257]).unwrap();
        assert_eq!(read_file(&value), Err(ReadError::Invalid));
        std::fs::write(&value, [0xff]).unwrap();
        assert_eq!(read_file(&value), Err(ReadError::Invalid));
        std::fs::remove_dir_all(root).unwrap();
    }
}
