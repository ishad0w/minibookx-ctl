"""A rejected guest helper must never reach host service or shutdown commands."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch


class GuestGuard(unittest.TestCase):
    def test_import_and_wrong_identity_perform_no_service_commands(self):
        helper = Path(__file__).resolve().parents[2] / 'tests/systemd/guest_tests.py'
        spec = importlib.util.spec_from_file_location('guest_tests', helper)
        module = importlib.util.module_from_spec(spec)
        with patch('subprocess.run') as command:
            spec.loader.exec_module(module)
            command.assert_not_called()
            with patch.object(module.os, 'geteuid', return_value=1000):
                with self.assertRaisesRegex(SystemExit, 'Refusing'):
                    module.main()
            with patch.object(module.os, 'geteuid', return_value=0):
                with patch.object(module.Path, 'read_text', side_effect=['systemd\n', 'Unrelated machine\n']):
                    with self.assertRaisesRegex(SystemExit, 'Refusing'):
                        module.main()
            command.assert_not_called()


if __name__ == '__main__':
    unittest.main()
