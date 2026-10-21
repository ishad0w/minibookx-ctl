# Diagnostics and troubleshooting

Start with Doctor. It is a read-only terminal report; it does not install
packages, repair configuration or change the hardware.

```sh
minibookx-ctl --doctor
```

These commands work after installation from any directory. No source checkout
is needed. Run as your ordinary user first. Restricted optional fields suggest
`sudo`; unavailable fields do not stop the remaining checks.

## Doctor

Doctor summarizes the service, last accepted profile, current power limits,
display mode, tablet integration and optional graphics packages. Problems and
manual next steps take priority over a full inventory.

```sh
minibookx-ctl doctor --json > doctor.json
```

| Observation | What it establishes |
|---|---|
| Installed unit | A service file exists; this alone does not mean it is running |
| Populated service cgroup and recent policy record | Processes and a recent status record exist; physical behavior still needs checking |
| Accepted configuration | The worker's last validated profile and requests, rather than the current file on disk |
| Fan control disabled | The accepted startup mode leaves the fan to firmware; missing EC readings are then intentional |
| Active display mode | Timing read from a recognized active CRTC; advertised connector modes are insufficient |
| Bound Intel driver | Sysfs associates the GPU with a driver; applications may still use software rendering |
| Optional package advice | Installed or missing candidates for the recognized OS/version; not controller prerequisites |

An invalid configuration reload keeps the previous accepted policy. Check the
service journal, correct the file using `sudoedit`, and validate it as the
service account before relying on the new settings:

```sh
sudo -u minibookx-ctl /usr/local/bin/minibookx-ctl \
  --check-config /etc/minibookx-ctl/config.conf
sudo journalctl -u minibookx-ctl.service -b
```

The report never parses daemon configuration as root. Its configuration summary
comes from bounded worker status. Current on-disk contents can be newer, invalid
or unreadable and are not treated as applied policy.
Changing `fan_control` needs a restart: the running worker rejects the entire
reload when that flag differs. See [firmware fan setup](configuration.md#leave-the-fan-under-firmware-control).

### Graphics recommendations

Package profiles cover Debian 13, Ubuntu 24.04/26.04, Fedora 43/44 and
Arch/CachyOS rolling releases. Unknown versions stay unknown rather than
inheriting an unverified recipe from `ID_LIKE`. A failed package query is not a
list of missing packages.

Queries use fixed, bounded, read-only package-database commands. Root reports
drop the query child's privileges. The daemon and installer never query or run
the package manager. Only selected graphics-package names are reported.

Graphics/media recommendations are optional to the controller. Missing FFmpeg
or OpenH264 does not make its service unhealthy. Follow
[distribution setup](distributions.md#intel-graphics-and-hardware-video-decoding)
for the package choices; Debug includes versions and codec details. Verify
actual playback with `vainfo` and the application's own diagnostics. Neither
package presence nor `vainfo` alone proves a browser is decoding the current
video on the GPU.

## Technical debug report

Use Debug when collecting a report for an issue:

```sh
minibookx-ctl --debug
minibookx-ctl debug --json > minibookx-debug.json
```

Debug retains the detailed configuration, package versions, power metadata,
EC references, battery measurements and sensor readings omitted from Doctor.
It performs no raw EC/I2C probing. An expected chip in the supported profile
is labelled as a reference, not a detected identity. Serial numbers, MAC
addresses and the kernel command line are omitted. Review the report before
sharing it.

### Reading power diagnostics

On the tested U300, BIOS defines PL1/PL2. With `power_control = false`, the
controller preserves the firmware/system policy. This is the default.

| Field | Source and limitation |
|---|---|
| Accepted PL1/PL2 | Requests in the worker's last accepted configuration |
| Saved pre-override PL1/PL2 | RAPL values captured by the broker before its first override; not a direct BIOS Setup reading |
| Broker last readback | Last values read during controller application/restoration; not a new hardware sample |
| Current RAPL PL1/PL2 | Independently observed sysfs values; reads are not atomic with status records |
| Time windows | Current constraint time windows, when exposed by the kernel |
| Package enabled | The package powercap flag; not independent evidence for every property of PL1 and PL2 |
| Firmware lock / individual enable flags | Unknown through the powercap fields used by this report |

Broker records are historical evidence, not live probes. When a fresh active
policy cannot be confirmed, ownership/restoration state remains unknown. An
independent recovery attempt after process loss may be newer than the last
published broker record; consult the journal rather than treating a leftover
`restoration_pending` value as current ownership.

`firmware` in configuration selects the saved pre-override RAPL value. Another
system component may have changed RAPL after firmware initialized it. No baseline
is captured if the controller has never enabled power control; `unavailable` is
then expected.

Matching numbers confirm numeric readback, not actual hardware enforcement,
thermal performance or the absence of another limiter. The controller changes
neither enable flags nor time windows and does not access MSRs or BIOS Setup.
A mismatch calls for checking service logs and other power-management software,
rather than starting competing writers. See [power configuration](configuration.md#optional-package-power-limits).

ABI references: [Linux powercap](https://docs.kernel.org/power/powercap/powercap.html)
and [Intel RAPL implementation](https://github.com/torvalds/linux/blob/master/drivers/powercap/intel_rapl_common.c).

## Service status

```sh
minibookx-ctl --status
systemctl status minibookx-ctl.service --no-pager
sudo journalctl -u minibookx-ctl.service -b
```

| Record | Contents |
|---|---|
| `policy` | Accepted profile, temperatures and requested fan target |
| `fan` | Whether managed fan control is enabled; applied duty and independent high-temperature override when enabled |
| `power` | Broker's saved RAPL baseline, last readback and recovery attribution |
| `tablet` | Posture decision, angle and reliability |

`target_pwm_percent` is requested electrical duty, not RPM.
`policy_applied_raw` and `policy_applied_pwm_percent` are the last
broker-acknowledged command. `fan.actual_raw` can be newer. `duty_writes` counts
writes acknowledged in response to worker requests, excluding autonomous broker
actions between them. `fan_speed_ec` is the EC's derived tachometer field.
Records update separately and may span adjacent sampling cycles.

With `fan_control = false`, `fan.enabled` is false, `manual` is false and
`fan.actual_raw` is null. The accepted config also records `fan_control: false`.
This means the service is not controlling or measuring the fan; it does not mean
the physical fan is stopped. Fan targets and temperature fields are not live
measurements in this mode. Tablet and optional power records remain separate.

With managed fan control enabled, the broker alone samples and checks tach
feedback, normally every five seconds.
Worker values are cached observations, not extra hardware reads. The broker's
`tach_state` and `feedback_age_ms` distinguish fresh feedback from an unknown or
older sample. The independent CPU temperature guard continues on its own timer.

A stopped service reports `active: false` and `state: "not-running"`.
`starting-or-recovering` means a complete active snapshot is unavailable.
Do not certify liveness from a leftover status file alone.

The broker's final tablet record distinguishes its evidence for releasing input:

| `tablet.reason` | Meaning |
|---|---|
| `StoppedOffAcknowledged` | The guardian confirmed tablet OFF through its live input descriptor, writing the event when needed |
| `StoppedGuardianExited` | The guardian completed its successful shutdown path, including OFF, before the broker received a separate acknowledgement |
| `StopUnconfirmed` | Neither proof is available; `tablet` is null and `reliable` is false |

An OFF acknowledgement confirms the switch is known OFF through the same live
descriptor; an already-OFF switch needs no repeated write. It does not confirm
desktop consumption. A later guardian cleanup error still fails shutdown even
if OFF was acknowledged.
Inspect the service journal for that error. Final records may remain after the
service exits; ordinary `--status` then reports the stopped state instead of
presenting those records as live.

## Common problems

| Symptom | Next step |
|---|---|
| Service fails to start | Read the journal; verify the supported revision and [runtime prerequisites](distributions.md#runtime-packages). Do not bypass identity checks |
| Config edit has no effect | Check reload errors and the last accepted profile; validate the file as the service account |
| Changing `fan_control` is rejected on reload | Validate the file, then restart the service; this is a startup setting |
| Kernel lacks writable EC access | Check [kernel support and host policy](distributions.md#check-the-running-kernel), or deliberately [keep firmware fan control](configuration.md#leave-the-fan-under-firmware-control) |
| Fan is silent below the first threshold | Expected policy behavior; review the [curve and start delay](configuration.md#fan-profiles) |
| Uncertain power write/restoration | Keep recovery records, stop competing power writers and inspect the journal |
| Built-in input stays disabled | Follow [tablet troubleshooting](tablet-mode.md#troubleshooting) |
| Startup display corruption | Follow the separate [graphics route](gpu.md#choose-the-right-fix) |
| Installer refuses a pending transaction | Preserve its files and finish recovery with the release that created that transaction; do not delete attribution to force an update |

To request firmware fan control and normal input:

```sh
sudo systemctl stop minibookx-ctl.service
```

If recovery fails, leave the service failed and preserve its journals for
investigation. Removing records is not hardware recovery. The limitations of
userspace recovery are described once in [the service design](design.md#recovery-and-sleep).

## Inspect without applying a policy

```sh
minibookx-ctl --inspect
minibookx-ctl --capabilities
sudo -u minibookx-ctl /usr/local/bin/minibookx-ctl --monitor \
  --config /etc/minibookx-ctl/config.conf --samples 10
```

Monitor prints CSV decisions without applying fan duty. For an offline curve
simulation, run from a source checkout:

```sh
printf 'seconds,celsius\n0,68\n1,82\n13,82\n14,82\n' |
  minibookx-ctl --simulate --config config/presets.conf
```
