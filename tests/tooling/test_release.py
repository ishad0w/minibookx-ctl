#!/usr/bin/env python3
"""Release trust-boundary fixtures. No network, credentials or real publishing."""
# SPDX-License-Identifier: Apache-2.0
from pathlib import Path
import os
import tarfile
import tempfile
import unittest
from unittest import mock
from types import SimpleNamespace

import package_release as package
import publish_release as publish


class ProvenanceTests(unittest.TestCase):
    def test_source_copy_keeps_the_developer_commands_and_relocated_tests(self):
        with tempfile.TemporaryDirectory() as temp:
            destination = Path(temp)
            for directory in ('scripts', 'tests'):
                self.assertIn(directory, package.SOURCE_DIRS)
                package.copy_public_tree(package.REPO / directory, destination / directory)
            required = [
                'scripts/build-static.sh', 'scripts/libdbus_source.py',
                'scripts/package_release.py', 'scripts/publish_release.py',
                'scripts/release_format.py', 'tests/containers/run.sh',
                'tests/tooling/test_bootstrap.py', 'tests/tooling/test_vbt_identity.py',
                'tests/tooling/check_docs.py', 'tests/tooling/verify_units.py',
            ]
            for name in required:
                self.assertEqual((destination / name).read_bytes(), (package.REPO / name).read_bytes())
            self.assertFalse(list(destination.rglob('__pycache__')))

    def test_reject_stale_dirty_or_wrong_target_binary(self):
        commit = "b" * 40
        info = {"version": "0.1.1", "binary_version": "0.1.1+g" + commit[:12],
                "commit": commit, "target": package.TARGET, "dirty": False, "rustc": "rustc fixture"}
        self.assertEqual(package.checked_identity(commit, "0.1.1", info, "rustc fixture"), commit[:12])
        for key, bad in (("commit", "a" * 40), ("dirty", True), ("dirty", 0),
                         ("target", "x86_64-unknown-linux-gnu"), ("version", "0.4.0"),
                         ("rustc", "rustc older toolchain"), ("binary_version", "0.1.1")):
            changed = dict(info, **{key: bad})
            with self.subTest(key=key), self.assertRaises(ValueError):
                package.checked_identity(commit, "0.1.1", changed, "rustc fixture")

    def test_public_tree_refuses_links_raw_captures_and_special_files(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, dest = root / "source", root / "dest"
            source.mkdir()
            (source / "ordinary.md").write_text("Documentation\n")
            package.copy_public_tree(source, dest)
            for name in ("capture.bin", "private.log"):
                bad = source / name
                bad.write_bytes(b"private capture fixture")
                with self.assertRaises(ValueError):
                    package.copy_public_tree(source, dest)
                bad.unlink()
            (source / "link.md").symlink_to(source / "ordinary.md")
            with self.assertRaises(ValueError):
                package.copy_public_tree(source, dest)

    def test_archive_normalizes_unsafe_modes_and_is_repeatable(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "source"
            source.mkdir(mode=0o777)
            (source / "tool").write_text("fixture\n")
            (source / "tool").chmod(0o777)
            first, second = root / "first.gz", root / "second.gz"
            package.archive(first, source, 1234)
            package.archive(second, source, 1234)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            with tarfile.open(first) as archive:
                self.assertTrue(all(m.mode == 0o755 and m.uid == m.gid == 0 for m in archive))
            with self.assertRaises(FileExistsError):
                package.archive(first, source, 1234)


class PublicationTests(unittest.TestCase):
    def setUp(self):
        self.commit = "a" * 40
        self.tag = "v0.1.1"
        self.metadata = {"tag": self.tag, "binary_version": "0.1.1+g" + self.commit[:12]}
        self.names = ["SHA256SUMS", "build-metadata.json", "binary.tar.gz", "sources.tar.gz"]
        self.calls = []

    def test_only_explicit_dispatch_for_this_main_commit_can_publish(self):
        allowed = {
            "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_REF": "refs/heads/main",
            "GITHUB_REPOSITORY": publish.REPO,
            "GITHUB_SHA": self.commit,
            "MINIBOOKX_PUBLISH": "true",
        }
        args = ['publish_release.py', '--commit', self.commit]
        with mock.patch('sys.argv', args), mock.patch.object(publish, 'publish') as publish_build:
            for key, value in (
                ('GITHUB_EVENT_NAME', 'push'), ('GITHUB_EVENT_NAME', 'pull_request'),
                ('GITHUB_REF', 'refs/heads/other'), ('GITHUB_REPOSITORY', 'other/repo'),
                ('GITHUB_SHA', 'b' * 40), ('MINIBOOKX_PUBLISH', ''),
                ('MINIBOOKX_PUBLISH', 'false'),
            ):
                with self.subTest(key=key, value=value), \
                     mock.patch.dict(os.environ, dict(allowed, **{key: value}), clear=True), \
                     self.assertRaisesRegex(ValueError, 'explicit workflow dispatch'):
                    publish.main()
                publish_build.assert_not_called()
            with mock.patch.dict(os.environ, allowed, clear=True):
                publish.main()
            publish_build.assert_called_once_with(Path('dist').resolve(), self.commit)

    def test_repository_request_has_no_trailing_slash_that_github_rejects(self):
        result = SimpleNamespace(returncode=0, stdout='{"default_branch":"main"}', stderr="")
        with mock.patch.object(publish.subprocess, "run", return_value=result) as run:
            self.assertEqual(publish.api("")["default_branch"], "main")
        self.assertEqual(run.call_args.args[0], ["gh", "api", "repos/" + publish.REPO])

    def run_publish(self, head=None, tag=None, release=None):
        reads = 0
        def api(endpoint, data=None, missing=False):
            nonlocal reads
            self.calls.append((endpoint, data))
            if endpoint == "": return {"default_branch": "main"}
            if endpoint.startswith("git/ref/tags/"): return tag
            if endpoint == "git/refs": return {}
            if endpoint == "git/ref/heads/main": return {"object": {"sha": head or self.commit}}
            raise AssertionError(endpoint)
        def release_info(_tag, missing=False):
            nonlocal reads
            reads += 1
            return release if release is not None else (None if reads == 1 else {"draft": True})
        with mock.patch.object(publish, "checked_assets", return_value=(self.metadata, self.names)), \
             mock.patch.object(publish, "api", side_effect=api), \
             mock.patch.object(publish, "release_info", side_effect=release_info), \
             mock.patch.object(publish, "verify_existing_release") as verify, \
             mock.patch.object(publish, "upload_missing_assets"), \
             mock.patch.object(publish, "gh") as gh, \
             mock.patch("builtins.print"):
            publish.publish(Path("/fixture"), self.commit)
            return gh.call_args_list, verify.call_count

    def test_new_commit_cannot_reuse_a_published_semantic_version(self):
        with self.assertRaisesRegex(ValueError, "never repoint"):
            self.run_publish(tag={"object": {"type": "commit", "sha": "b" * 40}})

    def test_existing_published_release_only_verifies(self):
        calls, checked = self.run_publish(
            tag={"object": {"type": "commit", "sha": self.commit}},
            release={"draft": False, "prerelease": False, "html_url": "https://example.invalid/release"})
        self.assertEqual(calls, [])
        self.assertEqual(checked, 1)

    def test_old_main_build_cannot_replace_latest(self):
        calls, _ = self.run_publish(head="b" * 40)
        self.assertIn("--latest=false", calls[-1].args)
        self.assertIn("--draft=false", calls[-1].args)

    def test_current_main_build_publishes_after_draft_asset_check(self):
        calls, checked = self.run_publish()
        self.assertEqual(checked, 1)
        self.assertIn("--latest=true", calls[-1].args)
        self.assertTrue(any(c[0] == "git/refs" and c[1]["sha"] == self.commit for c in self.calls))

    def test_semantic_tag_and_release_title_do_not_include_the_build_hash(self):
        calls, checked = self.run_publish()
        self.assertEqual(checked, 1)
        create = next(call.args for call in calls if call.args[:2] == ('release', 'create'))
        self.assertEqual(create[2], 'v0.1.1')
        self.assertEqual(create[create.index('--title') + 1], 'Release (v0.1.1)')
        self.assertTrue(any(c[0] == 'git/refs' and c[1]['ref'] == 'refs/tags/v0.1.1'
                            and c[1]['sha'] == self.commit for c in self.calls))

    def test_annotated_tag_is_not_silently_replaced(self):
        with self.assertRaisesRegex(ValueError, 'never repoint'):
            self.run_publish(tag={'object': {'type': 'tag', 'sha': self.commit}})
        self.assertFalse(any(call[0] == 'git/refs' for call in self.calls))


class DraftLookupTests(unittest.TestCase):
    def test_draft_lookup_uses_numeric_id_instead_of_published_tag_endpoint(self):
        result = SimpleNamespace(returncode=0, stderr="", stdout='{"apiUrl":"https://api.github.com/repos/' + publish.REPO + '/releases/123"}')
        with mock.patch.object(publish.subprocess, "run", return_value=result), \
             mock.patch.object(publish, "api", return_value={"tag_name": "fixture", "draft": True}) as api:
            self.assertTrue(publish.release_info("fixture")["draft"])
        api.assert_called_once_with("releases/123")

    def test_missing_release_is_distinct_from_api_failure_or_foreign_url(self):
        missing = SimpleNamespace(returncode=1, stderr="release not found\n", stdout="")
        with mock.patch.object(publish.subprocess, "run", return_value=missing):
            self.assertIsNone(publish.release_info("fixture", missing=True))
            with self.assertRaises(RuntimeError):
                publish.release_info("fixture")
        denied = SimpleNamespace(returncode=1, stderr="HTTP 403", stdout="")
        with mock.patch.object(publish.subprocess, "run", return_value=denied), self.assertRaises(RuntimeError):
            publish.release_info("fixture", missing=True)
        foreign = SimpleNamespace(returncode=0, stderr="", stdout='{"apiUrl":"https://example.invalid/releases/123"}')
        with mock.patch.object(publish.subprocess, "run", return_value=foreign), self.assertRaises(ValueError):
            publish.release_info("fixture")


class DraftResumeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.folder = Path(self.temp.name)
        self.names = ["SHA256SUMS", "build-metadata.json"]
        for name in self.names:
            (self.folder / name).write_bytes((name + " fixture").encode())
        self.release = {"draft": True, "tag_name": "v0.1.1",
                        "assets": [{"name": "SHA256SUMS"}]}
        self.calls = []
        self.mismatch = False

    def gh(self, *args):
        self.calls.append(args)
        if args[:2] == ("release", "download"):
            destination = Path(args[args.index("--dir") + 1]) / "SHA256SUMS"
            destination.write_bytes(b"wrong" if self.mismatch else (self.folder / "SHA256SUMS").read_bytes())

    def test_partial_draft_resumes_by_comparing_then_uploading_only_missing(self):
        with mock.patch.object(publish, "gh", side_effect=self.gh):
            publish.upload_missing_assets(self.release, self.folder, self.names)
        self.assertEqual(self.calls[-1][-1], str(self.folder / "build-metadata.json"))
        self.assertNotIn(str(self.folder / "SHA256SUMS"), self.calls[-1])
        self.assertFalse(any("--clobber" in call for call in self.calls))

    def test_mismatched_draft_asset_is_never_replaced(self):
        self.mismatch = True
        with mock.patch.object(publish, "gh", side_effect=self.gh), self.assertRaisesRegex(ValueError, "refusing to overwrite"):
            publish.upload_missing_assets(self.release, self.folder, self.names)
        self.assertEqual(len(self.calls), 1)

    def test_concurrent_publication_or_upload_can_only_cause_a_safe_conflict(self):
        def conflict(*args):
            self.gh(*args)
            if args[:2] == ("release", "upload"):
                raise RuntimeError("fixture: release became public and asset appeared")
        with mock.patch.object(publish, "gh", side_effect=conflict), self.assertRaises(RuntimeError):
            publish.upload_missing_assets(self.release, self.folder, self.names)
        self.assertFalse(any("--clobber" in call for call in self.calls))
        self.release["draft"] = False
        with mock.patch.object(publish, "gh") as gh, self.assertRaises(ValueError):
            publish.upload_missing_assets(self.release, self.folder, self.names)
        gh.assert_not_called()


if __name__ == "__main__":
    unittest.main()
