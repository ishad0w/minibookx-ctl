use minibookx_ctl::{broker, build_info, doctor, hardware, install, paths, presets, service};
#[cfg(feature = "dev-tools")]
use minibookx_ctl::{
    policy::{Config, Controller},
    power::PowerConfig,
    presets::Loaded,
    sensor::CpuSensor,
    thermal,
};
use std::{env, io::Write, path::PathBuf, process::ExitCode};
#[cfg(feature = "dev-tools")]
use std::{
    io::{self, BufRead},
    time::{Duration, Instant},
};

const HELP: &str = "minibookx-ctl — CHUWI MiniBook X U300 support
Usage:
  minibookx-ctl --install [--enable] [--destdir PATH]
  minibookx-ctl --uninstall [--destdir PATH]
  minibookx-ctl --daemon [--config PATH] [--no-tablet]
  minibookx-ctl --status
  minibookx-ctl --doctor [--json]  (also: minibookx-ctl doctor)
  minibookx-ctl --debug [--json]   (also: minibookx-ctl debug)
  minibookx-ctl --check-config PATH
  minibookx-ctl --version
  minibookx-ctl --build-info

Use the installed systemd unit for --daemon. Profiles are selected in the config.
Power limits are enabled only by power_control=true in the configuration.
Systemd lifecycle: --prepare, --recover. Recovery may be retried with the service stopped.
Reports perform no hardware writes.";

#[cfg(feature = "dev-tools")]
const DEV_HELP: &str = "Development tools (unprivileged, read-only):
  minibookx-ctl --inspect
  minibookx-ctl --monitor [--config PATH] [--samples N]
  minibookx-ctl --simulate [--config PATH] < trace.csv
  minibookx-ctl --capabilities";

fn run() -> Result<(), Box<dyn std::error::Error>> {
    run_args(env::args().skip(1))
}

fn reject_extra_operands(
    command: &str,
    args: &mut impl Iterator<Item = String>,
) -> Result<(), Box<dyn std::error::Error>> {
    if [
        "--help",
        "-h",
        "--version",
        "--build-info",
        "--prepare",
        "--recover",
        "--status",
        "--capabilities",
        "--check-config-stdin",
    ]
    .contains(&command)
        && args.next().is_some()
    {
        return Err("unexpected argument; this command accepts no operands".into());
    }
    Ok(())
}

fn diagnostic_json(
    mut args: impl Iterator<Item = String>,
) -> Result<bool, Box<dyn std::error::Error>> {
    match (args.next().as_deref(), args.next()) {
        (None, None) => Ok(false),
        (Some("--json"), None) => Ok(true),
        _ => Err("doctor/debug accept only --json".into()),
    }
}

fn run_args(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn std::error::Error>> {
    let Some(command) = args.next() else {
        println!("{HELP}");
        return Ok(());
    };
    reject_extra_operands(&command, &mut args)?;
    if command == "--help" || command == "-h" {
        println!("{HELP}");
        #[cfg(feature = "dev-tools")]
        println!("{DEV_HELP}");
        return Ok(());
    }
    if command == "--version" {
        println!("minibookx-ctl {}", build_info::VERSION);
        return Ok(());
    }
    if command == "--build-info" {
        println!("{}", build_info::json());
        return Ok(());
    }
    match command.as_str() {
        "--install" => install::run(true, args),
        "--uninstall" => install::run(false, args),
        "--doctor" | "doctor" | "--debug" | "debug" => {
            let json_output = diagnostic_json(args)?;
            if command == "--debug" || command == "debug" {
                doctor::print_debug(json_output).map_err(Into::into)
            } else {
                doctor::print(json_output).map_err(Into::into)
            }
        }
        "--prepare" => hardware::prepare().map_err(Into::into),
        "--recover" => hardware::recover().map_err(Into::into),
        "--status" => service::print_status().map_err(Into::into),
        "--check-config" => {
            let path = PathBuf::from(args.next().ok_or("--check-config requires a file")?);
            if args.next().is_some() {
                return Err("unexpected argument".into());
            }
            require_unprivileged_config()?;
            let loaded = presets::load(&path)?;
            println!(
                "configuration valid; profile={} fan_control={} power_control={}",
                loaded.preset.map_or("custom".into(), |p| p.to_string()),
                loaded.fan_control,
                loaded.power.enabled
            );
            Ok(())
        }
        // Installer-only snapshot validation. The installer attaches sealed
        // bytes as stdin and drops to the service account before exec.
        "--check-config-stdin" => install::check_config_stdin(),
        "--daemon" => run_daemon(args),
        #[cfg(feature = "dev-tools")]
        "--inspect" | "--monitor" | "--simulate" | "--capabilities" => {
            run_policy_tool(&command, args)
        }
        _ => Err("unknown command; use --help".into()),
    }
}

fn require_unprivileged_config() -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: geteuid has no arguments or side effects.
    if unsafe { libc::geteuid() } == 0 {
        return Err("read user configuration as an ordinary user or the service account".into());
    }
    Ok(())
}

fn run_daemon(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn std::error::Error>> {
    let mut path = None;
    let mut tablet = true;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--config" if path.is_none() => {
                path = Some(PathBuf::from(args.next().ok_or("--config needs a path")?));
            }
            "--no-tablet" => tablet = false,
            _ => return Err(format!("unsupported option {flag}").into()),
        }
    }
    // The policy child opens config only after closing raw descriptors and
    // dropping its uid, gid and capabilities.
    broker::run(path.unwrap_or_else(|| paths::CONFIG.into()), tablet)?;
    Ok(())
}

#[cfg(feature = "dev-tools")]
fn run_policy_tool(
    command: &str,
    mut args: impl Iterator<Item = String>,
) -> Result<(), Box<dyn std::error::Error>> {
    require_unprivileged_config()?;
    if command == "--capabilities" {
        return thermal::print_package_capabilities(std::path::Path::new("/sys/class/thermal"))
            .map_err(Into::into);
    }
    let mut path = None;
    let mut samples = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--config" if path.is_none() => {
                path = Some(PathBuf::from(args.next().ok_or("--config needs a path")?))
            }
            "--samples" if command == "--monitor" && samples.is_none() => {
                let value = args
                    .next()
                    .ok_or("--samples needs a number")?
                    .parse::<u64>()?;
                if value == 0 {
                    return Err("sample count must be positive".into());
                }
                samples = Some(value);
            }
            _ => return Err(format!("unsupported option {flag}").into()),
        }
    }
    let loaded = match path {
        Some(p) => presets::load(&p)?,
        None => Loaded {
            config: Config::default(),
            preset: None,
            text: String::new(),
            power: PowerConfig::default(),
            fan_control: true,
        },
    };
    let mut controller = Controller::new(loaded.config)?;
    if command == "--simulate" {
        println!("seconds,celsius,pwm_percent,raw_duty,changed,next_sample_ms");
        for (i, line) in io::stdin().lock().lines().enumerate() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line == "seconds,celsius" {
                continue;
            }
            let (s, t) = line
                .split_once(',')
                .ok_or_else(|| format!("line {} needs seconds,celsius", i + 1))?;
            let seconds = s.trim().parse::<f64>()?;
            let temp = t.trim().parse::<f64>()?;
            if !seconds.is_finite()
                || !(0.0..=1.0e9).contains(&seconds)
                || !temp.is_finite()
                || !(1.0..=125.0).contains(&temp)
            {
                return Err("invalid trace sample".into());
            }
            let d = controller.sample(
                Duration::from_secs_f64(seconds),
                (temp * 1000.).round() as i32,
            )?;
            println!(
                "{seconds:.3},{temp:.3},{},{},{},{}",
                d.pwm_percent,
                d.raw_duty,
                d.changed,
                d.next_sample.as_millis()
            );
        }
        return Ok(());
    }
    let sensor = CpuSensor::discover(std::path::Path::new("/sys/class/hwmon"))?;
    if command == "--inspect" {
        for p in sensor.paths() {
            println!("cpu_sensor={}", p.display())
        }
        println!(
            "package_temperature_c={:.3}",
            f64::from(sensor.read_mc()?) / 1000.
        );
        return Ok(());
    }
    let epoch = Instant::now();
    let mut taken = 0;
    println!("seconds,celsius,pwm_percent,raw_duty,changed,next_sample_ms");
    loop {
        let now = epoch.elapsed();
        let temp = sensor.read_mc()?;
        let d = controller.sample(now, temp)?;
        println!(
            "{:.3},{:.3},{},{},{},{}",
            now.as_secs_f64(),
            f64::from(temp) / 1000.,
            d.pwm_percent,
            d.raw_duty,
            d.changed,
            d.next_sample.as_millis()
        );
        taken += 1;
        if samples.is_some_and(|n| taken >= n) {
            break;
        }
        std::thread::sleep(d.next_sample);
    }
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "minibookx-ctl: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "dev-tools"))]
    #[test]
    fn release_cli_rejects_development_tools_before_opening_any_config() {
        for command in ["--inspect", "--monitor", "--simulate", "--capabilities"] {
            assert!(run_args([command.to_owned()].into_iter()).is_err());
        }
    }

    #[test]
    fn no_operand_commands_reject_arguments_before_any_lifecycle_or_diagnostic_action() {
        for command in [
            "--prepare",
            "--recover",
            "--status",
            "--capabilities",
            "--check-config-stdin",
            "--version",
            "--build-info",
            "--help",
            "-h",
        ] {
            for unexpected in ["--unexpected", "unwanted-operand"] {
                let args = [command.to_owned(), unexpected.to_owned()];
                let mut args = args.into_iter();
                let command = args.next().unwrap();
                let error = reject_extra_operands(&command, &mut args)
                    .unwrap_err()
                    .to_string();
                assert_eq!(
                    error,
                    "unexpected argument; this command accepts no operands"
                );
            }
        }
    }

    #[test]
    fn diagnostic_flags_are_validated_without_running_diagnostics_or_lifecycle_actions() {
        for args in [
            vec!["--json", "--unexpected"],
            vec!["--json", "--json"],
            vec!["--config", "/fixture-only"],
        ] {
            let error = diagnostic_json(args.iter().map(|s| (*s).to_owned()))
                .unwrap_err()
                .to_string();
            assert_eq!(error, "doctor/debug accept only --json");
        }
        assert!(!diagnostic_json(std::iter::empty()).unwrap());
        for command in ["--doctor", "doctor", "--debug", "debug"] {
            let mut args = ["--json".to_owned()].into_iter();
            reject_extra_operands(command, &mut args).unwrap();
            assert!(diagnostic_json(args).unwrap());
        }
    }
}
