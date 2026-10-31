FROM debian:13-slim
RUN printf '#!/bin/sh\nexit 101\n' > /usr/sbin/policy-rc.d \
    && chmod 755 /usr/sbin/policy-rc.d \
    && apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
       systemd systemd-sysv dbus libpam-systemd polkitd udev kmod passwd util-linux procps python3 \
    && mkdir /tmp/guest-kernel \
    && cd /tmp/guest-kernel \
    && kernel_package=$(apt-cache depends linux-image-amd64 | awk '/Depends: linux-image-[0-9]/ {print $2; exit}') \
    && test -n "$kernel_package" \
    && apt-get download "$kernel_package" \
    && dpkg-deb --fsys-tarfile ./*.deb | tar -xOf - --wildcards './boot/vmlinuz-*' > /boot/vmlinuz \
    && rm -rf /tmp/guest-kernel \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --no-create-home --shell /usr/sbin/nologin minibookx-ctl
