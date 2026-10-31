# SPDX-License-Identifier: Apache-2.0
"""Derive panel timings from a locally owned, exact known U300 VBT.

No firmware binary is distributed by this project. Run unprivileged. This writes
ordinary files to a NEW output directory; it does not install or flash anything.
"""

import argparse
import hashlib
import json
import stat
import struct
from pathlib import Path

STOCK_SHA = "0d6ab7c6349a8845969cc04561beee5d96de0cd566dacd5747605346d598d7cd"
KNOWN = {
    STOCK_SHA,
    "de373f827392e62b3ef4104358052417e5c274b888bfcbfa745b679f23b7d23a",
    "2f04a83ff7f10ab339cda63642958e58273d842c70c59a704c4056ae51a03567",
}
EXPECTED = {
    "59-94": (
        "de373f827392e62b3ef4104358052417e5c274b888bfcbfa745b679f23b7d23a",
        165754,
    ),
    "60": ("2f04a83ff7f10ab339cda63642958e58273d842c70c59a704c4056ae51a03567", 165920),
}


def newc(files):
    archive = bytearray()

    def member(name, data, mode, inode):
        encoded = name.encode() + b"\0"
        fields = [
            inode,
            mode,
            0,
            0,
            2 if stat.S_ISDIR(mode) else 1,
            0,
            len(data),
            0,
            0,
            0,
            0,
            len(encoded),
            0,
        ]
        archive.extend(b"070701" + "".join(f"{n:08x}" for n in fields).encode())
        archive.extend(encoded)
        archive.extend(b"\0" * (-len(archive) % 4))
        archive.extend(data)
        archive.extend(b"\0" * (-len(archive) % 4))

    directories = ["usr", "usr/lib", "usr/lib/firmware", "usr/lib/firmware/i915"]
    for i, name in enumerate(directories):
        member(name, b"", stat.S_IFDIR | 0o755, i)
    for i, (name, data) in enumerate(sorted(files.items()), len(directories)):
        member("usr/lib/firmware/i915/" + name, data, stat.S_IFREG | 0o644, i)
    member("TRAILER!!!", b"", 0, 0)
    archive.extend(b"\0" * (-len(archive) % 512))
    return bytes(archive)


def derive(blob):
    if len(blob) != 10752 or hashlib.sha256(blob).hexdigest() not in KNOWN:
        raise ValueError(
            "not the exact locally tested U300 VBT; refusing another panel/revision"
        )
    stock = bytearray(blob)
    struct.pack_into("<I", stock, 8035, 138267)
    stock[26] = 94  # Original byte, not a repaired checksum.
    if hashlib.sha256(stock).hexdigest() != STOCK_SHA:
        raise ValueError("stock normalization failed")
    files, modes = {}, {}
    for mode, (expected, clock) in EXPECTED.items():
        data = bytearray(stock)
        struct.pack_into("<I", data, 8035, clock)
        data[26] = 0
        data[26] = (-sum(data[:10269])) & 255
        if hashlib.sha256(data).hexdigest() != expected or sum(data[:10269]) % 256:
            raise ValueError("derived VBT failed byte identity/checksum check")
        files[f"chuwi-u300-{mode}.bin"] = bytes(data)
        modes[mode] = {
            "clock_kHz": clock,
            "sha256": expected,
            "changed_offsets": [
                i for i, (a, b) in enumerate(zip(stock, data)) if a != b
            ],
        }
    cpio = newc(files)
    manifest = {
        "stock_sha256": STOCK_SHA,
        "format": "RGB888",
        "lanes": 4,
        "htotal": 1272,
        "vtotal": 2174,
        "modes": modes,
        "cpio_sha256": hashlib.sha256(cpio).hexdigest(),
        "cpio_blake2b": hashlib.blake2b(cpio).hexdigest(),
        "cpio_bytes": len(cpio),
    }
    return files, cpio, manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    files, cpio, manifest = derive(args.input.read_bytes())
    args.output.mkdir(mode=0o700, parents=False, exist_ok=False)
    for name, data in files.items():
        (args.output / name).write_bytes(data)
    (args.output / "vbt-modes.cpio").write_bytes(cpio)
    (args.output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps(manifest, indent=2))


if __name__ == "__main__":
    main()
