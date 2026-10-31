"""An unsupported panel must fail before the offline tool creates output."""

# SPDX-License-Identifier: Apache-2.0
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

BUILDER = Path(__file__).resolve().parents[2] / "contrib/vbt/build_vbt.py"


class UnknownPanel(unittest.TestCase):
    def test_correct_size_does_not_replace_complete_firmware_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "unknown.vbt"
            source.write_bytes(b"\0" * 10752)
            destination = root / "generated"
            result = subprocess.run(
                [
                    sys.executable,
                    str(BUILDER),
                    "--input",
                    str(source),
                    "--output",
                    str(destination),
                ],
                capture_output=True,
                text=True,
                check=False,
                timeout=5,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("refusing another panel/revision", result.stderr)
            self.assertFalse(destination.exists())
            self.assertEqual(source.read_bytes(), b"\0" * 10752)


if __name__ == "__main__":
    unittest.main()
