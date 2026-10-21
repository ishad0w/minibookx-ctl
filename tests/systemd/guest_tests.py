# SPDX-License-Identifier: Apache-2.0
"""Runs ONLY in the disposable QEMU guest, under real systemd PID 1."""
from pathlib import Path
import json
import os
import signal
import subprocess
import time
import traceback

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


def start():
    command("systemctl", "start", UNIT)
    wait_for(started, "service did not become ready")
    wait_for(lambda: events() and events()[-1] == "1", "tablet ON was not delivered")


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
    print("VM_ASSERT PASS fan disabled without EC/coretemp; real namespace shape", flush=True)

    # A fanless worker still has to renew the broker's independent 12s lease.
    time.sleep(13)
    assert props()["MainPID"] == str(main) and started(), props()
    print("VM_ASSERT PASS fanless independent heartbeat", flush=True)

    CONFIG.write_text(disabled.replace("preset = balance", "preset = quiet", 1))
    wait_for(
        lambda: json.loads((ROOT / "policy/status.json").read_text())["accepted_config"]["preset"] == "quiet",
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
    CONFIG.write_text(disabled.replace("fan_control = false", "fan_control = invalid", 1))
    before = len(events())
    command("systemctl", "start", UNIT, check=False)
    command("systemctl", "stop", UNIT, check=False)
    assert "0" in events()[before:] and events()[-1] == "0"
    assert not EC.exists() and not (ROOT / "module.lease").exists()
    # Stopping an already failed start may unload this inactive unit entirely.
    command("systemctl", "reset-failed", UNIT, check=False)
    CONFIG.write_text(original)
    print("VM_ASSERT PASS rejected startup configuration releases guardian without EC", flush=True)


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
            groups = next(line for line in status.splitlines() if line.startswith("Groups:"))
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
        lambda: int(props()["NRestarts"]) > int(before["NRestarts"])
        and props()["ActiveState"] == "active",
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
    command("systemctl", "stop", "sys-kernel-debug.mount")
    assert command("systemctl", "is-active", "sys-kernel-debug.mount", check=False) == "inactive"
    assert not EC.exists()
    fanless()
    assert command("systemctl", "is-active", "sys-kernel-debug.mount", check=False) == "inactive"
    print("VM_ASSERT PASS fanless unit does not require a mounted debugfs", flush=True)
    # No hardware is passed through. This guest-only filesystem stands in for
    # debugfs so the real unit must allow EC files created after its setup.
    command("mount", "-t", "tmpfs", "-o", "mode=0700", "minibookx-test", "/sys/kernel/debug")
    start()
    main = int(props()["MainPID"])
    worker(main)
    assert EC.exists() and EC.read_bytes()[0x4F] & 0x40
    print("VM_ASSERT PASS EC leaf created and written after namespace startup", flush=True)
    print("VM_ASSERT PASS start, sandbox, logind, privilege drop, tablet ON", flush=True)
    command("systemctl", "restart", UNIT)
    wait_for(started, "restart failed")
    assert int(props()["MainPID"]) != main
    print("VM_ASSERT PASS restart", flush=True)
    command("systemctl", "stop", UNIT)
    stopped()
    print("VM_ASSERT PASS stop, ExecStopPost, fan auto, tablet OFF", flush=True)
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
    print("VM_ASSERT PASS failed recovery retains records and releases tablet", flush=True)
    # Turning fan control off cannot skip a prior attributed recovery failure.
    original = CONFIG.read_text()
    CONFIG.write_text(original.replace("fan_control = true", "fan_control = false", 1))
    command("systemctl", "start", UNIT, check=False)
    command("systemctl", "stop", UNIT, check=False)
    assert (ROOT / "manual.owner").exists()
    assert (ROOT / "module.lease").read_text().strip() == "fan=true"
    assert EC.read_bytes()[0x4F] & 0x40
    CONFIG.write_text(original)
    print("VM_ASSERT PASS disabled config cannot bypass pending fan recovery", flush=True)
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
    except Exception:
        traceback.print_exc()
        print(command("systemctl", "status", UNIT, "--no-pager", check=False), flush=True)
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
