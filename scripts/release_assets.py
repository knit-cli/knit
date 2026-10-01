#!/usr/bin/env python3
"""Publish one raw archive/checksum pair without replacing release assets."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


class AssetError(Exception):
    pass


def validate_pair(archive_name, archive, checksum):
    try:
        match = re.fullmatch(r"([0-9a-fA-F]{64}) [ *]([^\r\n]+)\r?\n?", checksum.decode("ascii"))
    except UnicodeDecodeError as exc:
        raise AssetError("checksum is not ASCII") from exc
    if not match or match[2] != archive_name:
        raise AssetError("checksum must contain exactly the expected archive filename")
    if hashlib.sha256(archive).hexdigest() != match[1].lower():
        raise AssetError("archive bytes do not match their checksum")


class Release:
    def __init__(self, repo, tag):
        self.repo, self.tag = repo, tag

    def gh(self, *args):
        return subprocess.check_output(["gh", "release", *args, "--repo", self.repo])

    def existing(self, names):
        assets = json.loads(self.gh("view", self.tag, "--json", "assets"))["assets"]
        available = {asset["name"] for asset in assets}
        result = {}
        with tempfile.TemporaryDirectory() as directory:
            for name in names:
                if name in available:
                    self.gh("download", self.tag, "--pattern", name, "--dir", directory)
                    result[name] = (Path(directory) / name).read_bytes()
        return result

    def upload(self, name, content):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / name
            path.write_bytes(content)
            # No --clobber: a competing upload fails rather than replacing bytes.
            self.gh("upload", self.tag, str(path))


def complete(release, archive_name, checksum_name):
    existing = release.existing([archive_name, checksum_name])
    if len(existing) != 2:
        return False
    validate_pair(archive_name, existing[archive_name], existing[checksum_name])
    return True


def publish(release, archive_name, checksum_name, archive, checksum):
    validate_pair(archive_name, archive, checksum)
    desired = {archive_name: archive, checksum_name: checksum}
    existing = release.existing(desired)
    # Validate ALL existing files before uploading anything, including partial pairs.
    for name, data in existing.items():
        if hashlib.sha256(data).digest() != hashlib.sha256(desired[name]).digest():
            raise AssetError(f"{name} already exists with different bytes; refusing replacement")
    for name, data in desired.items():
        if name not in existing:
            release.upload(name, data)
    # Check published bytes as well, including a partial upload recovered by retry.
    actual = release.existing(desired)
    if actual != desired:
        raise AssetError("published raw assets do not match the verified local bytes")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check", "publish"))
    parser.add_argument("--repo", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--directory", type=Path, default=Path("."))
    args = parser.parse_args()
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z][0-9A-Za-z.-]*)?", args.tag):
        raise AssetError("expected a v<semver> tag")
    if args.target not in {"x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl",
                           "x86_64-apple-darwin", "aarch64-apple-darwin", "x86_64-pc-windows-msvc"}:
        raise AssetError("unsupported release target")
    base = f"knit-{args.tag}-{args.target}"
    archive_name = base + (".zip" if "windows" in args.target else ".tar.gz")
    checksum_name = base + ".sha256"
    release = Release(args.repo, args.tag)
    if args.command == "check":
        exists = complete(release, archive_name, checksum_name)
        print("Existing raw assets verified; skipping rebuild." if exists else
              "Raw asset pair incomplete; build locally, then compare before missing-only upload.")
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
            output.write(f"complete={str(exists).lower()}\n")
    else:
        publish(release, archive_name, checksum_name,
                (args.directory / archive_name).read_bytes(),
                (args.directory / checksum_name).read_bytes())


if __name__ == "__main__":
    try:
        main()
    except (AssetError, OSError, subprocess.CalledProcessError, ValueError) as error:
        raise SystemExit(f"::error::{error}")
