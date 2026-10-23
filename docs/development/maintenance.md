# Building and maintenance

For installation and updates, use the [prebuilt release](../guide/install.md).
This guide covers source builds, release artifacts and checks after system
updates.

## Build from source

Build as an ordinary user with the current stable Rust toolchain. Native checks
need GNU Make, a C compiler, pkg-config, libdbus development headers, Python 3.11+,
ShellCheck and systemd's unit-verification tools. Static builds also need a musl
C compiler, Meson, Ninja and binutils. These are development dependencies; the
installed controller does not need them.

```sh
rustup toolchain install stable --profile minimal \
  --component rustfmt --component clippy --component rust-docs \
  --target x86_64-unknown-linux-musl
make check
make static-test
```

The executable is
`target/x86_64-unknown-linux-musl/release/minibookx-ctl`. The build script downloads
and verifies the pinned libdbus source, then builds its client library locally.
It uses a bundled source archive when building from the corresponding source
release.

`rust-docs` supplies the compiler's runtime license notices embedded in every
build; it is required even when you do not use Rust's local documentation.

The default C compiler is `musl-gcc`. A compatible compiler wrapper can be selected
explicitly:

```sh
CC_x86_64_unknown_linux_musl=/absolute/path/to/compiler-wrapper \
  make static-test
```

| Command | Purpose |
|---|---|
| `make format` | Format Rust source |
| `make check` | Formatting, Clippy, Rust/Python tests, unit checks, documentation links and ShellCheck |
| `make static-test` | Build, verify and test the static musl executable |
| `make distros` | Check prerequisites, package advice and staged/mocked-service installation in seven images across five distributions |
| `make package` | Package a clean commit and its matching static build into `dist/` |
| `make verify-release` | Check release assets against the current source commit |

`scripts/` contains the static-build driver, pinned libdbus source handling,
packaging, shared artifact validation and publication. `make` is their common
entry point. Tests and their fixtures live under `tests/`; the optional offline
panel builder stays with its manual feature under `contrib/vbt/`.

The build embeds the preset template, single systemd unit and license notices in
the executable. Rust runtime notices come from the compiler used for that build;
other notices come from `packaging/licenses/`. No network access or compiler is
used by the installed service.

## Real systemd lifecycle test

The container installer test substitutes `systemctl`. To test actual service
supervision, use the separate file-backed QEMU fixture in `tests/systemd/`.
It compiles the production broker, worker, IPC, service and logind code with fake
EC, temperatures, posture sensors and a file sink for tablet events. Hardware
session preparation, lease handling and recovery use a file-only model rather
than the production `hardware.rs` implementation. The shipped
systemd unit runs under real PID 1. The feature-gated fixture is not part of a
normal/release executable and refuses a host without its QEMU identity.

Prerequisites are Docker, QEMU and a compatible prebuilt x86_64 guest kernel.
The kernel needs initramfs/gzip, serial console, namespaces, cgroups, seccomp,
proc/sysfs and the virtual devices required for systemd. KVM is the normal
accelerator; `--accel tcg` is available when KVM is unavailable. No host unit or
physical device is changed; the guest has no disk or network.

After the normal static build prepares libdbus, build the fixture with the same
static inputs:

```sh
PKG_CONFIG_LIBDIR="$PWD/target/native/libdbus-musl/lib/pkgconfig" \
PKG_CONFIG_PATH='' PKG_CONFIG_ALLOW_CROSS=1 DBUS_1_STATIC=1 \
CC_x86_64_unknown_linux_musl=musl-gcc \
RUSTFLAGS='-C target-feature=+crt-static' \
  cargo build --locked --release --target x86_64-unknown-linux-musl \
  --features systemd-test-fixture --bin minibookx-systemd-fixture
docker build -t minibookx-systemd-test -f tests/systemd/Dockerfile .
vm_container=$(docker create minibookx-systemd-test)
docker export --output systemd-rootfs.tar "$vm_container"
docker rm "$vm_container"
python3 tests/systemd/run_vm.py \
  --kernel /absolute/path/to/prebuilt-guest-bzImage \
  --rootfs systemd-rootfs.tar \
  --fixture target/x86_64-unknown-linux-musl/release/minibookx-systemd-fixture \
  --output /absolute/path/to/new-vm-results
```

Use your compiler wrapper instead of `musl-gcc` when applicable. Replace both
absolute paths; the result directory must not exist. A failed export needs
cleanup with `docker rm` for the container just created. Review `result.json`
and `serial.log`; a pass requires the guest assertions and successful VM exit.

The suite checks start/stop/restart, the sandbox and privilege drop, real logind
authorization, worker/broker loss, systemd watchdog handling, `ExecStopPost`,
tablet OFF and recovery-record retention after an injected restoration fault.
Fan-disabled cases start without debugfs, EC or CPU-temperature files, exercise
profile reload and reject a changed startup flag. A guest-only fake debugfs
source checks late EC-file creation through the unit's real private bind.
It does not validate physical PWM, real sensor orientation, RAPL writes,
production hardware lease/recovery guards, libinput consumption, panel output
or suspend hardware. Production lease and hardware recovery checks remain covered
by their filesystem/unit tests, not by this VM substitution.

The suite also checks lid-event integration and descriptor
isolation, bounded sampling under early wakes, diagnostic command suppression,
fanless/no-tablet operation and thermal protection with a stopped worker.
It covers the fake hardware model and
namespace mechanics above, not real late loading of `ec_sys`. Re-run the suite
after relevant changes; it does not qualify a new build on the physical laptop.

### Evidence boundaries

| Check | What it validates |
|---|---|
| Rust tests with fake backends | Policy, parsing, bounds, errors and recovery decisions |
| Distribution containers | Prerequisite installation, reports, account/config validation, mocked installer sequencing and GPU package metadata/solver checks |
| QEMU with systemd PID 1 | Real process supervision, sandbox/logind and lifecycle against a file-only hardware session/recovery model |
| Documented U300 observations | Physical behavior of the recorded software/device combination only |

The current physical-check scope is recorded once in the
[hardware qualification table](../hardware/README.md#hardware-qualification).
It distinguishes a controller reinstall from a clean OS installation and
existing power-policy preservation from untested RAPL overrides.

Native firmware-inclusion mechanisms were checked with synthetic file data:
mkinitcpio on Arch and dracut on Fedora produced images containing the file;
Debian's initramfs-tools `copy_file` helper preserved its bytes in the merged-usr
image layout. These checks validate inclusion mechanics, not panel timings or a
physical boot through every distribution's loader.

The disposable CachyOS container disables its pacman's nested network-namespace
sandbox so package hooks can run under Docker's restricted capabilities.
Signature checks and the remaining package sandbox stay enabled. This test-only
setting is never installed on a user's system.

Re-run the relevant checks after material changes. A VM pass does not qualify a
new binary on the laptop, and older physical results do not become tests of the
current checkout. A clean-OS hardware installation and live RAPL write/restore
under the current unit remain separate qualifications.

### Distribution matrix

`make distros` uses these disposable images:

| Distribution | Image |
|---|---|
| Debian 13 | `debian:13-slim` |
| Ubuntu 24.04 | `ubuntu:24.04` |
| Ubuntu 26.04 | `ubuntu:26.04` |
| Fedora 43 | `fedora:43` |
| Fedora 44 | `fedora:44` |
| Arch | `archlinux:base` |
| CachyOS | `cachyos/cachyos:latest` |

Checks install runtime prerequisites, run reports, and exercise account/config
validation, staged updates and removal with mocked service control. GPU checks
query candidate metadata and request solver-only transactions; they do not install
GPU packages. APT media alternatives are checked; Fedora uses its native stack.
RPM Fusion transactions, codec coverage and real playback are not qualified.
Python and diffutils are test tools, not runtime dependencies.

## Release files and versions

```sh
minibookx-ctl --version
minibookx-ctl --build-info
```

The release version comes from `Cargo.toml`. Each published release increments
its patch version, with the matching change in `Cargo.lock`. For example,
`0.1.2` uses commit title **`Release (v0.1.2)`**, tag **`v0.1.2`** and GitHub
release title **`Release (v0.1.2)`**. Tags use a `v` prefix and no hash suffix.

The executable still reports `0.1.2+g<commit>`, using the source commit's first
12 characters. Asset filenames keep the same build identity; metadata records
the full commit, compiler and static libdbus identity. Make the final commit
before building: changing even its message changes the hash and requires a new
build. An older release does not need rebuilding when it is retired.

Each release contains five assets:

| Asset | Contents |
|---|---|
| `minibookx-ctl-x86_64-linux` | Static executable used by the bootstrap |
| `minibookx-ctl-0.1.2-<commit>-x86_64-unknown-linux-musl.tar.gz` | Executable, configuration template, service, documentation and notices |
| `minibookx-ctl-0.1.2-<commit>-sources.tar.gz` | Corresponding source, locked Cargo dependencies and pinned libdbus source |
| `build-metadata.json` | Build and source identity |
| `SHA256SUMS` | Checksums of the other four assets |

Packaging requires committed source and a binary built from that exact clean
commit. The read-only developer tool checks the artifacts without executing
their binary:

```sh
make verify-release
```

Use `RELEASE_DIR=/path/to/assets` and `COMMIT=full-commit` when checking another
release. `make help` lists the supported developer commands.

Python release tooling is not part of installation. The bootstrap resolves
`latest` once, validates that it names a tag in this repository, and downloads
that tag's executable and checksums.

Pull requests run checks without publishing. Main-branch builds publish only
after the native checks, static build/tests, release validation and seven
container suites pass. The [distribution matrix](#distribution-matrix)
lists their versions and scope. Every push to `main` is a release candidate;
bump the patch version before pushing it. Published versions are immutable:
retrying the exact same build can verify and resume it, but a different commit
cannot reuse its tag or replace its assets. A new release must use a new version.

```mermaid
flowchart LR
    C[Commit on main] --> T[Native checks and tests]
    T --> B[Static executable and tests]
    B --> P[Five validated release assets]
    P --> D[Runtime, package advice and install checks in seven images]
    D --> R[Versioned release for this commit]
```

The source archive records its compiler in `BUILD-TOOLCHAIN.txt`. Future stable
Rust versions may produce different bytes from the same source. Security updates
to a static dependency require a new executable; updating the host's shared
libdbus does not update the library linked into an existing release.

## After kernel, BIOS or firmware updates

Keep a working boot entry. Before relying on unattended control:

1. Start the service and inspect `--doctor`, `--status` and its journal.
2. Stop it and confirm that firmware fan control returns.
3. Start it again and check the selected profile and observed power limits.
4. Fold and unfold the laptop; check keyboard, touchpad and screen orientation.
5. Close and open the lid; check the expected sleep/wake behavior.
6. Suspend and resume; check input and fan control again.

If detection fails, leave the service stopped and compare the device with the
[supported hardware](../hardware/README.md). Do not bypass a revision check to make
the service start. The same unit runs recovery after a stop or failure and
before a restart; there is no separate recovery service.

Display modifications are maintained separately:

| Change | Manual guide |
|---|---|
| VBT timing and early firmware | [Display setup](../guide/display.md) |
| i915 timeout investigation | [Unbuilt developer RFC](../../contrib/i915/README.md) |
| Boot menu and console orientation | [Boot settings](../guide/boot.md) |
| GNOME rotation behavior | [Tablet mode](../guide/tablet-mode.md) |

Start with the [stock-driver 60 Hz setup](../guide/display.md) on the tested
panel. The supplied RFC is not a verified replacement module. Any independently
qualified replacement must match the distribution
kernel's exact source, configuration, headers, toolchain requirements and signing
policy. Repeat the manual module checks and review boot entries after a kernel
update. Installing or updating minibookx-ctl does not build a module, install
a rebuild hook or refresh boot files.
