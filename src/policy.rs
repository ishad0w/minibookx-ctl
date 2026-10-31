use std::time::Duration;

const EMERGENCY_PWM: u8 = 80;
const EMERGENCY_RELEASE_HYSTERESIS_MC: i32 = 5_000;
const EMERGENCY_COOL_HOLD: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub at_mc: i32,
    pub pwm_percent: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub steps: Vec<Step>,
    pub hysteresis_mc: i32,
    pub start_hold: Duration,
    pub up_hold: Duration,
    pub min_on: Duration,
    pub down_hold: Duration,
    pub emergency_mc: i32,
    pub idle_interval: Duration,
    pub active_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Actual EC writes and lifecycle protection belong to the runtime.
            // PWM is electrical duty, not a fraction of RPM.
            steps: vec![
                Step {
                    at_mc: 60_000,
                    pwm_percent: 35,
                },
                Step {
                    at_mc: 85_000,
                    pwm_percent: 40,
                },
                Step {
                    at_mc: 90_000,
                    pwm_percent: 50,
                },
                Step {
                    at_mc: 93_000,
                    pwm_percent: 70,
                },
            ],
            hysteresis_mc: 5_000,
            start_hold: Duration::from_secs(12),
            up_hold: Duration::from_secs(20),
            min_on: Duration::from_secs(45),
            down_hold: Duration::from_secs(30),
            emergency_mc: 95_000,
            idle_interval: Duration::from_secs(5),
            active_interval: Duration::from_secs(1),
        }
    }
}

impl Config {
    /// General policy validation also permits untested low PWM for simulation.
    pub fn validate(&self) -> Result<(), String> {
        if self.steps.is_empty() || self.steps.len() > 16 {
            return Err("curve needs 1..16 steps".into());
        }
        let mut previous = (0, 0);
        for s in &self.steps {
            if !(20_000..=100_000).contains(&s.at_mc)
                || !(1..=80).contains(&s.pwm_percent)
                || s.at_mc <= previous.0
                || s.pwm_percent < previous.1
            {
                return Err(
                    "curve needs increasing 20..100 C thresholds and nondecreasing 1..80% PWM"
                        .into(),
                );
            }
            previous = (s.at_mc, s.pwm_percent);
        }
        if !(85_000..=100_000).contains(&self.emergency_mc) || previous.0 >= self.emergency_mc {
            return Err(
                "emergency temperature must be 85..100 C and above the normal curve".into(),
            );
        }
        if !(1_000..=15_000).contains(&self.hysteresis_mc)
            || self.active_interval < Duration::from_secs(1)
            || self.idle_interval < self.active_interval
            || self.idle_interval > Duration::from_secs(10)
            || self.start_hold > Duration::from_secs(120)
            || self.up_hold > Duration::from_secs(120)
            || self.min_on > Duration::from_secs(300)
            || self.down_hold > Duration::from_secs(120)
        {
            return Err("invalid hysteresis, hold times or polling intervals".into());
        }
        Ok(())
    }

    /// The live backend must call this; Controller itself is also a simulator.
    pub fn validate_for_hardware(&self) -> Result<(), String> {
        self.validate()?;
        if self.steps.iter().any(|s| s.pwm_percent < 35) {
            return Err(
                "live U300 control requires at least 35% PWM; lower values are simulation only"
                    .into(),
            );
        }
        if self.emergency_mc > 95_000 {
            return Err(
                "live U300 control requires an emergency threshold no higher than 95 C".into(),
            );
        }
        Ok(())
    }

    pub fn from_text(text: &str) -> Result<Self, String> {
        let mut c = Self::default();
        let mut seen = std::collections::HashSet::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("line {} needs key=value", i + 1))?;
            let (key, value) = (key.trim(), value.trim());
            if !seen.insert(key) {
                return Err(format!("duplicate key: {key}"));
            }
            let integer = || {
                value
                    .parse::<u64>()
                    .map_err(|_| format!("invalid value for {key}"))
            };
            let milli_celsius = || {
                integer()?
                    .checked_mul(1000)
                    .and_then(|v| v.try_into().ok())
                    .ok_or_else(|| format!("temperature overflow for {key}"))
            };
            match key {
                "curve" => {
                    c.steps = value
                        .split(',')
                        .map(|entry| {
                            let (t, p) = entry.split_once(':').ok_or("curve needs C:PWM pairs")?;
                            let temp = t
                                .trim()
                                .parse::<i32>()
                                .map_err(|_| "invalid curve temperature")?;
                            let at_mc = temp.checked_mul(1000).ok_or("temperature overflow")?;
                            let pwm_percent = p.trim().parse().map_err(|_| "invalid PWM")?;
                            Ok(Step { at_mc, pwm_percent })
                        })
                        .collect::<Result<_, String>>()?;
                }
                "hysteresis_c" => c.hysteresis_mc = milli_celsius()?,
                "start_hold_s" => c.start_hold = Duration::from_secs(integer()?),
                "up_hold_s" => c.up_hold = Duration::from_secs(integer()?),
                "min_on_s" => c.min_on = Duration::from_secs(integer()?),
                "down_hold_s" => c.down_hold = Duration::from_secs(integer()?),
                "emergency_c" => c.emergency_mc = milli_celsius()?,
                "idle_interval_ms" => c.idle_interval = Duration::from_millis(integer()?),
                "active_interval_ms" => c.active_interval = Duration::from_millis(integer()?),
                _ => return Err(format!("unknown key: {key}")),
            }
        }
        c.validate()?;
        Ok(c)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    pub pwm_percent: u8,
    pub raw_duty: u8,
    pub changed: bool,
    pub next_sample: Duration,
}

/// Pure policy: no EC, sysfs, clocks, threads or write permissions.
/// Delays measure observed continuous conditions between caller samples.
/// The runtime must reset/recreate this state after suspend, never integrate
/// a sleeping machine or a missed sampling interval as a thermal measurement.
pub struct Controller {
    config: Config,
    level: usize,
    pending_start: Option<Duration>,
    up_since: Vec<Option<Duration>>,
    pending_down: Option<(usize, Duration)>,
    on_since: Option<Duration>,
    emergency: bool,
    emergency_cool_since: Option<Duration>,
    previous: Option<(Duration, i32)>,
    last_emitted: Option<u8>,
    takeover_pwm: Option<u8>,
}

impl Controller {
    pub fn new(config: Config) -> Result<Self, String> {
        config.validate()?;
        let up_since = vec![None; config.steps.len()];
        Ok(Self {
            config,
            level: 0,
            pending_start: None,
            up_since,
            pending_down: None,
            on_since: None,
            emergency: false,
            emergency_cool_since: None,
            previous: None,
            last_emitted: None,
            takeover_pwm: None,
        })
    }

    /// First live sample after startup/resume. Do not stop a firmware-running
    /// fan or wait out start/up holds on a CPU that is already hot. Keep a duty
    /// above the curve until the normal cooling hold permits a lower stage.
    pub fn takeover(&mut self, now: Duration, temp_mc: i32, raw: u8) -> Result<Decision, String> {
        if self.previous.is_some()
            || !(1_000..=125_000).contains(&temp_mc)
            || (raw != 0 && !(crate::ec::MIN_RAW..=crate::ec::MAX_RAW).contains(&raw))
        {
            return Err("invalid initial fan sample".into());
        }
        let pwm = ((u16::from(raw) * 100 + 46) / 92).min(80) as u8;
        self.takeover_pwm = (raw != 0).then_some(pwm);
        let heat_level = self
            .config
            .steps
            .iter()
            .take_while(|s| temp_mc >= s.at_mc)
            .count();
        let running_level = if raw == 0 {
            0
        } else {
            self.config
                .steps
                .iter()
                .position(|s| s.pwm_percent >= pwm)
                .map_or(self.config.steps.len(), |i| i + 1)
        };
        self.level = heat_level.max(running_level);
        self.on_since = (self.level > 0).then_some(now);
        self.sample(now, temp_mc)
    }

    /// Reconfigure without clearing an emergency latch or the real minimum-on
    /// clock. Normal holds restart because the new thresholds were not observed
    /// continuously under the new policy. The caller validates live PWM bounds.
    pub fn reconfigure(&mut self, config: Config) -> Result<(), String> {
        config.validate()?;
        if config == self.config {
            return Ok(());
        }
        if self.level > 0 {
            let previous_pwm = self.config.steps[self.level - 1].pwm_percent;
            self.level = config
                .steps
                .iter()
                .position(|s| s.pwm_percent >= previous_pwm)
                .map_or(config.steps.len(), |i| i + 1);
        }
        self.up_since = vec![None; config.steps.len()];
        self.pending_start = None;
        self.pending_down = None;
        self.emergency_cool_since = None;
        self.config = config;
        Ok(())
    }

    /// Single-sensor wrapper for simulations and ordinary coretemp control.
    pub fn sample(&mut self, now: Duration, temp_mc: i32) -> Result<Decision, String> {
        self.sample_with_cpu(now, temp_mc, temp_mc)
    }

    /// The independent broker may observe heat between worker samples. Start
    /// minimum-on conservatively at observation and retain the ordinary latch.
    pub(crate) fn observe_emergency(&mut self, now: Duration) {
        if self.emergency {
            return;
        }
        if self.level == 0 {
            self.on_since = Some(now);
        }
        self.emergency = true;
        self.emergency_cool_since = None;
        self.pending_start = None;
        self.pending_down = None;
        self.up_since.fill(None);
    }

    /// A sampled EC70 control input may be supplied independently of the CPU
    /// safety input. CPU >= emergency_mc always overrides every delay.
    pub fn sample_with_cpu(
        &mut self,
        now: Duration,
        temp_mc: i32,
        cpu_mc: i32,
    ) -> Result<Decision, String> {
        if !(1_000..=125_000).contains(&temp_mc) {
            return Err("invalid control temperature".into());
        }
        if !(1_000..=125_000).contains(&cpu_mc) {
            return Err("invalid CPU temperature".into());
        }
        if self.previous.is_some_and(|(before, _)| now < before) {
            return Err("sample clock moved backwards".into());
        }

        if cpu_mc >= self.config.emergency_mc {
            self.observe_emergency(now);
            self.emergency_cool_since = None;
        }

        if self.emergency {
            let release_mc = self.config.emergency_mc - EMERGENCY_RELEASE_HYSTERESIS_MC;
            if temp_mc >= release_mc || cpu_mc >= release_mc {
                self.emergency_cool_since = None;
            } else {
                let since = *self.emergency_cool_since.get_or_insert(now);
                if now.saturating_sub(since) >= EMERGENCY_COOL_HOLD {
                    self.emergency = false;
                    self.takeover_pwm = None;
                    self.emergency_cool_since = None;
                    self.level = self
                        .config
                        .steps
                        .iter()
                        .take_while(|s| temp_mc >= s.at_mc)
                        .count();
                    self.pending_down = None;
                    if self.level == 0 {
                        if self.on_long_enough(now) {
                            self.on_since = None;
                        } else {
                            // The emergency cooling hold has already elapsed.
                            // Finish minimum-on at the lowest quiet setting,
                            // without requiring another whole cooling hold.
                            self.level = 1;
                            self.pending_down = Some((0, since));
                        }
                    }
                    self.up_since.fill(None);
                }
            }
        } else if self.level == 0 {
            self.pending_down = None;
            self.up_since.fill(None);
            if temp_mc >= self.config.steps[0].at_mc {
                let since = *self.pending_start.get_or_insert(now);
                if now.saturating_sub(since) >= self.config.start_hold {
                    // First start always uses the minimum configured step.
                    // Higher thresholds begin their separate hold now.
                    self.level = 1;
                    self.on_since = Some(now);
                    self.pending_start = None;
                    self.track_higher_thresholds(now, temp_mc);
                }
            } else {
                self.pending_start = None;
            }
        } else {
            self.pending_start = None;
            self.track_higher_thresholds(now, temp_mc);
            let mut higher = self.level;
            for (i, since) in self.up_since.iter().enumerate() {
                if i + 1 > self.level
                    && since.is_some_and(|since| now.saturating_sub(since) >= self.config.up_hold)
                {
                    higher = i + 1;
                }
            }
            if higher > self.level {
                self.level = higher;
                self.pending_down = None;
                for since in &mut self.up_since[..self.level] {
                    *since = None;
                }
            } else {
                let mut target = self.level;
                while target > 0
                    && temp_mc < self.config.steps[target - 1].at_mc - self.config.hysteresis_mc
                {
                    target -= 1;
                }
                if target < self.level {
                    let since = match self.pending_down {
                        Some((pending, since)) if pending == target => since,
                        _ => {
                            self.pending_down = Some((target, now));
                            now
                        }
                    };
                    if (target != 0 || self.on_long_enough(now))
                        && now.saturating_sub(since) >= self.config.down_hold
                    {
                        self.level = target;
                        self.takeover_pwm = None;
                        self.pending_down = None;
                        if target == 0 {
                            self.on_since = None;
                        }
                    }
                } else {
                    self.pending_down = None;
                }
            }
        }

        let rising = self.previous.is_some_and(|(_, old)| temp_mc >= old + 2_000);
        self.previous = Some((now, temp_mc));
        let near_start = temp_mc >= self.config.steps[0].at_mc - 8_000;
        let next_sample = if self.emergency
            || self.level > 0
            || self.pending_start.is_some()
            || self.up_since.iter().any(Option::is_some)
            || near_start
            || rising
            || cpu_mc >= self.config.emergency_mc - 10_000
        {
            self.config.active_interval
        } else {
            self.config.idle_interval
        };
        let pwm_percent = if self.emergency {
            EMERGENCY_PWM
        } else if self.level == 0 {
            0
        } else {
            self.config.steps[self.level - 1]
                .pwm_percent
                .max(self.takeover_pwm.unwrap_or(0))
        };
        // CTR=91, interior duty only. A value of255 would not mean full speed.
        let raw_duty = ((u16::from(pwm_percent) * 92 + 50) / 100) as u8;
        let changed = self.last_emitted != Some(pwm_percent);
        self.last_emitted = Some(pwm_percent);
        Ok(Decision {
            pwm_percent,
            raw_duty,
            changed,
            next_sample,
        })
    }

    fn on_long_enough(&self, now: Duration) -> bool {
        self.on_since
            .is_none_or(|since| now.saturating_sub(since) >= self.config.min_on)
    }

    fn track_higher_thresholds(&mut self, now: Duration, temp_mc: i32) {
        for (i, since) in self.up_since.iter_mut().enumerate() {
            if i < self.level || temp_mc < self.config.steps[i].at_mc - self.config.hysteresis_mc {
                *since = None;
            } else if temp_mc >= self.config.steps[i].at_mc {
                since.get_or_insert(now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(c: &mut Controller, s: u64, t: i32) -> Decision {
        c.sample(Duration::from_secs(s), t * 1000).unwrap()
    }

    /// A compact CSV trace exercises the compiled policy, including commands
    /// that must remain unchanged; it is not a replacement policy model.
    fn trace(csv: &str) {
        let mut c = Controller::new(Config::default()).unwrap();
        for row in csv.lines().filter(|s| !s.trim().is_empty()) {
            let v: Vec<i32> = row.split(',').map(|s| s.trim().parse().unwrap()).collect();
            let d = c
                .sample_with_cpu(Duration::from_secs(v[0] as u64), v[1] * 1000, v[2] * 1000)
                .unwrap();
            assert_eq!(i32::from(d.pwm_percent), v[3], "trace row {row}");
        }
    }

    #[test]
    fn quiet_idle_has_no_repeated_commands_and_slow_sampling() {
        let mut c = Controller::new(Config::default()).unwrap();
        let first = at(&mut c, 0, 40);
        assert_eq!(first.next_sample, Duration::from_secs(5));
        assert_eq!(first.raw_duty, 0);
        assert!(first.changed);
        for s in 1..100 {
            assert!(!at(&mut c, s, 40).changed);
        }
    }

    #[test]
    fn brief_82_degree_burst_does_not_start_even_later() {
        trace("0,40,40,0\n5,82,82,0\n6,40,40,0\n18,40,40,0");
    }

    #[test]
    fn sustained_82_starts_at_12_seconds_and_stays_at_35() {
        let mut c = Controller::new(Config::default()).unwrap();
        for s in 0..12 {
            let d = at(&mut c, s, 82);
            assert_eq!(d.pwm_percent, 0);
            assert_eq!(d.next_sample, Duration::from_secs(1));
        }
        assert_eq!(at(&mut c, 12, 82).raw_duty, 32);
        for s in 13..300 {
            let d = at(&mut c, s, 82);
            assert_eq!(d.pwm_percent, 35);
            assert!(!d.changed);
        }
    }

    #[test]
    fn first_start_needs_a_continuous_threshold_condition() {
        trace("0,60,60,0\n11,59,59,0\n12,60,60,0\n23,60,60,0\n24,60,60,35");
    }

    #[test]
    fn upper_thresholds_have_independent_continuous_holds() {
        trace(
            "0,82,82,0\n12,82,82,35\n13,90,90,35\n23,93,93,35\n32,93,93,35\n33,93,93,50\n42,93,93,50\n43,93,93,70",
        );
    }

    #[test]
    fn hotter_start_does_not_skip_first_quiet_step_or_stack_ema_delay() {
        trace("0,93,93,0\n12,93,93,35\n31,93,93,35\n32,93,93,70");
    }

    #[test]
    fn cooling_below_upper_hysteresis_band_cancels_the_pending_raise() {
        trace("0,82,82,0\n12,82,82,35\n20,93,93,35\n21,79,79,35\n60,79,79,35");
    }

    #[test]
    fn falling_below_a_higher_hysteresis_band_resets_only_its_hold() {
        trace(
            "0,82,82,0\n12,82,82,35\n13,93,93,35\n30,87,87,35\n33,87,87,50\n34,93,93,50\n53,93,93,50\n54,93,93,70",
        );
    }

    #[test]
    fn upper_hold_survives_temperature_noise_inside_the_hysteresis_band() {
        let mut c = Controller::new(Config::default()).unwrap();
        at(&mut c, 0, 82);
        at(&mut c, 12, 82);
        for s in 13..33 {
            assert_eq!(
                at(&mut c, s, if s % 2 == 0 { 92 } else { 94 }).pwm_percent,
                35
            );
        }
        assert_eq!(at(&mut c, 33, 92).pwm_percent, 70);
    }

    #[test]
    fn hot_takeover_preserves_firmware_duty_until_a_cooling_hold() {
        let mut c = Controller::new(Config::default()).unwrap();
        assert_eq!(c.takeover(Duration::ZERO, 90_000, 74).unwrap().raw_duty, 74);
        for s in 1..40 {
            assert_eq!(at(&mut c, s, 90).pwm_percent, 80);
        }
        assert_eq!(at(&mut c, 40, 87).pwm_percent, 80);
        assert_eq!(at(&mut c, 69, 87).pwm_percent, 80);
        assert_eq!(at(&mut c, 70, 87).pwm_percent, 50);
    }

    #[test]
    fn hot_takeover_with_stopped_firmware_fan_bypasses_start_and_up_holds() {
        let mut c = Controller::new(Config::default()).unwrap();
        assert_eq!(
            c.takeover(Duration::ZERO, 93_000, 0).unwrap().pwm_percent,
            70
        );
        let mut c = Controller::new(Config::default()).unwrap();
        assert_eq!(c.takeover(Duration::ZERO, 95_000, 0).unwrap().raw_duty, 74);
        let mut c = Controller::new(Config::default()).unwrap();
        assert_eq!(
            c.takeover(Duration::ZERO, 40_000, 0).unwrap().pwm_percent,
            0
        );
    }

    #[test]
    fn minimum_on_and_continuous_cooling_hold_both_apply() {
        trace("0,60,60,0\n12,60,60,35\n13,50,50,35\n43,50,50,35\n56,50,50,35\n57,50,50,0");
    }

    #[test]
    fn cooling_hold_resets_inside_the_hysteresis_band() {
        trace(
            "0,60,60,0\n12,60,60,35\n20,50,50,35\n40,56,56,35\n41,50,50,35\n70,50,50,35\n71,50,50,0",
        );
    }

    #[test]
    fn normal_cooling_obeys_every_hysteresis_boundary_and_down_hold() {
        trace(
            "0,93,93,0\n12,93,93,35\n32,93,93,70\n33,89,89,70\n34,87,87,70\n63,87,87,70\n64,87,87,50\n65,85,85,50\n66,84,84,50\n95,84,84,50\n96,84,84,40\n97,80,80,40\n98,79,79,40\n128,79,79,35\n129,55,55,35\n130,54,54,35\n160,54,54,0",
        );
    }

    #[test]
    fn noise_near_start_threshold_does_not_cycle_running_fan() {
        let mut c = Controller::new(Config::default()).unwrap();
        at(&mut c, 0, 60);
        at(&mut c, 12, 60);
        for s in 13..150 {
            assert_eq!(at(&mut c, s, 58 + (s % 3) as i32).pwm_percent, 35);
        }
    }

    #[test]
    fn emergency_bypasses_start_hold_and_uses_separate_cpu_input() {
        trace("0,40,40,0\n1,40,95,80\n2,40,99,80\n3,40,92,80\n33,40,92,80");
        let mut c = Controller::new(Config::default()).unwrap();
        let d = at(&mut c, 0, 95);
        assert_eq!(d.raw_duty, 74);
        assert_eq!(d.next_sample, Duration::from_secs(1));
        assert!(!at(&mut c, 1, 95).changed);
    }

    #[test]
    fn emergency_cooldown_is_continuous_and_has_five_degree_hysteresis() {
        trace("0,95,95,80\n1,89,89,80\n29,90,90,80\n30,89,89,80\n59,89,89,80\n60,89,89,40");
    }

    #[test]
    fn emergency_release_respects_minimum_on_without_restarting_cooling_hold() {
        trace("0,95,95,80\n1,40,40,80\n30,40,40,80\n31,40,40,35\n44,40,40,35\n45,40,40,0");
    }

    #[test]
    fn cpu_guard_bypasses_pending_up_and_down_delays() {
        trace("0,70,70,0\n12,70,70,35\n13,90,90,35\n14,90,95,80\n15,60,60,80");
        trace("0,70,70,0\n12,70,70,35\n13,60,60,35\n14,60,95,80");
    }

    #[test]
    fn repeated_timestamps_cannot_satisfy_a_hold() {
        let mut c = Controller::new(Config::default()).unwrap();
        for _ in 0..30 {
            assert_eq!(at(&mut c, 10, 82).pwm_percent, 0);
        }
        assert_eq!(at(&mut c, 22, 82).pwm_percent, 35);
    }

    #[test]
    fn invalid_sensor_or_time_never_emits_or_advances_policy_state() {
        let mut c = Controller::new(Config::default()).unwrap();
        at(&mut c, 10, 82);
        for (control, cpu) in [
            (0, 95_000),
            (82_000, 0),
            (-274_000, 40_000),
            (130_000, 130_000),
        ] {
            assert!(
                c.sample_with_cpu(Duration::from_secs(11), control, cpu)
                    .is_err()
            );
        }
        assert!(c.sample(Duration::from_secs(9), 70_000).is_err());
        assert_eq!(at(&mut c, 21, 82).pwm_percent, 0);
        assert_eq!(at(&mut c, 22, 82).pwm_percent, 35);
    }

    #[test]
    fn malformed_configuration_is_rejected_instead_of_partially_loaded() {
        for text in [
            "curve=70:35,60:50",
            "curve=70:81",
            "curve=70:35\ncurve=80:50",
            "idle_interval_ms=0",
            "curv=70:35",
            "start_hold_s=-1",
            "up_hold_s=121",
            "emergency_c=93",
            "emergency_c=18446744073709551615",
        ] {
            assert!(Config::from_text(text).is_err(), "accepted {text}");
        }
    }

    #[test]
    fn legacy_configuration_uses_new_hold_and_emergency_defaults() {
        let c = Config::from_text("curve=55:25,65:35,75:50,85:70,90:80\n").unwrap();
        assert_eq!(c.start_hold, Duration::from_secs(12));
        assert_eq!(c.up_hold, Duration::from_secs(20));
        assert_eq!(c.emergency_mc, 95_000);
        assert!(c.validate_for_hardware().is_err());
    }

    #[test]
    fn low_duty_curve_is_simulation_only() {
        let config =
            Config::from_text(include_str!("../tests/fixtures/low-duty-simulation.conf")).unwrap();
        assert!(config.validate_for_hardware().is_err());
        let mut c = Controller::new(config).unwrap();
        assert_eq!(at(&mut c, 0, 55).raw_duty, 23);
    }

    #[test]
    fn interior_pwm_values_are_rounded_for_ctr_91() {
        for (pwm, raw) in [(25, 23), (35, 32), (40, 37), (50, 46), (70, 64), (80, 74)] {
            let config = Config::from_text(&format!("curve=70:{pwm}\nstart_hold_s=0\n")).unwrap();
            let mut c = Controller::new(config).unwrap();
            assert_eq!(at(&mut c, 0, 70).raw_duty, raw);
        }
    }

    #[test]
    fn hardware_guard_cannot_be_configured_above_95_degrees() {
        let c = Config::from_text("emergency_c=96\n").unwrap();
        assert!(c.validate_for_hardware().is_err());
    }

    #[test]
    fn quiet_preset_ignores_79_and_brief_82_but_starts_after_twenty_seconds() {
        let d = crate::presets::Document::parse(include_str!("../config/presets.conf")).unwrap();
        let mut c = Controller::new(d.config(crate::presets::Preset::Quiet).clone()).unwrap();
        assert_eq!(at(&mut c, 0, 79).pwm_percent, 0);
        assert_eq!(at(&mut c, 300, 79).pwm_percent, 0);
        assert_eq!(at(&mut c, 301, 82).pwm_percent, 0);
        assert_eq!(at(&mut c, 302, 79).pwm_percent, 0);
        assert_eq!(at(&mut c, 303, 82).pwm_percent, 0);
        assert_eq!(at(&mut c, 322, 82).pwm_percent, 0);
        assert_eq!(at(&mut c, 323, 82).pwm_percent, 35);
        assert_eq!(at(&mut c, 400, 82).pwm_percent, 35);
    }

    #[test]
    fn performance_cools_earlier_without_bypassing_the_95_degree_guard() {
        let d = crate::presets::Document::parse(include_str!("../config/presets.conf")).unwrap();
        let mut c = Controller::new(d.config(crate::presets::Preset::Performance).clone()).unwrap();
        assert_eq!(at(&mut c, 0, 82).pwm_percent, 0);
        assert_eq!(at(&mut c, 3, 82).pwm_percent, 35);
        assert_eq!(at(&mut c, 7, 82).pwm_percent, 35);
        assert_eq!(at(&mut c, 8, 82).pwm_percent, 80);
        assert_eq!(at(&mut c, 9, 95).pwm_percent, 80);
    }

    #[test]
    fn preset_change_keeps_emergency_latched_until_a_new_continuous_cooldown() {
        let mut c = Controller::new(Config::default()).unwrap();
        assert_eq!(at(&mut c, 0, 95).pwm_percent, 80);
        assert_eq!(at(&mut c, 1, 89).pwm_percent, 80);
        let d = crate::presets::Document::parse(include_str!("../config/presets.conf")).unwrap();
        c.reconfigure(d.config(crate::presets::Preset::Quiet).clone())
            .unwrap();
        assert_eq!(at(&mut c, 2, 94).pwm_percent, 80);
        assert_eq!(at(&mut c, 3, 89).pwm_percent, 80);
        assert_eq!(at(&mut c, 32, 89).pwm_percent, 80);
        assert_eq!(at(&mut c, 33, 89).pwm_percent, 40);
    }

    #[test]
    fn changed_policy_preserves_real_on_time_and_does_not_cut_a_running_fan() {
        let mut c = Controller::new(Config::default()).unwrap();
        at(&mut c, 0, 82);
        assert_eq!(at(&mut c, 12, 82).pwm_percent, 35);
        let config = Config {
            min_on: Duration::from_secs(100),
            down_hold: Duration::from_secs(1),
            ..Config::default()
        };
        c.reconfigure(config).unwrap();
        assert_eq!(at(&mut c, 14, 40).pwm_percent, 35);
        assert_eq!(at(&mut c, 111, 40).pwm_percent, 35);
        assert_eq!(at(&mut c, 112, 40).pwm_percent, 0);
    }

    #[test]
    fn identical_policy_reload_does_not_starve_the_pending_start() {
        let mut c = Controller::new(Config::default()).unwrap();
        at(&mut c, 0, 82);
        for s in 1..12 {
            c.reconfigure(Config::default()).unwrap();
            assert_eq!(at(&mut c, s, 82).pwm_percent, 0);
        }
        c.reconfigure(Config::default()).unwrap();
        assert_eq!(at(&mut c, 12, 82).pwm_percent, 35);
    }

    #[test]
    fn broker_observation_does_not_restart_an_already_latched_cooling_timer() {
        let mut c = Controller::new(Config::default()).unwrap();
        assert_eq!(at(&mut c, 0, 95).pwm_percent, 80);
        assert_eq!(at(&mut c, 1, 40).pwm_percent, 80);
        c.observe_emergency(Duration::from_secs(20));
        assert_eq!(at(&mut c, 31, 40).pwm_percent, 35);
        assert_eq!(at(&mut c, 45, 40).pwm_percent, 0);
    }
}
