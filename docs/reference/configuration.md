# Configuration keys

For ordinary changes, use the [configuration guide](../guide/configuration.md).
[config/presets.conf](../../config/presets.conf) is the canonical default template.

## File and reload behavior

The installed file is `/etc/minibookx-ctl/config.conf`, owned by
`root:minibookx-ctl`, mode 0640. The worker parses it after dropping privileges.
Unknown or duplicate keys, duplicate sections, empty files and invalid profiles
are rejected, including errors in inactive profiles. An invalid reload retains
the whole last accepted configuration.

| Top-level key | Values | Application |
|---|---|---|
| `preset` | `balance`, `quiet`, `performance` | Automatic reload |
| `fan_control` | `true` or `false` | Service restart required |
| `power_control` | `true` or `false` | Automatic reload |

A reload that changes `fan_control` rejects the entire update until restart.
With fan control disabled, curve settings are still validated, but the service
does not prepare or access the EC or require temperature/tachometer readings.
Recovery of earlier attributed fan ownership is still required.

## Fan settings

Each named profile has these keys:

| Key | Meaning | Accepted range |
|---|---|---|
| `curve` | `temperature_c:pwm_percent` pairs | 1–16 steps; ascending 20–100°C thresholds below `emergency_c`; nondecreasing 35–80% duty |
| `hysteresis_c` | Cooling margin below a threshold | 1–15°C |
| `start_hold_s` | Sustained heat before starting | 0–120 s |
| `up_hold_s` | Sustained heat before increasing speed | 0–120 s |
| `down_hold_s` | Sustained cooling before reducing speed | 0–120 s |
| `min_on_s` | Minimum run time before normal shutdown | 0–300 s |
| `emergency_c` | Bypass normal delays | 85–95°C, above the last curve threshold |
| `idle_interval_ms` | Sampling at stable cold idle | Between `active_interval_ms` and 10000 ms |
| `active_interval_ms` | Sampling while heating or running | At least 1000 ms, no greater than `idle_interval_ms` |

Each higher curve threshold has its own heat timer. Cooling requires both the
hysteresis margin and cooling delay; stopping also respects minimum run time.
The actual thresholds and delays come from the selected profile, so a short
spike need not start a stopped fan.

At `emergency_c`, policy requests approximately 80% duty. Release requires
30 continuous seconds at least 5°C below that threshold. The broker has an
independent 95°C override, released after 30 continuous seconds below 90°C.
Sampling has latency; hardware thermal protection must remain enabled.
[Architecture](../development/design.md#sampling-and-fan-decisions) describes
the independent checks, and the [EC reference](../hardware/ec.md) explains duty
conversion and feedback.

## Power limits

Each profile accepts whole watts or `firmware` for `pl1_w` and `pl2_w`.
When `power_control = true`, the broker accepts PL1 5–28 W, PL2 5–30 W and
PL1 ≤ PL2. Both the requested values and the restorable baseline must satisfy
these bounds. The limits are software bounds, not a thermal qualification.

`firmware` means the RAPL values captured before the service's first override;
it is not a BIOS Setup reading. Another component may already have changed them.
No baseline is captured while power control remains disabled.

Stop and suspend restore the captured limits. Conflicting changes fault the
service, although sysfs cannot eliminate races with another writer. This feature
does not change enable flags, time windows, MSRs, PL4, voltage or GPU frequency.
Numeric readback does not prove actual power enforcement.

Write/restoration has filesystem-fixture tests, but no live RAPL override cycle
under the current unit has been qualified on the laptop. See
[power recovery](../development/design.md#power-limit-recovery) and
[diagnostic fields](diagnostics.md#reading-power-diagnostics).
