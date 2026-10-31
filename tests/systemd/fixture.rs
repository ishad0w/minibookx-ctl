//! Development-only VM executable. Production control flow, fixed fake hardware.
//! EC, temperatures, posture inputs and switch events use files or constants.
//! The VM keeps optional RAPL writes disabled; no hardware is passed through.
#![allow(dead_code)]
pub use minibookx_ctl::{ipc, paths, power, status};
use std::{fs, io, path::Path};

#[path = "../../src/broker.rs"]
mod broker;
#[path = "../../src/policy.rs"]
mod policy;
#[path = "../../src/presets.rs"]
mod presets;
#[path = "../../src/runtime.rs"]
mod runtime;
#[path = "../../src/service.rs"]
mod service;
#[path = "../../src/systemd.rs"]
mod systemd;

const ROOT: &str = "/run/minibookx-ctl";

fn count(operation: &str) -> io::Result<()> {
    use std::io::Write;
    // The broker has dropped DAC capabilities and cannot write in the worker's
    // directory. Each process records in its own writable ownership domain.
    let directory = if unsafe { libc::geteuid() } == 0 {
        ROOT.to_owned()
    } else {
        format!("{ROOT}/policy")
    };
    writeln!(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{directory}/reads-{}", std::process::id()))?,
        "{operation}"
    )
}

// Pollable stand-in only: production lid.rs tests cover the evdev protocol.
mod lid {
    use super::{ROOT, fs, io};
    use std::os::{
        fd::{AsRawFd, RawFd},
        unix::net::UnixDatagram,
    };

    pub struct Lid {
        socket: UnixDatagram,
        closed: Option<bool>,
    }
    impl Lid {
        pub fn discover() -> io::Result<Self> {
            let path = format!("{ROOT}/lid-events");
            match fs::remove_file(&path) {
                Ok(()) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
            let socket = UnixDatagram::bind(path)?;
            socket.set_nonblocking(true)?;
            let mut lid = Self {
                socket,
                closed: None,
            };
            lid.resync()?;
            Ok(lid)
        }
        pub fn fd(&self) -> RawFd {
            self.socket.as_raw_fd()
        }
        pub fn closed(&self) -> Option<bool> {
            self.closed
        }
        pub fn drain(&mut self, revents: libc::c_short) -> io::Result<()> {
            if revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                self.closed = None;
                return Err(io::Error::other("fake lid disconnected"));
            }
            let mut byte = [0];
            loop {
                match self.socket.recv(&mut byte) {
                    Ok(_) => (),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e),
                }
            }
            self.resync()
        }
        pub fn resync(&mut self) -> io::Result<()> {
            self.closed = None;
            self.closed = Some(match fs::read_to_string(format!("{ROOT}/lid-state")) {
                Ok(value) if value.trim() == "closed" => true,
                Ok(value) if value.trim() == "open" => false,
                Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                _ => return Err(io::Error::other("invalid fake lid state")),
            });
            Ok(())
        }
    }
}

mod ec {
    pub use minibookx_ctl::ec::*;
    use std::{fs::File, io, os::unix::fs::FileExt};
    pub struct FileEc(pub File);
    impl EcIo for FileEc {
        fn read(&self, register: u8) -> io::Result<u8> {
            let mut value = [0];
            if self.0.read_at(&mut value, u64::from(register))? != 1 {
                return Err(io::Error::other("short fake EC read"));
            }
            Ok(value[0])
        }
        fn write(&mut self, register: u8, value: u8) -> io::Result<()> {
            if register == 0x4f
                && value & MANUAL == 0
                && std::path::Path::new("/run/minibookx-ctl/fail-restore").exists()
            {
                return Err(io::Error::other("injected restoration failure"));
            }
            if self.0.write_at(&[value], u64::from(register))? != 1 {
                return Err(io::Error::other("short fake EC write"));
            }
            Ok(())
        }
    }
}

mod hardware {
    use super::{Path, ROOT, ec, fs, io};
    use std::{
        fs::OpenOptions,
        os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        path::PathBuf,
    };
    fn ec_path() -> io::Result<PathBuf> {
        let source = Path::new("/sys/kernel/debug");
        let alias = Path::new("/run/minibookx-ctl/hardware/debugfs");
        let a = fs::metadata(alias)?;
        let s = fs::metadata(source)?;
        Ok(if a.dev() == s.dev() && a.ino() == s.ino() {
            alias
        } else {
            source
        }
        .join("ec/ec0/io"))
    }
    #[derive(Clone)]
    pub struct Control;
    impl Control {
        pub fn open() -> io::Result<Self> {
            check_profile()?;
            Ok(Self)
        }
    }
    pub fn check_profile() -> io::Result<()> {
        if fs::read_to_string("/sys/class/dmi/id/product_name")?.trim()
            != "MiniBookX Systemd Fixture"
        {
            return Err(io::Error::other("fixture requires the isolated QEMU guest"));
        }
        Ok(())
    }
    fn open() -> io::Result<ec::FileEc> {
        if Path::new(&format!("{ROOT}/forbid-ec")).exists() {
            return Err(io::Error::other("test forbids EC access"));
        }
        Ok(ec::FileEc(
            OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(ec_path()?)?,
        ))
    }
    pub fn prepare() -> io::Result<()> {
        check_profile()?;
        fs::set_permissions(
            format!("{ROOT}/hardware"),
            fs::Permissions::from_mode(0o700),
        )?;
        recover()
    }
    pub fn recover() -> io::Result<()> {
        check_profile()?;
        eprintln!("VM_RECOVER invoked");
        let fan = match fs::read_to_string(format!("{ROOT}/module.lease")) {
            Ok(value) => match value.trim() {
                "fan=true" => true,
                "fan=false" => false,
                _ => return Err(io::Error::other("invalid fake module lease")),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        if fan && Path::new(&format!("{ROOT}/manual.owner")).exists() {
            ec::restore_auto(&mut open()?)?;
        } else if !fan && Path::new(&format!("{ROOT}/manual.owner")).exists() {
            return Err(io::Error::other("unowned fake manual marker"));
        }
        for name in ["manual.owner", "module.lease"] {
            match fs::remove_file(format!("{ROOT}/{name}")) {
                Ok(()) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
        }
        eprintln!("VM_RECOVER completed");
        Ok(())
    }
    pub struct Session {
        pub fan: Option<ec::Fan<ec::FileEc>>,
    }
    impl Session {
        pub fn open_locked(fan_control: bool, _: Control) -> io::Result<Self> {
            Self::open(fan_control)
        }
        pub fn open(fan_control: bool) -> io::Result<Self> {
            fs::write(
                format!("{ROOT}/module.lease"),
                if fan_control {
                    "fan=true\n"
                } else {
                    "fan=false\n"
                },
            )?;
            let fan = if fan_control {
                if Path::new(&format!("{ROOT}/forbid-ec")).exists() {
                    return Err(io::Error::other("test forbids EC setup"));
                }
                let ec_path = ec_path()?;
                if !ec_path.exists() {
                    fs::create_dir_all(ec_path.parent().unwrap())?;
                    let mut bytes = [0; 256];
                    bytes[0x4a] = 91;
                    bytes[0x70] = 60;
                    bytes[0x76] = 9;
                    bytes[0x77] = 196;
                    fs::write(ec_path, bytes)?;
                }
                let fan = ec::Fan::new(open()?);
                fan.baseline()?;
                Some(fan)
            } else {
                None
            };
            Ok(Self { fan })
        }
        pub fn activate(&mut self) -> io::Result<()> {
            let Some(fan) = &mut self.fan else {
                return Ok(());
            };
            fs::write(format!("{ROOT}/manual.owner"), "fixture broker\n")?;
            fan.activate()
        }
        pub fn restore(&mut self) -> io::Result<()> {
            if let Some(fan) = &mut self.fan {
                fan.restore()?;
            }
            match fs::remove_file(format!("{ROOT}/manual.owner")) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        }
    }
}

mod sensor {
    use std::{io, path::Path};
    pub struct CpuSensor;
    impl CpuSensor {
        pub fn discover(_: &Path) -> io::Result<Self> {
            if Path::new("/run/minibookx-ctl/forbid-ec").exists() {
                return Err(io::Error::other("test forbids coretemp discovery"));
            }
            Ok(Self)
        }
        pub fn read_mc(&self) -> io::Result<i32> {
            super::count("cpu")?;
            match std::fs::read_to_string("/run/minibookx-ctl/temperature") {
                Ok(value) => value.trim().parse().map_err(io::Error::other),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(60_000),
                Err(e) => Err(e),
            }
        }
    }
}

mod tablet {
    pub use minibookx_ctl::tablet::{
        Estimate, EstimateReason, EstimatorConfig, HingeEstimator, SAMPLE_INTERVAL, Sample,
    };
    use std::{
        fs::{File, OpenOptions},
        io::{self, Write},
        os::unix::fs::OpenOptionsExt,
    };
    pub struct Sensors;
    impl Sensors {
        pub fn discover(_: &super::hardware::Control) -> io::Result<Self> {
            Ok(Self)
        }
        pub fn restore(&mut self) -> io::Result<()> {
            super::count("sensor-restore")
        }
        pub fn retain_for_recovery(&mut self) {}
        pub fn sample(&self) -> io::Result<Sample> {
            super::count("posture")?;
            if std::path::Path::new("/run/minibookx-ctl/fail-posture").exists() {
                return Err(io::Error::other("injected posture read failure"));
            }
            if std::path::Path::new("/run/minibookx-ctl/posture-rotated").exists() {
                // Same hinge angle after rotating both gravity vectors.
                return Ok(Sample {
                    primary: [0.0, 0.0, -1000.0],
                    secondary: [-1000.0, 0.0, 0.0],
                });
            }
            // Synthetic 270-degree hinge, fixed numerical input only.
            Ok(Sample {
                primary: [1000.0, 0.0, 0.0],
                secondary: [0.0, 0.0, -1000.0],
            })
        }
    }
    pub struct VirtualSwitch(File);
    impl VirtualSwitch {
        pub fn create() -> io::Result<Self> {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o644)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open("/run/minibookx-ctl/switch-events")?;
            Ok(Self(file))
        }
        pub fn send(&mut self, tablet: bool) -> io::Result<()> {
            writeln!(self.0, "{}", u8::from(tablet))?;
            self.0.flush()
        }
    }
}

fn run() -> io::Result<()> {
    hardware::check_profile()?;
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::other("fixture requires guest root"));
    }
    match std::env::args().nth(1).as_deref() {
        Some("--prepare") => hardware::prepare(),
        Some("--recover") => hardware::recover(),
        Some("--daemon") => broker::run(
            "/etc/minibookx-ctl/config.conf".into(),
            !Path::new(&format!("{ROOT}/no-tablet")).exists(),
        ),
        _ => Err(io::Error::other("fixture lifecycle command required")),
    }
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("VM_FIXTURE_ERROR {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
