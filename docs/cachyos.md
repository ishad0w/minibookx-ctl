# Clean CachyOS setup

This is the setup checked after a complete OS reinstall on the
[supported MiniBook X U300](hardware/README.md): CachyOS, GNOME/Wayland and the
distribution's existing Limine integration. It keeps the packaged kernel and
i915 driver. Other distributions and bootloaders have their own
[package](distributions.md) and [boot](../contrib/bootloaders/README.md) instructions.

The controller handles the fan and tablet switch. You configure the desktop,
panel timing and bootloader separately. No Rust/C toolchain, kernel headers or
DKMS package is needed for this setup. Python 3 is needed only to prepare the
optional panel-timing file.

```mermaid
flowchart LR
    A[Prerequisites and Mutter] --> B[Install and Doctor]
    B --> C[Main 60 Hz and panel mounting]
    C --> D[Visible boot trial]
    D --> E[Fallback and quiet menu]
    E --> F[Folding, sleep and cold start]
```

If the original 50 Hz output is unreadable, one suspend/resume cycle restored
usable output on the tested unit. This is a temporary workaround, not a
guarantee; use a working recovery boot if necessary. Do not change the BIOS
panel type to work around corruption.

## 1. Check the installed system

Start in an ordinary laptop position, around 90–120°. Check the
[runtime packages and running kernel](distributions.md#runtime-packages), then
confirm that [debugfs is mounted](distributions.md#mount-debugfs-when-needed)
for managed fan control and VBT capture.

The checked clean installation already supplied the runtime packages and Intel
graphics stack. Install missing packages rather than replacing a working stack
with an unrelated driver. In particular, `xf86-video-intel` is unnecessary for
this GNOME/Wayland setup.

## 2. Check Mutter before testing rotation

GNOME Shell and Mutter have separate package versions. A GNOME 51.1 desktop can
still have the older Mutter 51.0 package. Follow
[the Mutter check](tablet-mode.md#sideways-display-after-unfolding), including
the native `extra/mutter` option if the already enabled CachyOS repositories
still offer the mismatch found in the clean-install test.

The required behavior comes from Mutter fix !4898, present in 51.1, together
with correct DRM panel mounting. The mounting argument is configured with the
display below. Rebooting at that stage also loads the updated compositor.

## 3. Install the controller and read its reports

Follow [Install and start](../packaging/README.md#install-and-start) as your
ordinary user. The public `install.sh` obtains the current release, requests
sudo for the fixed-path installer and starts the one service.

```sh
minibookx-ctl --version
minibookx-ctl --doctor
minibookx-ctl --debug
systemctl status minibookx-ctl.service --no-pager
```

Keep the default Balance profile and `power_control = false` for the first
checks. The daemon handles a powered-down base accelerometer automatically;
there is no manual I2C-register initialization step. If startup fails, inspect
the service journal instead of deleting recovery records or forcing a register.

Use Doctor's [graphics recommendations](diagnostics.md#graphics-recommendations)
to identify missing optional packages. In the checked installation, only
`libva-utils` was missing from that list. Its `vainfo` tool verifies advertised
VA-API capabilities; it does not prove that every browser video uses decoding
on the GPU. Follow [the native graphics recipe](distributions.md#intel-graphics-and-hardware-video-decoding)
only for packages you actually need.

## 4. Prepare 60 Hz and keep a recovery boot

The [manual VBT guide](display-driver-fix.md#manual-vbt-setup) provides the
complete capture, verification and installation steps. Get its builder from
the verified Linux release archive; the one-line installer installs the
executable, not the panel tools. Capture a fresh table from your own machine.
Stop if the builder does not recognize its full identity.

Check `python3 --version` before starting. If it is missing, install CachyOS's
`python` package through a normal full system update; no development toolchain
is needed. Python was already present on the checked installation.

Before regenerating images, inspect `efibootmgr -v`, `/etc/default/limine` and
your mounted ESP. The checked installation used `/boot`; an older installation
used `/boot/efi`. Edit the configuration actually loaded by your EFI executable.
Keep a working kernel and initramfs on independent files. The checked setup had
`linux-cachyos-lts` available while testing `linux-cachyos`; installing an LTS
kernel is a separate recovery choice, not a controller dependency.

Install the generated 60 Hz `.bin` and add the ordinary mkinitcpio `FILES`
drop-in from the VBT guide. Leave the alternative `vbt-modes.cpio` unused.
Then follow [main 60 Hz / LTS 50 Hz](../contrib/bootloaders/README.md#cachyos-limine-main-60-hz-and-lts-50-hz):
preserve your complete root/encryption/subvolume arguments, add the panel-mount
and console hints, and select the VBT only for the main kernel.

Use the native generator for the main kernel first. It updates image copies and
entry hashes; no custom copy hook is needed. Check the referenced initramfs
with `lsinitcpio` and confirm that it contains the named VBT before rebooting.
The 50 Hz fallback is a boot profile, not a second refresh choice in GNOME.

## 5. Check the menu and first 60 Hz boot

Configure [Limine's visible trial](../contrib/bootloaders/README.md#limine):
native 1200×1920 framebuffer, rotation 90°, five-second timeout and quiet mode
off. Keep the existing default selection and generated entries.

Reboot, inspect the physical menu and login screen, then check:

```sh
minibookx-ctl --doctor
sudo journalctl -k -b --no-pager | grep -Ei 'i915|dsi|vbt'
```

Doctor should report the active mode at approximately 60 Hz. Confirm clean
output on the panel itself; a screenshot or lack of log errors is insufficient.
If it is corrupted, use the independent recovery boot and follow
[rollback and diagnosis](display-driver-fix.md#validation-and-rollback).

Once the main boot passes, finish the native LTS regeneration if using that
fallback, then set Limine to `quiet: yes` and `timeout: 1` as documented in the
boot guide. The final checked setup uses a black background. Quiet waiting
remains black even if a different visible-menu color is configured. Keep the
one-second key window so the menu can still be revealed.

## 6. Check folding, sleep and a cold start

In GNOME's quick settings, unlock rotation while folded. Auto Rotate normally
appears in tablet mode; its absence in ordinary laptop mode is expected. No
GNOME extension, custom polkit rule or rotation script is required.

Work through the [first-run checks](../packaging/README.md#first-run-verification):
ordinary laptop, near-flat, tent, folded landscape/portrait, then unfold. Touch
a text field to check the on-screen keyboard. After unfolding, both normal
orientation and physical input should return.

Test suspend/resume while folded, wait a few seconds for the sensors and posture
confirmation, then check input and orientation again. Inspect the service
journal for errors; do not infer a failed resume from one transitional status
sample. Finish with a complete power-off, wait 10–15 seconds, and start with the
button. Check the login screen before any further sleep.

The [qualification table](hardware/README.md#hardware-qualification) records
what passed, the tested software and the remaining limits. Keep CPU power
control disabled unless you separately choose to test
[PL1/PL2 overrides](configuration.md#optional-package-power-limits).
