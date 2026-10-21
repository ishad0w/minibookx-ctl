//! Read-only, bounded diagnostics. No raw EC, I2C or device access is performed.
use crate::{build_info, hardware, paths, platform, rapl, status};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

#[path = "doctor_graphics.rs"]
mod graphics;

const MAX_FILE: u64 = 128 * 1024;
const MAX_ENTRIES: usize = 128;
const UNIT_CGROUP: &str = "/sys/fs/cgroup/system.slice/minibookx-ctl.service";

#[derive(Clone)]
struct FileInfo {
    regular: bool,
    symlink: bool,
    root_owned: bool,
    mode: u32,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Missing {
    Absent,
    Permission,
    Invalid,
    Other,
}
impl Missing {
    fn from_io(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => Self::Absent,
            io::ErrorKind::PermissionDenied => Self::Permission,
            _ => Self::Other,
        }
    }
    fn code(self) -> &'static str {
        match self {
            Self::Absent => "unavailable",
            Self::Permission => "permission-denied",
            Self::Invalid => "invalid",
            Self::Other => "unreadable",
        }
    }
    fn description(self) -> &'static str {
        match self {
            Self::Absent => "not exposed by this system",
            Self::Permission => "permission denied; try sudo for this field",
            Self::Invalid => "unrecognized or oversized data",
            Self::Other => "could not read this source",
        }
    }
}

trait Sources {
    fn read(&self, path: &str) -> Result<String, Missing>;
    fn entries(&self, path: &str) -> Result<Vec<String>, Missing>;
    fn runtime(&self) -> Result<Value, Missing>;
    fn packages(&self, _backend: graphics::PackageBackend) -> Result<graphics::Inventory, String> {
        Err("package inventory unavailable from this source".into())
    }
    fn module_loader(&self) -> Result<String, Missing> {
        Err(Missing::Absent)
    }
    fn trusted(&self, path: &str) -> Result<String, Missing> {
        self.read(path)
    }
    fn info(&self, _path: &str) -> Result<FileInfo, Missing> {
        Err(Missing::Absent)
    }
    fn boottime_ms(&self) -> Result<u64, Missing> {
        Err(Missing::Absent)
    }
    fn link_target(&self, _path: &str) -> Result<String, Missing> {
        Err(Missing::Absent)
    }
}

struct System;
impl Sources for System {
    fn packages(&self, backend: graphics::PackageBackend) -> Result<graphics::Inventory, String> {
        graphics::query_packages(backend)
    }
    fn module_loader(&self) -> Result<String, Missing> {
        platform::system_tool("modprobe")
            .map(|path| path.display().to_string())
            .map_err(Missing::from_io)
    }
    fn read(&self, path: &str) -> Result<String, Missing> {
        // All callers use fixed kernel paths or bounded kernel-generated leaf
        // names. Reject special files and symlink leaves, even when run as root.
        system_text(path, false)
    }
    fn trusted(&self, path: &str) -> Result<String, Missing> {
        system_text(path, true)
    }
    fn info(&self, path: &str) -> Result<FileInfo, Missing> {
        let info = fs::symlink_metadata(path).map_err(Missing::from_io)?;
        Ok(FileInfo {
            regular: info.is_file(),
            symlink: info.file_type().is_symlink(),
            root_owned: info.uid() == 0,
            mode: info.mode() & 0o777,
            bytes: info.len(),
        })
    }
    fn boottime_ms(&self) -> Result<u64, Missing> {
        status::boottime_ms().map_err(Missing::from_io)
    }
    fn link_target(&self, path: &str) -> Result<String, Missing> {
        let target = fs::read_link(path).map_err(Missing::from_io)?;
        let target = target
            .to_str()
            .filter(|v| v.len() <= 4096)
            .ok_or(Missing::Invalid)?;
        Ok(target.to_owned())
    }
    fn entries(&self, path: &str) -> Result<Vec<String>, Missing> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(path)
            .map_err(Missing::from_io)?
            .take(MAX_ENTRIES)
        {
            let entry = entry.map_err(Missing::from_io)?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !name.is_empty()
                && name.len() <= 80
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
                && name != "."
                && name != ".."
            {
                entries.push(name);
            }
        }
        entries.sort();
        Ok(entries)
    }
    fn runtime(&self) -> Result<Value, Missing> {
        status::current(Path::new(hardware::RUN_DIR)).map_err(Missing::from_io)
    }
}

fn system_text(path: &str, require_root: bool) -> Result<String, Missing> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .map_err(Missing::from_io)?;
    let info = file.metadata().map_err(Missing::from_io)?;
    if !info.is_file()
        || (require_root && (info.uid() != 0 || info.nlink() != 1 || info.mode() & 0o022 != 0))
    {
        return Err(Missing::Invalid);
    }
    read_bounded(file)
}

fn read_bounded(reader: impl Read) -> Result<String, Missing> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(Missing::from_io)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(Missing::Invalid);
    }
    String::from_utf8(bytes).map_err(|_| Missing::Invalid)
}

/// Remove terminal controls, Unicode direction controls and line breaks. JSON
/// values use the same treatment, so a pasted diagnostic remains safe to read.
fn clean(value: &str) -> String {
    value
        .chars()
        .take(256)
        .map(|c| {
            if c.is_control()
                || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
            {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn text(sources: &impl Sources, path: &str) -> Result<String, Missing> {
    let value = sources.read(path)?;
    let value = clean(value.trim());
    if value.is_empty() {
        Err(Missing::Invalid)
    } else {
        Ok(value)
    }
}
fn number(sources: &impl Sources, path: &str) -> Result<i64, Missing> {
    sources
        .read(path)?
        .trim()
        .parse()
        .map_err(|_| Missing::Invalid)
}
fn field(key: &str, label: &str, result: Result<String, Missing>) -> Value {
    match result {
        Ok(value) => json!({"key": key, "label": label, "state": "ok", "value": clean(&value)}),
        Err(missing) => {
            json!({"key": key, "label": label, "state": missing.code(), "message": missing.description()})
        }
    }
}
fn note(key: &str, label: &str, message: &str) -> Value {
    json!({"key": key, "label": label, "state": "unknown", "message": message})
}
fn warning(key: &str, label: &str, message: &str) -> Value {
    json!({"key": key, "label": label, "state": "warning", "message": clean(message)})
}
fn section(key: &str, title: &str, fields: Vec<Value>) -> Value {
    json!({"key": key, "title": title, "fields": fields})
}

fn system(sources: &impl Sources) -> Value {
    let mut fields = vec![field(
        "kernel",
        "Kernel",
        text(sources, "/proc/sys/kernel/osrelease"),
    )];
    fields.push(field(
        "module_loader",
        "Module loader",
        sources.module_loader(),
    ));
    let os = sources
        .read("/etc/os-release")
        .or_else(|_| sources.read("/usr/lib/os-release"))
        .and_then(|s| {
            s.lines()
                .find_map(|line| line.strip_prefix("PRETTY_NAME="))
                .map(|name| clean(name.trim_matches(['\'', '"'])))
                .filter(|s| !s.is_empty())
                .ok_or(Missing::Invalid)
        });
    fields.push(field("distribution", "Distribution", os));
    let cpu = sources.read("/proc/cpuinfo").and_then(|s| {
        s.lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == "model name").then(|| clean(value))
            })
            .filter(|s| !s.is_empty())
            .ok_or(Missing::Invalid)
    });
    fields.push(field("cpu", "CPU", cpu));
    for (key, label) in [
        ("sys_vendor", "Manufacturer"),
        ("product_name", "Model"),
        ("bios_vendor", "BIOS vendor"),
        ("bios_version", "BIOS version"),
        ("bios_date", "BIOS date"),
    ] {
        fields.push(field(
            key,
            label,
            text(sources, &format!("/sys/class/dmi/id/{key}")),
        ));
    }
    let known_profile = (|| {
        Ok::<_, Missing>(platform::supported_profile(
            &sources.read("/sys/class/dmi/id/sys_vendor")?,
            &sources.read("/sys/class/dmi/id/product_name")?,
            &sources.read("/sys/class/dmi/id/bios_version")?,
            &sources.read("/proc/cpuinfo")?,
        ))
    })()
    .unwrap_or(false);
    fields.push(if known_profile {
        field(
            "hardware_profile",
            "Controller profile",
            Ok("matches the supported U300 DMI/CPU profile; EC identity not probed".into()),
        )
    } else {
        warning(
            "hardware_profile",
            "Controller profile",
            "supported U300 DMI/CPU profile not confirmed; hardware control must remain disabled",
        )
    });
    section("system", "System", fields)
}

fn embedded_controller(sources: &impl Sources) -> Value {
    let firmware = text(sources, "/sys/class/dmi/id/ec_firmware_release").and_then(|v| {
        let (major, minor) = v.split_once('.').ok_or(Missing::Invalid)?;
        let (major, minor) = (
            major.parse::<u8>().map_err(|_| Missing::Invalid)?,
            minor.parse::<u8>().map_err(|_| Missing::Invalid)?,
        );
        match (major, minor) {
            (255, _) | (_, 255) | (0, 0) => Err(Missing::Absent),
            _ => Ok(format!("{major}.{minor} (reported by DMI)")),
        }
    });
    section(
        "embedded_controller",
        "Embedded controller",
        vec![
            note(
                "chip",
                "Detected EC chip",
                "not exported by a standard Linux interface; no raw probing performed",
            ),
            note(
                "profile_chip",
                "Supported profile expects",
                "ITE IT5570, revision 02; a profile reference, not a chip identification",
            ),
            field("firmware", "EC firmware release", firmware),
            note(
                "charger_chip",
                "Charger chip",
                "not identified; battery manufacturer is not the charging-controller model",
            ),
            field("ec_sys", "EC kernel interface", ec_interface(sources)),
        ],
    )
}

fn ec_interface(sources: &impl Sources) -> Result<String, Missing> {
    match text(sources, "/sys/module/ec_sys/parameters/write_support")?.as_str() {
        "Y" => Ok("present (built-in or loaded); write_support=Y (unchanged)".into()),
        "N" => Ok("present (built-in or loaded); write_support=N (unchanged)".into()),
        _ => Err(Missing::Invalid),
    }
}

fn lockdown_mode(sources: &impl Sources) -> Result<String, Missing> {
    let value = sources.read("/sys/kernel/security/lockdown")?;
    let selected: Vec<_> = value
        .split_whitespace()
        .filter_map(|word| word.strip_prefix('[')?.strip_suffix(']'))
        .collect();
    match selected.as_slice() {
        [mode @ ("none" | "integrity" | "confidentiality")] => Ok((*mode).into()),
        _ => Err(Missing::Invalid),
    }
}

fn fan(sources: &impl Sources, snapshot: &Result<Value, Missing>) -> Value {
    let enabled = accepted(snapshot).and_then(|value| {
        value
            .get("fan_control")
            .ok_or(Missing::Absent)?
            .as_bool()
            .ok_or(Missing::Invalid)
    });
    let mut fields = vec![field(
        "control",
        "Last accepted fan control",
        enabled.map(|enabled| {
            if enabled {
                "enabled; requires writable EC access".into()
            } else {
                "disabled; EC access is not required by this mode".into()
            }
        }),
    )];
    fields.push(field(
        "ec_sys",
        "EC kernel interface",
        ec_interface(sources),
    ));
    let lockdown = lockdown_mode(sources);
    fields.push(field("lockdown", "Kernel lockdown", lockdown.clone()));
    if enabled == Ok(true)
        && live_policy(sources, snapshot)
        && lockdown.as_deref().is_ok_and(|mode| mode != "none")
    {
        fields.push(warning(
            "ec_access",
            "Fan access",
            "kernel lockdown can deny ec_sys writes despite an installed or signed module; check the journal. To keep tablet/power features without EC access, set fan_control=false and restart the service",
        ));
    }
    fields.push(note(
        "scope",
        "EC access check",
        "No raw EC file was opened or write attempted. An absent interface may be unloaded or unavailable; check the exact running kernel. fan_control=false does not require ec_sys. See docs/distributions.md",
    ));
    section("fan", "Fan control", fields)
}

fn batteries(sources: &impl Sources) -> Value {
    let mut fields = Vec::new();
    match sources.entries("/sys/class/power_supply") {
        Err(error) => fields.push(field("power_supply", "Battery interfaces", Err(error))),
        Ok(entries) => {
            for name in entries.iter().take(16) {
                let root = format!("/sys/class/power_supply/{name}");
                if text(sources, &format!("{root}/type")).as_deref() != Ok("Battery") {
                    continue;
                }
                let scope = text(sources, &format!("{root}/scope"));
                let conventional_name = name.strip_prefix("BAT").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
                });
                // Peripheral battery names may contain a Bluetooth MAC or a
                // personal device name. Only report system batteries.
                if scope.as_deref() == Ok("Device")
                    || (scope.as_deref() != Ok("System") && !conventional_name)
                {
                    continue;
                }
                let label_name = if conventional_name {
                    name.as_str()
                } else {
                    "System battery"
                };
                for (key, label) in [
                    ("manufacturer", "manufacturer"),
                    ("model_name", "model"),
                    ("technology", "technology"),
                    ("status", "status"),
                ] {
                    fields.push(field(
                        &format!("{label_name}.{key}"),
                        &format!("{label_name} {label}"),
                        text(sources, &format!("{root}/{key}")),
                    ));
                }
                for (key, label, divisor, unit) in [
                    ("capacity", "charge", 1., "%"),
                    ("energy_now", "energy now", 1_000_000., "Wh"),
                    ("energy_full", "full energy", 1_000_000., "Wh"),
                    ("energy_full_design", "design energy", 1_000_000., "Wh"),
                    ("charge_full", "full charge", 1_000_000., "Ah"),
                    ("charge_full_design", "design charge", 1_000_000., "Ah"),
                    ("voltage_now", "voltage", 1_000_000., "V"),
                    ("power_now", "power", 1_000_000., "W"),
                    ("cycle_count", "cycles", 1., ""),
                    (
                        "charge_control_start_threshold",
                        "charge start threshold",
                        1.,
                        "%",
                    ),
                    (
                        "charge_control_end_threshold",
                        "charge stop threshold",
                        1.,
                        "%",
                    ),
                ] {
                    let result = number(sources, &format!("{root}/{key}")).and_then(|v| {
                        let limit = if key == "capacity" || key.ends_with("threshold") {
                            100
                        } else {
                            1_000_000_000
                        };
                        if !(0..=limit).contains(&v) {
                            Err(Missing::Invalid)
                        } else if divisor == 1. {
                            Ok(format!("{v}{unit}"))
                        } else {
                            Ok(format!("{:.2} {unit}", v as f64 / divisor))
                        }
                    });
                    // Optional energy/charge representations vary by driver.
                    // Preserve threshold availability, but do not flood the
                    // report with absent alternative electrical quantities.
                    if result != Err(Missing::Absent)
                        || key.ends_with("threshold")
                        || key == "capacity"
                    {
                        fields.push(field(
                            &format!("{label_name}.{key}"),
                            &format!("{label_name} {label}"),
                            result,
                        ));
                    }
                }
            }
            if fields.is_empty() {
                fields.push(note("battery", "Battery", "no battery interface found"));
            }
        }
    }
    section("battery", "Battery", fields)
}

fn sensors(sources: &impl Sources) -> Value {
    let mut fields = Vec::new();
    match sources.entries("/sys/class/hwmon") {
        Err(error) => fields.push(field("hwmon", "Temperature sensors", Err(error))),
        Ok(entries) => {
            for device in entries.iter().take(16) {
                let root = format!("/sys/class/hwmon/{device}");
                let driver =
                    text(sources, &format!("{root}/name")).unwrap_or_else(|_| device.clone());
                for index in 1..=8 {
                    let key = format!("temp{index}");
                    let result = number(sources, &format!("{root}/{key}_input"));
                    if result == Err(Missing::Absent) {
                        continue;
                    }
                    let label = text(sources, &format!("{root}/{key}_label"))
                        .unwrap_or_else(|_| key.clone());
                    let result = result.and_then(|v| {
                        if !(-40_000..=150_000).contains(&v) {
                            Err(Missing::Invalid)
                        } else {
                            Ok(format!("{:.1} °C", v as f64 / 1000.))
                        }
                    });
                    fields.push(field(
                        &format!("{device}.{key}"),
                        &format!("{driver}: {label}"),
                        result,
                    ));
                }
            }
            if fields.is_empty() {
                fields.push(note(
                    "hwmon",
                    "Temperature sensors",
                    "no hwmon temperature inputs found",
                ));
            }
        }
    }
    for (key, path, label) in [
        ("iio", "/sys/bus/iio/devices", "IIO accelerometers"),
        ("input", "/sys/class/input", "Tablet switch"),
    ] {
        let result = sources.entries(path).and_then(|entries| {
            let names: Vec<_> = entries
                .iter()
                .filter(|name| {
                    if key == "input" {
                        name.starts_with("input")
                    } else {
                        name.starts_with("iio:device")
                    }
                })
                .take(32)
                .filter_map(|name| text(sources, &format!("{path}/{name}/name")).ok())
                .filter(|name| key != "input" || name == crate::tablet::SWITCH_NAME)
                .collect();
            if names.is_empty() {
                Err(Missing::Absent)
            } else {
                Ok(names.join(", "))
            }
        });
        fields.push(field(key, label, result));
    }
    section("sensors", "Sensors and input", fields)
}

/// DRM's mode dump: name, rounded refresh, clock kHz, hdisplay/hsync/
/// hsync/htotal, vdisplay/vsync/vsync/vtotal, flags. Derive the actual rate.
fn parse_mode(line: &str) -> Option<String> {
    let (_, values) = line.split_once("\":")?;
    let numbers: Vec<_> = values.split_whitespace().take(10).collect();
    if numbers.len() < 10 {
        return None;
    }
    let value = |i: usize| numbers[i].parse::<u64>().ok();
    let (clock, width, height, htotal, vtotal) =
        (value(1)?, value(2)?, value(6)?, value(5)?, value(9)?);
    if !(1..=10_000).contains(&width)
        || !(1..=10_000).contains(&height)
        || !(width..=20_000).contains(&htotal)
        || !(height..=20_000).contains(&vtotal)
        || !(1..=2_000_000).contains(&clock)
    {
        return None;
    }
    Some(format!(
        "{width}×{height} @ {:.3} Hz ({clock} kHz pixel clock)",
        clock as f64 * 1000. / (htotal * vtotal) as f64
    ))
}

fn display(sources: &impl Sources) -> Value {
    let mut fields = Vec::new();
    match sources.entries("/sys/class/drm") {
        Err(error) => fields.push(field("connectors", "Internal display", Err(error))),
        Ok(entries) => {
            for name in entries
                .iter()
                .filter(|n| n.starts_with("card") && (n.contains("-DSI-") || n.contains("-eDP-")))
                .take(8)
            {
                let root = format!("/sys/class/drm/{name}");
                let result = text(sources, &format!("{root}/status"));
                fields.push(field(&format!("{name}.status"), name, result));
                fields.push(field(
                    &format!("{name}.enabled"),
                    &format!("{name} enabled"),
                    text(sources, &format!("{root}/enabled")),
                ));
            }
            if fields.is_empty() {
                fields.push(note(
                    "connectors",
                    "Internal display",
                    "no DSI/eDP connector found",
                ));
            }
        }
    }
    let timeout = number(
        sources,
        "/sys/module/i915/parameters/dsi_link_ready_timeout_us",
    )
    .and_then(|v| {
        if (0..=1_000_000).contains(&v) {
            Ok(format!("{v} µs; override parameter is exposed"))
        } else {
            Err(Missing::Invalid)
        }
    });
    fields.push(field("i915_timeout_us", "i915 DSI timeout", timeout));
    fields.push(note("patch_status", "Driver patch status", "parameter presence confirms an override interface, not its implementation or an upstream fix; absence alone cannot diagnose the display"));
    fields.push(field(
        "render_nodes",
        "GPU render nodes",
        sources.entries("/dev/dri").and_then(|entries| {
            let nodes: Vec<_> = entries
                .iter()
                .filter(|name| {
                    name.strip_prefix("renderD")
                        .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
                })
                .take(8)
                .map(|name| format!("/dev/dri/{name}"))
                .collect();
            if nodes.is_empty() {
                Err(Missing::Absent)
            } else {
                Ok(format!(
                    "{} (presence only; acceleration not tested)",
                    nodes.join(", ")
                ))
            }
        }),
    ));
    let mut debug_error = Missing::Absent;
    let mut found = false;
    for index in 0..16 {
        let path = format!("/sys/kernel/debug/dri/{index}/i915_display_info");
        match sources.read(&path) {
            Err(error) => {
                if error != Missing::Absent {
                    debug_error = error;
                }
            }
            Ok(text) => {
                found = true;
                let mut active_pipe = false;
                let mut recognized_crtc = false;
                let mut any_active = false;
                let mut count = 0;
                for line in text.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("CRTC ") || trimmed.starts_with("[CRTC:") {
                        active_pipe = false;
                    }
                    if trimmed.starts_with("uapi: enable=") {
                        recognized_crtc = true;
                        active_pipe = trimmed.contains("active=yes");
                        any_active |= active_pipe;
                    }
                    if active_pipe
                        && trimmed.starts_with("uapi: enable=")
                        && let Some(mode) = parse_mode(trimmed)
                    {
                        fields.push(field(
                            &format!("drm{index}.mode{count}"),
                            "Active display mode",
                            Ok(mode),
                        ));
                        count += 1;
                    }
                    if active_pipe
                        && trimmed.starts_with("pipe src=")
                        && let Some(bpp) = trimmed
                            .split("bpp=")
                            .nth(1)
                            .and_then(|v| v.split(|c: char| !c.is_ascii_digit()).next())
                            .and_then(|v| v.parse::<u8>().ok())
                            .filter(|v| (1..=64).contains(v))
                    {
                        fields.push(field(&format!("drm{index}.bpp"), "Active pipe color depth", Ok(format!("{bpp} bpp (pipe depth; not framebuffer format or DSI lane encoding)"))));
                    }
                    if count >= 8 {
                        break;
                    }
                }
                if count == 0 {
                    if recognized_crtc && !any_active {
                        fields.push(field(
                            &format!("drm{index}.mode"),
                            "Active display mode",
                            Ok("inactive; no active CRTC (the display may be asleep)".into()),
                        ));
                    } else {
                        fields.push(note(&format!("drm{index}.mode"), "Active display mode", "debugfs format not recognized; sysfs connector resolution does not establish refresh rate"));
                    }
                }
            }
        }
    }
    if !found {
        fields.push(field(
            "active_mode",
            "Active refresh/color (debugfs)",
            Err(debug_error),
        ));
    }
    section("display", "Display", fields)
}

fn unit_population(sources: &impl Sources) -> Result<bool, Missing> {
    let data = sources.read(&format!("{UNIT_CGROUP}/cgroup.events"))?;
    let values: Vec<_> = data
        .lines()
        .filter_map(|line| line.strip_prefix("populated "))
        .collect();
    match values.as_slice() {
        ["0"] => Ok(false),
        ["1"] => Ok(true),
        _ => Err(Missing::Invalid),
    }
}

fn record_age(sources: &impl Sources, snapshot: &Result<Value, Missing>) -> Result<u64, Missing> {
    let value = snapshot.as_ref().map_err(|e| *e)?;
    let stamp = value
        .pointer("/policy/snapshot_boottime_ms")
        .and_then(Value::as_u64)
        .ok_or(Missing::Absent)?;
    sources
        .boottime_ms()?
        .checked_sub(stamp)
        .ok_or(Missing::Invalid)
}

fn live_policy(sources: &impl Sources, snapshot: &Result<Value, Missing>) -> bool {
    unit_population(sources) == Ok(true)
        && record_age(sources, snapshot).is_ok_and(|age| age <= 90_000)
        && snapshot
            .as_ref()
            .ok()
            .and_then(|s| s.pointer("/policy/active"))
            .and_then(Value::as_bool)
            == Some(true)
}

fn metadata_field(sources: &impl Sources, path: &str, key: &str, label: &str) -> Value {
    match sources.info(path) {
        Err(error) => field(key, label, Err(error)),
        Ok(info) if !info.regular || !info.root_owned || info.mode & 0o022 != 0 => warning(
            key,
            label,
            if info.symlink {
                "symlink present; target not inspected"
            } else {
                "not a root-owned regular file protected against group/other writes"
            },
        ),
        Ok(info) => field(
            key,
            label,
            Ok(format!(
                "root-owned regular file; mode {:04o}; {} bytes; contents not read",
                info.mode, info.bytes
            )),
        ),
    }
}

fn installed_unit(sources: &impl Sources, name: &str) -> Result<String, Missing> {
    let mut last = Missing::Absent;
    for prefix in [
        "/etc/systemd/system",
        "/usr/lib/systemd/system",
        "/lib/systemd/system",
    ] {
        let path = format!("{prefix}/{name}");
        match sources.trusted(&path) {
            Ok(data) if data.lines().any(|s| s.trim() == "[Service]") => return Ok(path),
            Ok(_) => return Err(Missing::Invalid),
            Err(error) if error != Missing::Absent => {
                last = error;
                break;
            }
            Err(_) => (),
        }
    }
    Err(last)
}

fn boot_enablement(sources: &impl Sources) -> Value {
    let path = "/etc/systemd/system/multi-user.target.wants/minibookx-ctl.service";
    match sources.info(path) {
        Err(Missing::Absent) => note(
            "boot_enablement",
            "Boot enablement",
            "standard multi-user target link absent; other enablement paths are not inspected",
        ),
        Err(error) => field("boot_enablement", "Boot enablement", Err(error)),
        Ok(info) if info.symlink && info.root_owned => match sources.link_target(path) {
            Ok(target)
                if [
                    "../minibookx-ctl.service",
                    paths::UNIT_FILE,
                    "/usr/lib/systemd/system/minibookx-ctl.service",
                    "/lib/systemd/system/minibookx-ctl.service",
                ]
                .contains(&target.as_str()) =>
            {
                field(
                    "boot_enablement",
                    "Boot enablement",
                    Ok(
                        "root-owned standard multi-user target link points to the controller unit"
                            .into(),
                    ),
                )
            }
            Ok(_) => warning(
                "boot_enablement",
                "Boot enablement",
                "link target is unrecognized; target contents were not followed",
            ),
            Err(error) => field("boot_enablement", "Boot enablement", Err(error)),
        },
        Ok(_) => warning(
            "boot_enablement",
            "Boot enablement",
            "standard target entry is not a root-owned symlink",
        ),
    }
}

fn runtime(sources: &impl Sources, snapshot: &Result<Value, Missing>) -> Value {
    let mut fields = vec![
        field(
            "unit_installed",
            "Installed systemd unit",
            installed_unit(sources, paths::UNIT),
        ),
        metadata_field(
            sources,
            paths::BINARY,
            "installed_binary",
            "Installed binary",
        ),
        boot_enablement(sources),
        field(
            "process_presence",
            "Service processes",
            unit_population(sources).map(|v| {
                if v {
                    "unit cgroup contains processes (kernel observation, not systemd ActiveState)"
                        .into()
                } else {
                    "unit cgroup is empty; service is not running".into()
                }
            }),
        ),
    ];
    match record_age(sources, snapshot) {
        Ok(age) if age <= 90_000 => fields.push(field(
            "record_age",
            "Policy record age",
            Ok(format!("{:.1} s (recent)", age as f64 / 1000.)),
        )),
        Ok(age) => fields.push(warning(
            "record_age",
            "Policy record age",
            &format!(
                "{:.1} s; stale, do not treat records as live state",
                age as f64 / 1000.
            ),
        )),
        Err(Missing::Absent) => fields.push(note(
            "record_age",
            "Policy record age",
            "not supplied by this older or stopped service; freshness is unknown",
        )),
        Err(error) => fields.push(field("record_age", "Policy record age", Err(error))),
    }
    if snapshot
        .as_ref()
        .ok()
        .and_then(|v| v.pointer("/policy/active"))
        .and_then(Value::as_bool)
        == Some(true)
        && unit_population(sources) == Ok(false)
    {
        fields.push(warning("record_liveness", "Status consistency", "record says active but unit cgroup is empty; records may be left from a stopped service"));
    }
    match snapshot {
        Err(error) => fields.push(field("service", "Controller records", Err(*error))),
        Ok(value) => {
            for (path, label) in [
                ("/state", "Service state"),
                ("/active", "Service active"),
                ("/policy/active", "Policy active"),
                (
                    "/policy/config_error",
                    "Latest configuration reload rejected",
                ),
                ("/policy/paused_for_sleep", "Paused for sleep"),
                ("/policy/preset", "Fan profile"),
                ("/policy/fan_control", "Fan control requested"),
                ("/policy/cpu_c", "Policy CPU temperature °C"),
                ("/policy/ec_c", "Policy EC temperature °C"),
                ("/policy/fan_speed_ec", "EC tachometer field"),
                ("/policy/target_pwm_percent", "Requested PWM %"),
                ("/fan/actual_raw", "Applied raw fan duty"),
                ("/fan/enabled", "Broker fan control enabled"),
                ("/fan/emergency_override", "High-temperature override"),
                ("/policy/power_requested", "Power-limit control requested"),
                ("/policy/pl1_w_requested", "Requested PL1 W"),
                ("/policy/pl2_w_requested", "Requested PL2 W"),
                ("/tablet/tablet", "Tablet mode"),
                ("/tablet/angle_deg", "Estimated hinge angle °"),
                ("/tablet/reliable", "Tablet estimate reliable"),
            ] {
                if let Some(value) = value
                    .pointer(path)
                    .filter(|v| !v.is_null() && !v.is_array() && !v.is_object())
                {
                    let text = value
                        .as_str()
                        .map_or_else(|| value.to_string(), str::to_owned);
                    fields.push(field(path.trim_start_matches('/'), label, Ok(text)));
                }
            }
            if fields.is_empty() {
                fields.push(note(
                    "service",
                    "Controller service",
                    "no recognized status fields available",
                ));
            }
        }
    }
    section("service", "Controller service", fields)
}

fn accepted(snapshot: &Result<Value, Missing>) -> Result<&Value, Missing> {
    let snapshot = snapshot.as_ref().map_err(|e| *e)?;
    let value = snapshot
        .pointer("/policy/accepted_config")
        .ok_or(Missing::Absent)?;
    if value["schema_version"].as_u64() != Some(1)
        || value["source"].as_str() != Some("last-accepted-policy")
    {
        return Err(Missing::Invalid);
    }
    Ok(value)
}

fn accepted_limit(value: &Value, index: usize) -> Result<Option<i64>, Missing> {
    if value["pl1_w"]
        .as_i64()
        .zip(value["pl2_w"].as_i64())
        .is_some_and(|(a, b)| a > b)
    {
        return Err(Missing::Invalid);
    }
    let key = if index == 0 { "pl1_w" } else { "pl2_w" };
    let field = value.get(key).ok_or(Missing::Invalid)?;
    if field.is_null() {
        return Ok(None);
    }
    field
        .as_i64()
        .filter(|v| (5..=if index == 0 { 28 } else { 30 }).contains(v))
        .map(Some)
        .ok_or(Missing::Invalid)
}

fn accepted_curve(value: &Value) -> Result<String, Missing> {
    let steps = value["fan_curve"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 16)
        .ok_or(Missing::Invalid)?;
    let mut previous = (0, 0);
    let mut result = Vec::new();
    for step in steps {
        let at = step["at_mc"].as_i64().ok_or(Missing::Invalid)?;
        let pwm = step["pwm_percent"].as_i64().ok_or(Missing::Invalid)?;
        if !(20_000..=100_000).contains(&at)
            || !(35..=80).contains(&pwm)
            || at <= previous.0
            || pwm < previous.1
        {
            return Err(Missing::Invalid);
        }
        result.push(format!("{:.1}°C→{pwm}%", at as f64 / 1000.));
        previous = (at, pwm);
    }
    Ok(result.join(", "))
}

fn configuration(
    sources: &impl Sources,
    snapshot: &Result<Value, Missing>,
    expanded: bool,
) -> Value {
    let mut fields = vec![metadata_field(
        sources,
        paths::CONFIG,
        "disk_metadata",
        "Configuration file metadata",
    )];
    fields.push(note("source", "Configuration source", "values below are the worker's LAST ACCEPTED configuration; current on-disk contents are not read or compared"));
    let rejected = snapshot
        .as_ref()
        .ok()
        .and_then(|v| v.pointer("/policy/config_error"))
        .and_then(Value::as_bool);
    fields.push(match rejected {
        Some(true) => warning(
            "reload",
            "Configuration reload",
            "latest reload rejected; the previous accepted policy remains in use",
        ),
        Some(false) => field(
            "reload",
            "Configuration reload",
            Ok(
                "no rejection reported by worker; on-disk contents not independently checked"
                    .into(),
            ),
        ),
        None => field("reload", "Configuration reload", Err(Missing::Absent)),
    });
    match accepted(snapshot) {
        Err(error) => fields.push(field(
            "accepted",
            "Accepted configuration summary",
            Err(error),
        )),
        Ok(value) => {
            fields.push(field(
                "fan_control",
                "Accepted fan control",
                value
                    .get("fan_control")
                    .ok_or(Missing::Absent)
                    .and_then(|v| v.as_bool().ok_or(Missing::Invalid))
                    .map(|v| v.to_string()),
            ));
            fields.push(field(
                "preset",
                "Accepted profile",
                value["preset"]
                    .as_str()
                    .filter(|v| ["quiet", "balance", "performance", "custom"].contains(v))
                    .map(str::to_owned)
                    .ok_or(Missing::Invalid),
            ));
            fields.push(field(
                "fan_curve",
                "Accepted fan curve (PWM, not RPM)",
                accepted_curve(value),
            ));
            fields.push(field(
                "power_control",
                "Accepted power control",
                value["power_control"]
                    .as_bool()
                    .map(|v| v.to_string())
                    .ok_or(Missing::Invalid),
            ));
            for index in 0..2 {
                fields.push(field(
                    if index == 0 { "pl1_w" } else { "pl2_w" },
                    if index == 0 {
                        "Accepted PL1"
                    } else {
                        "Accepted PL2"
                    },
                    accepted_limit(value, index).map(|v| {
                        v.map_or("saved pre-override RAPL baseline".into(), |v| {
                            format!("{v} W")
                        })
                    }),
                ));
            }
            if expanded {
                for (key, label, max) in [
                    ("hysteresis_mc", "Accepted hysteresis m°C", 15_000),
                    ("emergency_mc", "Accepted emergency threshold m°C", 95_000),
                    ("start_hold_ms", "Accepted start hold ms", 120_000),
                    ("up_hold_ms", "Accepted speed-increase hold ms", 120_000),
                    ("down_hold_ms", "Accepted cooling hold ms", 120_000),
                    ("min_on_ms", "Accepted minimum run ms", 300_000),
                    ("idle_interval_ms", "Accepted idle sampling ms", 10_000),
                    ("active_interval_ms", "Accepted active sampling ms", 10_000),
                ] {
                    fields.push(field(
                        key,
                        label,
                        value[key]
                            .as_u64()
                            .filter(|v| *v <= max)
                            .map(|v| v.to_string())
                            .ok_or(Missing::Invalid),
                    ));
                }
            }
        }
    }
    section("configuration", "Last accepted configuration", fields)
}

fn rapl_read(sources: &impl Sources, path: &Path) -> Result<String, rapl::ReadError> {
    sources
        .read(path.to_str().ok_or(rapl::ReadError::Invalid)?)
        .map_err(|error| match error {
            Missing::Absent => rapl::ReadError::Absent,
            Missing::Permission => rapl::ReadError::Permission,
            Missing::Invalid => rapl::ReadError::Invalid,
            Missing::Other => rapl::ReadError::Other,
        })
}

fn rapl_error(error: rapl::ReadError) -> Missing {
    match error {
        rapl::ReadError::Absent => Missing::Absent,
        rapl::ReadError::Permission => Missing::Permission,
        rapl::ReadError::Invalid => Missing::Invalid,
        rapl::ReadError::Other => Missing::Other,
    }
}

#[cfg(test)]
fn rapl_limits(sources: &impl Sources) -> [Result<i64, Missing>; 2] {
    rapl::observe(Path::new(paths::RAPL_PACKAGE), &mut |path| {
        rapl_read(sources, path)
    })
    .limits_uw
    .map(|value| value.map(|v| v as i64).map_err(rapl_error))
}

fn broker_power(snapshot: &Result<Value, Missing>) -> Result<&Value, Missing> {
    let value = snapshot
        .as_ref()
        .map_err(|e| *e)?
        .get("power")
        .ok_or(Missing::Absent)?;
    if value.is_null() {
        return Err(Missing::Absent);
    }
    if value["schema_version"].as_u64() != Some(1)
        || value["source"].as_str() != Some("broker-power-state")
    {
        return Err(Missing::Invalid);
    }
    Ok(value)
}

fn broker_pair(snapshot: &Result<Value, Missing>, key: &str) -> Result<[u64; 2], Missing> {
    let value = broker_power(snapshot)?.get(key).ok_or(Missing::Invalid)?;
    if value.is_null() {
        return Err(Missing::Absent);
    }
    let values = value
        .as_array()
        .filter(|values| values.len() == 2)
        .ok_or(Missing::Invalid)?;
    let pair = [
        values[0].as_u64().ok_or(Missing::Invalid)?,
        values[1].as_u64().ok_or(Missing::Invalid)?,
    ];
    if !(5_000_000..=28_000_000).contains(&pair[0]) || pair[1] < pair[0] || pair[1] > 30_000_000 {
        return Err(Missing::Invalid);
    }
    Ok(pair)
}

fn power(sources: &impl Sources, snapshot: &Result<Value, Missing>) -> Value {
    let observed = rapl::observe(Path::new(paths::RAPL_PACKAGE), &mut |path| {
        rapl_read(sources, path)
    });
    let mut fields = Vec::new();
    let baseline = broker_pair(snapshot, "baseline_uw");
    let last = broker_pair(snapshot, "last_applied_uw");
    fields.push(match accepted(snapshot).and_then(|value| value["power_control"].as_bool().ok_or(Missing::Invalid)) {
        Ok(false) => field("ownership", "Power-limit policy", Ok("power control disabled; firmware/system baseline preserved by the controller".into())),
        Ok(true) => field("ownership", "Power-limit policy", Ok("optional controller override requested; compare accepted policy, broker readback and current RAPL separately".into())),
        Err(error) => field("ownership", "Power-limit policy", Err(error)),
    });
    for (index, current) in observed.limits_uw.iter().enumerate() {
        let label = if index == 0 { "PL1" } else { "PL2" };
        let actual = current.map_err(rapl_error);
        fields.push(field(
            &format!("actual_{label}"),
            &format!("Current RAPL {label}"),
            actual.map(|v| format!("{:.3} W", v as f64 / 1_000_000.)),
        ));
        fields.push(field(
            &format!("baseline_{label}"),
            &format!("Saved pre-override {label}"),
            baseline.map(|v| {
                format!(
                    "{:.3} W (captured RAPL; not BIOS Setup)",
                    v[index] as f64 / 1_000_000.
                )
            }),
        ));
        fields.push(field(
            &format!("broker_{label}"),
            &format!("Broker last readback {label}"),
            last.map(|v| {
                format!(
                    "{:.3} W (not a fresh reading)",
                    v[index] as f64 / 1_000_000.
                )
            }),
        ));
        fields.push(field(
            &format!("window_{label}"),
            &format!("Current {label} time window"),
            observed.windows_us[index]
                .map(|v| format!("{v} µs"))
                .map_err(rapl_error),
        ));
        let comparison = match accepted(snapshot) {
            Err(error) => field(
                &format!("compare_{label}"),
                &format!("{label} numeric readback"),
                Err(error),
            ),
            Ok(value) if value["power_control"].as_bool() == Some(false) => field(
                &format!("compare_{label}"),
                &format!("{label} numeric readback"),
                Ok("power control disabled; firmware or external policy owns these limits".into()),
            ),
            Ok(value) if value["power_control"].as_bool() != Some(true) => field(
                &format!("compare_{label}"),
                &format!("{label} numeric readback"),
                Err(Missing::Invalid),
            ),
            Ok(_)
                if !live_policy(sources, snapshot)
                    || snapshot
                        .as_ref()
                        .ok()
                        .and_then(|v| v.pointer("/policy/paused_for_sleep"))
                        .and_then(Value::as_bool)
                        != Some(false) =>
            {
                note(
                    &format!("compare_{label}"),
                    &format!("{label} numeric readback"),
                    "comparison unavailable: need recent active policy, populated service cgroup and no sleep pause",
                )
            }
            Ok(value) => {
                let requested = accepted_limit(value, index).and_then(|value| match value {
                    Some(watts) => Ok(watts as u64 * 1_000_000),
                    None => baseline.map(|pair| pair[index]),
                });
                match (requested, actual) {
                    (Ok(expected), Ok(actual)) if actual.abs_diff(expected) <= 125_000 => field(
                        &format!("compare_{label}"),
                        &format!("{label} numeric readback"),
                        Ok(format!(
                            "matches accepted {:.3} W within 0.125 W; numeric equality only",
                            expected as f64 / 1_000_000.
                        )),
                    ),
                    (Ok(expected), Ok(actual)) => warning(
                        &format!("compare_{label}"),
                        &format!("{label} numeric readback"),
                        &format!(
                            "observed {:.3} W differs from accepted {:.3} W; inspect other power writers and service logs",
                            actual as f64 / 1_000_000.,
                            expected as f64 / 1_000_000.
                        ),
                    ),
                    (Err(Missing::Absent), _) if accepted_limit(value, index) == Ok(None) => note(
                        &format!("compare_{label}"),
                        &format!("{label} numeric readback"),
                        "saved RAPL baseline unavailable; cannot compare the firmware choice",
                    ),
                    (Err(error), _) | (_, Err(error)) => field(
                        &format!("compare_{label}"),
                        &format!("{label} numeric readback"),
                        Err(error),
                    ),
                }
            }
        };
        fields.push(comparison);
    }
    fields.push(field(
        "package_enabled",
        "Package powercap enabled",
        observed
            .package_enabled
            .map(|v| v.to_string())
            .map_err(rapl_error),
    ));
    for key in ["restoration_pending", "uncertain"] {
        fields.push(if !live_policy(sources, snapshot) {
            note(key, if key == "uncertain" { "Broker write state" } else { "Broker restoration state" }, "no recent live policy confirmed; saved broker record cannot establish current override or restoration state")
        } else if key == "uncertain" && broker_power(snapshot).ok().and_then(|value| value[key].as_bool()) == Some(true) {
            warning(key, "Broker write state uncertain", "write or restoration could not be confirmed; inspect journalctl -u minibookx-ctl and preserve recovery records")
        } else {
            field(key, if key == "uncertain" { "Broker write state uncertain" } else { "Broker restoration pending" }, broker_power(snapshot).and_then(|value| value[key].as_bool().map(|v| v.to_string()).ok_or(Missing::Invalid)))
        });
    }
    fields.push(note("broker_record_scope", "Broker record scope", "saved baseline and last readback describe the broker's last published record. Recovery after process loss may happen later; use live status and the service journal to establish current ownership"));
    fields.push(note("enforcement", "RAPL verification limits", "numeric readback is not proof of hardware enforcement. Package enabled is not independent PL1/PL2 enable status; per-limit enable and firmware lock are unknown through this report's powercap ABI"));
    section("power", "Observed package power limits", fields)
}

fn integrations(sources: &impl Sources, snapshot: &Result<Value, Missing>) -> Value {
    let mut fields = Vec::new();
    let mut switch = Err(Missing::Absent);
    if let Ok(entries) = sources.entries("/sys/class/input") {
        for name in entries.iter().filter(|v| v.starts_with("input")).take(64) {
            let root = format!("/sys/class/input/{name}");
            if text(sources, &format!("{root}/name")).as_deref() == Ok(crate::tablet::SWITCH_NAME) {
                switch = sources.read(&format!("{root}/capabilities/sw")).and_then(|v| {
                    let bits = v.split_whitespace().last().and_then(|v| u64::from_str_radix(v, 16).ok()).ok_or(Missing::Invalid)?;
                    if bits & 2 != 0 { Ok("virtual SW_TABLET_MODE device present; desktop consumption not verified".into()) } else { Err(Missing::Invalid) }
                });
                break;
            }
        }
    }
    fields.push(field("tablet_switch", "Tablet input switch", switch));
    let tablet = snapshot
        .as_ref()
        .ok()
        .and_then(|v| v.pointer("/tablet/tablet"))
        .and_then(Value::as_bool);
    fields.push(field(
        "tablet_record",
        "Broker tablet state",
        tablet
            .map(|v| format!("{v} (reported state; not a fresh physical-posture test)"))
            .ok_or(Missing::Absent),
    ));
    fields.push(field(
        "sensor_proxy",
        "iio-sensor-proxy unit",
        installed_unit(sources, "iio-sensor-proxy.service"),
    ));
    fields.push(note("desktop", "Desktop integration", "GNOME/KDE auto-rotation, input inhibition and on-screen keyboard behavior are not probed; switch presence alone does not prove desktop behavior"));
    section("tablet", "Tablet integration", fields)
}

fn display_maintenance(sources: &impl Sources) -> Vec<Value> {
    vec![
        field(
            "loaded_module_version",
            "Loaded i915 module version",
            text(sources, "/sys/module/i915/version"),
        ),
        note(
            "manual_display_setup",
            "Display setup",
            "Driver patches, boot entries and refresh overrides are maintained manually; this report does not verify their update hooks",
        ),
    ]
}

#[cfg(test)]
fn collect(sources: &impl Sources) -> Value {
    collect_mode(sources, false)
}

fn collect_mode(sources: &impl Sources, expanded: bool) -> Value {
    let snapshot = sources.runtime();
    let mut display = display(sources);
    display["fields"]
        .as_array_mut()
        .unwrap()
        .extend(display_maintenance(sources));
    let mut sections = vec![
        runtime(sources, &snapshot),
        configuration(sources, &snapshot, expanded),
        fan(sources, &snapshot),
        power(sources, &snapshot),
        display,
        graphics::collect(sources),
        integrations(sources, &snapshot),
        system(sources),
    ];
    if expanded {
        sections.extend([
            embedded_controller(sources),
            batteries(sources),
            sensors(sources),
        ]);
    } else {
        for section in &mut sections {
            concise(section);
        }
    }
    json!({
        "schema_version": 2,
        "mode": if expanded { "debug" } else { "doctor" },
        "binary_version": build_info::VERSION,
        "read_only": true,
        "sections": sections,
        "notes": [
            "No hardware writes, raw EC/I2C probes, module loads or configuration changes were made.",
            "Unavailable fields are expected on some kernels. Use sudo only to read fields marked permission-denied.",
            "Serial numbers, MAC addresses, user names and the kernel command line are not collected. Review the report before sharing it."
        ]
    })
}

/// Keep the normal report focused on decisions. Detailed metadata, raw status
/// mirrors and installed package versions remain available through debug.
fn concise(section: &mut Value) {
    let name = section["key"].as_str().unwrap_or_default().to_owned();
    let Some(fields) = section["fields"].as_array_mut() else {
        return;
    };
    fields.retain(|field| {
        let key = field["key"].as_str().unwrap_or_default();
        if field["state"] == "warning" || field["state"] == "invalid" {
            return true;
        }
        match name.as_str() {
            "service" => [
                "unit_installed",
                "boot_enablement",
                "process_presence",
                "record_age",
                "record_liveness",
                "service",
                "state",
                "policy/paused_for_sleep",
                "policy/fan_control",
                "policy/cpu_c",
                "fan/enabled",
                "fan/actual_raw",
            ]
            .contains(&key),
            "configuration" => [
                "source",
                "reload",
                "accepted",
                "preset",
                "fan_control",
                "fan_curve",
                "power_control",
            ]
            .contains(&key),
            "fan" => ["control", "ec_sys", "lockdown"].contains(&key),
            "power" => [
                "ownership",
                "actual_PL1",
                "actual_PL2",
                "compare_PL1",
                "compare_PL2",
                "package_enabled",
                "enforcement",
            ]
            .contains(&key),
            "display" => {
                key.ends_with(".status")
                    || key.contains(".mode")
                    || key.ends_with(".bpp")
                    || ["connectors", "active_mode", "render_nodes"].contains(&key)
            }
            "graphics" => {
                key.ends_with(".driver")
                    || [
                        "gpu",
                        "distribution",
                        "package_profile",
                        "package_inventory",
                        "package_status",
                        "acceleration",
                    ]
                    .contains(&key)
            }
            "tablet" => ["tablet_switch", "tablet_record", "sensor_proxy"].contains(&key),
            "system" => ["distribution", "hardware_profile"].contains(&key),
            _ => true,
        }
    });
    let action = match name.as_str() {
        "service" => Some((
            "service_action",
            "Service help",
            "if startup failed, run systemctl status minibookx-ctl and journalctl -u minibookx-ctl; see packaging/README.md",
        )),
        "graphics" => Some((
            "graphics_action",
            "Graphics help",
            "graphics/media packages are optional to the controller. See docs/distributions.md for manual setup; debug includes installed versions and codec advice",
        )),
        _ => None,
    };
    if let Some((key, label, message)) = action {
        fields.push(note(key, label, message));
    }
}

fn human(report: &Value, writer: &mut impl Write) -> io::Result<()> {
    writeln!(
        writer,
        "minibookx-ctl {} — {}",
        build_info::VERSION,
        report["mode"].as_str().unwrap_or("doctor")
    )?;
    writeln!(writer, "Read-only hardware and installation report")?;
    if let Some(sections) = report["sections"].as_array() {
        for section in sections {
            writeln!(
                writer,
                "\n{}",
                section["title"].as_str().unwrap_or("Diagnostics")
            )?;
            if let Some(fields) = section["fields"].as_array() {
                for field in fields {
                    let value = field["value"]
                        .as_str()
                        .or_else(|| field["message"].as_str())
                        .unwrap_or("unknown");
                    let marker = if field["state"] == "warning" {
                        "[!] "
                    } else {
                        ""
                    };
                    writeln!(
                        writer,
                        "  {marker}{}: {}",
                        clean(field["label"].as_str().unwrap_or("Field")),
                        clean(value)
                    )?;
                }
            }
        }
    }
    if let Some(notes) = report["notes"].as_array() {
        writeln!(writer)?;
        for note in notes {
            writeln!(writer, "{}", note.as_str().unwrap_or_default())?;
        }
    }
    Ok(())
}

pub fn print(json_output: bool) -> io::Result<()> {
    print_mode(json_output, false)
}

pub fn print_debug(json_output: bool) -> io::Result<()> {
    print_mode(json_output, true)
}

fn print_mode(json_output: bool, expanded: bool) -> io::Result<()> {
    let report = collect_mode(&System, expanded);
    let mut output = io::stdout().lock();
    if json_output {
        serde_json::to_writer_pretty(&mut output, &report).map_err(io::Error::other)?;
        writeln!(output)
    } else {
        human(&report, &mut output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::BTreeMap};
    #[derive(Default)]
    struct Fixture {
        files: BTreeMap<String, Result<String, Missing>>,
        directories: BTreeMap<String, Vec<String>>,
        infos: BTreeMap<String, FileInfo>,
        links: BTreeMap<String, String>,
        snapshot: Option<Result<Value, Missing>>,
        clock_ms: Option<u64>,
        reads: RefCell<Vec<String>>,
    }
    impl Sources for Fixture {
        fn read(&self, path: &str) -> Result<String, Missing> {
            self.reads.borrow_mut().push(path.to_owned());
            self.files
                .get(path)
                .cloned()
                .unwrap_or(Err(Missing::Absent))
        }
        fn entries(&self, path: &str) -> Result<Vec<String>, Missing> {
            self.directories.get(path).cloned().ok_or(Missing::Absent)
        }
        fn runtime(&self) -> Result<Value, Missing> {
            self.snapshot
                .clone()
                .unwrap_or_else(|| Ok(json!({"active":false,"state":"not-running"})))
        }
        fn info(&self, path: &str) -> Result<FileInfo, Missing> {
            self.infos.get(path).cloned().ok_or(Missing::Absent)
        }
        fn boottime_ms(&self) -> Result<u64, Missing> {
            self.clock_ms.ok_or(Missing::Absent)
        }
        fn link_target(&self, path: &str) -> Result<String, Missing> {
            self.links.get(path).cloned().ok_or(Missing::Absent)
        }
    }
    impl Fixture {
        fn put(&mut self, path: &str, value: &str) {
            self.files.insert(path.into(), Ok(value.into()));
        }
    }
    fn policy_snapshot(stamp: u64) -> Value {
        json!({"policy": {
            "active": true, "paused_for_sleep": false, "config_error": false,
            "preset": "quiet", "snapshot_boottime_ms": stamp,
            "accepted_config": {"schema_version": 1, "source": "last-accepted-policy", "preset": "quiet",
                "fan_curve": [{"at_mc":80000,"pwm_percent":35},{"at_mc":88000,"pwm_percent":40}],
                "power_control": true, "pl1_w": 12, "pl2_w": 15}
        }, "tablet": {"tablet": false, "reliable": true}})
    }
    #[test]
    fn fan_disabled_is_distinct_from_unavailable_ec_and_does_not_probe_hardware() {
        let mut fixture = Fixture::default();
        let mut snapshot = policy_snapshot(10_000);
        snapshot["policy"]["accepted_config"]["fan_control"] = json!(false);
        fixture.put(
            "/sys/kernel/security/lockdown",
            "none [integrity] confidentiality\n",
        );
        let report = fan(&fixture, &Ok(snapshot));
        let row = |key: &str| {
            report["fields"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["key"] == key)
                .unwrap()
        };
        assert_eq!(row("control")["state"], "ok");
        assert!(
            row("control")["value"]
                .as_str()
                .unwrap()
                .starts_with("disabled;")
        );
        assert_eq!(row("ec_sys")["state"], "unavailable");
        assert!(
            report["fields"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v["state"] != "warning")
        );
        assert!(!fixture.reads.borrow().iter().any(|path| {
            [hardware::EC_PATH, hardware::EC_ALIAS_PATH, paths::CONFIG].contains(&path.as_str())
        }));
    }

    #[test]
    fn lockdown_advice_depends_on_known_fan_policy_and_preserves_unknown_older_records() {
        let mut fixture = Fixture {
            clock_ms: Some(12_000),
            ..Fixture::default()
        };
        fixture.put(&format!("{UNIT_CGROUP}/cgroup.events"), "populated 1\n");
        fixture.put(
            "/sys/kernel/security/lockdown",
            "none [integrity] confidentiality\n",
        );
        fixture.put("/sys/module/ec_sys/parameters/write_support", "Y\n");
        let mut snapshot = policy_snapshot(10_000);
        let old = fan(&fixture, &Ok(snapshot.clone()));
        assert_eq!(old["fields"][0]["state"], "unavailable");
        snapshot["policy"]["accepted_config"]["fan_control"] = json!(true);
        let report = fan(&fixture, &Ok(snapshot.clone()));
        assert!(report.to_string().contains("built-in or loaded"));
        assert!(
            report["fields"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["key"] == "ec_access" && v["state"] == "warning")
        );
        snapshot["policy"]["accepted_config"]["fan_control"] = json!("false");
        assert_eq!(
            fan(&fixture, &Ok(snapshot))["fields"][0]["state"],
            "invalid"
        );
        for malformed in [
            "none integrity confidentiality",
            "[none] [integrity] confidentiality",
            "[other]",
        ] {
            fixture.put("/sys/kernel/security/lockdown", malformed);
            assert_eq!(lockdown_mode(&fixture), Err(Missing::Invalid));
        }
    }

    #[test]
    fn installed_sensor_proxy_is_recognized_in_each_system_unit_directory() {
        for prefix in [
            "/etc/systemd/system",
            "/usr/lib/systemd/system",
            "/lib/systemd/system",
        ] {
            let mut fixture = Fixture::default();
            fixture.put(
                &format!("{prefix}/iio-sensor-proxy.service"),
                "[Service]\nExecStart=/usr/libexec/iio-sensor-proxy\n",
            );
            let report = integrations(&fixture, &fixture.runtime());
            let probe = report["fields"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["key"] == "sensor_proxy")
                .unwrap();
            assert_eq!(probe["state"], "ok", "{prefix}: {probe}");
            assert_eq!(probe["value"], format!("{prefix}/iio-sensor-proxy.service"));
        }
    }
    fn rapl_fixture(fixture: &mut Fixture, pl1: &str, pl2: &str) {
        let root = "/sys/class/powercap/intel-rapl:0";
        fixture.put(&format!("{root}/name"), "package-0");
        // Deliberately reverse the conventional index order.
        fixture.put(&format!("{root}/constraint_0_name"), "short_term");
        fixture.put(&format!("{root}/constraint_0_power_limit_uw"), pl2);
        fixture.put(&format!("{root}/constraint_1_name"), "long_term");
        fixture.put(&format!("{root}/constraint_1_power_limit_uw"), pl1);
    }
    #[test]
    fn accepted_policy_is_not_confused_with_disk_contents_or_stale_process_records() {
        let mut fixture = Fixture::default();
        let mut snapshot = policy_snapshot(10_000);
        snapshot["policy"]["config_error"] = json!(true);
        snapshot["policy"]["config_path"] = json!("/root/never-read-this");
        fixture.snapshot = Some(Ok(snapshot));
        fixture.clock_ms = Some(12_000);
        fixture.put(
            &format!("{UNIT_CGROUP}/cgroup.events"),
            "populated 0\nfrozen 0\n",
        );
        fixture.put(
            "/etc/minibookx-ctl/config.conf",
            "preset = performance\nprivate fixture",
        );
        rapl_fixture(&mut fixture, "12000000", "15000000");
        let report = collect(&fixture);
        let output = report.to_string();
        assert!(output.contains("latest reload rejected"));
        assert!(output.contains("LAST ACCEPTED"));
        assert!(output.contains("records may be left from a stopped service"));
        assert!(!output.contains("matches accepted 12.000 W"));
        assert!(!output.contains("private fixture"));
        assert!(
            !fixture
                .reads
                .borrow()
                .iter()
                .any(|p| p == "/etc/minibookx-ctl/config.conf" || p == "/root/never-read-this")
        );
    }
    #[test]
    fn numeric_power_comparison_requires_fresh_live_policy_and_named_constraints() {
        let mut fixture = Fixture {
            snapshot: Some(Ok(policy_snapshot(10_000))),
            clock_ms: Some(12_000),
            ..Fixture::default()
        };
        fixture.put(
            &format!("{UNIT_CGROUP}/cgroup.events"),
            "populated 1\nfrozen 0\n",
        );
        rapl_fixture(&mut fixture, "12000000", "28000000");
        let report = power(&fixture, &fixture.runtime()).to_string();
        assert!(report.contains("matches accepted 12.000 W"));
        assert!(report.contains("28.000 W differs from accepted 15.000 W"));
        fixture.clock_ms = Some(101_000);
        let stale = power(&fixture, &fixture.runtime()).to_string();
        assert!(!stale.contains("matches accepted"));
        assert!(
            runtime(&fixture, &fixture.runtime())
                .to_string()
                .contains("stale")
        );
        fixture.clock_ms = Some(1);
        assert_eq!(
            record_age(&fixture, &fixture.runtime()),
            Err(Missing::Invalid)
        );
    }
    #[test]
    fn paused_inactive_or_unobservable_services_do_not_certify_applied_limits() {
        for state in ["paused", "inactive", "permission", "ambiguous-cgroup"] {
            let mut snapshot = policy_snapshot(10_000);
            match state {
                "paused" => snapshot["policy"]["paused_for_sleep"] = json!(true),
                "inactive" => snapshot["policy"]["active"] = json!(false),
                _ => (),
            }
            let mut fixture = Fixture {
                snapshot: Some(Ok(snapshot)),
                clock_ms: Some(12_000),
                ..Fixture::default()
            };
            let events = format!("{UNIT_CGROUP}/cgroup.events");
            fixture.put(&events, "populated 1\nfrozen 0\n");
            if state == "permission" {
                fixture.files.insert(events, Err(Missing::Permission));
            } else if state == "ambiguous-cgroup" {
                fixture.put(&events, "populated 0\npopulated 1\n");
            }
            // Matching numbers alone are not evidence of an active override.
            rapl_fixture(&mut fixture, "12000000", "15000000");
            let report = power(&fixture, &fixture.runtime()).to_string();
            assert!(
                report.contains("comparison unavailable"),
                "{state}: {report}"
            );
            assert!(!report.contains("matches accepted"), "{state}: {report}");
            assert!(
                !report.contains("differs from accepted"),
                "{state}: {report}"
            );
        }
    }
    #[test]
    fn disabled_power_policy_and_duplicate_constraint_names_remain_distinct() {
        let mut fixture = Fixture {
            snapshot: Some(Ok(policy_snapshot(10_000))),
            clock_ms: Some(12_000),
            ..Fixture::default()
        };
        fixture.put(&format!("{UNIT_CGROUP}/cgroup.events"), "populated 1\n");
        rapl_fixture(&mut fixture, "12000000", "15000000");
        fixture.put(
            "/sys/class/powercap/intel-rapl:0/constraint_2_name",
            "long_term",
        );
        fixture.put(
            "/sys/class/powercap/intel-rapl:0/constraint_2_power_limit_uw",
            "12000000",
        );
        let section = power(&fixture, &fixture.runtime());
        let fields = section["fields"].as_array().unwrap();
        let field = |key: &str| fields.iter().find(|value| value["key"] == key).unwrap();
        assert_eq!(field("actual_PL1")["state"], "invalid");
        assert_eq!(field("compare_PL1")["state"], "invalid");
        // The shared recognizer rejects an ambiguous package for both paths.
        assert_eq!(field("compare_PL2")["state"], "invalid");

        let mut snapshot = policy_snapshot(10_000);
        snapshot["policy"]["accepted_config"]["power_control"] = json!(false);
        fixture.snapshot = Some(Ok(snapshot));
        let disabled = power(&fixture, &fixture.runtime()).to_string();
        assert!(disabled.contains("power control disabled"));
        assert!(!disabled.contains("matches accepted"));
        assert!(!disabled.contains("differs from accepted"));
    }
    #[test]
    fn unreadable_optional_constraint_name_is_not_mistaken_for_absence() {
        for error in [Missing::Permission, Missing::Invalid, Missing::Other] {
            let mut fixture = Fixture {
                snapshot: Some(Ok(policy_snapshot(10_000))),
                clock_ms: Some(12_000),
                ..Fixture::default()
            };
            fixture.put(&format!("{UNIT_CGROUP}/cgroup.events"), "populated 1\n");
            rapl_fixture(&mut fixture, "12000000", "15000000");
            fixture.files.insert(
                "/sys/class/powercap/intel-rapl:0/constraint_2_name".into(),
                Err(error),
            );
            assert_eq!(rapl_limits(&fixture), [Err(error); 2]);
            let report = power(&fixture, &fixture.runtime()).to_string();
            assert!(!report.contains("matches accepted"));
            assert!(!report.contains("differs from accepted"));
            assert!(report.contains(error.code()));
        }
    }
    #[test]
    fn firmware_limits_and_older_records_are_unknown_instead_of_false_matches() {
        let mut fixture = Fixture::default();
        let mut snapshot = policy_snapshot(10_000);
        snapshot["policy"]["accepted_config"]["pl1_w"] = Value::Null;
        fixture.snapshot = Some(Ok(snapshot));
        fixture.clock_ms = Some(12_000);
        fixture.put(&format!("{UNIT_CGROUP}/cgroup.events"), "populated 1\n");
        rapl_fixture(&mut fixture, "12000000", "15000000");
        assert!(
            power(&fixture, &fixture.runtime())
                .to_string()
                .contains("saved RAPL baseline unavailable")
        );
        fixture.snapshot = Some(Ok(json!({"policy":{"active":true,"preset":"quiet"}})));
        let report = collect(&fixture).to_string();
        assert!(report.contains("freshness is unknown"));
        assert!(!report.contains("matches accepted"));
        assert!(!report.contains("\"state\":\"invalid\""));
    }
    #[test]
    fn saved_baseline_current_readback_and_abi_limits_are_distinct() {
        let mut snapshot = policy_snapshot(10_000);
        snapshot["policy"]["accepted_config"]["pl1_w"] = Value::Null;
        snapshot["power"] = json!({"schema_version":1, "source":"broker-power-state",
            "baseline_uw":[12_000_000, 28_000_000], "last_applied_uw":[12_000_000, 15_000_000],
            "restoration_pending":true, "uncertain":false});
        let mut fixture = Fixture {
            snapshot: Some(Ok(snapshot)),
            clock_ms: Some(12_000),
            ..Fixture::default()
        };
        fixture.put(&format!("{UNIT_CGROUP}/cgroup.events"), "populated 1\n");
        rapl_fixture(&mut fixture, "12000000", "15000000");
        fixture.put(&format!("{}/enabled", paths::RAPL_PACKAGE), "0");
        fixture.put(
            &format!("{}/constraint_1_time_window_us", paths::RAPL_PACKAGE),
            "28000000",
        );
        let report = power(&fixture, &fixture.runtime());
        let fields = report["fields"].as_array().unwrap();
        let row = |key| fields.iter().find(|value| value["key"] == key).unwrap();
        assert!(
            row("baseline_PL2")["value"]
                .as_str()
                .unwrap()
                .contains("28.000 W")
        );
        assert!(
            row("broker_PL2")["value"]
                .as_str()
                .unwrap()
                .contains("15.000 W")
        );
        assert!(
            row("actual_PL2")["value"]
                .as_str()
                .unwrap()
                .contains("15.000 W")
        );
        assert!(
            row("compare_PL1")["value"]
                .as_str()
                .unwrap()
                .contains("numeric equality only")
        );
        assert_eq!(row("package_enabled")["value"], "false");
        assert_eq!(row("window_PL1")["value"], "28000000 µs");
        assert_eq!(row("window_PL2")["state"], "unavailable");
        assert!(
            row("enforcement")["message"]
                .as_str()
                .unwrap()
                .contains("firmware lock are unknown")
        );
        // The report only uses fixed sysfs metadata; no lock/MSR probing.
        assert!(
            !fixture
                .reads
                .borrow()
                .iter()
                .any(|path| path.contains("msr") || path.contains("lock"))
        );
        fixture.snapshot.as_mut().unwrap().as_mut().unwrap()["power"]["baseline_uw"] =
            json!([0, u64::MAX]);
        assert_eq!(
            broker_pair(&fixture.runtime(), "baseline_uw"),
            Err(Missing::Invalid)
        );
    }

    #[test]
    fn doctor_is_concise_while_debug_keeps_detailed_evidence() {
        let fixture = Fixture::default();
        let doctor = collect_mode(&fixture, false);
        let debug = collect_mode(&fixture, true);
        let count = |value: &Value| {
            value["sections"]
                .as_array()
                .unwrap()
                .iter()
                .map(|section| section["fields"].as_array().unwrap().len())
                .sum::<usize>()
        };
        assert!(
            count(&doctor) < count(&debug) / 2,
            "{} vs {}",
            count(&doctor),
            count(&debug)
        );
        assert!(doctor.to_string().contains("Service help"));
        assert!(!doctor.to_string().contains("Broker last readback PL1"));
        assert!(debug.to_string().contains("Broker last readback PL1"));
        assert!(
            doctor
                .to_string()
                .contains("graphics/media packages are optional")
        );
    }
    #[test]
    fn malformed_status_and_custom_enablement_targets_are_never_followed() {
        let mut fixture = Fixture::default();
        let mut snapshot = policy_snapshot(10_000);
        snapshot["policy"]["accepted_config"]["fan_curve"] =
            json!([{"at_mc":80000,"pwm_percent":255}]);
        snapshot["policy"]["accepted_config"]["pl1_w"] = json!(i64::MAX);
        fixture.snapshot = Some(Ok(snapshot));
        let path = "/etc/systemd/system/multi-user.target.wants/minibookx-ctl.service";
        fixture.infos.insert(
            path.into(),
            FileInfo {
                regular: false,
                symlink: true,
                root_owned: true,
                mode: 0o777,
                bytes: 30,
            },
        );
        fixture
            .links
            .insert(path.into(), "/root/private-target".into());
        assert!(
            configuration(&fixture, &fixture.runtime(), false)
                .to_string()
                .contains("invalid")
        );
        assert!(
            boot_enablement(&fixture)
                .to_string()
                .contains("target is unrecognized")
        );
        assert!(
            !fixture
                .reads
                .borrow()
                .iter()
                .any(|p| p == "/root/private-target")
        );
    }
    #[test]
    fn display_diagnostics_do_not_read_boot_configuration_or_legacy_hook_files() {
        let fixture = Fixture::default();
        let report = json!(display_maintenance(&fixture)).to_string();
        assert!(report.contains("maintained manually"));
        assert!(
            fixture
                .reads
                .borrow()
                .iter()
                .all(|path| path == "/sys/module/i915/version")
        );
    }
    #[test]
    fn missing_and_permission_denied_fields_do_not_abort_report() {
        let mut fixture = Fixture::default();
        fixture.files.insert(
            "/sys/class/dmi/id/bios_version".into(),
            Err(Missing::Permission),
        );
        fixture.files.insert(
            "/sys/kernel/debug/dri/0/i915_display_info".into(),
            Err(Missing::Permission),
        );
        let report = collect_mode(&fixture, true);
        assert_eq!(report["read_only"], true);
        assert_eq!(report["sections"].as_array().unwrap().len(), 11);
        assert!(report.to_string().contains("permission-denied"));
        assert!(report.to_string().contains("not-running"));
        assert!(
            !report
                .to_string()
                .contains("ITE IT5570, revision 02 (detected)")
        );
    }
    #[test]
    fn modes_use_actual_pixel_clock_and_ignore_disabled_pipe() {
        let mut fixture = Fixture::default();
        fixture.put("/sys/kernel/debug/dri/0/i915_display_info", "CRTC 80: pipe A\n uapi: enable=yes, active=yes, mode=\"1200x1920\": 50 138267 1200 1240 1256 1272 1920 2140 2142 2174 0x48 0x5\n pipe src=1200x1920+0+0, dither=no, bpp=24\nCRTC 81: pipe B\n uapi: enable=no, active=no, mode=\"999x999\": 60 9999 999 1 2 1000 999 1 2 1000\n");
        let report = display(&fixture).to_string();
        assert!(report.contains("1200×1920 @ 50.000 Hz"));
        assert!(report.contains("24 bpp"));
        assert!(!report.contains("999×999"));
        assert!(parse_mode("mode=\"x\": 60 1 1 1 1 0 1 1 1 0").is_none());
        assert!(parse_mode("mode=\"x\": nonsense").is_none());
    }
    #[test]
    fn sleeping_display_never_uses_advertised_modes_as_active_refresh() {
        let mut fixture = Fixture::default();
        fixture.put("/sys/kernel/debug/dri/0/i915_display_info", "[CRTC:151:pipe A]:\n uapi: enable=no, active=no, mode=\"\": 0 0 0 0 0 0 0 0 0 0 0x0 0x0\n pipe src=0x0+0+0, dither=no, bpp=0\n");
        let value = display(&fixture).to_string();
        assert!(value.contains("inactive; no active CRTC"));
        assert!(!value.contains(" Hz"));
        assert!(!value.contains("0 bpp"));
    }
    #[test]
    fn peripheral_battery_names_are_not_collected() {
        let mut fixture = Fixture::default();
        fixture.directories.insert(
            "/sys/class/power_supply".into(),
            vec!["BAT0".into(), "hid-AA:BB:CC:DD:EE:FF-battery".into()],
        );
        fixture.put("/sys/class/power_supply/BAT0/type", "Battery");
        fixture.put(
            "/sys/class/power_supply/hid-AA:BB:CC:DD:EE:FF-battery/type",
            "Battery",
        );
        fixture.put(
            "/sys/class/power_supply/hid-AA:BB:CC:DD:EE:FF-battery/scope",
            "Device",
        );
        fixture.put(
            "/sys/class/power_supply/hid-AA:BB:CC:DD:EE:FF-battery/model_name",
            "Private personal device",
        );
        let value = batteries(&fixture).to_string();
        assert!(value.contains("BAT0"));
        assert!(!value.contains("AA:BB"));
        assert!(!value.contains("Private"));
    }
    #[test]
    fn malformed_values_and_terminal_escapes_stay_safe() {
        let mut fixture = Fixture::default();
        fixture.put(
            "/sys/class/dmi/id/product_name",
            "MiniBook\u{1b}[2J\n\u{202e}X",
        );
        fixture
            .directories
            .insert("/sys/class/power_supply".into(), vec!["BAT0".into()]);
        fixture.put("/sys/class/power_supply/BAT0/type", "Battery");
        fixture.put("/sys/class/power_supply/BAT0/capacity", "999");
        fixture.put(
            "/sys/class/power_supply/BAT0/charge_control_end_threshold",
            "not-a-number",
        );
        fixture.put("/sys/class/dmi/id/ec_firmware_release", "255.255");
        let report = collect_mode(&fixture, true);
        let mut bytes = Vec::new();
        human(&report, &mut bytes).unwrap();
        let output = String::from_utf8(bytes).unwrap();
        assert!(!output.contains('\u{1b}'));
        assert!(!output.contains('\u{202e}'));
        assert!(!output.contains("999%"));
        assert!(!output.contains("255.255"));
        assert!(report.to_string().contains("invalid"));
    }
    #[test]
    fn input_reads_reject_oversize_and_non_utf8() {
        assert_eq!(
            read_bounded(&vec![b'x'; MAX_FILE as usize + 1][..]),
            Err(Missing::Invalid)
        );
        assert_eq!(read_bounded(&[0xff][..]), Err(Missing::Invalid));
        assert!(read_bounded(&b"ordinary value\n"[..]).is_ok());
    }
    #[test]
    fn system_reader_rejects_symlink_leaves_and_special_files() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("minibookx-doctor-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let value = dir.join("value");
        fs::write(&value, "fixture").unwrap();
        let link = dir.join("link");
        symlink(&value, &link).unwrap();
        assert!(System.read(link.to_str().unwrap()).is_err());
        assert_eq!(System.read("/dev/null"), Err(Missing::Invalid));
        fs::remove_dir_all(dir).unwrap();
    }
}
