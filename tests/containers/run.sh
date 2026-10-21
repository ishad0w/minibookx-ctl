#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Runtime prerequisite and staging checks only: no host devices, /boot, services or accounts.
set -eu
if [ "${1:-}" = --help ]; then
    printf '%s\n' \
        'Usage: tests/containers/run.sh [debian ubuntu24 ubuntu fedora43 fedora arch cachyos]' \
        'No arguments runs the complete matrix. Set MINIBOOKX_TEST_BINARY to override the static build.'
    exit 0
fi
script_dir=$(CDPATH='' cd -P -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH='' cd -P -- "$script_dir/../.." && pwd)
binary=${MINIBOOKX_TEST_BINARY:-"$project_dir/target/x86_64-unknown-linux-musl/release/minibookx-ctl"}
if [ ! -x "$binary" ]; then
    echo 'Build a static executable first, or set MINIBOOKX_TEST_BINARY to one.' >&2
    exit 2
fi
binary=$(realpath -- "$binary")
if [ "$#" -eq 0 ]; then
    set -- debian ubuntu24 ubuntu fedora43 fedora arch cachyos
fi
image_for() {
    case "$1" in
        debian) image=debian:13-slim ;;
        ubuntu) image=ubuntu:26.04 ;;
        ubuntu24) image=ubuntu:24.04 ;;
        fedora) image=fedora:44 ;;
        fedora43) image=fedora:43 ;;
        arch) image=archlinux:base ;;
        cachyos) image=cachyos/cachyos:latest ;;
        *) echo 'Choose debian, ubuntu24, ubuntu, fedora43, fedora, arch or cachyos.' >&2; return 2 ;;
    esac
    printf '%s\n' "$image"
}
# Validate the complete selection before any pull or container execution.
for distro in "$@"; do
    image_for "$distro" >/dev/null || exit 2
done
for distro in "$@"; do
    image=$(image_for "$distro")
    docker pull "$image"
    docker image inspect "$image" --format '{{json .RepoDigests}}'
    docker run --rm --cpus=2 --memory=2g --pids-limit=256 \
        --mount "type=bind,source=$project_dir,target=/source,readonly" \
        --mount "type=bind,source=$binary,target=/test-binary,readonly" \
        "$image" /bin/sh -c \
        'cp /source/tests/containers/check.sh /tmp/distro-check.sh; exec /bin/sh /tmp/distro-check.sh "$1"' \
        distro-check "$distro"
done
