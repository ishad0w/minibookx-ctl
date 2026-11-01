"""Release artifacts are checked as data without executing their contents."""

# SPDX-License-Identifier: Apache-2.0
import gzip
import hashlib
import io
import json
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import release_format as release


def elf():
    data = bytearray(256)
    struct.pack_into(
        "<16sHHIQQQIHHHHHH",
        data,
        0,
        b"\x7fELF\x02\x01\x01" + bytes(9),
        2,
        62,
        1,
        0x400000,
        64,
        0,
        0,
        64,
        56,
        2,
        64,
        0,
        0,
    )
    struct.pack_into(
        "<IIQQQQQQ", data, 64, 1, 5, 0, 0x400000, 0, len(data), len(data), 4096
    )
    struct.pack_into("<IIQQQQQQ", data, 120, 0x6474E551, 6, 0, 0, 0, 0, 0, 16)
    return bytes(data)


def archive(files, top, extra=None):
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w:gz") as stream:
        for name, data in files.items():
            member = tarfile.TarInfo(top + "/" + name)
            member.size = len(data)
            member.mode = 0o644
            stream.addfile(member, io.BytesIO(data))
        if extra is not None:
            stream.addfile(extra)
    return raw.getvalue()


class ReleaseFormatTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.folder = Path(self.temp.name)
        self.commit = "a" * 40
        self.tag = "v0.1.1"
        self.top = "minibookx-ctl-0.1.1-" + self.commit[:12] + "-" + release.TARGET
        self.meta = {
            "schema_version": 1,
            "version": "0.1.1",
            "build": self.commit[:12],
            "commit": self.commit,
            "tag": self.tag,
            "target": release.TARGET,
            "binary_version": "0.1.1+g" + self.commit[:12],
            "binary_archive": self.top + ".tar.gz",
            "source_archive": "minibookx-ctl-0.1.1-"
            + self.commit[:12]
            + "-sources.tar.gz",
            "binary_sha256": hashlib.sha256(elf()).hexdigest(),
            "rustc": "rustc fixture",
            "libdbus": {
                "source_sha256": hashlib.sha256(b"pinned dbus source").hexdigest()
            },
        }
        self.files = {
            release.NAME: elf(),
            "build-metadata.json": json.dumps(self.meta).encode(),
            "config/presets.conf": b"preset=balance\n",
            "LICENSE": b"Apache-2.0",
            "NOTICE": b"Notice",
            "THIRD_PARTY.md": b"Notices",
            "packaging/licenses/test.txt": b"License",
            "packaging/minibookx-ctl.service": b"Unit fixture",
        }
        (self.folder / "build-metadata.json").write_bytes(
            self.files["build-metadata.json"]
        )
        (self.folder / release.FLAT_BINARY).write_bytes(elf())
        dbus = b"pinned dbus source"
        self.sources = {
            "BUILD-COMMIT": (self.commit + "\n").encode(),
            "build-metadata.json": self.files["build-metadata.json"],
            "Cargo.toml": b"[package]",
            "Cargo.lock": b"version = 4",
            ".cargo/config.toml": b"[source]",
            "vendor/libc/Cargo.toml": b"[package]",
            "third-party/libdbus.json": json.dumps(
                {"archive": "dbus.tar.xz", "sha256": hashlib.sha256(dbus).hexdigest()}
            ).encode(),
            "third-party/sources/dbus.tar.xz": dbus,
        }
        self.save_sources()
        self.save_archive()

    def save_sources(self):
        top = self.meta["source_archive"].removesuffix(".tar.gz")
        (self.folder / self.meta["source_archive"]).write_bytes(
            archive(self.sources, top)
        )
        self.checksums()

    def save_archive(self):
        (self.folder / self.meta["binary_archive"]).write_bytes(
            archive(self.files, self.top)
        )
        self.checksums()

    def checksums(self):
        (self.folder / "SHA256SUMS").write_text(
            "".join(
                hashlib.sha256(p.read_bytes()).hexdigest() + "  " + p.name + "\n"
                for p in sorted(self.folder.iterdir())
                if p.name != "SHA256SUMS"
            )
        )

    def test_flat_asset_and_archive_must_describe_same_identified_build(self):
        metadata, names = release.checked_assets(self.folder, self.commit)
        self.assertEqual(metadata, self.meta)
        self.assertEqual(len(names), 5)
        self.assertIn(release.FLAT_BINARY, names)
        with self.assertRaisesRegex(ValueError, "workflow commit"):
            release.checked_assets(self.folder, "b" * 40)

    def test_semantic_tag_keeps_commit_identity_in_binary_and_archive_metadata(self):
        metadata, names = release.checked_assets(self.folder, self.commit)
        self.assertEqual(len(names), 5)
        self.assertEqual(metadata["tag"], "v0.1.1")
        self.assertEqual(metadata["commit"], self.commit)
        self.assertEqual(metadata["binary_version"], "0.1.1+g" + self.commit[:12])
        self.assertIn(self.commit[:12], metadata["binary_archive"])
        self.assertIn(self.commit[:12], metadata["source_archive"])
        for key, value in [
            ("commit", "b" * 40),
            ("build", "b" * 12),
            ("version", "0.2.0"),
            ("binary_version", "0.1.1"),
        ]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                release.validate_metadata(
                    json.dumps(self.meta | {key: value}).encode(), self.tag
                )

    def test_version_mismatches_and_noncanonical_tags_are_rejected(self):
        raw = self.files["build-metadata.json"]
        for tag in [
            "v0.1.2",
            "v0.2.0",
            "v1.0.0",
            "v0.1.1/extra",
            "v0.1.1?extra",
            "v0.1.1-aaaaaaaaaaaa",
            "v0.1.1-rc1",
            "0.1.1",
            "v00.1.1",
            None,
        ]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.validate_metadata(raw, tag)

    def test_plain_semantic_tags_apply_to_later_versions_without_history_exceptions(
        self,
    ):
        for version in ["0.1.2", "0.2.0", "1.0.0"]:
            prefix = release.NAME + "-" + version + "-" + self.commit[:12]
            metadata = self.meta | {
                "version": version,
                "tag": "v" + version,
                "binary_version": version + "+g" + self.commit[:12],
                "binary_archive": prefix + "-" + release.TARGET + ".tar.gz",
                "source_archive": prefix + "-sources.tar.gz",
            }
            with self.subTest(version=version):
                self.assertEqual(
                    release.validate_metadata(
                        json.dumps(metadata).encode(), metadata["tag"]
                    ),
                    metadata,
                )

    def test_rehashed_replaced_flat_binary_still_refuses_metadata_disagreement(self):
        replacement = bytearray(elf())
        replacement[-1] = 1
        (self.folder / release.FLAT_BINARY).write_bytes(replacement)
        self.checksums()
        with self.assertRaisesRegex(ValueError, "build metadata"):
            release.checked_assets(self.folder, self.commit)

    def test_archive_replacement_cannot_use_different_embedded_binary_or_metadata(self):
        for path in (release.NAME, "build-metadata.json"):
            original = self.files[path]
            self.files[path] += b"changed"
            self.save_archive()
            with (
                self.subTest(path=path),
                self.assertRaisesRegex(ValueError, "archive binary|Archive binary"),
            ):
                release.checked_assets(self.folder, self.commit)
            self.files[path] = original

    def test_download_checksum_corruption_extra_files_and_symlinks_fail(self):
        (self.folder / release.FLAT_BINARY).write_bytes(b"changed")
        with self.assertRaisesRegex(ValueError, "checksum"):
            release.checked_assets(self.folder, self.commit)
        (self.folder / release.FLAT_BINARY).write_bytes(elf())
        (self.folder / "extra").write_bytes(b"not published")
        with self.assertRaisesRegex(ValueError, "unexpected/missing"):
            release.checked_assets(self.folder, self.commit)
        (self.folder / "extra").unlink()
        source = self.folder / release.FLAT_BINARY
        target = self.folder.parent / (self.folder.name + "-fixture")
        source.rename(target)
        self.addCleanup(lambda: target.unlink(missing_ok=True))
        source.symlink_to(target)
        with self.assertRaises(OSError):
            release.checked_assets(self.folder, self.commit)

    def test_source_archive_must_identify_the_build_and_carry_its_inputs(self):
        release.checked_assets(self.folder, self.commit)
        for name, value, message in [
            ("BUILD-COMMIT", b"b" * 40 + b"\n", "published commit"),
            ("build-metadata.json", b"{}", "metadata differs"),
            ("Cargo.lock", None, "vendored Rust"),
            ("vendor/libc/Cargo.toml", None, "vendored Rust"),
            ("third-party/sources/dbus.tar.xz", b"other source", "libdbus"),
        ]:
            original = dict(self.sources)
            if value is None:
                del self.sources[name]
            else:
                self.sources[name] = value
            self.save_sources()
            with self.assertRaisesRegex(ValueError, message):
                release.checked_assets(self.folder, self.commit)
            self.sources = original
        (self.folder / self.meta["source_archive"]).write_bytes(b"not a tarball")
        self.checksums()
        with self.assertRaisesRegex(ValueError, "compressed release archive"):
            release.checked_assets(self.folder, self.commit)

    def test_removed_auxiliary_service_and_missing_notices_refuse_publication(self):
        self.files["packaging/minibookx-ctl-recover.service"] = b"old unit"
        self.save_archive()
        with self.assertRaisesRegex(ValueError, "auxiliary service"):
            release.checked_assets(self.folder, self.commit)
        del self.files["packaging/minibookx-ctl-recover.service"]
        del self.files["NOTICE"]
        self.save_archive()
        with self.assertRaisesRegex(ValueError, "license files"):
            release.checked_assets(self.folder, self.commit)

    def test_metadata_rejects_duplicate_keys_wrong_target_and_boolean_schema(self):
        raw = self.files["build-metadata.json"]
        with self.assertRaisesRegex(ValueError, "duplicate JSON"):
            release.validate_metadata(raw[:-1] + b', "tag": "duplicate"}', self.tag)
        for key, value in (
            ("schema_version", True),
            ("commit", "b" * 40),
            ("target", "wrong"),
        ):
            with self.subTest(key=key), self.assertRaises(ValueError):
                release.validate_metadata(
                    json.dumps(self.meta | {key: value}).encode(), self.tag
                )

    def test_archive_rejects_traversal_links_duplicates_modes_and_expansion_bombs(self):
        for path, kind, mode in (
            ("../outside", tarfile.REGTYPE, 0o644),
            ("link", tarfile.SYMTYPE, 0o644),
            ("minibookx-ctl", tarfile.REGTYPE, 0o644),
            ("world-writable", tarfile.REGTYPE, 0o666),
        ):
            member = tarfile.TarInfo(self.top + "/" + path)
            member.type = kind
            member.mode = mode
            member.linkname = "target"
            with self.subTest(path=path), self.assertRaises(ValueError):
                release.unpack_archive(archive(self.files, self.top, member), self.top)
        with (
            mock.patch.object(release, "MAX_EXPANDED", 128),
            self.assertRaisesRegex(ValueError, "expanded"),
        ):
            release.unpack_archive(gzip.compress(bytes(129)), self.top)

    def test_elf_rejects_dynamic_interpreter_executable_stack_and_missing_headers(self):
        release.verify_binary(elf())
        for kind, flags in ((3, 4), (0x6474E551, 7), (0, 0)):
            changed = bytearray(elf())
            struct.pack_into("<II", changed, 120, kind, flags)
            with self.subTest(kind=kind, flags=flags), self.assertRaises(ValueError):
                release.verify_binary(changed)
        dependent = bytearray(elf()) + bytearray(256)
        struct.pack_into("<H", dependent, 56, 3)
        struct.pack_into("<IIQQQQQQ", dependent, 176, 2, 6, 256, 0, 0, 32, 32, 8)
        struct.pack_into("<qQqQ", dependent, 256, 1, 10, 0, 0)
        with self.assertRaisesRegex(ValueError, "shared-library dependencies"):
            release.verify_binary(dependent)

    def test_cli_uses_the_same_read_only_checks_and_fails_closed(self):
        def command(*arguments):
            return subprocess.run(
                [sys.executable, release.__file__, *map(str, arguments)],
                check=False,
                text=True,
                capture_output=True,
                timeout=5,
            )

        # These synthetic assets are deliberately not executable. Validation
        # must read their bytes rather than invoke downloaded release code.
        binary = self.folder / release.FLAT_BINARY
        self.assertEqual(command("elf", binary).returncode, 0)
        accepted = command("check", self.folder, "--commit", self.commit)
        self.assertEqual(accepted.returncode, 0, accepted.stderr)
        self.assertIn("5 assets", accepted.stdout)
        through_make = subprocess.run(
            [
                "make",
                "--no-print-directory",
                "verify-release",
                f"RELEASE_DIR={self.folder}",
                f"COMMIT={self.commit}",
            ],
            check=False,
            cwd=Path(release.__file__).resolve().parent.parent,
            text=True,
            capture_output=True,
            timeout=5,
        )
        self.assertEqual(through_make.returncode, 0, through_make.stderr)
        self.assertIn("5 assets", through_make.stdout)
        rejected = command("check", self.folder, "--commit", "b" * 40)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("workflow commit", rejected.stderr)
        changed = bytearray(elf())
        struct.pack_into("<I", changed, 120, 3)
        binary.write_bytes(changed)
        rejected = command("elf", binary)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("dynamic interpreter", rejected.stderr)
        binary.unlink()
        binary.symlink_to(self.folder / "build-metadata.json")
        self.assertNotEqual(command("elf", binary).returncode, 0)


if __name__ == "__main__":
    unittest.main()
