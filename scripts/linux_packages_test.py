"""Tests for scripts/linux_packages.py (stdlib unittest, no nfpm install).

Fixtures are synthetic: tiny fake static ELF64 binaries packed into raw
release archives with sidecar checksums, exactly matching the layout
.github/workflows/release.yml produces. nfpm is mocked at the module's
`run_nfpm` seam (or at subprocess.run for the wrapper's own tests).
"""

import io
import json
import struct
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import linux_packages as lp  # noqa: E402

VERSION = "0.0.0-test.7"
PKG_VERSION = "0.0.0~test.7"

EM = {
    "x86_64-unknown-linux-musl": lp.EM_X86_64,
    "aarch64-unknown-linux-musl": lp.EM_AARCH64,
}
ARCHS = {
    ("deb", "x86_64-unknown-linux-musl"): "amd64",
    ("deb", "aarch64-unknown-linux-musl"): "arm64",
    ("rpm", "x86_64-unknown-linux-musl"): "x86_64",
    ("rpm", "aarch64-unknown-linux-musl"): "aarch64",
}
REPO_ROOT = Path(lp.__file__).resolve().parent.parent


def fake_elf(target):
    """Synthetic little-endian static ELF64 for one release target."""
    payload = b"#!/knit-test-stub\n" + b"knit-test-stub " * 16
    elf = bytearray(64)
    elf[0:4] = lp.ELF_MAGIC
    elf[4] = 2  # ELFCLASS64
    elf[5] = 1  # ELFDATA2LSB
    struct.pack_into("<H", elf, 0x10, 2)  # ET_EXEC
    struct.pack_into("<H", elf, 0x12, EM[target])
    struct.pack_into("<Q", elf, 0x20, 0)  # e_phoff: no program headers (static)
    struct.pack_into("<H", elf, 0x38, 0)  # e_phnum
    return bytes(elf) + payload


def canonical_name(packager, arch, pkg_version=PKG_VERSION):
    if packager == "deb":
        return "knit_{}_{}.deb".format(pkg_version, arch)
    return "knit-{}-{}.{}.rpm".format(pkg_version, lp.RPM_RELEASE, arch)


def expected_names(pkg_version=PKG_VERSION):
    return {
        canonical_name(packager, arch, pkg_version)
        for (packager, _target), arch in ARCHS.items()
    }


def write_raw_archive(assets_dir, target, binary=None, archive_name=None, sha_name=None):
    base = archive_name or "knit-v{}-{}".format(VERSION, target)
    blob = io.BytesIO()
    with tarfile.open(fileobj=blob, mode="w:gz") as tar:
        info = tarfile.TarInfo(base + "/")
        info.type = tarfile.DIRTYPE
        tar.addfile(info)
        data = binary if binary is not None else fake_elf(target)
        info = tarfile.TarInfo(base + "/knit")
        info.size = len(data)
        info.mode = 0o755
        tar.addfile(info, io.BytesIO(data))
    archive_bytes = blob.getvalue()
    (assets_dir / (sha_name or base + ".sha256")).write_text(
        "{}  {}.tar.gz\n".format(lp.sha256_bytes(archive_bytes), base), encoding="utf-8"
    )
    (assets_dir / (base + ".tar.gz")).write_bytes(archive_bytes)


def fake_package_bytes(config, packager):
    """Deterministic stand-in package: depends on the config minus temp paths."""
    canonical = dict(config)
    canonical["contents"] = [
        {key: value for key, value in entry.items() if key != "src"}
        for entry in config["contents"]
    ]
    digest = lp.sha256_bytes(json.dumps(canonical, sort_keys=True).encode("utf-8"))
    return "fake-nfpm-package {} {}\n".format(packager, digest).encode("utf-8")


class FakeNfpm:
    """Replace lp.run_nfpm; records configs, writes one artifact per call."""

    def __init__(self, fail_on=(), extra_artifact=False):
        self.calls = []
        self.fail_on = set(fail_on)
        self.extra_artifact = extra_artifact

    def __call__(self, config_path, packager, target_dir):
        config = json.loads(Path(config_path).read_text(encoding="utf-8"))
        # Staged input files vanish with the temp dir once package() returns,
        # so capture their bytes now, while the call is live.
        self.calls.append(
            {
                "config": config,
                "packager": packager,
                "staged_binary": Path(config["contents"][0]["src"]).read_bytes(),
                "staged_license": Path(config["contents"][1]["src"]).read_bytes(),
            }
        )
        if packager in self.fail_on:
            raise lp.PackagingError("nfpm {} failed (exit 1):\nboom".format(packager))
        if packager == "deb":
            name = "{name}_{version}_{arch}.deb".format(**config)
        else:
            name = "{name}-{version}-{release}.{arch}.rpm".format(**config)
        (Path(target_dir) / name).write_bytes(fake_package_bytes(config, packager))
        if self.extra_artifact:
            (Path(target_dir) / ("extra." + packager)).write_bytes(b"stray")


# Version-ordering itself is validated with the real dpkg/rpm comparators by
# the coordinator's smoke workflow; here we only assert the tilde mapping.


class Fixtures(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.assets = Path(self.tmp.name) / "assets"
        self.output = Path(self.tmp.name) / "out"
        self.assets.mkdir(parents=True)
        for target in lp.TARGETS:
            write_raw_archive(self.assets, target)
        self.nfpm = FakeNfpm()
        self._real_run_nfpm = lp.run_nfpm
        lp.run_nfpm = self.nfpm
        self.addCleanup(setattr, lp, "run_nfpm", self._real_run_nfpm)

    def build(self, **overrides):
        return lp.package(
            overrides.pop("version", VERSION),
            overrides.pop("assets_dir", self.assets),
            overrides.pop("output_dir", self.output),
            overrides.pop("max_bytes", lp.DEFAULT_MAX_BINARY_BYTES),
        )

    def assertNoOutput(self):
        self.assertFalse(self.output.exists(), "rejected input must write no output")


class BuildTest(Fixtures):
    def test_outputs_exist(self):
        manifest = self.build()
        self.assertEqual(set(manifest["packages"]), expected_names())
        for name, info in manifest["packages"].items():
            path = self.output / name
            self.assertTrue(path.is_file(), name)
            blob = path.read_bytes()
            self.assertEqual(lp.sha256_bytes(blob), info["sha256"])
        for name in ("SHA256SUMS", "manifest.json"):
            self.assertTrue((self.output / name).is_file(), name)

    def test_package_filenames(self):
        manifest = self.build()
        self.assertEqual(
            manifest["packages"][canonical_name("deb", "amd64")]["target"],
            "x86_64-unknown-linux-musl",
        )
        self.assertEqual(
            manifest["packages"][canonical_name("deb", "arm64")]["target"],
            "aarch64-unknown-linux-musl",
        )
        self.assertEqual(
            manifest["packages"][canonical_name("rpm", "x86_64")]["target"],
            "x86_64-unknown-linux-musl",
        )
        self.assertEqual(
            manifest["packages"][canonical_name("rpm", "aarch64")]["target"],
            "aarch64-unknown-linux-musl",
        )

    def test_sha256sums_file(self):
        manifest = self.build()
        lines = (self.output / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(lines), 4)
        names = [line.split("  ")[1] for line in lines]
        self.assertEqual(names, sorted(names))
        self.assertEqual(set(names), expected_names())
        for line in lines:
            digest, name = line.split("  ")
            blob = (self.output / name).read_bytes()
            self.assertEqual(lp.sha256_bytes(blob), digest)
            self.assertEqual(digest, manifest["packages"][name]["sha256"])

    def test_manifest_records_version_and_raw_archives(self):
        manifest = self.build()
        self.assertEqual(manifest["version"], VERSION)
        self.assertEqual(manifest["package_version"], PKG_VERSION)
        self.assertEqual(manifest["mtime"], lp.FIXED_MTIME)
        for target in lp.TARGETS:
            entry = manifest["raw"][target]
            self.assertEqual(entry["archive"], "knit-v{}-{}.tar.gz".format(VERSION, target))
            sidecar = (self.assets / entry["archive"].replace(".tar.gz", ".sha256")).read_text()
            self.assertEqual(entry["sha256"], sidecar.split()[0])

    def test_nfpm_called_per_arch_and_packager(self):
        self.build()
        seen = {(call["packager"], call["config"]["arch"]) for call in self.nfpm.calls}
        self.assertEqual(
            seen,
            {(packager, arch) for (packager, _target), arch in ARCHS.items()},
        )

    def test_nfpm_config_metadata(self):
        self.build()
        by_packager = {}
        for call in self.nfpm.calls:
            by_packager[call["packager"]] = call["config"]
        for packager, config in by_packager.items():
            self.assertEqual(config["name"], "knit")
            self.assertEqual(config["version"], PKG_VERSION)
            self.assertEqual(config["version_schema"], "none")
            self.assertEqual(config["maintainer"], lp.MAINTAINER)
            self.assertEqual(config["homepage"], "https://github.com/knit-cli/knit")
            self.assertEqual(config["license"], "Apache-2.0")
            self.assertEqual(config["description"], lp.DESCRIPTION)
            self.assertEqual(config["platform"], "linux")
            self.assertEqual(config["mtime"], lp.FIXED_MTIME_RFC3339)
            binary_entry, license_entry = config["contents"]
            self.assertEqual(binary_entry["dst"], "/usr/bin/knit")
            self.assertEqual(binary_entry["file_info"]["mode"], 0o755)
            self.assertEqual(license_entry["file_info"]["mode"], 0o644)
        self.assertEqual(
            by_packager["deb"]["depends"],
            ["git (>= 1:2.31)", "ca-certificates"],
        )
        self.assertEqual(
            by_packager["rpm"]["depends"],
            ["git >= 2.31", "ca-certificates"],
        )
        self.assertNotIn("release", by_packager["deb"])
        self.assertEqual(by_packager["rpm"]["release"], lp.RPM_RELEASE)
        # RPM build-host metadata must be independent of the build machine.
        self.assertNotIn("rpm", by_packager["deb"])
        self.assertEqual(
            by_packager["rpm"]["rpm"], {"buildhost": lp.RPM_BUILDHOST}
        )
        self.assertEqual(
            by_packager["deb"]["contents"][1]["dst"], "/usr/share/doc/knit/LICENSE"
        )
        self.assertEqual(
            by_packager["rpm"]["contents"][1]["dst"], "/usr/share/licenses/knit/LICENSE"
        )

    def test_config_arch_matches_target(self):
        self.build()
        for call in self.nfpm.calls:
            # The staged binary file is named after its release target, so the
            # config's arch can be cross-checked against ARCHS.
            staged_name = Path(call["config"]["contents"][0]["src"]).name
            target = staged_name[len("knit-"):]
            self.assertEqual(
                call["config"]["arch"], ARCHS[(call["packager"], target)]
            )

    def test_verified_binary_is_packaged(self):
        self.build()
        by_target = {
            call["config"]["arch"]: call for call in self.nfpm.calls
        }
        for (_packager, target), arch in ARCHS.items():
            call = by_target[arch]
            self.assertEqual(call["staged_binary"], fake_elf(target), arch)

    def test_license_file_is_packaged(self):
        self.build()
        for call in self.nfpm.calls:
            self.assertEqual(
                call["staged_license"], (REPO_ROOT / "LICENSE").read_bytes()
            )

    def test_config_written_as_json(self):
        self.build()
        for call in self.nfpm.calls:
            # FakeNfpm already json.loads the config file; a parse failure
            # would have raised, proving the config is valid JSON.
            self.assertIn("name", call["config"])

    def test_deterministic_rebuild(self):
        self.build()
        first = {
            path.name: path.read_bytes()
            for path in sorted(self.output.iterdir())
            if path.suffix in (".deb", ".rpm")
        }
        other = Path(self.tmp.name) / "out2"
        lp.package(VERSION, self.assets, other, lp.DEFAULT_MAX_BINARY_BYTES)
        for name, blob in first.items():
            self.assertEqual(blob, (other / name).read_bytes(), name)
        self.assertEqual(
            (self.output / "SHA256SUMS").read_bytes(),
            (other / "SHA256SUMS").read_bytes(),
        )
        self.assertEqual(
            (self.output / "manifest.json").read_bytes(),
            (other / "manifest.json").read_bytes(),
        )

    def test_same_version_rebuild_in_place_allowed(self):
        first = self.build()
        second = self.build()
        for name, info in first["packages"].items():
            self.assertEqual(second["packages"][name]["sha256"], info["sha256"])
        self.assertEqual(
            {path.name for path in self.output.iterdir()},
            expected_names() | {"SHA256SUMS", "manifest.json"},
        )

    def test_leading_v_version_accepted(self):
        manifest = self.build(version="v" + VERSION)
        self.assertEqual(manifest["version"], VERSION)
        self.assertEqual(set(manifest["packages"]), expected_names())

    def test_cli_main(self):
        with mock.patch("sys.stdout", new=io.StringIO()) as stdout:
            exit_code = lp.main(
                [
                    "--version",
                    "v" + VERSION,
                    "--assets-dir",
                    str(self.assets),
                    "--output-dir",
                    str(self.output),
                ]
            )
        self.assertEqual(exit_code, 0)
        for name in sorted(expected_names()):
            self.assertIn("built {} (".format(name), stdout.getvalue())
            self.assertTrue((self.output / name).is_file())


class VersionTest(unittest.TestCase):
    def test_normalize_strips_one_leading_v(self):
        self.assertEqual(lp.normalize_version("v0.1.0"), "0.1.0")
        self.assertEqual(lp.normalize_version("v0.1.0-alpha.22"), "0.1.0-alpha.22")
        self.assertEqual(lp.normalize_version("0.1.0-alpha.22"), "0.1.0-alpha.22")

    def test_normalize_rejects_invalid_versions(self):
        for bad in (
            "",
            "v",
            "vv0.1.0",
            "0.1",
            "0.1.0-",
            "0.1.0-alpha.22-",
            "x0.1.0",
            "0.0.0/../../evil",
            "../0.0.0",
            "0.0.0/x",
            "0.0.0-x\"",
            "0.0.0-#{v}",
        ):
            with self.assertRaises(lp.PackagingError, msg=repr(bad)):
                lp.normalize_version(bad)

    def test_package_version_mapping(self):
        cases = {
            "0.1.0": "0.1.0",
            "1.2.3": "1.2.3",
            "0.1.0-alpha.22": "0.1.0~alpha.22",
            "0.0.0-test.7": "0.0.0~test.7",
            "1.0.0-rc.1": "1.0.0~rc.1",
        }
        for version, expected in cases.items():
            self.assertEqual(lp.package_version(version), expected, version)

    def test_prerelease_sorting_uses_generated_names(self):
        self.assertIn("~", canonical_name("deb", "amd64", lp.package_version("0.1.0-alpha.22")))
        self.assertNotIn(
            "~", canonical_name("deb", "amd64", lp.package_version("0.1.0"))
        )


class RejectionTest(Fixtures):
    def test_missing_asset(self):
        (self.assets / "knit-v{}-x86_64-unknown-linux-musl.tar.gz".format(VERSION)).unlink()
        with self.assertRaises(lp.PackagingError):
            self.build()
        self.assertNoOutput()
        self.assertEqual(self.nfpm.calls, [])

    def test_checksum_mismatch_runs_no_nfpm(self):
        path = self.assets / "knit-v{}-aarch64-unknown-linux-musl.tar.gz".format(VERSION)
        path.write_bytes(path.read_bytes() + b"tamper")
        with self.assertRaises(lp.PackagingError):
            self.build()
        self.assertNoOutput()
        self.assertEqual(self.nfpm.calls, [], "sidecar must gate before packaging")

    def test_sidecar_not_a_digest(self):
        path = self.assets / "knit-v{}-x86_64-unknown-linux-musl.sha256".format(VERSION)
        path.write_text("not-a-digest  file\n", encoding="utf-8")
        with self.assertRaises(lp.PackagingError):
            self.build()
        self.assertNoOutput()

    def test_sidecar_without_filename_rejected(self):
        path = self.assets / "knit-v{}-x86_64-unknown-linux-musl.sha256".format(VERSION)
        archive = self.assets / "knit-v{}-x86_64-unknown-linux-musl.tar.gz".format(VERSION)
        path.write_text(
            "{}\n".format(lp.sha256_bytes(archive.read_bytes())), encoding="utf-8"
        )
        with self.assertRaises(lp.PackagingError):
            self.build()
        self.assertNoOutput()

    def test_sidecar_naming_other_archive_rejected(self):
        path = self.assets / "knit-v{}-x86_64-unknown-linux-musl.sha256".format(VERSION)
        archive = self.assets / "knit-v{}-x86_64-unknown-linux-musl.tar.gz".format(VERSION)
        path.write_text(
            "{}  knit-v{}-aarch64-unknown-linux-musl.tar.gz\n".format(
                lp.sha256_bytes(archive.read_bytes()), VERSION
            ),
            encoding="utf-8",
        )
        with self.assertRaises(lp.PackagingError):
            self.build()
        self.assertNoOutput()

    def test_truncated_archive(self):
        base = "knit-v{}-x86_64-unknown-linux-musl".format(VERSION)
        broken = b"\x1f\x8b\x08\x00not-really-gzip"
        (self.assets / (base + ".tar.gz")).write_bytes(broken)
        (self.assets / (base + ".sha256")).write_text(
            "{}  {}.tar.gz\n".format(lp.sha256_bytes(broken), base), encoding="utf-8"
        )
        with self.assertRaises(lp.PackagingError):
            self.build()
        self.assertNoOutput()

    def test_symlinked_knit_rejected(self):
        base = "knit-v{}-x86_64-unknown-linux-musl".format(VERSION)
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
            "{}  {}.tar.gz\n".format(lp.sha256_bytes(archive), base), encoding="utf-8"
        )
        with self.assertRaises(lp.PackagingError):
            self.build()
        self.assertNoOutput()

    def test_hardlinked_knit_rejected(self):
        base = "knit-v{}-aarch64-unknown-linux-musl".format(VERSION)
        blob = io.BytesIO()
        with tarfile.open(fileobj=blob, mode="w:gz") as tar:
            info = tarfile.TarInfo(base + "/real-knit")
            info.size = 4
            tar.addfile(info, io.BytesIO(b"knit"))
            info = tarfile.TarInfo(base + "/knit")
            info.type = tarfile.LNKTYPE
            info.linkname = base + "/real-knit"
            tar.addfile(info)
        archive = blob.getvalue()
        (self.assets / (base + ".tar.gz")).write_bytes(archive)
        (self.assets / (base + ".sha256")).write_text(
            "{}  {}.tar.gz\n".format(lp.sha256_bytes(archive), base), encoding="utf-8"
        )
        with self.assertRaises(lp.PackagingError):
            self.build()
        self.assertNoOutput()

    def test_wrong_machine_elf_rejected(self):
        # x86-64 binary shipped under the aarch64 name.
        write_raw_archive(
            self.assets,
            "aarch64-unknown-linux-musl",
            binary=fake_elf("x86_64-unknown-linux-musl"),
        )
        with self.assertRaises(lp.PackagingError):
            self.build()
        self.assertNoOutput()

    def test_not_an_elf_rejected(self):
        write_raw_archive(
            self.assets,
            "x86_64-unknown-linux-musl",
            binary=b"#!/bin/sh\necho not knit\n",
        )
        with self.assertRaises(lp.PackagingError):
            self.build()
        self.assertNoOutput()

    def test_big_endian_elf_rejected(self):
        elf = bytearray(fake_elf("x86_64-unknown-linux-musl"))
        elf[5] = 2  # ELFDATA2MSB
        write_raw_archive(
            self.assets, "x86_64-unknown-linux-musl", binary=bytes(elf)
        )
        with self.assertRaises(lp.PackagingError):
            self.build()
        self.assertNoOutput()

    def test_binary_size_cap(self):
        with self.assertRaises(lp.PackagingError):
            self.build(max_bytes=8)
        self.assertNoOutput()

    def test_non_positive_max_bytes_rejected(self):
        for bad in (0, -1):
            with self.assertRaises(lp.PackagingError):
                self.build(max_bytes=bad)
        self.assertNoOutput()

    def test_invalid_version_rejected(self):
        for bad in ("v", "0.1.0-", "0.0.0/../../evil", "0.1.0-alpha.22-"):
            with self.assertRaises(lp.PackagingError):
                self.build(version=bad)
        self.assertNoOutput()

    def test_missing_license_rejected(self):
        with self.assertRaises(lp.PackagingError):
            lp.package(
                VERSION,
                self.assets,
                self.output,
                lp.DEFAULT_MAX_BINARY_BYTES,
                repo_root=self.tmp.name,
            )
        self.assertNoOutput()

    def test_stale_package_from_other_release_rejected(self):
        self.output.mkdir(parents=True)
        stale_deb = self.output / "knit_9.9.9_amd64.deb"
        stale_deb.write_bytes(b"stale")
        stale_rpm = self.output / "knit-9.9.9-1.x86_64.rpm"
        stale_rpm.write_bytes(b"stale")
        with self.assertRaises(lp.PackagingError):
            self.build()
        self.assertEqual(stale_deb.read_bytes(), b"stale")
        self.assertEqual(stale_rpm.read_bytes(), b"stale")
        self.assertEqual(self.nfpm.calls, [])

    def test_nfpm_failure_writes_no_packages(self):
        self.nfpm.fail_on = {"deb"}
        with self.assertRaises(lp.PackagingError):
            self.build()
        self.assertEqual(list(self.output.glob("*.deb")), [])
        self.assertFalse((self.output / "SHA256SUMS").exists())

    def test_nfpm_extra_artifact_rejected(self):
        self.nfpm.extra_artifact = True
        with self.assertRaises(lp.PackagingError):
            self.build()
        self.assertFalse((self.output / "SHA256SUMS").exists())

    def test_cli_reports_error(self):
        (self.assets / "knit-v{}-x86_64-unknown-linux-musl.tar.gz".format(VERSION)).unlink()
        code = lp.main(
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


class RunNfpmTest(unittest.TestCase):
    def test_command_arguments_and_env(self):
        captured = {}

        def fake_run(command, stdout, stderr, env=None):
            captured["command"] = command
            captured["env"] = env
            return mock.Mock(returncode=0, stdout=b"packaged!")

        with mock.patch.object(lp.subprocess, "run", side_effect=fake_run):
            lp.run_nfpm(Path("/tmp/cfg.json"), "deb", Path("/tmp/out"))
        self.assertEqual(
            captured["command"],
            [
                "nfpm",
                "package",
                "--config",
                "/tmp/cfg.json",
                "--packager",
                "deb",
                "--target",
                "/tmp/out",
            ],
        )
        self.assertEqual(captured["env"]["SOURCE_DATE_EPOCH"], str(lp.FIXED_MTIME))

    def test_source_date_epoch_overrides_host(self):
        captured = {}

        def fake_run(command, stdout, stderr, env=None):
            captured["env"] = env
            return mock.Mock(returncode=0, stdout=b"")

        with mock.patch.object(lp.subprocess, "run", side_effect=fake_run):
            with mock.patch.dict(lp.os.environ, {"SOURCE_DATE_EPOCH": "1"}):
                lp.run_nfpm(Path("/tmp/cfg.json"), "deb", Path("/tmp/out"))
        self.assertEqual(captured["env"]["SOURCE_DATE_EPOCH"], str(lp.FIXED_MTIME))

    def test_nonzero_exit_raises(self):
        result = mock.Mock(returncode=3, stdout=b"boom\n")
        with mock.patch.object(lp.subprocess, "run", return_value=result):
            with self.assertRaises(lp.PackagingError) as ctx:
                lp.run_nfpm(Path("/tmp/cfg.json"), "rpm", Path("/tmp/out"))
        self.assertIn("exit 3", str(ctx.exception))
        self.assertIn("boom", str(ctx.exception))

    def test_missing_binary_raises(self):
        with mock.patch.object(
            lp.subprocess, "run", side_effect=FileNotFoundError("nfpm")
        ):
            with self.assertRaises(lp.PackagingError) as ctx:
                lp.run_nfpm(Path("/tmp/cfg.json"), "deb", Path("/tmp/out"))
        self.assertIn("cannot run nfpm", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
