#!/usr/bin/env python3
"""Build Homebrew bottles and the tap formula for a knit release.

Inputs are the four raw release archives (and their ``.sha256`` sidecars) that
`.github/workflows/release.yml` uploads to a GitHub release:

    knit-v<version>-<target>.tar.gz      knit-v<version>-<target>.sha256

with ``<target>`` one of aarch64-apple-darwin, x86_64-apple-darwin,
aarch64-unknown-linux-musl, x86_64-unknown-linux-musl. Every archive is
checksum-verified before anything is read from it, and only the single
regular file named ``knit`` inside is read (never extracted to disk).
Each binary is structurally validated as well: a complete 64-bit Mach-O
load-command walk (macOS platform only, deployment target no newer than
the Big Sur bottle floor) or a little-endian ELF64 with a sane program
header table. The version must be a plain semantic version string, and a
nonempty output directory may not hold bottles from a different version.

Each archive becomes a real Homebrew bottle with the keg layout Homebrew
expects (first tar entry must be the keg root, so ``brew`` can resolve the
version from the file list):

    knit/<version>/                        keg root
    knit/<version>/.brew/knit.rb           source-only formula copy
    knit/<version>/INSTALL_RECEIPT.json    build receipt (Homebrew Tab)
    knit/<version>/bin/knit                the verified release binary

Bottles are fully relocatable static/standalone binaries, so the formula's
bottle block honestly declares ``cellar: :any_skip_relocation``. macOS tags
are pinned to the oldest verified compatible floor (Big Sur); Linux tags are
arch-only. Bottle file names use Homebrew's download URL format
(``knit-<version>.<tag>.bottle.tar.gz``, single dash).

Outputs written under ``--output-dir``:

    bottles/knit-<version>.<tag>.bottle.tar.gz   (one per target)
    knit.rb                                       complete tap formula
    SHA256SUMS                                    bottle digests
    manifest.json                                 machine-readable build record

Everything is deterministic: sorted tar entries, uid/gid 0, mtime 0, and a
gzip header with mtime 0, so rebuilding from identical inputs yields
byte-identical bottles. Python 3.9+, standard library only.
"""

import argparse
import gzip
import hashlib
import io
import json
import re
import struct
import sys
import tarfile
from pathlib import Path

BIN = "knit"
TAP_NAME = "knit-cli/tap"
TAP_REMOTE = "https://github.com/knit-cli/homebrew-tap"
RELEASE_URL = "https://github.com/knit-cli/knit/releases/download"
# Receipts written by a bottle tool identify the Homebrew whose Tab format they
# follow; pin it so the file is reproducible and parses on current brew.
RECEIPT_HOMEBREW_VERSION = "7.0.4"
DEFAULT_MAX_BINARY_BYTES = 256 * 1024 * 1024

# Raw release target -> (bottle tag, receipt arch, is_arm).
TARGETS = {
    "aarch64-apple-darwin": ("arm64_big_sur", "arm64", True),
    "x86_64-apple-darwin": ("big_sur", "x86_64", False),
    "aarch64-unknown-linux-musl": ("arm64_linux", "arm64", True),
    "x86_64-unknown-linux-musl": ("x86_64_linux", "x86_64", False),
}

# Bottle tag -> (receipt arch, is_arm, default Homebrew prefix). The prefix is
# used only for informational receipt paths.
TAG_INFO = {
    tag: (arch, is_arm, prefix)
    for tag, arch, is_arm, prefix in [
        ("arm64_big_sur", "arm64", True, "/opt/homebrew"),
        ("big_sur", "x86_64", False, "/usr/local"),
        ("arm64_linux", "arm64", True, "/home/linuxbrew/.linuxbrew"),
        ("x86_64_linux", "x86_64", False, "/home/linuxbrew/.linuxbrew"),
    ]
}

MACHO_MAGIC_64 = 0xFEEDFACF
MACHO_HEADER_SIZE = 32
CPU_TYPE_ARM64 = 0x0100000C
CPU_TYPE_X86_64 = 0x01000007
LC_BUILD_VERSION = 0x32
LC_VERSION_MIN_MACOSX = 0x24
PLATFORM_MACOS = 1
# Bottle tags claim Big Sur, so any encoded deployment target above macOS 11
# would ship a mis-tagged bottle.
MACOS_MINOS_FLOOR = 11 << 16
ELF_MAGIC = b"\x7fELF"
EM_AARCH64 = 183
EM_X86_64 = 62
PT_INTERP = 3
ELF_PHDR_MIN_SIZE = 56

SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
VERSION_RE = re.compile(
    r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?"
)


class PackagingError(Exception):
    """Fatal input problem: fail the release rather than ship a bad bottle."""


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def read_sidecar_sha256(path):
    """Return the hex digest recorded in a `<name>.sha256` sidecar asset."""
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise PackagingError("cannot read checksum sidecar {}: {}".format(path, exc))
    fields = text.split()
    if not fields or not SHA256_RE.match(fields[0]):
        raise PackagingError(
            "checksum sidecar {} does not start with a sha256 hex digest".format(path)
        )
    return fields[0]


def format_macos_minos(minos):
    return "{}.{}.{}".format(minos >> 16, (minos >> 8) & 0xFF, minos & 0xFF)


def validate_macos_binary(data, target, source):
    """Validate a 64-bit Mach-O and its macOS deployment-target floor.

    Walks the full load command table (bounded by ncmds/sizeofcmds) and
    requires at least one deployment target (LC_BUILD_VERSION on the macOS
    platform, or LC_VERSION_MIN_MACOSX) not newer than Big Sur, matching the
    bottle tags the formula declares.
    """
    if len(data) < MACHO_HEADER_SIZE:
        raise PackagingError(
            "{}: too short for a 64-bit Mach-O binary ({} bytes)".format(
                source, len(data)
            )
        )
    magic, cpu_type, _cpusubtype, _filetype, ncmds, sizeofcmds = struct.unpack_from(
        "<IIIIII", data, 0
    )
    expected_cpu = CPU_TYPE_ARM64 if "aarch64" in target else CPU_TYPE_X86_64
    if magic != MACHO_MAGIC_64:
        raise PackagingError(
            "{}: expected a 64-bit Mach-O binary, found magic {:#x}".format(
                source, magic
            )
        )
    if cpu_type != expected_cpu:
        raise PackagingError(
            "{}: Mach-O cputype {:#x} does not match target {}".format(
                source, cpu_type, target
            )
        )
    table_start = MACHO_HEADER_SIZE
    table_end = table_start + sizeofcmds
    if table_end > len(data) or ncmds * 8 > sizeofcmds:
        raise PackagingError(
            "{}: malformed Mach-O load command table (ncmds {}, sizeofcmds {})".format(
                source, ncmds, sizeofcmds
            )
        )
    floors = []
    offset = table_start
    for _ in range(ncmds):
        if offset + 8 > table_end:
            raise PackagingError(
                "{}: malformed Mach-O load command table (truncated)".format(source)
            )
        cmd, cmdsize = struct.unpack_from("<II", data, offset)
        if cmdsize < 8 or cmdsize % 4 != 0 or offset + cmdsize > table_end:
            raise PackagingError(
                "{}: malformed Mach-O load command {:#x} (cmdsize {})".format(
                    source, cmd, cmdsize
                )
            )
        if cmd == LC_BUILD_VERSION:
            if cmdsize < 24:
                raise PackagingError(
                    "{}: LC_BUILD_VERSION command too short (cmdsize {})".format(
                        source, cmdsize
                    )
                )
            platform, minos = struct.unpack_from("<II", data, offset + 8)
            if platform != PLATFORM_MACOS:
                raise PackagingError(
                    "{}: LC_BUILD_VERSION platform {} is not macOS ({})".format(
                        source, platform, PLATFORM_MACOS
                    )
                )
            if minos > MACOS_MINOS_FLOOR:
                raise PackagingError(
                    "{}: macOS deployment target {} is newer than the Big Sur "
                    "bottle floor".format(source, format_macos_minos(minos))
                )
            floors.append(minos)
        elif cmd == LC_VERSION_MIN_MACOSX:
            if cmdsize < 16:
                raise PackagingError(
                    "{}: LC_VERSION_MIN_MACOSX command too short (cmdsize {})".format(
                        source, cmdsize
                    )
                )
            (minos,) = struct.unpack_from("<I", data, offset + 8)
            if minos > MACOS_MINOS_FLOOR:
                raise PackagingError(
                    "{}: macOS deployment target {} is newer than the Big Sur "
                    "bottle floor".format(source, format_macos_minos(minos))
                )
            floors.append(minos)
        offset += cmdsize
    if not floors:
        raise PackagingError(
            "{}: no macOS deployment target load command "
            "(LC_BUILD_VERSION or LC_VERSION_MIN_MACOSX)".format(source)
        )


def validate_binary(data, target, source):
    """Reject binaries that do not match the target platform.

    Catches asset mix-ups (e.g. an Intel archive under the arm64 name) before
    they become a wrongly-tagged bottle. Linux targets must additionally be
    static (no PT_INTERP): the formula claims :any_skip_relocation, which is
    only honest for binaries that reference no interpreter path.
    """
    if target.endswith("-apple-darwin"):
        validate_macos_binary(data, target, source)
        return
    if data[:4] != ELF_MAGIC or len(data) < 64:
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
    phoff = struct.unpack_from("<Q", data, 0x20)[0]
    phentsize, phnum = struct.unpack_from("<HH", data, 0x36)
    if phnum > 0:
        if phentsize < ELF_PHDR_MIN_SIZE:
            raise PackagingError(
                "{}: ELF program header entry size {} is smaller than "
                "Elf64_Phdr ({})".format(source, phentsize, ELF_PHDR_MIN_SIZE)
            )
        if phoff + phnum * phentsize > len(data):
            raise PackagingError(
                "{}: ELF program header table extends past the end of the "
                "binary ({} headers at {:#x})".format(source, phnum, phoff)
            )
    for index in range(phnum):
        offset = phoff + index * phentsize
        if struct.unpack_from("<I", data, offset)[0] == PT_INTERP:
            raise PackagingError(
                "{}: ELF has a PT_INTERP segment (dynamic interpreter); "
                ":any_skip_relocation bottles must be static".format(source)
            )


def load_release_binary(assets_dir, version, target, max_bytes):
    """Verify one raw archive and return (binary bytes, archive sha256)."""
    base = "knit-v{}-{}".format(version, target)
    archive = assets_dir / (base + ".tar.gz")
    sidecar = assets_dir / (base + ".sha256")
    for path in (archive, sidecar):
        if not path.is_file():
            raise PackagingError("missing release asset {}".format(path))
    expected = read_sidecar_sha256(sidecar)
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
    except (tarfile.TarError, OSError) as exc:
        raise PackagingError("cannot open archive {}: {}".format(archive.name, exc))
    if len(binary) != member.size or len(binary) > max_bytes:
        raise PackagingError("truncated read of {} from {}".format(member.name, archive.name))
    validate_binary(binary, target, archive.name)
    return binary, actual


def receipt(version, tag):
    arch, _is_arm, prefix = TAG_INFO[tag]
    return {
        "homebrew_version": RECEIPT_HOMEBREW_VERSION,
        "used_options": [],
        "unused_options": [],
        "built_as_bottle": True,
        "poured_from_bottle": False,
        "loaded_from_api": False,
        "installed_as_dependency": False,
        "installed_on_request": True,
        "changed_files": [],
        "time": None,
        "source_modified_time": 0,
        "stdlib": None,
        "compiler": "clang",
        "aliases": [],
        "runtime_dependencies": [],
        "arch": arch,
        "source": {
            "spec": "stable",
            "version": version,
            "version_scheme": 0,
            "versions": {"stable": version, "version_scheme": 0},
            "revisions": [],
            "source_modified_time": 0,
            "path": "{}/Cellar/knit/{}/.brew/knit.rb".format(prefix, version),
            "tap": TAP_NAME,
            "tap_git_head": None,
            "tap_git_remote": TAP_REMOTE,
        },
    }


def formula(version, raw_shas, bottle_shas=None, root_url=None):
    """Render the tap formula; pass bottle_shas=None for the source-only copy."""
    lines = []
    if bottle_shas is None:
        lines.append(
            "# Source-only copy carried inside each bottle; the live formula in "
            "the tap adds the bottle block."
        )
    else:
        lines.append(
            "# Generated by scripts/homebrew_bottles.py from the v{} release "
            "assets. Do not edit by hand; regenerate instead.".format(version)
        )
    lines += [
        "class Knit < Formula",
        '  desc "Local-first CLI for coordinating cross-repo feature bundles"',
        '  homepage "https://github.com/knit-cli/knit"',
        '  version "{}"'.format(version),
        '  license "Apache-2.0"',
        "",
    ]
    platform_blocks = [
        ("on_macos do", ["aarch64-apple-darwin", "x86_64-apple-darwin"]),
        ("on_linux do", ["aarch64-unknown-linux-musl", "x86_64-unknown-linux-musl"]),
    ]
    for opener, targets in platform_blocks:
        lines.append("  " + opener)
        for index, target in enumerate(targets):
            arm = "aarch64" in target
            branch = "if Hardware::CPU.arm?" if index == 0 else "else"
            lines.append("    " + branch)
            lines.append(
                '      url "{}/v{}/knit-v{}-{}.tar.gz"'.format(
                    RELEASE_URL, version, version, target
                )
            )
            lines.append('      sha256 "{}"'.format(raw_shas[target]))
        lines.append("    end")
        lines.append("  end")
        lines.append("")
    if bottle_shas is not None:
        lines.append("  bottle do")
        lines.append('    root_url "{}"'.format(root_url))
        for target, (tag, _, _) in TARGETS.items():
            lines.append(
                "    sha256 cellar: :any_skip_relocation, {}: \"{}\"".format(
                    tag, bottle_shas[tag]
                )
            )
        lines.append("  end")
        lines.append("")
    lines += [
        "  def install",
        '    bin.install "{}"'.format(BIN),
        "  end",
        "",
        "  test do",
        '    assert_match "knit", shell_output("#{{bin}}/{} --version")'.format(BIN),
        "  end",
        "end",
    ]
    return "\n".join(lines) + "\n"


def build_bottle(version, tag, binary, source_only_formula):
    """Return deterministic bottle bytes for one target."""
    keg = "{}/{}".format(BIN, version)
    receipt_bytes = (
        json.dumps(receipt(version, tag), sort_keys=True, indent=2) + "\n"
    ).encode("utf-8")
    entries = [
        (keg + "/", None, 0o755),
        (keg + "/.brew/", None, 0o755),
        (keg + "/.brew/{}.rb".format(BIN), source_only_formula.encode("utf-8"), 0o644),
        (keg + "/INSTALL_RECEIPT.json", receipt_bytes, 0o644),
        (keg + "/bin/", None, 0o755),
        (keg + "/bin/{}".format(BIN), binary, 0o555),
    ]
    buffer = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=buffer, mtime=0) as gz:
        with tarfile.open(fileobj=gz, mode="w", format=tarfile.USTAR_FORMAT) as tar:
            for name, data, mode in entries:
                info = tarfile.TarInfo(name)
                info.uid = 0
                info.gid = 0
                info.uname = ""
                info.gname = ""
                info.mtime = 0
                info.mode = mode
                if data is None:
                    info.type = tarfile.DIRTYPE
                    info.size = 0
                    tar.addfile(info)
                else:
                    info.size = len(data)
                    tar.addfile(info, io.BytesIO(data))
    return buffer.getvalue()


def package(version, assets_dir, output_dir, max_bytes):
    """Build all bottles plus formula, SHA256SUMS, and manifest."""
    if not VERSION_RE.fullmatch(version):
        raise PackagingError(
            "invalid version {!r}: expected a semantic version like 0.1.0 or "
            "0.1.0-alpha.21, without a leading 'v'".format(version)
        )
    if max_bytes <= 0:
        raise PackagingError("max_bytes must be positive, got {}".format(max_bytes))
    assets_dir = Path(assets_dir)
    output_dir = Path(output_dir)
    bottles_dir = output_dir / "bottles"
    bottles_dir.mkdir(parents=True, exist_ok=True)
    expected_names = {
        "{}-{}.{}.bottle.tar.gz".format(BIN, version, tag)
        for tag, _, _ in TARGETS.values()
    }
    stale = sorted(
        path.name
        for path in bottles_dir.glob("*.bottle.tar.gz")
        if path.name not in expected_names
    )
    if stale:
        raise PackagingError(
            "output bottles directory already contains artifacts from another "
            "release ({}); clean {} before rebuilding".format(
                ", ".join(stale), bottles_dir
            )
        )

    raw_shas = {}
    binaries = {}
    for target in TARGETS:
        binaries[target], raw_shas[target] = load_release_binary(
            assets_dir, version, target, max_bytes
        )

    root_url = "{}/v{}".format(RELEASE_URL, version)
    source_only = formula(version, raw_shas)
    bottle_shas = {}
    bottle_names = {}
    for target, (tag, _, _) in TARGETS.items():
        name = "{}-{}.{}.bottle.tar.gz".format(BIN, version, tag)
        blob = build_bottle(version, tag, binaries[target], source_only)
        (bottles_dir / name).write_bytes(blob)
        bottle_shas[tag] = sha256_bytes(blob)
        bottle_names[tag] = name

    complete = formula(version, raw_shas, bottle_shas, root_url)
    (output_dir / "{}.rb".format(BIN)).write_text(complete, encoding="utf-8")
    (output_dir / "SHA256SUMS").write_text(
        "".join(
            "{}  {}\n".format(bottle_shas[tag], bottle_names[tag])
            for tag in sorted(bottle_names)
        ),
        encoding="utf-8",
    )
    manifest = {
        "version": version,
        "tap": TAP_NAME,
        "root_url": root_url,
        "raw": {
            target: {
                "archive": "knit-v{}-{}.tar.gz".format(version, target),
                "sha256": raw_shas[target],
            }
            for target in TARGETS
        },
        "bottles": {
            tag: {
                "file": bottle_names[tag],
                "sha256": bottle_shas[tag],
                "source_target": target,
            }
            for target, (tag, _, _) in TARGETS.items()
        },
    }
    (output_dir / "manifest.json").write_text(
        json.dumps(manifest, sort_keys=True, indent=2) + "\n", encoding="utf-8"
    )
    return manifest


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Package knit release archives into Homebrew bottles."
    )
    parser.add_argument(
        "--version", required=True, help="release version without leading 'v' (e.g. 0.1.0-alpha.21)"
    )
    parser.add_argument(
        "--assets-dir", required=True, type=Path, help="directory holding the raw release assets"
    )
    parser.add_argument(
        "--output-dir", required=True, type=Path, help="directory to write bottles, formula, checksums, manifest"
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
        print("::error::homebrew packaging failed: {}".format(exc), file=sys.stderr)
        return 1
    for tag in sorted(manifest["bottles"]):
        info = manifest["bottles"][tag]
        print("built {} (sha256 {})".format(info["file"], info["sha256"]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
