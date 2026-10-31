# Tablet mode

Tablet detection is enabled when the service starts. Folding the screen behind
the keyboard tells Linux it is in tablet mode: the built-in keyboard and touchpad
stop responding, while the touchscreen stays usable.

## Linux integration

The service publishes the standard `SW_TABLET_MODE` switch. libinput uses it to
inhibit built-in input; rotation and the on-screen keyboard belong to the desktop.
The daemon does not require GNOME or Mutter. Install and start the
[service](install.md), then configure the desktop you use below.

## GNOME

Use GNOME on Wayland with `iio-sensor-proxy` installed; it is included in the
[runtime package instructions](install.md#packages). No GNOME extension or
manual sensor-register setup is needed.

For correct GNOME orientation after unfolding, two things matter:

1. GNOME's **Mutter compositor must contain fix !4898**, included in Mutter 51.1.
   An older distribution package may have a backport. The GNOME Shell version
   alone does not tell you which Mutter version is installed.
2. Linux must know how this portrait-native panel is mounted. Follow
   [panel orientation](boot.md#panel-orientation) to set the tested U300's hint.
   It works at either 50 or 60 Hz.

### Check Mutter

| Distribution | Show the installed version |
|---|---|
| Arch / CachyOS | `pacman -Q mutter` |
| Debian / Ubuntu | `apt policy mutter` |
| Fedora | `rpm -q mutter` |

Update through your distribution, then log out and back in or reboot to load
the new compositor. Keep GNOME Shell and Mutter from a compatible desktop stack.

<details>
<summary>CachyOS offers a Mutter package without the fix</summary>

Compare the candidates from the repositories already enabled on your system:

```sh
pacman -Si mutter extra/mutter
```

If the default candidate lacks the fix and `extra/mutter` supplies a compatible
fixed version, review a full update selecting that package:

```sh
sudo pacman -Syu extra/mutter
```

This needs no AUR build or repository-priority change. Skip it if your installed
package already contains the fix.

</details>

## Try it

| Position | Expected behavior |
|---|---|
| Ordinary laptop | Keyboard and touchpad work |
| Nearly flat, around 180° | No repeated switching; unfolding a little further should restore input if needed |
| Tent or fully folded tablet | Built-in input turns off; touch works |
| Folded and turned sideways | Screen rotates if rotation is unlocked |
| Unfolded into a laptop | Physical input and normal landscape orientation return |

Hold each position still briefly. For the on-screen keyboard, **touch a text
field** while folded. Folding alone need not open it.

GNOME normally shows **Auto Rotate** in quick settings only while in tablet
mode. Unlock it there. For a fixed orientation in laptop mode, use
**Settings → Displays → Orientation**. The service does not change GNOME's
policy about rotation outside tablet mode.

After sleep, allow a few seconds for detection to settle. If a vertical hinge
prevents a new posture from being recognized, tilt the laptop slightly first.
The [sensor reference](../hardware/tablet-sensors.md) explains these limits.

## If something does not work

- **No tablet mode:** check `minibookx-ctl --doctor` and the service journal.
  A current release wakes the supported base sensor automatically.
- **Input returns but the image stays sideways:** check both Mutter and the
  panel-orientation hint above.
- **Input stays disabled:** return to an ordinary laptop angle, then hold still.
  Stopping the service requests normal input and firmware fan control:

```sh
sudo systemctl stop minibookx-ctl.service
```

## Plasma and other desktops

The same Linux tablet switch is used; the GNOME/Mutter requirements above do not
apply. Configure sensor integration, automatic rotation and the virtual keyboard
for your desktop. Plasma provides rotation controls, including an optional
tablet-only restriction. These desktops have not been physically tested by
this project.

Sources: [Mutter 51.1](https://github.com/GNOME/mutter/blob/51.1/NEWS),
[fix !4898](https://gitlab.gnome.org/GNOME/mutter/-/merge_requests/4898),
[GNOME rotation controls](https://github.com/GNOME/gnome-shell/blob/51.1/js/misc/systemActions.js),
[Plasma controls](https://github.com/KDE/kscreen/blob/Plasma/6.6/kcm/ui/Orientation.qml).
