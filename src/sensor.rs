use std::{
    fs::{self, File},
    io,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};

pub struct CpuSensor {
    files: Vec<(PathBuf, File)>,
}

impl CpuSensor {
    /// Discover package sensors by driver and label, never unstable hwmonN IDs.
    pub fn discover(root: &Path) -> io::Result<Self> {
        let mut files = Vec::new();
        for entry in fs::read_dir(root)? {
            let dir = entry?.path();
            if fs::read_to_string(dir.join("name"))
                .ok()
                .is_none_or(|s| s.trim() != "coretemp")
            {
                continue;
            }
            for attr in fs::read_dir(&dir)? {
                let path = attr?.path();
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !name.starts_with("temp") || !name.ends_with("_label") {
                    continue;
                }
                if fs::read_to_string(&path)?.trim().starts_with("Package id ") {
                    let input = dir.join(name.replace("_label", "_input"));
                    files.push((input.clone(), File::open(input)?));
                }
            }
        }
        if files.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no coretemp package sensor",
            ));
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Self { files })
    }

    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.files.iter().map(|(p, _)| p.as_path())
    }

    /// Keep descriptors open and use stack buffers. No per-sample discovery,
    /// child processes, EC transactions or heap allocation on success.
    pub fn read_mc(&self) -> io::Result<i32> {
        let mut highest = i32::MIN;
        for (_, file) in &self.files {
            let mut buf = [0_u8; 32];
            let n = file.read_at(&mut buf, 0)?;
            let value = std::str::from_utf8(&buf[..n])
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .filter(|t| (1_000..=125_000).contains(t))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid package temperature")
                })?;
            highest = highest.max(value);
        }
        Ok(highest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovers_real_package_and_reads_updates_through_one_descriptor() {
        let root = std::env::temp_dir().join(format!("minibook-fand-test-{}", std::process::id()));
        fs::create_dir_all(root.join("hwmon93")).unwrap();
        fs::create_dir_all(root.join("hwmon4")).unwrap();
        fs::write(root.join("hwmon93/name"), "coretemp\n").unwrap();
        fs::write(root.join("hwmon93/temp1_label"), "Package id 0\n").unwrap();
        fs::write(root.join("hwmon93/temp1_input"), "41000\n").unwrap();
        fs::write(root.join("hwmon4/name"), "acpitz\n").unwrap();
        fs::write(root.join("hwmon4/temp1_label"), "Package id 0\n").unwrap();
        fs::write(root.join("hwmon4/temp1_input"), "27800\n").unwrap();
        let sensor = CpuSensor::discover(&root).unwrap();
        assert_eq!(sensor.read_mc().unwrap(), 41000);
        fs::write(root.join("hwmon93/temp1_input"), "75000\n").unwrap();
        assert_eq!(sensor.read_mc().unwrap(), 75000);
        fs::write(root.join("hwmon93/temp1_input"), "-274000\n").unwrap();
        assert!(sensor.read_mc().is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
