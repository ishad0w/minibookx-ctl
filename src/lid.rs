//! Read-only ACPI lid notifications. The broker opens this after forking.
//!
//! Events wake the broker; EVIOCGSW supplies the current value. Never replay
//! queued switch values over a newer snapshot, including across suspend.
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, RawFd},
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

const EV_SYN: u16 = 0;
const EV_SW: u16 = 5;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const SW_LID: u16 = 0;
const NAME: &str = "Lid Switch";
const PHYS: &str = "PNP0C0D/button/input0";

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn query<T>(file: &File, number: u32, value: &mut T) -> io::Result<()> {
    // SAFETY: EVIOCG* reads a fixed Linux input UAPI buffer of exactly T's size.
    // The descriptor is owned, and the initialized output remains live.
    if unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            libc::_IOR::<T>(b'E'.into(), number),
            value,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn text(file: &File, number: u32) -> io::Result<String> {
    let mut bytes = [0_u8; 128];
    query(file, number, &mut bytes)?;
    let end = bytes
        .iter()
        .position(|v| *v == 0)
        .ok_or_else(|| invalid("unterminated lid identity"))?;
    String::from_utf8(bytes[..end].to_vec()).map_err(|_| invalid("invalid lid identity"))
}

fn is_native_lid(
    id: &libc::input_id,
    name: &str,
    phys: &str,
    types: &[u8; 4],
    switches: &[u8; 4],
) -> bool {
    id.bustype == 0x19 // BUS_HOST, as supplied by the ACPI button driver
        && name == NAME
        && phys == PHYS
        && types[0] & (1 << EV_SW) != 0
        && switches[0] & (1 << SW_LID) != 0
}

fn identify(file: &File) -> io::Result<()> {
    let m = file.metadata()?;
    if !m.file_type().is_char_device() || libc::major(m.rdev()) != 13 {
        return Err(invalid("lid source is not an input character device"));
    }
    let mut id = libc::input_id {
        bustype: 0,
        vendor: 0,
        product: 0,
        version: 0,
    };
    let mut types = [0_u8; 4];
    let mut switches = [0_u8; 4];
    query(file, 0x02, &mut id)?; // EVIOCGID
    query(file, 0x20, &mut types)?; // EVIOCGBIT(0)
    query(file, 0x20 + u32::from(EV_SW), &mut switches)?;
    if !is_native_lid(
        &id,
        &text(file, 0x06)?, // EVIOCGNAME
        &text(file, 0x07)?, // EVIOCGPHYS
        &types,
        &switches,
    ) {
        return Err(invalid("unexpected ACPI lid identity or capabilities"));
    }
    Ok(())
}

trait Source {
    /// None means the nonblocking queue is empty, never EOF.
    fn event(&mut self) -> io::Result<Option<(u16, u16)>>;
    fn closed(&mut self) -> io::Result<bool>;
}

impl Source for File {
    fn event(&mut self) -> io::Result<Option<(u16, u16)>> {
        // SAFETY: zero is valid for every scalar in Linux's input_event.
        let mut event: libc::input_event = unsafe { std::mem::zeroed() };
        // SAFETY: event is a writable, correctly aligned input_event buffer.
        let n = unsafe {
            libc::read(
                self.as_raw_fd(),
                (&mut event as *mut libc::input_event).cast(),
                std::mem::size_of_val(&event),
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(e)
            };
        }
        if n as usize != std::mem::size_of_val(&event) {
            return Err(invalid("lid source disconnected or returned a short event"));
        }
        Ok(Some((event.type_, event.code)))
    }

    fn closed(&mut self) -> io::Result<bool> {
        let mut switches = [0_u8; 4];
        query(self, 0x1b, &mut switches)?; // EVIOCGSW
        Ok(switches[0] & (1 << SW_LID) != 0)
    }
}

#[derive(Default)]
struct State {
    closed: Option<bool>,
    dropped: bool,
}

impl State {
    fn sync(&mut self, source: &mut impl Source, force: bool) -> io::Result<()> {
        let result = self.read(source, force);
        if result.is_err() {
            self.closed = None;
        }
        result
    }

    fn read(&mut self, source: &mut impl Source, force: bool) -> io::Result<()> {
        let mut changed = force;
        // Bounded even if a broken device supplies an endless event stream.
        for _ in 0..256 {
            let event = match source.event() {
                // Signals must not fault a healthy input device. Retries share
                // the event bound so repeated interruptions cannot busy-loop.
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            let Some((kind, code)) = event else {
                if changed && !self.dropped {
                    self.closed = Some(source.closed()?);
                }
                return Ok(());
            };
            changed = true;
            if kind == EV_SYN && code == SYN_DROPPED {
                self.dropped = true;
                self.closed = None;
            } else if self.dropped && kind == EV_SYN && code == SYN_REPORT {
                // Ignore through SYN_REPORT inclusively, then obtain current
                // state after draining. Later queued values are never applied.
                self.dropped = false;
            }
        }
        Err(invalid("lid event queue did not drain within its bound"))
    }
}

pub struct Lid {
    file: File,
    state: State,
}

fn candidate(paths: &[PathBuf]) -> io::Result<&Path> {
    match paths {
        [] => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "native ACPI Lid Switch is missing",
        )),
        [path] => Ok(path),
        _ => Err(invalid("multiple native ACPI Lid Switch devices")),
    }
}

/// Unidentified hotplug entries may vanish while sysfs is being scanned. Once
/// an entry names the lid, failures stay visible rather than hiding that source.
fn discover_paths(input: &Path) -> io::Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(input)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name
            .strip_prefix("event")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|v| v.is_ascii_digit()))
        {
            continue;
        }
        let device = entry.path().join("device");
        let identity = match fs::read_to_string(device.join("name")) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if identity.trim() == NAME && fs::read_to_string(device.join("phys"))?.trim() == PHYS {
            candidates.push(Path::new("/dev/input").join(name));
        }
    }
    Ok(candidates)
}

impl Lid {
    pub fn discover() -> io::Result<Self> {
        let candidates = discover_paths(Path::new("/sys/class/input"))?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(candidate(&candidates)?)?;
        identify(&file)?;
        let mut lid = Self {
            file,
            state: State::default(),
        };
        lid.resync()?;
        Ok(lid)
    }

    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    pub fn closed(&self) -> Option<bool> {
        self.state.closed
    }

    pub fn drain(&mut self, revents: libc::c_short) -> io::Result<()> {
        if revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            self.state.closed = None;
            return Err(invalid("lid source disconnected or failed"));
        }
        self.state.sync(&mut self.file, false)
    }

    /// Called after resume, before accepting another posture sample.
    pub fn resync(&mut self) -> io::Result<()> {
        self.state.closed = None;
        self.state.sync(&mut self.file, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, os::unix::fs::symlink};

    struct InputTree(PathBuf);
    impl InputTree {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "minibookx-lid-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn device(&self, event: &str, name: &str) -> PathBuf {
            let device = self.0.join(event).join("device");
            fs::create_dir_all(&device).unwrap();
            fs::write(device.join("name"), name).unwrap();
            device
        }
    }
    impl Drop for InputTree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[derive(Default)]
    struct Fake {
        events: VecDeque<io::Result<Option<(u16, u16)>>>,
        closed: bool,
        queries: usize,
        fail_query: bool,
    }
    impl Source for Fake {
        fn event(&mut self) -> io::Result<Option<(u16, u16)>> {
            self.events.pop_front().unwrap_or(Ok(None))
        }
        fn closed(&mut self) -> io::Result<bool> {
            self.queries += 1;
            if self.fail_query {
                Err(io::Error::other("disconnected"))
            } else {
                Ok(self.closed)
            }
        }
    }
    impl Fake {
        fn queue(&mut self, events: &[(u16, u16)]) {
            self.events
                .extend(events.iter().copied().map(|v| Ok(Some(v))));
        }
    }

    #[test]
    fn initialization_and_old_queued_values_use_current_state() {
        let mut state = State::default();
        let mut source = Fake {
            closed: true,
            ..Fake::default()
        };
        source.queue(&[(EV_SW, SW_LID), (EV_SYN, SYN_REPORT)]);
        state.sync(&mut source, true).unwrap();
        assert_eq!(state.closed, Some(true));
        assert_eq!(source.queries, 1);
        // A notification queued before the preceding ioctl cannot overwrite it.
        source.queue(&[(EV_SW, SW_LID), (EV_SYN, SYN_REPORT)]);
        state.sync(&mut source, false).unwrap();
        assert_eq!(state.closed, Some(true));
        state.sync(&mut source, false).unwrap();
        assert_eq!(source.queries, 2); // No polling query when queue is empty.
        source.closed = false;
        source.queue(&[(EV_SW, SW_LID), (EV_SYN, SYN_REPORT)]);
        state.sync(&mut source, false).unwrap();
        assert_eq!(state.closed, Some(false));
        assert_eq!(source.queries, 3);
    }

    #[test]
    fn overflow_is_unknown_until_report_then_resynchronizes() {
        let mut state = State {
            closed: Some(false),
            dropped: false,
        };
        let mut source = Fake {
            closed: true,
            ..Fake::default()
        };
        source.queue(&[(EV_SYN, SYN_DROPPED), (EV_SW, SW_LID)]);
        state.sync(&mut source, false).unwrap();
        assert_eq!(state.closed, None);
        assert_eq!(source.queries, 0);
        source.queue(&[(EV_SW, SW_LID), (EV_SYN, SYN_REPORT), (EV_SW, SW_LID)]);
        state.sync(&mut source, false).unwrap();
        assert_eq!(state.closed, Some(true));
        assert_eq!(source.queries, 1);
    }

    #[test]
    fn resume_discards_old_events_and_queries_even_without_a_transition() {
        let mut state = State {
            closed: Some(true),
            dropped: false,
        };
        let mut source = Fake::default();
        source.queue(&[(EV_SW, SW_LID), (EV_SYN, SYN_REPORT)]);
        state.sync(&mut source, true).unwrap();
        assert_eq!(state.closed, Some(false));
        assert_eq!(source.queries, 1);
    }

    #[test]
    fn resume_during_overflow_stays_unknown_until_a_report() {
        let mut state = State {
            closed: None,
            dropped: true,
        };
        let mut source = Fake::default();
        state.sync(&mut source, true).unwrap();
        assert_eq!(state.closed, None);
        assert_eq!(source.queries, 0);
        source.queue(&[(EV_SW, SW_LID)]);
        state.sync(&mut source, false).unwrap();
        assert_eq!(state.closed, None);
        assert_eq!(source.queries, 0);
        source.queue(&[(EV_SYN, SYN_REPORT)]);
        state.sync(&mut source, false).unwrap();
        assert_eq!(state.closed, Some(false));
        assert_eq!(source.queries, 1);
    }

    #[test]
    fn interrupted_reads_retry_within_the_existing_bound() {
        let mut state = State::default();
        let mut source = Fake::default();
        source
            .events
            .push_back(Err(io::ErrorKind::Interrupted.into()));
        state.sync(&mut source, true).unwrap();
        assert_eq!(state.closed, Some(false));
        assert_eq!(source.queries, 1);
        source
            .events
            .extend((0..256).map(|_| Err(io::Error::from(io::ErrorKind::Interrupted))));
        assert!(state.sync(&mut source, true).is_err());
        assert_eq!(state.closed, None);
        assert_eq!(source.queries, 1);
    }

    #[test]
    fn failures_do_not_leave_a_known_open_state() {
        let mut state = State {
            closed: Some(false),
            dropped: false,
        };
        let mut source = Fake {
            fail_query: true,
            ..Fake::default()
        };
        assert!(state.sync(&mut source, true).is_err());
        assert_eq!(state.closed, None);
        source.events.push_back(Err(io::Error::other("HUP")));
        state.closed = Some(false);
        assert!(state.sync(&mut source, false).is_err());
        assert_eq!(state.closed, None);
    }

    #[test]
    fn poll_disconnects_invalidate_the_cached_value_before_reading() {
        for flag in [libc::POLLHUP, libc::POLLERR, libc::POLLNVAL] {
            let mut lid = Lid {
                // No reads/ioctls occur on this descriptor in the error path.
                file: File::open("/dev/null").unwrap(),
                state: State {
                    closed: Some(false),
                    dropped: false,
                },
            };
            assert!(lid.drain(flag | libc::POLLIN).is_err());
            assert_eq!(lid.closed(), None);
        }
    }

    #[test]
    fn an_endless_queue_is_bounded_and_invalidates_state() {
        let mut state = State {
            closed: Some(false),
            dropped: false,
        };
        let mut source = Fake::default();
        source.queue(&[(EV_SW, SW_LID); 256]);
        assert!(state.sync(&mut source, false).is_err());
        assert_eq!(state.closed, None);
        assert_eq!(source.queries, 0);
    }

    #[test]
    fn ioctl_layout_matches_linux_input_uapi() {
        // libc models ioctl's request as unsigned long on GNU and int on
        // musl. Compare the UAPI's 32-bit pattern, independent of signedness.
        assert_eq!(
            libc::_IOR::<libc::input_id>(b'E'.into(), 0x02) as u32,
            0x80084502
        );
        assert_eq!(libc::_IOR::<[u8; 4]>(b'E'.into(), 0x1b) as u32, 0x8004451b);
        assert_eq!(libc::_IOR::<[u8; 4]>(b'E'.into(), 0x25) as u32, 0x80044525);
    }

    #[test]
    fn discovery_requires_one_identified_candidate_not_an_event_number() {
        assert_eq!(candidate(&[]).unwrap_err().kind(), io::ErrorKind::NotFound);
        let paths = [PathBuf::from("/dev/input/event72")];
        assert_eq!(candidate(&paths).unwrap(), paths[0]);
        assert!(candidate(&[paths[0].clone(), PathBuf::from("/dev/input/event3")]).is_err());
    }

    #[test]
    fn vanished_unidentified_devices_do_not_hide_a_lid_or_change_missing_to_open() {
        let tree = InputTree::new();
        let removed = tree.device("event0", "USB keyboard");
        fs::remove_dir_all(removed).unwrap();
        // sysfs entries can also remain as dangling links after removal.
        symlink(tree.0.join("removed"), tree.0.join("event8")).unwrap();
        tree.device("event2", "USB mouse"); // No phys read for unrelated input.
        let paths = discover_paths(&tree.0).unwrap();
        assert_eq!(
            candidate(&paths).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let lid = tree.device("event72", NAME);
        fs::write(lid.join("phys"), PHYS).unwrap();
        assert_eq!(
            discover_paths(&tree.0).unwrap(),
            [PathBuf::from("/dev/input/event72")]
        );
    }

    #[test]
    fn identified_lid_failures_and_unreadable_identity_remain_errors() {
        let tree = InputTree::new();
        let lid = tree.device("event72", NAME);
        assert_eq!(
            discover_paths(&tree.0).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        fs::write(lid.join("phys"), PHYS).unwrap();
        let other = tree.device("event3", "USB keyboard");
        fs::write(other.join("name"), [0xff]).unwrap();
        assert_eq!(
            discover_paths(&tree.0).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::write(other.join("name"), NAME).unwrap();
        fs::write(other.join("phys"), PHYS).unwrap();
        assert!(candidate(&discover_paths(&tree.0).unwrap()).is_err());
    }

    #[test]
    fn identity_and_lid_capabilities_are_both_required() {
        let mut id = libc::input_id {
            bustype: 0x19,
            vendor: 0,
            product: 5,
            version: 0,
        };
        let types = [1 << EV_SW, 0, 0, 0];
        let switches = [1 << SW_LID, 0, 0, 0];
        assert!(is_native_lid(&id, NAME, PHYS, &types, &switches));
        assert!(!is_native_lid(&id, "Other switch", PHYS, &types, &switches));
        assert!(!is_native_lid(&id, NAME, "other/input0", &types, &switches));
        assert!(!is_native_lid(&id, NAME, PHYS, &[0; 4], &switches));
        assert!(!is_native_lid(&id, NAME, PHYS, &types, &[0; 4]));
        id.bustype = 0x06; // A virtual switch is not the native ACPI lid.
        assert!(!is_native_lid(&id, NAME, PHYS, &types, &switches));
    }
}
