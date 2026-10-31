# Manual file installation

Use this when you want to create the account and copy the files yourself.
For ordinary or repeated updates, the [standard installer](../guide/install.md)
does these steps for you and preserves your configuration.

Install the [runtime prerequisites](../guide/install.md#packages) first. The
commands below also need `tar` and `sha256sum`. Run as your ordinary user,
using sudo only where shown. No compiler, Git or Python is needed.

## Download a release

Create an empty directory and download two files from the
[same release](https://github.com/ishad0w/minibookx-ctl/releases):

- The archive ending in `-x86_64-unknown-linux-musl.tar.gz`.
- `SHA256SUMS`.

From that directory, verify the archive:

```sh
sha256sum --check --ignore-missing SHA256SUMS
```

The Linux archive must report `OK`; stop on failure or if no file was checked.
Checksums detect inconsistent downloads, not a compromised publisher.

Extract it and enter the resulting directory. These patterns assume you
downloaded exactly one Linux archive:

```sh
tar --extract --file minibookx-ctl-*-x86_64-unknown-linux-musl.tar.gz \
  --no-same-owner --no-same-permissions
cd minibookx-ctl-*-x86_64-unknown-linux-musl
./minibookx-ctl --build-info
cat build-metadata.json
```

The binary and metadata should identify the same version and full source commit.
The following commands run from this directory. It includes the binary, unit,
sample config, notices and panel tools. If you only need the VBT builder, stop
here and return to the [display guide](../guide/display.md).

For an offline installation using the binary's own installer, you can instead
run `sudo ./minibookx-ctl --install --enable` and skip the file-copy steps below.
The separate flat executable asset supports the same mode: verify its checksum
and make it executable with `chmod +x` first.

## Create the service account

For a first installation:

```sh
sudo useradd --system --user-group --no-create-home \
  --home-dir /nonexistent --shell /usr/sbin/nologin \
  --password '!' minibookx-ctl
```

Use `/usr/bin/nologin` instead if that is your distribution's location. If the
account already exists, do not recreate it. Check it with `id minibookx-ctl`,
`getent passwd minibookx-ctl`, `getent group minibookx-ctl` and
`sudo passwd --status minibookx-ctl`.

It must have a non-root system UID below 1000, its own private primary group,
no supplementary groups, a locked password, `/nonexistent` as home and the
system's `nologin` shell. Do not add sudo or raw-device permissions.

## Copy the files

For an update, first follow [manual updates](#manual-updates) below. Do not copy
over a running service. Resolve unexpected symlinks or writable destination
directories before installing root-owned files.

```sh
sudo install -d -o root -g root -m0755 \
  /usr/local/bin /etc/systemd/system /usr/local/share/doc/minibookx-ctl
sudo install -o root -g root -m0755 minibookx-ctl /usr/local/bin/minibookx-ctl
sudo install -o root -g root -m0644 packaging/minibookx-ctl.service \
  /etc/systemd/system/minibookx-ctl.service
sudo install -o root -g root -m0644 LICENSE NOTICE THIRD_PARTY.md \
  /usr/local/share/doc/minibookx-ctl/
sudo install -o root -g root -m0644 config/presets.conf \
  /usr/local/share/doc/minibookx-ctl/presets.conf
sudo cp -R packaging/licenses /usr/local/share/doc/minibookx-ctl/
sudo chown -R root:root /usr/local/share/doc/minibookx-ctl/licenses
sudo chmod -R u=rwX,go=rX /usr/local/share/doc/minibookx-ctl/licenses
```

Keep the notices and complete `licenses` directory with the executable.

**For a new installation only**, create the configuration:

```sh
sudo install -d -o root -g minibookx-ctl -m0750 /etc/minibookx-ctl
sudo install -o root -g minibookx-ctl -m0640 config/presets.conf \
  /etc/minibookx-ctl/config.conf
```

For updates, keep the existing file. Its owner must remain
`root:minibookx-ctl` with mode 0640. Compare it with the new sample if needed.

## Configure and start

The default is Balance fan control, tablet detection and no CPU power overrides.
Use [configuration](../guide/configuration.md) to change this before starting.
If EC access is unavailable, set `fan_control = false`.

```sh
sudoedit /etc/minibookx-ctl/config.conf
sudo -u minibookx-ctl /usr/local/bin/minibookx-ctl \
  --check-config /etc/minibookx-ctl/config.conf
sudo systemd-analyze verify /etc/systemd/system/minibookx-ctl.service
sudo systemctl daemon-reload
sudo systemctl enable --now minibookx-ctl.service
minibookx-ctl --doctor
```

Continue with the ordinary [first checks](../guide/install.md#check-the-result).
For an update, restore the previous service state instead of enabling a service
you had deliberately left disabled.

## Manual updates

Download and verify the new release before stopping the old one. Record whether
the service is enabled and running, then recover with the installed binary:

```sh
systemctl show --property=ActiveState,UnitFileState minibookx-ctl.service
sudo systemctl stop minibookx-ctl.service
sudo /usr/local/bin/minibookx-ctl --recover
sudo ls -la /run/minibookx-ctl/
```

Both stop and recovery must succeed. No `module.lease`, `power.lease`,
`sensor.lease` or `manual.owner` should remain; status files and the private
`hardware` directory may remain. An absent runtime directory is fine.
If recovery fails, keep the files and use
[troubleshooting](../guide/troubleshooting.md#recovery-failed).

Back up the installed files, configuration and any unit drop-ins before
replacing them. For example, this creates a new root-private archive:

```sh
sudo install -d -o root -g root -m0700 /var/backups/minibookx-ctl
manual_backup=$(sudo mktemp /var/backups/minibookx-ctl/manual-XXXXXXXX.tar.gz)
sudo tar --create --gzip --file "$manual_backup" --directory / \
  usr/local/bin/minibookx-ctl etc/systemd/system/minibookx-ctl.service \
  etc/minibookx-ctl usr/local/share/doc/minibookx-ctl
printf 'Backup: %s\n' "$manual_backup"
```

Back up custom unit drop-ins separately. Repeat the copy and validation steps,
preserving your config, then restore the previous service state.

For rollback, stop and recover the new version first. With `manual_backup` set
to your trusted backup path, inspect and restore it:

```sh
sudo tar --list --gzip --file "$manual_backup"
sudo tar --extract --gzip --file "$manual_backup" --directory /
sudo systemctl daemon-reload
```

Restore any matching unit drop-ins too. [Validate the restored config](#configure-and-start)
before returning to the previous service state. Manual copies have no automatic
transaction rollback.

## Remove manually

Complete stop and recovery above, then:

```sh
sudo systemctl disable minibookx-ctl.service
sudo rm -- /usr/local/bin/minibookx-ctl /etc/systemd/system/minibookx-ctl.service
sudo systemctl daemon-reload
```

Config, notices and the locked account remain. Display and boot changes have
their own rollback instructions. Never remove unresolved recovery records.
