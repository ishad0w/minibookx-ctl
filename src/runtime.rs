//! One-thread, event-driven controller. CPU sampling uses a 1..5 s monotonic timer;
//! configuration changes and sleep notifications use file descriptors.
use crate::{
    ec,
    policy::{Config, Controller},
    power::PowerConfig,
    presets::{self, Loaded, Preset},
    sensor::CpuSensor,
    service::{self, Session},
    systemd as sd,
};
use std::{
    io,
    os::fd::{OwnedFd, RawFd},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn accepted_config_summary(
    config: &Config,
    power: &PowerConfig,
    preset: Option<Preset>,
    fan_control: bool,
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "source": "last-accepted-policy",
        "preset": preset.map_or_else(|| "custom".into(), |p| p.to_string()),
        "fan_control": fan_control,
        "fan_curve": config.steps.iter().map(|step| serde_json::json!({
            "at_mc": step.at_mc, "pwm_percent": step.pwm_percent,
        })).collect::<Vec<_>>(),
        "hysteresis_mc": config.hysteresis_mc,
        "start_hold_ms": config.start_hold.as_millis(),
        "up_hold_ms": config.up_hold.as_millis(),
        "down_hold_ms": config.down_hold.as_millis(),
        "min_on_ms": config.min_on.as_millis(),
        "emergency_mc": config.emergency_mc,
        "idle_interval_ms": config.idle_interval.as_millis(),
        "active_interval_ms": config.active_interval.as_millis(),
        "power_control": power.enabled,
        "pl1_w": power.pl1_w,
        "pl2_w": power.pl2_w,
    })
}

struct State {
    session: Session,
    sensor: Option<CpuSensor>,
    fan_control: bool,
    config: Config,
    power: PowerConfig,
    controller: Controller,
    logind: sd::Logind,
    next_tick: Instant,
    config_path: Option<PathBuf>,
    config_text: String,
    preset: Option<Preset>,
    config_error: bool,
    inhibitor: Option<OwnedFd>,
    epoch: Instant,
    last_feedback_status: Option<Instant>,
    last_status: Option<Instant>,
    last_rpm: Option<u16>,
    last_ec_temp: Option<u8>,
    tach_state: crate::ipc::TachState,
    applied: u8,
    broker_emergency: bool,
    paused: bool,
    started: bool,
    reads: u64,
    writes: u64,
}
impl State {
    fn schedule(&mut self, delay: Duration) -> io::Result<()> {
        self.next_tick = Instant::now()
            .checked_add(delay)
            .ok_or_else(|| io::Error::other("sampling deadline overflow"))?;
        Ok(())
    }
    fn status(&mut self, cpu: i32, target: u8) -> io::Result<()> {
        let text = serde_json::json!({
            "active": true,
            "preset": self.preset.map_or_else(|| "custom".into(), |p| p.to_string()),
            "config_error": self.config_error,
            "paused_for_sleep": self.paused,
            "fan_control": self.fan_control,
            "manual": self.fan_control && !self.paused,
            "cpu_c": (self.fan_control && !self.paused).then_some(f64::from(cpu) / 1000.),
            "ec_c": self.last_ec_temp,
            "target_pwm_percent": self.fan_control.then_some(target),
            // Compatibility keys: these are the last broker-acknowledged values.
            "policy_applied_raw": (self.fan_control && !self.paused).then_some(self.applied),
            "policy_applied_pwm_percent": (self.fan_control && !self.paused).then_some(f64::from(self.applied) / 92. * 100.),
            "broker_emergency": self.broker_emergency,
            "fan_speed_ec": self.last_rpm,
            "broker_tach_state": self.tach_state.as_str(),
            "temperature_reads": self.reads,
            "duty_writes": self.writes,
            "sampling": if self.fan_control { "adaptive" } else { "disabled" },
            "power_requested": self.power.enabled,
            "pl1_w_requested": self.power.pl1_w,
            "pl2_w_requested": self.power.pl2_w,
            "snapshot_boottime_ms": crate::status::boottime_ms()?,
            "accepted_config": accepted_config_summary(&self.config, &self.power, self.preset, self.fan_control),
        }).to_string();
        service::write_status(&text).map_err(|e| sd::context("write policy status", e))?;
        self.last_status = Some(Instant::now());
        Ok(())
    }
    fn reload_config(&mut self) -> io::Result<()> {
        let Some(path) = &self.config_path else {
            return Ok(());
        };
        let result = presets::load(path).and_then(|loaded| {
            validate_startup_mode(self.fan_control, loaded.fan_control)?;
            loaded
                .config
                .validate_for_hardware()
                .map_err(io::Error::other)?;
            // Validate everything before requesting any hardware change. The
            // broker applies PL1/PL2 transactionally or retains the old limits.
            Controller::new(loaded.config.clone()).map_err(io::Error::other)?;
            if (loaded.text != self.config_text || self.config_error)
                && self.started
                && !self.paused
            {
                self.session.apply_power(&loaded.power)?;
            }
            Ok(loaded)
        });
        match result {
            Ok(loaded) => {
                if loaded.text == self.config_text && !self.config_error {
                    return Ok(());
                }
                self.controller
                    .reconfigure(loaded.config.clone())
                    .map_err(io::Error::other)?;
                self.config = loaded.config;
                self.power = loaded.power;
                self.preset = loaded.preset;
                self.config_text = loaded.text;
                self.config_error = false;
                eprintln!(
                    "configuration applied: preset={}",
                    self.preset
                        .map_or_else(|| "custom".into(), |p| p.to_string())
                );
            }
            Err(e) => {
                if !self.config_error {
                    eprintln!("configuration rejected; keeping last valid policy: {e}");
                }
                self.config_error = true;
            }
        }
        self.last_status = None;
        if self.paused {
            self.status(0, 0)?;
        }
        self.schedule(Duration::ZERO)
    }
    fn tick(&mut self) -> io::Result<()> {
        self.logind.check_connected()?;
        if self.paused {
            // CLOCK_MONOTONIC does not advance through S3; no wake alarm is used.
            self.schedule(Duration::from_secs(5))?;
        } else if !self.fan_control {
            self.session.heartbeat()?;
            if self
                .last_status
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(30))
            {
                self.status(0, 0)?;
            }
            self.schedule(Duration::from_secs(5))?;
        } else {
            let cpu = self
                .sensor
                .as_ref()
                .ok_or_else(|| io::Error::other("fan control has no CPU sensor"))?
                .read_mc()
                .map_err(|e| sd::context("read coretemp sample", e))?;
            self.reads += 1;
            let decision = self
                .controller
                .sample(self.epoch.elapsed(), cpu)
                .map_err(io::Error::other)?;
            let requested = ec::ramp(
                self.applied,
                decision.raw_duty,
                cpu >= self.config.emergency_mc,
            );
            let now = Instant::now();
            let applied = self
                .session
                .fan
                .apply(requested)
                .map_err(|e| sd::context("apply fan duty through broker", e))?;
            if applied.emergency && !self.broker_emergency {
                self.controller.observe_emergency(self.epoch.elapsed());
            }
            self.broker_emergency = applied.emergency;
            self.last_ec_temp = applied.ec_temperature;
            self.last_rpm = applied.tach;
            self.tach_state = applied.tach_state;
            let changed = self.applied != applied.raw;
            if changed {
                self.applied = applied.raw;
            }
            if applied.changed {
                self.writes += 1;
            }
            if changed || applied.changed {
                eprintln!(
                    "fan cpu_C={:.1} target_PWM={} applied_raw={} applied_PWM={:.1} broker_emergency={} duty_writes={}",
                    f64::from(cpu) / 1000.,
                    decision.pwm_percent,
                    self.applied,
                    f64::from(self.applied) / 92. * 100.,
                    self.broker_emergency,
                    self.writes
                );
            }
            // The broker alone samples and validates tach feedback, even if
            // this worker stalls. This timer only limits status-file updates.
            let refreshed_health = self
                .last_feedback_status
                .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(5));
            if refreshed_health {
                self.last_feedback_status = Some(now);
            }
            if changed
                || (refreshed_health && self.applied > 0)
                || self
                    .last_status
                    .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(30))
            {
                self.status(cpu, decision.pwm_percent)?;
            }
            // Continue a gradual ramp with 1s ticks; do not make it load-dependent.
            self.schedule(
                if self.broker_emergency || self.applied != decision.raw_duty {
                    Duration::from_secs(1)
                } else {
                    decision.next_sample
                },
            )?;
        }
        Ok(())
    }
    fn sleep(&mut self, preparing: bool) -> io::Result<()> {
        if !self.started {
            self.paused = preparing;
            return Ok(());
        }
        if preparing == self.paused {
            return Ok(());
        }
        if preparing {
            self.paused = true;
            // The broker also restores its original power limits on restore.
            self.session.fan.restore()?;
            self.broker_emergency = false;
            self.tach_state = crate::ipc::TachState::Unknown;
            self.last_ec_temp = None;
            self.last_rpm = None;
            self.inhibitor.take();
            self.status(0, 0)?;
            eprintln!("sleep: hardware policy released, delay inhibitor released");
        } else {
            self.inhibitor = Some(self.logind.delay_inhibitor()?);
            self.controller = Controller::new(self.config.clone()).map_err(io::Error::other)?;
            self.sensor = self
                .fan_control
                .then(|| CpuSensor::discover(Path::new("/sys/class/hwmon")))
                .transpose()?;
            self.session.apply_power(&self.power)?;
            self.session.activate()?;
            self.epoch = Instant::now();
            self.applied = 0;
            self.broker_emergency = false;
            self.last_feedback_status = None;
            self.last_ec_temp = None;
            self.last_rpm = None;
            self.tach_state = crate::ipc::TachState::Unknown;
            self.last_status = None;
            self.paused = false;
            eprintln!("resume: inhibitor reacquired, hardware checked, policy timers reset");
        }
        self.schedule(Duration::ZERO)
    }
}
fn validate_startup_mode(accepted: bool, requested: bool) -> io::Result<()> {
    if accepted != requested {
        return Err(io::Error::other(
            "fan_control change requires a service restart",
        ));
    }
    Ok(())
}
impl Drop for State {
    fn drop(&mut self) {
        if let Err(e) = self.session.fan.restore() {
            eprintln!("auto restore on shutdown failed: {e}");
        }
        self.inhibitor.take();
    }
}
fn pollfd(fd: RawFd, events: i16) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}
fn timeout_ms(deadline: Instant, now: Instant) -> i32 {
    // Round up rather than busy-spin for the last fractional millisecond.
    deadline
        .saturating_duration_since(now)
        .as_nanos()
        .div_ceil(1_000_000)
        .min(i32::MAX as u128) as i32
}
fn event_loop(
    state: &mut State,
    signals: &sd::Signals,
    watch: Option<&sd::ConfigWatch>,
    notify: &sd::Notifier,
) -> io::Result<()> {
    let interval = notify.watchdog_interval();
    let mut watchdog = interval.and_then(|delay| Instant::now().checked_add(delay));
    loop {
        // Drain on every iteration, including queued messages received during
        // setup calls. No callback can reenter State while it is being changed.
        for preparing in state.logind.drain_sleep_events()? {
            state.sleep(preparing)?;
        }
        if Instant::now() >= state.next_tick {
            state.tick()?;
        }
        let now = Instant::now();
        if watchdog.is_some_and(|deadline| now >= deadline) {
            notify.send("WATCHDOG=1")?;
            watchdog = interval.and_then(|delay| now.checked_add(delay));
        }
        let deadline = watchdog.map_or(state.next_tick, |d| d.min(state.next_tick));
        let bus = state.logind.watch();
        let bus_events =
            (if bus.read { libc::POLLIN } else { 0 }) | (if bus.write { libc::POLLOUT } else { 0 });
        let mut sources = [
            pollfd(signals.fd(), libc::POLLIN),
            pollfd(watch.map_or(-1, |w| w.fd()), libc::POLLIN),
            pollfd(bus.fd, bus_events),
        ];
        // SAFETY: sources is a live array with the stated element count. All fds
        // remain owned by objects outside this poll call; timeout is bounded.
        let result = unsafe {
            libc::poll(
                sources.as_mut_ptr(),
                sources.len() as libc::nfds_t,
                timeout_ms(deadline, Instant::now()),
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(sd::context("poll runtime event sources", error));
        }
        let pending_signals = signals.drain()?;
        for signal in &pending_signals {
            if *signal != libc::SIGHUP {
                eprintln!("stop: returning fan and power control to firmware");
                state.session.fan.restore()?;
                state.inhibitor.take();
                return Ok(());
            }
        }
        for source in &sources {
            if source.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(io::Error::other("runtime event source disconnected"));
            }
        }
        // Give sleep transitions priority over reload writes when several fds
        // wake together. A config rename requires no periodic stat operation.
        if sources[2].revents & (libc::POLLIN | libc::POLLOUT) != 0 {
            for preparing in state.logind.drain_sleep_events()? {
                state.sleep(preparing)?;
            }
        }
        if pending_signals.contains(&libc::SIGHUP) {
            state.reload_config()?;
        }
        if let Some(watch) = watch
            && sources[1].revents & libc::POLLIN != 0
            && watch.drain()?
        {
            state.reload_config()?;
        }
    }
}

pub fn run(loaded: Loaded, config_path: Option<PathBuf>) -> io::Result<()> {
    let Loaded {
        config,
        power,
        preset,
        text,
        fan_control,
    } = loaded;
    config.validate_for_hardware().map_err(io::Error::other)?;
    // Block before acquiring any hardware session. Drop order restores hardware
    // before the original signal mask is restored at function exit.
    let signals =
        sd::Signals::new().map_err(|e| sd::context("initialize policy signal source", e))?;
    let notify = sd::Notifier::from_environment()
        .map_err(|e| sd::context("initialize systemd notifier", e))?;
    let watch = config_path
        .as_deref()
        .map(sd::ConfigWatch::new)
        .transpose()
        .map_err(|e| sd::context("initialize config directory watch", e))?;
    let session = Session::open(fan_control)
        .map_err(|e| sd::context("open private hardware broker channel", e))?;
    let sensor = fan_control
        .then(|| CpuSensor::discover(Path::new("/sys/class/hwmon")))
        .transpose()
        .map_err(|e| sd::context("discover policy coretemp sensors", e))?;
    let controller = Controller::new(config.clone()).map_err(io::Error::other)?;
    let logind = sd::Logind::connect().map_err(|e| sd::context("connect policy to logind", e))?;
    let mut state = State {
        session,
        sensor,
        fan_control,
        config,
        power,
        controller,
        logind,
        next_tick: Instant::now(),
        config_path,
        config_text: text,
        preset,
        config_error: false,
        inhibitor: None,
        epoch: Instant::now(),
        last_feedback_status: None,
        last_status: None,
        last_rpm: None,
        last_ec_temp: None,
        tach_state: crate::ipc::TachState::Unknown,
        applied: 0,
        broker_emergency: false,
        paused: false,
        started: false,
        reads: 0,
        writes: 0,
    };
    // Close the load/watch race without a privileged write before the inhibitor.
    state
        .reload_config()
        .map_err(|e| sd::context("close config load/watch race", e))?;
    state.inhibitor = Some(
        state
            .logind
            .delay_inhibitor()
            .map_err(|e| sd::context("acquire startup delay inhibitor", e))?,
    );
    state.paused = state
        .logind
        .preparing_for_sleep()
        .map_err(|e| sd::context("read startup PreparingForSleep", e))?;
    state.started = true;
    if state.paused {
        state.session.fan.restore().map_err(|e| {
            sd::context("restore startup automatic control while preparing sleep", e)
        })?;
        state.inhibitor.take();
    } else {
        state
            .session
            .apply_power(&state.power)
            .map_err(|e| sd::context("apply startup power policy", e))?;
        state
            .session
            .activate()
            .map_err(|e| sd::context("activate startup fan session", e))?;
    }
    state
        .tick()
        .map_err(|e| sd::context("initial policy sample", e))?;
    eprintln!(
        "ready: U300 policy with live presets; 1 thread; fan_control={fan_control}; bounded broker access"
    );
    notify
        .send(if fan_control {
            "READY=1\nWATCHDOG=1\nSTATUS=Fan policy active; configuration watch enabled"
        } else {
            "READY=1\nWATCHDOG=1\nSTATUS=Fan control disabled; configuration watch enabled"
        })
        .map_err(|e| sd::context("announce policy READY", e))?;
    let result = event_loop(&mut state, &signals, watch.as_ref(), &notify);
    if let Err(e) = &result {
        eprintln!("controller failure: {e}");
    }
    if let Err(e) = notify.send("STOPPING=1") {
        eprintln!("systemd stop notification failed: {e}");
    }
    // State's Drop returns to firmware on all errors and ordinary shutdown.
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepted_status_contains_validated_active_values_without_raw_config_text() {
        let mut config = Config::default();
        config.steps[0].at_mc = 71_000;
        let power = PowerConfig {
            enabled: true,
            pl1_w: Some(12),
            pl2_w: Some(15),
        };
        let value = accepted_config_summary(&config, &power, Some(Preset::Quiet), false);
        assert_eq!(value["fan_control"], false);
        assert_eq!(value["source"], "last-accepted-policy");
        assert_eq!(value["preset"], "quiet");
        assert_eq!(value["fan_curve"][0]["at_mc"], 71_000);
        assert_eq!(value["pl1_w"], 12);
        assert_eq!(value["power_control"], true);
        assert!(value.get("config_text").is_none());
        assert!(value.get("config_path").is_none());
        assert!(value.to_string().len() < 2048);
    }
    #[test]
    fn poll_timeout_rounds_up_and_saturates() {
        let now = Instant::now();
        assert_eq!(timeout_ms(now, now), 0);
        assert_eq!(timeout_ms(now + Duration::from_micros(1), now), 1);
        assert_eq!(timeout_ms(now + Duration::from_millis(5), now), 5);
        assert_eq!(
            timeout_ms(now + Duration::from_secs(365 * 86400), now),
            i32::MAX
        );
    }

    #[test]
    fn fan_mode_changes_require_restart_in_both_directions() {
        assert!(validate_startup_mode(true, true).is_ok());
        assert!(validate_startup_mode(false, false).is_ok());
        for (accepted, requested) in [(true, false), (false, true)] {
            assert!(
                validate_startup_mode(accepted, requested)
                    .unwrap_err()
                    .to_string()
                    .contains("restart")
            );
        }
    }
}
