# Distribution setup

The prebuilt x86_64 Linux executable contains its userspace libraries. Running
it does not require Python, Git, Rust or a shared libdbus package. It still uses
the host's kernel drivers, systemd and system D-Bus.

Install missing prerequisites yourself using the commands below. The installer
creates the controller's account, files and service; it does not run a package
manager, change your desktop or configure the display driver.

| Task | Shared procedure | Distribution-specific prerequisite |
|---|---|---|
| Install, update or remove | The same static executable and built-in installer | Runtime packages and required kernel interfaces |
| Managed fan control | Optional feature of the same service/config | Writable EC access and CPU temperature readings |
| Tablet control | The same service, using the standard Linux switch | I2C/IIO drivers, uinput and desktop input support |
| Graphics and panel fixes | Separate manual guides | GPU packages, initramfs generator and bootloader configuration |

Package and installer checks cover five distributions. Physical fan, sensor,
display and sleep behavior has been tested only on the documented CachyOS U300.
For that system, the [clean-install walkthrough](cachyos.md) puts the separate
package, desktop and display procedures in order.

## Runtime packages

Most desktop installations already provide these packages. `curl`, CA
certificates, `sha256sum` from coreutils and `sudo` are used by the download
bootstrap. The service needs systemd/logind, system D-Bus, `modprobe` and account
management tools. The unprivileged worker obtains logind's standard sleep-delay
inhibitor through polkit; install the distribution policy service without custom
rules. Desktop tablet handling uses libinput; orientation sensing uses
`iio-sensor-proxy`.

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

These commands target a conventional writable Fedora installation. Use the
operating system's own package/layering procedure for an immutable variant.
`util-linux` supplies the account's `nologin` shell; see its
[package contents](https://packages.fedoraproject.org/pkgs/util-linux/util-linux/fedora-44.html).

**Fedora 44 kernel check:** the official repository query below found matching
loadable `ec_sys.ko` files only in `kernel-debug-modules-core`. The userspace
packages above do not supply this driver. Check the running kernel before
installing the controller; a package-file query does not identify built-in
drivers:

```sh
modinfo -k "$(uname -r)" ec_sys
dnf repoquery --whatprovides '*/ec_sys.ko*'
```

Managed fan control requires a kernel with `CONFIG_ACPI_EC_DEBUGFS` enabled and
writable EC access. Fedora's debug kernel is a possible manual kernel choice,
but it has not been tested on this laptop. Alternatively, a supporting build can
[leave the fan under firmware control](configuration.md#leave-the-fan-under-firmware-control)
and use the independent tablet features. Those still need their own drivers.
The installer does not select, install or rebuild a kernel. Successful Fedora
container checks establish neither EC access nor tablet behavior on the laptop.

### Arch and CachyOS

```sh
sudo pacman -Syu --needed curl ca-certificates coreutils sudo \
  systemd dbus polkit kmod shadow libinput iio-sensor-proxy
```

The clean CachyOS/GNOME installation already had these prerequisites. Its
separate desktop correction was [Mutter 51.1](tablet-mode.md#sideways-display-after-unfolding);
the only added graphics diagnostic was `libva-utils`. Check package versions
before assuming that a new image needs the same changes.

### Check the running kernel

Check the kernel you will actually boot, independently of the userspace package
list. These commands only inspect module metadata; they do not load modules or
write hardware:

```sh
uname -r
for module in ec_sys i2c_dev uinput coretemp mxc4005; do
    modinfo -k "$(uname -r)" -F filename "$module"
done
```

`coretemp` supplies CPU temperatures; `mxc4005` is the tested lid accelerometer's
IIO driver. `ec_sys` requires the kernel's
[`CONFIG_ACPI_EC_DEBUGFS` option](https://github.com/torvalds/linux/blob/master/drivers/acpi/Kconfig).
It is not supplied by installing `kmod`, GPU packages or kernel headers.
An available driver can be a module path or `(builtin)`. If built-in metadata is
unavailable, inspect that kernel's configuration and device interfaces rather
than assuming a failed `modinfo` query proves the driver is absent.

The requirements depend on the enabled features:

| Feature | Kernel interfaces |
|---|---|
| Managed fan (`fan_control = true`) | CPU temperatures, writable `ec_sys` and mounted debugfs |
| Firmware fan (`fan_control = false`) | No EC, debugfs, CPU-temperature or tachometer access by the service |
| Tablet detection | `i2c-dev`, `uinput` and the recognized lid IIO accelerometer |
| Optional PL1/PL2 | Recognized Intel package RAPL interfaces; disabled by default |

These versioned checks establish how particular kernels were configured, not
that every kernel flavor or later update has the same configuration:

| Distribution/kernel checked | EC debug interface | Evidence and limit |
|---|---|---|
| Debian 13, `6.12.113-1`, x86 | Built in: `CONFIG_ACPI_EC_DEBUGFS=y` | [Official kernel config](https://salsa.debian.org/kernel-team/linux/-/blob/debian/6.12.113-1/debian/config/kernelarch-x86/config); no separate `ec_sys.ko` is expected |
| Ubuntu 24.04, `6.8.0-31-generic` | Module: `CONFIG_ACPI_EC_DEBUGFS=m` | Config from the [official generic headers package](https://archive.ubuntu.com/ubuntu/pool/main/l/linux/linux-headers-6.8.0-31-generic_6.8.0-31.31_amd64.deb) |
| Ubuntu 26.04, `7.0.0-14-generic` | Module: `CONFIG_ACPI_EC_DEBUGFS=m` | Config from the [official generic headers package](https://archive.ubuntu.com/ubuntu/pool/main/l/linux/linux-headers-7.0.0-14-generic_7.0.0-14.14_amd64.deb) |
| Arch Linux, `linux 7.2.11` | `ec_sys.ko.zst` in the package | [Official package file list](https://archlinux.org/packages/core/x86_64/linux/files/); rolling package contents can change |
| CachyOS, tested U300 kernel | EC access worked on the documented hardware build | [Hardware qualification](hardware/README.md#hardware-qualification); not a claim about every CachyOS kernel |
| Fedora 44, regular kernel package query | No loadable `ec_sys.ko` found; debug variant provides it | Check the running config as well as the repository query above |

An enabled driver does not guarantee permission to write. Kernel lockdown and
other host policy may restrict the EC interface, including on Debian, Ubuntu,
Arch and CachyOS. Keep those policies under the administrator's control; do not
disable security features just because a package check passed. See the upstream
[EC interface](https://github.com/torvalds/linux/blob/master/drivers/acpi/ec_sys.c).

If a required driver is absent, check the distribution's module packages for
that exact kernel version and flavor, or select an appropriate maintained
kernel yourself. Debian and Ubuntu kernel/module packaging also varies by
flavor; the runtime package command alone does not establish driver availability.
Reboot into the selected kernel before repeating the check.

### Mount debugfs when needed

Managed fan control and manual VBT capture use debugfs. Check that it is mounted
at its exact mountpoint:

```sh
findmnt --mountpoint /sys/kernel/debug
```

If it is not mounted and your system permits it, start the standard systemd mount:

```sh
sudo systemctl start sys-kernel-debug.mount
```

Do not unmask a deliberately disabled mount or override host policy merely to
start this service. Firmware fan mode does not require a debugfs mount or use
the EC interface. The unit's optional private bind is described in
[the service design](design.md).

The service prepares `ec_sys` only for managed fan control, and `i2c-dev`/`uinput`
for tablet detection. It validates the recognized device identity and access.
Finding module metadata does not prove that those checks will succeed. The
controller does not compile missing drivers, and the service account needs no
raw-device group membership.

Continue with [installation and first start](../packaging/README.md). For
rotation, the on-screen keyboard and differences between GNOME and Plasma, see
[desktop and tablet mode](tablet-mode.md).

## Intel graphics and hardware video decoding

The U300 uses the in-tree i915 kernel driver, Mesa for rendering and Intel's
media driver through VA-API for video acceleration. These packages are separate
from the controller. Start with `minibookx-ctl --doctor`: it reports the OS
version, Intel GPU driver binding and package recommendations. Debug retains
the relevant installed package versions and codec details.
It recommends missing packages but never installs or replaces anything.

The package profiles cover Debian 13, Ubuntu 24.04/26.04, Fedora 43/44 and
Arch/CachyOS rolling releases. An unknown OS or version is reported without
guessing a recipe from `ID_LIKE`. These are package recommendations, not a claim
that each distribution has been tested on the laptop.

| Component | Purpose |
|---|---|
| Kernel i915 | Display and GPU access; normally supplied by the distribution kernel |
| Mesa OpenGL/Vulkan | Desktop and application rendering |
| Intel media driver / VA-API | Hardware video decoding and encoding, with codec support depending on the build |
| Intel graphics firmware | Firmware requested by the kernel driver |
| `vainfo` | Optional tool for inspecting the media driver's advertised profiles |

Use the recipe for your release when a package is missing. Package presence
does not establish that an application is using it. A working Wayland desktop
does not require the legacy `xf86-video-intel` Xorg driver.

### Debian 13

```sh
sudo apt install libgl1-mesa-dri mesa-vulkan-drivers vainfo
sudo apt install firmware-intel-graphics
```

`firmware-intel-graphics` requires the `non-free-firmware` repository component.
The `intel-media-va-driver` package is the Free Kernel build; the optional
`intel-media-va-driver-non-free` in `non-free` is the Full Feature build.
If neither variant is installed, choose one explicitly with
`sudo apt install intel-media-va-driver` or
`sudo apt install intel-media-va-driver-non-free`. The two media packages
conflict; review apt's proposed replacement if deliberately switching. Keep
your existing variant when only adding firmware or tools. Codec support depends
on both the GPU and the selected driver build.

References: [graphics firmware](https://packages.debian.org/trixie/firmware-intel-graphics),
[free media build](https://packages.debian.org/trixie/intel-media-va-driver),
[full media build](https://packages.debian.org/trixie/intel-media-va-driver-non-free).

### Ubuntu 24.04 and 26.04

```sh
sudo apt install libgl1-mesa-dri mesa-vulkan-drivers vainfo linux-firmware
```

Ubuntu 24.04 supplies graphics firmware through `linux-firmware`. In 26.04 the
Intel graphics payload is split into `linux-firmware-intel-graphics`;
`linux-firmware` remains the normal metapackage and should not be removed.
Doctor accepts either the split package or the metapackage on 26.04.

The free media build is in `universe`; the optional
`intel-media-va-driver-non-free` Full Feature build is in `multiverse`.
They are alternatives, not two packages to install together. If changing the
variant, let apt resolve the replacement and review the transaction. When
neither is installed, select one with `sudo apt install intel-media-va-driver`
or `sudo apt install intel-media-va-driver-non-free`. Skip this choice if your
existing media driver works and only the other packages are missing.

References: [media variants](https://packages.ubuntu.com/en/intel-media-va-driver),
[Intel graphics firmware](https://packages.ubuntu.com/en/resolute-updates/linux-firmware-intel-graphics),
[firmware metapackage](https://packages.ubuntu.com/en/linux-firmware).

### Fedora 43 and 44

```sh
sudo dnf install mesa-dri-drivers mesa-vulkan-drivers \
  libva-intel-media-driver libva-utils intel-gpu-firmware
```

Fedora supplies a free codec build of the
[Intel media driver](https://packages.fedoraproject.org/pkgs/intel-media-driver-free/libva-intel-media-driver/fedora-44.html).
This does not mean that every common codec is absent. Inspect the installed
build with `vainfo` before choosing extra codec packages.

If broader media-driver coverage is needed, RPM Fusion nonfree supplies
`intel-media-driver`. Enable that repository using its current instructions,
then review a manual `dnf install intel-media-driver` transaction. On these
Fedora releases its packaging supports coexistence with
`libva-intel-media-driver`, using a separate `dri-nonfree` library directory.
Do not blindly swap or remove the Fedora driver, and do not replace Mesa for
this Intel media-driver change. See the
[RPM Fusion package definition](https://github.com/rpmfusion/intel-media-driver/blob/f44/intel-media-driver.spec).

Additional application codecs are a separate choice:

| Package choice | What it changes |
|---|---|
| OpenH264 and its browser/GStreamer plugins | H.264 codec support through the relevant application framework; it does not enable VA-API or supply all video codecs |
| `libavcodec-freeworld` from RPM Fusion | Broader codec support alongside Fedora's FFmpeg libraries, subject to matching package versions |
| Full `ffmpeg` from RPM Fusion | An alternative to `ffmpeg-free`; these packages conflict and require a reviewed replacement transaction |

Follow [Fedora's OpenH264 guide](https://fedoraproject.org/wiki/OpenH264) or
[RPM Fusion's multimedia guide](https://rpmfusion.org/Howto/Multimedia) for the
choice you need. The [FFmpeg package definition](https://github.com/rpmfusion/ffmpeg/blob/f44/ffmpeg.spec)
records the conflict with the free variant. Installing the FFmpeg command-line
tool is not a general prerequisite for browser GPU decoding. Flatpak
applications also have their own runtimes and codec extensions; changing host
packages may not change their capabilities.

### Arch and CachyOS

```sh
sudo pacman -Syu --needed mesa vulkan-intel intel-media-driver libva-utils linux-firmware-intel
```

Use the normal [Intel graphics packages](https://wiki.archlinux.org/title/Intel_graphics)
for a Wayland desktop. A legacy Xorg driver is not a prerequisite for this setup.
Firmware availability alone is not a reason to force GuC/HuC module parameters.

## Check the result

```sh
minibookx-ctl --doctor
vainfo
```

Doctor uses read-only package-database queries and sysfs; it does not start a
renderer or decode a test video. A database-query failure is reported as unknown,
not as a list of missing packages. Package recommendations are optional desktop
setup, not service prerequisites.

Run `vainfo` as your graphical-session user. It reports supported VA-API
profiles, which still do not prove a particular player is using them. Check
your browser's own media diagnostics during playback for that. The
[graphics guide](gpu.md) separates decoder problems from panel problems.

A 50 Hz panel mode cannot show all 60 distinct frames in a 60 fps video each
second. The experimental [60 Hz timing option](display-driver-fix.md#refresh-rates)
addresses that mismatch; it does not add codec support. Panel timings, the i915
patch, initramfs changes and [bootloader settings](../contrib/bootloaders/README.md)
all have separate manual guides.
The controller installer does not change them.

## What CI checks

Development checks use seven disposable images across five distributions:

| Distribution | Image |
|---|---|
| Debian 13 | `debian:13-slim` |
| Ubuntu 24.04 | `ubuntu:24.04` |
| Ubuntu 26.04 | `ubuntu:26.04` |
| Fedora 43 | `fedora:43` |
| Fedora 44 | `fedora:44` |
| Arch Linux | `archlinux:base` |
| CachyOS | `cachyos/cachyos:latest` |

```sh
make distros
```

These checks install runtime prerequisites, run read-only reports and exercise
installation, config preservation, updates and removal. Account/config checks
use a real locked account with mocked service control, including validation of
`fan_control = false`. See [test coverage](maintenance.md#evidence-boundaries)
for the separation between containers, systemd VMs and physical hardware.

The package-advice check resolves Doctor's candidates against each image's
repositories and asks the package solver to calculate a transaction without
installing those GPU packages. It checks metadata for both APT media alternatives;
Fedora uses its native free stack. RPM Fusion transactions, codec coverage and
actual application acceleration are not tested. Python and diffutils are test
tools, not product dependencies. CI does not start the hardware service or modify
a bootloader.

Containers do not run the hardware service or qualify EC access, folding,
acceleration, boot or suspend. Physical testing remains limited to the documented
[U300 revision and software build](hardware/README.md#hardware-qualification).
