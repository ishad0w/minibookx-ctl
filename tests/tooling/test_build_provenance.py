"""Exercise build.rs in isolated Git/source trees, without building hardware code."""

# SPDX-License-Identifier: Apache-2.0
import hashlib
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]


class BuildMetadataTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.tree = self.base / "tree"
        self.tree.mkdir()
        shutil.copy2(REPO / "build.rs", self.tree / "build.rs")
        licenses = self.tree / "packaging/licenses"
        licenses.mkdir(parents=True)
        (licenses / "fixture.txt").write_text("license fixture\n")
        output = self.base / "out"
        output.mkdir()
        self.env = dict(
            os.environ,
            CARGO_MANIFEST_DIR=str(self.tree),
            TARGET="x86_64-unknown-linux-gnu",
        )
        self.env["OUT_DIR"] = str(output)
        self.env.pop("MINIBOOKX_BUILD_COMMIT", None)
        self.env.pop("MINIBOOKX_BUILD_DIRTY", None)
        self.exe = self.base / "build-script"
        subprocess.run(
            [
                "rustc",
                "--edition=2024",
                str(self.tree / "build.rs"),
                "-o",
                str(self.exe),
            ],
            env=self.env,
            capture_output=True,
            check=True,
            timeout=30,
        )

    def git(self, *args):
        return subprocess.run(
            ["git", *args],
            cwd=self.tree,
            capture_output=True,
            text=True,
            check=True,
            timeout=15,
        ).stdout.strip()

    def initialize(self):
        self.git("init", "-q")
        self.git("add", "build.rs")
        self.git("add", "packaging")
        self.git(
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "fixture",
        )
        return self.git("rev-parse", "HEAD")

    def run_build(self, **values):
        return subprocess.run(
            [str(self.exe)],
            check=False,
            cwd=self.tree,
            env=dict(self.env, **values),
            capture_output=True,
            text=True,
            timeout=15,
        )

    def test_dirty_checkout_cannot_claim_clean_and_wrong_sha_is_rejected(self):
        commit = self.initialize()
        clean = self.run_build()
        self.assertEqual(clean.returncode, 0, clean.stderr)
        self.assertIn("MINIBOOKX_BUILD_COMMIT=" + commit, clean.stdout)
        self.assertNotEqual(
            self.run_build(MINIBOOKX_BUILD_COMMIT="a" * 40).returncode, 0
        )
        (self.tree / "unreviewed-file").write_text("fixture\n")
        self.assertIn("MINIBOOKX_BUILD_DIRTY_SUFFIX=.dirty", self.run_build().stdout)
        self.assertNotEqual(self.run_build(MINIBOOKX_BUILD_DIRTY="false").returncode, 0)

    def test_source_archive_uses_its_recorded_sha_inside_an_unrelated_checkout(self):
        # Git searches parents: an archive must not adopt the enclosing repo's SHA.
        subprocess.run(
            ["git", "init", "-q", str(self.base)], check=True, capture_output=True
        )
        commit = "c" * 40
        (self.tree / "BUILD-COMMIT").write_text(commit + "\n")
        result = self.run_build()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("MINIBOOKX_BUILD_COMMIT=" + commit, result.stdout)
        (self.tree / "BUILD-COMMIT").unlink()
        self.assertIn("MINIBOOKX_BUILD_COMMIT=unknown", self.run_build().stdout)

    def test_static_c_library_is_watched_and_its_actual_hash_is_embedded(self):
        prefix = self.tree / "target/native/libdbus-musl"
        library = prefix / "lib/libdbus-1.a"
        library.parent.mkdir(parents=True)
        library.write_bytes(b"first archive fixture")
        settings = {
            "TARGET": "x86_64-unknown-linux-musl",
            "DBUS_1_STATIC": "1",
            "PKG_CONFIG_LIBDIR": str(prefix / "lib/pkgconfig"),
        }
        first = self.run_build(**settings)
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertIn("rerun-if-changed=" + str(library), first.stdout)
        self.assertIn(
            "MINIBOOKX_LIBDBUS_SHA256="
            + hashlib.sha256(library.read_bytes()).hexdigest(),
            first.stdout,
        )
        library.write_bytes(b"changed archive fixture")
        second = self.run_build(**settings)
        self.assertIn(
            "MINIBOOKX_LIBDBUS_SHA256="
            + hashlib.sha256(library.read_bytes()).hexdigest(),
            second.stdout,
        )
        self.assertNotEqual(first.stdout, second.stdout)

    def test_embedded_license_manifest_is_sorted_and_tracks_compiler_notices(self):
        self.assertEqual(self.run_build().returncode, 0)
        manifest = (Path(self.env["OUT_DIR"]) / "license_manifest.rs").read_text()
        self.assertIn('("fixture.txt", include_bytes!(', manifest)
        self.assertIn(
            '("rust-runtime/COPYRIGHT-library.html", include_bytes!(', manifest
        )
        self.assertIn(
            '("rust-runtime/licenses/Apache-2.0.txt", include_bytes!(', manifest
        )
        entries = [
            line.strip().split(", include_bytes!")[0]
            for line in manifest.splitlines()[1:-1]
        ]
        self.assertEqual(entries, sorted(entries))
        (self.tree / "packaging/licenses/link").symlink_to(self.tree / "build.rs")
        self.assertNotEqual(self.run_build().returncode, 0)

    def test_missing_compiler_notices_explain_the_required_component(self):
        sysroot = self.base / "minimal-sysroot"
        sysroot.mkdir()
        compiler = self.base / "minimal-rustc"
        compiler.write_text(
            "#!/bin/sh\nprintf '%s\\n' " + shlex.quote(str(sysroot)) + "\n"
        )
        compiler.chmod(0o755)
        result = self.run_build(RUSTC=str(compiler))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("rustup component add rust-docs", result.stderr)
        self.assertFalse((Path(self.env["OUT_DIR"]) / "license_manifest.rs").exists())


class StaticBuildDriverTests(unittest.TestCase):
    def test_bundled_source_and_static_environment_reach_the_correct_tools(self):
        # Exercise the real shell driver with inert tools. No downloads, C/Rust
        # builds, installation or host compiler configuration are needed here.
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            scripts, tools = root / "scripts", root / "tools"
            scripts.mkdir()
            tools.mkdir()
            driver = scripts / "build-static.sh"
            shutil.copy2(REPO / "scripts/build-static.sh", driver)
            for name in ["libdbus_source.py", "release_format.py"]:
                (scripts / name).touch()
            archive = root / "third-party/sources/dbus-1.16.2.tar.xz"
            archive.parent.mkdir(parents=True)
            archive.write_bytes(b"bundled fixture, never compiled")
            log = root / "calls"
            tool = f"""#!{sys.executable}
import json, os, pathlib, sys
name = pathlib.Path(sys.argv[0]).name
if name == 'rustc':
    assert sys.argv[1:3] == ['--print', 'target-libdir']
    print(os.environ['TOOL_FIXTURE'])
else:
    with open(os.environ['TOOL_LOG'], 'a') as output:
        output.write(json.dumps([name, sys.argv[1:], {{k: os.environ.get(k) for k in
            ['PKG_CONFIG_LIBDIR', 'PKG_CONFIG_PATH', 'PKG_CONFIG_ALLOW_CROSS', 'DBUS_1_STATIC', 'RUSTFLAGS']}}]) + '\\n')
"""
            for name in ["python3", "cargo", "rustc", "fixture-musl-gcc"]:
                path = tools / name
                path.write_text(tool)
                path.chmod(0o755)
            env = dict(
                os.environ,
                PATH=str(tools) + ":" + os.environ["PATH"],
                TOOL_FIXTURE=str(root),
                TOOL_LOG=str(log),
                PKG_CONFIG_PATH="/unrelated-host-libraries",
                CC_x86_64_unknown_linux_musl="fixture-musl-gcc",
            )
            result = subprocess.run(
                ["/bin/sh", str(driver), "--test"],
                check=False,
                env=env,
                text=True,
                capture_output=True,
                timeout=5,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertEqual(
                [call[0] for call in calls], ["python3", "cargo", "python3", "cargo"]
            )
            self.assertEqual(
                calls[0][1][:2], [str(scripts / "libdbus_source.py"), "build"]
            )
            self.assertEqual(calls[0][1][-3], "--archive")
            self.assertEqual(Path(calls[0][1][-2]).resolve(), archive)
            self.assertEqual(calls[0][1][-1], "--offline")
            self.assertEqual(
                calls[2][1][:2], [str(scripts / "release_format.py"), "elf"]
            )
            for _, arguments, environment in [calls[1], calls[3]]:
                self.assertIn("--locked", arguments)
                self.assertIn("x86_64-unknown-linux-musl", arguments)
                self.assertEqual(environment["PKG_CONFIG_PATH"], "")
                self.assertEqual(
                    environment["PKG_CONFIG_LIBDIR"],
                    str(root / "target/native/libdbus-musl/lib/pkgconfig"),
                )
                self.assertEqual(environment["DBUS_1_STATIC"], "1")
                self.assertEqual(environment["PKG_CONFIG_ALLOW_CROSS"], "1")
                self.assertIn("target-feature=+crt-static", environment["RUSTFLAGS"])
            self.assertIn("--features", calls[3][1])
            self.assertIn("systemd-test-fixture", calls[3][1])

            log.unlink()
            result = subprocess.run(
                ["/bin/sh", str(driver), "--fixture"],
                check=False,
                env=env,
                text=True,
                capture_output=True,
                timeout=5,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertEqual(
                [call[0] for call in calls], ["python3", "cargo", "python3"]
            )
            self.assertIn("--features", calls[1][1])
            self.assertIn("systemd-test-fixture", calls[1][1])
            self.assertEqual(calls[1][1][-2:], ["--bin", "minibookx-systemd-fixture"])
            self.assertTrue(calls[2][1][-1].endswith("/minibookx-systemd-fixture"))


if __name__ == "__main__":
    unittest.main()
