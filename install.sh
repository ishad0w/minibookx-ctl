#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Download as the invoking user; only the binary's fixed-path installer uses sudo.
set -eu
umask 077
repository=https://github.com/ishad0w/minibookx-ctl
asset=minibookx-ctl-x86_64-linux
release=
binary=
destdir=
enable=yes

fail() { printf '%s\n' "minibookx-ctl: $*" >&2; exit 1; }
usage() {
    cat <<'EOF'
Usage: sh install.sh [--release TAG | --binary PATH] [--no-enable] [--destdir PATH]

Download the latest release, verify its SHA-256 checksum, and install/start it.
--release TAG   Select an immutable v<major>.<minor>.<patch> release.
--binary PATH   Install a local executable instead of downloading a release.
--no-enable     Preserve service state; restart it only if it was already running.
--destdir PATH  Stage files without sudo, accounts, service changes or hardware I/O.

Run without sudo. Python, Git and Rust are not required.
EOF
}
while [ "$#" -gt 0 ]; do
    case "$1" in
        --release|--binary|--destdir)
            if [ "$#" -lt 2 ] || [ -z "$2" ]; then
                fail "$1 needs a value"
            fi
            case "$1" in
                --release) [ -z "$release" ] || fail 'duplicate --release'; release=$2 ;;
                --binary) [ -z "$binary" ] || fail 'duplicate --binary'; binary=$2 ;;
                --destdir) [ -z "$destdir" ] || fail 'duplicate --destdir'; destdir=$2 ;;
            esac
            shift 2 ;;
        --no-enable) enable=no; shift ;;
        --help|-h) usage; exit 0 ;;
        *) fail "unknown argument: $1" ;;
    esac
done
[ -z "$binary" ] || [ -z "$release" ] || fail 'choose either --release or --binary'
[ "$(id -u)" -ne 0 ] || fail 'run without sudo; privilege is requested only after verification'
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
    fail 'this release requires x86_64 Linux'
fi
if [ -n "$destdir" ]; then
    case "$destdir" in /*) ;; *) fail '--destdir must be an absolute path' ;; esac
    [ "$destdir" != / ] || fail '--destdir must not be /'
else
    command -v sudo >/dev/null 2>&1 || fail 'sudo is required for live installation'
fi
work=$(mktemp -d "${TMPDIR:-/tmp}/minibookx-install.XXXXXXXXXX")
trap 'rm -rf -- "$work"' 0
trap 'exit 1' HUP INT TERM
if [ -n "$binary" ]; then
    [ -f "$binary" ] || fail 'local binary does not exist'
    cp -- "$binary" "$work/$asset"
else
    command -v curl >/dev/null 2>&1 || fail 'curl is required to download a release'
    command -v sha256sum >/dev/null 2>&1 || fail 'sha256sum is required to verify a release'
    download() {
        curl --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --silent --show-error \
            --location --connect-timeout 15 --max-time 180 --retry 2 "$@"
    }
    if [ -z "$release" ]; then
        final_url=$(download --output /dev/null --write-out '%{url_effective}' "$repository/releases/latest")
        case "$final_url" in
            "$repository/releases/tag/"*) release=${final_url#"$repository/releases/tag/"} ;;
            *) fail 'latest release redirected outside the expected repository tag URL' ;;
        esac
    fi
    # Reject slashes, query strings, line breaks and unusual tags before URL use.
    case "$release" in ''|*[!v0-9.]*) fail 'invalid release tag' ;; esac
    printf '%s\n' "$release" | LC_ALL=C grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' || fail 'invalid release tag'
    base=$repository/releases/download/$release
    printf 'Downloading %s...\n' "$release"
    download --max-filesize 65536 --output "$work/SHA256SUMS" "$base/SHA256SUMS"
    download --max-filesize 16777216 --output "$work/$asset" "$base/$asset"
    # Only one fixed filename reaches sha256sum: never trust paths from a manifest.
    LC_ALL=C awk -v asset="$asset" '
        NR > 16 || length($1) != 64 || $1 ~ /[^0-9a-f]/ ||
        $2 !~ /^[A-Za-z0-9][A-Za-z0-9._-]*$/ ||
        $0 != $1 "  " $2 || seen[$2]++ { bad = 1; exit }
        $2 == asset { selected++; digest = $1 }
        END { if (bad || selected != 1) exit 1; print digest "  " asset }
    ' "$work/SHA256SUMS" > "$work/selected.sha256" || fail 'invalid release checksum manifest'
    (cd "$work" && sha256sum --check --status selected.sha256) || fail 'release checksum mismatch'
fi
chmod 700 "$work/$asset"
set -- --install
if [ -n "$destdir" ]; then
    set -- "$@" --destdir "$destdir"
    "$work/$asset" "$@"
else
    if [ "$enable" = yes ]; then set -- "$@" --enable; fi
    sudo -- "$work/$asset" "$@"
fi
