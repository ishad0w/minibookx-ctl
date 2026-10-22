# minibookx-ctl

A Linux control utility for the **CHUWI MiniBook X U300**.

> [!WARNING]
> Experimental software, tested on one U300 revision with CachyOS and GNOME.
> Provided as-is, without warranty. See [tested hardware](docs/hardware/README.md).

## Features

- [Tablet mode](docs/guide/tablet-mode.md): folding disables the built-in keyboard
  and touchpad; GNOME handles rotation and the on-screen keyboard.
- [Fan profiles](docs/guide/configuration.md): Quiet, Balance and Performance,
  with editable curves and delays for brief temperature spikes.
- [Optional CPU power limits](docs/guide/configuration.md#cpu-power-limits):
  PL1/PL2 overrides, disabled by default.
- [Diagnostics](docs/guide/troubleshooting.md): check the service, active settings
  and missing graphics packages with `--doctor`.

One binary, one systemd service. The repository also has manual guides for
[60 Hz and startup display corruption](docs/guide/display.md),
[screen and boot-menu orientation](docs/guide/boot.md), and
[Intel video acceleration](docs/guide/graphics.md).

## Install

Check the [requirements and packages](docs/guide/install.md#requirements), then
run as your ordinary user:

```sh
curl -fsSL https://raw.githubusercontent.com/ishad0w/minibookx-ctl/main/install.sh | sh
```

The installer asks for sudo, creates the service account and starts the service.
Defaults are Balance fan control and tablet detection; CPU power limits stay
unchanged. Repeat the command to update while keeping your configuration.

```sh
minibookx-ctl --doctor
```

**[Installation and updates](docs/guide/install.md)** ·
**[Configuration](docs/guide/configuration.md)** ·
**[All documentation](docs/README.md)**

Display and desktop fixes are separate choices. Installing the controller does
not change panel timings, boot settings or distribution packages.

## Hardware notes

[Hardware and test results](docs/hardware/README.md) ·
[BIOS options](docs/hardware/bios.md) ·
[Embedded controller](docs/hardware/ec.md) ·
[Charging](docs/hardware/charging.md)

## License

[Apache-2.0](LICENSE).

[Third-party licenses and notices](THIRD_PARTY.md), including the separate i915 RFC.
