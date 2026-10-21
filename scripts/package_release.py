#!/usr/bin/env python3
"""Package a clean, identified static build and its exact corresponding sources."""
# SPDX-License-Identifier: Apache-2.0
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib

from libdbus_source import ensure_archive, metadata as dbus_metadata
from release_format import FLAT_BINARY, MAX_FILE, read_asset, verify_binary

REPO = Path(__file__).resolve().parent.parent
TARGET = "x86_64-unknown-linux-musl"
TOP_FILES = [
    "Cargo.toml", "Cargo.lock", "build.rs", ".gitignore", ".gitattributes",
    ".editorconfig", "AGENTS.md", "MAP.md", "SECURITY.md", "Makefile", "README.md", "LICENSE",
    "NOTICE", "THIRD_PARTY.md", "install.sh", "rust-toolchain.toml",
    "rustfmt.toml", "clippy.toml",
]
SOURCE_DIRS = ["src", "config", "packaging", "scripts", "docs", "contrib", "tests", ".github", "third-party"]
DENY_SUFFIXES = {".bin", ".cpio", ".rom", ".fd", ".aml", ".log", ".pyc"}


def run(args, **kwargs):
    return subprocess.run(args, cwd=REPO, check=True, text=True, **kwargs)


def output(args):
    return run(args, stdout=subprocess.PIPE).stdout.strip()


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def json_bytes(value):
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()


def checked_identity(commit, version, info, rustc):
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("release needs a full lowercase Git commit")
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
        raise ValueError("release needs a numeric major.minor.patch base version")
    expected = {"version": version, "binary_version": version + "+g" + commit[:12],
                "commit": commit, "target": TARGET, "dirty": False, "rustc": rustc}
    if not isinstance(info, dict) or type(info.get("dirty")) is not bool or any(info.get(k) != v for k, v in expected.items()):
        raise ValueError("binary provenance differs from clean source HEAD/version/target")
    return commit[:12]


def copy_public_tree(source, dest):
    for item in source.rglob("*"):
        relative = item.relative_to(source)
        if "__pycache__" in relative.parts:
            continue
        if item.is_symlink():
            raise ValueError(f"release source must not contain a symlink: {relative}")
        if item.is_dir():
            continue
        if not item.is_file():
            raise ValueError(f"release source contains a special file: {relative}")
        if (item.suffix.lower() in DENY_SUFFIXES or "evidence" in relative.parts or
                "firmware" in relative.parts and item.suffix != ".md"):
            raise ValueError(f"remove private/raw artifacts before packaging: {relative}")
        target = dest / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(item, target)


def archive(path, source, epoch):
    def normalized(info):
        if not (info.isfile() or info.isdir()):
            raise ValueError("release archives cannot contain links or special files")
        info.uid = info.gid = 0
        info.uname = info.gname = "root"
        info.mtime = epoch
        info.mode = 0o755 if info.isdir() or info.mode & 0o111 else 0o644
        return info
    with open(path, "xb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT) as stream:
                stream.add(source, arcname=source.name, filter=normalized)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=REPO / f"target/{TARGET}/release/minibookx-ctl")
    parser.add_argument("--output", type=Path, default=REPO / "dist")
    args = parser.parse_args()
    if os.geteuid() == 0:
        raise ValueError("release packaging and dependency tools must run without root")
    if Path(output(["git", "rev-parse", "--show-toplevel"])).resolve() != REPO:
        raise ValueError("package from the project's own clean Git checkout")
    if output(["git", "status", "--porcelain", "--untracked-files=all"]):
        raise ValueError("commit all reviewed changes before packaging a release")
    commit = output(["git", "rev-parse", "HEAD"])
    version = tomllib.loads((REPO / "Cargo.toml").read_text())["package"]["version"]
    binary_path = args.binary.resolve()
    verify_binary(read_asset(binary_path, MAX_FILE))
    info = json.loads(output([str(binary_path), "--build-info"]))
    build = checked_identity(commit, version, info, output(["rustc", "-V"]))
    prefix = f"minibookx-ctl-{version}-{build}"
    pin = dbus_metadata()
    dbus_archive = ensure_archive(offline=True)
    c_build = json.loads((REPO / "target/native/libdbus-musl/build-info.json").read_text())
    if (c_build.get("source") != pin or
            digest(REPO / "target/native/libdbus-musl/lib/libdbus-1.a") != c_build.get("static_library_sha256") or
            info.get("libdbus_static_sha256") != c_build.get("static_library_sha256")):
        raise ValueError("libdbus build input/library identity is missing or inconsistent")
    rustc = run(["rustc", "-Vv"], stdout=subprocess.PIPE).stdout
    identity = {
        "schema_version": 1, "version": version, "commit": commit, "build": build,
        "tag": f"v{version}",
        "target": TARGET,
        "binary_archive": prefix + "-" + TARGET + ".tar.gz",
        "source_archive": prefix + "-sources.tar.gz",
        "binary_sha256": digest(binary_path), "binary_version": info["binary_version"],
        "rustc": rustc,
        "libdbus": {"version": pin["version"], "source_sha256": pin["sha256"],
                    "static_library_sha256": c_build["static_library_sha256"]},
    }
    metadata = json_bytes(identity)
    epoch = int(output(["git", "show", "-s", "--format=%ct", "HEAD"]))
    args.output.mkdir(parents=True, exist_ok=True)
    names = [identity["binary_archive"], identity["source_archive"], "build-metadata.json", FLAT_BINARY, "SHA256SUMS"]
    if any((args.output / name).exists() for name in names):
        raise ValueError("output assets already exist; never overwrite an identified release")
    with tempfile.TemporaryDirectory(prefix="minibookx-release-") as temp:
        base = Path(temp)
        source = base / (prefix + "-sources")
        source.mkdir()
        for name in TOP_FILES:
            shutil.copy2(REPO / name, source / name)
        for name in SOURCE_DIRS:
            copy_public_tree(REPO / name, source / name)
        bundled = source / "third-party/sources"
        bundled.mkdir(parents=True, exist_ok=True)
        shutil.copy2(dbus_archive, bundled / pin["archive"])
        cargo_config = run(["cargo", "vendor", "--locked", str(source / "vendor")], stdout=subprocess.PIPE).stdout
        (source / ".cargo").mkdir()
        (source / ".cargo/config.toml").write_text(cargo_config.replace(str(source / "vendor"), "vendor"))
        runtime = source / "packaging/licenses/rust-runtime"
        runtime.mkdir(exist_ok=True)
        sysroot = Path(output(["rustc", "--print", "sysroot"]))
        rust_docs = sysroot / "share/doc/rust"
        shutil.copy2(rust_docs / "COPYRIGHT-library.html", runtime / "COPYRIGHT-library.html")
        shutil.copytree(rust_docs / "licenses", runtime / "licenses", dirs_exist_ok=True)
        (source / "BUILD-TOOLCHAIN.txt").write_text(rustc)
        (source / "BUILD-COMMIT").write_text(commit + "\n")
        (source / "build-metadata.json").write_bytes(metadata)
        binary = base / (prefix + "-" + TARGET)
        binary.mkdir()
        shutil.copy2(binary_path, binary / "minibookx-ctl")
        (binary / "minibookx-ctl").chmod(0o755)
        for name in ("README.md", "SECURITY.md", "LICENSE", "NOTICE", "THIRD_PARTY.md", "install.sh", "BUILD-TOOLCHAIN.txt"):
            shutil.copy2(source / name, binary / name)
        for name in ("packaging", "config", "docs", "contrib"):
            shutil.copytree(source / name, binary / name)
        (binary / "third-party").mkdir()
        shutil.copy2(source / "third-party/libdbus.json", binary / "third-party/libdbus.json")
        (binary / "build-metadata.json").write_bytes(metadata)
        for folder in (binary, source):
            archive(base / (folder.name + ".tar.gz"), folder, epoch)
        (base / "build-metadata.json").write_bytes(metadata)
        shutil.copy2(binary_path, base / FLAT_BINARY)
        (base / FLAT_BINARY).chmod(0o755)
        (base / "SHA256SUMS").write_text("".join(digest(base / name) + "  " + name + "\n" for name in names[:-1]))
        if output(["git", "rev-parse", "HEAD"]) != commit or output(["git", "status", "--porcelain", "--untracked-files=all"]):
            raise ValueError("source changed while packaging; no outputs published")
        for name in names:
            with (args.output / name).open("xb") as destination, (base / name).open("rb") as origin:
                shutil.copyfileobj(origin, destination)
            print(args.output / name)


if __name__ == "__main__":
    main()
