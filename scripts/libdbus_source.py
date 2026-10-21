#!/usr/bin/env python3
"""Fetch the exact libdbus input and build only its static Linux client library.

The packager may import ``metadata`` and ``ensure_archive``. An explicit archive
is always checked; ``offline=True`` also permits a verified cache hit without a
network request. Building is a separate operation and never requires root.
"""
# SPDX-License-Identifier: Apache-2.0
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shlex
import shutil
import stat
import subprocess
import tarfile
import tempfile
import urllib.parse
import urllib.request

REPO = Path(__file__).resolve().parent.parent
DEFAULT_CACHE = REPO / "target/native/source-cache"
DEFAULT_PREFIX = REPO / "target/native/libdbus-musl"


def metadata() -> dict:
    """Return the reviewed source pin, independent of Cargo's binding version."""
    pin = json.loads((REPO / "third-party/libdbus.json").read_text(encoding="utf-8"))
    version = pin["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError("invalid libdbus version pin")
    archive = f"dbus-{version}.tar.xz"
    if (
        pin["archive"] != archive
        or pin["source_directory"] != f"dbus-{version}"
        or pin["url"] != f"https://dbus.freedesktop.org/releases/dbus/{archive}"
        or not re.fullmatch(r"[0-9a-f]{64}", pin["sha256"])
        or not isinstance(pin["size"], int)
        or not 0 < pin["size"] < 8 * 1024 * 1024
    ):
        raise ValueError("invalid libdbus source metadata")
    return pin


def verify_archive(path: Path) -> Path:
    """Reject symlinks, special files, size changes and an unpinned digest."""
    pin = metadata()
    path = Path(path).expanduser().absolute()
    fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(fd, "rb") as stream:
        before = os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_size != pin["size"]:
            raise ValueError(f"wrong libdbus source type or size: {path}")
        digest = hashlib.sha256()
        remaining = pin["size"] + 1
        while remaining:
            chunk = stream.read(min(64 * 1024, remaining))
            if not chunk:
                break
            digest.update(chunk)
            remaining -= len(chunk)
        after = os.fstat(stream.fileno())
        if (
            (before.st_size, before.st_mtime_ns, before.st_ctime_ns)
            != (after.st_size, after.st_mtime_ns, after.st_ctime_ns)
            or digest.hexdigest() != pin["sha256"]
        ):
            raise ValueError(f"libdbus source SHA-256 mismatch: {path}")
    return path


class _PrimaryHttpsRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        destination = urllib.parse.urlsplit(newurl)
        if destination.scheme != "https" or destination.netloc != "dbus.freedesktop.org":
            raise ValueError("refusing libdbus download redirect outside upstream HTTPS")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def ensure_archive(
    cache_dir: Path = DEFAULT_CACHE,
    archive: Path | None = None,
    offline: bool = False,
) -> Path:
    """Return the validated exact upstream tarball, reusable in release sources.

    Never download over a bad existing cache entry. Remove that entry explicitly
    after investigating a mismatch. Source archives can pass their bundled file
    through ``archive`` and rebuild without contacting upstream.
    """
    if archive is not None:
        return verify_archive(archive)
    pin = metadata()
    cache_dir = Path(cache_dir).expanduser().absolute()
    path = cache_dir / pin["archive"]
    if path.exists() or path.is_symlink():
        return verify_archive(path)
    if offline:
        raise FileNotFoundError(f"offline libdbus source is missing: {path}")
    cache_dir.mkdir(parents=True, exist_ok=True)
    temp_path = None
    try:
        with tempfile.NamedTemporaryFile(dir=cache_dir, prefix=".libdbus-", delete=False) as out:
            temp_path = Path(out.name)
            opener = urllib.request.build_opener(_PrimaryHttpsRedirect())
            with opener.open(pin["url"], timeout=30) as response:
                remaining = pin["size"] + 1
                while remaining:
                    chunk = response.read(min(64 * 1024, remaining))
                    if not chunk:
                        break
                    out.write(chunk)
                    remaining -= len(chunk)
            out.flush()
            os.fsync(out.fileno())
        verify_archive(temp_path)
        os.replace(temp_path, path)
        return verify_archive(path)
    finally:
        if temp_path is not None:
            temp_path.unlink(missing_ok=True)


def _extract(archive: Path, destination: Path) -> Path:
    """Extract a freshly validated, link-free source tree into an empty directory."""
    pin = metadata()
    verify_archive(archive)
    with tarfile.open(archive, "r:xz") as tar:
        members = tar.getmembers()
        for member in members:
            path = PurePosixPath(member.name)
            if (
                path.is_absolute()
                or ".." in path.parts
                or not path.parts
                or path.parts[0] != pin["source_directory"]
                or not (member.isfile() or member.isdir())
            ):
                raise ValueError(f"unexpected libdbus archive member: {member.name}")
        # The pinned release has no links, devices or absolute/traversing paths.
        tar.extractall(destination, members=members, filter="data")
    return destination / pin["source_directory"]


def _meson_array(values: list[str]) -> str:
    # Meson machine files use Python-like quoted strings, not shell commands.
    if any(ord(character) < 32 or ord(character) == 127 for value in values for character in value):
        raise ValueError("compiler arguments must not contain control characters")
    return "[" + ", ".join("'" + value.replace("\\", "\\\\").replace("'", "\\'") + "'" for value in values) + "]"


def _command(argv: list[str], **kwargs) -> None:
    print("+ " + shlex.join(argv), flush=True)
    subprocess.run(argv, check=True, **kwargs)


def build(args) -> None:
    if os.geteuid() == 0:
        raise ValueError("build as an ordinary user; libdbus needs no root installation")
    pin = metadata()
    archive = ensure_archive(args.cache_dir, args.archive, args.offline)
    prefix = args.prefix.expanduser().absolute()
    for tool in ["meson", "ninja", "pkg-config", "ar", "readelf"]:
        if shutil.which(tool) is None:
            raise FileNotFoundError(f"required build tool not found: {tool}")
    compiler = shutil.which(args.cc)
    if compiler is None:
        raise FileNotFoundError(f"musl C compiler not found: {args.cc}")
    cflags = shlex.split(os.environ.get("CFLAGS", ""))
    cc = [compiler, *args.cc_arg]
    prefix.parent.mkdir(parents=True, exist_ok=True)
    # An isolated fresh source/build tree avoids reusing modified extracted code
    # or a Meson cache from a different C compiler. Only --prefix is installed.
    with tempfile.TemporaryDirectory(prefix=".libdbus-build-", dir=prefix.parent) as temp:
        temp = Path(temp)
        source = _extract(archive, temp)
        cross = temp / "musl.ini"
        cross.write_text(
            "[binaries]\n"
            f"c = {_meson_array(cc)}\n"
            "ar = 'ar'\nstrip = 'strip'\npkg-config = 'pkg-config'\n"
            "[host_machine]\nsystem = 'linux'\ncpu_family = 'x86_64'\ncpu = 'x86_64'\nendian = 'little'\n"
            "[properties]\nneeds_exe_wrapper = true\n"
            "[built-in options]\n"
            f"c_args = {_meson_array([*cflags, '-fPIC', f'-ffile-prefix-map={temp}=/libdbus'])}\n"
            "c_link_args = ['-static']\n",
            encoding="utf-8",
        )
        probe = temp / "check-musl.c"
        probe.write_text(
            "#include <features.h>\n"
            "#if defined(__GLIBC__) || !defined(__linux__) || !defined(__x86_64__)\n"
            '#error "a Linux x86_64 musl compiler is required"\n'
            "#endif\nint main(void) { return 0; }\n",
            encoding="utf-8",
        )
        _command([*cc, *cflags, "-static", str(probe), "-o", str(temp / "check-musl")])
        elf = subprocess.check_output(["readelf", "-lW", str(temp / "check-musl")], text=True)
        if "INTERP" in elf:
            raise ValueError("compiler produced a dynamic executable instead of static musl")
        build_dir = temp / "build"
        options = [
            "--buildtype=release", "--default-library=static", "--wrap-mode=nodownload",
            f"--prefix={prefix}", "--libdir=lib", "--cross-file", str(cross),
            "--sysconfdir=/etc", "--localstatedir=/var",
            "-Dauto_features=disabled", "-Dchecks=true", "-Dasserts=false",
            "-Dintrusive_tests=false", "-Dinstalled_tests=false", "-Dmodular_tests=disabled",
            "-Dmessage_bus=false", "-Dtools=false", "-Dtraditional_activation=false",
            "-Duser_session=false", "-Dx11_autolaunch=disabled", "-Dlaunchd=disabled",
            "-Dapparmor=disabled", "-Dselinux=disabled", "-Dlibaudit=disabled",
            "-Dsystemd=disabled", "-Dvalgrind=disabled", "-Dverbose_mode=false", "-Dstats=false",
            "-Ddoxygen_docs=disabled", "-Dducktype_docs=disabled", "-Dxml_docs=disabled", "-Dqt_help=disabled",
            "-Drelocation=disabled", "-Depoll=enabled", "-Dinotify=disabled", "-Dkqueue=disabled",
            "-Dsystem_socket=/run/dbus/system_bus_socket", "-Druntime_dir=/run",
        ]
        # Do not allow host pkg-config libraries to enter the cross build.
        env = {**os.environ, "PKG_CONFIG_LIBDIR": str(temp / "empty-pkg-config"), "PKG_CONFIG_PATH": ""}
        (temp / "empty-pkg-config").mkdir()
        _command(["meson", "setup", str(build_dir), str(source), *options], env=env)
        # Upstream's default all target still contains some non-installed test
        # programs even with modular/intrusive tests disabled. Build just the
        # public library; no test server or daemon needs to be linked or run.
        _command(["meson", "compile", "-C", str(build_dir), "-j", str(args.jobs), "dbus-1"], env=env)
        _command(["meson", "install", "-C", str(build_dir), "--no-rebuild", "--tags", "devel"], env=env)
        # This generated ABI header is install_data() without an upstream
        # development tag. Copy only that required header, not static doc/XML
        # assets that upstream otherwise installs even when doc builds are off.
        arch_header = prefix / "lib/dbus-1.0/include/dbus/dbus-arch-deps.h"
        arch_header.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(build_dir / "dbus/dbus-arch-deps.h", arch_header)
        library = prefix / "lib/libdbus-1.a"
        if not library.is_file() or list(prefix.rglob("*.so*")):
            raise ValueError("libdbus install did not contain only the expected static library")
        if any((prefix / part).exists() for part in ["bin", "libexec", "share"]):
            raise ValueError("unexpected libdbus executable, daemon, data or documentation was installed")
        info = {
            "source": pin,
            "compiler": cc,
            "cflags": cflags,
            "meson_options": options,
            "static_library_sha256": hashlib.sha256(library.read_bytes()).hexdigest(),
        }
        (prefix / "build-info.json").write_text(json.dumps(info, indent=2) + "\n", encoding="utf-8")
    print(f"Static libdbus {pin['version']} prefix: {prefix}")
    print(f"Validated source archive: {archive}")
    print("Use these environment variables for the musl Cargo build:")
    for key, value in {
        "PKG_CONFIG_LIBDIR": str(prefix / "lib/pkgconfig"),
        "PKG_CONFIG_PATH": "",
        "PKG_CONFIG_ALLOW_CROSS": "1",
        "DBUS_1_STATIC": "1",
    }.items():
        print(f"export {key}={shlex.quote(value)}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ["archive", "build"]:
        command = sub.add_parser(name)
        command.add_argument("--cache-dir", type=Path, default=DEFAULT_CACHE)
        command.add_argument("--archive", type=Path, help="use an exact bundled tarball instead of downloading")
        command.add_argument("--offline", action="store_true", help="allow verified local sources only")
        if name == "build":
            command.add_argument("--prefix", type=Path, default=DEFAULT_PREFIX)
            command.add_argument("--cc", default=os.environ.get("CC_x86_64_unknown_linux_musl", "musl-gcc"))
            command.add_argument("--cc-arg", action="append", default=[], help="one extra compiler argument; repeat as needed")
            command.add_argument("--jobs", type=int, default=min(os.cpu_count() or 1, 8))
    args = parser.parse_args()
    if args.command == "archive":
        print(ensure_archive(args.cache_dir, args.archive, args.offline))
    elif args.jobs < 1 or args.jobs > 256:
        parser.error("--jobs must be 1..256")
    else:
        build(args)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"libdbus build: {error}") from error
