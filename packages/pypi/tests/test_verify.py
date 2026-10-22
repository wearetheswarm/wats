import os
import re
import unittest

from helpers import DARWIN, NAME, ROOT, TEST_KEY, VERSION, fixture, good_files

from wats_wrapper import verify
from wats_wrapper.verify import (
    RELEASE_PUBLIC_KEY,
    VerifyError,
    extract_binary,
    verify_release_files,
    verify_sha256,
    verify_signature,
)


def read(path):
    with open(path, "rb") as f:
        return f.read()


def check(files, name=NAME, key=TEST_KEY):
    verify_release_files(
        key, name, files["archive"], files["archive_sig"], files["sha256"], files["sha256_sig"]
    )


class Verify(unittest.TestCase):
    def test_the_pinned_key_is_the_release_key(self):
        rust = read(os.path.join(ROOT, "crates", "wats", "src", "update.rs")).decode()
        m = re.search(r'pub const RELEASE_PUBLIC_KEY: &str = "([^"]+)";', rust)
        self.assertIsNotNone(m)
        self.assertEqual(RELEASE_PUBLIC_KEY, m.group(1))
        install = read(os.path.join(ROOT, "install.sh")).decode()
        self.assertIn(f'PUBKEY="{RELEASE_PUBLIC_KEY}"', install)
        npm = read(os.path.join(ROOT, "packages", "npm", "lib", "verify.js")).decode()
        self.assertIn(f"const RELEASE_PUBLIC_KEY = '{RELEASE_PUBLIC_KEY}';", npm)

    def test_the_vendored_ed25519_is_the_tools_one(self):
        vendored = read(verify.ed25519_zip215.__file__)
        tools = read(os.path.join(ROOT, "tools", "verify-py", "ed25519_zip215.py"))
        self.assertEqual(vendored, tools, "copy tools/verify-py/ed25519_zip215.py again")

    def test_a_good_release_verifies_and_its_binary_comes_out(self):
        check(good_files())
        self.assertIn(b'echo "wats 9.9.9"', extract_binary(good_files()["archive"], NAME))

    def test_a_pax_archive_with_decoys_gives_the_binary_at_the_exact_path(self):
        name = f"wats-{VERSION}-{DARWIN}.tar.gz"
        check(good_files(name), name)
        binary = extract_binary(good_files(name)["archive"], name)
        self.assertIn(b"stand-in for the wats binary", binary)
        self.assertNotIn(b"echo decoy", binary)

    def test_the_rust_updater_fixtures_verify_the_same_way(self):
        d = os.path.join(ROOT, "crates", "wats", "tests", "fixtures", "upgrade")
        key = read(os.path.join(d, "test.pub")).decode().strip()
        files = {
            "archive": read(os.path.join(d, NAME)),
            "archive_sig": read(os.path.join(d, f"{NAME}.minisig")).decode(),
            "sha256": read(os.path.join(d, f"{NAME}.sha256")).decode(),
            "sha256_sig": read(os.path.join(d, f"{NAME}.sha256.minisig")).decode(),
        }
        check(files, key=key)
        with self.assertRaisesRegex(VerifyError, "made for another file"):
            verify_signature(key, files["archive"], read(os.path.join(d, f"{NAME}.wrong-file.minisig")).decode(), NAME)
        with self.assertRaisesRegex(VerifyError, "does not verify"):
            verify_signature(key, files["archive"], read(os.path.join(d, f"{NAME}.other-key.minisig")).decode(), NAME)

    def test_the_real_release_key_does_not_verify_the_fixtures(self):
        with self.assertRaisesRegex(VerifyError, "does not verify"):
            check(good_files(), key=RELEASE_PUBLIC_KEY)

    def test_a_signature_by_another_key_is_refused(self):
        f = good_files()
        f["archive_sig"] = fixture("other-key", f"{NAME}.minisig").decode()
        with self.assertRaisesRegex(VerifyError, "does not verify with the release key"):
            check(f)

    def test_a_tampered_archive_is_refused(self):
        f = good_files()
        f["archive"] = f["archive"][:-1] + bytes([f["archive"][-1] ^ 1])
        with self.assertRaisesRegex(VerifyError, "does not verify"):
            check(f)

    def test_a_tampered_trusted_comment_is_refused(self):
        f = good_files()
        f["archive_sig"] = f["archive_sig"].replace("timestamp:", "timestamp:1")
        with self.assertRaisesRegex(VerifyError, "does not verify"):
            check(f)

    def test_a_signature_made_for_another_file_name_is_refused(self):
        f = good_files()
        f["archive_sig"] = fixture("wrong-file", f"{NAME}.minisig").decode()
        with self.assertRaisesRegex(VerifyError, "made for another file"):
            check(f)

    def test_a_legacy_signature_is_refused(self):
        f = good_files()
        f["archive_sig"] = fixture("legacy", f"{NAME}.minisig").decode()
        with self.assertRaisesRegex(VerifyError, "legacy"):
            check(f)

    def test_a_signed_checksum_that_does_not_match_is_refused(self):
        f = good_files()
        f["sha256"] = fixture("bad-sha256", f"{NAME}.sha256").decode()
        f["sha256_sig"] = fixture("bad-sha256", f"{NAME}.sha256.minisig").decode()
        with self.assertRaisesRegex(VerifyError, "SHA-256 mismatch"):
            check(f)

    def test_a_checksum_of_another_file_or_garbage_is_refused(self):
        f = good_files()
        with self.assertRaisesRegex(VerifyError, "checksum of another file"):
            verify_sha256(f["archive"], f["sha256"], "other.tar.gz")
        with self.assertRaisesRegex(VerifyError, "not a checksum line"):
            verify_sha256(f["archive"], "", NAME)
        with self.assertRaisesRegex(VerifyError, "not a minisign"):
            verify_signature(TEST_KEY, f["archive"], "not a signature", NAME)
        with self.assertRaisesRegex(VerifyError, "public key"):
            verify_signature("RWnope", f["archive"], f["archive_sig"], NAME)

    def test_an_archive_without_the_binary_is_refused(self):
        with self.assertRaisesRegex(VerifyError, r"has no wats-1\.0\.0-x/wats"):
            extract_binary(good_files()["archive"], "wats-1.0.0-x.tar.gz")
        with self.assertRaisesRegex(VerifyError, "cannot read the archive"):
            extract_binary(b"nope", NAME)


if __name__ == "__main__":
    unittest.main()
