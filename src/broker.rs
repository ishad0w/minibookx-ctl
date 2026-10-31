//! Root hardware broker and independent tablet-switch guardian.
//!
//! Configuration is opened only after the policy process has dropped uid/gid
//! and closed every hardware descriptor. The private socket accepts only fixed,
//! bounded operations; it cannot name a device, register, path or command.
use crate::{
    ec, hardware,
    ipc::{REQUEST_BYTES, Reply, Request, TachState},
    lid::Lid,
    power::Manager,
    sensor::CpuSensor,
    service,
    tablet::{Estimate, EstimateReason, HingeEstimator, SAMPLE_INTERVAL, Sensors, VirtualSwitch},
};
use std::{
    ffi::CString,
    fs,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
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
const CPU_INTERVAL: Duration = Duration::from_secs(1);
const CHILD_INTERVAL: Duration = Duration::from_secs(1);
const DIAGNOSTIC_INTERVAL: Duration = Duration::from_secs(5);
const POLICY_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Default)]
struct TabletRetry {
    next: Option<Duration>,
    failures: u32,
}
impl TabletRetry {
    fn failed(&mut self, now: Duration) {
        self.next = Some(now + Duration::from_secs((30_u64 << self.failures.min(4)).min(300)));
        self.failures = self.failures.saturating_add(1);
    }
    fn due(&self, now: Duration) -> bool {
        self.next.is_some_and(|next| now >= next)
    }
}

fn transient_sample_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EIO | libc::EREMOTEIO | libc::ETIMEDOUT | libc::EAGAIN | libc::EINTR)
    )
}

fn setup_tablet(
    control: &hardware::Control,
    guardian: &mut Option<Guardian>,
    sensors: &mut Option<Sensors>,
    lid: &mut Option<Lid>,
) -> io::Result<()> {
    if let Some(g) = guardian {
        g.off()?;
        // Discovery can outlast the awake heartbeat. Keep OFF confirmed while
        // it runs; the first estimate explicitly rearms supervision.
        g.send(b'P')?;
    }
    restore_sensors(sensors)?;
    crate::tablet::recover_sensor(control.directory(), None)?;
    *lid = Some(Lid::discover()?);
    *sensors = Some(Sensors::discover(control)?);
    Ok(())
}

/// Keep only the newest unsaved value. New observations must not reset a
/// failed write's deadline or multiply attempts during frequent FD wakes.
#[derive(Default)]
struct PendingSnapshot {
    text: Option<String>,
    retry_at: Duration,
}
impl PendingSnapshot {
    fn set(&mut self, text: String) {
        self.text = Some(text);
    }
    fn deadline(&self) -> Option<Duration> {
        self.text.as_ref().map(|_| self.retry_at)
    }
    fn flush(
        &mut self,
        now: Duration,
        write: impl FnOnce(&str) -> io::Result<()>,
    ) -> io::Result<()> {
        if now < self.retry_at {
            return Ok(());
        }
        if let Some(text) = &self.text {
            if let Err(error) = write(text) {
                self.retry_at = now + DIAGNOSTIC_INTERVAL;
                return Err(error);
            }
            self.text = None;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Snapshots {
    fan: PendingSnapshot,
    tablet: PendingSnapshot,
    power: PendingSnapshot,
}
impl Snapshots {
    fn deadline(&self) -> Option<Duration> {
        [&self.fan, &self.tablet, &self.power]
            .into_iter()
            .filter_map(PendingSnapshot::deadline)
            .min()
    }
    fn flush(&mut self, now: Duration, mut write: impl FnMut(&str, &str) -> io::Result<()>) {
        for (name, snapshot) in [
            ("fan", &mut self.fan),
            ("tablet", &mut self.tablet),
            ("power", &mut self.power),
        ] {
            if let Err(error) = snapshot.flush(now, |text| write(name, text)) {
                eprintln!("cannot publish broker {name} status: {error}");
            }
        }
    }
    fn flush_final(&mut self, now: Duration, write: impl FnMut(&str, &str) -> io::Result<()>) {
        // One best-effort final attempt, after hardware release, without
        // waiting for a retry timer or publishing an older queued state.
        for snapshot in [&mut self.fan, &mut self.tablet, &mut self.power] {
            snapshot.retry_at = Duration::ZERO;
        }
        self.flush(now, write);
    }
}

/// Independent monotonic deadlines: an IPC or lid event cannot advance another
/// source's cadence. Skip missed slots rather than replaying stale samples.
struct Periodic {
    next: Duration,
    interval: Duration,
}
impl Periodic {
    fn new(now: Duration, interval: Duration) -> Self {
        Self {
            next: now + interval,
            interval,
        }
    }
    fn take(&mut self, now: Duration) -> bool {
        if now < self.next {
            return false;
        }
        // All intervals are fixed, positive and at most five seconds.
        let late = (now - self.next).as_nanos() % self.interval.as_nanos();
        self.next = now + self.interval - Duration::from_nanos(late as u64);
        true
    }
    fn restart(&mut self, now: Duration) {
        self.next = now + self.interval;
    }
}

struct Schedule {
    children: Periodic,
    posture: Periodic,
    heartbeat: Periodic,
    cpu: Periodic,
}
impl Schedule {
    fn new(now: Duration) -> Self {
        Self {
            children: Periodic::new(now, CHILD_INTERVAL),
            posture: Periodic {
                next: now,
                interval: SAMPLE_INTERVAL,
            },
            heartbeat: Periodic::new(now, SAMPLE_INTERVAL),
            cpu: Periodic::new(now, CPU_INTERVAL),
        }
    }
    fn deadline(&self, tablet_active: bool, fan_active: bool, health: &FanHealth) -> Duration {
        let mut next = self.children.next;
        if tablet_active {
            next = next.min(self.posture.next).min(self.heartbeat.next);
        }
        if fan_active {
            next = next.min(self.cpu.next).min(health.deadline());
        }
        next
    }
}

fn timeout_ms(deadline: Duration, now: Duration) -> i32 {
    crate::systemd::poll_timeout(deadline.saturating_sub(now))
}

/// Both policy requests and the independent guard use this accounting. Record
/// completion only after a successful fresh read, never on an attempted read.
fn sample_cpu(
    read: impl FnOnce() -> io::Result<i32>,
    deadline: &mut Periodic,
    clock: impl FnOnce() -> Duration,
) -> io::Result<i32> {
    let temperature = read()?;
    deadline.restart(clock());
    Ok(temperature)
}

#[derive(Default)]
struct TabletReport {
    commanded: Option<bool>,
    diagnostic: Option<(bool, EstimateReason)>,
    next_snapshot: Duration,
}
impl TabletReport {
    /// Only actual switch transitions (including the first OFF) reach IPC.
    /// Safety OFF/ACK paths deliberately bypass this deduplication.
    fn update(
        &mut self,
        estimate: Estimate,
        now: Duration,
        command: impl FnOnce(bool) -> io::Result<()>,
    ) -> io::Result<(bool, bool)> {
        let changed = self.commanded != Some(estimate.tablet);
        if changed {
            command(estimate.tablet)?;
            self.commanded = Some(estimate.tablet);
        }
        let diagnostic = (estimate.reliable, estimate.reason);
        let snapshot = changed || now >= self.next_snapshot;
        let log = changed || (snapshot && self.diagnostic != Some(diagnostic));
        if snapshot {
            self.diagnostic = Some(diagnostic);
            self.next_snapshot = now + DIAGNOSTIC_INTERVAL;
        }
        Ok((snapshot, log))
    }
}

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
    fn deadline(&self) -> Duration {
        self.last_check
            .map_or(Duration::ZERO, |last| last + FAN_HEALTH_INTERVAL)
    }
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
fn install_signals() -> io::Result<()> {
    // SAFETY: these handlers only store to lock-free atomic booleans.
    for (signal, handler) in [
        (libc::SIGTERM, stop_signal as *const ()),
        (libc::SIGINT, stop_signal as *const ()),
        (libc::SIGHUP, reload_signal as *const ()),
    ] {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = handler as libc::sighandler_t;
        // No SA_RESTART: TERM must interrupt the guardian's disarmed read.
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
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
    Ok(isolate_fds(&[fd])?[0])
}
fn isolate_fds(fds: &[RawFd]) -> io::Result<Vec<RawFd>> {
    let first_closed = 3 + fds.len() as u32;
    let mut copies = Vec::new();
    for fd in fds {
        // SAFETY: duplicate above every destination so crossed fd numbers
        // cannot overwrite the other inherited descriptor during isolation.
        let copy = unsafe { libc::fcntl(*fd, libc::F_DUPFD_CLOEXEC, first_closed as i32) };
        if copy < 0 {
            return Err(io::Error::last_os_error());
        }
        copies.push(unsafe { OwnedFd::from_raw_fd(copy) });
    }
    for (index, fd) in copies.iter().enumerate() {
        if unsafe { libc::dup2(fd.as_raw_fd(), 3 + index as i32) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    drop(copies);
    // SAFETY: fd is owned by this post-fork process. Retain it as fd3, then
    // close all inherited raw EC/I2C/guardian/lease descriptors before setuid.
    if unsafe { libc::syscall(libc::SYS_close_range, first_closed, u32::MAX, 0_u32) } < 0 {
        let fds: Vec<_> = fs::read_dir("/proc/self/fd")?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_string_lossy().parse::<i32>().ok())
            .filter(|n| *n >= first_closed as i32)
            .collect();
        for n in fds {
            unsafe {
                libc::close(n);
            }
        }
    }
    Ok((3..first_closed as i32).collect())
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
        self.stop_while(|| {})
    }
    fn stop_while(&mut self, mut pending: impl FnMut()) -> io::Result<i32> {
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
            pending();
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

/// Hardware has already been released. Let a terminating policy finish its
/// outstanding Restore call without performing another hardware transaction.
fn acknowledge_shutdown(socket: &UnixDatagram, released: bool) {
    let mut bytes = [0; REQUEST_BYTES + 1];
    // The broker endpoint stays nonblocking, including during teardown.
    if let Ok(count) = socket.recv(&mut bytes)
        && let Ok((request, sequence)) = Request::decode(&bytes[..count])
    {
        let _ = socket.send(
            &Reply {
                sequence,
                rejected: request != Request::Restore || !released,
                ..Reply::default()
            }
            .encode(),
        );
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
    BrokerBackup,
}
impl OffConfirmation {
    fn reason(&self) -> &'static str {
        match self {
            Self::Acknowledged => "StoppedOffAcknowledged",
            Self::GuardianExited => "StoppedGuardianExited",
            Self::BrokerBackup => "StoppedBrokerBackup",
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
    backup: Option<VirtualSwitch>,
    off_pending: bool,
}
impl Guardian {
    fn spawn(uid: libc::uid_t, gid: libc::gid_t) -> io::Result<Self> {
        let switch = VirtualSwitch::create()?;
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
                let fds =
                    isolate_fds(&[child_socket.into_raw_fd(), switch.into_fd().into_raw_fd()])?;
                let socket = unsafe { UnixStream::from_raw_fd(fds[0]) };
                let switch = VirtualSwitch::from_fd(unsafe { OwnedFd::from_raw_fd(fds[1]) });
                if unsafe { libc::prctl(libc::PR_SET_NAME, c"minibookx-guard".as_ptr(), 0, 0, 0) }
                    != 0
                {
                    return Err(io::Error::last_os_error());
                }
                drop_user(uid, gid)?;
                guardian_loop(socket, switch)
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
            backup: Some(switch),
            off_pending: false,
        })
    }
    fn send(&mut self, b: u8) -> io::Result<()> {
        self.socket.write_all(&[b])
    }
    fn off(&mut self) -> io::Result<()> {
        if self.off_pending {
            return Err(io::Error::other(
                "previous tablet OFF unconfirmed; guardian must be stopped",
            ));
        }
        // After any incomplete exchange, a late ACK is ambiguous. Never reuse
        // that stream for another OFF; teardown reaps the writer and uses the
        // original switch descriptor if needed.
        self.off_pending = true;
        self.send(b'0')?;
        // An acknowledgement means the same still-live uinput FD received OFF.
        self.socket.set_read_timeout(Some(Duration::from_secs(1)))?;
        let mut ack = [0];
        self.socket.read_exact(&mut ack)?;
        if ack != *b"0" {
            return Err(io::Error::other("tablet OFF acknowledgement missing"));
        }
        self.off_pending = false;
        Ok(())
    }
    fn stop(&mut self) -> GuardianShutdown {
        let acknowledgement = self.off();
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        let exited = self.child.stop();
        // Reap the other writer before the final OFF; it can no longer race us
        // with an old queued ON command. The original device stays alive here.
        let backup_off = self.backup.as_mut().map(VirtualSwitch::force_off);
        if backup_off.as_ref().is_some_and(Result::is_ok) {
            std::thread::sleep(Duration::from_millis(250));
        }
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
        } else if backup_off.as_ref().is_some_and(Result::is_ok) {
            eprintln!("tablet OFF confirmed through broker's original switch descriptor");
            Some(OffConfirmation::BrokerBackup)
        } else {
            None
        };
        let error = (!clean_exit).then(|| {
            io::Error::other(format!(
                "tablet guardian shutdown failed; OFF acknowledgement: {acknowledgement:?}; guardian exit: {exited:?}; backup OFF: {backup_off:?}"
            ))
        });
        self.backup.take();
        GuardianShutdown {
            confirmation,
            error,
        }
    }
}
impl Drop for Guardian {
    fn drop(&mut self) {
        if self.child.status.is_none() || self.backup.is_some() {
            let _ = self.stop();
        }
    }
}
fn guardian_loop(mut socket: UnixStream, mut switch: VirtualSwitch) -> io::Result<()> {
    STOP.store(false, Ordering::Relaxed);
    install_signals()?;
    // Remain OFF during setup. EOF still detects a dead broker; arm the
    // heartbeat timeout only after the first explicit 0/1 command.
    socket.set_read_timeout(None)?;
    socket.set_write_timeout(Some(Duration::from_millis(200)))?;
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
            fs::write("/proc/self/oom_score_adj", b"0\n")?;
            if unsafe { libc::prctl(libc::PR_SET_NAME, c"minibookx-work".as_ptr(), 0, 0, 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
            drop_user(uid, gid)
                .map_err(|e| io::Error::other(format!("worker privilege boundary: {e}")))?;
            // SAFETY: this child remains single-threaded; no environment readers.
            unsafe {
                std::env::remove_var("NOTIFY_SOCKET");
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
/// Fail only this feature after a sensor/lid fault. OFF must be confirmed;
/// otherwise common service cleanup must restore cooling and retain ownership.
fn disable_tablet(
    guardian: &mut Option<Guardian>,
    sensors: &mut Option<Sensors>,
    lid: &mut Option<Lid>,
    snapshot: &mut PendingSnapshot,
    error: io::Error,
) -> io::Result<()> {
    eprintln!("tablet detection unavailable; retry scheduled: {error}");
    if let Some(g) = guardian {
        g.off()?;
        g.send(b'P')?;
    }
    if let Err(error) = restore_sensors(sensors) {
        // Keep the object and lease for stop/ExecStopPost to retry. This cannot
        // prevent fan/power protection from continuing in the meantime.
        eprintln!("tablet sensor restoration pending: {error}");
    }
    *lid = None;
    snapshot.set(tablet_status_text(Some(false), None, false, "Unavailable"));
    Ok(())
}

fn write_snapshot(name: &str, text: &str) -> io::Result<()> {
    if text.len() > 4096 {
        return Err(io::Error::other("broker snapshot exceeds its size bound"));
    }
    // Names are fixed in Snapshots, never accepted through IPC.
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
fn fan_status_text(
    fan: Option<&ec::Fan<ec::FileEc>>,
    emergency: bool,
    health: &FanHealth,
    now: Duration,
) -> String {
    serde_json::json!({
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
    .to_string()
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
pub fn run(path: PathBuf, tablet_enabled: bool) -> io::Result<()> {
    hardware::check_profile()?;
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::other("--daemon requires the root system unit"));
    }
    let (uid, gid) = user()?;
    policy_directory(uid, gid)?;
    let mut guardian = if tablet_enabled {
        Some(Guardian::spawn(uid, gid)?)
    } else {
        None
    };
    let mut estimator = HingeEstimator::default();
    let (parent, worker) = UnixDatagram::pair()?;
    parent.set_nonblocking(true)?;
    let mut child = spawn_policy(worker, path, uid, gid)?;
    let notify = crate::systemd::Notifier::from_environment()?;
    STOP.store(false, Ordering::Relaxed);
    RELOAD.store(false, Ordering::Relaxed);
    let signals = crate::systemd::Signals::for_process()?;
    let (fan_control, sequence) = await_startup(&parent, &mut child)?;
    // Fork both children before opening any hardware/ownership descriptor.
    let mut lid = None;
    let mut snapshots = Snapshots::default();
    // A shared guard serializes sensor wake and all later EC/RAPL operations.
    let control = hardware::Control::open()?;
    let mut sensors = None;
    let mut tablet_failed = false;
    let mut tablet_retry = TabletRetry::default();
    if tablet_enabled {
        let setup = setup_tablet(&control, &mut guardian, &mut sensors, &mut lid);
        if let Err(error) = setup {
            disable_tablet(
                &mut guardian,
                &mut sensors,
                &mut lid,
                &mut snapshots.tablet,
                error,
            )?;
            tablet_failed = true;
            tablet_retry.failed(Duration::ZERO);
        }
    }
    // Configuration has only been parsed in the fully unprivileged worker.
    // This single bounded choice is fixed for the lifetime of this broker.
    let mut session = hardware::Session::open_locked(fan_control, control.clone())?;
    let cpu = fan_control
        .then(|| CpuSensor::discover(Path::new("/sys/class/hwmon")))
        .transpose()?;
    let mut power = Manager::new();
    snapshots.power.set(power.diagnostic_status().to_string());
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
    let mut last_duty = epoch.elapsed();
    let mut schedule = Schedule::new(epoch.elapsed());
    let mut health = FanHealth::default();
    let mut tablet_report = TabletReport::default();
    let mut paused = false;
    let mut emergency = EmergencyGuard::default();
    let mut rate_epoch = Instant::now();
    let mut requests = 0_u32;
    let mut fan_report = None;
    let mut ready = false;
    let result = (|| {
        while !STOP.load(Ordering::Relaxed) {
            for signal in signals.drain()? {
                if signal == libc::SIGHUP {
                    RELOAD.store(true, Ordering::Relaxed);
                } else {
                    STOP.store(true, Ordering::Relaxed);
                }
            }
            if STOP.load(Ordering::Relaxed) {
                break;
            }
            if RELOAD.swap(false, Ordering::Relaxed) {
                unsafe {
                    libc::kill(child.pid, libc::SIGHUP);
                }
            }
            if schedule.children.take(epoch.elapsed()) {
                if let Some(status) = child.exited()? {
                    return Err(io::Error::other(format!(
                        "policy process exited unexpectedly: {status}"
                    )));
                }
                if let Some(g) = &mut guardian
                    && g.child.exited()?.is_some()
                {
                    return Err(io::Error::other("tablet guardian exited"));
                }
            }
            let manual = session.fan.as_ref().is_some_and(|fan| fan.may_be_manual());
            if !paused && manual && epoch.elapsed().saturating_sub(last_duty) >= POLICY_TIMEOUT {
                return Err(io::Error::other("policy duty updates expired"));
            }
            if !paused && last_request.elapsed() >= POLICY_TIMEOUT {
                return Err(io::Error::other("policy heartbeat expired"));
            }
            let mut deadline = schedule.deadline(
                guardian.is_some() && !paused && !tablet_failed,
                !paused && session.fan.as_ref().is_some_and(|fan| fan.may_be_manual()),
                &health,
            );
            if let Some(next) = snapshots.deadline() {
                deadline = deadline.min(next);
            }
            if !paused {
                deadline = deadline.min(last_request.duration_since(epoch) + POLICY_TIMEOUT);
                if manual {
                    deadline = deadline.min(last_duty + POLICY_TIMEOUT);
                }
                if let Some(next) = tablet_retry.next {
                    deadline = deadline.min(next);
                }
            }
            let source = |fd, events| libc::pollfd {
                fd,
                events,
                revents: 0,
            };
            let mut sources = [
                source(parent.as_raw_fd(), libc::POLLIN),
                source(
                    if paused {
                        -1
                    } else {
                        lid.as_ref().map_or(-1, Lid::fd)
                    },
                    libc::POLLIN,
                ),
                // No guardian data is unsolicited; watch only disconnects.
                source(guardian.as_ref().map_or(-1, |g| g.socket.as_raw_fd()), 0),
                source(signals.fd(), libc::POLLIN),
            ];
            // SAFETY: all descriptors remain owned outside this bounded poll.
            let n = unsafe {
                libc::poll(
                    sources.as_mut_ptr(),
                    sources.len() as libc::nfds_t,
                    timeout_ms(deadline, epoch.elapsed()),
                )
            };
            if n < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            // TERM may arrive with a readable request/socket closure. Enter
            // cleanup before talking to a guardian that is also shutting down.
            if sources[3].revents != 0 {
                // Drain before processing hardware IPC. A signal queued just
                // before poll is readable, so there is no check/poll race.
                for signal in signals.drain()? {
                    if signal == libc::SIGHUP {
                        RELOAD.store(true, Ordering::Relaxed);
                    } else {
                        STOP.store(true, Ordering::Relaxed);
                    }
                }
                if STOP.load(Ordering::Relaxed) {
                    break;
                }
            }
            for index in [0, 2] {
                if sources[index].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    return Err(io::Error::other("broker child connection lost"));
                }
            }
            let lid_error = if sources[1].revents != 0 {
                lid.as_mut().unwrap().drain(sources[1].revents).err()
            } else {
                None
            };
            if sources[0].revents & libc::POLLIN != 0 {
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
                let operation: io::Result<()> = (|| match request {
                    Request::Initialize { .. } => Err(io::Error::other(
                        "startup mode is already fixed; restart required",
                    )),
                    Request::Ready if !ready => {
                        notify.send(if fan_control {
                            "READY=1\nWATCHDOG=1\nSTATUS=Controller active; managed fan control"
                        } else {
                            "READY=1\nWATCHDOG=1\nSTATUS=Controller active; firmware fan control"
                        })?;
                        ready = true;
                        if tablet_failed {
                            notify.send("STATUS=Cooling active; tablet detection unavailable, retry scheduled")?;
                        }
                        Ok(())
                    }
                    Request::Ready => {
                        Err(io::Error::other("policy readiness already acknowledged"))
                    }
                    Request::Heartbeat => {
                        if !paused
                            && session.fan.as_ref().is_some_and(|fan| fan.may_be_manual())
                            && epoch.elapsed().saturating_sub(last_duty) >= POLICY_TIMEOUT
                        {
                            return Err(io::Error::other("policy duty updates expired"));
                        }
                        if ready {
                            notify.send("WATCHDOG=1")?;
                        }
                        Ok(())
                    }
                    Request::Activate => {
                        if paused {
                            estimator.reset();
                            if tablet_enabled {
                                let setup =
                                    setup_tablet(&control, &mut guardian, &mut sensors, &mut lid);
                                if let Err(error) = setup {
                                    disable_tablet(
                                        &mut guardian,
                                        &mut sensors,
                                        &mut lid,
                                        &mut snapshots.tablet,
                                        error,
                                    )?;
                                    tablet_failed = true;
                                    tablet_retry.failed(epoch.elapsed());
                                } else {
                                    tablet_failed = false;
                                    tablet_retry = TabletRetry::default();
                                }
                            }
                            schedule.posture.next = epoch.elapsed();
                            schedule.heartbeat.restart(epoch.elapsed());
                            schedule.cpu.next = epoch.elapsed();
                        }
                        paused = false;
                        last_duty = epoch.elapsed();
                        session.activate()
                    }
                    Request::Duty(raw) => {
                        let fan = session
                            .fan
                            .as_mut()
                            .ok_or_else(|| io::Error::other("fan control is disabled"))?;
                        let sensor = cpu
                            .as_ref()
                            .ok_or_else(|| io::Error::other("fan CPU sensor is unavailable"))?;
                        let temperature =
                            sample_cpu(|| sensor.read_mc(), &mut schedule.cpu, || epoch.elapsed())?;
                        let target = if emergency.observe(epoch.elapsed(), temperature) {
                            ec::MAX_RAW
                        } else {
                            raw
                        };
                        reply.changed = fan.apply(target)?;
                        health.duty(epoch.elapsed(), target);
                        health.refresh(fan, epoch.elapsed())?;
                        last_duty = epoch.elapsed();
                        Ok(())
                    }
                    Request::Restore => {
                        paused = true;
                        emergency = EmergencyGuard::default();
                        health = FanHealth::default();
                        estimator.reset();
                        // Return cooling/power before any blocking guardian
                        // exchange. A failure must not skip another domain.
                        let fan = session.restore();
                        let rapl = power.restore();
                        let off = guardian
                            .as_mut()
                            .map_or(Ok(()), |g| g.off().and_then(|()| g.send(b'P')));
                        // Keep sensor attribution if OFF is unconfirmed.
                        let sensor = if off.is_ok() {
                            restore_sensors(&mut sensors)
                        } else {
                            if let Some(sensor) = &mut sensors {
                                sensor.retain_for_recovery();
                            }
                            Ok(())
                        };
                        let sensor_pending = sensor.is_err()
                            || crate::tablet::sensor_recovery_pending(control.directory());
                        reply.recovery_pending =
                            sensor_pending || rapl.is_err() || power.restoration_pending();
                        for (name, result) in [("sensor", sensor), ("power", rapl)] {
                            if let Err(error) = result {
                                eprintln!("{name} recovery deferred while paused: {error}");
                            }
                        }
                        restore_results([fan, off])?;
                        snapshots.tablet.set(tablet_status_text(
                            Some(false),
                            None,
                            !tablet_failed && !sensor_pending,
                            if sensor_pending {
                                "RecoveryPending"
                            } else if tablet_failed {
                                "Unavailable"
                            } else {
                                "Paused"
                            },
                        ));
                        tablet_report = TabletReport::default();
                        Ok(())
                    }
                    Request::Power(config) => power.apply(&config),
                })();
                let fatal_power = power.uncertain();
                if let Err(e) = &operation {
                    eprintln!("broker operation rejected: {e}");
                    if power_request && !fatal_power {
                        // The bounded optional request is declined, not applied.
                        // Keep the worker's healthy fan/tablet policy alive and
                        // expose the explicit deferred outcome and diagnostic.
                        reply.recovery_pending = true;
                    } else {
                        reply.rejected = true;
                    }
                }
                if report_power {
                    snapshots.power.set(power.diagnostic_status().to_string());
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
                && epoch.elapsed() >= schedule.cpu.next
            {
                let sensor = cpu
                    .as_ref()
                    .ok_or_else(|| io::Error::other("fan CPU sensor is unavailable"))?;
                let temp = sample_cpu(|| sensor.read_mc(), &mut schedule.cpu, || epoch.elapsed())?;
                if emergency.observe(epoch.elapsed(), temp) {
                    session.fan.as_mut().unwrap().apply(ec::MAX_RAW)?;
                    health.duty(epoch.elapsed(), ec::MAX_RAW);
                }
            }
            if !paused
                && let Some(fan) = &session.fan
                && fan.may_be_manual()
            {
                health.refresh(fan, epoch.elapsed())?;
            }
            let tablet_result = (|| {
                if !paused && tablet_retry.due(epoch.elapsed()) {
                    setup_tablet(&control, &mut guardian, &mut sensors, &mut lid)?;
                    tablet_failed = false;
                    tablet_retry = TabletRetry::default();
                    estimator.reset();
                    tablet_report = TabletReport::default();
                    schedule.posture.next = epoch.elapsed();
                    schedule.heartbeat.restart(epoch.elapsed());
                    notify.send("STATUS=Controller active; tablet detection recovered")?;
                }
                if let Some(error) = lid_error {
                    return Err(error);
                }
                if let Some(g) = &mut guardian {
                    if !paused && !tablet_failed && schedule.posture.take(epoch.elapsed()) {
                        let sample = match sensors.as_ref().map(Sensors::sample).transpose() {
                            Ok(sample) => sample,
                            Err(error) if transient_sample_error(&error) => None,
                            Err(error) => return Err(error),
                        };
                        let estimate = estimator.update(
                            sample,
                            lid.as_ref().and_then(Lid::closed),
                            epoch.elapsed(),
                        );
                        let (snapshot, log) =
                            tablet_report.update(estimate, epoch.elapsed(), |tablet| {
                                if tablet { g.send(b'1') } else { g.off() }
                            })?;
                        if log {
                            eprintln!(
                                "tablet={} angle={:?} reliable={} reason={:?}",
                                estimate.tablet,
                                estimate.angle_deg,
                                estimate.reliable,
                                estimate.reason
                            );
                        }
                        if snapshot {
                            snapshots.tablet.set(tablet_status_text(
                                Some(estimate.tablet),
                                estimate.angle_deg,
                                estimate.reliable,
                                &format!("{:?}", estimate.reason),
                            ));
                        }
                    }
                    if !paused && !tablet_failed && schedule.heartbeat.take(epoch.elapsed()) {
                        g.send(b'.')?;
                    }
                }
                Ok::<_, io::Error>(())
            })();
            if let Err(error) = tablet_result {
                disable_tablet(
                    &mut guardian,
                    &mut sensors,
                    &mut lid,
                    &mut snapshots.tablet,
                    error,
                )?;
                tablet_failed = true;
                tablet_retry.failed(epoch.elapsed());
                notify
                    .send("STATUS=Cooling active; tablet detection unavailable, retry scheduled")?;
                estimator.reset();
            }
            let state = (
                session.fan.as_ref().is_some_and(|fan| fan.may_be_manual()),
                session.fan.as_ref().and_then(|fan| fan.current_raw()),
                emergency.active,
                health.state,
                health.last_check,
            );
            if fan_report != Some(state) {
                snapshots.fan.set(fan_status_text(
                    session.fan.as_ref(),
                    emergency.active,
                    &health,
                    epoch.elapsed(),
                ));
                fan_report = Some(state);
            }
            snapshots.flush(epoch.elapsed(), write_snapshot);
        }
        Ok(())
    })();
    // Do not leave the fan in manual mode while waiting for guardian OFF/exit.
    let fan_result = session.restore();
    snapshots.fan.set(fan_status_text(
        session.fan.as_ref(),
        false,
        &health,
        epoch.elapsed(),
    ));
    let power_result = power.restore();
    snapshots.power.set(power.diagnostic_status().to_string());
    if let Err(error) = notify.send("STOPPING=1") {
        eprintln!("systemd stop notification failed: {error}");
    }
    let tablet_shutdown = guardian.as_mut().map(Guardian::stop);
    let (tablet, reliable, reason) = match &tablet_shutdown {
        Some(GuardianShutdown {
            confirmation: Some(confirmation),
            ..
        }) => (Some(false), true, confirmation.reason()),
        Some(_) => (None, false, "StopUnconfirmed"),
        None => (Some(false), true, "Stopped"),
    };
    snapshots
        .tablet
        .set(tablet_status_text(tablet, None, reliable, reason));
    let tablet_error = tablet_shutdown.and_then(|shutdown| shutdown.error);
    if let Some(error) = &tablet_error {
        eprintln!("{error}");
    }
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
    let released = fan_result.is_ok()
        && power_result.is_ok()
        && !power.restoration_pending()
        && sensor_result.is_ok()
        && !crate::tablet::sensor_recovery_pending(control.directory())
        && tablet_error.is_none();
    match child.stop_while(|| acknowledge_shutdown(&parent, released)) {
        Ok(status) if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 => {
            eprintln!("policy worker exited cleanly");
        }
        result => eprintln!("policy worker shutdown: {result:?}"),
    }
    drop(child);
    drop(guardian);
    snapshots.flush_final(epoch.elapsed(), write_snapshot);
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

    #[test]
    fn tablet_retry_is_bounded_and_identity_errors_are_not_transient() {
        let mut retry = TabletRetry::default();
        assert!(!retry.due(Duration::from_secs(999)));
        let mut now = Duration::ZERO;
        for expected in [30, 60, 120, 240, 300, 300] {
            retry.failed(now);
            let next = now + Duration::from_secs(expected);
            assert!(!retry.due(next - Duration::from_nanos(1)));
            assert!(retry.due(next));
            now = next;
        }
        assert!(transient_sample_error(&io::Error::from_raw_os_error(
            libc::EREMOTEIO
        )));
        assert!(!transient_sample_error(&io::Error::new(
            io::ErrorKind::InvalidData,
            "foreign CONTROL"
        )));
        assert!(!transient_sample_error(&io::Error::from_raw_os_error(
            libc::ENODEV
        )));
    }

    #[test]
    fn killed_guardian_uses_brokers_copy_of_the_original_switch() {
        let path = std::env::temp_dir().join(format!("minibookx-backup-{}", std::process::id()));
        let file = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut guardian = guardian_fixture(GuardianBehavior::Stalled);
        guardian.backup = Some(VirtualSwitch::from_fd(file.into()));
        // The stand-in FD accepts production input_event writes. A failed
        // child cannot acknowledge OFF, so only the retained FD proves it.
        let shutdown = guardian.stop();
        assert_eq!(shutdown.confirmation, Some(OffConfirmation::BrokerBackup));
        assert!(shutdown.error.is_some());
        assert!(!fs::read(&path).unwrap().is_empty());
        fs::remove_file(path).unwrap();
    }
    use crate::policy::{Config, Controller};
    use std::{cell::RefCell, rc::Rc};

    #[test]
    fn shutdown_acknowledges_only_restore_after_successful_release() {
        let (broker, worker) = UnixDatagram::pair().unwrap();
        broker.set_nonblocking(true).unwrap();
        worker
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        for (sequence, (request, released, accepted)) in [
            (Request::Restore, true, true),
            (Request::Restore, false, false),
            (Request::Duty(ec::MIN_RAW), true, false),
            (Request::Activate, true, false),
            (Request::Heartbeat, true, false),
        ]
        .into_iter()
        .enumerate()
        {
            let sequence = sequence as u32;
            worker.send(&request.encode(sequence)).unwrap();
            acknowledge_shutdown(&broker, released);
            let mut reply = [0; crate::ipc::REPLY_BYTES + 1];
            let count = worker.recv(&mut reply).unwrap();
            assert_eq!(Reply::decode(&reply[..count], sequence).is_ok(), accepted);
        }
        worker.send(&[0; REQUEST_BYTES + 1]).unwrap();
        acknowledge_shutdown(&broker, true);
        assert!(worker.recv(&mut [0; crate::ipc::REPLY_BYTES]).is_err());
        // With no queued request, teardown must not block awaiting the worker.
        acknowledge_shutdown(&broker, true);
    }

    fn estimate(tablet: bool, reason: EstimateReason) -> Estimate {
        Estimate {
            tablet,
            reason,
            reliable: reason == EstimateReason::Valid,
            angle_deg: None,
        }
    }

    #[test]
    fn failed_snapshot_keeps_the_newest_value_without_early_retries() {
        let mut snapshot = PendingSnapshot::default();
        snapshot.set("Valid".into());
        let mut writes = Vec::new();
        assert!(
            snapshot
                .flush(Duration::ZERO, |text| {
                    writes.push(text.to_owned());
                    Err(io::Error::other("status storage unavailable"))
                })
                .is_err()
        );
        for ms in (0..5_000).step_by(10) {
            snapshot.set("Unavailable".into());
            snapshot
                .flush(Duration::from_millis(ms), |text| {
                    writes.push(text.to_owned());
                    Ok(())
                })
                .unwrap();
            assert_eq!(snapshot.deadline(), Some(Duration::from_secs(5)));
        }
        assert_eq!(writes, ["Valid"]);
        snapshot
            .flush(Duration::from_secs(5), |text| {
                writes.push(text.to_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(writes, ["Valid", "Unavailable"]);
        assert_eq!(snapshot.deadline(), None);
        snapshot
            .flush(Duration::from_secs(30), |_| panic!("clean record retried"))
            .unwrap();
    }

    #[test]
    fn snapshot_domains_progress_independently_and_successful_changes_are_immediate() {
        let mut snapshots = Snapshots::default();
        snapshots.fan.set("fan".into());
        snapshots.tablet.set("tablet".into());
        let mut writes = Vec::new();
        snapshots.flush(Duration::ZERO, |name, _| {
            writes.push(name.to_owned());
            if name == "fan" {
                Err(io::Error::other("fan snapshot blocked"))
            } else {
                Ok(())
            }
        });
        assert_eq!(snapshots.deadline(), Some(Duration::from_secs(5)));
        snapshots.tablet.set("changed".into());
        snapshots.power.set("power".into());
        snapshots.flush(Duration::from_secs(1), |name, _| {
            writes.push(name.to_owned());
            Ok(())
        });
        assert_eq!(writes, ["fan", "tablet", "tablet", "power"]);
        assert_eq!(snapshots.deadline(), Some(Duration::from_secs(5)));
        snapshots.flush(Duration::from_secs(5), |name, _| {
            writes.push(name.to_owned());
            Ok(())
        });
        assert_eq!(writes, ["fan", "tablet", "tablet", "power", "fan"]);
        assert_eq!(snapshots.deadline(), None);
    }

    #[test]
    fn delayed_snapshot_retry_skips_missed_slots_instead_of_spinning() {
        let mut snapshot = PendingSnapshot::default();
        snapshot.set("pending".into());
        let mut attempts = 0;
        for now in [0, 30, 30, 31, 34] {
            let _ = snapshot.flush(Duration::from_secs(now), |_| {
                attempts += 1;
                Err(io::Error::other("still blocked"))
            });
        }
        assert_eq!(attempts, 2);
        assert_eq!(snapshot.deadline(), Some(Duration::from_secs(35)));
    }

    #[test]
    fn final_snapshot_replaces_failed_live_state_without_waiting_or_skipping_domains() {
        let mut snapshots = Snapshots::default();
        snapshots.tablet.set("tablet ON".into());
        snapshots.flush(Duration::ZERO, |_, _| Err(io::Error::other("blocked")));
        snapshots.tablet.set("tablet OFF".into());
        snapshots.fan.set("firmware".into());
        snapshots.power.set("restored".into());
        let mut writes = Vec::new();
        snapshots.flush_final(Duration::from_secs(1), |name, text| {
            writes.push((name.to_owned(), text.to_owned()));
            if name == "fan" {
                Err(io::Error::other("fan status still blocked"))
            } else {
                Ok(())
            }
        });
        assert_eq!(
            writes,
            [
                ("fan".into(), "firmware".into()),
                ("tablet".into(), "tablet OFF".into()),
                ("power".into(), "restored".into()),
            ]
        );
        assert!(snapshots.tablet.deadline().is_none());
        assert!(snapshots.power.deadline().is_none());
        assert_eq!(snapshots.fan.deadline(), Some(Duration::from_secs(6)));
    }

    #[test]
    fn diagnostics_do_not_repeat_switch_commands_and_real_transitions_are_immediate() {
        let mut report = TabletReport::default();
        let mut commands = Vec::new();
        let mut logs = 0;
        let mut snapshots = 0;
        for milliseconds in (0..10_000).step_by(250) {
            let reason = if milliseconds % 500 == 0 {
                EstimateReason::Moving
            } else {
                EstimateReason::Valid
            };
            let (snapshot, log) = report
                .update(
                    estimate(false, reason),
                    Duration::from_millis(milliseconds),
                    |value| {
                        commands.push(value);
                        Ok(())
                    },
                )
                .unwrap();
            logs += usize::from(log);
            snapshots += usize::from(snapshot);
        }
        assert_eq!(commands, [false]); // Includes the first explicit OFF/ACK.
        assert_eq!(snapshots, 2);
        assert!(logs <= 2);
        for tablet in [true, false] {
            assert_eq!(
                report
                    .update(
                        estimate(tablet, EstimateReason::Valid),
                        Duration::from_millis(10_001),
                        |value| {
                            commands.push(value);
                            Ok(())
                        }
                    )
                    .unwrap(),
                (true, true)
            );
        }
        assert_eq!(commands, [false, true, false]);
    }

    #[test]
    fn first_off_requires_ack_and_failed_commands_are_not_cached() {
        let mut report = TabletReport::default();
        let first = estimate(false, EstimateReason::Moving);
        assert!(
            report
                .update(first, Duration::ZERO, |_| Err(io::Error::other("no ACK")))
                .is_err()
        );
        assert_eq!(report.commanded, None);
        let mut guardian = guardian_fixture(GuardianBehavior::Acknowledge);
        report
            .update(first, Duration::ZERO, |value| {
                assert!(!value);
                guardian.off()
            })
            .unwrap();
        assert_eq!(report.commanded, Some(false));
        // The fixture exits cleanly only after it received OFF and supplied ACK.
        guardian.socket.shutdown(std::net::Shutdown::Both).unwrap();
        let status = guardian.child.stop().unwrap();
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    }

    #[test]
    fn early_wakes_do_not_multiply_posture_cpu_or_heartbeat_work() {
        let mut schedule = Schedule::new(Duration::ZERO);
        let mut counts = [0; 4];
        // Fake hardware sees 60,001 wakes (e.g. IPC/lid), but only due sources
        // may run. No wall clock sleeps or physical sensor reads are involved.
        for milliseconds in 0..=60_000 {
            let now = Duration::from_millis(milliseconds);
            if schedule.posture.take(now) {
                counts[0] += 1;
            }
            if now >= schedule.cpu.next {
                sample_cpu(
                    || {
                        counts[1] += 1;
                        Ok(45_000)
                    },
                    &mut schedule.cpu,
                    || now,
                )
                .unwrap();
            }
            if schedule.heartbeat.take(now) {
                counts[2] += 1;
            }
            if schedule.children.take(now) {
                counts[3] += 1;
            }
        }
        assert_eq!(counts, [241, 60, 240, 60]);
    }

    #[test]
    fn periodic_deadlines_skip_missed_slots_without_drift_or_zero_timeout_spin() {
        let mut periodic = Periodic::new(Duration::ZERO, SAMPLE_INTERVAL);
        assert!(!periodic.take(Duration::from_millis(249)));
        assert!(periodic.take(Duration::from_millis(751)));
        assert_eq!(periodic.next, Duration::from_secs(1));
        assert!(!periodic.take(Duration::from_millis(751)));
        assert_eq!(
            timeout_ms(periodic.next, Duration::from_nanos(999_999_999)),
            1
        );
        assert_eq!(timeout_ms(periodic.next, periodic.next), 0);
        assert_eq!(timeout_ms(Duration::MAX, Duration::ZERO), i32::MAX);
    }

    #[test]
    fn inactive_sources_cannot_force_busy_polling() {
        let mut schedule = Schedule::new(Duration::ZERO);
        let health = FanHealth::default();
        // --no-tablet with fan_control=false, and all paused modes: the stale
        // zero posture/health deadlines are excluded. Only supervision remains.
        for seconds in 0..60 {
            let now = Duration::from_secs(seconds);
            schedule.children.take(now);
            assert_eq!(
                timeout_ms(schedule.deadline(false, false, &health), now),
                1000
            );
        }
        assert_eq!(schedule.posture.next, Duration::ZERO);
        assert_eq!(schedule.cpu.next, CPU_INTERVAL);
    }

    #[test]
    fn fresh_duty_sample_defers_only_one_second_and_failed_reads_never_defer_guard() {
        use std::cell::Cell;
        let mut next = Periodic::new(Duration::ZERO, CPU_INTERVAL);
        let reads = Cell::new(0);
        let temperature = Cell::new(45_000);
        let read = || {
            reads.set(reads.get() + 1);
            Ok(temperature.get())
        };
        // Duty arrives just before the independent deadline. Even with no more
        // worker messages, the broker must read again one second later.
        sample_cpu(read, &mut next, || Duration::from_millis(999)).unwrap();
        assert_eq!(next.next, Duration::from_millis(1999));
        assert!(Duration::from_secs(1) < next.next);
        temperature.set(95_000);
        let mut guard = EmergencyGuard::default();
        let now = next.next;
        let temp = sample_cpu(read, &mut next, || now).unwrap();
        assert!(guard.observe(now, temp));
        assert_eq!(reads.get(), 2);
        let before_failure = next.next;
        assert!(
            sample_cpu(
                || Err(io::Error::other("sensor failed")),
                &mut next,
                || Duration::from_secs(10)
            )
            .is_err()
        );
        assert_eq!(next.next, before_failure);
        // Completion, rather than read start, anchors the fresh-sample bound.
        sample_cpu(read, &mut next, || Duration::from_secs(11)).unwrap();
        assert_eq!(next.next, Duration::from_secs(12));
    }

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
        LateAcknowledgement,
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
                    GuardianBehavior::Acknowledge
                    | GuardianBehavior::LateAcknowledgement
                    | GuardianBehavior::FailAfterAcknowledgement => {
                        let mut message = 0_u8;
                        if libc::read(child_fd, (&raw mut message).cast(), 1) != 1
                            || message != b'0'
                        {
                            libc::_exit(111);
                        }
                        if matches!(behavior, GuardianBehavior::LateAcknowledgement) {
                            libc::nanosleep(
                                &libc::timespec {
                                    tv_sec: 1,
                                    tv_nsec: 100_000_000,
                                },
                                std::ptr::null_mut(),
                            );
                        }
                        if libc::write(child_fd, b"0".as_ptr().cast(), 1) != 1 {
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
            backup: None,
            off_pending: false,
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
    fn a_late_off_ack_cannot_confirm_a_subsequent_transaction() {
        let mut guardian = guardian_fixture(GuardianBehavior::LateAcknowledgement);
        assert!(guardian.off().is_err(), "the first OFF must time out");
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            guardian.off().is_err(),
            "an old ACK must not confirm a new OFF"
        );
        let shutdown = guardian.stop();
        assert_ne!(shutdown.confirmation, Some(OffConfirmation::Acknowledged));
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
