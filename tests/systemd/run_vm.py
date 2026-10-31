# SPDX-License-Identifier: Apache-2.0
"""Run a file-backed lifecycle fixture in a diskless, networkless QEMU guest."""

import argparse
import gzip
import hashlib
import json
import os
import signal
import stat
import subprocess
import sys
import tarfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]


def entry(name, mode, data=b"", uid=0, gid=0, rdev=(0, 0), ino=1):
    filename = name.encode() + b"\0"
    fields = (ino, mode, uid, gid, 1, 0, len(data), 0, 0, *rdev, len(filename), 0)
    body = b"070701" + b"".join(f"{v:08x}".encode() for v in fields) + filename
    body += b"\0" * (-len(body) % 4)
    body += data + b"\0" * (-len(data) % 4)
    return body


def make_image(rootfs, fixture, destination):
    files = {}
    with tarfile.open(rootfs) as archive:
        for member in archive:
            name = member.name.removeprefix("./").rstrip("/")
            if (
                not name
                or name in (".dockerenv", ".containerenv")
                or name.startswith(("dev/", "proc/", "sys/", "run/", "boot/"))
            ):
                continue
            if name.startswith("/") or ".." in Path(name).parts:
                raise ValueError("unsafe rootfs member")
            if member.isfile() or member.islnk():
                data = archive.extractfile(member).read()
                kind = stat.S_IFREG
            elif member.issym():
                data, kind = member.linkname.encode(), stat.S_IFLNK
            elif member.isdir():
                data, kind = b"", stat.S_IFDIR
            else:
                continue
            files[name] = (kind | member.mode, data, member.uid, member.gid, (0, 0))

    def put(name, data=b"", mode=stat.S_IFREG | 0o644):
        files[name] = (
            mode,
            data if isinstance(data, bytes) else data.encode(),
            0,
            0,
            (0, 0),
        )

    for directory in [
        "dev",
        "proc",
        "sys",
        "run",
        "etc/minibookx-ctl",
        "usr/local/bin",
        "etc/systemd/system",
        "etc/systemd/system/minibookx-ctl.service.d",
        "etc/systemd/system/multi-user.target.wants",
    ]:
        put(directory, mode=stat.S_IFDIR | 0o755)
    for name, number in [
        ("console", (5, 1)),
        ("null", (1, 3)),
        ("tty", (5, 0)),
        ("random", (1, 8)),
        ("urandom", (1, 9)),
    ]:
        files[f"dev/{name}"] = (stat.S_IFCHR | 0o666, b"", 0, 0, number)
    passwd = files["etc/passwd"][1]
    if b"\nminibookx-ctl:" not in b"\n" + passwd:
        put(
            "etc/passwd",
            passwd
            + b"minibookx-ctl:x:61000:61000:VM fixture:/nonexistent:/usr/sbin/nologin\n",
        )
        put("etc/group", files["etc/group"][1] + b"minibookx-ctl:x:61000:\n")
    put("etc/machine-id", b"")
    put("etc/hostname", "minibookx-systemd-test\n")
    put("usr/local/bin/minibookx-ctl", fixture.read_bytes(), stat.S_IFREG | 0o755)
    put("etc/minibookx-ctl/config.conf", (REPO / "config/presets.conf").read_bytes())
    put(
        "etc/systemd/system/minibookx-ctl.service",
        (REPO / "packaging/minibookx-ctl.service").read_bytes(),
    )
    put(
        "etc/systemd/system/minibookx-ctl.service.d/test.conf",
        """[Service]
WatchdogSec=3s
RestartSec=1s
[Unit]
StartLimitBurst=20
""",
    )
    put(
        "usr/local/bin/guest-tests.py",
        Path(__file__).with_name("guest_tests.py").read_bytes(),
    )
    put(
        "etc/systemd/system/fixture-tests.service",
        """[Unit]
After=systemd-logind.service dbus.service
Requires=systemd-logind.service dbus.service
[Service]
Type=oneshot
ExecStart=/usr/bin/python3 /usr/local/bin/guest-tests.py
StandardOutput=journal+console
StandardError=journal+console
TimeoutStartSec=150s
""",
    )
    put(
        "etc/systemd/system/multi-user.target.wants/fixture-tests.service",
        "../fixture-tests.service",
        stat.S_IFLNK | 0o777,
    )
    put(
        "init",
        """#!/bin/sh
export SYSTEMD_IN_INITRD=0
exec /sbin/init
""",
        stat.S_IFREG | 0o755,
    )
    with gzip.open(destination, "wb", compresslevel=2) as output:
        for ino, (name, data) in enumerate(files.items(), 1):
            output.write(entry(name, *data, ino=ino))
        output.write(entry("TRAILER!!!", 0))


def start_vm(command, log):
    # Apply the child-only limit in a fresh interpreter, without a Python
    # preexec_fn callback. exec preserves the PID/session used for cleanup.
    launcher = (
        "import os, resource, sys\n"
        "resource.setrlimit(resource.RLIMIT_MEMLOCK, (0, 0))\n"
        "os.execvp(sys.argv[1], sys.argv[1:])\n"
    )
    return subprocess.Popen(
        [sys.executable, "-I", "-S", "-c", launcher, *command],
        stdin=subprocess.DEVNULL,
        stdout=log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )


def run(args):
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=False)
    kernel, fixture = (
        Path(args.kernel).resolve(strict=True),
        Path(args.fixture).resolve(strict=True),
    )
    image = output / "initramfs.cpio.gz"
    make_image(Path(args.rootfs).resolve(strict=True), fixture, image)
    command = [
        "qemu-system-x86_64",
        "-no-user-config",
        "-nodefaults",
        "-machine",
        "q35",
        "-accel",
        args.accel,
        "-cpu",
        "host" if args.accel == "kvm" else "max",
        "-m",
        "1024M",
        "-smp",
        "2",
        "-display",
        "none",
        "-monitor",
        "none",
        "-serial",
        "stdio",
        "-nic",
        "none",
        "-no-reboot",
        "-smbios",
        "type=1,manufacturer=QEMU,product=MiniBookX Systemd Fixture",
        "-kernel",
        str(kernel),
        "-initrd",
        str(image),
        "-append",
        (
            "console=ttyS0,115200 rdinit=/init panic=-1 loglevel=4 "
            "systemd.unit=multi-user.target systemd.journald.forward_to_console=1"
        ),
    ]
    (output / "command.json").write_text(json.dumps(command, indent=2) + "\n")
    start = time.monotonic()

    with (output / "serial.log").open("wb") as log:
        process = start_vm(command, log)

        def interrupted(signum, frame):
            raise KeyboardInterrupt

        previous = {
            sig: signal.signal(sig, interrupted)
            for sig in (signal.SIGTERM, signal.SIGINT)
        }
        try:
            code = process.wait(timeout=180)
        except (subprocess.TimeoutExpired, KeyboardInterrupt):
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
            code = -1
        finally:
            for sig, handler in previous.items():
                signal.signal(sig, handler)
    serial = (output / "serial.log").read_text(errors="replace")
    passed = (
        code == 0
        and "VM_RESULT PASS" in serial
        and "VM_RESULT FAIL" not in serial
        and "Kernel panic" not in serial
    )
    result = {
        "passed": passed,
        "returncode": code,
        "elapsed_seconds": round(time.monotonic() - start, 2),
        "kernel_sha256": hashlib.sha256(kernel.read_bytes()).hexdigest(),
        "fixture_sha256": hashlib.sha256(fixture.read_bytes()).hexdigest(),
        "assertions": [line for line in serial.splitlines() if "VM_ASSERT" in line],
        "scope": (
            "Real broker/runtime/IPC/logind/unit sandbox. File-backed replacement "
            "hardware Session/prepare/recover and sensors/switch; production "
            "hardware lease validation is unit-tested separately. "
            "No physical fan, RAPL or libinput validation."
        ),
    }
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    return 0 if passed else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["kernel", "rootfs", "fixture", "output"]:
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--accel", choices=["kvm", "tcg"], default="kvm")
    raise SystemExit(run(parser.parse_args()))
