# Documentation

Start with **[installation](guide/install.md)**. Then choose the features or
fixes you need. Fan control and tablet detection come with the service; display,
desktop and bootloader changes are manual.

## User guides

| I want to… | Guide |
|---|---|
| Install, update or remove the utility | [Installation](guide/install.md) |
| Change fan behavior or CPU power limits | [Configuration](guide/configuration.md) |
| Use the laptop as a tablet | [Tablet mode and desktop setup](guide/tablet-mode.md) |
| Try 60 Hz or fix startup display corruption | [Display setup](guide/display.md) |
| Correct screen, console or boot-menu orientation | [Boot settings](guide/boot.md) |
| Check Intel video acceleration and packages | [Graphics](guide/graphics.md) |
| Find out why something is not working | [Troubleshooting](guide/troubleshooting.md) |

Each guide includes distribution-specific steps where needed. Quiet startup,
menu colors and timeout are [optional preferences](guide/boot.md#optional-appearance).

## Technical reference

For custom installations and a closer look at the hardware or reported values.
You do not need to read these to use the controller.

| Reference | Contents |
|---|---|
| [Manual file installation](reference/manual-install.md) | Account, file permissions, systemd unit and manual updates |
| [Configuration keys](reference/configuration.md) | Accepted values, timers and power-limit semantics |
| [Diagnostic fields](reference/diagnostics.md) | Status records, readback and their limits |
| [Kernel requirements](reference/kernel-support.md) | EC access, driver availability and debugfs |
| [Hardware](hardware/README.md) | Supported revision, requirements, feature limits, BIOS, EC and charging |
| [Panel](hardware/panel.md) | Timings, VBT, i915 readiness, GuC/HuC and FBC |
| [Posture sensors](hardware/tablet-sensors.md) | Detection, ambiguous positions and sensor initialization |

## Development

- [Architecture and recovery](development/design.md)
- [Building, testing and releases](development/maintenance.md)
- [i915 timeout RFC](../contrib/i915/README.md) — an unbuilt developer proposal
- [Security](../SECURITY.md)
