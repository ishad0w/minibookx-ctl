"""Pure validation of release metadata, checksums, archives and static ELF files.

This module does not install files, execute release code, access hardware or
invoke package/service managers. It is shared by local and CI release checks.
"""

# SPDX-License-Identifier: Apache-2.0
import argparse
import gzip
import hashlib
import io
import json
import os
import re
import stat
import struct
import tarfile
from pathlib import Path

NAME = "minibookx-ctl"
TARGET = "x86_64-unknown-linux-musl"
FLAT_BINARY = NAME + "-x86_64-linux"
MAX_FILE = 16 * 1024 * 1024
MAX_ARCHIVE = 32 * 1024 * 1024
MAX_EXPANDED = 64 * 1024 * 1024
MAX_MEMBERS = 2048
# The source archive carries vendored crates and the pinned libdbus tarball.
MAX_SOURCE_EXPANDED = 128 * 1024 * 1024
MAX_SOURCE_MEMBERS = 16384
TAG_RE = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z")
HASH_RE = re.compile(r"[0-9a-f]{64}\Z")
COMMIT_RE = re.compile(r"[0-9a-f]{40}\Z")


class Refuse(ValueError):
    pass


def fail(message):
    raise Refuse(message)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def read_asset(path, maximum=MAX_ARCHIVE):
    """Read one bounded regular asset without following its final symlink."""
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    try:
        before = os.fstat(fd)
        if not stat.S_ISREG(before.st_mode) or before.st_size > maximum:
            fail("release asset must be a bounded regular file")
        with os.fdopen(fd, "rb", closefd=False) as stream:
            data = stream.read(maximum + 1)
        after = os.fstat(fd)
        keys = ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns", "st_mode")
        if len(data) > maximum or any(
            getattr(before, k) != getattr(after, k) for k in keys
        ):
            fail("release asset changed while reading or exceeds its size limit")
        return data
    finally:
        os.close(fd)


def parse_json(data, description):
    def no_duplicates(items):
        result = {}
        for key, value in items:
            if key in result:
                fail(f"duplicate JSON key in {description}: {key}")
            result[key] = value
        return result

    try:
        return json.loads(data, object_pairs_hook=no_duplicates)
    except Refuse:
        raise
    except (ValueError, UnicodeError) as error:
        raise Refuse(f"invalid {description}") from error


def verify_binary(data):
    """Validate the static x86_64 ELF shape without executing downloaded code."""
    if len(data) < 64 or data[:7] != b"\x7fELF\x02\x01\x01" or data[7] not in (0, 3):
        fail("binary must be an ELF64 little-endian executable")
    header = struct.unpack_from("<16sHHIQQQIHHHHHH", data)
    if (
        header[1] not in (2, 3)
        or header[2:4] != (62, 1)
        or header[8] != 64
        or header[9] != 56
    ):
        fail("binary must be an x86_64 Linux executable")
    offset, count = header[5], header[10]
    if not 1 <= count <= 128 or offset < 64 or offset + count * 56 > len(data):
        fail("invalid ELF program headers")
    load = stack = False
    for index in range(count):
        kind, flags, file_offset, _, _, file_size, _, _ = struct.unpack_from(
            "<IIQQQQQQ", data, offset + index * 56
        )
        if file_offset + file_size > len(data):
            fail("ELF segment exceeds the binary")
        if kind == 1:
            load = True
        elif kind == 3:
            fail("binary uses a dynamic interpreter; a static release is required")
        elif kind == 2:
            if file_size % 16:
                fail("invalid ELF dynamic table")
            for entry in range(file_offset, file_offset + file_size, 16):
                tag, _ = struct.unpack_from("<qQ", data, entry)
                if tag == 0:
                    break
                if tag == 1:
                    fail("binary has shared-library dependencies")
        elif kind == 0x6474E551:
            stack = True
            if flags & 1:
                fail("binary requests an executable stack")
    if not load or not stack:
        fail("binary lacks load or non-executable stack metadata")


def validate_metadata(data, tag):
    metadata = parse_json(data, "build metadata")
    if not isinstance(metadata, dict):
        fail("release metadata must be an object")
    commit = metadata.get("commit")
    if not isinstance(commit, str) or not COMMIT_RE.fullmatch(commit):
        fail("release metadata has an invalid full commit")
    build = commit[:12]
    match = TAG_RE.fullmatch(tag) if isinstance(tag, str) else None
    if match is None:
        fail("release tag must be v<major>.<minor>.<patch>")
    version = ".".join(match.groups())
    prefix = NAME + "-" + version + "-" + build
    expected = {
        "schema_version": 1,
        "version": version,
        "build": build,
        "tag": tag,
        "target": TARGET,
        "binary_version": version + "+g" + build,
        "binary_archive": prefix + "-" + TARGET + ".tar.gz",
        "source_archive": prefix + "-sources.tar.gz",
    }
    if type(metadata.get("schema_version")) is not int or any(
        metadata.get(key) != value for key, value in expected.items()
    ):
        fail("release metadata is inconsistent with its tag or target")
    if not isinstance(metadata.get("binary_sha256"), str) or not HASH_RE.fullmatch(
        metadata["binary_sha256"]
    ):
        fail("release metadata has an invalid binary hash")
    if (
        not isinstance(metadata.get("rustc"), str)
        or not 1 <= len(metadata["rustc"]) <= 8192
    ):
        fail("release metadata lacks the Rust toolchain identity")
    return metadata


def unpack_archive(data, top, max_expanded=None, max_members=None):
    """Validate an entire release archive before returning its file bytes."""
    max_expanded = MAX_EXPANDED if max_expanded is None else max_expanded
    max_members = MAX_MEMBERS if max_members is None else max_members
    if len(data) > MAX_ARCHIVE:
        fail("release archive exceeds the size limit")
    try:
        with gzip.GzipFile(fileobj=io.BytesIO(data)) as compressed:
            expanded = compressed.read(max_expanded + 1)
        if len(expanded) > max_expanded:
            fail("expanded release archive exceeds the size limit")
        result, seen = {}, set()
        with tarfile.open(fileobj=io.BytesIO(expanded), mode="r:") as archive:
            for number, member in enumerate(archive):
                if number >= max_members:
                    fail("too many release archive members")
                name = (
                    member.name[:-1]
                    if member.isdir() and member.name.endswith("/")
                    else member.name
                )
                parts = name.split("/")
                if (
                    not name
                    or "\\" in name
                    or any(
                        ord(character) < 32 or ord(character) == 127
                        for character in name
                    )
                    or any(part in ("", ".", "..") for part in parts)
                    or parts[0] != top
                ):
                    fail("unsafe path in release archive")
                if name in seen:
                    fail("duplicate path in release archive")
                seen.add(name)
                if (
                    member.mode & 0o7022
                    or member.sparse is not None
                    or not (member.isdir() or member.isfile())
                ):
                    fail(
                        "release archive contains a link, special file, sparse file or unsafe permissions"
                    )
                if len(parts) == 1:
                    if not member.isdir():
                        fail("archive root must be a directory")
                    continue
                relative = "/".join(parts[1:])
                if member.isfile():
                    if not 0 <= member.size <= MAX_FILE:
                        fail("release member exceeds the size limit")
                    stream = archive.extractfile(member)
                    if stream is None:
                        fail("unreadable release member")
                    content = stream.read(MAX_FILE + 1)
                    if len(content) != member.size:
                        fail("truncated release member")
                    result[relative] = content
        for path in result:
            components = path.split("/")
            if any(
                "/".join(components[:index]) in result
                for index in range(1, len(components))
            ):
                fail("release archive has a file/directory collision")
        if not result:
            fail("empty release archive")
        return result
    except (OSError, EOFError, tarfile.TarError) as error:
        raise Refuse("invalid compressed release archive") from error


def checksum_entries(data):
    try:
        text = data.decode("ascii")
    except UnicodeError as error:
        raise Refuse("invalid release SHA256SUMS") from error
    result = {}
    for line in text.splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9][A-Za-z0-9._-]*)", line)
        if match is None or match[2] in result:
            fail("malformed or duplicate release SHA256SUMS entry")
        result[match[2]] = match[1]
    if not result or len(result) > 16:
        fail("invalid release checksum count")
    return result


def checked_assets(folder, expected_commit):
    """Check every published byte and the flat/archived binary correspondence."""
    if not COMMIT_RE.fullmatch(expected_commit):
        fail("expected full commit is required")
    raw = read_asset(folder / "build-metadata.json", 65536)
    value = parse_json(raw, "build metadata")
    metadata = validate_metadata(
        raw, value.get("tag", "") if isinstance(value, dict) else ""
    )
    if metadata["commit"] != expected_commit:
        fail("build metadata does not match the workflow commit")
    names = {
        metadata["binary_archive"],
        metadata["source_archive"],
        "build-metadata.json",
        FLAT_BINARY,
    }
    sums = checksum_entries(read_asset(folder / "SHA256SUMS", 65536))
    if set(sums) != names or {p.name for p in folder.iterdir()} != names | {
        "SHA256SUMS"
    }:
        fail("unexpected/missing release file or checksum")
    assets = {
        name: read_asset(
            folder / name, MAX_FILE if name == FLAT_BINARY else MAX_ARCHIVE
        )
        for name in names
    }
    for name, content in assets.items():
        if digest(content) != sums[name]:
            fail("release checksum mismatch: " + name)
    binary = assets[FLAT_BINARY]
    verify_binary(binary)
    if digest(binary) != metadata["binary_sha256"]:
        fail("flat binary does not match its build metadata")
    top = metadata["binary_archive"].removesuffix(".tar.gz")
    files = unpack_archive(assets[metadata["binary_archive"]], top)
    if files.get(NAME) != binary or files.get("build-metadata.json") != raw:
        fail("archive binary or embedded metadata differs from the flat release assets")
    required = {
        "config/presets.conf",
        "LICENSE",
        "NOTICE",
        "THIRD_PARTY.md",
        "packaging/minibookx-ctl.service",
    }
    if not required.issubset(files) or not any(
        p.startswith("packaging/licenses/") for p in files
    ):
        fail("runtime archive lacks configuration, service or license files")
    if "packaging/minibookx-ctl-recover.service" in files:
        fail("runtime archive contains a removed auxiliary service")
    check_sources(assets[metadata["source_archive"]], metadata, raw)
    return metadata, sorted(names | {"SHA256SUMS"})


def check_sources(data, metadata, raw_metadata):
    """The corresponding source must identify the same build and carry its inputs."""
    top = metadata["source_archive"].removesuffix(".tar.gz")
    files = unpack_archive(data, top, MAX_SOURCE_EXPANDED, MAX_SOURCE_MEMBERS)
    if files.get("BUILD-COMMIT") != (metadata["commit"] + "\n").encode():
        fail("source archive does not identify the published commit")
    if files.get("build-metadata.json") != raw_metadata:
        fail("source archive metadata differs from the published metadata")
    required = {
        "Cargo.toml",
        "Cargo.lock",
        ".cargo/config.toml",
        "third-party/libdbus.json",
    }
    if not required.issubset(files) or not any(p.startswith("vendor/") for p in files):
        fail("source archive lacks locked or vendored Rust dependencies")
    pin = parse_json(files["third-party/libdbus.json"], "libdbus pin")
    archive = pin.get("archive") if isinstance(pin, dict) else None
    if (
        not isinstance(archive, str)
        or "/" in archive
        or digest(files.get("third-party/sources/" + archive, b"")) != pin.get("sha256")
        or not isinstance(metadata.get("libdbus"), dict)
        or metadata["libdbus"].get("source_sha256") != pin.get("sha256")
    ):
        fail("source archive lacks the pinned libdbus source")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    elf = commands.add_parser(
        "elf", help="check one static x86_64 ELF without executing it"
    )
    elf.add_argument("binary", type=Path)
    check = commands.add_parser(
        "check", help="check a complete release against its source commit"
    )
    check.add_argument("directory", type=Path)
    check.add_argument("--commit", required=True)
    args = parser.parse_args()
    if args.command == "elf":
        verify_binary(read_asset(args.binary, MAX_FILE))
        print(
            "Static ELF check passed: x86_64, no INTERP/DT_NEEDED, non-executable stack."
        )
    else:
        metadata, names = checked_assets(args.directory.resolve(), args.commit)
        print(
            f"Verified {metadata['tag']}: {len(names)} assets; static ELF and archive policy passed."
        )


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError) as error:
        raise SystemExit(f"release check: {error}") from error
