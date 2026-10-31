//! Shared hardware identity and fixed system-tool locations.
use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub const KNOWN_BIOS: &str = "ZW.RP11D.26060101.009";

pub(crate) fn supported_profile(vendor: &str, model: &str, bios: &str, cpuinfo: &str) -> bool {
    vendor.starts_with("CHUWI")
        && model.trim() == "MiniBook X"
        && bios.trim() == KNOWN_BIOS
        && cpuinfo.contains("Intel(R) Processor U300")
}

pub(crate) fn profile_mismatch(
    vendor: &str,
    model: &str,
    bios: &str,
    cpuinfo: &str,
) -> Option<String> {
    if supported_profile(vendor, model, bios, cpuinfo) {
        return None;
    }
    let mut mismatches = Vec::new();
    if !vendor.starts_with("CHUWI") {
        mismatches.push(format!("manufacturer {:?}, expected CHUWI", vendor.trim()));
    }
    if model.trim() != "MiniBook X" {
        mismatches.push(format!("model {:?}, expected MiniBook X", model.trim()));
    }
    if !cpuinfo.contains("Intel(R) Processor U300") {
        let name = cpuinfo
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == "model name").then_some(value.trim())
            })
            .unwrap_or("unknown");
        mismatches.push(format!("CPU {name:?}, expected Intel U300"));
    }
    // A BIOS revision is meaningful only for this board/CPU profile. Never
    // suggest the U300 firmware to an N100/N150 owner with the same model name.
    if mismatches.is_empty() && bios.trim() != KNOWN_BIOS {
        mismatches.push(format!(
            "BIOS {:?}, supported revision {KNOWN_BIOS}",
            bios.trim()
        ));
    }
    (!mismatches.is_empty()).then(|| {
        format!(
            "unsupported profile: {}; do not flash a BIOS to bypass this check",
            mismatches.join("; ")
        )
    })
}

pub(crate) fn check_profile() -> io::Result<()> {
    let vendor = fs::read_to_string("/sys/class/dmi/id/sys_vendor")?;
    let model = fs::read_to_string("/sys/class/dmi/id/product_name")?;
    let bios = fs::read_to_string("/sys/class/dmi/id/bios_version")?;
    let cpu = fs::read_to_string("/proc/cpuinfo")?;
    match profile_mismatch(&vendor, &model, &bios, &cpu) {
        Some(reason) => Err(io::Error::new(io::ErrorKind::InvalidData, reason)),
        None => Ok(()),
    }
}

/// Distribution package tools may live in either bin or sbin. Never consult
/// an inherited PATH; symlinks in these administrator-controlled directories
/// are normal on merged-/usr distributions.
pub(crate) fn system_tool(name: &str) -> io::Result<PathBuf> {
    find_tool(
        name,
        &["/usr/bin", "/usr/sbin", "/bin", "/sbin"].map(Path::new),
    )
}

fn find_tool(name: &str, directories: &[&Path]) -> io::Result<PathBuf> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "system tool must be a fixed executable name",
        ));
    }
    for directory in directories {
        let path = directory.join(name);
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 => {
                return Ok(path);
            }
            Ok(_) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("required system tool {name} is missing from bin/sbin"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn profile_checks_the_complete_supported_identity() {
        let cpu = "model name : Intel(R) Processor U300\n";
        assert!(supported_profile(
            "CHUWI\n",
            "MiniBook X\n",
            KNOWN_BIOS,
            cpu
        ));
        for (vendor, model, bios, cpu) in [
            ("Other", "MiniBook X", KNOWN_BIOS, cpu),
            ("CHUWI", "Other", KNOWN_BIOS, cpu),
            ("CHUWI", "MiniBook X", "other", cpu),
            ("CHUWI", "MiniBook X", KNOWN_BIOS, "Intel(R) N100"),
        ] {
            assert!(!supported_profile(vendor, model, bios, cpu));
        }
    }

    #[test]
    fn tools_can_live_in_sbin_and_must_be_executable_regular_files() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "minibookx-tools-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let bin = root.join("bin");
        let sbin = root.join("sbin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir(&sbin).unwrap();
        let directories = [bin.as_path(), sbin.as_path()];
        fs::create_dir(bin.join("modprobe")).unwrap();
        let executable = sbin.join("kmod");
        fs::write(&executable, "fixture; never executed").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink("kmod", sbin.join("modprobe")).unwrap();
        assert!(find_tool("modprobe", &directories).is_err());
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            find_tool("modprobe", &directories).unwrap(),
            sbin.join("modprobe")
        );
        for name in ["", "../modprobe", "/usr/bin/modprobe", "a/b"] {
            assert_eq!(
                find_tool(name, &directories).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
