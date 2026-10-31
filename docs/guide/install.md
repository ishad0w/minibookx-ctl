# Installation

The same installer works across supported Linux distributions. It installs one
binary, one systemd service and a configuration file, with a locked service
account. It does not install packages or change your display and bootloader.

## Requirements

- The [supported MiniBook X U300 revision](../hardware/README.md), x86_64 Linux
  and systemd. Other MiniBook revisions have not been qualified.
- The packages below. Most are already present on a desktop installation.
- For fan control, a kernel that permits writable `ec_sys` access. If yours
  does not, you can still [install with firmware fan control](#without-fan-control).

Managed fan control also needs [debugfs mounted](../reference/kernel-support.md#debugfs),
which is normally already available on the tested desktop setup.

The prebuilt binary needs no compiler, kernel headers, DKMS, Python or Git.
GNOME/Wayland is the tested desktop; see [tablet mode](tablet-mode.md) for its
rotation requirements.

## Packages

Run the command for your distribution to install missing runtime and desktop
prerequisites. These do not include GPU packages or development tools.

### Arch and CachyOS

```sh
sudo pacman -Syu --needed curl ca-certificates coreutils sudo \
  systemd dbus polkit kmod shadow libinput iio-sensor-proxy
```

### Debian and Ubuntu

```sh
sudo apt update
sudo apt install curl ca-certificates coreutils sudo \
  systemd udev dbus polkitd kmod passwd libinput10 iio-sensor-proxy
```

### Fedora

```sh
sudo dnf install curl-minimal ca-certificates coreutils sudo \
  systemd systemd-udev dbus polkit kmod shadow-utils util-linux libinput iio-sensor-proxy
```

On Fedora, check fan support before starting: the regular kernel may lack
`ec_sys`. Use [firmware fan control](#without-fan-control) if it is unavailable.
The [kernel reference](../reference/kernel-support.md) explains how to check.
For immutable Fedora variants, use their own package-layering procedure.

## Install and start

Leave the laptop in its ordinary working position. Run as your ordinary user:

```sh
curl -fsSL https://raw.githubusercontent.com/ishad0w/minibookx-ctl/a3e6c07d64128a3add1b9490c17518b4b73b3d0d/install.sh | sh
```

The bootstrap URL pins a reviewed source commit. It downloads the latest
release and checks its checksum, then asks for
sudo to install and start it. If you prefer to read the script first:

```sh
curl -fsSL https://raw.githubusercontent.com/ishad0w/minibookx-ctl/a3e6c07d64128a3add1b9490c17518b4b73b3d0d/install.sh -o install.sh
less install.sh
sh install.sh
```

As with other downloaded software, you are trusting the publisher; a checksum
is not an independent signature. [Security details](../../SECURITY.md#installer-trust).

The defaults enable **Balance fan control** and **tablet detection**. CPU power
limits remain unchanged. Check that the service started:

```sh
minibookx-ctl --doctor
systemctl status minibookx-ctl.service --no-pager
```

If startup fails, read [troubleshooting](troubleshooting.md) before retrying.
The service initializes the supported base sensor automatically.

### Without fan control

To let firmware manage the fan, download the script above and install without
starting the service:

```sh
sh install.sh --no-enable
sudoedit /etc/minibookx-ctl/config.conf
```

Change the existing top-level setting to `fan_control = false`. Leave
`power_control = false` unless you intend to override CPU power limits. Then:

```sh
sudo -u minibookx-ctl /usr/local/bin/minibookx-ctl \
  --check-config /etc/minibookx-ctl/config.conf
sudo systemctl enable --now minibookx-ctl.service
minibookx-ctl --doctor
```

Tablet detection remains enabled. On updates, `--no-enable` preserves whether
the service was enabled and running. Updating stops it for recovery and restarts
it if it was running; this option is not a way to keep an active service stopped.

## Check the result

Try normal laptop use, then fold into a tablet and unfold again. Built-in input
should turn off while folded and return afterwards; touch stays available.
Follow [desktop setup](tablet-mode.md) if rotation or the on-screen keyboard needs
attention. Finally, test sleep and wake with your usual desktop controls.

The fan can stay off at cool idle. [Fan profiles](configuration.md#fan-profiles)
explain when it should start. Optional [60 Hz](display.md),
[boot rotation](boot.md) and [graphics packages](graphics.md) each have their own
guide; none is required just to install the controller.

## Update or remove

Repeat the installation command to update. Your configuration is preserved;
the latest example is saved at `/usr/local/share/doc/minibookx-ctl/presets.conf`.
To select a published version with the downloaded script:

```sh
sh install.sh --release v0.1.5
```

To remove the binary and service:

```sh
sudo /usr/local/bin/minibookx-ctl --uninstall
```

Removal preserves your config and locked service account. Manually applied
display changes remain. If recovery fails, keep the installed files and follow
[recovery troubleshooting](troubleshooting.md#recovery-failed).

For offline setup or copying the files yourself, use
[manual installation](../reference/manual-install.md).
