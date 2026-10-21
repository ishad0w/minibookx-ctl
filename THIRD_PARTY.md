# Third-party licenses

Original project code is Apache-2.0. Dependencies retain their own licenses.
The executable embeds their notices, and releases include corresponding source
archives.

## Dependencies

| Component | Locked version | License / distribution choice |
|---|---|---|
| libc Rust bindings | 0.2.189 | MIT OR Apache-2.0; MIT notice retained |
| dbus-rs | 0.9.12 | MIT OR Apache-2.0; MIT notice retained |
| libdbus-sys bindings/build | 0.2.7 | MIT OR Apache-2.0; dbus-rs MIT notice retained |
| Static libdbus client | 1.16.2, separately built from pinned upstream source | AFL-2.1 OR GPL-2.0-or-later; distributed under AFL-2.1 |
| serde_json / serde_core | 1.0.154 / 1.0.229 | MIT OR Apache-2.0; MIT notices retained |
| itoa | 1.0.18 | MIT OR Apache-2.0; MIT notice retained |
| memchr | 2.8.3 | MIT OR Unlicense; MIT notice and COPYING retained |
| zmij | 1.0.23 | MIT; notice retained |
| Rust standard library/runtime | Build toolchain, recorded with release | MIT / Apache-2.0 and bundled notices; copied from the toolchain |
| musl C library | Bundled with the release's Rust toolchain | MIT and per-file notices; complete musl COPYRIGHT retained |

`Cargo.lock` lists the exact crate versions. Build-only and conditional crates
are included in the source archive with their licenses, even when they are not
linked into the Linux executable.

## Source archives and redistribution

### Separate Linux display RFC

The unbuilt developer patch in `contrib/i915` changes `icl_dsi.c`,
`intel_display_params.c` and `intel_display_params.h`. At its recorded base,
all three are MIT-licensed, with Intel copyright notices from 2018 or 2023.
The patch's own additions are also MIT-licensed. Retain the target files'
original notices when applying or redistributing it. The full notice and source
links are in [the RFC guide](contrib/i915/README.md#license) and
[COPYING](contrib/i915/COPYING). The Linux kernel's overall GPL license does not
replace these file-specific notices. No i915 code is linked into the controller.

### Static executable

The source archive contains the locked Cargo dependencies, their original
notices, and `third-party/sources/dbus-1.16.2.tar.xz`. The build uses that pinned C
archive; the binding crate's older bundled copy is not linked. Library sources
are unmodified.

libdbus is distributed under AFL-2.1. Its complete `COPYING` and referenced
license texts are in `packaging/licenses/libdbus/`. This build includes the client
library, not libdbus's GPL-only standalone tools. Retain the notices and distribute
the matching source archive alongside the static binary to meet the library's
source-availability and attribution requirements.

The C source version and checksum are in
[third-party/libdbus.json](third-party/libdbus.json). `build-metadata.json` and
`BUILD-TOOLCHAIN.txt` identify the release compiler. During compilation,
`build.rs` embeds the notices in `packaging/licenses/` and that compiler's Rust
runtime copyright and license files. The built-in installer copies these to
`/usr/local/share/doc/minibookx-ctl/`, alongside the project license and notices.

The direct executable asset, binary archive and source archive are published in
the same release. Redistributors must retain the notices and provide the
corresponding sources as described above; a checksum manifest alone is not a
replacement for those obligations.

## Upstream license information

Sources: [dbus-rs](https://github.com/diwic/dbus-rs),
[libdbus](https://dbus.freedesktop.org/), [libc bindings](https://github.com/rust-lang/libc),
[Rust licenses](https://www.rust-lang.org/policies/licenses),
[musl copyright](https://git.musl-libc.org/cgit/musl/tree/COPYRIGHT).
