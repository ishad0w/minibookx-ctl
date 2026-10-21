//! Unprivileged policy-facing API. Raw hardware descriptors never enter this process.
pub use crate::paths::RUN_DIR;
use crate::{
    ipc::{REPLY_BYTES, Reply, Request, TachState},
    power::PowerConfig,
};
use std::{
    cell::RefCell,
    fs, io,
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        net::UnixDatagram,
    },
    path::Path,
    time::Duration,
};
thread_local! {static TRANSPORT:RefCell<Option<UnixDatagram>>=const {RefCell::new(None)};}

pub(crate) fn install_transport(socket: UnixDatagram) {
    TRANSPORT.with(|v| *v.borrow_mut() = Some(socket));
}
struct Client {
    socket: UnixDatagram,
    sequence: u32,
}
impl Client {
    fn call(&mut self, request: Request) -> io::Result<Reply> {
        self.sequence = self.sequence.wrapping_add(1);
        let b = request.encode(self.sequence);
        if self.socket.send(&b)? != b.len() {
            return Err(io::Error::other("short broker request"));
        }
        let mut reply = [0; REPLY_BYTES + 1];
        let n = self.socket.recv(&mut reply)?;
        Reply::decode(&reply[..n], self.sequence)
    }
}
pub struct ProxyFan {
    client: Client,
    manual: bool,
}
pub struct AppliedFan {
    pub raw: u8,
    pub emergency: bool,
    pub changed: bool,
    pub ec_temperature: Option<u8>,
    pub tach: Option<u16>,
    pub tach_state: TachState,
}
impl ProxyFan {
    pub fn apply(&mut self, raw: u8) -> io::Result<AppliedFan> {
        let reply = self.client.call(Request::Duty(raw))?;
        Ok(AppliedFan {
            raw: reply
                .actual_raw
                .ok_or_else(|| io::Error::other("broker omitted applied fan duty"))?,
            emergency: reply.emergency,
            changed: reply.changed,
            ec_temperature: reply.temperature,
            tach: reply.tach,
            tach_state: reply.tach_state,
        })
    }
    pub fn restore(&mut self) -> io::Result<()> {
        self.client.call(Request::Restore)?;
        self.manual = false;
        Ok(())
    }
}
impl Drop for ProxyFan {
    fn drop(&mut self) {
        if self.manual {
            let _ = self.restore();
        }
    }
}
pub struct Session {
    pub fan: ProxyFan,
    fan_control: bool,
}
impl Session {
    pub fn open(fan_control: bool) -> io::Result<Self> {
        // Only the root parent supplies this inherited private socket.
        let socket = TRANSPORT.with(|v| v.borrow_mut().take()).ok_or_else(|| {
            io::Error::other("no inherited hardware broker; use the system service")
        })?;
        socket.set_read_timeout(Some(Duration::from_secs(10)))?;
        socket.set_write_timeout(Some(Duration::from_secs(3)))?;
        let mut session = Self {
            fan: ProxyFan {
                client: Client {
                    socket,
                    sequence: 0,
                },
                manual: false,
            },
            fan_control,
        };
        session
            .fan
            .client
            .call(Request::Initialize { fan_control })?;
        session
            .fan
            .client
            .socket
            .set_read_timeout(Some(Duration::from_secs(3)))?;
        Ok(session)
    }
    pub fn activate(&mut self) -> io::Result<()> {
        self.fan.client.call(Request::Activate)?;
        self.fan.manual = self.fan_control;
        Ok(())
    }
    pub fn heartbeat(&mut self) -> io::Result<()> {
        self.fan.client.call(Request::Heartbeat).map(|_| ())
    }
    pub fn apply_power(&mut self, config: &PowerConfig) -> io::Result<()> {
        config.validate().map_err(io::Error::other)?;
        self.fan
            .client
            .call(Request::Power(config.clone()))
            .map(|_| ())
    }
}
pub fn write_status(text: &str) -> io::Result<()> {
    let dir = Path::new(RUN_DIR).join("policy");
    let m = fs::symlink_metadata(&dir)?;
    // SAFETY: geteuid takes no arguments.
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
        return Err(io::Error::other("untrusted policy runtime directory"));
    }
    let p = dir.join("status.tmp");
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&p)?;
    f.write_all(text.as_bytes())?;
    f.set_permissions(fs::Permissions::from_mode(0o644))?;
    fs::rename(p, dir.join("status.json"))
}
pub fn print_status() -> io::Result<()> {
    println!("{}", crate::status::current(Path::new(RUN_DIR))?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_reply_still_reports_a_broker_override_to_the_worker() {
        let (worker, broker) = UnixDatagram::pair().unwrap();
        worker
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        broker
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let server = std::thread::spawn(move || {
            let mut bytes = [0; crate::ipc::REQUEST_BYTES + 1];
            let count = broker.recv(&mut bytes).unwrap();
            let (request, sequence) = Request::decode(&bytes[..count]).unwrap();
            assert_eq!(request, Request::Duty(0));
            broker
                .send(
                    &Reply {
                        sequence,
                        changed: false,
                        actual_raw: Some(crate::ec::MAX_RAW),
                        emergency: true,
                        temperature: Some(44),
                        tach: Some(0),
                        tach_state: TachState::Recent,
                        ..Reply::default()
                    }
                    .encode(),
                )
                .unwrap();
        });
        let mut fan = ProxyFan {
            client: Client {
                socket: worker,
                sequence: 0,
            },
            manual: false,
        };
        let applied = fan.apply(0).unwrap();
        assert!(!applied.changed);
        assert_eq!(applied.raw, crate::ec::MAX_RAW);
        assert!(applied.emergency);
        assert_eq!(applied.ec_temperature, Some(44));
        assert_eq!(applied.tach, Some(0));
        // A current low counter does not override the broker's recent-sample
        // grace policy in this unprivileged client.
        assert_eq!(applied.tach_state, TachState::Recent);
        server.join().unwrap();
    }
}
