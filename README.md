# minibookx-ctl

A Linux control utility for the **CHUWI MiniBook X U300**.

> [!WARNING]
> Experimental software, tested on one MiniBook X U300 revision. Provided as-is,
> without warranty. Use at your own risk.

**[Install and update](packaging/README.md)** ·
**[Manual installation](packaging/manual-install.md)** ·
**[Configure profiles](docs/configuration.md)**

For a new CachyOS/GNOME installation, follow the
**[checked setup from start to finish](docs/cachyos.md)**, including the separate
Mutter, display and bootloader steps.

Install the latest static binary and start its systemd service:

```sh
curl -fsSL https://raw.githubusercontent.com/ishad0w/minibookx-ctl/main/install.sh | sh
```

Run this as your ordinary user; installation requests sudo. The
[installation guide](packaging/README.md) lists prerequisites and shows how to
review the script before running it. Repeating the command updates the binary
and preserves your configuration.
To install before choosing which hardware features to enable, use
[`--no-enable`](packaging/README.md#install-and-start). The
[setup checklist](docs/README.md#choose-what-to-set-up) links the separate manual
steps for the controller, desktop and display.

## Features

- **[Fan control](docs/configuration.md#fan-profiles)** — Quiet, Balance and Performance profiles, with custom curves
  and delays to keep brief temperature spikes quiet; or
  [leave the fan to firmware](docs/configuration.md#leave-the-fan-under-firmware-control).
- **[Tablet mode](docs/tablet-mode.md)** — disables the built-in keyboard and touchpad when folded;
  enables GNOME's touch interface and on-screen keyboard.
- **[Power profiles](docs/configuration.md#optional-package-power-limits)** — optional PL1/PL2 limits, disabled by default.
- **[Background service](docs/design.md#recovery-and-sleep)** — starts with systemd, reloads profile settings and
  returns a managed fan to firmware before sleep or shutdown.
- **[Doctor](docs/diagnostics.md#doctor)** — read-only checks of service health,
  configuration, power limits, display mode and graphics packages, with distribution-specific advice;
  detailed [debug reports](docs/diagnostics.md#technical-debug-report)
  with JSON output.

## Fixes and setup guides

The [display guide](docs/gpu.md) covers startup corruption, uneven video playback,
bootloader rotation and desktop auto-rotation. These are manual changes:

- [60 Hz for smoother 60 fps video, using stock i915 on the tested panel](docs/display-driver-fix.md)
- [Screen orientation after unfolding and desktop rotation](docs/tablet-mode.md#sideways-display-after-unfolding)
- [GRUB/Limine orientation and quiet startup](contrib/bootloaders/README.md)
- [Distribution packages and Intel video acceleration](docs/distributions.md)
- [Hardware, embedded controller, BIOS and charging](docs/hardware/README.md)
- [All documentation](docs/README.md)

The [i915 timeout RFC](contrib/i915/README.md) is a separate developer proposal;
it is not a tested replacement driver.

Hardware testing uses CachyOS/GNOME. Container checks do not extend
that hardware support to other models.

## License

[Apache-2.0](LICENSE).

Dependencies have their own [licenses and notices](THIRD_PARTY.md).

The separate [i915 RFC](contrib/i915/README.md#license) uses MIT terms matching
the affected upstream files; preserve their notices.
