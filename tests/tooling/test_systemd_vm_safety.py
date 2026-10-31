"""Guest identity guards and host-side VM launch isolation."""

import importlib.util
import io
import json
import resource
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch


class GuestGuard(unittest.TestCase):
    def test_import_and_wrong_identity_perform_no_service_commands(self):
        helper = Path(__file__).resolve().parents[2] / "tests/systemd/guest_tests.py"
        spec = importlib.util.spec_from_file_location("guest_tests", helper)
        module = importlib.util.module_from_spec(spec)
        with patch("subprocess.run") as command:
            spec.loader.exec_module(module)
            command.assert_not_called()
            with (
                patch.object(module.os, "geteuid", return_value=1000),
                self.assertRaisesRegex(SystemExit, "Refusing"),
            ):
                module.main()
            with (
                patch.object(module.os, "geteuid", return_value=0),
                patch.object(
                    module.Path,
                    "read_text",
                    side_effect=["systemd\n", "Unrelated machine\n"],
                ),
                self.assertRaisesRegex(SystemExit, "Refusing"),
            ):
                module.main()
            command.assert_not_called()

    def test_unexpected_guest_error_is_reported_before_poweroff(self):
        class UnexpectedGuestError(Exception):
            pass

        helper = Path(__file__).resolve().parents[2] / "tests/systemd/guest_tests.py"
        spec = importlib.util.spec_from_file_location("guest_failure", helper)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        output = io.StringIO()
        with (
            patch.object(module.os, "geteuid", return_value=0),
            patch.object(
                module.Path,
                "read_text",
                side_effect=["systemd\n", "MiniBookX Systemd Fixture\n"],
            ),
            patch.object(module, "run", side_effect=UnexpectedGuestError),
            patch.object(module, "command", return_value="diagnostics") as command,
            patch.object(module.traceback, "print_exc") as traceback,
            redirect_stdout(output),
        ):
            module.main()
        traceback.assert_called_once_with()
        self.assertIn("VM_RESULT FAIL", output.getvalue())
        self.assertNotIn("VM_RESULT PASS", output.getvalue())
        self.assertEqual(
            [call.args[0] for call in command.call_args_list],
            ["systemctl", "journalctl", "systemctl"],
        )
        self.assertEqual(command.call_args.args, ("systemctl", "poweroff"))
        self.assertEqual(command.call_args.kwargs, {"check": False})


class VmLaunch(unittest.TestCase):
    def test_memlock_is_child_only_and_exec_preserves_session_and_arguments(self):
        runner = Path(__file__).resolve().parents[2] / "tests/systemd/run_vm.py"
        spec = importlib.util.spec_from_file_location("run_vm", runner)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        original = resource.getrlimit(resource.RLIMIT_MEMLOCK)
        program = (
            "import json, os, resource, sys\n"
            "print(json.dumps([resource.getrlimit(resource.RLIMIT_MEMLOCK), "
            "os.getpid(), os.getpgrp(), os.getsid(0), sys.argv[1:]]))\n"
        )
        arguments = ["literal spaces", "$(no shell); *"]
        with tempfile.TemporaryFile() as log:
            process = module.start_vm(
                [sys.executable, "-I", "-S", "-c", program, *arguments], log
            )
            try:
                self.assertEqual(process.wait(timeout=5), 0)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()
            log.seek(0)
            limits, pid, group, session, received = json.load(log)
        self.assertEqual(limits, [0, 0])
        self.assertEqual((pid, group, session), (process.pid,) * 3)
        self.assertEqual(received, arguments)
        self.assertEqual(resource.getrlimit(resource.RLIMIT_MEMLOCK), original)


if __name__ == "__main__":
    unittest.main()
