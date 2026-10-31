# i915 DSI readiness: developer RFC

**This exact patch has not been compiled or boot-tested. It is a developer
candidate, not a verified installation fix.** It has not been submitted upstream.
The controller installer never downloads, builds or installs kernel code.

For the [tested panel](../../docs/hardware/README.md), start with the experimental
[60 Hz setup using stock i915](../../docs/guide/display.md). A different
panel may not support these timings. If startup corruption persists, collect
logs and investigate the readiness failure before choosing a driver change.
Changing a timeout does not make an unsupported refresh rate usable.

## What is known

| Artifact or test | Status |
|---|---|
| [RFC artifact](ready-timeout-rfc.patch) | Based on Linux `32c52ec87d6245ee2d12ed5dd509e40062939b50`; apply/whitespace/checkpatch checks only |
| Exact RFC build and boot | Not performed; no distribution/kernel combination is qualified for installation of this artifact |
| Upstream failing baseline and full debug logs | Still needed |

No validated replacement i915 binary is shipped. Success with the documented
60 Hz setup does not qualify this separate driver proposal.

## Proposed override

| Property | Proposal |
|---|---|
| Load-time parameter | `dsi_link_ready_timeout_us` |
| Default | 2500 µs, preserving the original wait |
| Effective range | 2500–100000 µs |
| Runtime modification | Read-only parameter; no live reconfiguration |
| Files changed | `icl_dsi.c`, `intel_display_params.c`, `intel_display_params.h` |

The gen11+ DSI enable sequence waits for `LINK_READY`. Downstream software
instrumentation observed readiness after the existing deadline. The wait exits
when readiness is observed; a 10000 µs ceiling is not an unconditional 10 ms
delay. The bounds are proposed software policy, not a panel specification.

The RFC keeps the default unchanged and adds no DMI/model match. Review is still
needed to decide whether an explicit parameter is appropriate or whether the
enable sequence should be fixed automatically. See the patch message for the
timing method and unresolved design questions.

## Manual developer validation

The sequence below is a development checklist. It becomes a usable installation
recipe only after someone builds and tests the exact candidate against a named
target. Keep a working kernel, initramfs and boot entry on independent files.
Use a different kernel version for recovery, with its own module directory, or
have bootable rescue media and a recovery procedure. A second entry for the same
kernel can select the same replacement module and is not an independent fallback.

### 1. Match the distribution kernel

Record the actual target and selected module:

```sh
kver=$(uname -r)
modinfo -k "$kver" -F filename i915
modinfo -k "$kver" -F vermagic i915
```

Obtain that distribution build's exact source, patches, configuration, generated
headers, `Module.symvers` and required compiler. A nearby kernel.org version is
not equivalent. CachyOS kernel builds may require LLVM settings. Built-in i915
cannot be replaced through the module method below. Signed-module policies also
apply to a replacement.

`modules_prepare` does not generate `Module.symvers` for
`CONFIG_MODVERSIONS`. Follow the distribution's preparation procedure and
[Linux kbuild documentation](https://docs.kernel.org/kbuild/modules.html).

### 2. Apply and build as an ordinary user

Replace both absolute paths. The example assumes an already prepared in-tree
LLVM build; retain your distribution's separate output-directory/compiler flags
when different.

```sh
build_tree=/absolute/path/to/prepared-kernel-tree
patch_file=/absolute/path/to/minibookx-ctl/contrib/i915/ready-timeout-rfc.patch
test "$(make -s -C "$build_tree" kernelrelease)" = "$kver"
git -C "$build_tree" apply --check "$patch_file"
git -C "$build_tree" apply "$patch_file"
make -C "$build_tree" LLVM=1 LLVM_IAS=1 M=drivers/gpu/drm/i915 -j4 modules
module="$build_tree/drivers/gpu/drm/i915/i915.ko"
modinfo -F vermagic "$module"
modinfo -p "$module"
```

A failed patch application needs source review. Confirm target identity, symbol
versions, the new parameter and required module signature. Vermagic alone is
insufficient. Use the trusted signing process; disabling Secure Boot is not an
installation step. Build time depends on the kernel source, configuration and
available resources.

### 3. Install a separate override and regenerate the actual boot image

Only after reviewing the built candidate, retain the packaged module and install
the replacement separately:

```sh
sudo install -Dm644 "$module" "/usr/lib/modules/$kver/updates/local-i915/i915.ko"
sudo depmod -a "$kver"
modinfo -k "$kver" -F filename i915
```

The selected filename must identify your replacement. If it does not, inspect
module search precedence before booting. A parameter configuration applies only
to a module that implements it:

```conf
# /etc/modprobe.d/i915-dsi.conf
options i915 dsi_link_ready_timeout_us=10000
```

Regenerate only the test kernel's initramfs through its existing integration.
Use the [distribution-specific generator](../../docs/guide/display.md#3-select-60-hz-and-regenerate)
with that kernel's package name/version; do not regenerate every installed
kernel or overwrite the recovery image. Ensure the selected boot image contains the new
module and modprobe configuration, and that ESP copies, hashes and signatures
are updated by the native integration.

### 4. Verify physically and keep rollback

```sh
cat /sys/module/i915/parameters/dsi_link_ready_timeout_us
sudo journalctl -k -b | grep -Ei 'i915|dsi|link.ready'
minibookx-ctl --doctor
```

Check the physical image before any sleep cycle, then check suspend/resume and
power-off/button starts. Parameter presence confirms the option was loaded,
not that the patch is correct. Record the exact source, module and kernel for
each result. Upstream build coverage must include affected i915/xe display use.

Every kernel update needs a new manual review and matching build; never copy an
old module into a new kernel directory. This project provides no DKMS package,
rebuild hook, interceptor or automatic installer.

## Rollback

Boot the independent working kernel or rescue media. Remove only your separate override and its
parameter configuration, run `depmod` for the target, then regenerate that
target's boot image using the same integration. Verify selection before reboot.
Stock i915 restores the stock timeout; the original startup corruption may
return. A 50 Hz entry restores timings, not necessarily clean output.

Run only one maintainer of this module override; competing rebuild hooks can
replace the selected file unexpectedly.

## Before upstream submission

Build and boot this exact candidate on its recorded upstream base. Collect a
failing upstream baseline, working comparison, full DRM logs, hardware identity
and repeatable timing method. Separate downstream observations from upstream
tests. Explain the existing deadline, proposed bounds and why an option is the
right interface. Use the target's `MAINTAINERS` and `scripts/get_maintainer.pl`.
A human author must approve attribution and DCO sign-off; none is invented here.

References: [Intel bug reporting](https://drm.pages.freedesktop.org/intel-docs/how-to-file-i915-bugs.html),
[Linux patch submission](https://docs.kernel.org/process/submitting-patches.html).

## License

All three affected files at the recorded base use MIT terms:

| Upstream file | Original notice |
|---|---|
| [icl_dsi.c](https://github.com/torvalds/linux/blob/32c52ec87d6245ee2d12ed5dd509e40062939b50/drivers/gpu/drm/i915/display/icl_dsi.c) | Intel 2018 copyright and full MIT permission notice |
| [intel_display_params.c](https://github.com/torvalds/linux/blob/32c52ec87d6245ee2d12ed5dd509e40062939b50/drivers/gpu/drm/i915/display/intel_display_params.c) | Intel 2023 copyright, SPDX MIT |
| [intel_display_params.h](https://github.com/torvalds/linux/blob/32c52ec87d6245ee2d12ed5dd509e40062939b50/drivers/gpu/drm/i915/display/intel_display_params.h) | Intel 2023 copyright, SPDX MIT |

The RFC's own additions are distributed under the same MIT terms in
[COPYING](COPYING). Preserve each upstream file's original notices. The kernel's
overall GPL licensing does not replace those file-specific notices, and the
controller's Apache-2.0 license does not relicense the target kernel.
