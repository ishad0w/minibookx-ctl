//! Bounded, opt-in RAPL limits. Never writes MSRs, PL4, voltage or TCC.
use crate::{
    paths::{RAPL_PACKAGE, RUN_DIR},
    rapl,
};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub const MAX_PL1_W: u32 = 28;
pub const MAX_PL2_W: u32 = 30;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PowerConfig {
    pub enabled: bool,
    pub pl1_w: Option<u32>,
    pub pl2_w: Option<u32>,
}
impl PowerConfig {
    pub fn validate(&self) -> Result<(), String> {
        for (name, value, max) in [
            ("PL1", self.pl1_w, MAX_PL1_W),
            ("PL2", self.pl2_w, MAX_PL2_W),
        ] {
            if value.is_some_and(|w| !(5..=max).contains(&w)) {
                return Err(format!("{name} must be firmware or 5..={max} W"));
            }
        }
        if self.pl1_w.zip(self.pl2_w).is_some_and(|(a, b)| a > b) {
            return Err("PL1 must not exceed PL2".into());
        }
        Ok(())
    }
}

fn read_number(path: &Path) -> io::Result<u64> {
    rapl::read_file(path)?
        .trim()
        .parse()
        .map_err(io::Error::other)
}
pub(crate) fn validate_pair(pair: [u64; 2]) -> io::Result<()> {
    if pair[0] < 5_000_000
        || pair[0] > u64::from(MAX_PL1_W) * 1_000_000
        || pair[1] < pair[0]
        || pair[1] > u64::from(MAX_PL2_W) * 1_000_000
    {
        return Err(io::Error::other(
            "RAPL pair outside supported 5..28 / 5..30 W envelope",
        ));
    }
    Ok(())
}

fn validate_attributed_pair(
    current: [u64; 2],
    original: [u64; 2],
    before: [u64; 2],
    target: [u64; 2],
) -> io::Result<()> {
    validate_pair(current)?;
    for i in 0..2 {
        if ![original[i], before[i], target[i]].contains(&current[i]) {
            return Err(io::Error::other(
                "RAPL changed outside this controller; recovery refused",
            ));
        }
    }
    Ok(())
}

fn constraint_paths(root: &Path) -> io::Result<[PathBuf; 2]> {
    Ok(rapl::discover(root, &mut rapl::read_file)?.limit_paths(root))
}

pub struct Manager {
    package: PathBuf,
    journal: PathBuf,
    journal_owner: u32,
    paths: Option<[PathBuf; 2]>,
    original: Option<[u64; 2]>,
    last: Option<[u64; 2]>,
    owns_journal: bool,
    uncertain: bool,
    last_error: Option<String>,
}
impl Manager {
    pub fn new() -> Self {
        Self {
            package: PathBuf::from(RAPL_PACKAGE),
            journal: Path::new(RUN_DIR).join("power.lease"),
            journal_owner: 0,
            paths: None,
            original: None,
            last: None,
            owns_journal: false,
            uncertain: false,
            last_error: None,
        }
    }
    fn discover(&mut self) -> io::Result<()> {
        if self.paths.is_some() {
            return Ok(());
        }
        let root = &self.package;
        let paths = constraint_paths(root)?;
        let original = [read_number(&paths[0])?, read_number(&paths[1])?];
        validate_pair(original)?;
        self.paths = Some(paths);
        self.original = Some(original);
        self.last = Some(original);
        Ok(())
    }
    pub fn apply(&mut self, config: &PowerConfig) -> io::Result<()> {
        let result = self.apply_with(config, write_pair);
        self.last_error = result.as_ref().err().map(ToString::to_string);
        result
    }
    fn apply_with(
        &mut self,
        config: &PowerConfig,
        mut update_pair: impl FnMut(&[PathBuf; 2], [u64; 2], [u64; 2]) -> io::Result<()>,
    ) -> io::Result<()> {
        config.validate().map_err(io::Error::other)?;
        if !config.enabled {
            return self.restore();
        }
        if !self.owns_journal && self.pending_record()? {
            return Err(io::Error::other(
                "pending power recovery; limits left untouched until recovery succeeds",
            ));
        }
        self.discover()?;
        let original = self.original.unwrap();
        let target = [
            config
                .pl1_w
                .map_or(original[0], |v| u64::from(v) * 1_000_000),
            config
                .pl2_w
                .map_or(original[1], |v| u64::from(v) * 1_000_000),
        ];
        validate_pair(target)?;
        let paths = self.paths.as_ref().unwrap();
        let before = [read_number(&paths[0])?, read_number(&paths[1])?];
        if Some(before) != self.last {
            return Err(io::Error::other(
                "another process changed RAPL limits; refusing to compete",
            ));
        }
        if target == before {
            return Ok(());
        }
        // Persist attribution BEFORE any potentially successful sysfs write.
        write_journal(&self.journal, original, before, target)?;
        self.owns_journal = true;
        self.uncertain = true;
        if let Err(e) = update_pair(paths, before, target) {
            let current = [read_number(&paths[0])?, read_number(&paths[1])?];
            let rollback = validate_attributed_pair(current, original, before, target)
                .and_then(|()| update_pair(paths, current, before));
            if rollback.is_ok() {
                self.last = Some(before);
                self.uncertain = false;
            }
            return Err(io::Error::other(format!(
                "RAPL update failed: {e}; rollback={rollback:?}"
            )));
        }
        self.last = Some(target);
        self.uncertain = false;
        eprintln!(
            "power limits applied: PL1={}W PL2={}W",
            target[0] / 1_000_000,
            target[1] / 1_000_000
        );
        Ok(())
    }
    pub fn uncertain(&self) -> bool {
        self.uncertain
    }
    fn pending_record(&self) -> io::Result<bool> {
        match fs::symlink_metadata(&self.journal) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
    pub fn restoration_pending(&self) -> bool {
        self.owns_journal || self.pending_record().unwrap_or(true)
    }
    /// Broker-owned attribution, not a fresh hardware reading. A successful
    /// write/readback proves the number was accepted, not thermal enforcement.
    pub fn diagnostic_status(&self) -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "source": "broker-power-state",
            "baseline_uw": self.original,
            "last_applied_uw": self.last,
            "restoration_pending": self.restoration_pending(),
            "uncertain": self.uncertain,
            "error": self.last_error,
        })
    }
    pub fn restore(&mut self) -> io::Result<()> {
        if !self.owns_journal {
            return Ok(());
        }
        restore_at(&self.package, &self.journal, self.journal_owner)?;
        self.owns_journal = false;
        if let Some(original) = self.original {
            self.last = Some(original)
        }
        Ok(())
    }
}
impl Default for Manager {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            eprintln!("power restore failed: {e}")
        }
    }
}

fn write_pair(paths: &[PathBuf; 2], current: [u64; 2], target: [u64; 2]) -> io::Result<()> {
    // Preserve PL1 <= PL2 throughout the two-file update.
    let order = if target[1] < current[0] {
        [0, 1]
    } else {
        [1, 0]
    };
    for i in order {
        let observed = read_number(&paths[i])?;
        if observed == target[i] {
            continue;
        }
        if observed != current[i] {
            return Err(io::Error::other(
                "RAPL changed outside this controller; update refused",
            ));
        }
        fs::write(&paths[i], format!("{}\n", target[i]))?;
        if read_number(&paths[i])? != target[i] {
            return Err(io::Error::other("RAPL readback mismatch"));
        }
    }
    Ok(())
}
fn write_journal(
    journal: &Path,
    original: [u64; 2],
    before: [u64; 2],
    target: [u64; 2],
) -> io::Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let temp = journal.with_extension("tmp");
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    let text = format!(
        "v1\n{} {} {} {} {} {}\n",
        original[0], original[1], before[0], before[1], target[0], target[1]
    );
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    fs::rename(temp, journal)
}

pub fn restore_journal() -> io::Result<()> {
    restore_at(
        Path::new(RAPL_PACKAGE),
        &Path::new(RUN_DIR).join("power.lease"),
        0,
    )
}

fn restore_at(package: &Path, journal: &Path, owner: u32) -> io::Result<()> {
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(journal)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let m = file.metadata()?;
    if !m.is_file() || m.uid() != owner || m.nlink() != 1 || m.mode() & 0o077 != 0 || m.len() > 256
    {
        return Err(io::Error::other("untrusted power journal"));
    }
    let mut text = String::new();
    file.take(257).read_to_string(&mut text)?;
    let mut fields = text.split_whitespace();
    if fields.next() != Some("v1") {
        return Err(io::Error::other("invalid power journal"));
    }
    let values: Vec<u64> = fields
        .map(|s| s.parse().map_err(io::Error::other))
        .collect::<io::Result<_>>()?;
    if values.len() != 6 {
        return Err(io::Error::other("invalid power journal length"));
    }
    let original = [values[0], values[1]];
    let before = [values[2], values[3]];
    let target = [values[4], values[5]];
    for p in [original, before, target] {
        validate_pair(p)?
    }
    let mut manager = Manager::new();
    manager.package = package.to_path_buf();
    manager.discover()?;
    let paths = manager.paths.take().unwrap();
    let current = [read_number(&paths[0])?, read_number(&paths[1])?];
    validate_attributed_pair(current, original, before, target)?;
    write_pair(&paths, current, original)?;
    fs::remove_file(journal)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn orphaned_or_foreign_power_state_quarantines_only_opt_in_writes() {
        let fixture = ConstraintFixture::new(&["long_term", "short_term"]);
        let mut manager = fixture.manager();
        let journal = manager.journal.clone();
        write_journal(
            &journal,
            [12_000_000, 15_000_000],
            [12_000_000, 15_000_000],
            [14_000_000, 18_000_000],
        )
        .unwrap();
        let paths = constraint_paths(&fixture.0).unwrap();
        fs::write(&paths[0], "13000000").unwrap();
        assert!(restore_at(&fixture.0, &journal, unsafe { libc::geteuid() }).is_err());
        assert!(journal.exists());
        assert!(
            manager
                .apply(&PowerConfig {
                    enabled: true,
                    pl1_w: Some(14),
                    pl2_w: Some(18)
                })
                .is_err()
        );
        manager.apply(&PowerConfig::default()).unwrap();
        assert_eq!(fs::read_to_string(&paths[0]).unwrap(), "13000000");
        assert_eq!(manager.diagnostic_status()["restoration_pending"], true);
    }

    struct ConstraintFixture(PathBuf);

    impl ConstraintFixture {
        fn new(names: &[&str]) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "minibookx-power-constraints-{}-{id}",
                std::process::id()
            ));
            fs::create_dir(&root).unwrap();
            fs::write(root.join("name"), "package-0\n").unwrap();
            for (i, name) in names.iter().enumerate() {
                fs::write(root.join(format!("constraint_{i}_name")), name).unwrap();
            }
            Self(root)
        }

        fn manager(&self) -> Manager {
            fs::write(self.0.join("name"), "package-0\n").unwrap();
            for (path, value) in constraint_paths(&self.0)
                .unwrap()
                .into_iter()
                .zip([12_000_000, 15_000_000])
            {
                fs::write(path, value.to_string()).unwrap();
            }
            let mut manager = Manager::new();
            manager.package = self.0.clone();
            manager.journal = self.0.join("power.lease");
            // Production always requires root ownership. Only these private
            // filesystem fixtures substitute the ordinary test user's UID.
            manager.journal_owner = unsafe { libc::geteuid() };
            manager
        }

        fn limits(&self) -> [u64; 2] {
            constraint_paths(&self.0)
                .unwrap()
                .map(|path| read_number(&path).unwrap())
        }
    }

    impl Drop for ConstraintFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn two_constraint_package_does_not_require_peak_power() {
        let fixture = ConstraintFixture::new(&["long_term\n", "short_term\n"]);
        assert!(!fixture.0.join("constraint_2_name").exists());
        assert_eq!(
            constraint_paths(&fixture.0).unwrap(),
            [
                fixture.0.join("constraint_0_power_limit_uw"),
                fixture.0.join("constraint_1_power_limit_uw"),
            ]
        );
    }

    #[test]
    fn constraint_names_select_limits_and_ignore_optional_peak() {
        let fixture = ConstraintFixture::new(&["short_term\n", "long_term\n", "peak_power\n"]);
        assert_eq!(
            constraint_paths(&fixture.0).unwrap(),
            [
                fixture.0.join("constraint_1_power_limit_uw"),
                fixture.0.join("constraint_0_power_limit_uw"),
            ]
        );
    }

    #[test]
    fn missing_or_duplicate_required_constraints_are_rejected() {
        for names in [
            vec!["long_term", "peak_power"],
            vec!["short_term", "peak_power"],
            vec!["long_term", "short_term", "long_term"],
            vec!["long_term", "short_term", "short_term"],
        ] {
            let fixture = ConstraintFixture::new(&names);
            assert!(constraint_paths(&fixture.0).is_err(), "{names:?}");
        }
    }

    #[test]
    fn unreadable_optional_constraint_is_not_treated_as_absent() {
        let fixture = ConstraintFixture::new(&["long_term", "short_term"]);
        // A directory produces a real read error even when tests run as root.
        fs::create_dir(fixture.0.join("constraint_2_name")).unwrap();
        let error = constraint_paths(&fixture.0).unwrap_err();
        assert_ne!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn rejects_unbounded_or_inverted_user_limits() {
        for (a, b) in [(29, 30), (28, 31), (4, 15), (20, 15)] {
            assert!(
                PowerConfig {
                    enabled: true,
                    pl1_w: Some(a),
                    pl2_w: Some(b)
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            PowerConfig {
                enabled: true,
                pl1_w: Some(28),
                pl2_w: Some(30)
            }
            .validate()
            .is_ok()
        );
        assert!(PowerConfig::default().validate().is_ok());
    }
    #[test]
    fn disabled_policy_performs_no_hardware_access() {
        let mut m = Manager::new();
        m.apply(&PowerConfig::default()).unwrap();
        assert!(m.paths.is_none());
    }

    #[test]
    fn applied_limits_reload_and_disable_restore_the_captured_pair() {
        let fixture = ConstraintFixture::new(&["short_term", "long_term", "peak_power"]);
        let peak = fixture.0.join("constraint_2_power_limit_uw");
        fs::write(&peak, "40000000\n").unwrap();
        let enabled = fixture.0.join("enabled");
        let window = fixture.0.join("constraint_1_time_window_us");
        fs::write(&enabled, "1\n").unwrap();
        fs::write(&window, "28000000\n").unwrap();
        let mut manager = fixture.manager();
        assert!(manager.diagnostic_status()["baseline_uw"].is_null());
        let config = PowerConfig {
            enabled: true,
            pl1_w: Some(8),
            pl2_w: Some(10),
        };
        manager.apply(&config).unwrap();
        assert_eq!(
            manager.diagnostic_status()["baseline_uw"],
            serde_json::json!([12_000_000, 15_000_000])
        );
        assert_eq!(
            manager.diagnostic_status()["last_applied_uw"],
            serde_json::json!([8_000_000, 10_000_000])
        );
        assert_eq!(manager.diagnostic_status()["restoration_pending"], true);
        assert_eq!(fixture.limits(), [8_000_000, 10_000_000]);
        let journal = fs::read(&manager.journal).unwrap();
        manager.apply(&config).unwrap();
        assert_eq!(fs::read(&manager.journal).unwrap(), journal);
        manager
            .apply(&PowerConfig {
                pl1_w: Some(14),
                pl2_w: Some(20),
                ..config
            })
            .unwrap();
        assert_eq!(fixture.limits(), [14_000_000, 20_000_000]);
        manager.apply(&PowerConfig::default()).unwrap();
        assert_eq!(
            manager.diagnostic_status()["last_applied_uw"],
            serde_json::json!([12_000_000, 15_000_000])
        );
        assert_eq!(manager.diagnostic_status()["restoration_pending"], false);
        assert_eq!(fixture.limits(), [12_000_000, 15_000_000]);
        assert!(!manager.journal.exists());
        assert_eq!(fs::read_to_string(peak).unwrap(), "40000000\n");
        assert_eq!(fs::read_to_string(enabled).unwrap(), "1\n");
        assert_eq!(fs::read_to_string(window).unwrap(), "28000000\n");
    }

    #[test]
    fn orphan_recovery_restores_an_attributed_half_written_pair() {
        let fixture = ConstraintFixture::new(&["long_term", "short_term"]);
        let manager = fixture.manager();
        write_journal(
            &manager.journal,
            [12_000_000, 15_000_000],
            [12_000_000, 15_000_000],
            [8_000_000, 10_000_000],
        )
        .unwrap();
        // Simulate termination after PL1 was lowered and before PL2 was set.
        fs::write(fixture.0.join("constraint_0_power_limit_uw"), "8000000\n").unwrap();
        restore_at(&manager.package, &manager.journal, manager.journal_owner).unwrap();
        assert_eq!(fixture.limits(), [12_000_000, 15_000_000]);
        assert!(!manager.journal.exists());
    }

    #[test]
    fn failed_second_write_does_not_roll_back_an_observed_external_change() {
        let fixture = ConstraintFixture::new(&["long_term", "short_term"]);
        let mut manager = fixture.manager();
        let config = PowerConfig {
            enabled: true,
            pl1_w: Some(8),
            pl2_w: Some(10),
        };
        let mut attempts = 0;
        let error = manager
            .apply_with(&config, |paths, current, target| {
                attempts += 1;
                if attempts == 1 {
                    assert_eq!(current, [12_000_000, 15_000_000]);
                    assert_eq!(target, [8_000_000, 10_000_000]);
                    // This lowering operation writes PL1 first. Model its accepted
                    // write, a failed PL2 write, then a foreign PL1 before rollback.
                    fs::write(&paths[0], "8000000\n")?;
                    fs::write(&paths[0], "7000000\n")?;
                    Err(io::Error::other("injected second constraint write failure"))
                } else {
                    write_pair(paths, current, target)
                }
            })
            .unwrap_err();
        assert!(error.to_string().contains("recovery refused"));
        assert_eq!(attempts, 1, "foreign state must prevent any rollback write");
        assert_eq!(fixture.limits(), [7_000_000, 15_000_000]);
        assert!(manager.uncertain());
        assert!(manager.owns_journal && manager.journal.exists());
        assert_eq!(manager.last, Some([12_000_000, 15_000_000]));
        assert!(manager.restore().is_err());
        assert_eq!(fixture.limits(), [7_000_000, 15_000_000]);
        assert!(manager.journal.exists());
        manager.owns_journal = false;
    }

    #[test]
    fn failed_second_write_rolls_back_an_attributed_partial_update() {
        let fixture = ConstraintFixture::new(&["long_term", "short_term"]);
        let mut manager = fixture.manager();
        let config = PowerConfig {
            enabled: true,
            pl1_w: Some(8),
            pl2_w: Some(10),
        };
        let mut attempts = 0;
        let error = manager
            .apply_with(&config, |paths, current, target| {
                attempts += 1;
                if attempts == 1 {
                    fs::write(&paths[0], "8000000\n")?;
                    Err(io::Error::other("injected second constraint write failure"))
                } else {
                    assert_eq!(current, [8_000_000, 15_000_000]);
                    assert_eq!(target, [12_000_000, 15_000_000]);
                    write_pair(paths, current, target)
                }
            })
            .unwrap_err();
        assert!(error.to_string().contains("rollback=Ok(())"));
        assert_eq!(attempts, 2);
        assert_eq!(fixture.limits(), [12_000_000, 15_000_000]);
        assert!(!manager.uncertain());
        assert_eq!(manager.last, Some([12_000_000, 15_000_000]));
        manager.restore().unwrap();
        assert!(!manager.journal.exists());
    }

    #[test]
    fn each_constraint_write_refuses_foreign_values_but_accepts_its_target() {
        let fixture = ConstraintFixture::new(&["long_term", "short_term"]);
        let _manager = fixture.manager();
        let paths = constraint_paths(&fixture.0).unwrap();
        let current = [12_000_000, 15_000_000];
        let target = [8_000_000, 10_000_000];

        fs::write(&paths[0], "7000000\n").unwrap();
        assert!(write_pair(&paths, current, target).is_err());
        assert_eq!(fixture.limits(), [7_000_000, 15_000_000]);

        // A conflict at the second constraint must remain untouched even if
        // the first constraint has already been successfully updated.
        fs::write(&paths[0], "12000000\n").unwrap();
        fs::write(&paths[1], "13000000\n").unwrap();
        assert!(write_pair(&paths, current, target).is_err());
        assert_eq!(fixture.limits(), [8_000_000, 13_000_000]);

        // An already-applied target is safe to skip, including during retry.
        fs::write(&paths[1], "15000000\n").unwrap();
        write_pair(&paths, current, target).unwrap();
        assert_eq!(fixture.limits(), target);
    }

    #[test]
    fn external_limit_changes_are_not_clobbered_and_keep_recovery_evidence() {
        let fixture = ConstraintFixture::new(&["long_term", "short_term"]);
        let mut manager = fixture.manager();
        let config = PowerConfig {
            enabled: true,
            pl1_w: Some(8),
            pl2_w: Some(10),
        };
        manager.apply(&config).unwrap();
        fs::write(fixture.0.join("constraint_0_power_limit_uw"), "7000000\n").unwrap();
        assert!(manager.apply(&config).is_err());
        assert!(manager.restore().is_err());
        assert_eq!(fixture.limits(), [7_000_000, 10_000_000]);
        assert!(manager.journal.exists());
        manager.owns_journal = false;
    }

    #[test]
    fn recovery_rejects_an_exposed_journal_and_can_retry_after_permissions_are_fixed() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = ConstraintFixture::new(&["long_term", "short_term"]);
        let mut manager = fixture.manager();
        manager
            .apply(&PowerConfig {
                enabled: true,
                pl1_w: Some(8),
                pl2_w: Some(10),
            })
            .unwrap();
        fs::set_permissions(&manager.journal, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(manager.restore().is_err());
        assert_eq!(fixture.limits(), [8_000_000, 10_000_000]);
        assert!(manager.journal.exists());
        fs::set_permissions(&manager.journal, fs::Permissions::from_mode(0o600)).unwrap();
        manager.restore().unwrap();
        assert_eq!(fixture.limits(), [12_000_000, 15_000_000]);
    }
}
