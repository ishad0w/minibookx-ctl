#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
set -eu
task_mode=${1:-}
case $task_mode in
    ''|--test|--fixture) ;;
    *) echo 'Usage: build-static.sh [--test|--fixture]' >&2; exit 2 ;;
esac
script_dir=$(CDPATH='' cd -P -- "$(dirname -- "$0")" && pwd)
cd "$script_dir/.."
target=x86_64-unknown-linux-musl
CC_x86_64_unknown_linux_musl=${CC_x86_64_unknown_linux_musl:-musl-gcc}
export CC_x86_64_unknown_linux_musl
command -v "$CC_x86_64_unknown_linux_musl" >/dev/null || {
    echo 'Install musl-tools (musl-gcc), or set CC_x86_64_unknown_linux_musl to a compatible wrapper.' >&2
    exit 1
}
target_libdir=$(rustc --print target-libdir --target "$target")
[ -d "$target_libdir" ] || {
    echo "Install the Rust target first: rustup target add $target" >&2
    exit 1
}
RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static"
export RUSTFLAGS
task_prefix="$(pwd -P)/target/native/libdbus-musl"
task_bundled_source="$script_dir/../third-party/sources/dbus-1.16.2.tar.xz"
if [ -f "$task_bundled_source" ]; then
    python3 "$script_dir/libdbus_source.py" build --prefix "$task_prefix" --cc "$CC_x86_64_unknown_linux_musl" \
        --archive "$task_bundled_source" --offline
else
    python3 "$script_dir/libdbus_source.py" build --prefix "$task_prefix" --cc "$CC_x86_64_unknown_linux_musl"
fi
# Only the reviewed static client library can enter this cross build.
PKG_CONFIG_LIBDIR="$task_prefix/lib/pkgconfig"
PKG_CONFIG_PATH=''
PKG_CONFIG_ALLOW_CROSS=1
DBUS_1_STATIC=1
export PKG_CONFIG_LIBDIR PKG_CONFIG_PATH PKG_CONFIG_ALLOW_CROSS DBUS_1_STATIC
task_binary=minibookx-ctl
if [ "$task_mode" = --fixture ]; then
    task_binary=minibookx-systemd-fixture
    cargo build --locked --release --target "$target" --features systemd-test-fixture --bin "$task_binary"
else
    cargo build --locked --release --target "$target" --bin "$task_binary"
fi
python3 "$script_dir/release_format.py" elf "target/$target/release/$task_binary"
if [ "$task_mode" = --test ]; then
    cargo test --locked --target "$target" --bin minibookx-ctl
    cargo test --locked --target "$target" --features systemd-test-fixture,dev-tools
fi
