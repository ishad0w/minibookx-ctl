# 60 Hz and startup display corruption

The original panel mode is 50 Hz, so it cannot show every frame of 60 fps video.
On the tested U300, a 60 Hz timing also removed the corruption seen after startup.
This setup uses the distribution's existing i915 driver; no module build is needed.

> [!WARNING]
> The panel's rated maximum refresh is unknown. This is an experiment tested on
> one panel. Pixel clock increases by about 20% over the firmware's 50 Hz mode.
> The builder checks your local panel table and refuses unknown ones.
> Keep a working recovery boot; do not bypass that check or change the BIOS panel type.

This is independent of the controller service. You will prepare a panel-timing
file, include it in Linux's early boot image, and select it with a boot argument.

```mermaid
flowchart LR
    A[Read your panel table] --> B[Build the 60 Hz file]
    B --> C[Include it in the boot image]
    C --> D[Select it at boot]
    D --> E[Check cold start and sleep]
```

## Before starting

You need Python 3 and `contrib/vbt/build_vbt.py` from the
[verified Linux release archive](../reference/manual-install.md#download-a-release)
or a source checkout. The one-line installer downloads only the executable,
so it does not put this tool on your system. No compiler or kernel headers
are needed.

Keep a working kernel and its boot image separate from the files you will
regenerate. An already working LTS entry can serve this purpose; a second entry
pointing to the same images cannot. Keep the menu accessible during testing.

If startup corruption makes the current display unusable, sleep and wake
temporarily restored it on the tested unit. Use recovery media if that does
not help.

## 1. Prepare the timing file

Create a directory for your panel files. In the Python command, replace
`/path/to/minibookx-ctl` with the extracted archive or checkout directory.
Use a fresh directory name if `~/minibookx-panel` already exists.

```sh
mkdir -m 0700 ~/minibookx-panel
cd ~/minibookx-panel
umask 077
sudo cat /sys/kernel/debug/dri/0000:00:02.0/i915_vbt > local-u300.vbt
python3 /path/to/minibookx-ctl/contrib/vbt/build_vbt.py \
  --input local-u300.vbt --output generated-vbt
```

If the debugfs directory is missing, check [the debugfs mount](../reference/kernel-support.md#debugfs).
Some kernels use a different directory name under `/sys/kernel/debug/dri/`.
If the builder rejects the table, stop: the same laptop name does not guarantee
the same panel.

The file used below is `generated-vbt/chuwi-u300-60.bin`. The accompanying
`manifest.json` records what changed. Keep both and your original table.
Other generated modes and the alternative `.cpio` archive are not used here.

Install the timing file:

```sh
sudo install -Dm644 generated-vbt/chuwi-u300-60.bin \
  /usr/lib/firmware/i915/chuwi-u300-60.bin
```

If that destination already exists from an earlier attempt, back it up before
replacing it.

## 2. Include the file in early boot

The display driver can load before the normal filesystem, so the file must also
be in the **initramfs**, Linux's early boot image. Use the tool your installation
already uses. Configure inclusion below, but wait until step 3 to regenerate it.

### mkinitcpio — Arch and CachyOS

Create `/etc/mkinitcpio.conf.d/60-minibookx-panel.conf` containing:

```sh
FILES+=(/usr/lib/firmware/i915/chuwi-u300-60.bin)
```

On an older mkinitcpio without drop-in support, add the path to the existing
`FILES` array in `/etc/mkinitcpio.conf` instead.

### dracut — Fedora and other dracut systems

Create `/etc/dracut.conf.d/60-minibookx-vbt.conf` containing:

```sh
install_items+=" /usr/lib/firmware/i915/chuwi-u300-60.bin "
```

### initramfs-tools — Debian and Ubuntu

Create `/etc/initramfs-tools/hooks/minibookx-vbt` with this content:

```sh
#!/bin/sh
set -eu
case "${1:-}" in prereqs) exit 0 ;; esac
. /usr/share/initramfs-tools/hook-functions
copy_file firmware /usr/lib/firmware/i915/chuwi-u300-60.bin \
  /lib/firmware/i915/chuwi-u300-60.bin || [ "$?" -eq 1 ]
```

Then make it executable:

```sh
sudo chmod 0755 /etc/initramfs-tools/hooks/minibookx-vbt
```

This small hook copies the file whenever the boot image is rebuilt. It does
not build or patch a driver.

## 3. Select 60 Hz and regenerate

Add this argument to the kernel you want to test:

```text
i915.vbt_firmware=i915/chuwi-u300-60.bin
```

Keep the existing root, encryption and other arguments. Leave the recovery
entry without this new argument. For CachyOS with Limine, use the example below;
for another setup, use [its argument location](boot.md#adding-kernel-arguments).

<details>
<summary>CachyOS Limine: main kernel at 60 Hz, existing LTS fallback at 50 Hz</summary>

This assumes `linux-cachyos` and an independently bootable `linux-cachyos-lts`
are already installed. In `/etc/default/limine`, give the main kernel the new
argument and give LTS its own full command line without it:

```text
KERNEL_CMDLINE[default]="<your existing arguments> i915.vbt_firmware=i915/chuwi-u300-60.bin"
KERNEL_CMDLINE[linux-cachyos-lts]="<your existing arguments>"
```

Replace both placeholders before saving. A kernel-specific value **replaces**
the default, so retain root, encryption, subvolume and any orientation settings
in both values. Quiet-boot options are optional.

Check the resolved arguments:

```sh
sudo limine-entry-tool --get-cmdline linux-cachyos
sudo limine-entry-tool --get-cmdline linux-cachyos-lts
```

Only the main kernel should select the VBT file. The next step regenerates
that kernel alone, leaving the recovery images unchanged for the first trial.

</details>

Other configurations that apply arguments to every kernel also need a separate
recovery command line without the override.

Then regenerate the **test kernel's** boot image using your normal tools:

| Installation | Example command | Inspect the resulting image |
|---|---|---|
| CachyOS with its Limine integration | `sudo limine-mkinitcpio linux-cachyos` | `lsinitcpio /path/to/referenced/initramfs` |
| Arch with mkinitcpio | `sudo mkinitcpio -p linux` | `lsinitcpio /boot/initramfs-linux.img` |
| Conventional Fedora with dracut | `sudo dracut --force --kver "$(uname -r)"` | `lsinitrd "/boot/initramfs-$(uname -r).img"` |
| Debian / Ubuntu with initramfs-tools | `sudo update-initramfs -u -k "$(uname -r)"` | `lsinitramfs "/boot/initrd.img-$(uname -r)"` |

Use your installed kernel's name and actual image path. If adding the arguments
already regenerated the test image through the same tool, inspect that image
rather than rebuilding it twice. The inspection must
list `i915/chuwi-u300-60.bin` in the image your boot entry loads. If your setup
uses a unified kernel image or signing, retain its usual generation procedure.

CachyOS's native Limine tool updates its copied images and entry hashes. Keep
using that integration for future kernel updates. Do not regenerate the recovery
kernel's images during the first trial.

## 4. Test and keep a way back

Reboot into the test entry. Check the physical screen and use
`sudo minibookx-ctl --doctor` to inspect the active refresh rate through GPU
debugfs. Without that permission, check the mode in desktop display settings.
Play a 60 fps video,
then try sleep and wake. Finish with a full shutdown, a 10–15 second pause and
a button start; inspect the login screen before putting it to sleep again.

If the picture is corrupted, boot the recovery entry. To return to the original
50 Hz timing, remove only the `i915.vbt_firmware` argument from the selected
entry. You can do this temporarily in the boot menu's entry editor, then remove
it from the persistent configuration once booted. The unused timing file may
stay on disk. To remove it entirely, also remove its inclusion configuration
and regenerate the affected images.

50 Hz can bring back the original startup problem. If 60 Hz is unusable or
does not fix it, collect the kernel log; the [i915 reference](../../contrib/i915/README.md)
describes the separate driver investigation. Its supplied RFC is unbuilt, so it
is not a ready-to-install fallback. See [collecting display logs](troubleshooting.md#report-an-issue).

After the main boot passes, apply any pending changes to the fallback through
your normal generator as well. With CachyOS Limine, use the same regeneration
command with the fallback's package name. Keep its command line free of the
VBT override if you want it to retain 50 Hz.

## What to expect afterwards

- The mode is selected at boot. **GNOME does not gain a live 50/60 Hz switch.**
  A 50 Hz fallback needs a reboot without the override argument.
- Resolution and panel color depth remain unchanged. The setup uses RGB888.
- Kernel updates should carry the inclusion setting forward through your normal
  image generator. Check the selected argument and image if the rate reverts.
- Rotation is separate; use [panel orientation](boot.md#panel-orientation) if
  the image returns sideways after unfolding.

The [panel reference](../hardware/panel.md) has timing calculations, source
references and the limits of the tests. Physical qualification remains limited
to [one U300](../hardware/README.md#features-and-limits).
