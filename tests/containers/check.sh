#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Disposable-container checks only. Never start the daemon or expose host devices.
set -eu
case "$1" in
    debian|ubuntu|ubuntu24)
        printf '#!/bin/sh\nexit 101\n' > /usr/sbin/policy-rc.d
        chmod 755 /usr/sbin/policy-rc.d
        apt-get update
        DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
            systemd udev dbus polkitd kmod passwd libinput10 iio-sensor-proxy ca-certificates curl coreutils sudo diffutils python3
        ;;
    fedora|fedora43)
        dnf install -y --setopt=install_weak_deps=False \
            systemd systemd-udev dbus polkit kmod shadow-utils util-linux libinput iio-sensor-proxy ca-certificates curl-minimal coreutils sudo diffutils python3
        ;;
    arch|cachyos)
        if [ "$1" = cachyos ]; then
            # CachyOS pacman also isolates package hooks in a network namespace.
            # Nested network namespaces are unavailable in this unprivileged
            # Docker fixture. Keep signature checks and the download sandbox;
            # relax only that extra namespace, inside this disposable container.
            sed -i '/^\[options\]$/a DisableSandboxNetwork' /etc/pacman.conf
        fi
        pacman -Syu --needed --noconfirm \
            systemd dbus polkit kmod shadow libinput iio-sensor-proxy ca-certificates curl coreutils sudo diffutils python
        ;;
    *) exit 2 ;;
esac
# The production device filter requires these stock module-load dependencies.
test -f /usr/lib/systemd/system/modprobe@.service || test -f /lib/systemd/system/modprobe@.service
mkdir /stage
cp /test-binary /tmp/minibookx-ctl
chmod 755 /tmp/minibookx-ctl
/tmp/minibookx-ctl --version
/tmp/minibookx-ctl --help
python3 /source/tests/tooling/verify_units.py
/tmp/minibookx-ctl --doctor --json > /tmp/doctor.json
/tmp/minibookx-ctl --debug --json > /tmp/debug.json
python3 - <<'PY'
import json
from pathlib import Path
for mode in ('doctor', 'debug'):
    report = json.loads(Path('/tmp/' + mode + '.json').read_text())
    assert report['schema_version'] == 2
    assert report['mode'] == mode
    assert report['read_only'] is True
    assert {'system', 'display', 'graphics', 'service', 'configuration', 'fan', 'power', 'tablet'} <= {
        section['key'] for section in report['sections']
    }
    graphics = next(section for section in report['sections'] if section['key'] == 'graphics')
    rows = {field['key']: field for field in graphics['fields']}
    assert rows['distribution']['state'] == 'ok', rows
    # Root reports must successfully query the real database after dropping UID.
    assert 'package_status' in rows and 'package_inventory' not in rows, rows
    if mode == 'debug':
        assert 'mesa_gl' in rows, rows
        system = next(section for section in report['sections'] if section['key'] == 'system')
        loader = next(field for field in system['fields'] if field['key'] == 'module_loader')
        assert loader['state'] == 'ok', loader
        assert loader['value'].endswith('/modprobe'), loader
PY
if getent passwd minibookx-ctl; then
    echo 'Unexpected pre-existing service account in base image.' >&2
    exit 1
fi
/tmp/minibookx-ctl --install --destdir /stage
test -x /stage/usr/local/bin/minibookx-ctl
test -s /stage/etc/systemd/system/minibookx-ctl.service
test ! -e /stage/etc/systemd/system/minibookx-ctl-recover.service
test -s /stage/etc/minibookx-ctl/config.conf
test -s /stage/usr/local/share/doc/minibookx-ctl/LICENSE
test -s /stage/usr/local/share/doc/minibookx-ctl/licenses/rust-runtime/COPYRIGHT-library.html
cmp /tmp/minibookx-ctl /stage/usr/local/bin/minibookx-ctl
printf '\n# Container update preservation check\n' >> /stage/etc/minibookx-ctl/config.conf
cp /stage/etc/minibookx-ctl/config.conf /tmp/saved-config
/tmp/minibookx-ctl --install --destdir /stage
cmp /tmp/saved-config /stage/etc/minibookx-ctl/config.conf
/tmp/minibookx-ctl --uninstall --destdir /stage
test ! -e /stage/usr/local/bin/minibookx-ctl
cmp /tmp/saved-config /stage/etc/minibookx-ctl/config.conf
if getent passwd minibookx-ctl; then
    echo 'Staging unexpectedly created a live service account.' >&2
    exit 1
fi
printf 'PASS: %s runtime prerequisites, static executable and staged install/update/uninstall.\n' "$1"
/bin/sh /source/tests/containers/mock-service-install.sh
/bin/sh /source/tests/containers/check-package-advice.sh "$1"
printf 'Not tested in Docker: service startup, hardware I/O, GPU acceleration, suspend or boot.\n'
