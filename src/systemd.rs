//! Linux event sources and systemd/logind integration without libsystemd.
//! D-Bus is processed by the caller's single poll loop. Vendored libdbus is
//! linked statically; no process, async executor or worker thread is spawned.
use dbus::{
    Message, MessageType,
    arg::Variant,
    channel::{BusType, Channel, Watch, default_reply},
};
use std::{
    env,
    ffi::{CString, OsStr, OsString},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        linux::net::SocketAddrExt,
        unix::{
            ffi::OsStrExt,
            net::{SocketAddr, UnixDatagram},
        },
    },
    path::Path,
    time::Duration,
};
const LOGIND: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const CALL_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) fn context(stage: &'static str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{stage}: {error}"))
}
fn bus_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("logind D-Bus: {error}"))
}
fn call(channel: &Channel, message: Message) -> io::Result<Message> {
    channel
        .send_with_reply_and_block(message, CALL_TIMEOUT)
        .map_err(bus_error)
}
fn method(destination: &str, path: &str, interface: &str, member: &str) -> io::Result<Message> {
    Message::new_method_call(destination, path, interface, member).map_err(bus_error)
}
/// Separate synchronous replies cannot consume queued sleep notifications.
pub struct Logind {
    events: Channel,
    calls: Channel,
    owner: String,
}
impl Logind {
    pub fn connect() -> io::Result<Self> {
        let mut events = Channel::get_private(BusType::System).map_err(bus_error)?;
        let calls = Channel::get_private(BusType::System).map_err(bus_error)?;
        for rule in [
            "type='signal',sender='org.freedesktop.login1',path='/org/freedesktop/login1',interface='org.freedesktop.login1.Manager',member='PrepareForSleep'",
            "type='signal',sender='org.freedesktop.DBus',path='/org/freedesktop/DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='org.freedesktop.login1'",
        ] {
            call(
                &events,
                method(
                    "org.freedesktop.DBus",
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "AddMatch",
                )?
                .append1(rule),
            )?;
        }
        let owner = call(
            &calls,
            method(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetNameOwner",
            )?
            .append1(LOGIND),
        )?
        .read1::<String>()
        .map_err(bus_error)?;
        events.set_watch_enabled(true);
        Ok(Self {
            events,
            calls,
            owner,
        })
    }
    pub fn watch(&self) -> Watch {
        self.events.watch()
    }
    pub fn check_connected(&self) -> io::Result<()> {
        if self.events.is_connected() && self.calls.is_connected() {
            Ok(())
        } else {
            Err(io::Error::other("system D-Bus disconnected"))
        }
    }
    pub fn delay_inhibitor(&self) -> io::Result<OwnedFd> {
        call(
            &self.calls,
            method(LOGIND, MANAGER_PATH, MANAGER, "Inhibit")?
                .append3(
                    "sleep",
                    "minibookx-ctl",
                    "Restore automatic cooling and power limits before suspend",
                )
                .append1("delay"),
        )?
        .read1::<OwnedFd>()
        .map_err(bus_error)
    }
    pub fn preparing_for_sleep(&self) -> io::Result<bool> {
        let value: Variant<bool> = call(
            &self.calls,
            method(
                LOGIND,
                MANAGER_PATH,
                "org.freedesktop.DBus.Properties",
                "Get",
            )?
            .append2(MANAGER, "PreparingForSleep"),
        )?
        .read1()
        .map_err(bus_error)?;
        Ok(value.0)
    }
    /// Drain all pending signals without blocking. An owner change is fatal:
    /// EC automation is safer than continuing without reliable sleep events.
    pub fn drain_sleep_events(&self) -> io::Result<Vec<bool>> {
        self.events
            .read_write(Some(Duration::ZERO))
            .map_err(|()| io::Error::other("system D-Bus disconnected"))?;
        let mut events = Vec::new();
        while let Some(message) = self.events.pop_message() {
            if let Some(preparing) = classify_sleep_signal(&message, &self.owner)? {
                events.push(preparing);
            } else if message.msg_type() == MessageType::Signal
                && message.sender().as_deref() == Some("org.freedesktop.DBus")
                && message.path().as_deref() == Some("/org/freedesktop/DBus")
                && message.interface().as_deref() == Some("org.freedesktop.DBus")
                && message.member().as_deref() == Some("NameOwnerChanged")
            {
                let (name, old, new) = message
                    .read3::<String, String, String>()
                    .map_err(bus_error)?;
                if name == LOGIND && old != new {
                    return Err(io::Error::other(
                        "logind restarted; returning control to firmware",
                    ));
                }
            } else if let Some(reply) = default_reply(&message) {
                self.events
                    .send(reply)
                    .map_err(|()| io::Error::other("D-Bus reply failed"))?;
            }
        }
        self.check_connected()?;
        Ok(events)
    }
}
fn classify_sleep_signal(message: &Message, owner: &str) -> io::Result<Option<bool>> {
    if message.msg_type() != MessageType::Signal
        || message.sender().as_deref() != Some(owner)
        || message.path().as_deref() != Some(MANAGER_PATH)
        || message.interface().as_deref() != Some(MANAGER)
        || message.member().as_deref() != Some("PrepareForSleep")
    {
        return Ok(None);
    }
    message.read1().map(Some).map_err(bus_error)
}
/// Persistent AF_UNIX datagram socket, including systemd's @abstract names.
pub struct Notifier {
    socket: Option<UnixDatagram>,
}
impl Notifier {
    pub fn from_environment() -> io::Result<Self> {
        let socket = env::var_os("NOTIFY_SOCKET")
            .map(|name| notify_socket(&name))
            .transpose()?;
        Ok(Self { socket })
    }
    pub fn send(&self, state: &str) -> io::Result<()> {
        if let Some(socket) = &self.socket
            && socket
                .send(state.as_bytes())
                .map_err(|e| context("send systemd notification", e))?
                != state.len()
        {
            return Err(io::Error::other("short systemd notification"));
        }
        Ok(())
    }
}
fn notify_socket(name: &OsStr) -> io::Result<UnixDatagram> {
    let bytes = name.as_bytes();
    let address = match bytes.first() {
        Some(b'@') if bytes.len() > 1 => SocketAddr::from_abstract_name(&bytes[1..])?,
        Some(b'/') => SocketAddr::from_pathname(Path::new(name))?,
        _ => {
            return Err(io::Error::other(
                "NOTIFY_SOCKET must be absolute or @abstract",
            ));
        }
    };
    let socket = UnixDatagram::unbound()
        .map_err(|e| context("socket(AF_UNIX, SOCK_DGRAM) for NOTIFY_SOCKET", e))?;
    socket
        .set_write_timeout(Some(Duration::from_millis(250)))
        .map_err(|e| context("setsockopt(SO_SNDTIMEO) for NOTIFY_SOCKET", e))?;
    socket
        .connect_addr(&address)
        .map_err(|e| context("connect systemd NOTIFY_SOCKET", e))?;
    Ok(socket)
}
pub fn policy_heartbeat_interval() -> Duration {
    // The worker reports liveness over bounded IPC; only the broker notifies
    // systemd. WATCHDOG_PID therefore remains the broker's PID throughout.
    heartbeat_interval(env::var("WATCHDOG_USEC").ok().as_deref())
}
fn heartbeat_interval(usec: Option<&str>) -> Duration {
    watchdog_interval(usec)
        .unwrap_or(Duration::from_secs(5))
        .min(Duration::from_secs(5))
}
fn watchdog_interval(usec: Option<&str>) -> Option<Duration> {
    let usec = usec?.parse::<u64>().ok()?;
    (usec > 0).then(|| Duration::from_micros((usec / 2).max(1)))
}
/// Thread-local blocked signals are consumed by signalfd, never a signal handler.
pub struct Signals {
    fd: OwnedFd,
    old_mask: libc::sigset_t,
    // The saved pthread mask must be restored on the original thread.
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl Signals {
    pub fn new() -> io::Result<Self> {
        // SAFETY: sigset_t is an integer struct; pointers refer to live objects.
        let mut mask = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        let mut old_mask = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        unsafe {
            libc::sigemptyset(&mut mask);
            for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
                libc::sigaddset(&mut mask, signal);
            }
        }
        // SAFETY: blocks only the supplied signals on the calling thread.
        let result = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &mask, &mut old_mask) };
        if result != 0 {
            return Err(context(
                "pthread_sigmask(SIG_BLOCK)",
                io::Error::from_raw_os_error(result),
            ));
        }
        // SAFETY: mask is initialized; the new descriptor is ours to close.
        let fd = unsafe { libc::signalfd(-1, &mask, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            // SAFETY: undo our own successful mask change after construction failure.
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &old_mask, std::ptr::null_mut());
            }
            return Err(context("signalfd", error));
        }
        // SAFETY: signalfd returned this new, valid, uniquely owned descriptor.
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            old_mask,
            _thread: std::marker::PhantomData,
        })
    }
    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
    pub fn drain(&self) -> io::Result<Vec<i32>> {
        let mut signals = Vec::new();
        loop {
            // SAFETY: signalfd_siginfo is a C integer struct; read fills its bytes.
            let mut info = unsafe { std::mem::zeroed::<libc::signalfd_siginfo>() };
            let size = std::mem::size_of_val(&info);
            // SAFETY: info points to a writable buffer of the supplied size.
            let n = unsafe {
                libc::read(
                    self.fd(),
                    (&mut info as *mut libc::signalfd_siginfo).cast(),
                    size,
                )
            };
            if n < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(context("read signalfd", error));
            }
            if n as usize != size {
                return Err(io::Error::other("short signalfd read"));
            }
            signals.push(info.ssi_signo as i32);
        }
        Ok(signals)
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        // SAFETY: restores the exact mask captured on this same thread.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.old_mask, std::ptr::null_mut());
        }
    }
}
pub struct ConfigWatch {
    fd: OwnedFd,
    name: OsString,
}
impl ConfigWatch {
    pub fn new(path: &Path) -> io::Result<Self> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::other("config has no file name"))?
            .to_owned();
        let directory = CString::new(parent.as_os_str().as_bytes())?;
        // SAFETY: inotify_init1 returns an independent descriptor or -1.
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if raw < 0 {
            return Err(context("inotify_init1", io::Error::last_os_error()));
        }
        // SAFETY: raw is a new uniquely owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // Watch the parent directory so atomic file replacement retains the watch.
        let mask = libc::IN_CLOSE_WRITE
            | libc::IN_MOVED_TO
            | libc::IN_MOVED_FROM
            | libc::IN_DELETE
            | libc::IN_ATTRIB
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF
            | libc::IN_ONLYDIR;
        // SAFETY: directory is NUL terminated; mask contains valid inotify flags.
        if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), directory.as_ptr(), mask) } < 0 {
            return Err(context(
                "inotify_add_watch(config parent)",
                io::Error::last_os_error(),
            ));
        }
        Ok(Self { fd, name })
    }
    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
    pub fn drain(&self) -> io::Result<bool> {
        let mut changed = false;
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            // SAFETY: buffer has the given writable capacity; fd is nonblocking.
            let n = unsafe { libc::read(self.fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if n < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(context("read config inotify events", error));
            }
            if n == 0 {
                return Err(io::Error::other("config watch closed"));
            }
            changed |= config_events(&buffer[..n as usize], self.name.as_bytes())?;
        }
        Ok(changed)
    }
}
fn config_events(mut bytes: &[u8], name: &[u8]) -> io::Result<bool> {
    let mut changed = false;
    let header = std::mem::size_of::<libc::inotify_event>();
    while !bytes.is_empty() {
        if bytes.len() < header {
            return Err(io::Error::other("short inotify header"));
        }
        // SAFETY: slice contains the whole header; unaligned stack buffers are allowed.
        let event =
            unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<libc::inotify_event>()) };
        let size = header
            .checked_add(event.len as usize)
            .ok_or_else(|| io::Error::other("inotify event overflow"))?;
        if bytes.len() < size {
            return Err(io::Error::other("short inotify event"));
        }
        if event.mask & (libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0 {
            return Err(io::Error::other("config directory watch lost"));
        }
        if event.mask & libc::IN_Q_OVERFLOW != 0 {
            changed = true;
        }
        let data = &bytes[header..size];
        let end = data.iter().position(|b| *b == 0).unwrap_or(data.len());
        if &data[..end] == name {
            changed = true;
        }
        bytes = &bytes[size..];
    }
    Ok(changed)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn abstract_notify_socket_sends_exact_payload() {
        let name = format!("minibookx-ctl-test-{}", std::process::id());
        let address = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let receiver = UnixDatagram::bind_addr(&address).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let socket = notify_socket(OsStr::new(&format!("@{name}"))).unwrap();
        socket.send(b"READY=1\nWATCHDOG=1").unwrap();
        let mut bytes = [0; 64];
        let size = receiver.recv(&mut bytes).unwrap();
        assert_eq!(&bytes[..size], b"READY=1\nWATCHDOG=1");
        assert!(notify_socket(OsStr::new("relative.sock")).is_err());
    }
    #[test]
    fn policy_heartbeat_tracks_watchdog_without_exceeding_the_broker_timeout() {
        assert_eq!(
            watchdog_interval(Some("10000000")),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            heartbeat_interval(Some("3000000")),
            Duration::from_millis(1500)
        );
        for usec in [None, Some("0"), Some("garbage"), Some("60000000")] {
            assert_eq!(heartbeat_interval(usec), Duration::from_secs(5));
        }
    }
    #[test]
    fn pathname_notify_socket_sends_exact_payload() {
        let path =
            std::env::temp_dir().join(format!("minibookx-notify-{}.sock", std::process::id()));
        let receiver = UnixDatagram::bind(&path).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let socket = notify_socket(path.as_os_str()).unwrap();
        socket.send(b"STOPPING=1").unwrap();
        let mut bytes = [0; 64];
        let size = receiver.recv(&mut bytes).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(&bytes[..size], b"STOPPING=1");
    }
    #[test]
    fn blocked_signal_is_read_through_signalfd() {
        let signals = Signals::new().unwrap();
        // SAFETY: send SIGHUP only to this thread, where Signals has blocked it.
        assert_eq!(
            unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGHUP) },
            0
        );
        assert_eq!(signals.drain().unwrap(), [libc::SIGHUP]);
        assert!(signals.drain().unwrap().is_empty());
    }
    #[test]
    fn config_directory_watch_survives_atomic_replacement() {
        let directory =
            std::env::temp_dir().join(format!("minibookx-watch-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("config.conf");
        std::fs::write(&path, "one").unwrap();
        let watch = ConfigWatch::new(&path).unwrap();
        std::fs::write(directory.join("unrelated"), "ignored").unwrap();
        assert!(!watch.drain().unwrap());
        std::fs::write(directory.join("config.new"), "two").unwrap();
        std::fs::rename(directory.join("config.new"), &path).unwrap();
        assert!(watch.drain().unwrap());
        assert!(!watch.drain().unwrap());
        std::fs::write(&path, "three").unwrap();
        assert!(watch.drain().unwrap());
        std::fs::remove_dir_all(directory).unwrap();
        assert!(watch.drain().is_err());
    }
    #[test]
    fn sleep_events_require_exact_sender_path_and_type() {
        let mut message = Message::new_signal(MANAGER_PATH, MANAGER, "PrepareForSleep")
            .unwrap()
            .append1(true);
        message.set_sender(Some(":1.42".into()));
        assert_eq!(
            classify_sleep_signal(&message, ":1.42").unwrap(),
            Some(true)
        );
        assert_eq!(classify_sleep_signal(&message, ":1.43").unwrap(), None);
        let mut wrong = Message::new_signal("/wrong", MANAGER, "PrepareForSleep")
            .unwrap()
            .append1(true);
        wrong.set_sender(Some(":1.42".into()));
        assert_eq!(classify_sleep_signal(&wrong, ":1.42").unwrap(), None);
        let mut malformed = Message::new_signal(MANAGER_PATH, MANAGER, "PrepareForSleep")
            .unwrap()
            .append1("true");
        malformed.set_sender(Some(":1.42".into()));
        assert!(classify_sleep_signal(&malformed, ":1.42").is_err());
    }
}
