"""Offline checks for executable advice and the bootstrap trust boundary."""

import subprocess
import tempfile
import unittest
from pathlib import Path

from check_docs import bootstrap_pin, source_links


class DocumentationLinks(unittest.TestCase):
    def test_source_advice_checks_paths_and_heading_anchors(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "src").mkdir()
            (root / "docs").mkdir()
            source = root / "src/report.rs"
            source.write_text('doc_url!("guide.md#setup")')
            (root / "docs/guide.md").write_text("# Setup\n")
            self.assertEqual(source_links(root), [])
            for link in ("guide.md#absent", "missing.md", "../README.md"):
                source.write_text(f'doc_url!("{link}")')
                self.assertEqual(len(source_links(root)), 1)

    def test_pin_must_be_ancestor_with_identical_reviewed_script(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)

            def git(*args):
                return (
                    subprocess.check_output(["git", *args], cwd=root).decode().strip()
                )

            git("init", "-q")
            git("config", "user.name", "Fixture")
            git("config", "user.email", "fixture@example.invalid")
            (root / "install.sh").write_text("#!/bin/sh\nexit 0\n")
            git("add", "install.sh")
            git("commit", "-qm", "reviewed bootstrap")
            pin = git("rev-parse", "HEAD")
            readme = root / "README.md"
            readme.write_text(
                f"https://raw.githubusercontent.com/ishad0w/minibookx-ctl/{pin}/install.sh"
            )
            self.assertEqual(bootstrap_pin(root), [])
            (root / "install.sh").write_text("changed\n")
            self.assertEqual(len(bootstrap_pin(root)), 1)
            readme.write_text(readme.read_text().replace(pin, "f" * 40))
            self.assertIn("ancestor", bootstrap_pin(root)[0])


if __name__ == "__main__":
    unittest.main()
