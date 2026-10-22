# Security

## Report a vulnerability

Use **Security → Report a vulnerability** in the GitHub repository. Report
ordinary bugs through Issues. Include the build version, kernel, affected
hardware and steps to reproduce; omit passwords, raw firmware and personal logs.

## Service permissions

One executable runs under one systemd unit. Internally it separates the root
hardware broker, unprivileged policy worker and tablet-switch guardian into
processes. Configuration is parsed after the worker drops privileges.
It cannot specify commands, register addresses or device paths. The service
account has no login, raw-device access, sudoers rule or file capabilities.

The fixed `fan_control` startup choice can leave EC control disabled; it cannot
select another backend or bypass recovery of earlier ownership. The trusted
root broker uses a namespace-local debugfs bind under a root-owned 0700 runtime
directory for lazy EC setup; `/sys` remains protected by the unit. It uses that
alias only when it identifies the canonical debugfs source. Its fixed register
implementation remains part of the trust boundary; the worker cannot traverse
the private parent and receives neither raw descriptors nor capabilities. See
[feature setup](docs/guide/configuration.md#leave-the-fan-to-firmware)
for firmware fan mode.

Hardware operations are restricted to the supported profile and fixed protocol.
The broker and guardian remain trusted components. See the
[architecture](docs/development/design.md) for their responsibilities and recovery behavior.

## Installer trust

Run an installer you trust. The standalone shell bootstrap downloads as your
ordinary user, resolves one immutable release tag in this repository and checks
the executable against that release's SHA-256 manifest. Only then does it invoke
the executable's built-in installer through sudo. Local `--binary` installation
uses the executable you explicitly supply instead.

The Rust installer writes fixed paths using embedded configuration, service and
license files. Existing configuration is retained. Destination symlinks,
hardlinked files and unsafe permissions are refused; installation does not parse
the configuration as root or install sudoers rules, setuid files or capabilities.
Before stopping an existing controller, it validates the captured config in a
separate process under the service account, with inherited descriptors and
privileges removed. It installs that same validated snapshot. It then stops the
controller and requires recovery before replacing its
executable. Staging with `--destdir` does not change live services or accounts.

An invalid saved config blocks an update before stopping the old service.
A recovery failure keeps the old files in place; a later copy or startup failure
attempts to restore those files and the previous service state. This rollback
does not survive power loss or SIGKILL during installation. Keep an independent
[manual backup](docs/reference/manual-install.md#manual-updates) when needed.
Local systemd drop-ins remain the administrator's responsibility.

The bootstrap, installer and daemon do not install packages, download drivers,
compile modules or edit boot files. Doctor can query the installed package
database through fixed read-only commands; privileged reports run those queries
with privileges dropped. It never applies the suggested package changes.

Checksums detect inconsistent downloads; they are not independent signatures
and do not protect against a
compromised publisher. Source and checksum files remain available with every
release for independent review.

## Recovery limits

Systemd runs recovery through the same unit after a stop or failure and before
a restart. Recovery records remain root-owned and are retained when restoration
fails. The [service design](docs/development/design.md#recovery-and-sleep) defines its recovery
guarantees and hardware failure limits. Keep hardware thermal protection enabled.
Update and recovery procedures are in the
[installation guide](docs/guide/install.md).
