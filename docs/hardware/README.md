# Hardware reference

Support is limited to one tested MiniBook X U300 revision. N100/N150 models,
other BIOS versions and replacement panels are not supported profiles. The
service checks the hardware profile before enabling control.

## Supported hardware

| Component | Tested hardware |
|---|---|
| Laptop | CHUWI MiniBook X |
| CPU | Intel U300 |
| BIOS | `ZW.RP11D.26060101.009` |
| Embedded controller | ITE IT5570, revision 02 |
| Panel | Portrait-native 1200×1920 MIPI-DSI; exact part number unknown |
| Panel link | Four lanes, RGB888 / 24 bpp |
| Lid accelerometer | MEMSIC-compatible `MDA6655`, Linux `mxc4005` driver / IIO |
| Base accelerometer | Second I2C accelerometer at address `0x15` |
| Keyboard | AT Translated Set 2, i8042 |
| Touchpad | `AMR12174:00 0911:5288`; also exposes a pointing-stick/mouse node |
| Touchscreen | USB NYM 869B-E112 |
| Audio | Realtek ALC269VC |
| PL1 / PL2 on the test unit | 12 W / 15 W |

A matching laptop name or EC chip is not enough: register behavior also depends
on firmware. Do not bypass the profile check to use another revision.

## Tested software

The clean-OS setup was checked on 22 October 2026 with these packages.

| Component | Version |
|---|---|
| Distribution | CachyOS |
| Kernel | `7.2.11-1-cachyos` |
| Desktop | GNOME 51.1, Wayland |
| Compositor | Mutter `51.1-1` from the enabled `extra` repository |
| Mesa / Intel Vulkan | 26.2.4 |
| VA-API | libva 2.24.1, intel-media-driver 26.3.5 |
| Intel firmware package | `linux-firmware-intel 20260916` |
| Bootloader | Limine 12.9.3 |
| Kernel-entry integration | limine-mkinitcpio-hook 1.40.0 |
| Service manager | systemd 262.1 |

## Hardware qualification

The following supervised checks used a completely reinstalled CachyOS/GNOME
system on 22 October 2026. Public `install.sh` installed controller build
`0.1.0+gb1a8bccb67d0`; its binary matched the published checksum. The
[walkthrough](../cachyos.md) links the manual steps used for desktop, display
and boot configuration.

The runtime implementation is retained for v0.1.1. These are physical results
for the recorded build, not a new hardware test of the repackaged binary.
Container and VM results remain [separate evidence](../maintenance.md#evidence-boundaries).

| Check | Result |
|---|---|
| Public installation | Public bootstrap installed the fixed build and enabled one service; locked account and default Balance verified; config and manual boot/display files preserved byte-for-byte on update |
| Privilege separation | Worker has no effective capabilities, supplementary groups or raw EC/I2C/uinput descriptors; `NoNewPrivs` is enabled |
| Base-sensor initialization | Starts from CONTROL `01`, wakes to `00` automatically; stopping restores `01` and clears ownership records; starting again detects folded posture |
| Native display setup | Unmodified packaged kernel/i915; 1200×1920 at 165920 kHz / approximately 60 Hz, RGB888 / 24 bpp; ordinary mkinitcpio inclusion and generated Limine entries |
| Panel mounting and boot menu | Correct panel mounting and Mutter 51.1; native 90° Limine menu without stretching; final quiet black setup has a one-second timeout |
| Cold power-off/button start | Service starts automatically; clean, correctly oriented login screen before any sleep |
| Ordinary laptop and approximately flat | Built-in keyboard and touchpad remain usable near 180°; the estimate is not a calibrated hinge measurement |
| Tent and folded tablet | Built-in input is inhibited; touchscreen, folded landscape/portrait rotation and on-screen keyboard work |
| Stop and restart while folded | Physical input returns on stop and is inhibited again after start |
| Unfolding | Normal laptop orientation, keyboard and touchpad return |
| Fan activation and release | 35% requested PWM gives plausible tachometer feedback and quiet, steady rotation; original Balance restored exactly and fan stops at cool idle |
| Deep/S3 suspend and automatic wake | About 28 seconds suspended; same broker PID and no restart; folded touch/OSK/input, 60 Hz output and subsequent unfolding work |
| Existing CPU power policy | Package RAPL and MMIO stay at PL1/PL2 12/15 W with unchanged windows; `power_control = false`, no override baseline or power lease |

After S3, tablet mode returned about 1.06 seconds after the kernel's resume exit,
including sensor startup and stable-pose confirmation. A transitional OFF sample
does not establish a failed resume. This timing is one observation, not a
latency guarantee.

The LTS 50 Hz profile's arguments, images and hashes were checked, but that LTS
boot was not physically repeated in this session. The final quiet cold boot
passed; Esc was not separately pressed in the new one-second window. The
`fan_control = false` startup path, RAPL overrides, raised limits and all
high-temperature fan stages remain outside this physical qualification.

## Compatibility notes

| Feature | Status |
|---|---|
| Fan profiles | Supported; higher-temperature stages have not all been load-tested |
| Tablet input and GNOME on-screen keyboard | Supported; see [tablet mode](../tablet-mode.md) |
| Deep/S3 suspend | Supported; fan control returns to firmware before sleep |
| Display fixes | Separate manual setup; see the [display guide](../gpu.md) |
| PL1/PL2 control | Optional; disabled by default. Existing-policy preservation is checked above; live overrides/restoration and raised limits remain untested on the hardware |
| PL4, voltage and GPU-frequency control | Not implemented |
| Battery charge limit | Not implemented; see [battery and charging](charging.md) |
| Keyboard-backlight brightness/timeout | No Linux setter in this utility; see [BIOS settings](bios.md) |

Suspend can log `xHC error in resume, USBSTS 0x401, Reinit` for USB controller
`0000:00:0d.0`. The controller reinitializes; this is separate from the DSI display
issue. Physical operation on other distributions and desktop environments has
not been tested; package/container checks are listed separately in
[distribution setup](../distributions.md).

## Firmware and controllers

| Reference | Contents |
|---|---|
| [Embedded controller](ec.md) | IT5570 host registers, PWM conversion, automatic curve and recovery |
| [EC firmware map](ec-firmware.md) | Active-image identity, code/data addresses, capture limits and unresolved interfaces |
| [BIOS settings](bios.md) | Debug Mode, menu paths, power limits, thermal policy and input settings |
| [Complete BIOS option catalogue](bios.md#complete-setup-option-catalogue) | Searchable CSV of the captured Setup definitions, including hidden and diagnostic controls |
| [Battery and charging](charging.md) | Available battery data, charger identity and charge-limit support |

These references describe the implemented interfaces and useful firmware
findings. The BIOS catalogue contains static option definitions, not saved
settings. Raw firmware images, private machine dumps and research logs are not
distributed with the project.
