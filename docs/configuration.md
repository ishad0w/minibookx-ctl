# Configuration

Edit `/etc/minibookx-ctl/config.conf` to choose the enabled features and profile.
The complete defaults are in [config/presets.conf](../config/presets.conf).

```sh
sudoedit /etc/minibookx-ctl/config.conf
sudo -u minibookx-ctl /usr/local/bin/minibookx-ctl \
  --check-config /etc/minibookx-ctl/config.conf
minibookx-ctl --status
```

Profile, curve and power settings reload automatically. Changing
`fan_control` requires a service restart; a reload with a different value rejects
the whole update and logs that a restart is required. An invalid, empty or
unreadable update keeps the last accepted configuration. Every profile is checked,
including inactive ones. Keep the file root-owned and readable by the
`minibookx-ctl` group; use `sudoedit` rather than making it writable by the
service account.

## Leave the fan under firmware control

`fan_control = true` is the default. Set the top-level key to `false` if you want
tablet detection without managed fan control, for example when your kernel does
not provide writable EC access. The installed sample at
`/usr/local/share/doc/minibookx-ctl/presets.conf` includes this startup setting.

On a first installation, download the [installer](../packaging/README.md#install-and-start)
and install without starting the service:

```sh
sh install.sh --no-enable
sudoedit /etc/minibookx-ctl/config.conf
```

Change the existing top-level line, before any profile section. If an update
preserved an older configuration without that key, add it once before the first
`[balance]`, `[quiet]` or `[performance]` section:

```ini
fan_control = false
```

Keep `power_control = false` unless you separately intend to override CPU power
limits. Validate the complete file, then start:

```sh
sudo -u minibookx-ctl /usr/local/bin/minibookx-ctl \
  --check-config /etc/minibookx-ctl/config.conf
sudo systemctl enable --now minibookx-ctl.service
minibookx-ctl --status
```

For an already running installation, edit and validate the same file, then use
`sudo systemctl restart minibookx-ctl.service`. Restart first returns any managed
fan to firmware. Check `fan.enabled: false` and
`policy.accepted_config.fan_control: false` in status; an on-disk edit alone does
not establish that the new mode is active.

With fan control disabled, the service does not prepare or access the EC and
does not require CPU-temperature or tachometer readings. The firmware keeps its
own fan policy; this setting does not request a stopped fan or a particular RPM.
Tablet sensing, logind sleep handling and independently enabled power limits
remain available. Their own kernel and desktop prerequisites still apply.
Pending recovery of earlier fan ownership is always attempted before startup;
disabling this feature cannot bypass a failed restoration.

## Fan profiles

The following settings apply when `fan_control = true`. They remain valid in
the configuration while fan control is disabled, but do not drive the fan.

Change the existing `preset` line to select a profile:

```ini
preset = quiet
```

| Profile | Temperature °C → PWM % | Start delay | Increase delay | Cooling delay |
|---|---|---:|---:|---:|
| `balance` | 70→35, 85→40, 90→50, 93→70 | 12 s | 20 s | 30 s |
| `quiet` | 80→35, 88→40, 92→50, 94→70 | 20 s | 30 s | 45 s |
| `performance` | 50→35, 55→50, 65→60, 75→70, 82→80 | 3 s | 5 s | 20 s |

The fan stays off below the first threshold. Brief temperature spikes do not
start it: the threshold must remain exceeded for the start delay. Increasing
speed also requires sustained heat; each higher threshold has its own timer.
Once running, normal changes ramp towards the target.

For example, a brief 82°C spike in Balance leaves a stopped fan off. Sustained
82°C starts it at 35% after 12 seconds. It does not request 40% until the CPU
stays at or above 85°C for the increase delay.

All three profiles use 5°C cooling hysteresis and a 45-second minimum run time.
This prevents repeated starts and stops near a threshold. Cooling must cross the
hysteresis boundary and satisfy the cooling delay before reducing speed.

**PWM is electrical duty, not percentage of maximum RPM.** Live control accepts
35–80% duty; lower starting speeds have not been tested. Firmware duty steps
also mean the applied percentage is approximate. See the
[EC reference](hardware/ec.md) for the conversion and tachometer field.

### Edit a curve

Replace `curve` within the chosen profile. Use ascending temperature thresholds
and nondecreasing PWM values, for example:

```ini
[balance]
curve = 70:35,85:40,90:50,93:70
```

This is an excerpt: keep the profile's remaining settings in the file and do not
add a second section or duplicate key.

| Setting | Meaning | Accepted for live control |
|---|---|---|
| `curve` | `temperature_c:pwm_percent` pairs | 1–16 steps; ascending 20–100°C thresholds below `emergency_c`; nondecreasing 35–80% PWM |
| `hysteresis_c` | Temperature margin before cooling down | 1–15°C |
| `start_hold_s` | Sustained heat before starting | 0–120 s |
| `up_hold_s` | Sustained heat before increasing speed | 0–120 s |
| `down_hold_s` | Sustained cooling before reducing speed | 0–120 s |
| `min_on_s` | Minimum run time before normal shutdown | 0–300 s |
| `emergency_c` | Temperature that bypasses normal delays | 85–95°C, above the last curve threshold |
| `idle_interval_ms` | Temperature sampling at stable cold idle | Between `active_interval_ms` and 10000 ms |
| `active_interval_ms` | Temperature sampling while heating or running | At least 1000 ms, no greater than `idle_interval_ms` |

### High-temperature behavior

At the default emergency threshold of 95°C, the policy bypasses normal delays
and requests approximately 80% PWM. It stays in this state until continuously
below 90°C for 30 seconds. A lower configured emergency threshold moves this
release point down by the same amount.

The privileged broker also samples CPU temperature and can override lower fan
requests independently at 95°C. Its override requires 30 continuous seconds
below 90°C before release, even if the worker missed the original heat pulse.
The worker observes this override and preserves its configured minimum run time.
Sampling still has latency. CPU thermal protection must
remain enabled; see [recovery limits](design.md#recovery-and-sleep).

## Optional package power limits

`power_control = false` is the default. Selecting Performance alone changes the
managed fan curve and leaves CPU power limits unchanged. Power control is
independent of `fan_control`. The tested laptop already has
PL1/PL2 configured in BIOS; these are its baseline power policy, not missing
features that the daemon must replace.

PL1 controls the sustained package limit; PL2 controls the short-term limit.
To enable writes, review the limits below and set `power_control = true` in the
same root-owned configuration. The single shipped service grants the broker
access to the fixed CPU-package limit files; no extra service or drop-in is needed.

| Profile | Shipped PL1 / PL2 | Meaning when power control is enabled |
|---|---|---|
| Balance | `firmware` / `firmware` | Restore the limits captured before this service's override |
| Quiet | `firmware` / 15 W | Keep the captured PL1 and request 15 W PL2 |
| Performance | 28 / 30 W | Untested example; review before enabling |

Each profile accepts whole watts or `firmware` for `pl1_w` and `pl2_w`.
`firmware` refers to RAPL values captured before this service's first override,
not a direct reading of BIOS Setup. Another system component may have changed
them since firmware initialized them. The baseline is not captured while power
control remains disabled. The override and its restorable baseline must satisfy
PL1 5–28 W, PL2 5–30 W and PL1≤PL2; an out-of-envelope baseline refuses opt-in.
These bounds do not establish that the cooling system can sustain them.

Stop and suspend restore the captured limits. Detected conflicting limits cause
a fault, but sysfs checks cannot eliminate races with another writer. Run only
one power-limit controller; see [power recovery](design.md#power-limit-recovery).
PL4, voltage, TCC and GPU frequency are not controlled by this service. It does
not change enable flags,
time windows, MSRs or BIOS settings. Numeric readback verifies the written value,
not the hardware's complete limiting behavior. See [power diagnostics](diagnostics.md#reading-power-diagnostics).

The write/restore policy is tested against temporary filesystem fixtures,
including an interrupted two-file update. A real RAPL write/restore cycle under
the new systemd unit has **not** been tested on the laptop. Keep power control off
until you deliberately qualify that feature; the supplied 28/30 W Performance
values are not a tested thermal recommendation.

## Check configuration and behavior

Use [Doctor, Debug and service status](diagnostics.md) to distinguish the
last accepted configuration from on-disk edits and observed hardware state.
