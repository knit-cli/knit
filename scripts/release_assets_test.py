"""Raw release immutability, partial recovery, and upload-race regression tests."""
import hashlib
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import release_assets as assets


ARCHIVE = "knit-v1.2.3-x86_64-unknown-linux-musl.tar.gz"
SUM = "knit-v1.2.3-x86_64-unknown-linux-musl.sha256"
DATA = b"synthetic archive bytes"
CHECKSUM = f"{hashlib.sha256(DATA).hexdigest()}  {ARCHIVE}\n".encode()


class Store:
    def __init__(self, files=None, fail=None):
        self.files = dict(files or {})
        self.uploads = []
        self.fail = fail

    def existing(self, names):
        return {name: self.files[name] for name in names if name in self.files}

    def upload(self, name, data):
        if name == self.fail:
            raise RuntimeError("simulated interrupted upload")
        if name in self.files:
            raise RuntimeError("duplicate asset")
        self.uploads.append(name)
        self.files[name] = data


class ReleaseAssetTests(unittest.TestCase):
    def publish(self, store, archive=DATA, checksum=CHECKSUM):
        assets.publish(store, ARCHIVE, SUM, archive, checksum)

    def test_new_pair_published_and_verified(self):
        store = Store()
        self.publish(store)
        self.assertEqual(store.files, {ARCHIVE: DATA, SUM: CHECKSUM})
        self.assertTrue(assets.complete(store, ARCHIVE, SUM))

    def test_complete_pair_skips_build_and_upload(self):
        store = Store({ARCHIVE: DATA, SUM: CHECKSUM})
        self.assertTrue(assets.complete(store, ARCHIVE, SUM))
        self.publish(store)
        self.assertEqual(store.uploads, [])

    def test_existing_bad_checksum_fails_prebuild(self):
        store = Store({ARCHIVE: b"modified", SUM: CHECKSUM})
        with self.assertRaisesRegex(assets.AssetError, "do not match"):
            assets.complete(store, ARCHIVE, SUM)
        self.assertEqual(store.uploads, [])

    def test_partial_pairs_resume_only_missing_member(self):
        for existing, missing in [(ARCHIVE, SUM), (SUM, ARCHIVE)]:
            with self.subTest(existing=existing):
                pair = {ARCHIVE: DATA, SUM: CHECKSUM}
                store = Store({existing: pair[existing]})
                self.assertFalse(assets.complete(store, ARCHIVE, SUM))
                self.publish(store)
                self.assertEqual(store.uploads, [missing])
                self.assertEqual(store.files, pair)

    def test_partial_mismatch_never_uploads_other_member(self):
        for existing in [ARCHIVE, SUM]:
            with self.subTest(existing=existing):
                store = Store({existing: b"previous published bytes"})
                with self.assertRaisesRegex(assets.AssetError, "refusing replacement"):
                    self.publish(store)
                self.assertEqual(store.files, {existing: b"previous published bytes"})
                self.assertEqual(store.uploads, [])

    def test_complete_different_rebuild_refused(self):
        store = Store({ARCHIVE: DATA, SUM: CHECKSUM})
        new = b"different rebuild"
        checksum = f"{hashlib.sha256(new).hexdigest()}  {ARCHIVE}\n".encode()
        with self.assertRaisesRegex(assets.AssetError, "refusing replacement"):
            self.publish(store, new, checksum)
        self.assertEqual(store.files, {ARCHIVE: DATA, SUM: CHECKSUM})

    def test_failure_between_uploads_can_resume_same_bytes(self):
        store = Store(fail=SUM)
        with self.assertRaises(RuntimeError):
            self.publish(store)
        self.assertEqual(store.files, {ARCHIVE: DATA})
        store.fail = None
        self.publish(store)
        self.assertEqual(store.uploads, [ARCHIVE, SUM])

    def test_upload_race_never_replaces_winner(self):
        class RacingStore(Store):
            def upload(self, name, data):
                self.files[name] = b"concurrent winner"
                super().upload(name, data)
        store = RacingStore()
        with self.assertRaisesRegex(RuntimeError, "duplicate"):
            self.publish(store)
        self.assertEqual(store.files[ARCHIVE], b"concurrent winner")

    def test_local_bad_checksum_or_wrong_filename_rejected_before_upload(self):
        for checksum in [b"garbage", CHECKSUM.replace(ARCHIVE.encode(), b"other.tar.gz"),
                         CHECKSUM + CHECKSUM, b"0" * 64 + b"  " + ARCHIVE.encode() + b"\n"]:
            with self.subTest(checksum=checksum):
                store = Store()
                with self.assertRaises(assets.AssetError):
                    self.publish(store, checksum=checksum)
                self.assertEqual(store.uploads, [])

    def test_windows_checksum_format(self):
        name = "knit-v1.2.3-x86_64-pc-windows-msvc.zip"
        checksum = f"{hashlib.sha256(DATA).hexdigest()} *{name}\r\n".encode()
        assets.validate_pair(name, DATA, checksum)

    def test_post_upload_corruption_is_detected(self):
        class CorruptStore(Store):
            def upload(self, name, data):
                super().upload(name, b"corrupt")
        with self.assertRaisesRegex(assets.AssetError, "published raw assets"):
            self.publish(CorruptStore())

    @patch("release_assets.subprocess.check_output")
    def test_gh_upload_has_no_overwrite_or_delete(self, run):
        release = assets.Release("example/cli", "v1.2.3")
        release.upload(ARCHIVE, DATA)
        command = run.call_args.args[0]
        self.assertEqual(command[:4], ["gh", "release", "upload", "v1.2.3"])
        self.assertNotIn("--clobber", command)
        self.assertNotIn("delete", command)
        self.assertEqual(command[-2:], ["--repo", "example/cli"])

    @patch("release_assets.subprocess.check_output")
    def test_listing_failure_is_not_treated_as_no_assets(self, run):
        run.side_effect = subprocess.CalledProcessError(1, "gh")
        with self.assertRaises(subprocess.CalledProcessError):
            assets.complete(assets.Release("example/cli", "v1.2.3"), ARCHIVE, SUM)
        self.assertEqual(run.call_count, 1)


if __name__ == "__main__":
    unittest.main()
