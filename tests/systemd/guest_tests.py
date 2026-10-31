# SPDX-License-Identifier: Apache-2.0
"""Runs ONLY in the disposable QEMU guest, under real systemd PID 1."""

import errno
import json
import os
import signal
import socket
import stat
import subprocess
import time
import traceback
from pathlib import Path

UNIT = "minibookx-ctl.service"
ROOT = Path("/run/minibookx-ctl")
CONFIG = Path("/etc/minibookx-ctl/config.conf")
EC = Path("/sys/kernel/debug/ec/ec0/io")


def command(*args, check=True):
    return subprocess.run(
        args,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=check,
        timeout=20,
    ).stdout.strip()


def props():
    return dict(
        line.split("=", 1)
        for line in command(
            "systemctl",
            "show",
            UNIT,
            "-p",
            "MainPID",
            "-p",
            "NRestarts",
            "-p",
            "ActiveState",
            "-p",
            "Result",
        ).splitlines()
    )


def wait_for(test, description, seconds=20):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if test():
            return
        time.sleep(0.1)
    raise AssertionError(description)


def started():
    if props()["ActiveState"] != "active":
        return False
    try:
        return json.loads((ROOT / "policy/status.json").read_text())["active"]
    except (FileNotFoundError, json.JSONDecodeError):
        return False


def events():
    try:
        return (ROOT / "switch-events").read_text().splitlines()
    except FileNotFoundError:
        return []


def stopped():
    assert props()["ActiveState"] == "inactive", props()
    if EC.exists():
        assert not EC.read_bytes()[0x4F] & 0x40
    assert not (ROOT / "manual.owner").exists()
    assert not (ROOT / "module.lease").exists()
    assert events()[-1] == "0", events()


def start(tablet=True):
    command("systemctl", "start", UNIT)
    wait_for(started, "service did not become ready")
    if tablet:
        wait_for(
            lambda: events() and events()[-1] == "1", "tablet ON was not delivered"
        )


def reads(pid, operation):
    for directory in (ROOT, ROOT / "policy"):
        try:
            return (
                (directory / f"reads-{pid}").read_text().splitlines().count(operation)
            )
        except FileNotFoundError:
            pass
    return 0


def fd_links(pid):
    links = []
    for fd in Path(f"/proc/{pid}/fd").iterdir():
        try:
            links.append(os.readlink(fd))
        except FileNotFoundError:
            pass  # The live worker may close an unrelated status/config file.
    return links


def lid_checks(main):
    # The broker opens the source after both forks; neither child receives it.
    inode = next(
        line.split()[6]
        for line in Path("/proc/net/unix").read_text().splitlines()
        if line.endswith(str(ROOT / "lid-events"))
    )
    link = f"socket:[{inode}]"
    assert link in fd_links(main)
    children = Path(f"/proc/{main}/task/{main}/children").read_text().split()
    for pid in children:
        assert link not in fd_links(pid)

    with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as source:

        def notify(state):
            (ROOT / "lid-state").write_text(state)
            source.sendto(b"event", str(ROOT / "lid-events"))

        notify("closed")
        wait_for(lambda: events()[-1] == "0", "closed lid did not release tablet")
        notify("open")
        wait_for(
            lambda: events()[-1] == "1", "open lid did not restore posture detection"
        )

        # Frequent unrelated readable-FD wakes must not multiply sensor reads.
        before = reads(main, "posture")
        event_count = len(events())
        started_at = time.monotonic()
        for _ in range(60):
            notify("open")
            time.sleep(0.02)
        elapsed = time.monotonic() - started_at
        samples = reads(main, "posture") - before
        assert 2 <= samples <= int(elapsed / 0.25) + 2, (elapsed, samples)
        assert len(events()) == event_count, events()[event_count:]
        print(
            f"VM_OPERATIONS lid_events=60 elapsed_s={elapsed:.3f} "
            f"posture_samples={samples} repeated_tablet_commands=0",
            flush=True,
        )
        print(
            "VM_ASSERT PASS lid transitions, descriptor isolation, bounded early-wake sampling",
            flush=True,
        )

    # Moving -> Valid changes diagnostics while the confirmed tablet bool stays ON.
    for rotated in (True, False):
        before = reads(main, "posture")
        marker = ROOT / "posture-rotated"
        marker.touch() if rotated else marker.unlink()
        wait_for(
            lambda before=before: reads(main, "posture") >= before + 3,
            "posture sampling stalled",
        )
        assert len(events()) == event_count, events()[event_count:]
    print("VM_ASSERT PASS diagnostic changes do not repeat tablet commands", flush=True)


def no_tablet_fanless():
    original = CONFIG.read_text()
    CONFIG.write_text(original.replace("fan_control = true", "fan_control = false", 1))
    (ROOT / "forbid-ec").touch()
    (ROOT / "no-tablet").touch()
    (ROOT / "lid-events").unlink(missing_ok=True)
    start(tablet=False)
    main = int(props()["MainPID"])
    policy = worker(main)
    before = len(events())
    time.sleep(1.3)
    assert reads(main, "cpu") == reads(main, "posture") == reads(policy, "cpu") == 0
    assert not (ROOT / "lid-events").exists()
    assert len(events()) == before
    print("VM_OPERATIONS no_tablet_fanless_cpu_reads=0 posture_samples=0", flush=True)
    command("systemctl", "stop", UNIT)
    assert props()["ActiveState"] == "inactive"
    assert not (ROOT / "module.lease").exists()
    (ROOT / "no-tablet").unlink()
    (ROOT / "forbid-ec").unlink()
    CONFIG.write_text(original)
    print(
        "VM_ASSERT PASS no-tablet and fanless need no lid/posture/temperature access",
        flush=True,
    )


def independent_thermal_guard():
    start()
    main = int(props()["MainPID"])
    policy = worker(main)
    before = reads(main, "cpu")
    os.kill(policy, signal.SIGSTOP)
    try:
        (ROOT / "temperature").write_text("97000")
        raised_at = time.monotonic()
        wait_for(
            lambda: EC.read_bytes()[0x4B] == 74,
            "broker thermal guard waited for worker",
            2,
        )
        assert reads(main, "cpu") > before
        assert props()["MainPID"] == str(main)
        print(
            f"VM_OPERATIONS stopped_worker_cpu_reads={reads(main, 'cpu') - before} "
            f"thermal_response_s={time.monotonic() - raised_at:.3f}",
            flush=True,
        )
    finally:
        os.kill(policy, signal.SIGCONT)
        (ROOT / "temperature").unlink(missing_ok=True)
        command("systemctl", "stop", UNIT)
    stopped()
    print("VM_ASSERT PASS independent thermal deadline with stopped worker", flush=True)


def hot_takeover():
    # EC is a regular file on the guest's tmpfs, never a passed-through device.
    data = bytearray(EC.read_bytes())
    data[0x4B] = 74
    EC.write_bytes(data)
    (ROOT / "temperature").write_text("90000")
    try:
        start()
        main = int(props()["MainPID"])
        policy = worker(main)
        wait_for(lambda: reads(policy, "cpu") >= 3, "hot startup did not sample")
        assert EC.read_bytes()[0x4B] == 74
        assert reads(main, "pwm=0") == 0, "hot startup stopped the firmware fan"
        assert (
            json.loads((ROOT / "policy/status.json").read_text())["policy_applied_raw"]
            == 74
        )
    finally:
        (ROOT / "temperature").unlink(missing_ok=True)
        command("systemctl", "stop", UNIT)
        data = bytearray(EC.read_bytes())
        data[0x4B] = 0
        EC.write_bytes(data)
    stopped()
    print(
        "VM_ASSERT PASS hot takeover preserves firmware PWM without an OFF write",
        flush=True,
    )


def tablet_faults():
    for source in ("sensor", "lid"):
        start()
        main = int(props()["MainPID"])
        before = reads(main, "cpu")
        if source == "sensor":
            (ROOT / "fail-posture").touch()
        else:
            (ROOT / "lid-state").write_text("invalid")
            with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as message:
                message.sendto(b"!", str(ROOT / "lid-events"))
        try:
            wait_for(
                lambda: (
                    json.loads((ROOT / "tablet.json").read_text())["reason"]
                    == "Unavailable"
                ),
                "tablet fault was not reported",
            )
            assert events()[-1] == "0", "failed tablet feature left input inhibited"
            samples = reads(main, "posture")
            wait_for(
                lambda main=main, before=before: reads(main, "cpu") > before + 2,
                "tablet fault stopped thermal supervision",
            )
            assert (
                props()["MainPID"] == str(main) and props()["ActiveState"] == "active"
            )
            assert EC.read_bytes()[0x4F] & 0x40
            assert reads(main, "posture") == samples, (
                "faulted sensor was polled repeatedly"
            )
        finally:
            (ROOT / "fail-posture").unlink(missing_ok=True)
            (ROOT / "lid-state").write_text("open")
            command("systemctl", "stop", UNIT)
        stopped()
    print(
        "VM_ASSERT PASS sensor/lid faults release input while cooling and guards continue",
        flush=True,
    )


def missing_tablet_and_status_errors():
    (ROOT / "lid-state").write_text("invalid")
    try:
        start(tablet=False)
        assert props()["ActiveState"] == "active"
        assert events()[-1] == "0"
        assert EC.read_bytes()[0x4F] & 0x40
        assert json.loads((ROOT / "tablet.json").read_text())["reason"] == "Unavailable"
    finally:
        (ROOT / "lid-state").write_text("open")
        command("systemctl", "stop", UNIT)
    stopped()
    print("VM_ASSERT PASS missing lid at startup leaves cooling available", flush=True)

    blocked = [ROOT / "tablet.tmp", ROOT / "fan.tmp", ROOT / "policy/status.tmp"]
    for path in blocked:
        path.mkdir()
    try:
        command("systemctl", "start", UNIT)
        main = int(props()["MainPID"])
        policy = worker(main)
        before = reads(policy, "cpu")
        wait_for(
            lambda: reads(policy, "cpu") >= before + 4, "status I/O interrupted policy"
        )
        assert props()["MainPID"] == str(main) and props()["ActiveState"] == "active"
        assert EC.read_bytes()[0x4F] & 0x40
    finally:
        for path in blocked:
            path.rmdir()
        command("systemctl", "stop", UNIT)
    stopped()
    print(
        "VM_ASSERT PASS status write failures do not stop cooling or watchdog",
        flush=True,
    )


def status_retry_recovery():
    original = CONFIG.read_text()
    CONFIG.write_text(original.replace("fan_control = true", "fan_control = false", 1))
    (ROOT / "forbid-ec").touch()
    blocked = [ROOT / "fan.tmp", ROOT / "power.tmp", ROOT / "tablet.tmp"]
    for path in blocked[:2]:
        path.mkdir()
    for name in ("fan.json", "power.json"):
        (ROOT / name).unlink(missing_ok=True)
    try:
        start()
        main = int(props()["MainPID"])
        policy = worker(main)
        assert not (ROOT / "fan.json").exists()
        assert not (ROOT / "power.json").exists()
        assert json.loads((ROOT / "tablet.json").read_text())["tablet"] is True
        blocked[2].mkdir()
        (ROOT / "fail-posture").touch()

        def failure_reported():
            journal = command(
                "journalctl", "-b", f"_PID={main}", "--no-pager", "-o", "cat"
            )
            return any(
                "cannot publish" in line and "tablet status" in line
                for line in journal.splitlines()
            )

        wait_for(failure_reported, "tablet status write failure was not exercised")
        assert events()[-1] == "0"
        assert json.loads((ROOT / "tablet.json").read_text())["tablet"] is True
        for path in blocked:
            path.rmdir()
        (ROOT / "fail-posture").unlink()

        def recovered():
            try:
                fan = json.loads((ROOT / "fan.json").read_text())
                power = json.loads((ROOT / "power.json").read_text())
                tablet = json.loads((ROOT / "tablet.json").read_text())
            except FileNotFoundError:
                return False
            return (
                fan["enabled"] is False
                and fan["actual_raw"] is None
                and power["restoration_pending"] is False
                and tablet["tablet"] is False
                and tablet["reliable"] is False
                and tablet["reason"] == "Unavailable"
            )

        wait_for(recovered, "latest fan/power/disabled-tablet status did not retry", 8)
        assert props()["MainPID"] == str(main) and props()["ActiveState"] == "active"
        assert events()[-1] == "0"
        assert reads(main, "cpu") == reads(policy, "cpu") == 0
    finally:
        for path in blocked:
            if path.exists():
                path.rmdir()
        (ROOT / "fail-posture").unlink(missing_ok=True)
        command("systemctl", "stop", UNIT)
        (ROOT / "forbid-ec").unlink()
        CONFIG.write_text(original)
    stopped()
    print(
        "VM_ASSERT PASS latest fan/power/disabled-tablet snapshots recover after I/O errors without EC access",
        flush=True,
    )


def notify_boundary():
    start()
    main = int(props()["MainPID"])
    policy = worker(main)
    original = CONFIG.read_text()
    (ROOT / "spoof-notify").touch()
    try:
        CONFIG.write_text(original + "\n# VM reload trigger\n")
        wait_for(
            lambda: reads(policy, "spoof-notify") > 0, "spoof attempt was not executed"
        )
        assert props()["MainPID"] == str(main), "worker replaced systemd's main PID"
        assert "forged" not in command("systemctl", "show", UNIT, "-p", "StatusText")
    finally:
        (ROOT / "spoof-notify").unlink()
        CONFIG.write_text(original)
        command("systemctl", "stop", UNIT)
    stopped()
    print(
        "VM_ASSERT PASS systemd rejects worker MAINPID/STATUS/watchdog notifications",
        flush=True,
    )


def sandbox():
    block = Path("/dev/minibookx-test-block")
    os.mknod(block, stat.S_IFBLK | 0o600, os.makedev(7, 0))
    try:
        fd = os.open(block, os.O_RDWR)
    except OSError as error:
        assert error.errno in (errno.ENXIO, errno.ENODEV), error
    else:
        os.close(fd)
    # Major 89 is the registered Linux I2C device ABI. No real bus exists;
    # ENXIO/ENODEV is expected after the cgroup filter permits the open.
    os.mknod("/dev/i2c-0", stat.S_IFCHR | 0o600, os.makedev(89, 0))
    os.mknod("/dev/minibookx-test-input", stat.S_IFCHR | 0o600, os.makedev(13, 64))
    dropin = Path("/etc/systemd/system/minibookx-ctl.service.d/probe.conf")
    dropin.write_text(
        "[Service]\nExecStartPost=/usr/local/bin/minibookx-ctl --sandbox-probe\n"
    )
    command("systemctl", "daemon-reload")
    try:
        start()
        command("systemctl", "stop", UNIT)
        stopped()
    finally:
        dropin.unlink()
        command("systemctl", "daemon-reload")
    print(
        "VM_ASSERT PASS device filter denies block access and permits uinput/I2C",
        flush=True,
    )


def installer_rollback():
    start()
    binary = Path("/usr/local/bin/minibookx-ctl")
    original = binary.read_bytes()
    config = CONFIG.read_bytes()
    candidate = Path("/usr/local/bin/minibookx-test-update")
    candidate.write_bytes(original + b"MINIBOOKX_VM_REJECT_START")
    candidate.chmod(0o755)
    try:
        result = subprocess.run(
            [str(candidate), "--install"],
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode != 0, result.stdout
        assert "previous files restored" in result.stderr, result.stderr
        assert binary.read_bytes() == original
        assert CONFIG.read_bytes() == config
        wait_for(started, "old service did not resume after rollback")
    finally:
        candidate.unlink()
        command("systemctl", "stop", UNIT)
    stopped()
    print(
        "VM_ASSERT PASS real installer rolls back failed auto-restarting update",
        flush=True,
    )


def release_with_stalled_guardian(target):
    start()
    main = int(props()["MainPID"])
    policy = worker(main)
    children = Path(f"/proc/{main}/task/{main}/children").read_text().split()
    guardian = next(int(pid) for pid in children if int(pid) != policy)
    restores = reads(main, "sensor-restore")
    os.kill(guardian, signal.SIGSTOP)
    try:
        wait_for(
            lambda: "\nState:\tT" in Path(f"/proc/{guardian}/status").read_text(),
            "guardian did not stop",
        )
        # Worker TERM exercises Restore, the same request used before sleep.
        # None of these paths signals the guardian before the observation.
        if target == "sensor":
            (ROOT / "fail-posture").touch()
        else:
            os.kill(main if target == "broker" else policy, signal.SIGTERM)
        wait_for(lambda: not EC.read_bytes()[0x4F] & 0x40, "fan stayed manual")
        # A fan return after killing the guardian is too late. It must happen
        # while that child is still stopped, before OFF/exit can be confirmed.
        assert "\nState:\tT" in Path(f"/proc/{guardian}/status").read_text()
        assert reads(main, "sensor-restore") == restores
        os.kill(guardian, signal.SIGCONT)
        if target == "sensor":
            wait_for(
                lambda: props()["MainPID"] != str(main),
                "sensor fault did not stop broker",
            )
        else:
            wait_for(
                lambda: props()["ActiveState"] == "inactive",
                "clean release did not finish",
            )
    finally:
        (ROOT / "fail-posture").unlink(missing_ok=True)
        try:
            os.kill(guardian, signal.SIGCONT)
        except ProcessLookupError:
            pass
        command("systemctl", "stop", UNIT)
        if target == "sensor":
            # stop may already have collected the inactive unit. Recovery is
            # still verified below; reset-failed need not find a loaded unit.
            command("systemctl", "reset-failed", UNIT, check=False)
    stopped()
    assert reads(main, "sensor-restore") > restores
    print(
        f"VM_ASSERT PASS {target} release: fan auto precedes guardian; sensor waits for OFF",
        flush=True,
    )


def fanless():
    original = CONFIG.read_text()
    assert "fan_control = true" in original
    disabled = original.replace("fan_control = true", "fan_control = false", 1)
    ROOT.mkdir(exist_ok=True)
    (ROOT / "forbid-ec").touch()
    CONFIG.write_text(disabled)
    start()
    main = int(props()["MainPID"])
    worker(main)
    lid_checks(main)
    snapshot = json.loads((ROOT / "policy/status.json").read_text())
    fan = json.loads((ROOT / "fan.json").read_text())
    assert snapshot["accepted_config"]["fan_control"] is False, snapshot
    assert snapshot["manual"] is False and snapshot["temperature_reads"] == 0
    assert snapshot["cpu_c"] is None and snapshot["policy_applied_raw"] is None
    assert fan["enabled"] is False and fan["actual_raw"] is None, fan
    assert not EC.exists()
    assert not (ROOT / "manual.owner").exists()

    # Inspect the real unit namespace: sysfs remains read-only while the
    # deliberate debugfs exception permits EC files created after setup.
    mounts = {}
    for line in Path(f"/proc/{main}/mountinfo").read_text().splitlines():
        fields = line.split()
        mounts[fields[4]] = set(fields[5].split(","))
    assert "ro" in mounts["/proc/sys"], mounts
    assert "ro" in mounts["/sys/kernel/debug"], mounts
    assert "rw" in mounts[str(ROOT / "hardware/debugfs")], mounts
    private = ROOT / "hardware"
    assert private.stat().st_mode & 0o777 == 0o700
    print(
        "VM_ASSERT PASS fan disabled without EC/coretemp; real namespace shape",
        flush=True,
    )

    # A fanless worker still has to renew the broker's independent 12s lease.
    time.sleep(13)
    assert props()["MainPID"] == str(main) and started(), props()
    print("VM_ASSERT PASS fanless independent heartbeat", flush=True)

    CONFIG.write_text(disabled.replace("preset = balance", "preset = quiet", 1))
    wait_for(
        lambda: (
            json.loads((ROOT / "policy/status.json").read_text())["accepted_config"][
                "preset"
            ]
            == "quiet"
        ),
        "fanless profile reload did not apply",
    )
    assert not EC.exists()

    # Startup choices must never be falsely acknowledged by a live reload.
    CONFIG.write_text(original)
    wait_for(
        lambda: json.loads((ROOT / "policy/status.json").read_text())["config_error"],
        "startup-only fan change was silently accepted",
    )
    snapshot = json.loads((ROOT / "policy/status.json").read_text())
    assert snapshot["accepted_config"]["fan_control"] is False
    assert not EC.exists()
    CONFIG.write_text(disabled)
    command("systemctl", "stop", UNIT)
    stopped()
    fail_and_restart("fanless worker SIGKILL", "worker", signal.SIGKILL)
    assert not EC.exists()
    (ROOT / "forbid-ec").unlink()
    CONFIG.write_text(original)
    print("VM_ASSERT PASS fanless stop/restart and restart-required config", flush=True)

    # Parsing fails in the dropped worker, after the tablet guardian exists.
    # The broker must still establish OFF while leaving the EC untouched.
    CONFIG.write_text(
        disabled.replace("fan_control = false", "fan_control = invalid", 1)
    )
    before = len(events())
    command("systemctl", "start", UNIT, check=False)
    command("systemctl", "stop", UNIT, check=False)
    assert "0" in events()[before:] and events()[-1] == "0"
    assert not EC.exists() and not (ROOT / "module.lease").exists()
    # Stopping an already failed start may unload this inactive unit entirely.
    command("systemctl", "reset-failed", UNIT, check=False)
    CONFIG.write_text(original)
    print(
        "VM_ASSERT PASS rejected startup configuration releases guardian without EC",
        flush=True,
    )


def worker(main):
    children = Path(f"/proc/{main}/task/{main}/children").read_text().split()
    for pid in children:
        status = Path(f"/proc/{pid}/status").read_text()
        uid = next(
            line for line in status.splitlines() if line.startswith("Uid:")
        ).split()[1]
        if uid != "0":
            assert "NoNewPrivs:\t1" in status
            for capability in ["CapInh", "CapPrm", "CapEff", "CapAmb"]:
                assert f"{capability}:\t0000000000000000" in status
            groups = next(
                line for line in status.splitlines() if line.startswith("Groups:")
            )
            assert groups.split() == ["Groups:"], groups
            gids = next(line for line in status.splitlines() if line.startswith("Gid:"))
            assert all(value != "0" for value in gids.split()[1:]), gids
            return int(pid)
    raise AssertionError(f"unprivileged worker missing from {children}")


def fail_and_restart(label, target, sig):
    start()
    before = props()
    main = int(before["MainPID"])
    old_events = len(events())
    pid = worker(main) if target == "worker" else main
    os.kill(pid, sig)
    wait_for(
        lambda: (
            int(props()["NRestarts"]) > int(before["NRestarts"])
            and props()["ActiveState"] == "active"
        ),
        f"{label}: no recovery/restart",
        25,
    )
    wait_for(started, f"{label}: policy did not restart")
    assert props()["MainPID"] != str(main)
    assert "0" in events()[old_events:], f"{label}: missing tablet OFF"
    print(f"VM_ASSERT PASS {label}", flush=True)
    command("systemctl", "stop", UNIT)
    stopped()


def run():
    assert Path("/proc/1/comm").read_text().strip() == "systemd"
    assert (
        Path("/sys/class/dmi/id/product_name").read_text().strip()
        == "MiniBookX Systemd Fixture"
    )
    print("VM_ASSERT PASS real systemd PID1", flush=True)
    # The kernel's default initramfs root can be 0777. Give this disposable
    # guest the ordinary root permissions required by the real installer.
    os.chmod("/", 0o755)
    command("systemctl", "stop", "sys-kernel-debug.mount")
    assert (
        command("systemctl", "is-active", "sys-kernel-debug.mount", check=False)
        == "inactive"
    )
    assert not EC.exists()
    fanless()
    no_tablet_fanless()
    assert (
        command("systemctl", "is-active", "sys-kernel-debug.mount", check=False)
        == "inactive"
    )
    print("VM_ASSERT PASS fanless unit does not require a mounted debugfs", flush=True)
    # No hardware is passed through. This guest-only filesystem stands in for
    # debugfs so the real unit must allow EC files created after its setup.
    command(
        "mount", "-t", "tmpfs", "-o", "mode=0700", "minibookx-test", "/sys/kernel/debug"
    )
    start()
    main = int(props()["MainPID"])
    worker(main)
    assert EC.exists() and EC.read_bytes()[0x4F] & 0x40
    print(
        "VM_ASSERT PASS EC leaf created and written after namespace startup", flush=True
    )
    print(
        "VM_ASSERT PASS start, sandbox, logind, privilege drop, tablet ON", flush=True
    )
    command("systemctl", "restart", UNIT)
    wait_for(started, "restart failed")
    assert int(props()["MainPID"]) != main
    print("VM_ASSERT PASS restart", flush=True)
    command("systemctl", "stop", UNIT)
    stopped()
    print("VM_ASSERT PASS stop, ExecStopPost, fan auto, tablet OFF", flush=True)
    hot_takeover()
    tablet_faults()
    missing_tablet_and_status_errors()
    status_retry_recovery()
    notify_boundary()
    sandbox()
    installer_rollback()
    release_with_stalled_guardian("broker")
    release_with_stalled_guardian("worker")
    release_with_stalled_guardian("sensor")
    independent_thermal_guard()
    fail_and_restart("worker SIGKILL", "worker", signal.SIGKILL)
    fail_and_restart("broker SIGKILL", "broker", signal.SIGKILL)
    fail_and_restart("systemd watchdog after worker SIGSTOP", "worker", signal.SIGSTOP)
    journal = command("journalctl", "-u", UNIT, "-b", "--no-pager", "-o", "cat")
    assert "Watchdog timeout" in journal or "watchdog timeout" in journal, journal
    assert journal.count("VM_RECOVER completed") >= 7

    # Inject only a fake backend restore error. Pending attribution must survive.
    start()
    (ROOT / "fail-restore").touch()
    command("systemctl", "stop", UNIT, check=False)
    wait_for(
        lambda: props()["ActiveState"] == "failed",
        "incomplete recovery reported success",
    )
    assert (ROOT / "manual.owner").exists()
    assert (ROOT / "module.lease").exists()
    assert EC.read_bytes()[0x4F] & 0x40
    assert events()[-1] == "0"
    print(
        "VM_ASSERT PASS failed recovery retains records and releases tablet", flush=True
    )
    # Turning fan control off cannot skip a prior attributed recovery failure.
    original = CONFIG.read_text()
    CONFIG.write_text(original.replace("fan_control = true", "fan_control = false", 1))
    command("systemctl", "start", UNIT, check=False)
    command("systemctl", "stop", UNIT, check=False)
    assert (ROOT / "manual.owner").exists()
    assert (ROOT / "module.lease").read_text().strip() == "fan=true"
    assert EC.read_bytes()[0x4F] & 0x40
    CONFIG.write_text(original)
    print(
        "VM_ASSERT PASS disabled config cannot bypass pending fan recovery", flush=True
    )
    (ROOT / "fail-restore").unlink()
    command("/usr/local/bin/minibookx-ctl", "--recover")
    command("systemctl", "reset-failed", UNIT, check=False)
    stopped()
    print("VM_ASSERT PASS recovery retry", flush=True)
    print("VM_RESULT PASS", flush=True)


def main():
    # Check BEFORE the try/finally: a rejected host invocation must not even
    # query its services, and must never run the guest poweroff cleanup.
    if (
        os.geteuid() != 0
        or Path("/proc/1/comm").read_text().strip() != "systemd"
        or Path("/sys/class/dmi/id/product_name").read_text().strip()
        != "MiniBookX Systemd Fixture"
    ):
        raise SystemExit("Refusing to run outside the isolated lifecycle guest")
    try:
        run()
    except Exception:  # noqa: BLE001 -- Report every guest failure before poweroff.
        traceback.print_exc()
        print(
            command("systemctl", "status", UNIT, "--no-pager", check=False), flush=True
        )
        print(
            command(
                "journalctl",
                "-b",
                "-u",
                UNIT,
                "-u",
                "systemd-logind",
                "--no-pager",
                check=False,
            ),
            flush=True,
        )
        print("VM_RESULT FAIL", flush=True)
    finally:
        command("systemctl", "poweroff", check=False)


if __name__ == "__main__":
    main()
