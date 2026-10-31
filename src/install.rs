//! Fixed-path installation of this executable and its embedded service files.
//!
//! Downloads, display settings and distribution package management deliberately
//! live elsewhere. Staging never invokes host commands or reads hardware.
use crate::{
    paths::{BINARY, CONFIG, DOC_DIR as DOC, RUN_DIR, UNIT, UNIT_FILE},
    platform::system_tool,
};
use std::{
    error::Error,
    ffi::CStr,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

include!(concat!(env!("OUT_DIR"), "/license_manifest.rs"));

const NAME: &str = "minibookx-ctl";
const RECOVERY_UNIT: &str = "minibookx-ctl-recover.service";
const OLD_UNIT_FILE: &str = "/etc/systemd/system/minibookx-ctl-recover.service";
const LEGACY_FILES: [&str; 3] = [
    OLD_UNIT_FILE,
    "/usr/local/share/doc/minibookx-ctl/installed-files.json",
    "/usr/local/share/doc/minibookx-ctl/build-metadata.json",
];
const PENDING: &str = "/var/lib/minibookx-ctl/installer-transaction.json";
const LEASES: [&str; 4] = [
    "module.lease",
    "manual.owner",
    "power.lease",
    "sensor.lease",
];
const MAX_FILE: u64 = 128 * 1024 * 1024;
const MAX_CONFIG: u64 = 64 * 1024;
const MAX_COMMAND_OUTPUT: usize = 4096;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(45);

fn refuse(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

/// Internal exec mode: stdin is a sealed snapshot, never a root-side parser.
pub fn check_config_stdin() -> Result<(), Box<dyn Error>> {
    // SAFETY: geteuid takes no arguments.
    if unsafe { libc::geteuid() } == 0 {
        return Err(refuse("config validation must run as the service account, never root").into());
    }
    check_config_reader(io::stdin().lock())?;
    Ok(())
}

fn check_config_reader(reader: impl Read) -> io::Result<()> {
    let mut text = String::new();
    reader.take(MAX_CONFIG + 1).read_to_string(&mut text)?;
    let loaded = crate::presets::parse(text)?;
    loaded.config.validate_for_hardware().map_err(refuse)?;
    loaded.power.validate().map_err(refuse)
}

#[derive(Default)]
struct Options {
    destdir: Option<PathBuf>,
    enable: bool,
}

impl Options {
    fn parse(install: bool, mut args: impl Iterator<Item = String>) -> io::Result<Self> {
        let mut options = Self::default();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--destdir" if options.destdir.is_none() => {
                    options.destdir = Some(
                        args.next()
                            .ok_or_else(|| refuse("--destdir needs a path"))?
                            .into(),
                    );
                }
                "--enable" if install && !options.enable => options.enable = true,
                _ => {
                    return Err(refuse(format!(
                        "unsupported installer argument: {argument}"
                    )));
                }
            }
        }
        if options.enable && options.destdir.is_some() {
            return Err(refuse("--enable cannot be used with --destdir"));
        }
        Ok(options)
    }
}

/// Install or uninstall using only fixed destination paths. Existing config is
/// retained; uninstall also retains the locked account and recovery records.
pub fn run(install: bool, args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let options = Options::parse(install, args)?;
    let target = Target::new(options.destdir.as_deref())?;
    let _signals = target
        .live
        .then(crate::systemd::Signals::installer_process)
        .transpose()?;
    // Read the running image even if its original pathname was replaced. Its
    // executable bytes must match the embedded files from this same process.
    apply(
        install,
        &options,
        &target,
        Path::new("/proc/self/exe"),
        &mut LiveHost,
    )?;
    if install {
        println!("Installed {BINARY}; configuration: {CONFIG}");
        if !target.live {
            println!("Staged files only; no account, service or hardware was changed.");
        } else if !options.enable {
            println!(
                "Existing service state preserved. To enable: sudo systemctl enable --now {UNIT}"
            );
        }
    } else {
        println!(
            "Removed managed executable, unit and bundled notices. Configuration and locked account retained."
        );
    }
    Ok(())
}

#[derive(Clone)]
struct Snapshot {
    data: Vec<u8>,
    mode: u32,
    gid: u32,
}

struct Target {
    root: PathBuf,
    owner: u32,
    live: bool,
}

impl Target {
    fn new(destdir: Option<&Path>) -> io::Result<Self> {
        // SAFETY: geteuid has no arguments.
        let owner = unsafe { libc::geteuid() };
        if destdir.is_none() && owner != 0 {
            return Err(refuse(
                "live installation requires root; use sudo or --destdir for staging",
            ));
        }
        let root = destdir.unwrap_or_else(|| Path::new("/"));
        if destdir.is_some() && root == Path::new("/") {
            return Err(refuse(
                "--destdir must be a staging directory, never the live root",
            ));
        }
        if !root.is_absolute()
            || root
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            return Err(refuse(
                "destination must be an absolute path without . or ..",
            ));
        }
        // The staging anchor must already exist and contain no symlink component.
        // Its ancestors may include /tmp; all paths beneath it are checked below.
        if fs::canonicalize(root)? != root {
            return Err(refuse("destination anchor must not contain symlinks"));
        }
        if destdir.is_some() {
            let anchor = fs::metadata(root)?;
            let live_root = fs::metadata("/")?;
            if anchor.dev() == live_root.dev() && anchor.ino() == live_root.ino() {
                return Err(refuse("a live-root mount cannot be used as --destdir"));
            }
        }
        if owner == 0 {
            Self::privileged_ancestors(root)?;
        }
        let target = Self {
            root: root.to_owned(),
            owner,
            live: destdir.is_none(),
        };
        target.directory(&target.root)?;
        Ok(target)
    }

    fn privileged_ancestors(root: &Path) -> io::Result<()> {
        for parent in root.ancestors().skip(1) {
            let metadata = fs::symlink_metadata(parent)?;
            // A root-owned sticky temporary directory cannot be used by another
            // user to rename our root-owned staging anchor. Other writable or
            // non-root-owned ancestors can be and are refused.
            if !metadata.is_dir()
                || metadata.uid() != 0
                || metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0
            {
                return Err(refuse(
                    "privileged staging requires trusted root-owned ancestors",
                ));
            }
        }
        Ok(())
    }

    fn path(&self, fixed: &str) -> io::Result<PathBuf> {
        if !fixed.starts_with('/')
            || Path::new(fixed)
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        {
            return Err(refuse("invalid internal installation path"));
        }
        Ok(self.root.join(&fixed[1..]))
    }

    fn directory(&self, path: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.uid() != self.owner || metadata.mode() & 0o022 != 0 {
            return Err(refuse(format!(
                "untrusted installation directory: {}",
                path.display()
            )));
        }
        Ok(())
    }

    fn parents(&self, path: &Path, create: bool) -> io::Result<()> {
        self.directory(&self.root)?;
        let parent = path
            .parent()
            .ok_or_else(|| refuse("installation path has no parent"))?;
        let relative = parent
            .strip_prefix(&self.root)
            .map_err(|_| refuse("path escaped destination"))?;
        let mut current = self.root.clone();
        for part in relative.components() {
            current.push(part);
            match self.directory(&current) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                    fs::create_dir(&current)?;
                    fs::set_permissions(&current, fs::Permissions::from_mode(0o755))?;
                    self.directory(&current)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn capture(&self, fixed: &str) -> io::Result<Option<Snapshot>> {
        let path = self.path(fixed)?;
        self.parents(&path, false)?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != self.owner
            || metadata.mode() & 0o7022 != 0
            || metadata.len() > MAX_FILE
        {
            return Err(refuse(format!("untrusted installed file: {fixed}")));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        let opened = file.metadata()?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err(refuse("installed file changed while opening"));
        }
        let mut data = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_FILE + 1)
            .read_to_end(&mut data)?;
        if data.len() as u64 > MAX_FILE {
            return Err(refuse("installed file grew beyond size limit"));
        }
        Ok(Some(Snapshot {
            data,
            mode: metadata.mode() & 0o777,
            gid: metadata.gid(),
        }))
    }

    fn write(&self, fixed: &str, snapshot: &Snapshot) -> io::Result<()> {
        let path = self.path(fixed)?;
        self.parents(&path, true)?;
        self.capture(fixed)?;
        let parent = path.parent().expect("checked parent");
        // O_EXCL prevents substitution; the parent is not writable by other users.
        let mut attempt = 0;
        let (temporary, mut file) = loop {
            let candidate = parent.join(format!(
                ".minibookx-ctl-install-{}-{attempt}",
                std::process::id()
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&candidate)
            {
                Ok(file) => break (candidate, file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempt < 32 => {
                    attempt += 1
                }
                Err(error) => return Err(error),
            }
        };
        let result = (|| {
            file.write_all(&snapshot.data)?;
            if self.live && self.owner == 0 {
                // SAFETY: file owns this descriptor; IDs come from a validated account or snapshot.
                if unsafe { libc::fchown(file.as_raw_fd(), self.owner, snapshot.gid) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            file.set_permissions(fs::Permissions::from_mode(snapshot.mode))?;
            file.sync_all()?;
            fs::rename(&temporary, &path)?;
            File::open(parent)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    fn remove(&self, fixed: &str) -> io::Result<()> {
        if self.capture(fixed)?.is_some() {
            fs::remove_file(self.path(fixed)?)?;
        }
        Ok(())
    }

    fn config_directory(&self, gid: u32) -> io::Result<()> {
        let path = self.path(CONFIG)?;
        self.parents(&path, true)?;
        let parent = path.parent().expect("fixed config parent");
        self.directory(parent)?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(parent)?;
        if self.live && self.owner == 0 {
            // SAFETY: this descriptor owns the checked directory; gid comes
            // from the validated private service account.
            if unsafe { libc::fchown(directory.as_raw_fd(), self.owner, gid) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        directory.set_permissions(fs::Permissions::from_mode(0o750))?;
        directory.sync_all()
    }

    fn leases_clear(&self) -> io::Result<()> {
        for lease in LEASES {
            let fixed = format!("{RUN_DIR}/{lease}");
            if self.capture(&fixed)?.is_some() {
                return Err(refuse(format!(
                    "hardware recovery incomplete: {fixed} remains; refusing file replacement"
                )));
            }
        }
        Ok(())
    }

    fn lock(&self) -> io::Result<InstallLock> {
        let fixed = "/var/lib/minibookx-ctl/install.lock";
        let path = self.path(fixed)?;
        self.parents(&path, true)?;
        self.capture(fixed)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        // SAFETY: file owns a regular, private lock descriptor.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(refuse("another installation is running"));
        }
        Ok(InstallLock(file))
    }
}

/// Release the transaction explicitly: a concurrently forked child may still
/// hold the same open-file description until exec closes its CLOEXEC copy.
struct InstallLock(File);
impl Drop for InstallLock {
    fn drop(&mut self) {
        // SAFETY: this guard owns the live descriptor acquired by Target::lock.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Enabled {
    #[default]
    No,
    Permanent,
    Runtime,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct State {
    loaded: bool,
    active: bool,
    transitioning: bool,
    enabled: Enabled,
}

fn parse_state(text: &str) -> io::Result<State> {
    let mut properties = std::collections::BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| refuse("invalid systemd state"))?;
        if !["LoadState", "ActiveState", "UnitFileState"].contains(&key)
            || properties.insert(key, value).is_some()
        {
            return Err(refuse("ambiguous systemd state"));
        }
    }
    if properties.len() != 3 {
        return Err(refuse("incomplete systemd state"));
    }
    let loaded = match properties["LoadState"] {
        "loaded" => true,
        "not-found"
            if properties["ActiveState"] == "inactive"
                && properties["UnitFileState"].is_empty() =>
        {
            return Ok(State::default());
        }
        _ => {
            return Err(refuse(
                "service is masked, unavailable or in an unexpected load state",
            ));
        }
    };
    let active = match properties["ActiveState"] {
        "active" | "reloading" | "refreshing" | "activating" | "deactivating" => true,
        "inactive" | "failed" => false,
        _ => return Err(refuse("service is transitioning; wait and retry")),
    };
    let enabled = match properties["UnitFileState"] {
        "enabled" => Enabled::Permanent,
        "enabled-runtime" => Enabled::Runtime,
        "disabled" | "static" | "indirect" | "generated" | "transient" | "linked"
        | "linked-runtime" => Enabled::No,
        _ => return Err(refuse("unsupported systemd enable state")),
    };
    Ok(State {
        loaded,
        active,
        transitioning: matches!(properties["ActiveState"], "activating" | "deactivating"),
        enabled,
    })
}

trait Host {
    fn account(&mut self) -> io::Result<Account>;
    fn check_config(&mut self, account: Account, data: &[u8]) -> io::Result<()>;
    fn state(&mut self, unit: &str) -> io::Result<State>;
    fn control(&mut self, args: &[&str]) -> io::Result<()>;
    fn recover(&mut self, binary: &Path) -> io::Result<()>;
    fn startup_interval(&mut self) {}
}

struct LiveHost;

#[derive(Clone, Copy)]
struct Account {
    uid: u32,
    gid: u32,
}

fn subprocess(program: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LANG", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Capture a bounded, escaped diagnostic on failure, never a raw stream
        // from the dropped config parser into a privileged terminal.
        .stderr(Stdio::piped())
        .process_group(0);
    command
}

fn execute(mut command: Command, capture: bool, timeout: Duration) -> io::Result<Vec<u8>> {
    let program = command.get_program().to_string_lossy().into_owned();
    if capture {
        command.stdout(Stdio::piped());
    }
    let mut child = command.spawn()?;
    let result = (|| {
        for fd in [
            child.stdout.as_ref().map(AsRawFd::as_raw_fd),
            child.stderr.as_ref().map(AsRawFd::as_raw_fd),
        ]
        .into_iter()
        .flatten()
        {
            // SAFETY: the child owns this pipe descriptor. Nonblocking reads keep
            // both output and inherited-pipe descendants inside the deadline.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        let deadline = Instant::now() + timeout;
        let mut output = Vec::new();
        let mut stderr = Vec::new();
        let mut eof = [!capture, false];
        let mut status = None;
        loop {
            if Instant::now() >= deadline {
                return Err(refuse(format!(
                    "{program} timed out; installed files retained for recovery"
                )));
            }
            let streams: [Option<&mut dyn Read>; 2] = [
                child.stdout.as_mut().map(|v| v as &mut dyn Read),
                child.stderr.as_mut().map(|v| v as &mut dyn Read),
            ];
            for (index, stream) in streams.into_iter().enumerate() {
                let Some(stream) = stream else { continue };
                if eof[index] {
                    continue;
                }
                // Bound work even if a child continuously floods its pipes.
                for _ in 0..16 {
                    let mut buffer = [0; MAX_COMMAND_OUTPUT + 1];
                    match stream.read(&mut buffer) {
                        Ok(0) => {
                            eof[index] = true;
                            break;
                        }
                        Ok(count) => {
                            let bytes = if index == 0 { &mut output } else { &mut stderr };
                            let remaining = MAX_COMMAND_OUTPUT - bytes.len();
                            if index == 0 && count > remaining {
                                return Err(refuse(format!(
                                    "{program} output exceeded its size limit"
                                )));
                            }
                            bytes.extend_from_slice(&buffer[..count.min(remaining)]);
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error),
                    }
                }
            }
            if status.is_none() {
                status = child.try_wait()?;
            }
            if let Some(status) = status
                && eof.into_iter().all(|v| v)
            {
                if !status.success() {
                    let diagnostic = String::from_utf8_lossy(&stderr)
                        .escape_default()
                        .to_string();
                    return Err(refuse(format!(
                        "{program} failed ({status}); stderr: {diagnostic}"
                    )));
                }
                return Ok(output);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        // SAFETY: each command gets its own process group. Include descendants
        // retaining the output pipe, even after its direct child has exited.
        unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
        let _ = child.wait();
    }
    result
}

fn command(program: &Path, args: &[&str]) -> io::Result<()> {
    execute(subprocess(program, args), false, COMMAND_TIMEOUT).map(|_| ())
}

fn service_account() -> io::Result<Account> {
    // Installer startup is single-threaded. Copy each NSS field before another lookup.
    let mut record = unsafe { libc::getpwnam(c"minibookx-ctl".as_ptr()) };
    if record.is_null() {
        let shell = system_tool("nologin")?;
        command(
            &system_tool("useradd")?,
            &[
                "--system",
                "--user-group",
                "--no-create-home",
                "--home-dir",
                "/nonexistent",
                "--shell",
                shell
                    .to_str()
                    .ok_or_else(|| refuse("invalid nologin path"))?,
                "--password",
                "!",
                NAME,
            ],
        )?;
        // SAFETY: static C name; copy the record before another NSS lookup.
        record = unsafe { libc::getpwnam(c"minibookx-ctl".as_ptr()) };
    }
    if record.is_null() {
        return Err(refuse("service account is unavailable"));
    }
    // SAFETY: non-null NSS record and its strings remain valid until the next lookup.
    let (uid, gid, home, shell) = unsafe {
        (
            (*record).pw_uid,
            (*record).pw_gid,
            CStr::from_ptr((*record).pw_dir).to_bytes().to_vec(),
            CStr::from_ptr((*record).pw_shell).to_bytes().to_vec(),
        )
    };
    if uid == 0
        || uid >= 1000
        || gid == 0
        || home != b"/nonexistent"
        || ![
            b"/usr/sbin/nologin".as_slice(),
            b"/usr/bin/nologin",
            b"/sbin/nologin",
            b"/bin/nologin",
        ]
        .contains(&shell.as_slice())
    {
        return Err(refuse(
            "existing service account is not a private system/nologin account",
        ));
    }
    // SAFETY: static group name; copied/checked before the subsequent NSS call.
    let group = unsafe { libc::getgrnam(c"minibookx-ctl".as_ptr()) };
    if group.is_null() {
        return Err(refuse("private service group is missing"));
    }
    // SAFETY: gr_mem is a null-terminated pointer array in the current NSS record.
    unsafe {
        if (*group).gr_gid != gid {
            return Err(refuse("unexpected primary service group"));
        }
        let mut member = (*group).gr_mem;
        if !member.is_null() {
            let mut terminated = false;
            for _ in 0..1024 {
                if (*member).is_null() {
                    terminated = true;
                    break;
                }
                if CStr::from_ptr(*member).to_bytes() != NAME.as_bytes() {
                    return Err(refuse("private service group contains another account"));
                }
                member = member.add(1);
            }
            if !terminated {
                return Err(refuse("too many private group members"));
            }
        }
    }
    let mut groups = [0; 64];
    let mut count = groups.len() as libc::c_int;
    // SAFETY: count describes the allocated array; libc never writes past it.
    if unsafe {
        libc::getgrouplist(
            c"minibookx-ctl".as_ptr(),
            gid,
            groups.as_mut_ptr(),
            &mut count,
        )
    } < 0
        || count < 1
        || count as usize > groups.len()
        || groups[..count as usize].iter().any(|group| *group != gid)
    {
        return Err(refuse("service account must not have supplementary groups"));
    }
    command(&system_tool("usermod")?, &["--lock", NAME])?;
    Ok(Account { uid, gid })
}

fn sealed_snapshot(data: &[u8], executable: bool) -> io::Result<File> {
    let flags = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
    // SAFETY: static name and Linux memfd flags. MFD_EXEC is required when
    // the kernel defaults anonymous files to non-executable; pre-6.3 kernels
    // reject that flag and retain their original executable-memfd behavior.
    let mut fd = unsafe {
        libc::memfd_create(
            c"minibookx-installer-snapshot".as_ptr(),
            flags | if executable { libc::MFD_EXEC } else { 0 },
        )
    };
    if fd < 0 && executable && io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
        fd = unsafe { libc::memfd_create(c"minibookx-installer-snapshot".as_ptr(), flags) };
    }
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: memfd_create returned a new owned descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(data)?;
    file.rewind()?;
    file.set_permissions(fs::Permissions::from_mode(if executable {
        0o555
    } else {
        0o400
    }))?;
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    // SAFETY: file owns this descriptor, and no writable mapping exists.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

/// Only async-signal-safe syscalls are used in Command::pre_exec.
fn config_child_privileges(account: Account) -> io::Result<()> {
    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    let header = CapHeader {
        version: 0x20080522,
        pid: 0,
    };
    // Two Linux capability ABI v3 entries: effective/permitted/inheritable.
    let empty_capabilities = [0_u32; 6];
    // SAFETY: this is the single exec child's setup. Mark all inherited extra
    // descriptors close-on-exec, including the image memfd and Command's error
    // pipe; the parser runs only after exec has closed them. The sealed config
    // snapshot is already installed as stdin by Command.
    if unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3_u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    } < 0
        || unsafe { libc::setgroups(0, std::ptr::null()) } != 0
        || unsafe { libc::setresgid(account.gid, account.gid, account.gid) } != 0
        || unsafe { libc::setresuid(account.uid, account.uid, account.uid) } != 0
        || unsafe { libc::syscall(libc::SYS_capset, &header, empty_capabilities.as_ptr()) } != 0
        || unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn check_config_snapshot(account: Account, data: &[u8]) -> io::Result<()> {
    if account.uid == 0 || account.gid == 0 || data.len() as u64 > MAX_CONFIG {
        return Err(refuse("invalid config validation account or snapshot size"));
    }
    let mut image = Vec::new();
    File::open("/proc/self/exe")?
        .take(MAX_FILE + 1)
        .read_to_end(&mut image)?;
    if image.len() as u64 > MAX_FILE {
        return Err(refuse("installer image exceeds its size limit"));
    }
    // The invoking user's download may be mode 0700. A sealed executable copy
    // lets the service account validate with this version without changing that
    // download's permissions or reopening a replaceable temporary pathname.
    let image = sealed_snapshot(&image, true)?;
    let input = sealed_snapshot(data, false)?;
    let path = PathBuf::from(format!("/proc/self/fd/{}", image.as_raw_fd()));
    let mut check = subprocess(&path, &["--check-config-stdin"]);
    check.stdin(Stdio::from(input));
    // SAFETY: the callback uses only the syscall-only setup above. No config is
    // parsed, allocation performed or lock acquired between fork and exec.
    unsafe { check.pre_exec(move || config_child_privileges(account)) };
    execute(check, false, COMMAND_TIMEOUT).map(|_| ()).map_err(|error| {
        refuse(format!(
            "existing config could not be validated by the new version as the service account: {error}; existing service left untouched; correct {CONFIG} and retry this installer"
        ))
    })
}

impl Host for LiveHost {
    fn startup_interval(&mut self) {
        std::thread::sleep(Duration::from_millis(250));
    }
    fn account(&mut self) -> io::Result<Account> {
        service_account()
    }
    fn check_config(&mut self, account: Account, data: &[u8]) -> io::Result<()> {
        check_config_snapshot(account, data)
    }
    fn state(&mut self, unit: &str) -> io::Result<State> {
        let output = execute(
            subprocess(
                &system_tool("systemctl")?,
                &[
                    "--no-pager",
                    "--no-ask-password",
                    "show",
                    "--all",
                    "--property=LoadState,ActiveState,UnitFileState",
                    unit,
                ],
            ),
            true,
            COMMAND_TIMEOUT,
        )?;
        parse_state(
            std::str::from_utf8(&output).map_err(|_| refuse("invalid systemd state encoding"))?,
        )
    }
    fn control(&mut self, args: &[&str]) -> io::Result<()> {
        let mut complete = vec!["--no-pager", "--no-ask-password"];
        complete.extend_from_slice(args);
        command(&system_tool("systemctl")?, &complete).map(|_| ())
    }
    fn recover(&mut self, binary: &Path) -> io::Result<()> {
        command(binary, &["--recover"]).map(|_| ())
    }
}

fn bundled(binary: Vec<u8>, gid: u32) -> Vec<(String, Snapshot)> {
    let mut files = vec![
        (
            BINARY.to_owned(),
            Snapshot {
                data: binary,
                mode: 0o755,
                gid: 0,
            },
        ),
        (
            UNIT_FILE.to_owned(),
            Snapshot {
                data: include_bytes!("../packaging/minibookx-ctl.service").to_vec(),
                mode: 0o644,
                gid: 0,
            },
        ),
        (
            CONFIG.to_owned(),
            Snapshot {
                data: include_bytes!("../config/presets.conf").to_vec(),
                mode: 0o640,
                gid,
            },
        ),
    ];
    for (name, data) in [
        ("LICENSE", include_bytes!("../LICENSE").as_slice()),
        ("NOTICE", include_bytes!("../NOTICE").as_slice()),
        (
            "THIRD_PARTY.md",
            include_bytes!("../THIRD_PARTY.md").as_slice(),
        ),
        (
            "presets.conf",
            include_bytes!("../config/presets.conf").as_slice(),
        ),
    ] {
        files.push((
            format!("{DOC}/{name}"),
            Snapshot {
                data: data.to_vec(),
                mode: 0o644,
                gid: 0,
            },
        ));
    }
    for (name, data) in BUNDLED_LICENSES {
        files.push((
            format!("{DOC}/licenses/{name}"),
            Snapshot {
                data: data.to_vec(),
                mode: 0o644,
                gid: 0,
            },
        ));
    }
    files
}

fn stopped(target: &Target, host: &mut impl Host) -> io::Result<()> {
    for unit in [UNIT, RECOVERY_UNIT] {
        if host.state(unit)?.loaded {
            host.control(&["stop", unit])?;
            if host.state(unit)?.active {
                return Err(refuse("service did not stop; files retained"));
            }
        }
    }
    if let Some(binary) = target.capture(BINARY)? {
        if binary.mode & 0o111 == 0 || !binary.data.starts_with(b"\x7fELF") {
            return Err(refuse(
                "installed recovery binary is not an executable ELF image",
            ));
        }
        host.recover(&target.path(BINARY)?)?;
    }
    target.leases_clear()
}

fn restore_enablement(host: &mut impl Host, state: State) -> io::Result<()> {
    match state.enabled {
        Enabled::Permanent => host.control(&["enable", UNIT])?,
        Enabled::Runtime => {
            host.control(&["disable", UNIT])?;
            host.control(&["enable", "--runtime", UNIT])?;
        }
        Enabled::No => host.control(&["disable", UNIT])?,
    }
    Ok(())
}

fn start_service(host: &mut impl Host) -> io::Result<()> {
    // The manager may have collected an inactive unit after stop/reload.
    // Resetting a rate limit is useful, but only a successful start and active
    // state are required. A missing reset target must not prevent rollback.
    if let Err(error) = host.control(&["reset-failed", UNIT]) {
        let _ = writeln!(
            io::stderr(),
            "could not reset failed state; attempting service start: {error}"
        );
    }
    host.control(&["start", UNIT])?;
    for _ in 0..4 {
        host.startup_interval();
        let current = host.state(UNIT)?;
        if !current.active || current.transitioning {
            return Err(refuse(
                "service did not stay active during the startup check",
            ));
        }
    }
    Ok(())
}

fn apply(
    install: bool,
    options: &Options,
    target: &Target,
    executable: &Path,
    host: &mut impl Host,
) -> io::Result<()> {
    // Defer termination during the transaction. Check pending signals between
    // steps; rollback stays protected until files and service state are settled.
    // Staging has no live service or hardware to protect.
    let signals = target
        .live
        .then(crate::systemd::Signals::installer)
        .transpose()?;
    let interrupted = || {
        if let Some(signals) = &signals
            && !signals.drain()?.is_empty()
        {
            return Err(refuse("installation interrupted"));
        }
        Ok(())
    };
    let _lock = target.lock()?;
    if target.capture(PENDING)?.is_some() {
        return Err(refuse(
            "a legacy installer transaction is pending; follow docs/guide/troubleshooting.md#legacy-installer-transaction before continuing; do not remove its journal blindly",
        ));
    }
    // Executed image is already trusted by the caller. Open it once with a size
    // bound; never execute content from the destination/config as installer code.
    let binary = if install {
        let mut bytes = Vec::new();
        File::open(executable)?
            .take(MAX_FILE + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE || !bytes.starts_with(b"\x7fELF") {
            return Err(refuse("installer executable must be a bounded ELF image"));
        }
        bytes
    } else {
        Vec::new()
    };
    // Validate all existing destinations before making account/service changes.
    let mut files = bundled(binary, 0);
    let mut previous = Vec::new();
    for path in files
        .iter()
        .map(|(path, _)| path.as_str())
        .chain(LEGACY_FILES)
    {
        previous.push((path.to_owned(), target.capture(path)?));
    }
    let config = previous
        .iter()
        .find(|(path, _)| path == CONFIG)
        .and_then(|(_, value)| value.as_ref());
    if config.is_some_and(|value| value.data.len() as u64 > MAX_CONFIG) {
        return Err(refuse(
            "existing config is too large; retained without parsing",
        ));
    }
    let state = if target.live {
        host.state(UNIT)?
    } else {
        State::default()
    };
    if state.transitioning {
        return Err(refuse("service is transitioning; wait and retry"));
    }
    if target.live {
        let legacy = host.state("minibook-fand.service")?;
        if legacy.active || legacy.enabled != Enabled::No {
            return Err(refuse(
                "minibook-fand is active/enabled; stop and disable the old controller manually first",
            ));
        }
        if install {
            let account = host.account()?;
            if let Some(config) = config {
                host.check_config(account, &config.data)?;
            }
            files
                .iter_mut()
                .find(|(path, _)| path == CONFIG)
                .expect("bundled config")
                .1
                .gid = account.gid;
        }
        interrupted()?;
        stopped(target, host)?;
    }
    if let Some(config) = config {
        let entry = files
            .iter_mut()
            .find(|(path, _)| path == CONFIG)
            .expect("bundled config");
        entry.1.data.clone_from(&config.data);
    }
    let result: io::Result<()> = (|| {
        interrupted()?;
        if install {
            let gid = files
                .iter()
                .find(|(path, _)| path == CONFIG)
                .expect("bundled config")
                .1
                .gid;
            // Tightening is intentionally after stop/recovery. A caught-error
            // rollback may retain these safe directory permissions.
            target.config_directory(gid)?;
            for (path, snapshot) in &files {
                interrupted()?;
                target.write(path, snapshot)?;
            }
        } else {
            if target.live && state.loaded {
                host.control(&["disable", UNIT])?;
            }
            for (path, _) in &files {
                interrupted()?;
                if path != CONFIG {
                    target.remove(path)?;
                }
            }
        }
        for path in LEGACY_FILES {
            target.remove(path)?;
        }
        if target.live {
            host.control(&["daemon-reload"])?;
            if install {
                if options.enable {
                    host.control(&["enable", UNIT])?;
                }
                if options.enable || state.active {
                    start_service(host)?;
                }
            }
        }
        interrupted()?;
        Ok(())
    })();
    if let Err(original) = result {
        // New startup can acquire hardware ownership before reporting failure.
        // Never restore the old executable while that recovery remains pending.
        if target.live
            && install
            && let Err(recovery) = stopped(target, host)
        {
            let enablement = restore_enablement(host, state);
            return Err(refuse(format!(
                "installation failed: {original}; recovery also failed: {recovery}; enablement rollback={enablement:?}; current files retained, service not restarted"
            )));
        }
        let rollback = (|| {
            let mut errors = Vec::new();
            if target.live
                && !state.loaded
                && options.enable
                && let Err(error) = host.control(&["disable", UNIT])
            {
                errors.push(error.to_string());
            }
            for (path, snapshot) in &previous {
                let result = match snapshot {
                    Some(value) => target.write(path, value),
                    None => target.remove(path),
                };
                if let Err(error) = result {
                    errors.push(format!("{path}: {error}"));
                }
            }
            if target.live {
                if let Err(error) = host.control(&["daemon-reload"]) {
                    errors.push(error.to_string());
                }
                if let Err(error) = restore_enablement(host, state) {
                    errors.push(format!("enablement: {error}"));
                }
                if state.active && errors.is_empty() {
                    start_service(host)?;
                }
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(refuse(errors.join("; ")))
            }
        })();
        return match rollback {
            Ok(()) => Err(refuse(format!(
                "installation failed; previous files restored: {original}"
            ))),
            Err(error) => Err(refuse(format!(
                "installation failed: {original}; rollback incomplete: {error}; inspect installed files before starting the service"
            ))),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests;
