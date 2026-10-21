# Documentation

New CachyOS/GNOME installation? Start with the
[clean-install walkthrough](cachyos.md). It orders the controller, desktop,
60 Hz and Limine instructions used on the tested U300.

## Choose what to set up

The controller, desktop settings and display fixes are separate choices. Use
only the features you need; installing the controller does not apply the other
changes in this checklist.

| Task | Manual guide |
|---|---|
| Set up a fresh CachyOS/GNOME installation | [Complete walkthrough](cachyos.md) |
| Install, update or remove the controller | [Normal installation](../packaging/README.md), or [copy the binary, account and unit yourself](../packaging/manual-install.md) |
| Keep the fan under firmware control | [Disable managed fan control](configuration.md#leave-the-fan-under-firmware-control) while retaining tablet detection |
| Select or tune a fan profile | [Profiles, curves and delays](configuration.md#fan-profiles) |
| Opt into CPU power limits | [PL1/PL2 settings](configuration.md#optional-package-power-limits); disabled by default |
| Disable built-in input when folded; use rotation and the on-screen keyboard | [Desktop and tablet mode](tablet-mode.md) |
| Correct sideways orientation after unfolding | [Panel mounting and compositor requirements](tablet-mode.md#sideways-display-after-unfolding) |
| Try 59.94/60 Hz and address startup corruption | [VBT, early firmware and boot arguments](display-driver-fix.md#manual-vbt-setup) |
| Rotate the boot menu/console or make startup quiet | [GRUB and Limine](../contrib/bootloaders/README.md) |
| Install Intel graphics and video-decoding packages | [Distribution setup](distributions.md#intel-graphics-and-hardware-video-decoding) |
| Investigate corruption that remains at 60 Hz | [i915 developer RFC](../contrib/i915/README.md); an unbuilt proposal, not an installable fix |
| Check what is actually running | [Doctor, Debug and service status](diagnostics.md) |

Begin with the [runtime prerequisites](distributions.md#runtime-packages). For
display problems, [choose the relevant graphics guide](gpu.md#choose-the-right-fix)
before changing panel or kernel settings.

## Hardware and development

| Guide | Contents |
|---|---|
| [Hardware reference](hardware/README.md) | Tested revision, EC, BIOS and charging |
| [Building and maintenance](maintenance.md) | Source builds, release artifacts, CI scope and system updates |
| [Service architecture](design.md) | Privilege boundaries, hardware ownership and recovery |
| [Security](../SECURITY.md) | Installer trust and vulnerability reporting |
| [Third-party licenses](../THIRD_PARTY.md) | Bundled notices and corresponding sources |
