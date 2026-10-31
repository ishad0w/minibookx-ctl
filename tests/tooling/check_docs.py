"""Check maintained Markdown links and accidental private paths; never rewrite it."""

# SPDX-License-Identifier: Apache-2.0
import re
import subprocess
import sys
import unicodedata
import urllib.parse
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
LINK = re.compile(r"(?<!!)\[[^\]\n]+\]\(([^)\n]+)\)")
PRIVATE = re.compile(
    r"/home/|/var/tmp/|192\.168\.|\.chuwipass|BEGIN [A-Z ]*PRIVATE KEY"
)


def anchors(text):
    """GFM heading IDs for the plain ATX headings used in maintained docs."""
    result, counts = set(), {}
    fence = None
    for line in text.splitlines():
        stripped = line.lstrip()
        if stripped.startswith(("```", "~~~")):
            marker = stripped[:3]
            fence = None if fence == marker else marker if fence is None else fence
            continue
        if fence is not None:
            continue
        match = re.match(r"^ {0,3}#{1,6}\s+(.+?)\s*#*\s*$", line)
        if not match:
            continue
        title = LINK.sub(lambda link: link[0].split("]")[0][1:], match[1]).lower()
        slug = "".join(
            c for c in title if c in " -_" or unicodedata.category(c)[0] in "LN"
        ).replace(" ", "-")
        duplicate = counts.get(slug, 0)
        counts[slug] = duplicate + 1
        result.add(slug if duplicate == 0 else f"{slug}-{duplicate}")
    return result


def source_links(repo):
    errors = []
    for source in (repo / "src").rglob("*.rs"):
        for link in re.findall(r'doc_url!\("([^"\n]+)"\)', source.read_text()):
            parsed = urllib.parse.urlsplit(link)
            target = (repo / "docs" / urllib.parse.unquote(parsed.path)).resolve()
            if not target.is_relative_to(repo / "docs") or not target.is_file():
                errors.append(f"{source.name}: missing documentation target {link}")
            elif parsed.fragment and parsed.fragment not in anchors(target.read_text()):
                errors.append(f"{source.name}: missing documentation anchor {link}")
    return errors


def bootstrap_pin(repo):
    pins = re.findall(
        r"https://raw\.githubusercontent\.com/ishad0w/minibookx-ctl/([0-9a-f]{40})/install\.sh",
        (repo / "README.md").read_text(),
    )
    if len(pins) != 1:
        return ["README.md: require one canonical bootstrap commit pin"]
    if not (repo / ".git").exists():
        # Corresponding source archives have no Git metadata. CI must perform
        # the ancestry/content check in its full-history checkout.
        return []
    pin = pins[0]
    ancestor = subprocess.run(
        ["git", "merge-base", "--is-ancestor", pin, "HEAD"],
        cwd=repo,
        check=False,
        capture_output=True,
    )
    if ancestor.returncode:
        return [
            "README.md: bootstrap pin must be an ancestor of HEAD (full history required)"
        ]
    contents = subprocess.run(
        ["git", "show", pin + ":install.sh"],
        cwd=repo,
        check=False,
        capture_output=True,
    )
    if contents.returncode or contents.stdout != (repo / "install.sh").read_bytes():
        return ["README.md: pinned bootstrap differs from the reviewed install.sh"]
    return []


def main():
    paths = [
        REPO / name
        for name in (
            "README.md",
            "AGENTS.md",
            "MAP.md",
            "SECURITY.md",
            "THIRD_PARTY.md",
        )
    ]
    for directory in ("docs", "packaging", "contrib"):
        paths.extend((REPO / directory).rglob("*.md"))
    errors = source_links(REPO) + bootstrap_pin(REPO)
    for path in sorted(set(paths)):
        text = path.read_text(encoding="utf-8")
        label = path.relative_to(REPO)
        if PRIVATE.search(text):
            errors.append(f"{label}: unexpected private path/address/credential marker")
        if text.count("```mermaid") and text.count("```") % 2:
            errors.append(f"{label}: unclosed fenced code block")
        for match in LINK.finditer(text):
            link = match[1].strip("<>")
            parsed = urllib.parse.urlsplit(link)
            if parsed.scheme or parsed.netloc or not (parsed.path or parsed.fragment):
                continue
            target = (
                (path.parent / urllib.parse.unquote(parsed.path)).resolve()
                if parsed.path
                else path.resolve()
            )
            if not target.is_relative_to(REPO) or not target.exists():
                errors.append(f"{label}: missing/outside local target {link}")
            elif (
                parsed.fragment
                and target.suffix == ".md"
                and urllib.parse.unquote(parsed.fragment)
                not in anchors(target.read_text(encoding="utf-8"))
            ):
                errors.append(f"{label}: missing heading anchor {link}")
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(
        f"Checked {len(set(paths))} maintained Markdown files; no documents rewritten."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
