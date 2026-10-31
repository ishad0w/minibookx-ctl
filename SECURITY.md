# Security

## Report a vulnerability

Use **Security → Report a vulnerability** in the GitHub repository. Report
ordinary bugs through Issues. Include the build version, kernel, affected
hardware and steps to reproduce; omit passwords, raw firmware and personal logs.

## Service permissions

One executable runs under one systemd unit. It separates the root hardware
broker, unprivileged policy worker and tablet-switch guardian into processes.
Configuration is parsed after the worker drops privileges. It cannot specify
commands, register addresses, device paths or hooks. The service account has no
login, raw-device access, sudoers rule or file capabilities.

The broker remains UID 0 and narrows its capabilities to `CAP_KILL` after setup;
that does not make it unprivileged. Worker and guardian use the dedicated
service UID with empty capability sets, no supplementary groups and no ptrace
access. Only the guardian and broker retain the original uinput descriptor;
the worker receives no hardware descriptors. The broker can send OFF on that
same device after a killed guardian has been reaped.

The unit uses no ambient capabilities. Its device allow-list is intentionally
broader than the code's fixed identities: `char-input r` permits all input
devices and `char-i2c rw` all I2C adapters. Root-side identity checks narrow the
actual opens; the device filter alone does not enforce those identities.
`OOMScoreAdjust=-250` favors broker/guardian recovery; the worker resets its
score to zero before dropping UID. This is not immunity from OOM termination.
Only the broker sends systemd readiness
and watchdog messages; the worker reports readiness and liveness over bounded
IPC. These restrictions supplement the process boundary, not replace it.

Managed fan control needs writable debugfs access. The unit exposes debugfs
inside a root-only runtime directory so an EC interface loaded later remains
reachable. This mount exposes more than the EC files to the trusted broker;
the register whitelist is enforced by the program, not by the mount. The worker
has neither access to that directory nor inherited hardware descriptors.
With `fan_control = false`, normal operation does not open the EC.
Hardware operations remain limited to the supported profile and fixed protocol.
See the [service design](docs/development/design.md) for details.

## Installer trust

Run an installer you trust. The documented bootstrap URL pins reviewed source
instead of following `main`. The script downloads a published release as your
ordinary user and checks its executable against that release's SHA-256 manifest.
Only then does it request sudo. `--binary` uses the executable you supply.

The built-in installer writes fixed paths. Existing configuration is retained
and validated by the service account before stopping a running controller.
The installer uses those exact validated bytes; it does not parse config as root.
Unsafe destinations, links and permissions are refused. Staging with `--destdir`
does not change live services, accounts or hardware.

An update requires recovery before replacing the executable. If copying or
startup fails, rollback attempts to restore the old files and service state,
including when a failed start is waiting to restart. Catchable termination
signals are deferred through recovery/rollback and final reporting. Synchronous
faults, power loss, SIGKILL or SIGSTOP can still
interrupt installation. Keep an independent
[manual backup](docs/reference/manual-install.md#manual-updates) when needed.
Administrator-created systemd drop-ins remain the administrator's responsibility.

The bootstrap, installer and daemon do not install packages, download drivers,
compile modules or edit boot files. Doctor uses fixed read-only package queries;
privileged reports run those queries with privileges dropped. It never applies
the suggested changes.

Checksums detect inconsistent downloads; they are not independent signatures
and do not protect against a compromised publisher. The publishing tool refuses
to replace existing public assets and requires an immutable published release,
including on retries. Version tags cannot be moved or deleted through normal
repository access. Source and checksum
files are included with each release.

## Recovery limits

Systemd invokes recovery after a stop or failure and before a restart. Root-owned
ownership records are retained if restoration fails. They live in `/run` and
last only for the current boot. Whether EC manual mode survives a warm reboot
has not been established; the controller refuses an unattributed manual mode
rather than clearing a mode that another tool may own.

Recovery requires a working kernel and reachable hardware. The software cannot
recover an inaccessible EC or guarantee fan takeover after a kernel hang.
Keep hardware thermal protection enabled. See
[recovery behavior](docs/development/design.md#recovery-and-sleep) and
[recovery troubleshooting](docs/guide/troubleshooting.md#recovery-failed).
