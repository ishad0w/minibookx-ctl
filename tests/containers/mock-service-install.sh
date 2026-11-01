#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Root only in a disposable Docker container. systemctl is replaced, not started.
set -eu
test -f /.dockerenv || { echo 'Run only in the disposable Docker fixture.' >&2; exit 1; }
test "$(id -u)" -eq 0
test ! -e /run/minibookx-ctl
test ! -e /usr/local/bin/minibookx-ctl

cp /test-binary /tmp/private-installer
chmod 700 /tmp/private-installer
mkdir -p /etc/minibookx-ctl
cp /source/config/presets.conf /etc/minibookx-ctl/config.conf
sed -i 's/^fan_control = true$/fan_control = false/' /etc/minibookx-ctl/config.conf
grep -qx 'fan_control = false' /etc/minibookx-ctl/config.conf
printf '\n# Config preserved by mocked-service installation\n' >> /etc/minibookx-ctl/config.conf
cp /etc/minibookx-ctl/config.conf /tmp/valid-installer-config
chmod 640 /etc/minibookx-ctl/config.conf

# These fixed tools are only replaced inside this private container. Account
# management and the config validator still run for real; no unit or hardware
# daemon is started. The runtime directory must stay absent, so --recover has
# no ownership record and cannot reach EC, power-limit or module writes.
cat > /usr/bin/systemctl <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$*" >> /tmp/minibookx-systemctl-calls
for argument in "$@"; do unit=$argument; done
case " $* " in
    *' show '*)
        if [ "$unit" = minibookx-ctl.service ] && [ -f /etc/systemd/system/minibookx-ctl.service ]; then
            printf 'LoadState=loaded\nActiveState='
            if [ -f /tmp/minibookx-mock-active ]; then printf 'active\n'; else printf 'inactive\n'; fi
            if [ -f /tmp/minibookx-mock-enabled ]; then printf 'UnitFileState=enabled\n'; else printf 'UnitFileState=disabled\n'; fi
        else
            printf 'LoadState=not-found\nActiveState=inactive\nUnitFileState=\n'
        fi ;;
    *' stop '*) rm -f /tmp/minibookx-mock-active ;;
    *' start '*) touch /tmp/minibookx-mock-active ;;
    *' enable '*) touch /tmp/minibookx-mock-enabled ;;
    *' disable '*) rm -f /tmp/minibookx-mock-enabled ;;
    *' reset-failed '*|*' daemon-reload '*) ;;
    *) echo 'Unexpected mocked systemctl operation.' >&2; exit 1 ;;
esac
EOF
chmod 755 /usr/bin/systemctl

# The validator must reject direct root invocation. Installation then validates
# the same bytes successfully after its actual service UID/GID/capability drop,
# even though that account cannot execute the mode-0700 source image directly.
if /tmp/private-installer --check-config-stdin < /tmp/valid-installer-config; then
    echo 'The internal validator accepted root.' >&2
    exit 1
fi
/tmp/private-installer --install
cmp /tmp/valid-installer-config /etc/minibookx-ctl/config.conf
service_uid=$(id -u minibookx-ctl)
service_gid=$(id -g minibookx-ctl)
test "$service_uid" -gt 0 && test "$service_uid" -lt 1000
test "$service_gid" -gt 0
test "$(id -G minibookx-ctl)" = "$service_gid"
test "$(stat -c '%u:%g:%a' /etc/minibookx-ctl)" = "0:$service_gid:750"
test "$(stat -c '%u:%g:%a' /etc/minibookx-ctl/config.conf)" = "0:$service_gid:640"
getent shadow minibookx-ctl | awk -F: '$2 !~ /^!/ { exit 1 }'

/tmp/private-installer --install --enable
test -f /tmp/minibookx-mock-active
test -f /tmp/minibookx-mock-enabled

# A persistently invalid edit must not stop an otherwise healthy running worker.
printf 'unknown_config_key = true\n' > /etc/minibookx-ctl/config.conf
cp /etc/minibookx-ctl/config.conf /tmp/invalid-installer-config
: > /tmp/minibookx-systemctl-calls
if /tmp/private-installer --install; then
    echo 'Invalid preserved configuration was accepted.' >&2
    exit 1
fi
test -f /tmp/minibookx-mock-active
test -f /tmp/minibookx-mock-enabled
cmp /tmp/invalid-installer-config /etc/minibookx-ctl/config.conf
cmp /tmp/private-installer /usr/local/bin/minibookx-ctl
if grep -Eq ' (stop|start|enable|disable|daemon-reload)( |$)' /tmp/minibookx-systemctl-calls; then
    echo 'Invalid config caused a service mutation.' >&2
    exit 1
fi

cp /tmp/valid-installer-config /etc/minibookx-ctl/config.conf
/tmp/private-installer --install
test -f /tmp/minibookx-mock-active
test -f /tmp/minibookx-mock-enabled
cmp /tmp/valid-installer-config /etc/minibookx-ctl/config.conf
/tmp/private-installer --uninstall
test ! -e /usr/local/bin/minibookx-ctl
test ! -e /etc/systemd/system/minibookx-ctl.service
test ! -e /tmp/minibookx-mock-active
test ! -e /tmp/minibookx-mock-enabled
cmp /tmp/valid-installer-config /etc/minibookx-ctl/config.conf
test ! -e /run/minibookx-ctl
printf 'PASS: actual private account, UID/GID-dropped config validation and mocked service install/update/removal.\n'
