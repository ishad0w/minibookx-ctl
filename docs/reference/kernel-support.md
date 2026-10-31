# Kernel requirements

Use this reference when a feature is unavailable despite installing the
[runtime packages](../guide/install.md#packages). Installing `kmod`, kernel
headers or GPU packages does not add a missing EC driver.

## Required interfaces

| Feature | Requirement |
|---|---|
| Standard systemd unit | `i2c-dev` and `uinput`, even with `--no-tablet` |
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

## Distribution kernels

Inspect the **running kernel**, including its flavor. Package names alone do
not prove that `CONFIG_ACPI_EC_DEBUGFS` is enabled or that writes are allowed.
Debian can build the interface into the kernel; Ubuntu and Arch-family kernels
can provide it as a module. In both cases use the checks above and Doctor,
rather than installing headers or DKMS to try to create the interface.

On Fedora, inspect the exact kernel and available packages:

```sh
modinfo -k "$(uname -r)" ec_sys
dnf repoquery --whatprovides '*/ec_sys.ko*'
```

If a provider is `kernel-debug-modules-core`, it belongs to a matching debug
kernel, not the regular running kernel. Do not mix its modules into another
kernel. Selecting a debug kernel is an administrator's choice and is not
physically qualified by this project. Alternatively,
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
