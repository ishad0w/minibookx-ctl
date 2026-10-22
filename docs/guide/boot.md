# Screen and boot-menu orientation

The U300's panel is mounted sideways. Linux, the console and the boot menu
each handle orientation separately. Change only the part that needs correcting.
These instructions use your existing bootloader; they do not replace it.

## Panel orientation

For the [tested U300 mounting](../hardware/README.md), add this kernel argument:

```text
video=DSI-1:panel_orientation=right_side_up
```

It tells Linux how the panel is mounted, so GNOME can return to the correct
landscape orientation after unfolding. It works at both 50 and 60 Hz, and also
needs the [Mutter fix](tablet-mode.md#setup). It does not fix display corruption.

Use [adding kernel arguments](#adding-kernel-arguments) below, reboot, then
check orientation before and after folding.

## Linux console

If the boot messages or text console remain sideways, add this separately:

```text
fbcon=rotate:1
```

This rotates the console. It does not rotate the boot menu or supply GNOME's
panel-mount information.

## Limine menu

Find the configuration used by your installed Limine. On CachyOS, check
`ESP_PATH` in `/etc/default/limine`; the file is commonly `/boot/limine.conf`,
but the mount location can differ. Back it up before editing.

If several copies exist, `efibootmgr -v` shows `BootCurrent` and its EFI path;
use it with the mounted EFI partition to identify the installation you booted.
An old configuration on another partition may have no effect.

Place these global options before the first menu entry:

```text
interface_resolution: 1200x1920
interface_rotation: 90
```

Keep the portrait framebuffer dimensions. Setting them to 1920×1200 stretched
the menu on this panel. Check the visible menu once after the change; another
panel mounting may need the other quarter-turn.

### Optional appearance

> [!TIP]
> Quiet startup, timeout and colors are personal preferences. They are unrelated
> to tablet detection, display corruption and the orientation fixes above.

To hide the menu while leaving a one-second opportunity to reveal it with Esc:

```text
quiet: yes
timeout: 1
```

Choose a longer nonzero timeout if you want more time. Keep the menu visible
during display experiments. A zero timeout removes the key window.

The hidden waiting screen is black. For an optional gray **visible menu**,
remove or comment out `wallpaper` and set:

```text
term_background: 00222226
term_background_bright: 222226
```

These colors do not recolor the hidden waiting screen. Preserve your existing
menu entries, default selection and other styling. For all options, see
[Limine's configuration reference](https://github.com/limine-bootloader/limine/blob/v12.x/CONFIG.md).

To reduce Linux's own console output, the optional kernel arguments are
`quiet loglevel=3`. This is separate from hiding Limine; userspace messages and
splash screens depend on the distribution.

## GRUB menu

The tested stock GRUB did not support rotating its menu. `GRUB_GFXMODE` changes
resolution, not orientation. Linux's panel and console arguments still work
after GRUB hands over to the kernel.

An earlier experiment used a custom GRUB patch, but it is not maintained here.
Replacing the EFI executable is not part of this guide. If rotating the menu
matters, Limine has native support; changing bootloaders is a separate choice.

<details>
<summary>GRUB framebuffer settings</summary>

At GRUB's command prompt, `videoinfo` lists available graphics modes.
`GRUB_GFXMODE` selects one; `GRUB_GFXPAYLOAD_LINUX=keep` passes that framebuffer
mode to Linux. Neither rotates the image. See the GNU GRUB manuals for
[gfxmode](https://www.gnu.org/software/grub/manual/grub/html_node/gfxmode.html)
and [gfxpayload](https://www.gnu.org/software/grub/manual/grub/html_node/gfxpayload.html).

</details>

## Adding kernel arguments

Append arguments to the existing command line; do not replace root-device,
encryption, subvolume or resume settings. Keep a working recovery entry when
testing display timings. If the same option is already present, edit it rather
than adding a conflicting copy.

| Boot setup | Where to edit | Apply the change |
|---|---|---|
| Arch / CachyOS GRUB | `GRUB_CMDLINE_LINUX_DEFAULT` in `/etc/default/grub` | `sudo grub-mkconfig -o /boot/grub/grub.cfg` |
| Debian / Ubuntu GRUB | Existing kernel arguments in `/etc/default/grub` | `sudo update-grub` |
| Fedora GRUB with BLS entries | Use `grubby` for the chosen kernel | Inspect with `sudo grubby --info=ALL`; use `--update-kernel` and `--args` for that kernel |
| CachyOS Limine | `KERNEL_CMDLINE[...]` in `/etc/default/limine` | `sudo limine-mkinitcpio <kernel-package>` |
| Standalone Limine | The entry's `cmdline` in its active `limine.conf` | Follow the installed version's image-hash/signing procedure |

For Fedora, see the [grubby manual](https://github.com/rhboot/grubby/blob/main/grubby.8).
Do not regenerate over Fedora's EFI stub. If your system uses a different layout
or unified kernel images, use that installation's normal tools.

After reboot, `cat /proc/cmdline` shows the arguments actually passed to Linux.

In CachyOS Limine, a kernel-specific `KERNEL_CMDLINE[...]` value replaces the
default rather than extending it. Keep the complete working arguments in each
value. The native generator maintains images and hashes on updates. See the
[CachyOS boot-manager documentation](https://wiki.cachyos.org/configuration/boot_manager_configuration/).

The [60 Hz procedure](display.md) also needs early firmware inclusion and has
its own main/fallback example. Follow that procedure when changing refresh
rate, rather than applying a timing argument on its own.
