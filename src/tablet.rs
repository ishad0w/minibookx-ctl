//! Tablet posture support for the inspected MiniBook X U300 revision only.
//!
//! The privileged broker owns the sensors and estimator; its independent
//! guardian drives the virtual switch; the broker retains its recovery FD. No raw sensor/device descriptor crosses
//! into the policy worker. Sensor access never unbinds a driver, sets an
//! I2C address globally. The only functional sensor write is an attributed
//! base-sensor PD-bit wake, restored before sleep, stop or independent recovery.
//!
//! A virtual EV_SW/SW_TABLET_MODE switch is the normal interface: libinput
//! suspends the internal keyboard/touchpad/pointing stick, and Mutter changes
//! touch-mode so GNOME Shell can offer its keyboard after a touchscreen tap.
//! Do not depend on destruction of the switch to restore the keyboard:
//! libinput's fallback device-removed handler does not resume it. The broker
//! must keep the switch alive and send OFF when policy dies or becomes stale.

mod power;

use crate::{hardware::Control, platform};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::Duration,
};

pub use crate::platform::KNOWN_BIOS;
pub const SWITCH_NAME: &str = "MiniBook X U300 tablet mode";
/// Expected broker cadence. Scheduling tolerance is a separate estimator bound.
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
/// A posture that has stayed settled this long is sampled at the slow rate.
const SETTLE_BEFORE_SLOW: Duration = Duration::from_secs(2);
const STABLE_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
/// A settled angle that moves this far returns to the fast rate.
const ACTIVITY_DEG: f64 = 10.0;
const SENSOR_ADDRESS: u16 = 0x15;
const SENSOR_ID: u8 = 0x02;
const I2C_RDWR: libc::c_ulong = 0x0707;
const I2C_M_RD: u16 = 0x0001;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// An additional independent gate before any raw hardware descriptor is opened.
/// This is intentionally a revision profile, not support for every MiniBook X.
pub fn check_hardware_profile() -> io::Result<()> {
    platform::check_profile()
}

fn acpi_is(path: &Path, controller: &str) -> bool {
    let Ok(value) = fs::read_to_string(path.join("firmware_node/path")) else {
        return false;
    };
    // ACPICA may print the padded root-segment name as either _SB or _SB_.
    value.trim() == format!("\\_SB_.PC00.{controller}")
        || value.trim() == format!("\\_SB.PC00.{controller}")
}

fn adapter_number(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("i2c-")?;
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn unique<T>(mut values: Vec<T>, message: &str) -> io::Result<T> {
    if values.len() != 1 {
        return Err(invalid(message));
    }
    Ok(values.remove(0))
}

fn open_read_attribute(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
}

fn read_number(file: &File) -> io::Result<f64> {
    let mut buffer = [0_u8; 64];
    let n = file.read_at(&mut buffer, 0)?;
    if n == 0 || n == buffer.len() {
        return Err(invalid("empty or oversized sensor attribute"));
    }
    std::str::from_utf8(&buffer[..n])
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .ok_or_else(|| invalid("non-numeric sensor attribute"))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// Lid accelerometer: signed 12-bit IIO raw counts, kernel mount matrix
    /// verified to be identity on the inspected revision.
    pub primary: [f64; 3],
    /// Base accelerometer: signed 12-bit counts read from registers 03..08.
    pub secondary: [f64; 3],
}

#[repr(C)]
struct I2cMessage {
    address: u16,
    flags: u16,
    length: u16,
    buffer: *mut u8,
}

#[repr(C)]
struct I2cTransfer {
    messages: *mut I2cMessage,
    count: u32,
}

pub struct Sensors {
    primary: [File; 3],
    secondary: Secondary,
    power: power::Ownership,
    _control: Control,
}

struct Secondary {
    file: File,
    adapter: PathBuf,
    adapter_number: u32,
}

impl Sensors {
    pub fn discover(control: &Control) -> io::Result<Self> {
        check_hardware_profile()?;
        let mut primaries = Vec::new();
        for entry in fs::read_dir("/sys/bus/iio/devices")? {
            let path = entry?.path();
            if !path
                .file_name()
                .and_then(|v| v.to_str())
                .is_some_and(|v| v.starts_with("iio:device"))
                || fs::read_to_string(path.join("name"))
                    .ok()
                    .is_none_or(|v| v.trim() != "mxc4005")
            {
                continue;
            }
            let actual = fs::canonicalize(&path)?;
            if !actual.components().any(|v| {
                v.as_os_str()
                    .to_str()
                    .is_some_and(|v| v.starts_with("i2c-MDA6655:"))
            }) || !actual.ancestors().any(|v| acpi_is(v, "I2C1"))
            {
                continue;
            }
            let matrix = fs::read_to_string(actual.join("in_accel_mount_matrix"))?;
            let numbers: Vec<_> = matrix
                .split([',', ';'])
                .map(str::trim)
                .map(str::parse::<i32>)
                .collect::<Result<_, _>>()
                .map_err(|_| invalid("invalid primary accelerometer mount matrix"))?;
            if numbers != [1, 0, 0, 0, 1, 0, 0, 0, 1] {
                return Err(invalid("unvalidated primary accelerometer mount matrix"));
            }
            primaries.push(actual);
        }
        let primary_iio = unique(
            primaries,
            "need exactly one MDA6655 IIO sensor on ACPI I2C1",
        )?;

        let secondary = Secondary::discover()?;
        let primary = [
            open_read_attribute(&primary_iio.join("in_accel_x_raw"))?,
            open_read_attribute(&primary_iio.join("in_accel_y_raw"))?,
            open_read_attribute(&primary_iio.join("in_accel_z_raw"))?,
        ];
        let mut sensor = Self {
            primary,
            secondary,
            power: power::Ownership::new(None),
            _control: control.clone(),
        };
        sensor.power = power::Ownership::new(power::Lease::acquire(
            &sensor.secondary,
            control.directory(),
            control.invocation(),
            0,
            std::thread::sleep,
        )?);
        // Discard the first complete post-settling sample. The broker starts
        // with a fresh estimator, so warm-up cannot establish a posture.
        sensor.sample()?;
        Ok(sensor)
    }

    pub fn restore(&mut self) -> io::Result<()> {
        self.power.restore(&self.secondary)
    }

    /// A guardian that could not confirm OFF must be gone before standalone
    /// recovery powers the sensor down. Leave its durable attribution intact.
    pub fn retain_for_recovery(&mut self) {
        self.power.retain();
    }

    /// One attempt only. I2C latency is bounded by the kernel adapter timeout;
    /// the broker must retain its watchdog and release input on failure.
    pub fn sample(&self) -> io::Result<Sample> {
        use power::SensorIo;
        if self.secondary.control()? != 0 {
            return Err(invalid("secondary sensor left the owned active state"));
        }
        let primary = [
            read_number(&self.primary[0])?,
            read_number(&self.primary[1])?,
            read_number(&self.primary[2])?,
        ];
        let bytes = self.secondary.read_register::<6>(0x03)?;
        let secondary = std::array::from_fn(|i| {
            f64::from(i16::from_be_bytes([bytes[i * 2], bytes[i * 2 + 1]]) >> 4)
        });
        Ok(Sample { primary, secondary })
    }
}

impl Drop for Sensors {
    fn drop(&mut self) {
        if self.power.restore_on_drop()
            && let Err(error) = self.restore()
        {
            eprintln!("secondary sensor restoration retained for recovery: {error}");
        }
    }
}

/// Include orphaned attribution when no live sensor object was acquired.
pub(crate) fn sensor_recovery_pending(dir: &Path) -> bool {
    power::recovery_pending(dir)
}

/// Must be called while the runtime control lock is held, independently of
/// whether EC/module setup was ever reached by the previous invocation.
pub(crate) fn recover_sensor(dir: &Path) -> io::Result<()> {
    let Some(lease) = power::Lease::read(dir, None, 0)? else {
        return Ok(());
    };
    check_hardware_profile()?;
    lease.restore(&Secondary::discover()?)
}

impl Secondary {
    fn discover() -> io::Result<Self> {
        // Enumerate sysfs adapter identities, never probe I2C addresses.
        let mut secondaries = Vec::new();
        for entry in fs::read_dir("/sys/bus/i2c/devices")? {
            let path = entry?.path();
            let Some(number) = path
                .file_name()
                .and_then(|v| v.to_str())
                .and_then(adapter_number)
            else {
                continue;
            };
            if acpi_is(&path, "I2C3") {
                secondaries.push((fs::canonicalize(path)?, number));
            }
        }
        let (secondary_adapter, number) = unique(
            secondaries,
            "need exactly one adapter with ACPI identity I2C3",
        )?;
        let secondary_device = PathBuf::from(format!("/dev/i2c-{number}"));
        Self::unclaimed(&secondary_adapter, number)?;
        // O_RDWR supports repeated-start reads and the separately journaled,
        // fixed CONTROL 01↔00 transition. No general register write is exposed.
        let secondary = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&secondary_device)?;
        let metadata = secondary.metadata()?;
        if !metadata.file_type().is_char_device()
            || libc::major(metadata.rdev()) != 89
            || libc::minor(metadata.rdev()) != number
        {
            return Err(invalid("unexpected i2c character device identity"));
        }
        Ok(Self {
            file: secondary,
            adapter: secondary_adapter,
            adapter_number: number,
        })
    }

    fn unclaimed(adapter: &Path, number: u32) -> io::Result<()> {
        if !acpi_is(adapter, "I2C3") {
            return Err(invalid("secondary adapter ACPI identity changed"));
        }
        if Path::new(&format!("/sys/bus/i2c/devices/{number}-0015")).exists() {
            return Err(invalid("secondary accelerometer acquired a kernel client"));
        }
        for entry in fs::read_dir(adapter)? {
            let name = entry?.file_name();
            if name
                .to_str()
                .is_some_and(|s| s.starts_with("i2c-") && s.contains(':'))
            {
                return Err(invalid("secondary adapter now has an ACPI I2C client"));
            }
        }
        Ok(())
    }

    fn read_register<const N: usize>(&self, register: u8) -> io::Result<[u8; N]> {
        if !matches!((register, N), (0x0e, 1) | (0x0d, 1) | (0x03, 6)) {
            return Err(invalid("sensor read outside the fixed register whitelist"));
        }
        let mut pointer = register;
        let mut buffer = [0_u8; N];
        let mut messages = [
            I2cMessage {
                address: SENSOR_ADDRESS,
                flags: 0,
                length: 1,
                buffer: &mut pointer,
            },
            I2cMessage {
                address: SENSOR_ADDRESS,
                flags: I2C_M_RD,
                length: N as u16,
                buffer: buffer.as_mut_ptr(),
            },
        ];
        let mut transfer = I2cTransfer {
            messages: messages.as_mut_ptr(),
            count: 2,
        };
        // SAFETY: both messages, the pointer byte and destination buffer remain
        // live for this synchronous ioctl. Address, count, flags and lengths
        // are fixed here. No retry loop and no adapter-timeout/config writes.
        let result = unsafe { libc::ioctl(self.file.as_raw_fd(), I2C_RDWR as _, &mut transfer) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if result != 2 {
            return Err(invalid("partial secondary sensor I2C transfer"));
        }
        Ok(buffer)
    }
}

impl power::SensorIo for Secondary {
    fn control(&self) -> io::Result<u8> {
        Self::unclaimed(&self.adapter, self.adapter_number)?;
        if self.read_register::<1>(0x0e)?[0] != SENSOR_ID {
            return Err(invalid(
                "secondary sensor ID differs from the inspected profile",
            ));
        }
        self.read_register::<1>(0x0d).map(|v| v[0])
    }

    fn write_control(&self, value: u8) -> Result<(), power::WriteFailure> {
        use power::WriteFailure;
        if value > 1 {
            return Err(WriteFailure::before(invalid(
                "secondary sensor write outside the fixed PD operation",
            )));
        }
        // Recheck ownership and identity before a single fixed register write.
        if self.control().map_err(WriteFailure::before)? != value ^ 1 {
            return Err(WriteFailure::before(invalid(
                "secondary sensor changed before the owned PD transition",
            )));
        }
        let mut bytes = [0x0d, value];
        let mut message = I2cMessage {
            address: SENSOR_ADDRESS,
            flags: 0,
            length: 2,
            buffer: bytes.as_mut_ptr(),
        };
        let mut transfer = I2cTransfer {
            messages: &mut message,
            count: 1,
        };
        // SAFETY: the single fixed message and two-byte buffer remain live for
        // this synchronous ioctl. No reset, interrupt, range or test writes.
        let result = unsafe { libc::ioctl(self.file.as_raw_fd(), I2C_RDWR as _, &mut transfer) };
        if result < 0 {
            return Err(WriteFailure::attempted(io::Error::last_os_error()));
        }
        if result != 1 {
            return Err(WriteFailure::attempted(invalid(
                "partial secondary sensor PD write",
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct EstimatorConfig {
    tablet_enter_deg: f64,
    tablet_exit_deg: f64,
    folded_window_deg: f64,
    min_projected_ratio: f64,
    max_direction_step_deg: f64,
    /// A longer gap breaks continuity; it cannot contribute to settling time.
    max_sample_gap: Duration,
    settle: Duration,
    invalid_timeout: Duration,
    /// Fresh motion keeps a confirmed posture only this long after the last
    /// stationary reading; a sensor emitting random vectors must fail open.
    max_motion_hold: Duration,
}

impl Default for EstimatorConfig {
    fn default() -> Self {
        Self {
            tablet_enter_deg: 185.0,
            tablet_exit_deg: 175.0,
            folded_window_deg: 20.0,
            min_projected_ratio: 0.25,
            max_direction_step_deg: 25.0,
            max_sample_gap: Duration::from_millis(750),
            settle: Duration::from_millis(500),
            invalid_timeout: Duration::from_millis(1500),
            max_motion_hold: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EstimateReason {
    Valid,
    /// Hall-open resolves the chosen policy for the 0/360 wrap region; this
    /// also matches a genuinely almost-closed but Hall-open lid. The two
    /// accelerometers cannot mathematically distinguish those situations.
    NearZeroHallOpen,
    NearZeroHistory,
    LidClosed,
    InvalidSample,
    HingeVertical,
    Moving,
    AmbiguousFold,
    Stale,
    ClockReset,
}

#[derive(Clone, Copy, Debug)]
pub struct Estimate {
    pub tablet: bool,
    /// Signed-angle result normalized into 0..360 before the Hall heuristic.
    /// The fully folded sample measures about 0.86 degrees, not exactly 360.
    pub angle_deg: Option<f64>,
    /// Sensor geometry is usable now. A near-zero Hall assumption is still
    /// reported separately in `reason`, rather than claimed as proof.
    pub reliable: bool,
    pub reason: EstimateReason,
}

fn vector_norm(v: [f64; 3]) -> f64 {
    v.iter().map(|v| v * v).sum::<f64>().sqrt()
}

fn unit(v: [f64; 3]) -> Option<[f64; 3]> {
    // Reject non-finite, clipped, zero and almost-zero 12-bit readings. Do not
    // demand a nominal 1024 counts/g: actual norm varies around 800..1260.
    if v.iter().any(|v| !v.is_finite() || v.abs() >= 2047.0) {
        return None;
    }
    let norm = vector_norm(v);
    (norm >= 256.0).then(|| v.map(|v| v / norm))
}

fn geometry(
    sample: Sample,
    projected_min: f64,
) -> Result<([f64; 3], [f64; 3], f64), EstimateReason> {
    let lid = unit(sample.primary).ok_or(EstimateReason::InvalidSample)?;
    let base = unit(sample.secondary).ok_or(EstimateReason::InvalidSample)?;
    if lid[0].hypot(lid[2]) < projected_min || base[0].hypot(base[2]) < projected_min {
        return Err(EstimateReason::HingeVertical);
    }
    // Both verified sensor frames share hinge axis Y. The X/Z signed cross
    // product preserves 0..360 information that unsigned acos(dot) loses.
    let cross = base[2] * lid[0] - base[0] * lid[2];
    let dot = base[0] * lid[0] + base[2] * lid[2];
    let angle = cross.atan2(dot).to_degrees().rem_euclid(360.0);
    Ok((lid, base, angle))
}

#[derive(Default)]
pub struct HingeEstimator {
    config: EstimatorConfig,
    tablet: bool,
    pending: Option<(bool, Duration)>,
    last_good: Option<Duration>,
    last_stationary: Option<Duration>,
    last_now: Option<Duration>,
    last_vectors: Option<([f64; 3], [f64; 3], Duration)>,
    /// Expected silence after the last good sample when the slow settled
    /// cadence was requested; fail-open time is extended by the same amount.
    good_allowance: Duration,
    cadence: Cadence,
}

#[derive(Default)]
struct Cadence {
    settled_since: Option<Duration>,
    settled_angle: Option<f64>,
    slow: bool,
}

impl HingeEstimator {
    pub fn reset(&mut self) {
        self.clear();
        self.cadence = Cadence::default();
    }

    fn clear(&mut self) {
        self.tablet = false;
        self.pending = None;
        self.last_good = None;
        self.last_stationary = None;
        self.last_now = None;
        self.last_vectors = None;
        self.good_allowance = Duration::ZERO;
    }

    /// When the caller should take the next posture sample: fast while a
    /// transition, motion or missing data may be in progress, slow once the
    /// posture has stayed settled.
    pub fn next_interval(&self) -> Duration {
        if self.cadence.slow {
            STABLE_SAMPLE_INTERVAL
        } else {
            SAMPLE_INTERVAL
        }
    }

    /// A deliberately slow cadence is expected silence, not missing data.
    fn silent_too_long(&self, now: Duration) -> bool {
        self.last_good.is_none_or(|last| {
            now.saturating_sub(last) >= self.config.invalid_timeout + self.good_allowance
        })
    }

    fn good(&mut self, now: Duration) {
        self.last_good = Some(now);
    }

    fn invalid(&mut self, now: Duration, reason: EstimateReason) -> Estimate {
        self.pending = None;
        let stale = self.silent_too_long(now);
        if stale {
            self.tablet = false;
        }
        Estimate {
            tablet: self.tablet,
            angle_deg: None,
            reliable: false,
            reason: if stale && self.last_good.is_some() {
                EstimateReason::Stale
            } else {
                reason
            },
        }
    }

    /// `now` is monotonic elapsed time supplied by the caller. Pass `None` for
    /// failed/stale sensor reads; this eventually re-enables physical input.
    /// A closed Hall lid always clears tablet mode immediately. With an open
    /// Hall lid the small wrap region is treated as fully folded, accepting
    /// the documented ambiguity at a genuinely tiny laptop opening angle.
    /// Sample again after `next_interval()`.
    pub fn update(
        &mut self,
        sample: Option<Sample>,
        lid_closed: Option<bool>,
        now: Duration,
    ) -> Estimate {
        let estimate = self.observe(sample, lid_closed, now);
        let settled = self.pending.is_none()
            && matches!(
                estimate.reason,
                EstimateReason::Valid
                    | EstimateReason::HingeVertical
                    | EstimateReason::NearZeroHallOpen
                    | EstimateReason::NearZeroHistory
                    | EstimateReason::LidClosed
            );
        let moved = match (estimate.angle_deg, self.cadence.settled_angle) {
            (Some(angle), Some(reference)) => {
                let delta = (angle - reference).rem_euclid(360.0);
                delta.min(360.0 - delta) >= ACTIVITY_DEG
            }
            _ => false,
        };
        if !settled || moved {
            self.cadence = Cadence::default();
        }
        if settled {
            self.cadence.settled_since.get_or_insert(now);
            if self.cadence.settled_angle.is_none() {
                self.cadence.settled_angle = estimate.angle_deg;
            }
        }
        self.cadence.slow = self
            .cadence
            .settled_since
            .is_some_and(|since| now.saturating_sub(since) >= SETTLE_BEFORE_SLOW);
        if self.last_good == Some(now) {
            self.good_allowance = self.next_interval().saturating_sub(SAMPLE_INTERVAL);
        }
        estimate
    }

    fn observe(
        &mut self,
        sample: Option<Sample>,
        lid_closed: Option<bool>,
        now: Duration,
    ) -> Estimate {
        if self.last_now.is_some_and(|last| now < last) {
            self.reset();
            self.last_now = Some(now);
            return self.invalid(now, EstimateReason::ClockReset);
        }
        let gap = self
            .last_now
            .is_some_and(|last| now.saturating_sub(last) > self.config.max_sample_gap);
        if gap {
            self.pending = None;
            self.last_vectors = None;
        }
        // A fresh sample cannot retroactively validate a silent interval. After
        // the fail-open deadline, even known tablet posture needs fresh dwell.
        // The slow settled cadence requested by next_interval() is not silence.
        if self.last_good.is_some() && self.silent_too_long(now) {
            self.tablet = false;
            self.pending = None;
        }
        self.last_now = Some(now);
        if lid_closed == Some(true) {
            self.clear();
            self.last_now = Some(now);
            return Estimate {
                tablet: false,
                angle_deg: None,
                reliable: true,
                reason: EstimateReason::LidClosed,
            };
        }
        let Some(sample) = sample else {
            return self.invalid(now, EstimateReason::InvalidSample);
        };
        let (Some(lid), Some(base)) = (unit(sample.primary), unit(sample.secondary)) else {
            return self.invalid(now, EstimateReason::InvalidSample);
        };
        if let Some((old_lid, old_base, last)) = self.last_vectors {
            let dt = now.saturating_sub(last);
            let largest_step = |a: [f64; 3], b: [f64; 3]| {
                a.into_iter()
                    .zip(b)
                    .map(|(a, b)| a * b)
                    .sum::<f64>()
                    .clamp(-1.0, 1.0)
                    .acos()
                    .to_degrees()
            };
            // Ignore this comparison after a polling gap; otherwise abrupt
            // acceleration/rotation must settle before a new posture latches.
            if dt <= self.config.max_sample_gap
                && (largest_step(lid, old_lid) > self.config.max_direction_step_deg
                    || largest_step(base, old_base) > self.config.max_direction_step_deg)
            {
                self.last_vectors = Some((lid, base, now));
                // Fresh, plausible vectors during motion are not missing
                // data. Keep the last posture while preventing a new latch.
                self.pending = None;
                if self
                    .last_stationary
                    .is_some_and(|last| now.saturating_sub(last) < self.config.max_motion_hold)
                {
                    self.good(now);
                }
                return Estimate {
                    tablet: self.tablet,
                    angle_deg: None,
                    reliable: false,
                    reason: EstimateReason::Moving,
                };
            }
        }
        self.last_vectors = Some((lid, base, now));
        let angle = match geometry(sample, self.config.min_projected_ratio) {
            Ok((_, _, angle)) => angle,
            Err(EstimateReason::HingeVertical) => {
                // Fresh stationary gravity vectors are not a sensor failure.
                // With the hinge parallel to gravity its angle is physically
                // unobservable. Keep the last confirmed posture (laptop on
                // startup) so a folded tablet remains usable in portrait.
                // Unfolding in this exact orientation requires a slight tilt
                // before the sensors can observe the new angle.
                self.pending = None;
                self.good(now);
                self.last_stationary = Some(now);
                return Estimate {
                    tablet: self.tablet,
                    angle_deg: None,
                    reliable: false,
                    reason: EstimateReason::HingeVertical,
                };
            }
            Err(reason) => return self.invalid(now, reason),
        };
        let mut threshold_angle = angle;
        let reason = if angle <= self.config.folded_window_deg {
            if lid_closed == Some(false) {
                threshold_angle = 360.0;
                EstimateReason::NearZeroHallOpen
            } else if self.tablet {
                threshold_angle = 360.0;
                EstimateReason::NearZeroHistory
            } else {
                return self.invalid(now, EstimateReason::AmbiguousFold);
            }
        } else {
            EstimateReason::Valid
        };
        self.good(now);
        self.last_stationary = Some(now);
        let target = if threshold_angle >= self.config.tablet_enter_deg {
            true
        } else if threshold_angle <= self.config.tablet_exit_deg {
            false
        } else {
            self.tablet
        };
        if target == self.tablet {
            self.pending = None;
        } else if let Some((pending, since)) = self.pending {
            if pending != target {
                self.pending = Some((target, now));
            } else if now.saturating_sub(since) >= self.config.settle {
                self.tablet = target;
                self.pending = None;
            }
        } else {
            self.pending = Some((target, now));
        }
        Estimate {
            tablet: self.tablet,
            angle_deg: Some(angle),
            reliable: true,
            reason,
        }
    }
}

// Linux uinput UAPI. Derive ioctl sizes from the structures instead of
// embedding the x86-64 size of input_event in the wire layout.
const fn iow(number: u32, size: usize) -> libc::c_ulong {
    ((1_u32 << 30) | ((size as u32) << 16) | ((b'U' as u32) << 8) | number) as libc::c_ulong
}
const UI_SET_EVBIT: libc::c_ulong = iow(100, std::mem::size_of::<libc::c_int>());
const UI_SET_SWBIT: libc::c_ulong = iow(109, std::mem::size_of::<libc::c_int>());
const UI_DEV_SETUP: libc::c_ulong = iow(3, std::mem::size_of::<UinputSetup>());
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const EV_SW: u16 = 0x05;
const SW_TABLET_MODE: u16 = 0x01;

#[repr(C)]
#[derive(Default)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputEvent {
    time: libc::timeval,
    event_type: u16,
    code: u16,
    value: i32,
}

/// Root broker/guardian-owned switch; never hand its descriptor to policy.
/// No key, pointer, lid or other event capabilities are advertised.
pub struct VirtualSwitch {
    file: File,
    tablet: bool,
    known: bool,
}

impl VirtualSwitch {
    pub(crate) fn into_fd(self) -> OwnedFd {
        self.file.into()
    }
    pub(crate) fn from_fd(fd: OwnedFd) -> Self {
        Self {
            file: fd.into(),
            tablet: false,
            known: false,
        }
    }
    pub(crate) fn force_off(&mut self) -> io::Result<()> {
        self.known = false;
        self.send(false)
    }
    pub fn create() -> io::Result<Self> {
        check_hardware_profile()?;
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open("/dev/uinput")?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_char_device()
            || libc::major(metadata.rdev()) != 10
            || libc::minor(metadata.rdev()) != 223
        {
            return Err(invalid("unexpected uinput character device identity"));
        }
        let mut setup = UinputSetup {
            id: InputId {
                bustype: 0x06, // BUS_VIRTUAL
                version: 1,
                ..InputId::default()
            },
            name: [0; 80],
            ff_effects_max: 0,
        };
        setup.name[..SWITCH_NAME.len()].copy_from_slice(SWITCH_NAME.as_bytes());
        // SAFETY: the descriptor belongs to /dev/uinput. The two capabilities
        // are fixed integers; setup is a fully initialized repr(C) UAPI value.
        let result = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                UI_SET_EVBIT as _,
                libc::c_int::from(EV_SW),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: same fixed-descriptor/capability contract as above.
        if unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                UI_SET_SWBIT as _,
                libc::c_int::from(SW_TABLET_MODE),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: setup remains live and matches UI_DEV_SETUP's layout.
        if unsafe { libc::ioctl(file.as_raw_fd(), UI_DEV_SETUP as _, &setup) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: UI_DEV_CREATE takes no pointer argument.
        if unsafe { libc::ioctl(file.as_raw_fd(), UI_DEV_CREATE as _) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            file,
            tablet: false,
            known: true,
        })
    }

    pub fn send(&mut self, tablet: bool) -> io::Result<()> {
        if self.known && self.tablet == tablet {
            return Ok(());
        }
        let events = [
            InputEvent {
                time: libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                },
                event_type: EV_SW,
                code: SW_TABLET_MODE,
                value: i32::from(tablet),
            },
            InputEvent {
                time: libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                },
                event_type: 0, // EV_SYN
                code: 0,       // SYN_REPORT
                value: 0,
            },
        ];
        // SAFETY: InputEvent has the Linux input_event repr(C) layout and all
        // fields are initialized. There is no padding on the supported target.
        let bytes = unsafe {
            std::slice::from_raw_parts(events.as_ptr().cast::<u8>(), std::mem::size_of_val(&events))
        };
        // Mark a possible ON before writing: even an indeterminate short/error
        // result must make shutdown attempt OFF rather than assume it stayed off.
        if tablet {
            self.tablet = true;
        }
        self.known = false;
        self.file.write_all(bytes)?;
        self.tablet = tablet;
        self.known = true;
        Ok(())
    }
}

// Closing the last shared file description destroys the uinput device. Neither
// fork owner issues UI_DEV_DESTROY: the broker must retain the original device
// long enough to send OFF after guardian SIGKILL.

#[cfg(test)]
mod tests {
    use super::*;

    const LAPTOP: Sample = Sample {
        primary: [-941.83, 9.33, 199.33],
        secondary: [-3.67, -1.17, -1261.5],
    };
    const FLAT: Sample = Sample {
        primary: [-69.67, 5.0, 823.25],
        secondary: [66.92, 7.75, -1248.42],
    };
    const TENT: Sample = Sample {
        primary: [989.25, -23.67, 78.42],
        secondary: [1005.75, -4.92, -491.75],
    };
    const FOLDED: Sample = Sample {
        primary: [25.33, -12.17, 801.17],
        secondary: [13.25, -0.17, 797.25],
    };

    fn at(milliseconds: u64) -> Duration {
        Duration::from_millis(milliseconds)
    }

    fn estimator() -> HingeEstimator {
        HingeEstimator::default()
    }

    fn synthetic(angle: f64) -> Sample {
        let radians = angle.to_radians();
        Sample {
            primary: [-1000.0 * radians.sin(), 0.0, -1000.0 * radians.cos()],
            secondary: [0.0, 0.0, -1000.0],
        }
    }

    #[test]
    fn measured_poses_use_signed_geometry_with_independent_normalization() {
        for (sample, expected) in [
            (LAPTOP, 101.78),
            (FLAT, 178.2),
            (TENT, 329.4),
            (FOLDED, 0.86),
        ] {
            let (_, _, angle) = geometry(sample, 0.25).unwrap();
            assert!((angle - expected).abs() < 0.2, "{angle} != {expected}");
        }
        let scaled = Sample {
            primary: LAPTOP.primary.map(|v| v * 0.5),
            secondary: LAPTOP.secondary.map(|v| v * 1.2),
        };
        assert!(
            (geometry(scaled, 0.25).unwrap().2 - geometry(LAPTOP, 0.25).unwrap().2).abs() < 1e-9
        );
    }

    #[test]
    fn tent_latches_after_dwell_then_laptop_restores_after_dwell() {
        let mut estimator = estimator();
        assert!(!estimator.update(Some(TENT), Some(false), at(0)).tablet);
        assert!(!estimator.update(Some(TENT), Some(false), at(250)).tablet);
        assert!(estimator.update(Some(TENT), Some(false), at(500)).tablet);
        let moving = estimator.update(Some(LAPTOP), Some(false), at(750));
        assert!(moving.tablet);
        assert_eq!(moving.reason, EstimateReason::Moving);
        assert!(estimator.update(Some(LAPTOP), Some(false), at(1000)).tablet);
        assert!(estimator.update(Some(LAPTOP), Some(false), at(1250)).tablet);
        assert!(!estimator.update(Some(LAPTOP), Some(false), at(1500)).tablet);
    }

    #[test]
    fn prolonged_fresh_motion_preserves_tablet_but_missing_samples_expire() {
        let mut estimator = estimator();
        for t in [0, 250, 500] {
            estimator.update(Some(TENT), Some(false), at(t));
        }
        for i in 1..=24 {
            let sample = if i % 2 == 0 { TENT } else { LAPTOP };
            let estimate = estimator.update(Some(sample), Some(false), at(500 + i * 250));
            assert!(estimate.tablet);
            assert_eq!(estimate.reason, EstimateReason::Moving);
        }
        assert!(estimator.update(None, Some(false), at(6750)).tablet);
        assert!(!estimator.update(None, Some(false), at(8000)).tablet);
    }

    #[test]
    fn endless_fresh_motion_releases_physical_input_after_the_motion_hold() {
        // A failing sensor can emit plausible but random vectors forever.
        let mut estimator = estimator();
        for t in [0, 250, 500] {
            estimator.update(Some(TENT), Some(false), at(t));
        }
        let mut released = None;
        for i in 1..=60 {
            let sample = if i % 2 == 0 { TENT } else { LAPTOP };
            let estimate = estimator.update(Some(sample), Some(false), at(500 + i * 250));
            assert_eq!(estimate.reason, EstimateReason::Moving);
            if !estimate.tablet && released.is_none() {
                released = Some(500 + i * 250);
            }
        }
        // Last stationary sample at 0.5 s; motion refreshes posture until
        // 10.25 s (under the 10 s hold), then the 1.5 s invalid timeout runs.
        assert_eq!(released, Some(11_750));
    }

    #[test]
    fn settled_posture_slows_sampling_and_any_change_returns_to_the_fast_rate() {
        let mut estimator = estimator();
        let mut t = 0;
        while t < 2_000 {
            estimator.update(Some(LAPTOP), Some(false), at(t));
            assert_eq!(estimator.next_interval(), SAMPLE_INTERVAL, "{t}");
            t += 250;
        }
        estimator.update(Some(LAPTOP), Some(false), at(t));
        assert_eq!(estimator.next_interval(), STABLE_SAMPLE_INTERVAL);
        for _ in 0..5 {
            t += 2_000;
            assert!(!estimator.update(Some(LAPTOP), Some(false), at(t)).tablet);
            assert_eq!(estimator.next_interval(), STABLE_SAMPLE_INTERVAL);
        }
        // Folding is observed at the next slow sample and confirmed at 4 Hz.
        t += 2_000;
        assert!(!estimator.update(Some(TENT), Some(false), at(t)).tablet);
        assert_eq!(estimator.next_interval(), SAMPLE_INTERVAL);
        assert!(
            !estimator
                .update(Some(TENT), Some(false), at(t + 250))
                .tablet
        );
        assert!(
            estimator
                .update(Some(TENT), Some(false), at(t + 500))
                .tablet
        );
        // A large angle change without a mode change also restarts fast sampling.
        let mut estimator = self::estimator();
        for t in (0..=2_000).step_by(250) {
            estimator.update(Some(synthetic(100.0)), Some(false), at(t));
        }
        assert_eq!(estimator.next_interval(), STABLE_SAMPLE_INTERVAL);
        estimator.update(Some(synthetic(150.0)), Some(false), at(4_000));
        assert_eq!(estimator.next_interval(), SAMPLE_INTERVAL);
    }

    #[test]
    fn slow_cadence_keeps_tablet_mode_and_missing_data_still_fails_open() {
        let mut estimator = estimator();
        let mut t = 0;
        while t <= 2_500 {
            estimator.update(Some(TENT), Some(false), at(t));
            t += 250;
        }
        assert_eq!(estimator.next_interval(), STABLE_SAMPLE_INTERVAL);
        let mut last_good = t - 250;
        for _ in 0..10 {
            t = last_good + 2_000;
            assert!(estimator.update(Some(TENT), Some(false), at(t)).tablet);
            assert_eq!(estimator.next_interval(), STABLE_SAMPLE_INTERVAL);
            last_good = t;
        }
        // The first missing sample arrives when the slow cadence expected it;
        // the ordinary 1.25 s that follows a missed fast sample still applies.
        let first_miss = last_good + 2_000;
        assert!(estimator.update(None, Some(false), at(first_miss)).tablet);
        assert_eq!(estimator.next_interval(), SAMPLE_INTERVAL);
        let mut released = None;
        for t in (first_miss + 250..=first_miss + 2_000).step_by(250) {
            if !estimator.update(None, Some(false), at(t)).tablet && released.is_none() {
                released = Some(t);
            }
        }
        assert_eq!(released, Some(first_miss + 1_250));
    }

    #[test]
    fn hall_open_wrap_is_reported_as_an_assumption_not_a_fake_360_reading() {
        let mut estimator = estimator();
        let first = estimator.update(Some(FOLDED), Some(false), at(0));
        assert_eq!(first.reason, EstimateReason::NearZeroHallOpen);
        assert!(first.angle_deg.unwrap() < 1.0);
        assert!(estimator.update(Some(FOLDED), Some(false), at(500)).tablet);
        let closed = estimator.update(Some(FOLDED), Some(true), at(501));
        assert!(!closed.tablet);
        assert_eq!(closed.reason, EstimateReason::LidClosed);
    }

    #[test]
    fn near_zero_with_unknown_hall_is_safe_on_start_but_preserves_known_tablet_history() {
        let mut estimator = estimator();
        let ambiguous = estimator.update(Some(FOLDED), None, at(0));
        assert!(!ambiguous.tablet && !ambiguous.reliable);
        assert_eq!(ambiguous.reason, EstimateReason::AmbiguousFold);
        estimator.update(Some(TENT), None, at(250));
        estimator.update(Some(TENT), None, at(500));
        estimator.update(Some(TENT), None, at(750));
        assert!(estimator.update(Some(TENT), None, at(1000)).tablet);
        estimator.update(Some(FOLDED), None, at(1250));
        let wrap = estimator.update(Some(FOLDED), None, at(1500));
        assert!(wrap.tablet);
        assert_eq!(wrap.reason, EstimateReason::NearZeroHistory);
    }

    #[test]
    fn fresh_unobservable_gravity_preserves_posture_but_sensor_failure_expires() {
        let mut estimator = estimator();
        estimator.update(Some(TENT), Some(false), at(0));
        assert!(estimator.update(Some(TENT), Some(false), at(500)).tablet);
        let vertical = Sample {
            primary: [0.0, 900.0, 0.0],
            secondary: [0.0, 1200.0, 0.0],
        };
        for time in (750..60_000).step_by(250) {
            assert!(
                estimator
                    .update(Some(vertical), Some(false), at(time))
                    .tablet
            );
        }
        let portrait = estimator.update(Some(vertical), Some(false), at(60_000));
        assert!(portrait.tablet && !portrait.reliable);
        assert_eq!(portrait.reason, EstimateReason::HingeVertical);
        // The settled portrait requested the slow 2 s cadence, so a failure
        // releases input 1.25 s after the expected sample at 62 s.
        assert_eq!(estimator.next_interval(), STABLE_SAMPLE_INTERVAL);
        assert!(estimator.update(None, Some(false), at(62_000)).tablet);
        assert!(estimator.update(None, Some(false), at(63_000)).tablet);
        let stale = estimator.update(None, Some(false), at(63_250));
        assert!(!stale.tablet && !stale.reliable);
        assert_eq!(stale.reason, EstimateReason::Stale);

        let mut startup = super::HingeEstimator::default();
        assert!(!startup.update(Some(vertical), Some(false), at(0)).tablet);
        assert!(
            !startup
                .update(Some(vertical), Some(false), at(60_000))
                .tablet
        );
    }

    #[test]
    fn threshold_noise_and_one_dynamic_sample_cannot_disable_the_keyboard() {
        let mut estimator = estimator();
        estimator.update(Some(synthetic(190.0)), Some(false), at(0));
        estimator.update(Some(synthetic(180.0)), Some(false), at(300));
        assert!(
            !estimator
                .update(Some(synthetic(190.0)), Some(false), at(500))
                .tablet
        );
        assert!(
            !estimator
                .update(Some(synthetic(190.0)), Some(false), at(900))
                .tablet
        );
        assert!(
            estimator
                .update(Some(synthetic(190.0)), Some(false), at(1000))
                .tablet
        );
        let moving = estimator.update(Some(LAPTOP), Some(false), at(1100));
        assert_eq!(moving.reason, EstimateReason::Moving);
        assert!(moving.tablet);
    }

    #[test]
    fn malformed_samples_and_regressing_clock_fail_open() {
        let mut estimator = estimator();
        estimator.update(Some(TENT), Some(false), at(1000));
        assert!(estimator.update(Some(TENT), Some(false), at(1500)).tablet);
        let mut broken = TENT;
        broken.primary[0] = f64::NAN;
        assert!(estimator.update(Some(broken), Some(false), at(2000)).tablet);
        assert!(!estimator.update(Some(broken), Some(false), at(3000)).tablet);
        let clock = estimator.update(Some(TENT), Some(false), at(100));
        assert!(!clock.tablet);
        assert_eq!(clock.reason, EstimateReason::ClockReset);
    }

    #[test]
    fn normal_flat_tent_and_folded_transitions_use_the_actual_sampling_cadence() {
        let mut estimator = estimator();
        for time in [0, 250, 500] {
            assert!(!estimator.update(Some(LAPTOP), Some(false), at(time)).tablet);
        }
        for time in [750, 1000, 1250, 1500] {
            assert!(!estimator.update(Some(FLAT), Some(false), at(time)).tablet);
        }
        estimator.update(Some(TENT), Some(false), at(1750));
        assert!(!estimator.update(Some(TENT), Some(false), at(2000)).tablet);
        assert!(!estimator.update(Some(TENT), Some(false), at(2250)).tablet);
        assert!(estimator.update(Some(TENT), Some(false), at(2500)).tablet);
        estimator.update(Some(FOLDED), Some(false), at(2750));
        let folded = estimator.update(Some(FOLDED), Some(false), at(3000));
        assert!(folded.tablet);
        assert_eq!(folded.reason, EstimateReason::NearZeroHallOpen);
        // A flat hinge retains the previous mode rather than guessing a side.
        for time in [3250, 3500, 3750, 4000] {
            assert!(estimator.update(Some(FLAT), Some(false), at(time)).tablet);
        }
    }

    #[test]
    fn jitter_keeps_motion_rejection_and_requires_fresh_stable_observations() {
        let mut estimator = estimator();
        estimator.update(Some(LAPTOP), Some(false), at(0));
        let moving = estimator.update(Some(TENT), Some(false), at(350));
        assert_eq!(moving.reason, EstimateReason::Moving);
        for time in [650, 925] {
            assert!(!estimator.update(Some(TENT), Some(false), at(time)).tablet);
        }
        assert!(estimator.update(Some(TENT), Some(false), at(1210)).tablet);
    }

    #[test]
    fn polling_gaps_break_pending_dwell_and_motion_history() {
        for gap_end in [1001, 5000] {
            let mut estimator = estimator();
            estimator.update(Some(LAPTOP), Some(false), at(0));
            estimator.update(Some(TENT), Some(false), at(250));
            estimator.update(Some(TENT), Some(false), at(500));
            let fresh = estimator.update(Some(LAPTOP), Some(false), at(gap_end + 500));
            assert_eq!(fresh.reason, EstimateReason::Valid);
            // Start a pending tablet transition, then miss the observation window.
            estimator.reset();
            estimator.update(Some(TENT), Some(false), at(0));
            estimator.update(Some(TENT), Some(false), at(250));
            assert!(
                !estimator
                    .update(Some(TENT), Some(false), at(gap_end))
                    .tablet
            );
            assert!(
                !estimator
                    .update(Some(TENT), Some(false), at(gap_end + 250))
                    .tablet
            );
            assert!(
                estimator
                    .update(Some(TENT), Some(false), at(gap_end + 500))
                    .tablet
            );
        }
    }

    #[test]
    fn missing_samples_interrupt_dwell_and_recovery_needs_the_full_settle_time() {
        let mut estimator = estimator();
        estimator.update(Some(TENT), Some(false), at(0));
        estimator.update(Some(TENT), Some(false), at(250));
        assert!(!estimator.update(None, Some(false), at(500)).tablet);
        assert!(!estimator.update(Some(TENT), Some(false), at(750)).tablet);
        assert!(!estimator.update(Some(TENT), Some(false), at(1000)).tablet);
        assert!(estimator.update(Some(TENT), Some(false), at(1250)).tablet);
        // No callbacks at all must also expire confirmed posture. A fresh
        // reading cannot turn the unobserved interval into successful dwell.
        assert!(!estimator.update(Some(TENT), Some(false), at(5000)).tablet);
        assert!(!estimator.update(Some(TENT), Some(false), at(5250)).tablet);
        assert!(estimator.update(Some(TENT), Some(false), at(5500)).tablet);
    }

    #[test]
    fn a_long_silent_gap_cannot_preserve_unobservable_tablet_history() {
        let mut estimator = estimator();
        estimator.update(Some(TENT), Some(false), at(0));
        estimator.update(Some(TENT), Some(false), at(250));
        assert!(estimator.update(Some(TENT), Some(false), at(500)).tablet);
        let vertical = Sample {
            primary: [0.0, 900.0, 0.0],
            secondary: [0.0, 1200.0, 0.0],
        };
        let fresh = estimator.update(Some(vertical), Some(false), at(5000));
        assert!(!fresh.tablet);
        assert_eq!(fresh.reason, EstimateReason::HingeVertical);
    }

    #[test]
    fn uapi_sizes_and_only_expected_adapter_names_are_accepted() {
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(UI_DEV_SETUP, 0x405c5503);
        assert_eq!(UI_SET_EVBIT, 0x40045564);
        assert_eq!(UI_SET_SWBIT, 0x4004556d);
        assert_eq!(adapter_number("i2c-3"), Some(3));
        for name in ["i2c-MDA6655:00", "3-0015", "i2c--1", "i2c-"] {
            assert_eq!(adapter_number(name), None);
        }
    }
}
