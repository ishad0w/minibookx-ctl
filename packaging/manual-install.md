# Manual installation

This guide installs the controller by copying files and creating its account
yourself. For the shorter download-and-install command, use
[normal installation](README.md). Both methods install the same binary, service
and configuration.

Install the [runtime prerequisites](../docs/distributions.md#runtime-packages)
first. The commands below also need `tar`, `sha256sum` and the usual GNU file
utilities. Run them as your ordinary user, using sudo only where shown.

## 1. Download and check a release

Create a private, empty directory for one release:

```sh
mkdir -m 0700 minibookx-release
cd minibookx-release
```

From the [release page](https://github.com/ishad0w/minibookx-ctl/releases), download
these two assets into that directory:

| Asset | What to choose |
|---|---|
| Linux binary archive | The file ending in `-x86_64-unknown-linux-musl.tar.gz` |
| Checksums | `SHA256SUMS` from that same release |

The binary archive contains everything used below, including the matching unit,
sample config, documentation and licenses. The separate `-sources.tar.gz` archive
is available for source review or rebuilding; it is unnecessary for installation.

Verify the downloaded archive before extracting it:

```sh
sha256sum --check --ignore-missing SHA256SUMS
```

**The intended Linux archive must be present and report `OK`.** This command
intentionally skips the other release assets that you have not downloaded.
Stop if the archive is missing, no file is checked or any checksum fails.
Checksums detect inconsistent downloads; they still require trust in the
[publisher](../SECURITY.md#installer-trust).

Inspect and extract the archive, then enter its directory. These wildcards assume
the download directory contains exactly one Linux binary archive:

```sh
tar --list --file minibookx-ctl-*-x86_64-unknown-linux-musl.tar.gz
tar --extract --file minibookx-ctl-*-x86_64-unknown-linux-musl.tar.gz \
    --no-same-owner --no-same-permissions
cd minibookx-ctl-*-x86_64-unknown-linux-musl
./minibookx-ctl --build-info
cat build-metadata.json
```

Check that the binary and metadata report the same full source commit and build
version. The remaining copy commands run from this extracted directory. A
compiler, Python and Git are unnecessary.

For an update, complete [stop and recovery](#stop-and-recovery) and make a
[backup](#updates-and-rollback) before replacing installed files.

## 2. Create the service account

Skip creation if `minibookx-ctl` already exists; inspect that account instead.
For a first installation, check the system's `nologin` location. The command
below uses `/usr/sbin/nologin`; replace it with `/usr/bin/nologin` if that is
where your distribution provides the executable. Do not use a custom shell or
a program in a user's directory.

```sh
sudo useradd --system --user-group --no-create-home \
    --home-dir /nonexistent --shell /usr/sbin/nologin \
    --password '!' minibookx-ctl
```

Check the new or existing account:

```sh
getent passwd minibookx-ctl
getent group minibookx-ctl
id minibookx-ctl
sudo passwd --status minibookx-ctl
```

| Property | Required value |
|---|---|
| UID | Non-root system UID below 1000 |
| Primary group | Private `minibookx-ctl` group, with no other members |
| Supplementary groups | None |
| Home and shell | `/nonexistent` and the system's `nologin` executable |
| Password | Locked (`L` or `LK`, depending on the distribution) |

Do not reuse a general-purpose account or add raw-device groups, sudo rules or
file capabilities. The root broker opens the fixed hardware interfaces; its
policy worker uses this restricted account.

## 3. Install the binary, unit and notices

Stop an existing service before copying over its executable. Inspect existing
destination paths and resolve unexpected symlinks, hardlinks, ownership or
group/other write access first.

```sh
sudo install -d -o root -g root -m0755 \
    /usr/local/bin /etc/systemd/system /usr/local/share/doc/minibookx-ctl
sudo install -o root -g root -m0755 minibookx-ctl /usr/local/bin/minibookx-ctl
sudo install -o root -g root -m0644 packaging/minibookx-ctl.service \
    /etc/systemd/system/minibookx-ctl.service
sudo install -o root -g root -m0644 LICENSE NOTICE THIRD_PARTY.md build-metadata.json \
    /usr/local/share/doc/minibookx-ctl/
sudo install -o root -g root -m0644 config/presets.conf \
    /usr/local/share/doc/minibookx-ctl/presets.conf
sudo cp -R packaging/licenses /usr/local/share/doc/minibookx-ctl/
sudo chown -R root:root /usr/local/share/doc/minibookx-ctl/licenses
sudo chmod -R u=rwX,go=rX /usr/local/share/doc/minibookx-ctl/licenses
```

Keep the notices with the executable, including the complete `licenses` directory.

## 4. Set up configuration and start

**For a new installation only**, copy the default configuration:

```sh
sudo install -d -o root -g minibookx-ctl -m0750 /etc/minibookx-ctl
sudo install -o root -g minibookx-ctl -m0640 config/presets.conf \
    /etc/minibookx-ctl/config.conf
```

**For an update, keep your existing config.** It must remain owned by
`root:minibookx-ctl`, mode 0640, inside the root-owned directory. The updated
sample is available under `/usr/local/share/doc/minibookx-ctl/presets.conf` for
comparison.

The defaults enable managed fan control and tablet detection, select Balance
and leave CPU power limits unchanged. Before starting, review
[configuration](../docs/configuration.md). To retain firmware fan control,
follow its [fan-disabled setup](../docs/configuration.md#leave-the-fan-under-firmware-control).
Use settings supported by the selected release's sample; an older binary may
not recognize keys in the current main-branch manual.

```sh
sudoedit /etc/minibookx-ctl/config.conf
sudo -u minibookx-ctl /usr/local/bin/minibookx-ctl \
  --check-config /etc/minibookx-ctl/config.conf
sudo systemd-analyze verify /etc/systemd/system/minibookx-ctl.service
sudo systemctl daemon-reload
sudo systemctl enable --now minibookx-ctl.service
minibookx-ctl --doctor
sudo journalctl -u minibookx-ctl.service -b
```

For an update, restore the previous enabled/running state rather than always
using `enable --now`. A service deliberately left stopped should stay stopped.
systemd creates the runtime directory; leave its ownership and recovery files
to the service. Continue with the [first-run checks](README.md#first-run-verification).

Display, GPU and desktop changes have [separate manual guides](../docs/README.md#choose-what-to-set-up).

## Stop and recovery

Before an update or removal, stop the service and finish recovery using the
currently installed executable. Stop competing fan or power controllers first.

```sh
sudo systemctl stop minibookx-ctl.service
sudo /usr/local/bin/minibookx-ctl --recover
sudo ls -la /run/minibookx-ctl/
```

Both stop and recovery must succeed. There must be no `module.lease`,
`power.lease`, `sensor.lease` or `manual.owner` left in the runtime directory;
status files and the private `hardware` directory may remain. An absent runtime directory is also
fine. Recovery also runs through the unit's `ExecStopPost`; the explicit command
lets you retry while stopped.

On failure, keep the installed executable and recovery records and inspect the
journal. Deleting a lease is not recovery. If another installer reports a pending
transaction, finish it with the tool that created it before replacing files.
See [troubleshooting](../docs/diagnostics.md#common-problems).

## Updates and rollback

Download and verify the new release first. Record the current service state:

```sh
systemctl show --property=LoadState,ActiveState,UnitFileState minibookx-ctl.service
```

Resolve an unknown state or a start/stop already in progress before continuing.
Complete [stop and recovery](#stop-and-recovery), then save a root-private backup.
`mktemp` chooses a new filename so a previous rollback copy is not overwritten:

```sh
sudo install -d -o root -g root -m0700 /var/backups/minibookx-ctl
manual_backup=$(sudo mktemp /var/backups/minibookx-ctl/manual-XXXXXXXX.tar.gz)
sudo tar --create --gzip --file "$manual_backup" --directory / \
    usr/local/bin/minibookx-ctl etc/systemd/system/minibookx-ctl.service \
    etc/minibookx-ctl usr/local/share/doc/minibookx-ctl
printf 'Backup: %s\n' "$manual_backup"
```

Back up any local unit drop-ins separately. Repeat the file-copy steps with the
new release only after the backup succeeds, retaining your config, then validate
it and reload systemd. Restore the service's previous boot-enablement and running
state.

For rollback, stop and recover the new version before restoring the saved
binary, unit, config and notices together. Use your trusted backup path as
`manual_backup` if returning in another shell. Inspect its contents, then restore:

```sh
sudo tar --list --gzip --file "$manual_backup"
sudo tar --extract --gzip --file "$manual_backup" --directory /
sudo systemctl daemon-reload
sudo -u minibookx-ctl /usr/local/bin/minibookx-ctl \
  --check-config /etc/minibookx-ctl/config.conf
```

Restore matching local drop-ins if used, then restore the recorded service state.
Manual file copies have no automatic transaction rollback.

## Removal

Complete [stop and recovery](#stop-and-recovery), then:

```sh
sudo systemctl disable minibookx-ctl.service
sudo rm -- /usr/local/bin/minibookx-ctl /etc/systemd/system/minibookx-ctl.service
sudo systemctl daemon-reload
```

Configuration, the locked account, notices and backups remain for later use.
Keep unresolved recovery records. Manual display and kernel changes have their
own rollback instructions.
