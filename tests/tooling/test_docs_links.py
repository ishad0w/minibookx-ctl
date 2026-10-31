"""Documentation checks must catch navigation failures without rewriting prose."""

import unittest

from check_docs import anchors


class HeadingLinks(unittest.TestCase):
    def test_code_examples_do_not_create_headings_and_duplicates_are_addressable(self):
        document = "# Install\n```sh\n# not a heading\n```\n## Install\n~~~text\n# neither\n~~~\n"
        self.assertEqual(anchors(document), {"install", "install-1"})

    def test_markup_and_unicode_match_the_heading_link(self):
        document = "# `doctor` / Debug (read-only)\n## [Café](guide.md) & PL1/PL2\n"
        self.assertIn("doctor--debug-read-only", anchors(document))
        self.assertIn("café--pl1pl2", anchors(document))


if __name__ == "__main__":
    unittest.main()
