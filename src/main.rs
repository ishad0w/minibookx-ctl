use minibookx_ctl::{
    broker, build_info, doctor, hardware, install, paths,
    policy::{Config, Controller},
    power::PowerConfig,
    presets::{self, Loaded},
    sensor::CpuSensor,
    service, thermal,
};
use std::{
    env,
    io::{self, BufRead},
    path::PathBuf,
    process::ExitCode,
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
  minibookx-ctl --inspect
  minibookx-ctl --monitor [--config PATH] [--samples N]
  minibookx-ctl --simulate [--config PATH] < trace.csv
  minibookx-ctl --capabilities
  minibookx-ctl --version
  minibookx-ctl --build-info

Use the installed systemd unit for --daemon. Profiles are selected in the config.
Power limits are enabled only by power_control=true in the configuration.
Systemd lifecycle: --prepare, --recover. Recovery may be retried with the service stopped.
Diagnostic and simulation commands perform no hardware writes.";

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
        "--install" => return install::run(true, args),
        "--uninstall" => return install::run(false, args),
        "--doctor" | "doctor" | "--debug" | "debug" => {
            let json_output = diagnostic_json(args)?;
            if command == "--debug" || command == "debug" {
                return doctor::print_debug(json_output).map_err(Into::into);
            }
            return doctor::print(json_output).map_err(Into::into);
        }
        "--prepare" => return hardware::prepare().map_err(Into::into),
        "--recover" => return hardware::recover().map_err(Into::into),
        "--status" => return service::print_status().map_err(Into::into),
        "--check-config" => {
            let path = PathBuf::from(args.next().ok_or("--check-config requires a file")?);
            if args.next().is_some() {
                return Err("unexpected argument".into());
            }
            let loaded = presets::load(&path)?;
            loaded.config.validate_for_hardware()?;
            loaded.power.validate()?;
            println!(
                "configuration valid; profile={} fan_control={} power_control={}",
                loaded.preset.map_or("custom".into(), |p| p.to_string()),
                loaded.fan_control,
                loaded.power.enabled
            );
            return Ok(());
        }
        // Installer-only snapshot validation. The installer attaches sealed
        // bytes as stdin and drops to the service account before exec.
        "--check-config-stdin" => return install::check_config_stdin(),
        "--capabilities" => {
            return thermal::print_package_capabilities(std::path::Path::new("/sys/class/thermal"))
                .map_err(Into::into);
        }
        _ => (),
    }
    if !["--daemon", "--inspect", "--monitor", "--simulate"].contains(&command.as_str()) {
        return Err("unknown command; use --help".into());
    }
    let mut path = None;
    let mut tablet = true;
    let mut samples = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--config" if path.is_none() => {
                path = Some(PathBuf::from(args.next().ok_or("--config needs a path")?))
            }
            "--no-tablet" if command == "--daemon" => tablet = false,
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
    if command == "--daemon" {
        // Deliberately no root-side config parsing. The policy child opens it
        // only after raw descriptors are closed and uid/gid have been dropped.
        broker::run(path.unwrap_or_else(|| paths::CONFIG.into()), tablet)?;
        return Ok(());
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
            eprintln!("minibookx-ctl: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
