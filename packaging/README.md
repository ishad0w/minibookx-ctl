# Installation

The installation consists of one executable, one systemd service and one
configuration file. Check the [supported hardware](../docs/hardware/README.md)
and install the [runtime prerequisites](../docs/distributions.md#runtime-packages)
for your distribution first.

For the complete tested CachyOS/GNOME setup, use the
[clean-install walkthrough](../docs/cachyos.md). This page installs only the
controller; the walkthrough also covers Mutter, panel timing and the boot menu.

| Requirement | Details |
|---|---|
| System | x86_64 Linux, systemd, system D-Bus/logind and polkit for the standard delay inhibitor |
| Download tools | `curl`, CA certificates, `sha256sum` and `sudo` |
| Managed fan | Writable `ec_sys`, mounted debugfs and CPU temperature readings (`coretemp` on the tested U300) |
| Tablet detection | `i2c-dev`, `uinput` and the lid accelerometer's IIO driver |
| Desktop rotation | Correct [DRM panel mounting and supported compositor](../docs/tablet-mode.md#sideways-display-after-unfolding); the U300 mounting hint is a separate manual setting |

The release is static. Python, Git and a compiler are unnecessary for installation.
The installer does not install distribution packages, change GNOME settings,
modify boot entries or rebuild a driver.

## Install and start

Run as your ordinary user:

```sh
curl -fsSL https://raw.githubusercontent.com/ishad0w/minibookx-ctl/main/install.sh | sh
```

Alternatively, save and read the script first:

```sh
curl -fsSL https://raw.githubusercontent.com/ishad0w/minibookx-ctl/main/install.sh -o install.sh
less install.sh
sh install.sh
```

The script resolves the latest release once, downloads its executable and
checksum, and verifies the download without root. It then requests sudo to run
that executable's `--install --enable` mode. The executable contains the matching
unit, default configuration and license notices.

As with any downloaded installer, you are trusting the publisher. Checksums
detect inconsistent downloads; they are not independent signatures. See
[security](../SECURITY.md#installer-trust).

The binary creates a locked `minibookx-ctl` service account, installs the files,
then enables and starts `minibookx-ctl.service`. The default configuration selects
Balance, enables managed fan control and tablet detection, and leaves CPU power
limits unchanged.

```sh
minibookx-ctl --doctor
systemctl status minibookx-ctl.service --no-pager
sudo journalctl -u minibookx-ctl.service -b
```

To install without starting a first installation:

```sh
sh install.sh --no-enable
sudoedit /etc/minibookx-ctl/config.conf
sudo -u minibookx-ctl /usr/local/bin/minibookx-ctl \
  --check-config /etc/minibookx-ctl/config.conf
sudo systemctl enable --now minibookx-ctl.service
```

`--no-enable` preserves an existing installation's running/enabled state during
an update; it does not stop a service that was already running.
Use this first-install route to [leave fan control to firmware](../docs/configuration.md#leave-the-fan-under-firmware-control)
before the service starts. The normal start requires the managed-fan prerequisites;
the installer does not silently disable a feature when its driver is missing.

## Choose the controller features

Edit `/etc/minibookx-ctl/config.conf` with `sudoedit`. Profile, curve and
power settings reload automatically; changing `fan_control` requires a restart.
The [configuration guide](../docs/configuration.md) explains feature selection,
fan curves, profiles, delays and diagnostics.

To keep the firmware's fan policy, use the [fan-disabled setup](../docs/configuration.md#leave-the-fan-under-firmware-control).
To request CPU power limits, review [PL1/PL2 configuration](../docs/configuration.md#optional-package-power-limits)
before setting `power_control = true`. This is experimental; the supplied
Performance wattages have not been qualified on the laptop. Both choices use
the same config and service.

Desktop rotation, GPU packages, 60 Hz and bootloader settings are independent
manual changes. The [setup checklist](../docs/README.md#choose-what-to-set-up)
links each guide.

## Updates and a specific build

Repeat the installation command. Your configuration is preserved byte-for-byte;
the latest sample is installed as
`/usr/local/share/doc/minibookx-ctl/presets.conf`. Changes to existing systemd
drop-ins remain your responsibility.

To select a release explicitly:

```sh
sh install.sh --release v0.1.1
```

Each release uses a version tag such as `v0.1.1`; the binary also reports its
source commit in `--version` and `--build-info`. Choose an existing tag from
the [release list](https://github.com/ishad0w/minibookx-ctl/releases).

An invalid saved config blocks the update before the running controller is
stopped. Otherwise the installer stops the old service, finishes hardware
recovery and replaces its files. A recovery failure leaves the old files in
place; a later copy or startup failure attempts to restore them and the previous
service state.

Automatic rollback does not survive power loss or SIGKILL during installation.
Keep a [manual backup](manual-install.md#updates-and-rollback) when you need an
independent way back. See [installer security](../SECURITY.md#installer-trust)
for the validation and file checks.

## Local and offline installation

A downloaded executable can install its own matching files:

```sh
chmod +x ./minibookx-ctl-x86_64-linux
sudo ./minibookx-ctl-x86_64-linux --install --enable
```

Use the [manual guide](manual-install.md) to copy files without invoking the
binary's installer.

## Remove

```sh
sudo /usr/local/bin/minibookx-ctl --uninstall
```

Removal stops the service and verifies hardware recovery before deleting the
executable and unit. It preserves your configuration and locked service account.
It does not undo manually installed display or kernel changes.

## Installed paths

| Path | Purpose |
|---|---|
| `/usr/local/bin/minibookx-ctl` | Root-owned executable |
| `/etc/minibookx-ctl/config.conf` | Configuration, root:`minibookx-ctl`, mode 0640 |
| `/etc/systemd/system/minibookx-ctl.service` | The only controller unit |
| `/usr/local/share/doc/minibookx-ctl/` | Default config and license notices; use `--build-info` for build identity |
| `/run/minibookx-ctl/` | Within-boot ownership, recovery and status records |

systemd runs recovery after the main process exits, including a failed start.
Before the next activation, the same binary retries recovery before entering
manual control. Unresolved recovery records must be kept; deleting them is not
hardware recovery. See [service lifecycle](../docs/design.md#recovery-and-sleep).

On the supported revision, starting the service also wakes a base accelerometer
that booted in power-down mode. This needs no `i2c-tools` package or manual
register command. See [tablet troubleshooting](../docs/tablet-mode.md#troubleshooting)
if the service cannot validate the sensor.

## First-run verification

Begin with the supplied Balance configuration, ordinary laptop posture and
power control disabled. Avoid starting another fan or power writer alongside
the service. Check behavior separately before restoring custom settings.

| Check | Expected result |
|---|---|
| Ordinary laptop position | Built-in keyboard/touchpad usable |
| Near-flat hinge | Stable prior mode inside the estimated boundary gap; [posture limits](../docs/tablet-mode.md#positions-that-need-a-little-care) apply |
| Tent and fully folded tablet | Built-in input inhibited; touchscreen usable |
| Folded landscape/portrait, then unfold | Unlocked desktop rotation works; normal laptop input/orientation returns |
| Normal workload and idle | Fan follows the configured delays; no repeated service faults |
| Stop and start | Stop returns fan to firmware and normal input; start succeeds again |
| Suspend and resume | Input, orientation and controller operation return |

With `fan_control = false`, verify firmware fan behavior instead of expecting the
configured curve. Status must report the disabled feature; lack of an EC reading
in that mode is intentional.

Graphics changes have their own [cold-start and resume validation](../docs/display-driver-fix.md#validation-and-rollback).
A clean controller start does not validate panel output or video decoding.

Before reinstalling the OS, save custom config, local VBT inputs/manifest and a
known working recovery image outside the target filesystem. Apply the setup
steps to the new installation rather than restoring old services or an entire
`/etc`. A retained ESP can contain old kernel images and entries; verify the
selected entry really loads the new system before calling it a clean test.
See [troubleshooting](../docs/diagnostics.md#common-problems) if installation or
recovery refuses to continue.
