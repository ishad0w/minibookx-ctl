# Diagnostic fields

For finding a fault or collecting a report, start with
[troubleshooting](../guide/troubleshooting.md). This reference explains what
the reports establish and how to interpret their detailed fields.

## Doctor and Debug

Doctor summarizes service health, accepted settings, power limits, display
timing and optional graphics packages. Debug adds package versions, sensor and
battery readings, power metadata and hardware references. Both are read-only.

| Observation | Meaning and limit |
|---|---|
| Installed unit | A unit exists; it may not be running |
| Populated cgroup and recent policy record | Processes and recent status exist; physical behavior still needs checking |
| Accepted config | Last validated worker settings, not necessarily the current file on disk |
| Fan control disabled | Firmware controls the fan; missing EC readings are intentional |
| Active display mode | Needs permission to read GPU debugfs; reports timing from a recognized active CRTC, not advertised modes |
| Intel driver binding | Sysfs associates the GPU with a driver; an application can still use software rendering |
| Package advice | Candidates for a recognized OS/version; not proof of acceleration |

The report does not parse daemon configuration as root. It reads bounded worker
status; an invalid or newer on-disk edit is not treated as an applied policy.
An invalid reload keeps the previous settings. A changed `fan_control` requires
a restart and causes the whole reload to be rejected until then.

Package profiles cover Debian 13, Ubuntu 24.04/26.04, Fedora 43/44 and
Arch/CachyOS rolling releases. Unknown versions remain unknown; `ID_LIKE`
does not select an unverified recipe. Queries are fixed, bounded and read-only;
privileged reports drop the query child's privileges. A query failure does not
become a list of missing packages. The daemon and installer do not run them.

Debug performs no raw EC/I2C probing. Expected profile chip identities are
labelled as references, not detections. Serial numbers, MAC addresses and the
kernel command line are omitted; review reports before publishing them.

## Reading power diagnostics

| Field | Source and limitation |
|---|---|
| Accepted PL1/PL2 | Worker requests in the last accepted configuration |
| Saved pre-override PL1/PL2 | RAPL values captured before the first override, not a direct BIOS reading |
| Broker last readback | Last application/restoration observation, not a fresh sample |
| Current RAPL PL1/PL2 | Independent sysfs observation; not atomic with status records |
| Time windows | Current constraint windows when exposed by the kernel |
| Package enabled | Package powercap flag, not independent proof of every PL1/PL2 property |
| Firmware lock / individual enable flags | Unknown through the fields used by this report |

With `power_control = false`, no override baseline is captured, so an unavailable
baseline is expected. `firmware` selects the saved pre-override value; other
system software may have changed RAPL since BIOS initialization.

Broker records are historical. An independent recovery after process loss may
be newer than the last record; a leftover `restoration_pending` does not prove
current ownership. Consult the journal if active policy cannot be confirmed.
Matching values establish numeric readback, not enforcement or thermal behavior.
The controller does not write enable flags, time windows, MSRs or BIOS Setup.

See [power configuration](configuration.md#power-limits),
[Linux powercap](https://docs.kernel.org/power/powercap/powercap.html) and
[Intel RAPL](https://github.com/torvalds/linux/blob/master/drivers/powercap/intel_rapl_common.c).

## Service status

`minibookx-ctl --status` combines records that update separately and may span
adjacent samples:

| Record | Contents |
|---|---|
| `policy` | Accepted settings, temperatures and requested fan target |
| `fan` | Enabled state, applied duty and independent high-temperature override |
| `power` | Broker baseline, last readback and recovery attribution |
| `tablet` | Posture decision, angle and reliability |

`target_pwm_percent` is requested electrical duty, not RPM.
`policy_applied_raw` and `policy_applied_pwm_percent` are the last acknowledged
command; `fan.actual_raw` may be newer. `duty_writes` counts acknowledged
worker-request writes, excluding autonomous broker actions. `fan_speed_ec`
is the EC-derived tachometer field.

With `fan_control = false`, `fan.enabled` and `manual` are false and
`fan.actual_raw` is null. This does not mean the physical fan is stopped.
Fan targets and temperature fields are not live measurements in this mode.

With managed control enabled, the broker checks tach feedback on its
[health timer](../development/design.md#sampling-and-fan-decisions).
Worker observations are cached; `tach_state` and `feedback_age_ms`
distinguish fresh from unknown/older feedback. The independent temperature
guard has its own timer.

A stopped service reports `active: false`, `state: "not-running"`.
`starting-or-recovering` means a complete active snapshot is unavailable.
Leftover files alone do not establish liveness.

### Final tablet state

| `tablet.reason` | Evidence |
|---|---|
| `StoppedOffAcknowledged` | Guardian confirmed OFF through its live descriptor, writing an event if needed |
| `StoppedGuardianExited` | Guardian completed its successful OFF/shutdown path before a separate acknowledgement reached the broker |
| `StopUnconfirmed` | Neither proof is available; `tablet` is null and `reliable` is false |

An OFF acknowledgement does not prove desktop consumption. A later cleanup
error still fails shutdown. Final records may remain, but ordinary status
reports the stopped state rather than presenting them as live. See
[tablet recovery](../development/design.md#tablet-switch-recovery).

During normal operation, tablet-mode transitions update the tablet record
immediately. Angle, reliability and reason changes are coalesced into snapshots
at most five seconds apart; diagnostic churn does not send another switch
command. Sensor/lid errors send OFF and report `Unavailable`; detection remains
disabled until restart while cooling continues. Unconfirmed OFF still triggers
service-wide recovery.

A temporary write failure can leave an older record visible. The broker keeps
the newest state and [retries publication](../development/design.md#config-and-status-files)
even with fan control or tablet detection disabled. Successful publication is
not required for recovery.

## Suspend mode

To inspect the selected Linux sleep mode without suspending:

```sh
cat /sys/power/mem_sleep
```

The selected mode is in brackets. A selected `deep` mode alone does not prove
that a suspend happened; use the kernel journal's suspend/resume entries when
comparing with the recorded S3 tests.

## Policy inspection without control

```sh
minibookx-ctl --inspect
minibookx-ctl --capabilities
sudo -u minibookx-ctl /usr/local/bin/minibookx-ctl --monitor \
  --config /etc/minibookx-ctl/config.conf --samples 10
```

Monitor prints CSV decisions without applying duty. From a source checkout,
an offline simulation can use supplied temperatures:

```sh
printf 'seconds,celsius\n0,68\n1,82\n13,82\n14,82\n' |
  minibookx-ctl --simulate --config config/presets.conf
```
