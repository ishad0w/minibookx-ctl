use super::*;
use std::{
    os::unix::fs::symlink,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    target: Target,
    source: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "minibookx-install-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let target = Target::new(Some(&root)).unwrap();
        let source = root.join("source-elf");
        fs::write(&source, b"\x7fELFnew-image").unwrap();
        Self { target, source }
    }
    fn write(&self, path: &str, data: &[u8], mode: u32) {
        self.target
            .write(
                path,
                &Snapshot {
                    data: data.into(),
                    mode,
                    gid: 0,
                },
            )
            .unwrap();
    }
    fn bytes(&self, path: &str) -> Option<Vec<u8>> {
        self.target.capture(path).unwrap().map(|value| value.data)
    }
    fn install(&self) {
        apply(
            true,
            &Options::default(),
            &self.target,
            &self.source,
            &mut NoHost,
        )
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.target.root).unwrap();
    }
}

struct NoHost;
impl Host for NoHost {
    fn account(&mut self) -> io::Result<Account> {
        panic!("staging must not inspect accounts")
    }
    fn check_config(&mut self, _: Account, _: &[u8]) -> io::Result<()> {
        panic!("staging must not validate configuration")
    }
    fn state(&mut self, _: &str) -> io::Result<State> {
        panic!("staging must not inspect services")
    }
    fn control(&mut self, _: &[&str]) -> io::Result<()> {
        panic!("staging must not alter services")
    }
    fn recover(&mut self, _: &Path) -> io::Result<()> {
        panic!("staging must not touch hardware")
    }
}

struct FakeHost {
    state: State,
    recovery: State,
    events: Vec<String>,
    fail: Option<&'static str>,
    fail_recovery: bool,
    fail_recovery_number: Option<usize>,
    recoveries: usize,
    leave_lease: Option<PathBuf>,
    edit_config_after_validation: Option<PathBuf>,
    interrupt_on_start: bool,
}
impl FakeHost {
    fn running() -> Self {
        Self {
            state: State {
                loaded: true,
                active: true,
                transitioning: false,
                enabled: Enabled::Permanent,
            },
            recovery: State::default(),
            events: Vec::new(),
            fail: None,
            fail_recovery: false,
            fail_recovery_number: None,
            recoveries: 0,
            leave_lease: None,
            edit_config_after_validation: None,
            interrupt_on_start: false,
        }
    }
}
impl Host for FakeHost {
    fn account(&mut self) -> io::Result<Account> {
        self.events.push("account".into());
        Ok(Account { uid: 999, gid: 999 })
    }
    fn check_config(&mut self, _: Account, data: &[u8]) -> io::Result<()> {
        self.events.push("check config".into());
        check_config_reader(data)?;
        if let Some(path) = &self.edit_config_after_validation {
            fs::write(path, b"unknown_config_key = true\n")?;
        }
        Ok(())
    }
    fn state(&mut self, unit: &str) -> io::Result<State> {
        if unit == UNIT {
            Ok(self.state)
        } else if unit == RECOVERY_UNIT {
            Ok(self.recovery)
        } else {
            Ok(State::default())
        }
    }
    fn control(&mut self, args: &[&str]) -> io::Result<()> {
        let event = args.join(" ");
        self.events.push(event.clone());
        if self.fail == Some(event.as_str()) {
            self.fail = None;
            if args[0] == "start" {
                // A failed Restart=on-failure start normally leaves systemd
                // activating/auto-restart, rather than stably inactive.
                self.state = parse_state(
                    "LoadState=loaded\nActiveState=activating\nUnitFileState=enabled\n",
                )?;
            }
            return Err(refuse("injected command failure"));
        }
        match args[0] {
            "stop" if args[1] == RECOVERY_UNIT => self.recovery.active = false,
            "stop" => {
                self.state.active = false;
                self.state.transitioning = false;
            }
            "start" => self.state.active = true,
            "enable" => self.state.enabled = Enabled::Permanent,
            "disable" => self.state.enabled = Enabled::No,
            _ => (),
        }
        if args[0] == "start" && self.interrupt_on_start {
            self.interrupt_on_start = false;
            // The isolated helper below targets only its own blocked thread.
            assert_eq!(
                unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGINT) },
                0
            );
        }
        Ok(())
    }
    fn recover(&mut self, _: &Path) -> io::Result<()> {
        self.events.push("recover".into());
        self.recoveries += 1;
        if self.fail_recovery || self.fail_recovery_number == Some(self.recoveries) {
            return Err(refuse("injected recovery failure"));
        }
        if let Some(path) = &self.leave_lease {
            fs::write(path, b"remaining").unwrap();
        }
        Ok(())
    }
}

#[test]
fn staging_has_one_unit_and_preserves_config_on_update_and_uninstall() {
    let fixture = Fixture::new();
    fixture.install();
    let config_directory = fixture
        .target
        .path(CONFIG)
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    let metadata = fs::metadata(&config_directory).unwrap();
    assert_eq!(metadata.mode() & 0o777, 0o750);
    // Staging leaves ownership with its invoking user and primary group.
    assert_eq!(metadata.uid(), fixture.target.owner);
    assert_eq!(
        metadata.gid(),
        fs::metadata(&fixture.target.root).unwrap().gid()
    );
    fixture.write(CONFIG, b"preset = quiet\npower_control = false\n", 0o640);
    fixture.write(OLD_UNIT_FILE, b"old recovery", 0o644);
    fixture.install();
    assert_eq!(
        fixture.bytes(CONFIG).unwrap(),
        b"preset = quiet\npower_control = false\n"
    );
    assert!(fixture.bytes(OLD_UNIT_FILE).is_none());
    let units: Vec<_> = fs::read_dir(fixture.target.root.join("etc/systemd/system"))
        .unwrap()
        .collect();
    assert_eq!(units.len(), 1);
    assert_eq!(
        fixture.bytes(UNIT_FILE).unwrap(),
        include_bytes!("../../packaging/minibookx-ctl.service")
    );
    assert!(
        fixture
            .bytes(&format!("{DOC}/licenses/{}", BUNDLED_LICENSES[0].0))
            .is_some()
    );
    apply(
        false,
        &Options::default(),
        &fixture.target,
        &fixture.source,
        &mut NoHost,
    )
    .unwrap();
    assert!(fixture.bytes(BINARY).is_none());
    assert!(fixture.bytes(UNIT_FILE).is_none());
    assert!(fixture.bytes(CONFIG).is_some());
}

#[test]
fn refuses_symlink_file_and_parent_hardlink_and_writable_parent() {
    for variant in ["symlink", "parent", "hardlink", "writable"] {
        let fixture = Fixture::new();
        fixture.install();
        let binary = fixture.target.path(BINARY).unwrap();
        match variant {
            "symlink" => {
                fs::remove_file(&binary).unwrap();
                symlink(&fixture.source, &binary).unwrap();
            }
            "hardlink" => {
                fs::remove_file(&binary).unwrap();
                fs::hard_link(&fixture.source, &binary).unwrap();
            }
            "writable" => {
                fs::set_permissions(binary.parent().unwrap(), fs::Permissions::from_mode(0o777))
                    .unwrap()
            }
            "parent" => {
                let original = fixture.target.root.join("usr/local/bin");
                let other = fixture.target.root.join("elsewhere");
                fs::rename(&original, &other).unwrap();
                symlink(&other, &original).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            apply(
                true,
                &Options::default(),
                &fixture.target,
                &fixture.source,
                &mut NoHost
            )
            .is_err(),
            "{variant}"
        );
        assert_eq!(fs::read(&fixture.source).unwrap(), b"\x7fELFnew-image");
    }
}

#[test]
fn pending_legacy_transaction_is_not_interpreted_or_overwritten() {
    let fixture = Fixture::new();
    fixture.write(PENDING, b"unparsed legacy recovery data", 0o600);
    let error = apply(
        true,
        &Options::default(),
        &fixture.target,
        &fixture.source,
        &mut NoHost,
    )
    .unwrap_err();
    assert!(error.to_string().contains("legacy installer transaction"));
    assert_eq!(
        fixture.bytes(PENDING).unwrap(),
        b"unparsed legacy recovery data"
    );
    assert!(fixture.bytes(BINARY).is_none());
}

#[test]
fn simultaneous_installer_is_refused() {
    let fixture = Fixture::new();
    let _lock = fixture.target.lock().unwrap();
    assert!(
        apply(
            true,
            &Options::default(),
            &fixture.target,
            &fixture.source,
            &mut NoHost
        )
        .unwrap_err()
        .to_string()
        .contains("another installation")
    );
}

#[test]
fn finished_install_unlocks_even_if_a_forked_child_still_holds_the_description() {
    let fixture = Fixture::new();
    let lock = fixture.target.lock().unwrap();
    // dup shares flock ownership exactly as fork does, without forking a
    // multithreaded test process or running any child-side Rust code.
    let inherited = lock.0.try_clone().unwrap();
    drop(lock);
    let next = fixture.target.lock().unwrap();
    drop(inherited);
    drop(next);
}

#[test]
fn malformed_and_ambiguous_systemd_state_is_not_treated_as_stopped() {
    for value in [
        "LoadState=loaded\nActiveState=inactive\n",
        "LoadState=loaded\nActiveState=inactive\nUnitFileState=disabled\nUnitFileState=enabled\n",
        "LoadState=error\nActiveState=inactive\nUnitFileState=disabled\n",
        "LoadState=not-found\nActiveState=active\nUnitFileState=\n",
        "LoadState=loaded\nActiveState=inactive\nUnitFileState=unknown\n",
    ] {
        assert!(parse_state(value).is_err(), "{value}");
    }
    assert_eq!(
        parse_state("LoadState=not-found\nActiveState=inactive\nUnitFileState=\n").unwrap(),
        State::default()
    );
    assert!(
        !parse_state("LoadState=loaded\nActiveState=inactive\nUnitFileState=static\n")
            .unwrap()
            .active
    );
}

#[test]
fn installer_options_reject_ambiguous_or_live_staging_combinations() {
    for values in [
        vec!["--destdir"],
        vec!["--enable", "--enable"],
        vec!["--destdir", "/tmp/a", "--enable"],
        vec!["--unknown"],
    ] {
        assert!(Options::parse(true, values.into_iter().map(String::from)).is_err());
    }
    assert!(Options::parse(false, ["--enable"].into_iter().map(String::from)).is_err());
    let fixture = Fixture::new();
    let alias = fixture.target.root.join("alias");
    symlink(&fixture.target.root, &alias).unwrap();
    assert!(Target::new(Some(&alias)).is_err());
    assert!(Target::new(Some(Path::new("relative"))).is_err());
    for live_root in ["/", "//", "///"] {
        assert!(Target::new(Some(Path::new(live_root))).is_err());
    }
    // Root staging must not be reached through another user's renameable
    // directory, even when the final anchor itself happens to belong to root.
    // This fixture's parent is owned by the unprivileged test runner.
    if fixture.target.owner != 0 {
        assert!(Target::privileged_ancestors(&fixture.target.root.join("root-anchor")).is_err());
    }
}

#[test]
fn old_recovery_or_stop_failure_prevents_binary_replacement_and_restart() {
    for failure in ["stop", "recovery", "module.lease", "sensor.lease"] {
        let mut fixture = Fixture::new();
        fixture.install();
        fixture.write(BINARY, b"\x7fELFold-image", 0o755);
        let config_directory = fixture
            .target
            .path(CONFIG)
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        fs::set_permissions(&config_directory, fs::Permissions::from_mode(0o755)).unwrap();
        fixture.target.live = true;
        let mut host = FakeHost::running();
        if failure == "stop" {
            host.fail = Some("stop minibookx-ctl.service");
        }
        if failure == "recovery" {
            host.fail_recovery = true;
        }
        if matches!(failure, "module.lease" | "sensor.lease") {
            let path = fixture
                .target
                .path(&format!("/run/minibookx-ctl/{failure}"))
                .unwrap();
            fixture.target.parents(&path, true).unwrap();
            host.leave_lease = Some(path);
        }
        assert!(
            apply(
                true,
                &Options::default(),
                &fixture.target,
                &fixture.source,
                &mut host
            )
            .is_err()
        );
        assert_eq!(fixture.bytes(BINARY).unwrap(), b"\x7fELFold-image");
        assert_eq!(
            fs::metadata(config_directory).unwrap().mode() & 0o777,
            0o755
        );
        assert!(!host.events.iter().any(|event| event.starts_with("start ")));
        if let Some(path) = &host.leave_lease {
            assert!(
                path.exists(),
                "pending ownership must survive a refused update"
            );
        }
    }
}

#[test]
fn successful_live_update_recovers_before_writing_and_preserves_enabled_state() {
    let mut fixture = Fixture::new();
    fixture.install();
    let config = [
        include_bytes!("../../config/presets.conf").as_slice(),
        b"\n# User custom configuration\n",
    ]
    .concat();
    fixture.write(CONFIG, &config, 0o640);
    fixture.write(BINARY, b"\x7fELFold-image", 0o755);
    fixture.target.live = true;
    let mut host = FakeHost::running();
    apply(
        true,
        &Options::default(),
        &fixture.target,
        &fixture.source,
        &mut host,
    )
    .unwrap();
    assert_eq!(fixture.bytes(BINARY).unwrap(), b"\x7fELFnew-image");
    assert_eq!(fixture.bytes(CONFIG).unwrap(), config);
    assert_eq!(
        host.events,
        [
            "account",
            "check config",
            "stop minibookx-ctl.service",
            "recover",
            "daemon-reload",
            "reset-failed minibookx-ctl.service",
            "start minibookx-ctl.service"
        ]
    );
    assert_eq!(host.state.enabled, Enabled::Permanent);
}

#[test]
fn failed_new_start_recovers_then_restores_old_binary_and_running_state() {
    let mut fixture = Fixture::new();
    fixture.install();
    fixture.write(BINARY, b"\x7fELFold-image", 0o755);
    fixture.write(OLD_UNIT_FILE, b"old recovery unit", 0o644);
    let config_directory = fixture
        .target
        .path(CONFIG)
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    fs::set_permissions(&config_directory, fs::Permissions::from_mode(0o755)).unwrap();
    fixture.target.live = true;
    let mut host = FakeHost::running();
    host.fail = Some("start minibookx-ctl.service");
    let error = apply(
        true,
        &Options::default(),
        &fixture.target,
        &fixture.source,
        &mut host,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("previous files restored"),
        "{error}"
    );
    assert_eq!(fixture.bytes(BINARY).unwrap(), b"\x7fELFold-image");
    assert_eq!(fixture.bytes(OLD_UNIT_FILE).unwrap(), b"old recovery unit");
    assert_eq!(
        fs::metadata(config_directory).unwrap().mode() & 0o777,
        0o750
    );
    assert_eq!(host.recoveries, 2);
    assert!(host.state.active);
    assert_eq!(host.state.enabled, Enabled::Permanent);
}

#[test]
fn a_collected_inactive_unit_does_not_block_start_or_update() {
    let mut fixture = Fixture::new();
    fixture.install();
    fixture.target.live = true;
    let mut host = FakeHost::running();
    host.fail = Some("reset-failed minibookx-ctl.service");
    apply(
        true,
        &Options::default(),
        &fixture.target,
        &fixture.source,
        &mut host,
    )
    .unwrap();
    assert!(host.state.active);
    assert_eq!(fixture.bytes(BINARY).unwrap(), b"\x7fELFnew-image");
}

#[test]
fn a_new_failed_install_stops_auto_restart_and_removes_enabled_unit() {
    let mut fixture = Fixture::new();
    fixture.target.live = true;
    let mut host = FakeHost::running();
    host.state = State::default();
    host.fail = Some("start minibookx-ctl.service");
    let error = apply(
        true,
        &Options {
            enable: true,
            ..Options::default()
        },
        &fixture.target,
        &fixture.source,
        &mut host,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("previous files restored"),
        "{error}"
    );
    assert!(!host.state.active && !host.state.transitioning);
    assert_eq!(host.state.enabled, Enabled::No);
    assert!(fixture.bytes(BINARY).is_none());
    assert!(fixture.bytes(UNIT_FILE).is_none());
    assert_eq!(host.recoveries, 1);
}

#[test]
fn an_initially_transitioning_service_is_left_untouched() {
    let mut fixture = Fixture::new();
    fixture.install();
    fixture.target.live = true;
    let mut host = FakeHost::running();
    host.state =
        parse_state("LoadState=loaded\nActiveState=activating\nUnitFileState=enabled\n").unwrap();
    let error = apply(
        true,
        &Options::default(),
        &fixture.target,
        &fixture.source,
        &mut host,
    )
    .unwrap_err();
    assert!(error.to_string().contains("transitioning"));
    assert!(host.events.is_empty());
    assert_eq!(host.recoveries, 0);
}

#[test]
fn interrupted_install_restores_previous_files_and_service() {
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "install::tests::interrupted_install_helper",
            "--ignored",
        ])
        .env("MINIBOOKX_TEST_INSTALL_INTERRUPT", "1")
        .status()
        .unwrap();
    assert!(
        status.success(),
        "isolated interrupt/rollback test failed: {status}"
    );
}

#[test]
#[ignore = "subprocess helper for signal isolation"]
fn interrupted_install_helper() {
    if std::env::var_os("MINIBOOKX_TEST_INSTALL_INTERRUPT").is_none() {
        return;
    }
    let mut fixture = Fixture::new();
    fixture.install();
    fixture.write(BINARY, b"\x7fELFold-image", 0o755);
    fixture.target.live = true;
    let mut host = FakeHost::running();
    host.interrupt_on_start = true;
    let error = apply(
        true,
        &Options::default(),
        &fixture.target,
        &fixture.source,
        &mut host,
    )
    .unwrap_err();
    assert!(error.to_string().contains("interrupted"), "{error}");
    assert!(
        error.to_string().contains("previous files restored"),
        "{error}"
    );
    assert_eq!(fixture.bytes(BINARY).unwrap(), b"\x7fELFold-image");
    assert!(host.state.active);
    assert_eq!(host.recoveries, 2);
}

#[test]
fn failed_new_recovery_keeps_new_binary_available_and_never_restarts_old_runtime() {
    let mut fixture = Fixture::new();
    fixture.install();
    fixture.write(BINARY, b"\x7fELFold-image", 0o755);
    fixture.target.live = true;
    let mut host = FakeHost::running();
    host.fail = Some("start minibookx-ctl.service");
    host.fail_recovery_number = Some(2);
    let error = apply(
        true,
        &Options::default(),
        &fixture.target,
        &fixture.source,
        &mut host,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("current files retained"),
        "{error}"
    );
    assert_eq!(fixture.bytes(BINARY).unwrap(), b"\x7fELFnew-image");
    assert_eq!(
        host.events
            .iter()
            .filter(|event| event.as_str() == "start minibookx-ctl.service")
            .count(),
        1
    );
    assert!(!host.state.active);
}

#[test]
fn uninstall_stops_and_recovers_before_removal_and_retains_custom_config() {
    let mut fixture = Fixture::new();
    fixture.install();
    fixture.write(CONFIG, b"my configuration", 0o640);
    fixture.target.live = true;
    let mut host = FakeHost::running();
    apply(
        false,
        &Options::default(),
        &fixture.target,
        &fixture.source,
        &mut host,
    )
    .unwrap();
    assert_eq!(
        host.events,
        [
            "stop minibookx-ctl.service",
            "recover",
            "disable minibookx-ctl.service",
            "daemon-reload"
        ]
    );
    assert!(fixture.bytes(BINARY).is_none());
    assert_eq!(fixture.bytes(CONFIG).unwrap(), b"my configuration");
}

#[test]
fn legacy_recovery_unit_is_stopped_before_direct_recovery_and_removed_only_after_success() {
    for fail_stop in [false, true] {
        let mut fixture = Fixture::new();
        fixture.install();
        fixture.write(OLD_UNIT_FILE, b"legacy recovery unit", 0o644);
        fixture.target.live = true;
        let mut host = FakeHost::running();
        host.recovery = State {
            loaded: true,
            active: true,
            transitioning: false,
            enabled: Enabled::No,
        };
        if fail_stop {
            host.fail = Some("stop minibookx-ctl-recover.service");
        }
        let result = apply(
            true,
            &Options::default(),
            &fixture.target,
            &fixture.source,
            &mut host,
        );
        if fail_stop {
            assert!(result.is_err());
            assert!(fixture.bytes(OLD_UNIT_FILE).is_some());
            assert_eq!(host.recoveries, 0);
        } else {
            result.unwrap();
            assert!(fixture.bytes(OLD_UNIT_FILE).is_none());
            assert_eq!(
                &host.events[..5],
                [
                    "account",
                    "check config",
                    "stop minibookx-ctl.service",
                    "stop minibookx-ctl-recover.service",
                    "recover"
                ]
            );
        }
    }
}

#[test]
fn invalid_preserved_config_leaves_running_service_and_files_untouched() {
    let mut fixture = Fixture::new();
    fixture.install();
    let invalid = b"unknown_config_key = true\n";
    fixture.write(CONFIG, invalid, 0o640);
    fixture.write(BINARY, b"\x7fELFold-image", 0o755);
    fixture.target.live = true;
    // The worker has rejected the on-disk edit and still runs its last valid
    // policy. Its next start would persistently reject these same bytes.
    let mut host = FakeHost::running();
    let error = apply(
        true,
        &Options::default(),
        &fixture.target,
        &fixture.source,
        &mut host,
    )
    .unwrap_err();
    assert!(error.to_string().contains("unknown"), "{error}");
    assert_eq!(host.events, ["account", "check config"]);
    assert!(host.state.active);
    assert_eq!(host.recoveries, 0);
    assert_eq!(fixture.bytes(BINARY).unwrap(), b"\x7fELFold-image");
    assert_eq!(fixture.bytes(CONFIG).unwrap(), invalid);
}

#[test]
fn update_installs_the_same_config_snapshot_that_passed_validation() {
    let mut fixture = Fixture::new();
    fixture.install();
    let captured = fixture.bytes(CONFIG).unwrap();
    fixture.target.live = true;
    let mut host = FakeHost::running();
    host.edit_config_after_validation = Some(fixture.target.path(CONFIG).unwrap());
    apply(
        true,
        &Options::default(),
        &fixture.target,
        &fixture.source,
        &mut host,
    )
    .unwrap();
    assert!(host.state.active);
    assert_eq!(fixture.bytes(CONFIG).unwrap(), captured);
}

#[test]
fn config_snapshot_parser_rejects_empty_oversized_invalid_and_out_of_bounds_data() {
    let valid = include_bytes!("../../config/presets.conf");
    check_config_reader(valid.as_slice()).unwrap();
    for invalid in [
        Vec::new(),
        vec![b'#'; MAX_CONFIG as usize + 1],
        vec![0xff],
        b"unknown_config_key = true\n".to_vec(),
        String::from_utf8(valid.to_vec())
            .unwrap()
            .replace("60:35", "60:10")
            .into_bytes(),
    ] {
        assert!(check_config_reader(invalid.as_slice()).is_err());
    }
    assert!(
        check_config_snapshot(Account { uid: 0, gid: 1 }, valid)
            .unwrap_err()
            .to_string()
            .contains("account")
    );
}

#[test]
fn memfd_snapshots_are_immutable_and_executable_copy_survives_exec() {
    let mut config = sealed_snapshot(b"exact config snapshot", false).unwrap();
    assert!(config.write_all(b"changed").is_err());
    let mut snapshot = Vec::new();
    config.read_to_end(&mut snapshot).unwrap();
    assert_eq!(snapshot, b"exact config snapshot");
    let image = sealed_snapshot(&fs::read("/usr/bin/true").unwrap(), true).unwrap();
    assert_eq!(image.metadata().unwrap().mode() & 0o777, 0o555);
    let path = PathBuf::from(format!("/proc/self/fd/{}", image.as_raw_fd()));
    execute(subprocess(&path, &[]), false, Duration::from_secs(2)).unwrap();
}

#[test]
fn status_only_commands_discard_large_output_and_capture_is_bounded() {
    let shell = Path::new("/bin/sh");
    let flood = [
        "-c",
        "head -c 1048576 /dev/zero; head -c 1048576 /dev/zero >&2",
    ];
    assert!(
        execute(subprocess(shell, &flood), false, Duration::from_secs(2))
            .unwrap()
            .is_empty()
    );
    assert!(
        execute(subprocess(shell, &flood), true, Duration::from_secs(2))
            .unwrap_err()
            .to_string()
            .contains("size limit")
    );
    assert_eq!(
        execute(
            subprocess(
                shell,
                &["-c", "printf 'small state'; printf 'ignored stderr' >&2"]
            ),
            true,
            Duration::from_secs(2),
        )
        .unwrap(),
        b"small state"
    );
}

#[test]
fn command_deadline_covers_waiting_children_and_inherited_output_pipes() {
    for (script, capture) in [("sleep 5", false), ("sleep 5 &", true)] {
        let start = Instant::now();
        let error = execute(
            subprocess(Path::new("/bin/sh"), &["-c", script]),
            capture,
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}

#[test]
fn failed_commands_include_bounded_stderr_without_terminal_controls() {
    let error = execute(
        subprocess(
            Path::new("/bin/sh"),
            &[
                "-c",
                "printf '\\033[31mconfig rejected\\n' >&2; head -c 16384 /dev/zero >&2; exit 1",
            ],
        ),
        false,
        Duration::from_secs(2),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("config rejected"), "{error}");
    assert!(!error.contains('\u{1b}') && !error.contains('\0'));
    assert!(error.len() < MAX_COMMAND_OUTPUT * 5);
}
