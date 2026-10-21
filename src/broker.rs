//! Root hardware broker and independent tablet-switch guardian.
//!
//! Configuration is opened only after the policy process has dropped uid/gid
//! and closed every hardware descriptor. The private socket accepts only fixed,
//! bounded operations; it cannot name a device, register, path or command.
use crate::{
    ec, hardware,
    ipc::{REQUEST_BYTES, Reply, Request, TachState},
    power::Manager,
    sensor::CpuSensor,
    service,
    tablet::{EstimatorConfig, HingeEstimator, SAMPLE_INTERVAL, Sensors, VirtualSwitch},
};
use std::{
    ffi::CString,
    fs,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
        unix::{
            fs::{MetadataExt, PermissionsExt},
            net::{UnixDatagram, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
static STOP: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);

/// Independent of configurable policy. Keep a short pulse observable by the
/// worker and require continuous cooling before releasing this safety override.
#[derive(Default)]
struct EmergencyGuard {
    active: bool,
    cool_since: Option<Duration>,
}

const FAN_HEALTH_INTERVAL: Duration = Duration::from_secs(5);
const TACH_GRACE: Duration = Duration::from_secs(10);

/// The broker is the sole owner of tach plausibility and spin-up deadlines.
/// Replies carry this cached result; policy cannot refresh or bypass the guard.
#[derive(Default)]
struct FanHealth {
    spin_started: Option<Duration>,
    last_good: Option<Duration>,
    last_check: Option<Duration>,
    temperature: Option<u8>,
    tach: Option<u16>,
    state: TachState,
}
impl FanHealth {
    fn duty(&mut self, now: Duration, raw: u8) {
        if raw == 0 {
            self.spin_started = None;
            self.last_good = None;
            self.state = TachState::Stopped;
        } else if self.spin_started.is_none() {
            self.spin_started = Some(now);
            self.last_good = None;
            self.state = TachState::Starting;
        }
    }
    fn record(&mut self, now: Duration, temperature: u8, tach: Option<u16>) -> io::Result<()> {
        self.last_check = Some(now);
        self.temperature = Some(temperature);
        self.tach = tach;
        let Some(started) = self.spin_started else {
            if self.state != TachState::Unknown {
                self.state = TachState::Stopped;
            }
            return Ok(());
        };
        if tach.is_some_and(|v| (500..=8000).contains(&v)) {
            self.last_good = Some(now);
        }
        if now.saturating_sub(started) > TACH_GRACE
            && self
                .last_good
                .is_none_or(|last| now.saturating_sub(last) > TACH_GRACE)
        {
            self.state = TachState::Failed;
            return Err(io::Error::other(
                "broker fan tach guard failed; returning EC auto",
            ));
        }
        self.state = if self.last_good.is_some() {
            TachState::Recent
        } else {
            TachState::Starting
        };
        Ok(())
    }
    fn refresh<T: ec::EcIo>(&mut self, fan: &ec::Fan<T>, now: Duration) -> io::Result<()> {
        if self
            .last_check
            .is_none_or(|last| now.saturating_sub(last) >= FAN_HEALTH_INTERVAL)
        {
            fan.verify()?;
            let (temperature, tach) = fan.feedback()?;
            self.record(now, temperature, tach)?;
        }
        Ok(())
    }
}
impl EmergencyGuard {
    fn observe(&mut self, now: Duration, temperature: i32) -> bool {
        if temperature >= 95_000 {
            self.active = true;
            self.cool_since = None;
        } else if self.active {
            if temperature < 90_000 {
                let since = *self.cool_since.get_or_insert(now);
                if now.saturating_sub(since) >= Duration::from_secs(30) {
                    self.active = false;
                    self.cool_since = None;
                }
            } else {
                self.cool_since = None;
            }
        }
        self.active
    }
}
extern "C" fn stop_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed)
}
extern "C" fn reload_signal(_: libc::c_int) {
    RELOAD.store(true, Ordering::Relaxed)
}
fn install_signals() {
    // SAFETY: these handlers only store to lock-free atomic booleans.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            stop_signal as *const () as libc::sighandler_t,
        );
        libc::signal(libc::SIGINT, stop_signal as *const () as libc::sighandler_t);
        libc::signal(
            libc::SIGHUP,
            reload_signal as *const () as libc::sighandler_t,
        );
    }
}
fn no_new_privileges() -> io::Result<()> {
    // SAFETY: PR_SET_NO_NEW_PRIVS has one integer argument and no pointers.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn capabilities(kill: bool) -> io::Result<()> {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let mut h = Header {
        version: 0x20080522,
        pid: 0,
    };
    let bit = if kill { 1 << 5 } else { 0 };
    let d = [
        Data {
            effective: bit,
            permitted: bit,
            inheritable: 0,
        },
        Data {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
    ];
    // SAFETY: capset consumes fixed-size Linux capability ABI structures.
    if unsafe { libc::syscall(libc::SYS_capset, &mut h, d.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    no_new_privileges()
}
fn user() -> io::Result<(libc::uid_t, libc::gid_t)> {
    // SAFETY: single-threaded startup, static C name, returned record copied now.
    let p = unsafe { libc::getpwnam(c"minibookx-ctl".as_ptr()) };
    if p.is_null() {
        return Err(io::Error::other(
            "dedicated minibookx-ctl account is missing; run installer",
        ));
    }
    let (uid, gid) = unsafe { ((*p).pw_uid, (*p).pw_gid) };
    if uid == 0 || gid == 0 {
        return Err(io::Error::other("service account must be unprivileged"));
    }
    Ok((uid, gid))
}
fn policy_directory(uid: libc::uid_t, gid: libc::gid_t) -> io::Result<()> {
    let root = Path::new(service::RUN_DIR);
    let m = fs::symlink_metadata(root)?;
    if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
        return Err(io::Error::other("untrusted root runtime directory"));
    }
    let p = root.join("policy");
    match fs::create_dir(&p) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e),
    }
    let m = fs::symlink_metadata(&p)?;
    if !m.is_dir() || (m.uid() != 0 && m.uid() != uid) {
        return Err(io::Error::other("untrusted policy directory"));
    }
    let c = CString::new(p.as_os_str().as_encoded_bytes())?;
    // SAFETY: parent directory is root-owned and not writable by the worker.
    if unsafe { libc::chown(c.as_ptr(), uid, gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    fs::set_permissions(p, fs::Permissions::from_mode(0o755))
}
fn isolate_fd(fd: RawFd) -> io::Result<RawFd> {
    // SAFETY: fd is owned by this post-fork process. Retain it as fd3, then
    // close all inherited raw EC/I2C/guardian/lease descriptors before setuid.
    if fd != 3 && unsafe { libc::dup2(fd, 3) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::syscall(libc::SYS_close_range, 4_u32, u32::MAX, 0_u32) } < 0 {
        let fds: Vec<_> = fs::read_dir("/proc/self/fd")?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_string_lossy().parse::<i32>().ok())
            .filter(|n| *n > 3)
            .collect();
        for n in fds {
            unsafe {
                libc::close(n);
            }
        }
    }
    Ok(3)
}
fn drop_user(uid: libc::uid_t, gid: libc::gid_t) -> io::Result<()> {
    let before = fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("Uid:") || l.starts_with("Gid:") || l.starts_with("Cap"))
        .collect::<Vec<_>>()
        .join("; ");
    // SAFETY: before any threads/D-Bus are created; discard supplementary groups.
    if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
        return Err(io::Error::other(format!(
            "clear supplementary groups: {}",
            io::Error::last_os_error()
        )));
    }
    if unsafe { libc::setresgid(gid, gid, gid) } != 0 {
        return Err(io::Error::other(format!(
            "drop gid: {}",
            io::Error::last_os_error()
        )));
    }
    if unsafe { libc::setresuid(uid, uid, uid) } != 0 {
        let error = io::Error::last_os_error();
        let after = fs::read_to_string("/proc/self/status")
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("Uid:") || l.starts_with("Gid:") || l.starts_with("Cap"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(io::Error::other(format!(
            "drop uid: {error}; before={before}; after-gid={after}"
        )));
    }
    capabilities(false).map_err(|e| io::Error::other(format!("drop capabilities: {e}")))?;
    // SAFETY: prevent same-uid ptrace/core access to inherited private IPC.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
struct Child {
    pid: libc::pid_t,
    status: Option<i32>,
}
impl Child {
    fn exited(&mut self) -> io::Result<Option<i32>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let mut status = 0;
        // SAFETY: wait only for our exact child.
        let r = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        if r == 0 {
            Ok(None)
        } else {
            self.status = Some(status);
            Ok(Some(status))
        }
    }
    fn stop(&mut self) -> io::Result<i32> {
        if let Some(status) = self.exited()? {
            return Ok(status);
        }
        // SAFETY: this is our unreaped child, so its PID cannot be reused.
        unsafe {
            libc::kill(self.pid, libc::SIGTERM);
        }
        for _ in 0..40 {
            if let Some(status) = self.exited()? {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        // SAFETY: reap only this child after the bounded graceful wait.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
        let mut status = 0;
        loop {
            if unsafe { libc::waitpid(self.pid, &mut status, 0) } >= 0 {
                self.status = Some(status);
                return Ok(status);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
#[derive(Debug, PartialEq, Eq)]
enum OffConfirmation {
    Acknowledged,
    GuardianExited,
}
impl OffConfirmation {
    fn reason(&self) -> &'static str {
        match self {
            Self::Acknowledged => "StoppedOffAcknowledged",
            Self::GuardianExited => "StoppedGuardianExited",
        }
    }
}
struct GuardianShutdown {
    confirmation: Option<OffConfirmation>,
    error: Option<io::Error>,
}
struct Guardian {
    socket: UnixStream,
    child: Child,
}
impl Guardian {
    fn spawn() -> io::Result<Self> {
        let (mut parent, child_socket) = UnixStream::pair()?;
        parent.set_read_timeout(Some(Duration::from_secs(3)))?;
        parent.set_write_timeout(Some(Duration::from_millis(200)))?;
        // SAFETY: startup is single-threaded; child closes all unrelated FDs.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            let result = (|| {
                let fd = isolate_fd(child_socket.into_raw_fd())?;
                let socket = unsafe { UnixStream::from_raw_fd(fd) };
                guardian_loop(socket)
            })();
            if let Err(e) = &result {
                eprintln!("tablet guardian: {e}")
            }
            unsafe {
                libc::_exit(i32::from(result.is_err()));
            }
        }
        drop(child_socket);
        let child = Child { pid, status: None };
        let mut ready = [0];
        parent.read_exact(&mut ready)?;
        if ready != *b"R" {
            return Err(io::Error::other("tablet guardian did not become ready"));
        }
        Ok(Self {
            socket: parent,
            child,
        })
    }
    fn send(&mut self, b: u8) -> io::Result<()> {
        self.socket.write_all(&[b])
    }
    fn off(&mut self) -> io::Result<()> {
        self.send(b'0')?;
        // An acknowledgement means the same still-live uinput FD received OFF.
        self.socket.set_read_timeout(Some(Duration::from_secs(1)))?;
        let mut ack = [0];
        self.socket.read_exact(&mut ack)?;
        if ack != *b"0" {
            return Err(io::Error::other("tablet OFF acknowledgement missing"));
        }
        Ok(())
    }
    fn stop(&mut self) -> GuardianShutdown {
        let acknowledgement = self.off();
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        let exited = self.child.stop();
        // systemd sends TERM to the whole cgroup. The guardian may therefore
        // finish OFF/settle and close before our separate OFF request is read.
        // Only its preserved successful exit status proves that path completed.
        let clean_exit = exited
            .as_ref()
            .is_ok_and(|status| libc::WIFEXITED(*status) && libc::WEXITSTATUS(*status) == 0);
        let confirmation = if acknowledgement.is_ok() {
            Some(OffConfirmation::Acknowledged)
        } else if clean_exit {
            Some(OffConfirmation::GuardianExited)
        } else {
            None
        };
        let error = (!clean_exit).then(|| {
            io::Error::other(format!(
                "tablet guardian shutdown failed; OFF acknowledgement: {acknowledgement:?}; guardian exit: {exited:?}"
            ))
        });
        GuardianShutdown {
            confirmation,
            error,
        }
    }
}
impl Drop for Guardian {
    fn drop(&mut self) {
        if self.child.status.is_none() {
            let _ = self.stop();
        }
    }
}
fn guardian_loop(mut socket: UnixStream) -> io::Result<()> {
    STOP.store(false, Ordering::Relaxed);
    install_signals();
    // Remain OFF during setup. EOF still detects a dead broker; arm the
    // heartbeat timeout only after the first explicit 0/1 command.
    socket.set_read_timeout(None)?;
    socket.set_write_timeout(Some(Duration::from_millis(200)))?;
    let mut switch = VirtualSwitch::create()?;
    capabilities(false)?;
    socket.write_all(b"R")?;
    let result = (|| {
        let mut b = [0; 1];
        while !STOP.load(Ordering::Relaxed) {
            match socket.read(&mut b) {
                Ok(0) => break,
                Ok(_) => match b[0] {
                    b'1' => {
                        socket.set_read_timeout(Some(Duration::from_secs(2)))?;
                        switch.send(true)?;
                    }
                    b'0' => {
                        socket.set_read_timeout(Some(Duration::from_secs(2)))?;
                        switch.send(false)?;
                        socket.write_all(b"0")?;
                    }
                    b'P' => {
                        switch.send(false)?;
                        socket.set_read_timeout(None)?;
                    }
                    b'.' => (),
                    _ => return Err(io::Error::other("invalid guardian message")),
                },
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    })();
    switch.send(false)?;
    // Keep the original device alive while libinput consumes the OFF event.
    std::thread::sleep(Duration::from_millis(250));
    result
}

fn spawn_policy(
    socket: UnixDatagram,
    path: PathBuf,
    uid: libc::uid_t,
    gid: libc::gid_t,
) -> io::Result<Child> {
    // SAFETY: no threads exist before this fork; libdbus starts only in child.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        let result = (|| {
            let fd = isolate_fd(socket.into_raw_fd())?;
            let socket = unsafe { UnixDatagram::from_raw_fd(fd) };
            drop_user(uid, gid)
                .map_err(|e| io::Error::other(format!("worker privilege boundary: {e}")))?;
            // SAFETY: this child remains single-threaded; no environment readers.
            unsafe {
                std::env::remove_var("WATCHDOG_PID");
            }
            service::install_transport(socket);
            let loaded = crate::presets::load(&path)
                .map_err(|e| io::Error::other(format!("worker config load: {e}")))?;
            crate::runtime::run(loaded, Some(path))
                .map_err(|e| io::Error::other(format!("worker event runtime: {e}")))
        })();
        if let Err(e) = &result {
            eprintln!("unprivileged policy: {e}")
        }
        unsafe {
            libc::_exit(i32::from(result.is_err()));
        }
    }
    Ok(Child { pid, status: None })
}
fn lid_closed() -> Option<bool> {
    let s = fs::read_to_string("/proc/acpi/button/lid/LID0/state").ok()?;
    if s.split_whitespace().last() == Some("closed") {
        Some(true)
    } else if s.split_whitespace().last() == Some("open") {
        Some(false)
    } else {
        None
    }
}
fn tablet_status_text(
    tablet: Option<bool>,
    angle: Option<f64>,
    reliable: bool,
    reason: &str,
) -> String {
    let tablet = tablet.map_or_else(|| "null".into(), |value| value.to_string());
    let angle = angle.map_or_else(|| "null".into(), |v| format!("{v:.2}"));
    // All fields originate from bounded hardware values or static enums.
    format!(
        "{{\"tablet\":{tablet},\"angle_deg\":{angle},\"reliable\":{reliable},\"reason\":\"{reason}\"}}\n"
    )
}
fn tablet_status(
    tablet: Option<bool>,
    angle: Option<f64>,
    reliable: bool,
    reason: &str,
) -> io::Result<()> {
    write_snapshot(
        "tablet",
        &tablet_status_text(tablet, angle, reliable, reason),
    )
}

fn write_snapshot(name: &str, text: &str) -> io::Result<()> {
    if text.len() > 4096 {
        return Err(io::Error::other("broker snapshot exceeds its size bound"));
    }
    // Names are fixed at the three call sites, never accepted through IPC.
    let root = Path::new(service::RUN_DIR);
    let temp = root.join(format!("{name}.tmp"));
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    f.write_all(text.as_bytes())?;
    f.set_permissions(fs::Permissions::from_mode(0o644))?;
    fs::rename(temp, root.join(format!("{name}.json")))
}
fn fan_status(
    fan: Option<&ec::Fan<ec::FileEc>>,
    emergency: bool,
    health: &FanHealth,
    now: Duration,
) -> io::Result<()> {
    write_snapshot(
        "fan",
        &serde_json::json!({
            "enabled": fan.is_some(),
            "manual": fan.is_some_and(|fan| fan.may_be_manual()),
            "actual_raw": fan.and_then(|fan| fan.current_raw()),
            "emergency_override": emergency,
            "tach_guard_active": fan.is_some_and(|fan| fan.may_be_manual()),
            "tach_state": health.state.as_str(),
            "ec_c": health.temperature,
            "fan_speed_ec": health.tach,
            "feedback_age_ms": health.last_check.map(|last| now.saturating_sub(last).as_millis()),
        })
        .to_string(),
    )
}
fn startup_mode(request: Request) -> io::Result<bool> {
    match request {
        Request::Initialize { fan_control } => Ok(fan_control),
        _ => Err(io::Error::other(
            "policy must initialize its startup mode first",
        )),
    }
}
fn await_startup(socket: &UnixDatagram, child: &mut Child) -> io::Result<(bool, u32)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if STOP.load(Ordering::Relaxed) || child.exited()?.is_some() {
            return Err(io::Error::other("policy stopped before startup handshake"));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("policy startup handshake timed out"));
        }
        let mut poll = libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll receives one live socket descriptor and a bounded timeout.
        let result = unsafe { libc::poll(&mut poll, 1, SAMPLE_INTERVAL.as_millis() as i32) };
        if result < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(io::Error::last_os_error());
        }
        if poll.revents & libc::POLLIN != 0 {
            let mut bytes = [0; REQUEST_BYTES + 1];
            let n = socket.recv(&mut bytes)?;
            let (request, sequence) = Request::decode(&bytes[..n])?;
            return Ok((startup_mode(request)?, sequence));
        }
    }
}
fn power_status(power: &Manager) {
    if let Err(error) = write_snapshot("power", &power.diagnostic_status().to_string()) {
        // Diagnostic I/O cannot prevent independent fan/power restoration.
        eprintln!("cannot publish broker power status: {error}");
    }
}
pub fn run(path: PathBuf, tablet_enabled: bool) -> io::Result<()> {
    hardware::check_profile()?;
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::other("--daemon requires the root system unit"));
    }
    let (uid, gid) = user()?;
    policy_directory(uid, gid)?;
    let mut guardian = if tablet_enabled {
        Some(Guardian::spawn()?)
    } else {
        None
    };
    let mut estimator = HingeEstimator::new(EstimatorConfig::default())?;
    let (parent, worker) = UnixDatagram::pair()?;
    parent.set_nonblocking(true)?;
    let mut child = spawn_policy(worker, path, uid, gid)?;
    STOP.store(false, Ordering::Relaxed);
    RELOAD.store(false, Ordering::Relaxed);
    install_signals();
    let (fan_control, sequence) = await_startup(&parent, &mut child)?;
    // Fork both children before opening any hardware/ownership descriptor.
    // A shared guard serializes sensor wake and all later EC/RAPL operations.
    let control = hardware::Control::open()?;
    let mut sensors = if tablet_enabled {
        Some(Sensors::discover(&control)?)
    } else {
        None
    };
    // Configuration has only been parsed in the fully unprivileged worker.
    // This single bounded choice is fixed for the lifetime of this broker.
    let mut session = hardware::Session::open_locked(fan_control, control.clone())?;
    let cpu = fan_control
        .then(|| CpuSensor::discover(Path::new("/sys/class/hwmon")))
        .transpose()?;
    let mut power = Manager::new();
    power_status(&power);
    capabilities(true)?;
    parent.send(
        &Reply {
            sequence,
            ..Reply::default()
        }
        .encode(),
    )?;
    eprintln!(
        "broker ready: root hardware pid={} unprivileged policy pid={} uid={} tablet_guardian={}",
        std::process::id(),
        child.pid,
        uid,
        guardian.is_some()
    );
    let epoch = Instant::now();
    let mut last_request = Instant::now();
    let mut last_cpu = Instant::now();
    let mut health = FanHealth::default();
    let mut last_tablet_sample: Option<Instant> = None;
    let mut last_tablet: Option<(bool, bool, String)> = None;
    let mut tablet_log = Instant::now();
    let mut paused = false;
    let mut emergency = EmergencyGuard::default();
    let mut rate_epoch = Instant::now();
    let mut requests = 0_u32;
    let mut fan_report = None;
    let result = (|| {
        while !STOP.load(Ordering::Relaxed) {
            if RELOAD.swap(false, Ordering::Relaxed) {
                unsafe {
                    libc::kill(child.pid, libc::SIGHUP);
                }
            }
            if let Some(status) = child.exited()? {
                if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
                    break;
                }
                return Err(io::Error::other("policy process exited unexpectedly"));
            }
            if let Some(g) = &mut guardian
                && g.child.exited()?.is_some()
            {
                return Err(io::Error::other("tablet guardian exited"));
            }
            if !paused && last_request.elapsed() > Duration::from_secs(12) {
                return Err(io::Error::other("policy heartbeat expired"));
            }
            let mut poll = libc::pollfd {
                fd: parent.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let n = unsafe { libc::poll(&mut poll, 1, SAMPLE_INTERVAL.as_millis() as i32) };
            if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(io::Error::last_os_error());
            }
            if poll.revents & libc::POLLIN != 0 {
                let mut bytes = [0; REQUEST_BYTES + 1];
                let n = parent.recv(&mut bytes)?;
                let (request, sequence) = Request::decode(&bytes[..n])?;
                if rate_epoch.elapsed() >= Duration::from_secs(1) {
                    rate_epoch = Instant::now();
                    requests = 0;
                }
                requests += 1;
                if requests > 32 {
                    return Err(io::Error::other("hardware request rate exceeded"));
                }
                let power_request = matches!(request, Request::Power(_));
                let report_power = power_request || matches!(request, Request::Restore);
                last_request = Instant::now();
                let mut reply = Reply {
                    sequence,
                    ..Reply::default()
                };
                let operation: io::Result<()> = match request {
                    Request::Initialize { .. } => Err(io::Error::other(
                        "startup mode is already fixed; restart required",
                    )),
                    Request::Heartbeat => Ok(()),
                    Request::Activate => {
                        if paused {
                            estimator.reset();
                            sensors = if tablet_enabled {
                                Some(Sensors::discover(&control)?)
                            } else {
                                None
                            };
                            if let Some(g) = &mut guardian {
                                g.off()?;
                            }
                        }
                        paused = false;
                        session.activate()
                    }
                    Request::Duty(raw) => {
                        let fan = session
                            .fan
                            .as_mut()
                            .ok_or_else(|| io::Error::other("fan control is disabled"))?;
                        let temperature = cpu
                            .as_ref()
                            .ok_or_else(|| io::Error::other("fan CPU sensor is unavailable"))?
                            .read_mc()?;
                        let target = if emergency.observe(epoch.elapsed(), temperature) {
                            ec::MAX_RAW
                        } else {
                            raw
                        };
                        reply.changed = fan.apply(target)?;
                        health.duty(epoch.elapsed(), target);
                        health.refresh(fan, epoch.elapsed())?;
                        Ok(())
                    }
                    Request::Restore => {
                        paused = true;
                        emergency = EmergencyGuard::default();
                        health = FanHealth::default();
                        estimator.reset();
                        let off = guardian
                            .as_mut()
                            .map_or(Ok(()), |g| g.off().and_then(|()| g.send(b'P')));
                        // A failed domain must never prevent independent
                        // thermal or power recovery. Keep sensor attribution
                        // if OFF could not be confirmed by the guardian.
                        let fan = session.restore();
                        let rapl = power.restore();
                        let sensor = if off.is_ok() {
                            restore_sensors(&mut sensors)
                        } else {
                            if let Some(sensor) = &mut sensors {
                                sensor.retain_for_recovery();
                            }
                            Ok(())
                        };
                        restore_results([fan, rapl, sensor, off])?;
                        tablet_status(Some(false), None, true, "Paused")?;
                        last_tablet = None;
                        Ok(())
                    }
                    Request::Power(config) => power.apply(&config),
                };
                let fatal_power = power.uncertain();
                if let Err(e) = &operation {
                    eprintln!("broker operation rejected: {e}");
                    reply.rejected = true;
                }
                if report_power {
                    power_status(&power);
                }
                reply.actual_raw = session.fan.as_ref().and_then(|fan| fan.current_raw());
                reply.emergency = emergency.active;
                reply.temperature = health.temperature;
                reply.tach = health.tach;
                reply.tach_state = health.state;
                parent.send(&reply.encode())?;
                if fatal_power {
                    return Err(io::Error::other(
                        "RAPL state uncertain after write/rollback; stopping",
                    ));
                }
                if operation.is_err() && !power_request {
                    return Err(io::Error::other("hardware operation failed"));
                }
            }
            if !paused
                && session.fan.as_ref().is_some_and(|fan| fan.may_be_manual())
                && last_cpu.elapsed() >= Duration::from_secs(1)
            {
                let temp = cpu
                    .as_ref()
                    .ok_or_else(|| io::Error::other("fan CPU sensor is unavailable"))?
                    .read_mc()?;
                if emergency.observe(epoch.elapsed(), temp) {
                    session.fan.as_mut().unwrap().apply(ec::MAX_RAW)?;
                    health.duty(epoch.elapsed(), ec::MAX_RAW);
                }
                last_cpu = Instant::now();
            }
            if !paused
                && let Some(fan) = &session.fan
                && fan.may_be_manual()
            {
                health.refresh(fan, epoch.elapsed())?;
            }
            if let Some(g) = &mut guardian {
                if paused || last_tablet_sample.is_some_and(|t| t.elapsed() < SAMPLE_INTERVAL) {
                    g.send(b'.')?;
                } else {
                    last_tablet_sample = Some(Instant::now());
                    let sample = match sensors.as_ref().map(Sensors::sample).transpose() {
                        Ok(sample) => sample,
                        Err(error) => {
                            g.off()?;
                            tablet_status(Some(false), None, false, "SensorFailure")?;
                            return Err(io::Error::other(format!("tablet sensor failed: {error}")));
                        }
                    };
                    let estimate = estimator.update(sample, lid_closed(), epoch.elapsed());
                    let reason = format!("{:?}", estimate.reason);
                    let state = (estimate.tablet, estimate.reliable, reason.clone());
                    if last_tablet.as_ref() != Some(&state) {
                        g.send(if estimate.tablet { b'1' } else { b'0' })?;
                        // Consume acknowledgement only for an actual OFF message.
                        if !estimate.tablet {
                            let mut ack = [0];
                            g.socket.read_exact(&mut ack)?;
                            if ack != *b"0" {
                                return Err(io::Error::other("tablet OFF not acknowledged"));
                            }
                        }
                        eprintln!(
                            "tablet={} angle={:?} reliable={} reason={reason}",
                            estimate.tablet, estimate.angle_deg, estimate.reliable
                        );
                        last_tablet = Some(state);
                        tablet_log = Instant::now();
                        tablet_status(
                            Some(estimate.tablet),
                            estimate.angle_deg,
                            estimate.reliable,
                            &reason,
                        )?;
                    } else if tablet_log.elapsed() >= Duration::from_secs(5) {
                        tablet_status(
                            Some(estimate.tablet),
                            estimate.angle_deg,
                            estimate.reliable,
                            &reason,
                        )?;
                        tablet_log = Instant::now();
                    }
                    g.send(b'.')?;
                }
            }
            let state = (
                session.fan.as_ref().is_some_and(|fan| fan.may_be_manual()),
                session.fan.as_ref().and_then(|fan| fan.current_raw()),
                emergency.active,
                health.state,
                health.last_check,
            );
            if fan_report != Some(state) {
                fan_status(
                    session.fan.as_ref(),
                    emergency.active,
                    &health,
                    epoch.elapsed(),
                )?;
                fan_report = Some(state);
            }
        }
        Ok(())
    })();
    let tablet_shutdown = guardian.as_mut().map(Guardian::stop);
    let (tablet, reliable, reason) = match &tablet_shutdown {
        Some(GuardianShutdown {
            confirmation: Some(confirmation),
            ..
        }) => (Some(false), true, confirmation.reason()),
        Some(_) => (None, false, "StopUnconfirmed"),
        None => (Some(false), true, "Stopped"),
    };
    let _ = tablet_status(tablet, None, reliable, reason);
    let tablet_error = tablet_shutdown.and_then(|shutdown| shutdown.error);
    if let Some(error) = &tablet_error {
        eprintln!("{error}");
    }
    let fan_result = session.restore();
    let _ = fan_status(session.fan.as_ref(), false, &health, epoch.elapsed());
    let power_result = power.restore();
    power_status(&power);
    let sensor_result = if reliable {
        restore_sensors(&mut sensors)
    } else {
        if let Some(sensor) = &mut sensors {
            sensor.retain_for_recovery();
        }
        Err(io::Error::other(
            "tablet OFF unconfirmed; sensor ownership retained for recovery",
        ))
    };
    drop(child);
    drop(guardian);
    restore_results([fan_result, power_result, sensor_result])?;
    if let Some(error) = tablet_error {
        return Err(error);
    }
    result
}

fn restore_sensors(sensors: &mut Option<Sensors>) -> io::Result<()> {
    if let Some(sensor) = sensors {
        sensor.restore()?;
    }
    *sensors = None;
    Ok(())
}

fn restore_results<const N: usize>(results: [io::Result<()>; N]) -> io::Result<()> {
    let errors: Vec<_> = results
        .into_iter()
        .filter_map(Result::err)
        .map(|e| e.to_string())
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(errors.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Config, Controller};
    use std::{cell::RefCell, rc::Rc};

    #[test]
    fn startup_mode_requires_an_explicit_bounded_initialization_request() {
        assert!(!startup_mode(Request::Initialize { fan_control: false }).unwrap());
        assert!(startup_mode(Request::Initialize { fan_control: true }).unwrap());
        for request in [
            Request::Activate,
            Request::Heartbeat,
            Request::Duty(0),
            Request::Restore,
            Request::Power(Default::default()),
        ] {
            assert!(startup_mode(request).is_err());
        }
    }

    #[derive(Clone, Copy)]
    enum GuardianBehavior {
        Acknowledge,
        ExitWithoutAcknowledgement(i32),
        FailAfterAcknowledgement,
        Killed,
        Stalled,
    }

    fn guardian_fixture(behavior: GuardianBehavior) -> Guardian {
        let (parent, socket) = UnixStream::pair().unwrap();
        parent
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        parent
            .set_write_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let parent_fd = parent.as_raw_fd();
        let child_fd = socket.as_raw_fd();
        // SAFETY: the test child uses only fixed syscalls and _exit, never
        // allocation, Rust destructors or inherited locks from parallel tests.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "{}", io::Error::last_os_error());
        if pid == 0 {
            unsafe {
                libc::close(parent_fd);
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
                let mut poll = libc::pollfd {
                    fd: child_fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                if libc::poll(&mut poll, 1, 2000) != 1 {
                    libc::_exit(110);
                }
                match behavior {
                    GuardianBehavior::Acknowledge | GuardianBehavior::FailAfterAcknowledgement => {
                        let mut message = 0_u8;
                        if libc::read(child_fd, (&raw mut message).cast(), 1) != 1
                            || message != b'0'
                            || libc::write(child_fd, b"0".as_ptr().cast(), 1) != 1
                        {
                            libc::_exit(111);
                        }
                        if matches!(behavior, GuardianBehavior::FailAfterAcknowledgement) {
                            libc::_exit(23);
                        }
                        // Parent shutdown supplies EOF after receiving the ACK.
                        if libc::read(child_fd, (&raw mut message).cast(), 1) != 0 {
                            libc::_exit(112);
                        }
                        libc::_exit(0);
                    }
                    GuardianBehavior::ExitWithoutAcknowledgement(code) => {
                        // Leave the request unread, reproducing the reset race.
                        // Exit0 models guardian_loop's successful OFF/settle path.
                        libc::_exit(code);
                    }
                    GuardianBehavior::Killed => {
                        libc::raise(libc::SIGKILL);
                        libc::_exit(113);
                    }
                    GuardianBehavior::Stalled => loop {
                        libc::pause();
                    },
                }
            }
        }
        drop(socket);
        Guardian {
            socket: parent,
            child: Child { pid, status: None },
        }
    }

    #[test]
    fn guardian_shutdown_confirms_ack_and_preserves_reaped_exit_status() {
        let mut guardian = guardian_fixture(GuardianBehavior::Acknowledge);
        let shutdown = guardian.stop();
        assert_eq!(shutdown.confirmation, Some(OffConfirmation::Acknowledged));
        assert!(shutdown.error.is_none());
        let status = guardian.child.exited().unwrap().unwrap();
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
        assert_eq!(guardian.child.stop().unwrap(), status);
    }

    #[test]
    fn guardian_reset_requires_a_successful_exit_to_confirm_off() {
        for (behavior, expected) in [
            (
                GuardianBehavior::ExitWithoutAcknowledgement(0),
                Some(OffConfirmation::GuardianExited),
            ),
            (GuardianBehavior::ExitWithoutAcknowledgement(23), None),
            (GuardianBehavior::Killed, None),
        ] {
            let mut guardian = guardian_fixture(behavior);
            let shutdown = guardian.stop();
            assert_eq!(shutdown.confirmation, expected);
            assert_eq!(shutdown.error.is_some(), expected.is_none());
            // A second status query must not turn a failure into exit0.
            assert_eq!(guardian.child.exited().unwrap(), guardian.child.status);
            if matches!(behavior, GuardianBehavior::ExitWithoutAcknowledgement(23)) {
                assert_eq!(libc::WEXITSTATUS(guardian.child.status.unwrap()), 23);
            }
        }
    }

    #[test]
    fn guardian_failure_after_ack_keeps_off_proof_but_reports_cleanup_error() {
        let mut guardian = guardian_fixture(GuardianBehavior::FailAfterAcknowledgement);
        let shutdown = guardian.stop();
        assert_eq!(shutdown.confirmation, Some(OffConfirmation::Acknowledged));
        assert!(shutdown.error.is_some());
        assert_eq!(libc::WEXITSTATUS(guardian.child.stop().unwrap()), 23);
    }

    #[test]
    fn guardian_deadline_kills_a_stalled_child_without_confirming_off() {
        let mut guardian = guardian_fixture(GuardianBehavior::Stalled);
        let shutdown = guardian.stop();
        assert!(shutdown.confirmation.is_none());
        assert!(shutdown.error.is_some());
        let status = guardian.child.status.unwrap();
        assert!(libc::WIFSIGNALED(status));
        assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
    }

    #[test]
    fn unconfirmed_stop_snapshot_reports_unknown_tablet_state() {
        let text = tablet_status_text(None, None, false, "StopUnconfirmed");
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value["tablet"].is_null());
        assert_eq!(value["reliable"], false);
        assert_eq!(value["reason"], "StopUnconfirmed");
    }

    #[test]
    fn guard_requires_continuous_cooling_below_the_release_boundary() {
        let mut guard = EmergencyGuard::default();
        let mut observe = |seconds, temp| guard.observe(Duration::from_secs(seconds), temp);
        assert!(!observe(0, 40_000));
        assert!(observe(1, 95_000));
        assert!(observe(2, 89_000));
        assert!(observe(20, 90_000));
        assert!(observe(21, 89_000));
        assert!(observe(50, 89_000));
        assert!(!observe(51, 89_000));
    }

    #[test]
    fn broker_only_heat_pulse_is_reported_and_preserves_policy_minimum_run() {
        #[derive(Clone)]
        struct FakeEc(Rc<RefCell<[u8; 256]>>);
        impl ec::EcIo for FakeEc {
            fn read(&self, register: u8) -> io::Result<u8> {
                Ok(self.0.borrow()[usize::from(register)])
            }
            fn write(&mut self, register: u8, value: u8) -> io::Result<()> {
                self.0.borrow_mut()[usize::from(register)] = value;
                Ok(())
            }
        }
        let io = FakeEc(Rc::new(RefCell::new([0; 256])));
        io.0.borrow_mut()[0x4a] = 91;
        let mut fan = ec::Fan::new(io);
        fan.activate().unwrap();
        let mut policy = Controller::new(Config::default()).unwrap();
        let mut guard = EmergencyGuard::default();
        let time = Duration::from_secs;
        let idle = policy.sample(time(0), 40_000).unwrap();
        fan.apply(idle.raw_duty).unwrap();

        // The worker's next scheduled idle sample is at t=5. Only the broker
        // sees the short heat pulse and starts the real fan at t=1.
        assert!(guard.observe(time(1), 95_000));
        fan.apply(ec::MAX_RAW).unwrap();
        assert!(guard.observe(time(2), 40_000));
        let decision = policy.sample(time(5), 40_000).unwrap();
        assert_eq!(decision.raw_duty, 0);
        let changed = fan
            .apply(if guard.observe(time(5), 40_000) {
                ec::MAX_RAW
            } else {
                decision.raw_duty
            })
            .unwrap();
        assert!(!changed);
        let reply = Reply {
            actual_raw: fan.current_raw(),
            emergency: guard.active,
            ..Reply::default()
        };
        let observed = Reply::decode(&reply.encode(), 0).unwrap();
        assert_eq!(observed.actual_raw, Some(ec::MAX_RAW));
        assert!(observed.emergency);
        policy.observe_emergency(time(5));

        assert_eq!(policy.sample(time(6), 40_000).unwrap().pwm_percent, 80);
        assert!(guard.observe(time(31), 40_000));
        assert!(!guard.observe(time(32), 40_000));
        assert_eq!(policy.sample(time(32), 40_000).unwrap().pwm_percent, 80);
        assert_eq!(policy.sample(time(36), 40_000).unwrap().pwm_percent, 35);
        assert_eq!(policy.sample(time(49), 40_000).unwrap().pwm_percent, 35);
        assert_eq!(policy.sample(time(50), 40_000).unwrap().pwm_percent, 0);
    }

    #[test]
    fn broker_tach_guard_has_one_spinup_deadline_and_never_invents_feedback() {
        let mut health = FanHealth::default();
        assert_eq!(health.state, TachState::Unknown);
        assert_eq!(health.temperature, None);
        assert_eq!(health.tach, None);
        let time = Duration::from_secs;
        health.duty(time(0), ec::MIN_RAW);
        for seconds in 0..=10 {
            // Repeated policy writes cannot restart the independent deadline.
            health.duty(time(seconds), ec::MIN_RAW);
            health.record(time(seconds), 45, None).unwrap();
            assert_eq!(health.state, TachState::Starting);
        }
        assert!(health.record(time(11), 45, Some(0)).is_err());
        assert_eq!(health.state, TachState::Failed);
        health.duty(time(12), 0);
        health.record(time(12), 44, Some(0)).unwrap();
        assert_eq!(health.state, TachState::Stopped);
        health.duty(time(20), ec::MIN_RAW);
        health.record(time(20), 45, Some(1200)).unwrap();
        assert_eq!(health.state, TachState::Recent);
        health.record(time(30), 45, None).unwrap();
        assert!(health.record(time(31), 45, None).is_err());
    }

    #[test]
    fn broker_feedback_cache_prevents_worker_requests_from_resampling_hardware() {
        use std::cell::Cell;
        struct CountingEc {
            values: [u8; 256],
            reads: Rc<Cell<u32>>,
        }
        impl ec::EcIo for CountingEc {
            fn read(&self, register: u8) -> io::Result<u8> {
                self.reads.set(self.reads.get() + 1);
                Ok(self.values[usize::from(register)])
            }
            fn write(&mut self, register: u8, value: u8) -> io::Result<()> {
                self.values[usize::from(register)] = value;
                Ok(())
            }
        }
        let reads = Rc::new(Cell::new(0));
        let mut values = [0; 256];
        values[0x4a] = 91;
        values[0x70] = 45;
        values[0x76] = 4;
        values[0x77] = 176; // 1200 in the EC feedback field.
        let mut fan = ec::Fan::new(CountingEc {
            values,
            reads: Rc::clone(&reads),
        });
        fan.activate().unwrap();
        let mut health = FanHealth::default();
        health.refresh(&fan, Duration::ZERO).unwrap();
        assert_eq!(health.state, TachState::Unknown); // No duty acknowledged yet.
        fan.apply(ec::MIN_RAW).unwrap();
        health.duty(Duration::ZERO, ec::MIN_RAW);
        let initial = reads.get();
        for milliseconds in [0, 250, 1000, 4999] {
            health
                .refresh(&fan, Duration::from_millis(milliseconds))
                .unwrap();
        }
        assert_eq!(reads.get(), initial);
        assert_eq!(health.state, TachState::Starting);
        health.refresh(&fan, Duration::from_secs(5)).unwrap();
        assert!(reads.get() > initial);
        assert_eq!(health.state, TachState::Recent);
        assert_eq!(health.temperature, Some(45));
        assert_eq!(health.tach, Some(1200));
    }
}
