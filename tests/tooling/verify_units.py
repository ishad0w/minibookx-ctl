"""Check unit syntax in an isolated fixture root; does not start any service."""

# SPDX-License-Identifier: Apache-2.0
import shutil
import subprocess
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]

main_unit = (REPO / "packaging/minibookx-ctl.service").read_text()
assert "WatchdogSignal=SIGTERM" in main_unit, "watchdog must allow guardian OFF cleanup"
assert "KillMode=control-group" in main_unit
assert "TimeoutAbortSec=3s" in main_unit
assert "NotifyAccess=main" in main_unit.splitlines()
assert not any(
    line.startswith(("User=", "Group=", "AmbientCapabilities="))
    for line in main_unit.splitlines()
)
assert "DevicePolicy=closed" in main_unit.splitlines()
assert "ExecStopPost=/usr/local/bin/minibookx-ctl --recover" in main_unit.splitlines()
assert "OnFailure=" not in main_unit
assert not (REPO / "packaging/minibookx-ctl-recover.service").exists()
assert "--allow-power-limits" not in main_unit
assert "Requires=systemd-logind.service" in main_unit.splitlines()
assert not any(
    line.startswith("Requires=") and "sys-kernel-debug.mount" in line
    for line in main_unit.splitlines()
)
assert (
    "BindPaths=-/sys/kernel/debug:/run/minibookx-ctl/hardware/debugfs"
    in main_unit.splitlines()
)
assert "ReadWritePaths=-/sys/kernel/debug" not in main_unit.splitlines()
for index in range(3):
    assert (
        f"ReadWritePaths=-/sys/class/powercap/intel-rapl:0/constraint_{index}_power_limit_uw"
        in main_unit.splitlines()
    )

with tempfile.TemporaryDirectory(prefix="minibookx-unit-check-") as temp:
    root = Path(temp)
    units = root / "etc/systemd/system"
    units.mkdir(parents=True)
    executable = root / "usr/local/bin/minibookx-ctl"
    executable.parent.mkdir(parents=True)
    # The analyzer checks executability but does not invoke commands.
    shutil.copy2("/usr/bin/true", executable)
    for name in (
        "sysinit",
        "basic",
        "shutdown",
        "sockets",
        "umount",
        "local-fs",
        "local-fs-pre",
    ):
        (units / (name + ".target")).write_text(
            "[Unit]\nDescription=Offline verification fixture\nDefaultDependencies=no\n"
        )
    (units / "systemd-logind.service").write_text(
        "[Unit]\nDescription=Offline logind fixture\nDefaultDependencies=no\n"
        "[Service]\nExecStart=/usr/local/bin/minibookx-ctl\n"
    )
    (units / "modprobe@.service").write_text(
        "[Unit]\nDescription=Offline module fixture\nDefaultDependencies=no\n"
        "[Service]\nType=oneshot\nExecStart=/usr/local/bin/minibookx-ctl\n"
    )
    (units / "sys-kernel-debug.mount").write_text(
        "[Unit]\nDescription=Offline debugfs fixture\nDefaultDependencies=no\n"
        "[Mount]\nWhat=debugfs\nWhere=/sys/kernel/debug\nType=debugfs\n"
    )
    names = ["minibookx-ctl.service"]
    for name in names:
        shutil.copy2(REPO / "packaging" / name, units / name)
    command = [
        "systemd-analyze",
        "--root",
        temp,
        "--generators=no",
        "--man=no",
        "verify",
    ]
    subprocess.run(
        command + ["/etc/systemd/system/" + name for name in names], check=True
    )
print(
    "Unit syntax verification passed; no accounts, services or hardware were changed."
)
