# Repository map

Use this map to find the code or guide relevant to a change. Working rules and
verification commands live in [AGENTS.md](AGENTS.md); user setup starts in the
[documentation index](docs/README.md).

## Entry points and runtime

| Path | Responsibility |
|---|---|
| [src/main.rs](src/main.rs) | CLI dispatch: installer, daemon, lifecycle, reports and simulations |
| [src/lib.rs](src/lib.rs) | Library module declarations shared with tests |
| [src/broker.rs](src/broker.rs) | Root broker, worker privilege drop, independent guards and tablet-guardian supervision |
| [src/hardware.rs](src/hardware.rs) | Fixed hardware session, module/ownership leases, private debugfs alias and recovery |
| [src/ec.rs](src/ec.rs) | Bounded EC register access, manual/automatic fan control and duty ramps |
| [src/runtime.rs](src/runtime.rs) | Unprivileged worker: config reload, policy sampling, sleep handling and policy status |
| [src/service.rs](src/service.rs), [src/ipc.rs](src/ipc.rs) | Worker-side session and the fixed private request/reply protocol |
| [src/policy.rs](src/policy.rs), [src/presets.rs](src/presets.rs) | Fan decisions/timers and config parsing/profile selection |
| [src/power.rs](src/power.rs) | Opt-in bounded PL1/PL2 writes, readback and journaled restoration |
| [src/rapl.rs](src/rapl.rs) | Shared read-only RAPL package/constraint recognition and observations |
| [src/tablet.rs](src/tablet.rs) | Revision-specific posture sensors, estimator and virtual tablet switch |
| [src/lid.rs](src/lid.rs) | Read-only evdev lid identity, notifications, queue recovery and current-state synchronization |
| [src/tablet/power.rs](src/tablet/power.rs) | Fixed base-sensor wake/restore transaction and bounded ownership journal |
| [src/systemd.rs](src/systemd.rs) | logind D-Bus, delay inhibitor, systemd notifications, signals and config watches |
| [src/sensor.rs](src/sensor.rs), [src/thermal.rs](src/thermal.rs) | CPU temperature readings and read-only thermal-capability inventory |
| [src/platform.rs](src/platform.rs) | Shared hardware-profile rules and fixed system-tool discovery |
| [src/paths.rs](src/paths.rs) | Shared installation, runtime and RAPL locations |
| [src/status.rs](src/status.rs) | Bounded status-file validation and root/worker snapshot aggregation |
| [src/doctor.rs](src/doctor.rs), [src/doctor_graphics.rs](src/doctor_graphics.rs) | Read-only reports and version-specific graphics-package advice |
| [src/build_info.rs](src/build_info.rs) | Expose embedded build identity |

For process ownership, startup choices and cleanup order, read
[the service design](docs/development/design.md). The root broker owns hardware access; the
worker parses configuration after dropping privileges; the guardian owns the
tablet-switch descriptor. [SECURITY.md](SECURITY.md) describes the installer and
runtime trust boundaries. Keep the required boundaries in
[AGENTS.md](AGENTS.md#required-boundaries) when changing these interfaces.

## Installation and release assets

| Path | Responsibility |
|---|---|
| [install.sh](install.sh) | Download and verify a release as the ordinary user, then invoke its embedded installer |
| [src/install.rs](src/install.rs) | Fixed-path installation/update/removal, config preflight and file/service rollback |
| [config/presets.conf](config/presets.conf) | Shipped user-config template and named profiles |
| [packaging/minibookx-ctl.service](packaging/minibookx-ctl.service) | The single production unit, sandbox and recovery lifecycle |
| [packaging/licenses/](packaging/licenses/), [THIRD_PARTY.md](THIRD_PARTY.md) | Dependency notices and source information |
| [build.rs](build.rs), [Cargo.toml](Cargo.toml), [Cargo.lock](Cargo.lock) | Embedded notices/provenance, build features and locked Rust dependencies |
| [third-party/libdbus.json](third-party/libdbus.json) | Pinned C dependency identity |

## Developer tools and tests

[Makefile](Makefile) is the normal command interface; `make help` lists its
targets. Detailed build/release instructions are in
[maintenance](docs/development/maintenance.md).

| Path | Responsibility |
|---|---|
| [scripts/build-static.sh](scripts/build-static.sh) | Rootless musl build, static verification and optional musl tests |
| [scripts/libdbus_source.py](scripts/libdbus_source.py) | Verify, prepare and build the pinned libdbus source |
| [scripts/package_release.py](scripts/package_release.py) | Package a clean commit, matching binary, notices and corresponding sources |
| [scripts/release_format.py](scripts/release_format.py) | Read-only ELF, archive, checksum and metadata validation |
| [scripts/publish_release.py](scripts/publish_release.py) | Guarded GitHub publication from the main-branch workflow |
| [.github/workflows/ci.yml](.github/workflows/ci.yml) | Checks, static artifacts, distribution matrix and publication gates |
| [src/install/tests.rs](src/install/tests.rs) | Installer failure, ownership and rollback fixtures; other Rust tests are beside their modules |
| [tests/tooling/](tests/tooling/) | Python regression tests plus offline Markdown/unit checks |
| [tests/containers/](tests/containers/) | Distribution prerequisites, package advice and staged/mocked-service installer checks |
| [tests/systemd/](tests/systemd/) | Separate feature-gated QEMU fixture: real supervision/logind with fake hardware |
| [tests/fixtures/](tests/fixtures/) | Test-only inputs, including a low-duty simulation fixture |

Container, VM and physical checks prove different things; see
[evidence boundaries](docs/development/maintenance.md#evidence-boundaries). Generated
`target/` and `dist/` output is ignored and is not maintained source.

## Documentation ownership

User instructions live together in `docs/guide/`. Technical field descriptions
and manual file installation belong in `docs/reference/`; hardware observations
belong in `docs/hardware/`; source and release instructions belong in
`docs/development/`. `docs/README.md` is the reader-facing index.

Keep mutable facts in one place: shipped defaults in `config/presets.conf`,
accepted ranges in the configuration reference, sensor timings in the sensor
reference, and physical results in the hardware qualification record. Other
pages should summarize and link. An explicitly illustrative config example or
a dated test observation is not a second specification of current defaults.

| Path | Contents |
|---|---|
| [docs/guide/install.md](docs/guide/install.md) | Install, update, remove; runtime package commands by distribution |
| [docs/guide/configuration.md](docs/guide/configuration.md) | Everyday fan/profile settings and optional power limits |
| [docs/guide/tablet-mode.md](docs/guide/tablet-mode.md) | Folding, GNOME setup and Mutter requirements |
| [docs/guide/display.md](docs/guide/display.md) | 60 Hz setup, native initramfs inclusion, validation and rollback |
| [docs/guide/boot.md](docs/guide/boot.md) | Orientation and persistent arguments; optional appearance tips |
| [docs/guide/graphics.md](docs/guide/graphics.md) | Intel acceleration packages by distribution |
| [docs/guide/troubleshooting.md](docs/guide/troubleshooting.md) | First checks, recovery errors and issue reports |
| [docs/reference/](docs/reference/) | Manual file installation, config/status semantics and kernel support |
| [docs/hardware/](docs/hardware/) | Tested revision, EC, BIOS catalogue, charging and hardware evidence |
| [docs/development/](docs/development/) | Architecture, builds, tests and releases |
| [packaging/README.md](packaging/README.md), [docs/distributions.md](docs/distributions.md) | Short navigation pages retained for paths printed by existing binaries; no duplicate instructions |
| [contrib/vbt/build_vbt.py](contrib/vbt/build_vbt.py) | Offline, identity-checked timing builder; does not install firmware |
| [contrib/i915/](contrib/i915/) | Unbuilt developer RFC and its validation requirements; separate from the controller |
