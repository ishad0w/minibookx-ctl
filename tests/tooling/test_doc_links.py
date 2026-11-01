"""Offline checks for executable advice and the bootstrap trust boundary."""

import tempfile
import unittest
from pathlib import Path

from check_docs import BOOTSTRAP, bootstrap_url, source_links


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

    def test_readme_runs_only_this_repositorys_main_bootstrap(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            readme = root / "README.md"
            readme.write_text(f"curl -fsSL {BOOTSTRAP} | sh\n")
            self.assertEqual(bootstrap_url(root), [])
            raw = "https://raw.githubusercontent.com/"
            for text in (
                "no command\n",
                f"{BOOTSTRAP}\n{BOOTSTRAP}\n",
                raw + "ishad0w/minibookx-ctl/" + "f" * 40 + "/install.sh\n",
                raw + "someone/minibookx-ctl/main/install.sh\n",
            ):
                readme.write_text(text)
                self.assertEqual(len(bootstrap_url(root)), 1, text)


if __name__ == "__main__":
    unittest.main()
