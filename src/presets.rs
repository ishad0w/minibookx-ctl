//! Data-only user configuration. No includes, commands or arbitrary file paths.
use crate::policy::Config;
use crate::power::PowerConfig;
use std::{
    fmt,
    fs::OpenOptions,
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    str::FromStr,
};

const MAX_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Balance,
    Quiet,
    Performance,
}
impl Preset {
    pub const ALL: [Self; 3] = [Self::Balance, Self::Quiet, Self::Performance];
    fn index(self) -> usize {
        match self {
            Self::Balance => 0,
            Self::Quiet => 1,
            Self::Performance => 2,
        }
    }
}
impl fmt::Display for Preset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Balance => "balance",
            Self::Quiet => "quiet",
            Self::Performance => "performance",
        })
    }
}
impl FromStr for Preset {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "balance" => Ok(Self::Balance),
            "quiet" => Ok(Self::Quiet),
            "performance" => Ok(Self::Performance),
            _ => Err("preset must be balance, quiet or performance".into()),
        }
    }
}

pub struct Document {
    pub active: Preset,
    pub fan_control: bool,
    configs: [Config; 3],
    powers: [PowerConfig; 3],
}
impl Document {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut active = None;
        let mut power_enabled = None;
        let mut fan_control = None;
        let mut section = None;
        let mut bodies: [Option<String>; 3] = [None, None, None];
        for line in text.lines() {
            let data = line.split('#').next().unwrap_or("").trim();
            if data.is_empty() {
                continue;
            }
            if let Some(name) = data.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                let preset: Preset = name.trim().parse()?;
                if bodies[preset.index()].is_some() {
                    return Err(format!("duplicate [{preset}] section"));
                }
                bodies[preset.index()] = Some(String::new());
                section = Some(preset);
            } else if let Some(preset) = section {
                let body = bodies[preset.index()].as_mut().unwrap();
                body.push_str(data);
                body.push('\n');
            } else {
                let (key, value) = data.split_once('=').ok_or("expected preset = NAME")?;
                match key.trim() {
                    "preset" if active.is_none() => active = Some(value.trim().parse()?),
                    "fan_control" if fan_control.is_none() => fan_control = Some(value.trim().parse::<bool>().map_err(|_| "fan_control must be true or false")?),
                    "power_control" if power_enabled.is_none() => power_enabled = Some(value.trim().parse::<bool>().map_err(|_|"power_control must be true or false")?),
                    _ => return Err("expected one preset and optional fan_control/power_control selections before the sections".into()),
                }
            }
        }
        let mut configs = [Config::default(), Config::default(), Config::default()];
        let mut powers: [PowerConfig; 3] = std::array::from_fn(|_| PowerConfig::default());
        for preset in Preset::ALL {
            let body = bodies[preset.index()]
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("missing or empty [{preset}] section"))?;
            let mut fan_body = String::new();
            let mut seen = [false; 2];
            let mut power = PowerConfig {
                enabled: power_enabled.unwrap_or(false),
                ..PowerConfig::default()
            };
            for line in body.lines() {
                let (key, value) = line.split_once('=').ok_or("expected key = value")?;
                let index = match key.trim() {
                    "pl1_w" => Some(0),
                    "pl2_w" => Some(1),
                    _ => None,
                };
                if let Some(index) = index {
                    if seen[index] {
                        return Err(format!("duplicate {}", key.trim()));
                    }
                    seen[index] = true;
                    let value = if value.trim() == "firmware" {
                        None
                    } else {
                        Some(
                            value
                                .trim()
                                .parse::<u32>()
                                .map_err(|_| "power limit must be firmware or whole watts")?,
                        )
                    };
                    if index == 0 {
                        power.pl1_w = value
                    } else {
                        power.pl2_w = value
                    }
                } else {
                    fan_body.push_str(line);
                    fan_body.push('\n');
                }
            }
            power.validate()?;
            powers[preset.index()] = power;
            configs[preset.index()] = Config::from_text(&fan_body)?;
            configs[preset.index()].validate_for_hardware()?;
        }
        Ok(Self {
            active: active.ok_or("missing preset selection")?,
            fan_control: fan_control.unwrap_or(true),
            configs,
            powers,
        })
    }
    pub fn config(&self, preset: Preset) -> &Config {
        &self.configs[preset.index()]
    }
    pub fn power(&self, preset: Preset) -> &PowerConfig {
        &self.powers[preset.index()]
    }
}

pub struct Loaded {
    pub config: Config,
    pub preset: Option<Preset>,
    pub text: String,
    pub power: PowerConfig,
    pub fan_control: bool,
}
pub fn load(path: &Path) -> io::Result<Loaded> {
    parse(read_text(path)?)
}

/// Parse a bounded snapshot after the caller has dropped privileges.
pub(crate) fn parse(text: String) -> io::Result<Loaded> {
    if text.len() as u64 > MAX_BYTES {
        return Err(io::Error::other("config exceeds 64 KiB"));
    }
    if !text
        .lines()
        .any(|line| !line.split('#').next().unwrap_or("").trim().is_empty())
    {
        return Err(io::Error::other("configuration is empty"));
    }
    let is_presets = text.lines().any(|line| {
        let line = line.split('#').next().unwrap_or("").trim();
        line.starts_with('[')
            || line
                .split_once('=')
                .is_some_and(|(k, _)| k.trim() == "preset")
    });
    let (config, preset, power, fan_control) = if is_presets {
        let document = Document::parse(&text).map_err(io::Error::other)?;
        (
            document.config(document.active).clone(),
            Some(document.active),
            document.power(document.active).clone(),
            document.fan_control,
        )
    } else {
        let mut fan_control = None;
        let mut policy = String::new();
        for line in text.lines() {
            let data = line.split('#').next().unwrap_or("").trim();
            if let Some((key, value)) = data.split_once('=')
                && key.trim() == "fan_control"
            {
                if fan_control.is_some() {
                    return Err(io::Error::other("duplicate fan_control"));
                }
                fan_control = Some(value.trim().parse::<bool>().map_err(io::Error::other)?);
            } else {
                policy.push_str(line);
                policy.push('\n');
            }
        }
        (
            Config::from_text(&policy).map_err(io::Error::other)?,
            None,
            PowerConfig::default(),
            fan_control.unwrap_or(true),
        )
    };
    Ok(Loaded {
        config,
        preset,
        text,
        power,
        fan_control,
    })
}
fn read_text(path: &Path) -> io::Result<String> {
    // A root service must never follow a user-controlled config symlink or block
    // opening a FIFO/device. Reject nonregular and oversized files before reading.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(io::Error::other(
            "config must be a regular file of at most 64 KiB",
        ));
    }
    let mut text = String::new();
    file.take(MAX_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_BYTES {
        return Err(io::Error::other("config exceeds 64 KiB"));
    }
    Ok(text)
}
#[cfg(test)]
mod tests {
    use super::*;
    const SAMPLE: &str = include_str!("../config/presets.conf");

    #[test]
    fn balance_matches_the_default_policy() {
        let d = Document::parse(SAMPLE).unwrap();
        assert_eq!(d.active, Preset::Balance);
        assert_eq!(d.config(Preset::Balance), &Config::default());
    }

    #[test]
    fn damaged_or_unsafe_inactive_presets_reject_the_whole_document() {
        for bad in [
            SAMPLE.replace("[quiet]", "[balance]"),
            SAMPLE.replace("preset = balance", "preset = missing"),
            SAMPLE.replace("curve = 80:35", "curve = 80:25"),
            SAMPLE.replace("emergency_c = 95", "emergency_c = 100"),
            SAMPLE.replace("up_hold_s = 30", "up_hold_s = 30\nup_hold_s = 2"),
        ] {
            assert!(Document::parse(&bad).is_err());
        }
    }

    #[test]
    fn configuration_loader_rejects_symlinks_fifos_and_empty_updates() {
        use std::{env, fs, os::unix::fs::symlink};
        let dir = env::temp_dir().join(format!("fand-presets-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.conf");
        let custom = SAMPLE.replace("70:35", "71:35");
        fs::write(&path, &custom).unwrap();
        let d = load(&path).unwrap();
        assert_eq!(d.preset, Some(Preset::Balance));
        assert_eq!(d.config.steps[0].at_mc, 71_000);
        assert_eq!(fs::read_to_string(&path).unwrap(), custom);
        let link = dir.join("link.conf");
        symlink(&path, &link).unwrap();
        assert!(load(&link).is_err());
        let fifo = dir.join("fifo.conf");
        let cpath = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: valid nul-terminated path, ordinary temporary FIFO for a read guard test.
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        assert!(load(&fifo).is_err());
        fs::write(&path, "# accidentally emptied during editing\n").unwrap();
        assert!(load(&path).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fan_control_defaults_on_and_accepts_only_an_explicit_boolean() {
        assert!(Document::parse(SAMPLE).unwrap().fan_control);
        let legacy = SAMPLE.replace("fan_control = true\n", "");
        assert!(Document::parse(&legacy).unwrap().fan_control);
        assert!(
            !Document::parse(&legacy.replacen(
                "preset = balance",
                "fan_control = false\npreset = balance",
                1
            ))
            .unwrap()
            .fan_control
        );
        assert!(
            !parse("fan_control = false\ncurve = 70:35\n".into())
                .unwrap()
                .fan_control
        );
        assert!(parse("curve = 70:35\n".into()).unwrap().fan_control);
        for setting in ["no", "0", "FALSE", "false\nfan_control = true"] {
            assert!(Document::parse(&format!("fan_control = {setting}\n{legacy}")).is_err());
            assert!(parse(format!("fan_control = {setting}\ncurve = 70:35\n")).is_err());
        }
    }
}
