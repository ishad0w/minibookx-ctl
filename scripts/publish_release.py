"""Publish this verified main build without replacing an existing public build."""

# SPDX-License-Identifier: Apache-2.0
import argparse
import json
import os
import re
import subprocess
import tempfile
from pathlib import Path

from release_format import checked_assets

REPO = "ishad0w/minibookx-ctl"


def gh(*args):
    result = subprocess.run(
        ["gh", *args], check=False, text=True, capture_output=True, timeout=180
    )
    if result.returncode:
        raise RuntimeError("GitHub operation failed: " + result.stderr.strip())
    return result.stdout


def api(endpoint, data=None, missing=False):
    path = "repos/" + REPO + ("/" + endpoint if endpoint else "")
    arguments = ["gh", "api", path]
    if data is not None:
        arguments += ["--method", "POST", "--input", "-"]
    result = subprocess.run(
        arguments,
        check=False,
        input=json.dumps(data) if data is not None else None,
        text=True,
        capture_output=True,
        timeout=60,
    )
    if result.returncode:
        if missing and "(HTTP 404)" in result.stderr:
            return None
        raise RuntimeError("GitHub API operation failed: " + result.stderr.strip())
    return json.loads(result.stdout)


def release_info(tag, missing=False):
    # REST /releases/tags/{tag} returns published releases only. The CLI also
    # resolves authenticated drafts and gives their canonical numeric API URL.
    result = subprocess.run(
        ["gh", "release", "view", tag, "--repo", REPO, "--json", "apiUrl"],
        check=False,
        text=True,
        capture_output=True,
        timeout=60,
    )
    if result.returncode:
        if missing and result.stderr.strip() == "release not found":
            return None
        raise RuntimeError("GitHub release lookup failed: " + result.stderr.strip())
    url = json.loads(result.stdout).get("apiUrl", "")
    match = re.fullmatch(
        r"https://api\.github\.com/repos/" + re.escape(REPO) + r"/releases/([0-9]+)",
        url,
    )
    if match is None:
        raise ValueError("release lookup returned an unexpected API URL")
    release = api("releases/" + match[1])
    if release.get("tag_name") != tag:
        raise ValueError("release lookup returned the wrong tag")
    return release


def verify_existing_release(release, folder, names):
    remote_names = [item["name"] for item in release["assets"]]
    if sorted(remote_names) != names:
        raise ValueError(
            "existing published release assets differ; refusing to overwrite"
        )
    with tempfile.TemporaryDirectory(prefix="minibookx-published-") as temp:
        gh("release", "download", release["tag_name"], "--repo", REPO, "--dir", temp)
        for name in names:
            if (Path(temp) / name).read_bytes() != (folder / name).read_bytes():
                raise ValueError(
                    "existing published release bytes differ; refusing to overwrite"
                )


def upload_missing_assets(release, folder, names):
    if not release.get("draft"):
        raise ValueError("asset upload requires a draft release")
    present = [item["name"] for item in release["assets"]]
    if len(set(present)) != len(present) or not set(present).issubset(names):
        raise ValueError("draft contains unexpected or duplicate assets")
    if present:
        with tempfile.TemporaryDirectory(prefix="minibookx-draft-") as temp:
            patterns = [arg for name in present for arg in ("--pattern", name)]
            gh(
                "release",
                "download",
                release["tag_name"],
                "--repo",
                REPO,
                "--dir",
                temp,
                *patterns,
            )
            for name in present:
                if (Path(temp) / name).read_bytes() != (folder / name).read_bytes():
                    raise ValueError("draft asset bytes differ; refusing to overwrite")
    missing = [name for name in names if name not in present]
    if missing:
        # Never clobber. A concurrent publisher/upload produces a safe conflict
        # instead of replacing assets that may have become public meanwhile.
        gh(
            "release",
            "upload",
            release["tag_name"],
            "--repo",
            REPO,
            *[str(folder / name) for name in missing],
        )


def publish(folder, commit):
    metadata, names = checked_assets(folder, commit)
    tag = metadata["tag"]
    repository = api("")
    if repository.get("default_branch") != "main":
        raise ValueError("publication requires main as the default branch")
    reference = api("git/ref/tags/" + tag, missing=True)
    if reference is None:
        api("git/refs", {"ref": "refs/tags/" + tag, "sha": commit})
    elif (
        reference["object"].get("type") != "commit"
        or reference["object"].get("sha") != commit
    ):
        raise ValueError(
            "tag already points elsewhere or is annotated; never repoint it"
        )
    release = release_info(tag, missing=True)
    if release is not None and not release["draft"]:
        if release.get("prerelease"):
            raise ValueError("existing release is a prerelease; refusing to rewrite it")
        verify_existing_release(release, folder, names)
        print(
            "Published build already exists with identical assets: "
            + release["html_url"]
        )
        return
    with tempfile.TemporaryDirectory(prefix="minibookx-notes-") as temp:
        notes = Path(temp) / "notes.md"
        notes.write_text(
            "Build `"
            + metadata["binary_version"]
            + "` from commit `"
            + commit
            + "`.\n\n"
            "Static x86_64 Linux executable, license notices, exact dependency sources, "
            "build metadata and SHA-256 checksums. Tested hardware scope, installation "
            "and recovery limits are documented in the repository.\n\n"
            "The display RFC and timing tools are separate contributions; installing "
            "the controller does not patch the kernel or change panel timings.\n"
        )
        if release is None:
            title = "Release (" + tag + ")"
            gh(
                "release",
                "create",
                tag,
                "--repo",
                REPO,
                "--verify-tag",
                "--draft",
                "--title",
                title,
                "--notes-file",
                str(notes),
            )
            release = release_info(tag)
        upload_missing_assets(release, folder, names)
        draft = release_info(tag)
        verify_existing_release(draft, folder, names)
        head = api("git/ref/heads/main")["object"]["sha"]
        latest = "true" if head == commit else "false"
        gh(
            "release",
            "edit",
            tag,
            "--repo",
            REPO,
            "--draft=false",
            "--latest=" + latest,
        )
    print("https://github.com/" + REPO + "/releases/tag/" + tag)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, default=Path("dist"))
    parser.add_argument("--commit", required=True)
    args = parser.parse_args()
    if (
        os.environ.get("GITHUB_EVENT_NAME") != "workflow_dispatch"
        or os.environ.get("MINIBOOKX_PUBLISH") != "true"
        or os.environ.get("GITHUB_REF") != "refs/heads/main"
        or os.environ.get("GITHUB_REPOSITORY") != REPO
        or os.environ.get("GITHUB_SHA") != args.commit
    ):
        raise ValueError(
            "publication requires explicit workflow dispatch on this repository's main branch"
        )
    publish(args.directory.resolve(), args.commit)


if __name__ == "__main__":
    main()
