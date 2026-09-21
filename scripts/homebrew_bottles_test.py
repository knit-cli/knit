"""Tests for scripts/homebrew_bottles.py (stdlib unittest, no Rust build).

Fixtures are synthetic: tiny fake Mach-O/ELF binaries packed into raw release
archives with sidecar checksums, exactly matching the layout
.github/workflows/release.yml produces.
"""

import gzip
import io
import json
import os
import struct
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import homebrew_bottles as hb  # noqa: E402

VERSION = "0.0.0-test.7"
CPU_TYPE = {
    "aarch64-apple-darwin": hb.CPU_TYPE_ARM64,
    "x86_64-apple-darwin": hb.CPU_TYPE_X86_64,
}
EM = {"aarch64-unknown-linux-musl": hb.EM_AARCH64, "x86_64-unknown-linux-musl": hb.EM_X86_64}


def macho_binary(target, minos=None, platform=hb.PLATFORM_MACOS, commands=None):
    """Synthetic 64-bit Mach-O with a macOS LC_BUILD_VERSION load command.

    minos defaults mirror the real release binaries: ARM builds target
    macOS 11, Intel builds target macOS 10.12. Pass ``commands`` to craft
    malformed or missing-floor command tables.
    """
    payload = b"#!/knit-test-stub\n" + b"knit-test-stub " * 16
    if minos is None:
        minos = (11 << 16) if "aarch64" in target else (10 << 16) | (12 << 8)
    if commands is None:
        commands = struct.pack(
            "<IIIIII", hb.LC_BUILD_VERSION, 24, platform, minos, minos, 0
        )
    header = struct.pack(
        "<IIIIIIII",
        hb.MACHO_MAGIC_64,
        CPU_TYPE[target],
        0,
        2,
        1,
        len(commands),
        0,
        0,
    )
    return header + commands + payload


def fake_binary(target):
    payload = b"#!/knit-test-stub\n" + b"knit-test-stub " * 16
    if target.endswith("-apple-darwin"):
        return macho_binary(target)
    elf = bytearray(64)
    elf[0:4] = hb.ELF_MAGIC
    elf[4] = 2  # ELFCLASS64
    elf[5] = 1  # ELFDATA2LSB
    struct.pack_into("<H", elf, 0x10, 2)  # ET_EXEC
    struct.pack_into("<H", elf, 0x12, EM[target])
    struct.pack_into("<Q", elf, 0x20, 0)  # e_phoff: no program headers (static)
    struct.pack_into("<H", elf, 0x38, 0)  # e_phnum
    return bytes(elf) + payload


def write_raw_archive(assets_dir, target, binary=None, archive_name=None, sha_name=None):
    base = archive_name or "knit-v{}-{}".format(VERSION, target)
    blob = io.BytesIO()
    root = base
    with tarfile.open(fileobj=blob, mode="w:gz") as tar:
        info = tarfile.TarInfo(root + "/")
        info.type = tarfile.DIRTYPE
        tar.addfile(info)
        data = binary if binary is not None else fake_binary(target)
        info = tarfile.TarInfo(root + "/knit")
        info.size = len(data)
        info.mode = 0o755
        tar.addfile(info, io.BytesIO(data))
    archive_bytes = blob.getvalue()
    (assets_dir / (sha_name or base + ".sha256")).write_text(
        "{}  {}.tar.gz\n".format(hb.sha256_bytes(archive_bytes), base), encoding="utf-8"
    )
    (assets_dir / (base + ".tar.gz")).write_bytes(archive_bytes)


class Fixtures(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.assets = Path(self.tmp.name) / "assets"
        self.output = Path(self.tmp.name) / "out"
        self.assets.mkdir(parents=True)
        for target in hb.TARGETS:
            write_raw_archive(self.assets, target)

    def build(self, **overrides):
        return hb.package(
            overrides.pop("version", VERSION),
            overrides.pop("assets_dir", self.assets),
            overrides.pop("output_dir", self.output),
            overrides.pop("max_bytes", hb.DEFAULT_MAX_BINARY_BYTES),
        )


class BuildTest(Fixtures):
    def test_outputs_exist(self):
        manifest = self.build()
        self.assertEqual(sorted(manifest["bottles"]), sorted(hb.TAG_INFO))
        for tag, info in manifest["bottles"].items():
            self.assertEqual(
                info["file"], "knit-{}.{}.bottle.tar.gz".format(VERSION, tag)
            )
            path = self.output / "bottles" / info["file"]
            self.assertTrue(path.is_file())
            self.assertEqual(hb.sha256_bytes(path.read_bytes()), info["sha256"])
        for name in ("knit.rb", "SHA256SUMS", "manifest.json"):
            self.assertTrue((self.output / name).is_file())

    def test_bottle_tar_layout_and_metadata(self):
        self.build()
        for tag in hb.TAG_INFO:
            path = self.output / "bottles" / "knit-{}.{}.bottle.tar.gz".format(VERSION, tag)
            raw = path.read_bytes()
            self.assertEqual(raw[:3], b"\x1f\x8b\x08")
            self.assertEqual(raw[4:8], b"\x00\x00\x00\x00", "gzip mtime must be 0")
            with tarfile.open(fileobj=io.BytesIO(raw), mode="r:gz") as tar:
                members = tar.getmembers()
                names = [m.name for m in members]
                # Python's tarfile stores directory entries without a trailing
                # slash; brew's `resolve_version` splits on "/" and reads the
                # second component, so both forms work. The keg root must be
                # the first entry so brew can resolve name and version.
                self.assertEqual(names[0], "knit/{}".format(VERSION))
                self.assertEqual(
                    names,
                    sorted(names),
                    "entries must be sorted, keg root first",
                )
                self.assertEqual(
                    set(names),
                    {
                        "knit/{}".format(VERSION),
                        "knit/{}/.brew".format(VERSION),
                        "knit/{}/.brew/knit.rb".format(VERSION),
                        "knit/{}/INSTALL_RECEIPT.json".format(VERSION),
                        "knit/{}/bin".format(VERSION),
                        "knit/{}/bin/knit".format(VERSION),
                    },
                )
                for member in members:
                    self.assertEqual(member.uid, 0)
                    self.assertEqual(member.gid, 0)
                    self.assertEqual(member.mtime, 0)
                binary = tar.extractfile("knit/{}/bin/knit".format(VERSION)).read()
                tag_targets = {
                    "arm64_big_sur": "aarch64-apple-darwin",
                    "big_sur": "x86_64-apple-darwin",
                    "arm64_linux": "aarch64-unknown-linux-musl",
                    "x86_64_linux": "x86_64-unknown-linux-musl",
                }
                self.assertEqual(binary, fake_binary(tag_targets[tag]))
                self.assertEqual(
                    tar.getmember("knit/{}/bin/knit".format(VERSION)).mode, 0o555
                )

    def test_receipt_contents(self):
        self.build()
        for tag, (arch, _, prefix) in hb.TAG_INFO.items():
            path = self.output / "bottles" / "knit-{}.{}.bottle.tar.gz".format(VERSION, tag)
            with tarfile.open(fileobj=io.BytesIO(path.read_bytes()), mode="r:gz") as tar:
                tab = json.loads(
                    tar.extractfile(
                        "knit/{}/INSTALL_RECEIPT.json".format(VERSION)
                    ).read()
                )
            self.assertEqual(tab["built_as_bottle"], True)
            self.assertEqual(tab["poured_from_bottle"], False)
            self.assertEqual(tab["runtime_dependencies"], [])
            self.assertEqual(tab["compiler"], "clang")
            self.assertEqual(tab["changed_files"], [])
            self.assertEqual(tab["source_modified_time"], 0)
            self.assertEqual(tab["arch"], arch)
            self.assertEqual(tab["source"]["tap"], "knit-cli/tap")
            self.assertEqual(
                tab["source"]["versions"],
                {"stable": VERSION, "version_scheme": 0},
            )
            self.assertEqual(
                tab["source"]["path"],
                "{}/Cellar/knit/{}/.brew/knit.rb".format(prefix, VERSION),
            )
            # The tool never sees the build machine, so the receipt must not
            # fabricate build provenance.
            self.assertNotIn("built_on", tab)

    def test_complete_formula(self):
        manifest = self.build()
        formula = (self.output / "knit.rb").read_text(encoding="utf-8")
        self.assertIn('version "{}"'.format(VERSION), formula)
        self.assertIn(
            'root_url "https://github.com/knit-cli/knit/releases/download/v{}"'.format(
                VERSION
            ),
            formula,
        )
        for target, (tag, _, _) in hb.TARGETS.items():
            self.assertIn(
                "releases/download/v{}/knit-v{}-{}.tar.gz".format(VERSION, VERSION, target),
                formula,
            )
            self.assertIn(
                'sha256 "{}"'.format(manifest["raw"][target]["sha256"]), formula
            )
            self.assertIn(
                "sha256 cellar: :any_skip_relocation, {}: \"{}\"".format(
                    tag, manifest["bottles"][tag]["sha256"]
                ),
                formula,
            )
        self.assertEqual(formula.count("cellar: :any_skip_relocation"), 4)
        self.assertIn('bin.install "knit"', formula)

    def test_bottle_formula_is_source_only(self):
        self.build()
        complete = (self.output / "knit.rb").read_text(encoding="utf-8")
        for tag in hb.TAG_INFO:
            path = self.output / "bottles" / "knit-{}.{}.bottle.tar.gz".format(VERSION, tag)
            with tarfile.open(fileobj=io.BytesIO(path.read_bytes()), mode="r:gz") as tar:
                inside = tar.extractfile(
                    "knit/{}/.brew/knit.rb".format(VERSION)
                ).read().decode("utf-8")
            self.assertNotIn("bottle do", inside)
            self.assertNotIn("root_url", inside)
            # Same source blocks as the complete formula.
            for target, (_, _, _) in hb.TARGETS.items():
                self.assertIn(
                    "releases/download/v{}/knit-v{}-{}.tar.gz".format(
                        VERSION, VERSION, target
                    ),
                    inside,
                )
            self.assertIn('bin.install "knit"', inside)
        self.assertIn("bottle do", complete)

    def test_sha256sums_file(self):
        manifest = self.build()
        lines = (self.output / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(lines), 4)
        names = [line.split("  ")[1] for line in lines]
        self.assertEqual(names, sorted(names))
        for line in lines:
            digest, name = line.split("  ")
            blob = (self.output / "bottles" / name).read_bytes()
            self.assertEqual(hb.sha256_bytes(blob), digest)
            tag = name[
                len("knit-{}.".format(VERSION)) : -len(".bottle.tar.gz")
            ]
            self.assertEqual(digest, manifest["bottles"][tag]["sha256"])

    def test_deterministic_rebuild(self):
        self.build()
        first = {
            path.name: path.read_bytes()
            for path in sorted((self.output / "bottles").iterdir())
        }
        other = Path(self.tmp.name) / "out2"
        hb.package(VERSION, self.assets, other, hb.DEFAULT_MAX_BINARY_BYTES)
        for name, blob in first.items():
            self.assertEqual(blob, (other / "bottles" / name).read_bytes(), name)
        self.assertEqual(
            (self.output / "knit.rb").read_bytes(), (other / "knit.rb").read_bytes()
        )

    def test_cli_main(self):
        exit_code = hb.main(
            [
                "--version",
                VERSION,
                "--assets-dir",
                str(self.assets),
                "--output-dir",
                str(self.output),
            ]
        )
        self.assertEqual(exit_code, 0)
        self.assertTrue((self.output / "knit.rb").is_file())


class RejectionTest(Fixtures):
    def test_missing_asset(self):
        (self.assets / "knit-v{}-x86_64-apple-darwin.tar.gz".format(VERSION)).unlink()
        with self.assertRaises(hb.PackagingError):
            self.build()

    def test_checksum_mismatch_after_sidecar(self):
        path = self.assets / "knit-v{}-x86_64-apple-darwin.tar.gz".format(VERSION)
        path.write_bytes(path.read_bytes() + b"tamper")
        with self.assertRaises(hb.PackagingError):
            self.build()

    def test_sidecar_not_a_digest(self):
        path = self.assets / "knit-v{}-x86_64-apple-darwin.sha256".format(VERSION)
        path.write_text("not-a-digest  file\n", encoding="utf-8")
        with self.assertRaises(hb.PackagingError):
            self.build()

    def test_wrong_platform_binary(self):
        # Intel binary shipped under the arm64 macOS name.
        write_raw_archive(
            self.assets,
            "aarch64-apple-darwin",
            binary=fake_binary("x86_64-apple-darwin"),
        )
        with self.assertRaises(hb.PackagingError):
            self.build()

    def test_dynamic_elf_rejected(self):
        elf = bytearray(fake_binary("aarch64-unknown-linux-musl"))
        # Put one PT_INTERP program header right after the 64-byte ELF header.
        payload_len = len(elf) - 64
        struct.pack_into("<Q", elf, 0x20, 64)  # e_phoff
        struct.pack_into("<H", elf, 0x36, 56)  # e_phentsize
        struct.pack_into("<H", elf, 0x38, 1)  # e_phnum
        interp = struct.pack("<I", hb.PT_INTERP) + b"\x00" * 52
        elf = elf[:64] + interp + elf[64 + payload_len :]
        write_raw_archive(
            self.assets, "aarch64-unknown-linux-musl", binary=bytes(elf)
        )
        with self.assertRaises(hb.PackagingError):
            self.build()

    def test_symlinked_knit_rejected(self):
        base = "knit-v{}-aarch64-apple-darwin".format(VERSION)
        blob = io.BytesIO()
        with tarfile.open(fileobj=blob, mode="w:gz") as tar:
            info = tarfile.TarInfo(base + "/")
            info.type = tarfile.DIRTYPE
            tar.addfile(info)
            info = tarfile.TarInfo(base + "/knit")
            info.type = tarfile.SYMTYPE
            info.linkname = "/etc/passwd"
            tar.addfile(info)
        archive = blob.getvalue()
        (self.assets / (base + ".tar.gz")).write_bytes(archive)
        (self.assets / (base + ".sha256")).write_text(
            "{}  {}.tar.gz\n".format(hb.sha256_bytes(archive), base), encoding="utf-8"
        )
        with self.assertRaises(hb.PackagingError):
            self.build()

    def test_binary_size_cap(self):
        with self.assertRaises(hb.PackagingError):
            self.build(max_bytes=8)

    def test_non_positive_max_bytes_rejected(self):
        for bad in (0, -1):
            with self.assertRaises(hb.PackagingError):
                self.build(max_bytes=bad)
        self.assertFalse(self.output.exists(), "rejected input must write no output")

    def test_version_with_leading_v_rejected(self):
        with self.assertRaises(hb.PackagingError):
            self.build(version="v" + VERSION)
        self.assertFalse(self.output.exists(), "rejected version must write no output")

    def test_version_path_traversal_rejected(self):
        for bad in ("0.0.0/../../evil", "../0.0.0", "0.0.0/x"):
            with self.assertRaises(hb.PackagingError):
                self.build(version=bad)
        self.assertFalse(self.output.exists(), "rejected version must write no output")

    def test_version_ruby_injection_rejected(self):
        for bad in ('0.0.0-x"\n}', "0.0.0-#{version}", "0.0.0-test #"):
            with self.assertRaises(hb.PackagingError):
                self.build(version=bad)
        self.assertFalse(self.output.exists(), "rejected version must write no output")

    def test_macho_deployment_floor_above_big_sur_rejected(self):
        write_raw_archive(
            self.assets,
            "aarch64-apple-darwin",
            binary=macho_binary("aarch64-apple-darwin", minos=12 << 16),
        )
        with self.assertRaises(hb.PackagingError):
            self.build()

    def test_macho_unsupported_platform_rejected(self):
        write_raw_archive(
            self.assets,
            "aarch64-apple-darwin",
            binary=macho_binary("aarch64-apple-darwin", platform=2),  # iOS
        )
        with self.assertRaises(hb.PackagingError):
            self.build()

    def test_macho_short_build_version_command_rejected(self):
        minos = 11 << 16
        commands = struct.pack(
            "<IIIIII", hb.LC_BUILD_VERSION, 16, hb.PLATFORM_MACOS, minos, minos, 0
        )
        write_raw_archive(
            self.assets,
            "aarch64-apple-darwin",
            binary=macho_binary("aarch64-apple-darwin", commands=commands),
        )
        with self.assertRaises(hb.PackagingError):
            self.build()

    def test_macho_command_overrunning_table_rejected(self):
        minos = 11 << 16
        commands = struct.pack(
            "<IIIIII", hb.LC_BUILD_VERSION, 40, hb.PLATFORM_MACOS, minos, minos, 0
        )
        write_raw_archive(
            self.assets,
            "aarch64-apple-darwin",
            binary=macho_binary("aarch64-apple-darwin", commands=commands),
        )
        with self.assertRaises(hb.PackagingError):
            self.build()

    def test_macho_truncated_command_table_rejected(self):
        binary = bytearray(macho_binary("aarch64-apple-darwin"))
        struct.pack_into("<I", binary, 16, 2)  # ncmds=2, sizeofcmds only fits 1
        write_raw_archive(
            self.assets, "aarch64-apple-darwin", binary=bytes(binary)
        )
        with self.assertRaises(hb.PackagingError):
            self.build()

    def test_macho_missing_deployment_target_rejected(self):
        commands = struct.pack("<II", 0x1B, 16) + b"\x00" * 8  # LC_UUID only
        write_raw_archive(
            self.assets,
            "aarch64-apple-darwin",
            binary=macho_binary("aarch64-apple-darwin", commands=commands),
        )
        with self.assertRaises(hb.PackagingError):
            self.build()

    def test_big_endian_elf_rejected(self):
        elf = bytearray(fake_binary("x86_64-unknown-linux-musl"))
        elf[5] = 2  # ELFDATA2MSB
        write_raw_archive(
            self.assets, "x86_64-unknown-linux-musl", binary=bytes(elf)
        )
        with self.assertRaises(hb.PackagingError):
            self.build()

    def test_elf_small_phentsize_rejected(self):
        elf = bytearray(fake_binary("x86_64-unknown-linux-musl"))
        struct.pack_into("<Q", elf, 0x20, 64)  # e_phoff
        struct.pack_into("<H", elf, 0x36, 32)  # e_phentsize < sizeof(Elf64_Phdr)
        struct.pack_into("<H", elf, 0x38, 1)  # e_phnum
        write_raw_archive(
            self.assets, "x86_64-unknown-linux-musl", binary=bytes(elf)
        )
        with self.assertRaises(hb.PackagingError):
            self.build()

    def test_elf_truncated_phdr_table_rejected(self):
        elf = bytearray(fake_binary("x86_64-unknown-linux-musl"))
        struct.pack_into("<Q", elf, 0x20, 64)  # e_phoff
        struct.pack_into("<H", elf, 0x36, 56)  # e_phentsize
        struct.pack_into("<H", elf, 0x38, 8)  # table end far past the binary
        write_raw_archive(
            self.assets, "x86_64-unknown-linux-musl", binary=bytes(elf)
        )
        with self.assertRaises(hb.PackagingError):
            self.build()

    def test_cli_reports_error(self):
        (self.assets / "knit-v{}-x86_64-apple-darwin.tar.gz".format(VERSION)).unlink()
        code = hb.main(
            [
                "--version",
                VERSION,
                "--assets-dir",
                str(self.assets),
                "--output-dir",
                str(self.output),
            ]
        )
        self.assertEqual(code, 1)


class OutputHygieneTest(Fixtures):
    def test_stale_bottle_from_other_release_rejected(self):
        bottles = self.output / "bottles"
        bottles.mkdir(parents=True)
        (bottles / "knit-9.9.9-old.1.arm64_big_sur.bottle.tar.gz").write_bytes(
            b"stale"
        )
        with self.assertRaises(hb.PackagingError):
            self.build()
        self.assertEqual(
            (bottles / "knit-9.9.9-old.1.arm64_big_sur.bottle.tar.gz").read_bytes(),
            b"stale",
        )

    def test_same_version_rebuild_in_place_allowed(self):
        first = self.build()
        second = self.build()
        self.assertEqual(
            sorted(first["bottles"]), sorted(second["bottles"])
        )
        for tag, info in second["bottles"].items():
            self.assertEqual(
                info["sha256"], first["bottles"][tag]["sha256"], tag
            )
        expected = {info["file"] for info in second["bottles"].values()}
        self.assertEqual(
            {path.name for path in (self.output / "bottles").iterdir()}, expected
        )


if __name__ == "__main__":
    unittest.main()
