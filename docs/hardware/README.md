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

## Software requirements

Physical checks used CachyOS with GNOME on Wayland and the packaged i915 driver.
Other distributions and desktops have not been tested on the laptop.

| Component | Requirement |
|---|---|
| Service | x86_64 Linux, systemd and system D-Bus; [runtime packages](../guide/install.md#packages) |
| Hardware access | Supported [kernel interfaces](../reference/kernel-support.md) |
| GNOME | Wayland, `iio-sensor-proxy`, and Mutter with the unfolding-orientation fix; [desktop setup](../guide/tablet-mode.md#gnome) |
| Display and bootloader | Separate [display](../guide/display.md) and [orientation](../guide/boot.md) settings; independent of controller installation |

## Features and limits

The table summarizes physical checks on this unit, not qualification of every
release or firmware version. Repeat the
[post-update checks](../development/maintenance.md#after-kernel-bios-or-firmware-updates)
after relevant changes. Container and VM tests have
[different scopes](../development/maintenance.md#evidence-boundaries).

| Feature | Supported behavior and limits |
|---|---|
| Fan control | Idle, sustained moderate load at 35–40% PWM, warm restart, tachometer feedback and return to firmware checked; higher-temperature stages and hot resume remain unqualified |
| Tablet input | Laptop/flat positions keep input active; tent/folded positions inhibit built-in input; stopping restores it |
| GNOME rotation and on-screen keyboard | Folded landscape/portrait use and normal orientation after unfolding checked; see [tablet mode](../guide/tablet-mode.md) |
| Base-sensor initialization | Wakes the supported powered-down sensor and restores its original state on stop |
| Deep/S3 suspend | Input, sensor detection and display work after resume; firmware controls the fan during sleep |
| Display | Stock-driver 60 Hz cold start and resume checked on this panel; rated maximum unknown; 70 Hz produces corruption |
| PL1/PL2 control | Disabled by default; BIOS policy preservation checked. Live overrides, restoration and raised limits remain untested on hardware |
| `fan_control = false` | Covered by software/VM tests; not physically qualified |
| PL4, voltage and GPU-frequency control | Not implemented |
| Battery charge limit | Not implemented; see [battery and charging](charging.md) |
| Keyboard-backlight brightness/timeout | No Linux setter in this utility; see [BIOS settings](bios.md) |

Suspend can log `xHC error in resume, USBSTS 0x401, Reinit` for USB controller
`0000:00:0d.0`. The controller reinitializes; this is separate from the DSI display
issue. Package/container checks are listed separately in the
[distribution test matrix](../development/maintenance.md#distribution-matrix).

## Firmware and controllers

| Reference | Contents |
|---|---|
| [Embedded controller](ec.md) | IT5570 host registers, PWM conversion, automatic curve and recovery |
| [EC firmware map](ec-firmware.md) | Active-image identity, code/data addresses, capture limits and unresolved interfaces |
| [BIOS settings](bios.md) | Debug Mode, menu paths, power limits, thermal policy and input settings |
| [Complete BIOS option catalogue](bios.md#complete-setup-option-catalogue) | Searchable CSV of the captured Setup definitions, including hidden and diagnostic controls |
| [Battery and charging](charging.md) | Available battery data, charger identity and charge-limit support |
| [Panel and graphics](panel.md) | Timing tables, fixed modes, i915 and graphics power features |
| [Posture sensors](tablet-sensors.md) | Detection thresholds, sensor startup and desktop interface |

These references describe the implemented interfaces and useful firmware
findings. The BIOS catalogue contains static option definitions, not saved
settings. Raw firmware images, private machine dumps and research logs are not
distributed with the project.
