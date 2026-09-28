#!/usr/bin/env python3
"""Build .deb and .rpm packages for a knit release with the nfpm CLI.

Inputs are the two Linux release archives (and their ``.sha256`` sidecars)
that `.github/workflows/release.yml` uploads to a GitHub release:

    knit-v<version>-<target>.tar.gz      knit-v<version>-<target>.sha256

with ``<target>`` one of aarch64-unknown-linux-musl, x86_64-unknown-linux-musl.
Every archive is checksum-verified against its sidecar before anything is
read from it, and only the single regular file named ``knit`` inside is read
(never extracted to disk; symlinks and other non-regular entries are
rejected). Each binary is structurally validated as a little-endian ELF64
whose machine matches the target, so asset mix-ups cannot become a
wrongly-tagged package.

Packaging is delegated to an installed `nfpm` CLI driven by generated JSON
configuration (nfpm parses JSON as a YAML superset). One .deb and one .rpm
are produced per target, installing the binary at /usr/bin/knit and the
repository LICENSE under the format's conventional license directory.

Pre-release versions are remapped with a tilde (`0.1.0-alpha.22` becomes
`0.1.0~alpha.22`) so they sort strictly before the final `0.1.0` release
under both the dpkg and rpm version algorithms. Output is deterministic:
the nfpm configuration declares a fixed mtime and rpm buildhost, and nfpm
runs with SOURCE_DATE_EPOCH pinned, so rebuilding identical inputs with the
same nfpm yields byte-identical packages.

Outputs written flat under ``--output-dir`` (ready for
scripts/build_apt_repository.sh and scripts/publish_linux_assets.sh); file
names spell the pre-release with a dash (GitHub normalizes '~' in asset
names to '.'), while the versions inside the packages keep the tilde:

    knit_<package-version>-spelled-with-dashes_<deb-arch>.deb     amd64, arm64
    knit-<package-version>-spelled-with-dashes-1.<rpm-arch>.rpm   x86_64, aarch64
    SHA256SUMS                                package digests
    manifest.json                             machine-readable build record

e.g. knit_0.1.0-alpha.22_amd64.deb and knit-0.1.0-alpha.22-1.x86_64.rpm for
release v0.1.0-alpha.22.

    usage: linux_packages.py --version v0.1.0-alpha.22 \
        --assets-dir DIR --output-dir DIR

The version accepts an optional leading ``v``. Python 3.9+, standard
library only; nfpm must be installed on PATH.
"""

import argparse
import hashlib
import io
import json
import os
import re
import struct
import subprocess
import sys
import tarfile
import tempfile
import zlib
from pathlib import Path

BIN = "knit"
MAINTAINER = "Knit maintainers <knit-cli@users.noreply.github.com>"
HOMEPAGE = "https://github.com/knit-cli/knit"
LICENSE_ID = "Apache-2.0"
DESCRIPTION = "Local-first CLI for coordinating cross-repo feature bundles"
GIT_MIN_VERSION = "2.31"
# Fixed modtime applied to every archived file so packages are reproducible.
# nfpm parses `mtime` as an RFC3339 timestamp; SOURCE_DATE_EPOCH carries the
# same instant as unix seconds for every other header nfpm or its writers
# might stamp.
FIXED_MTIME = 1704067200  # 2024-01-01T00:00:00Z
FIXED_MTIME_RFC3339 = "2024-01-01T00:00:00Z"
RPM_RELEASE = "1"
# nfpm stamps the build host's hostname into the rpm header; pin it so two
# containers (or two machines) produce byte-identical packages.
RPM_BUILDHOST = "knit-release"
DEFAULT_MAX_BINARY_BYTES = 256 * 1024 * 1024

# Raw release target -> package architectures per packager.
TARGETS = {
    "x86_64-unknown-linux-musl": {"deb": "amd64", "rpm": "x86_64"},
    "aarch64-unknown-linux-musl": {"deb": "arm64", "rpm": "aarch64"},
}
PACKAGERS = ("deb", "rpm")

ELF_MAGIC = b"\x7fELF"
EM_X86_64 = 62
EM_AARCH64 = 183

SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
VERSION_RE = re.compile(
    r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?"
)


class PackagingError(Exception):
    """Fatal input problem: fail the release rather than ship a bad package."""


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def read_sidecar_sha256(path, expected_name):
    """Return the digest from a `<digest>  <name>` sidecar, checking the name.

    The sidecar must name exactly the archive it accompanies; a digest for a
    different file is a release-asset mix-up, not a pass.
    """
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise PackagingError("cannot read checksum sidecar {}: {}".format(path, exc))
    fields = text.split()
    if len(fields) != 2 or not SHA256_RE.match(fields[0]):
        raise PackagingError(
            "checksum sidecar {} must hold '<sha256>  {}'".format(path, expected_name)
        )
    if fields[1] != expected_name:
        raise PackagingError(
            "checksum sidecar {} names {!r}, expected {!r}".format(
                path, fields[1], expected_name
            )
        )
    return fields[0]


def normalize_version(raw):
    """Validate the release version, accepting one optional leading ``v``."""
    version = raw[1:] if raw.startswith("v") else raw
    if not VERSION_RE.fullmatch(version):
        raise PackagingError(
            "invalid version {!r}: expected a semantic version like 0.1.0 or "
            "0.1.0-alpha.22, with an optional leading 'v'".format(raw)
        )
    return version


def package_version(version):
    """Map a semantic version to a deb/rpm version where pre-releases sort first.

    The first ``-`` becomes ``~``: under both the dpkg and rpm version
    algorithms a tilde sorts before the end of the version, so
    ``0.1.0~alpha.22 < 0.1.0`` while final versions pass through unchanged.
    """
    head, sep, tail = version.partition("-")
    return head + "~" + tail if sep else head


def validate_elf(data, target, source):
    """Reject non-ELF64 binaries and machines that do not match the target."""
    if len(data) < 64 or data[:4] != ELF_MAGIC:
        raise PackagingError("{}: expected an ELF64 binary".format(source))
    if data[4] != 2:
        raise PackagingError("{}: expected a 64-bit ELF binary".format(source))
    if data[5] != 1:
        raise PackagingError("{}: expected a little-endian ELF binary".format(source))
    (machine,) = struct.unpack_from("<H", data, 0x12)
    expected_machine = EM_AARCH64 if "aarch64" in target else EM_X86_64
    if machine != expected_machine:
        raise PackagingError(
            "{}: ELF machine {} does not match target {}".format(source, machine, target)
        )


def load_release_binary(assets_dir, version, target, max_bytes):
    """Verify one raw archive and return (binary bytes, archive sha256)."""
    base = "knit-v{}-{}".format(version, target)
    archive = assets_dir / (base + ".tar.gz")
    sidecar = assets_dir / (base + ".sha256")
    for path in (archive, sidecar):
        if not path.is_file():
            raise PackagingError("missing release asset {}".format(path))
    expected = read_sidecar_sha256(sidecar, archive.name)
    data = archive.read_bytes()
    actual = sha256_bytes(data)
    if actual != expected:
        raise PackagingError(
            "sha256 mismatch for {}: sidecar {}, file {}".format(
                archive.name, expected, actual
            )
        )
    try:
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as tar:
            members = [
                member
                for member in tar
                if member.name.count("/") <= 1
                and (member.name == BIN or member.name.endswith("/" + BIN))
            ]
            if len(members) != 1:
                raise PackagingError(
                    "expected exactly one '{}' entry in {}, found {}".format(
                        BIN, archive.name, len(members)
                    )
                )
            member = members[0]
            if not member.isreg():
                raise PackagingError(
                    "{} in {} is not a regular file".format(member.name, archive.name)
                )
            if member.size > max_bytes:
                raise PackagingError(
                    "{} in {} is {} bytes, over the {} byte cap".format(
                        member.name, archive.name, member.size, max_bytes
                    )
                )
            handle = tar.extractfile(member)
            if handle is None:
                raise PackagingError("cannot read {} from {}".format(member.name, archive.name))
            binary = handle.read(max_bytes + 1)
    except (tarfile.TarError, OSError, zlib.error) as exc:
        raise PackagingError("cannot open archive {}: {}".format(archive.name, exc))
    if len(binary) != member.size or len(binary) > max_bytes:
        raise PackagingError("truncated read of {} from {}".format(member.name, archive.name))
    validate_elf(binary, target, archive.name)
    return binary, actual


def dependencies(packager):
    """Runtime package dependencies, in each format's native syntax."""
    if packager == "deb":
        # Debian's git carries epoch 1 (e.g. 1:2.39.5); without the epoch a
        # bare >= 2.31 would also accept ancient 0:-epoch Git releases.
        return ["git (>= 1:{})".format(GIT_MIN_VERSION), "ca-certificates"]
    return ["git >= {}".format(GIT_MIN_VERSION), "ca-certificates"]


def license_destination(packager):
    if packager == "deb":
        return "/usr/share/doc/{}/LICENSE".format(BIN)
    return "/usr/share/licenses/{}/LICENSE".format(BIN)


def nfpm_config(pkg_version, packager, arch, binary_path, license_path):
    """Build the nfpm JSON configuration for one package."""
    config = {
        "name": BIN,
        "arch": arch,
        "platform": "linux",
        "version": pkg_version,
        # Keep the tilde pre-release version exactly as provided instead of
        # letting nfpm re-interpret it as strict semver.
        "version_schema": "none",
        "section": "default",
        "priority": "optional",
        "maintainer": MAINTAINER,
        "description": DESCRIPTION,
        "homepage": HOMEPAGE,
        "license": LICENSE_ID,
        "mtime": FIXED_MTIME_RFC3339,
        "depends": dependencies(packager),
        "contents": [
            {
                "src": str(binary_path),
                "dst": "/usr/bin/{}".format(BIN),
                "file_info": {"mode": 0o755},
            },
            {
                "src": str(license_path),
                "dst": license_destination(packager),
                "file_info": {"mode": 0o644},
            },
        ],
    }
    if packager == "rpm":
        config["release"] = RPM_RELEASE
        config["rpm"] = {"buildhost": RPM_BUILDHOST}
    return config


def canonical_package_name(packager, arch, pkg_version):
    # Output filenames spell the pre-release with '-': GitHub normalizes '~'
    # in asset names to '.', which would break checksum files and reupload
    # idempotence. The tilde stays inside package/config metadata, where the
    # dpkg/rpm version ordering needs it.
    file_version = pkg_version.replace("~", "-")
    if packager == "deb":
        return "{}_{}_{}.deb".format(BIN, file_version, arch)
    return "{}-{}-{}.{}.rpm".format(BIN, file_version, RPM_RELEASE, arch)


def nfpm_env():
    """Environment for nfpm with reproducibility pinned for all headers."""
    env = dict(os.environ)
    env["SOURCE_DATE_EPOCH"] = str(FIXED_MTIME)
    return env


def run_nfpm(config_path, packager, target_dir):
    """Run `nfpm package` for one config; raise PackagingError on failure."""
    command = [
        "nfpm",
        "package",
        "--config",
        str(config_path),
        "--packager",
        packager,
        "--target",
        str(target_dir),
    ]
    try:
        result = subprocess.run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            env=nfpm_env(),
        )
    except OSError as exc:
        raise PackagingError("cannot run nfpm (is it installed?): {}".format(exc))
    output = result.stdout.decode("utf-8", "replace").strip()
    if result.returncode != 0:
        raise PackagingError(
            "nfpm {} failed (exit {}):\n{}".format(packager, result.returncode, output)
        )
    return output


def package(raw_version, assets_dir, output_dir, max_bytes, repo_root=None):
    """Build all .deb/.rpm packages plus SHA256SUMS and manifest."""
    version = normalize_version(raw_version)
    if max_bytes <= 0:
        raise PackagingError("max_bytes must be positive, got {}".format(max_bytes))
    if repo_root is None:
        repo_root = Path(__file__).resolve().parent.parent
    license_path = Path(repo_root) / "LICENSE"
    if not license_path.is_file():
        raise PackagingError("missing LICENSE at {}".format(license_path))

    assets_dir = Path(assets_dir)
    output_dir = Path(output_dir)
    raw_shas = {}
    binaries = {}
    for target in TARGETS:
        binaries[target], raw_shas[target] = load_release_binary(
            assets_dir, version, target, max_bytes
        )

    pkg_version = package_version(version)
    expected_names = {
        canonical_package_name(packager, TARGETS[target][packager], pkg_version)
        for target in TARGETS
        for packager in PACKAGERS
    }
    output_dir.mkdir(parents=True, exist_ok=True)
    stale = sorted(
        path.name
        for pattern in ("*.deb", "*.rpm")
        for path in output_dir.glob(pattern)
        if path.name not in expected_names
    )
    if stale:
        raise PackagingError(
            "output directory already contains packages from another release "
            "({}); clean {} before rebuilding".format(", ".join(stale), output_dir)
        )

    packages = {}
    with tempfile.TemporaryDirectory(prefix="knit-linux-packages-") as staging:
        stage = Path(staging)
        staged_binaries = {}
        for target, binary in binaries.items():
            staged = stage / "knit-{}".format(target)
            staged.write_bytes(binary)
            staged_binaries[target] = staged
        for target in TARGETS:
            for packager in PACKAGERS:
                arch = TARGETS[target][packager]
                config = nfpm_config(
                    pkg_version, packager, arch, staged_binaries[target], license_path
                )
                config_path = stage / "nfpm-{}-{}.json".format(packager, arch)
                config_path.write_text(
                    json.dumps(config, indent=2, sort_keys=True) + "\n",
                    encoding="utf-8",
                )
                target_dir = stage / "built" / packager / arch
                target_dir.mkdir(parents=True)
                run_nfpm(config_path, packager, target_dir)
                artifacts = sorted(target_dir.glob("*." + packager))
                if len(artifacts) != 1:
                    raise PackagingError(
                        "expected exactly one {} artifact from nfpm for {}, "
                        "found {}: {}".format(
                            packager,
                            arch,
                            len(artifacts),
                            ", ".join(path.name for path in artifacts),
                        )
                    )
                blob = artifacts[0].read_bytes()
                name = canonical_package_name(packager, arch, pkg_version)
                (output_dir / name).write_bytes(blob)
                packages[name] = {
                    "packager": packager,
                    "arch": arch,
                    "target": target,
                    "sha256": sha256_bytes(blob),
                }

    (output_dir / "SHA256SUMS").write_text(
        "".join(
            "{}  {}\n".format(packages[name]["sha256"], name)
            for name in sorted(packages)
        ),
        encoding="utf-8",
    )
    manifest = {
        "version": version,
        "package_version": pkg_version,
        "mtime": FIXED_MTIME,
        "raw": {
            target: {
                "archive": "knit-v{}-{}.tar.gz".format(version, target),
                "sha256": raw_shas[target],
            }
            for target in sorted(TARGETS)
        },
        "packages": packages,
    }
    (output_dir / "manifest.json").write_text(
        json.dumps(manifest, sort_keys=True, indent=2) + "\n", encoding="utf-8"
    )
    return manifest


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Package knit Linux release archives into .deb and .rpm with nfpm."
    )
    parser.add_argument(
        "--version",
        required=True,
        help="release version, with or without a leading 'v' (e.g. v0.1.0-alpha.22)",
    )
    parser.add_argument(
        "--assets-dir", required=True, type=Path, help="directory holding the raw release assets"
    )
    parser.add_argument(
        "--output-dir", required=True, type=Path, help="directory to write packages, checksums, manifest"
    )
    parser.add_argument(
        "--max-binary-bytes",
        type=int,
        default=DEFAULT_MAX_BINARY_BYTES,
        help="reject binaries larger than this (default: %(default)s)",
    )
    args = parser.parse_args(argv)
    try:
        manifest = package(args.version, args.assets_dir, args.output_dir, args.max_binary_bytes)
    except PackagingError as exc:
        print("::error::linux packaging failed: {}".format(exc), file=sys.stderr)
        return 1
    for name in sorted(manifest["packages"]):
        info = manifest["packages"][name]
        print(
            "built {} ({} {}, sha256 {})".format(
                name, info["packager"], info["arch"], info["sha256"]
            )
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
