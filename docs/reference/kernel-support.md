# Kernel requirements

Use this reference when a feature is unavailable despite installing the
[runtime packages](../guide/install.md#packages). Installing `kmod`, kernel
headers or GPU packages does not add a missing EC driver.

## Required interfaces

| Feature | Requirement |
|---|---|
| Managed fan | Writable `ec_sys`, mounted debugfs and CPU temperature readings (`coretemp` on U300) |
| Firmware-managed fan | No EC, debugfs, temperature or tachometer access by the service |
| Tablet detection | `i2c-dev`, `uinput` and the recognized lid IIO accelerometer (`mxc4005` on the tested unit) |
| Optional PL1/PL2 | Recognized Intel package RAPL interface |

The service prepares its required modules. The service account needs no
raw-device group membership. To inspect the running kernel without loading
drivers or writing hardware:

```sh
uname -r
for module in ec_sys i2c_dev uinput coretemp mxc4005; do
    modinfo -k "$(uname -r)" -F filename "$module"
done
```

A driver can be a loadable module or built into the kernel. If `modinfo` fails,
check the kernel configuration and exposed device interfaces before concluding
that it is absent. EC access requires `CONFIG_ACPI_EC_DEBUGFS`.

The standard unit prepares `uinput` and `i2c-dev` at activation, including when
`--no-tablet` disables detection. Stock systemd module-load units run before its
device filter is created so the allow-listed devices can be recognized.

## Distribution observations

These are checks of specific kernel builds, not guarantees about later updates
or every flavor of a distribution's kernel.

| Kernel checked | EC interface | Source / limit |
|---|---|---|
| Debian 13, 6.12.113-1 x86 | Built in (`=y`) | [Kernel config](https://salsa.debian.org/kernel-team/linux/-/blob/debian/6.12.113-1/debian/config/kernelarch-x86/config); no separate module file expected |
| Ubuntu 24.04, 6.8.0-31-generic | Module (`=m`) | [Official headers/config package](https://archive.ubuntu.com/ubuntu/pool/main/l/linux/linux-headers-6.8.0-31-generic_6.8.0-31.31_amd64.deb) |
| Ubuntu 26.04, 7.0.0-14-generic | Module (`=m`) | [Official headers/config package](https://archive.ubuntu.com/ubuntu/pool/main/l/linux/linux-headers-7.0.0-14-generic_7.0.0-14.14_amd64.deb) |
| Arch linux 7.2.11 | Packaged `ec_sys.ko.zst` | [Package files](https://archlinux.org/packages/core/x86_64/linux/files/); rolling contents change |
| CachyOS tested U300 kernel | Writable access worked | [Features and limits](../hardware/README.md#features-and-limits) |
| Fedora 44 regular package query | No loadable module found; debug variant provided it | Check the running configuration too; a file search cannot detect a built-in driver |

On Fedora, inspect the exact kernel and available packages:

```sh
modinfo -k "$(uname -r)" ec_sys
dnf repoquery --whatprovides '*/ec_sys.ko*'
```

The recorded query found it in `kernel-debug-modules-core`. A debug kernel has
not been physically tested here; selecting another kernel is an administrator's
choice, not an installer step. Alternatively,
[disable managed fan control](../guide/install.md#without-fan-control).
Tablet detection still needs its own drivers.

Kernel lockdown or other host policies can restrict EC writes on any
distribution. A compiled-in driver does not establish writable access. Do not
disable security policy just to satisfy a package check.
References: [ACPI Kconfig](https://github.com/torvalds/linux/blob/master/drivers/acpi/Kconfig),
[ec_sys implementation](https://github.com/torvalds/linux/blob/master/drivers/acpi/ec_sys.c).

## debugfs

Managed fan control and manual VBT capture use debugfs. Check its mount:

```sh
findmnt --mountpoint /sys/kernel/debug
```

If absent and permitted by your system policy:

```sh
sudo systemctl start sys-kernel-debug.mount
```

Do not unmask a deliberately disabled mount. Firmware fan mode does not require
it. The service's private alias and access restrictions are described in
[architecture](../development/design.md).
