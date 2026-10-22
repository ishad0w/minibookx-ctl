# Intel graphics and video playback

The U300 uses the kernel's i915 driver, Mesa for graphics and Intel's media
driver for hardware video decoding. These packages are separate from the
controller. Start with:

```sh
minibookx-ctl --doctor
```

Doctor lists missing packages for recognized distributions. Install what you
need from the sections below; there is no reason to replace a working stack.
A Wayland desktop does not need the legacy `xf86-video-intel` Xorg driver.

## Arch and CachyOS

```sh
sudo pacman -Syu --needed mesa vulkan-intel intel-media-driver libva-utils linux-firmware-intel
```

`libva-utils` supplies `vainfo`, an optional diagnostic tool. The other packages
provide rendering, video decoding and Intel firmware.
[Arch reference](https://wiki.archlinux.org/title/Intel_graphics).

## Debian 13

```sh
sudo apt install libgl1-mesa-dri mesa-vulkan-drivers vainfo firmware-intel-graphics
```

The firmware package needs the `non-free-firmware` repository component.
For the media driver, choose **one** variant if neither is already installed:

| Package | Repository | Build |
|---|---|---|
| `intel-media-va-driver` | main | Free Kernel |
| `intel-media-va-driver-non-free` | non-free | Full Feature, broader codec support |

Install the chosen package with `sudo apt install <package>`. They conflict,
so review apt's transaction if switching variants.
[Firmware](https://packages.debian.org/trixie/firmware-intel-graphics) ·
[Free media driver](https://packages.debian.org/trixie/intel-media-va-driver) ·
[Full media driver](https://packages.debian.org/trixie/intel-media-va-driver-non-free).

## Ubuntu 24.04 and 26.04

```sh
sudo apt install libgl1-mesa-dri mesa-vulkan-drivers vainfo linux-firmware
```

For the media driver, choose `intel-media-va-driver` from `universe` or
`intel-media-va-driver-non-free` from `multiverse`. As on Debian, install one
variant, not both, and review replacements if switching.

Ubuntu 26.04 splits Intel graphics firmware into
`linux-firmware-intel-graphics`; keep the normal `linux-firmware` metapackage.
[Media packages](https://packages.ubuntu.com/en/intel-media-va-driver) ·
[Firmware package](https://packages.ubuntu.com/en/resolute-updates/linux-firmware-intel-graphics).

## Fedora 43 and 44

```sh
sudo dnf install mesa-dri-drivers mesa-vulkan-drivers \
  libva-intel-media-driver libva-utils intel-gpu-firmware
```

Fedora's media driver has a free codec build. Check `vainfo` before adding
third-party packages: codec availability depends on both the driver build and
the GPU. [Fedora package](https://packages.fedoraproject.org/pkgs/intel-media-driver-free/libva-intel-media-driver/fedora-44.html).

<details>
<summary>Optional broader codec support with RPM Fusion</summary>

If you need codecs absent from the native build, follow
[RPM Fusion's multimedia guide](https://rpmfusion.org/Howto/Multimedia) to enable
the appropriate repositories and review the packages you want:

| Choice | Purpose |
|---|---|
| `intel-media-driver` from RPM Fusion nonfree | Broader media-driver coverage; these releases package it alongside the Fedora driver in a separate directory |
| `libavcodec-freeworld` | Broader codecs alongside Fedora's FFmpeg libraries, with matching versions required |
| Full `ffmpeg` | Replaces the conflicting `ffmpeg-free` package |
| [OpenH264](https://fedoraproject.org/wiki/OpenH264) | H.264 support in the relevant application framework; it is not a VA-API driver |

Do not remove Fedora's media driver or replace Mesa just to add Intel decoding.
Review the package manager's proposed transaction. FFmpeg's command-line tool
is not a general requirement for browser acceleration. Flatpak applications
also use their own runtimes and codec extensions.

Packaging references: [Intel media driver](https://github.com/rpmfusion/intel-media-driver/blob/f44/intel-media-driver.spec),
[FFmpeg](https://github.com/rpmfusion/ffmpeg/blob/f44/ffmpeg.spec).

</details>

## Check playback

Run as your graphical-session user:

```sh
vainfo
```

This lists the media driver's supported profiles. Check your browser or player's
own media diagnostics while playing a video to see whether it actually uses
hardware decoding. Package presence alone cannot confirm that.

If decoding works but 60 fps video still looks uneven at 50 Hz, read the
[60 Hz display guide](display.md). A refresh-rate mismatch and a decoder problem
need different fixes.

There is no extra GuC/HuC or FBC override required by this setup. Details of
the tested driver's state are in the [panel and graphics reference](../hardware/panel.md).
Package recipes have container checks; physical graphics testing covers only
the [documented CachyOS system](../hardware/README.md).
