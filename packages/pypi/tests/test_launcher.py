import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time
import unittest

from helpers import (
    DARWIN,
    HERE,
    LINUX,
    LINUX_ARM,
    NAME,
    PKG,
    ROOT,
    TEST_KEY,
    VERSION,
    mirror,
    options,
    serve,
    tmpdir,
)

import wats_wrapper
from wats_wrapper import launcher
from wats_wrapper.launcher import WrapperError, ensure_binary

POSIX = os.name == "posix"
HARNESS = os.path.join(HERE, "harness.py")


def read(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


class Metadata(unittest.TestCase):
    def test_the_version_is_the_workspace_version(self):
        m = re.search(r'\[workspace\.package\][^\[]*?\nversion = "([^"]+)"', read(os.path.join(ROOT, "Cargo.toml")))
        self.assertIsNotNone(m)
        self.assertEqual(wats_wrapper.__version__, m.group(1))

    def test_the_package_name_is_one_constant(self):
        pyproject = read(os.path.join(PKG, "pyproject.toml"))
        self.assertIn(f'\nname = "{wats_wrapper.PACKAGE_NAME}"\n', pyproject)
        self.assertIn(f'\n{wats_wrapper.PACKAGE_NAME} = "wats_wrapper.launcher:main"\n', pyproject)

    def test_release_targets_match_the_release_workflow(self):
        self.assertEqual(launcher.release_target("Linux", "x86_64"), LINUX)
        self.assertEqual(launcher.release_target("Linux", "amd64"), LINUX)
        self.assertEqual(launcher.release_target("Darwin", "arm64"), DARWIN)
        self.assertEqual(launcher.release_target("Linux", "aarch64"), LINUX_ARM)
        self.assertEqual(launcher.release_target("Linux", "arm64"), LINUX_ARM)
        self.assertIsNone(launcher.release_target("Darwin", "x86_64"))
        self.assertIsNone(launcher.release_target("Windows", "AMD64"))
        workflow = read(os.path.join(ROOT, ".github", "workflows", "release.yml"))
        self.assertEqual(sorted(re.findall(r"- target: (\S+)", workflow)), sorted([DARWIN, LINUX, LINUX_ARM]))

    def test_the_cache_root_follows_the_platform(self):
        home = "/home/u"
        self.assertEqual(launcher.cache_root({}, "Linux", home), "/home/u/.cache/wats-wrapper")
        self.assertEqual(launcher.cache_root({"XDG_CACHE_HOME": "/x"}, "Linux", home), "/x/wats-wrapper")
        self.assertEqual(launcher.cache_root({"XDG_CACHE_HOME": "rel"}, "Linux", home), "/home/u/.cache/wats-wrapper")
        self.assertEqual(launcher.cache_root({}, "Darwin", home), "/home/u/Library/Caches/wats-wrapper")
        self.assertEqual(launcher.cache_root({"WATS_WRAPPER_CACHE_DIR": "/c"}, "Darwin", home), "/c")
        with self.assertRaisesRegex(WrapperError, "absolute"):
            launcher.cache_root({"WATS_WRAPPER_CACHE_DIR": "c"}, "Linux", home)

    def test_proxy_selection_honours_https_proxy_and_no_proxy(self):
        gh = "https://github.com/x"
        self.assertIsNone(launcher.proxy_for(gh, {}))
        self.assertEqual(launcher.proxy_for(gh, {"HTTPS_PROXY": "http://p:3128"}), "http://p:3128")
        self.assertEqual(launcher.proxy_for(gh, {"https_proxy": "p:3128"}), "http://p:3128")
        self.assertIsNone(launcher.proxy_for(gh, {"HTTPS_PROXY": "http://p", "NO_PROXY": ".github.com"}))
        self.assertIsNone(launcher.proxy_for("https://127.0.0.1/x", {"HTTPS_PROXY": "http://p"}))
        with self.assertRaisesRegex(WrapperError, "http:// proxy"):
            launcher.proxy_for(gh, {"HTTPS_PROXY": "socks5://p"})
        self.assertTrue(launcher.no_proxy("objects.github.com", "github.com"))
        self.assertTrue(launcher.no_proxy("anything", "*"))
        self.assertFalse(launcher.no_proxy("notgithub.com", "github.com"))

    def test_downloads_refuse_plain_http_off_loopback(self):
        with self.assertRaisesRegex(WrapperError, "HTTPS only"):
            launcher.download("http://example.invalid/x", 10, {})

    def test_plain_http_is_refused_without_the_tests_option_even_to_loopback(self):
        with self.assertRaisesRegex(WrapperError, "HTTPS only"):
            launcher.download("http://127.0.0.1:9/x", 10, {})

    def test_a_download_has_an_overall_deadline(self):
        import http.server
        import threading

        from helpers import LocalHTTPServer

        class Slow(http.server.BaseHTTPRequestHandler):
            def do_GET(self):  # noqa: N802
                self.send_response(200)
                self.end_headers()
                try:
                    for _ in range(200):
                        self.wfile.write(b"x" * 1024)
                        self.wfile.flush()
                        time.sleep(0.02)
                except OSError:
                    pass

            def log_message(self, *args):
                pass

        srv = LocalHTTPServer(("127.0.0.1", 0), Slow)
        threading.Thread(target=srv.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True).start()
        self.addCleanup(srv.server_close)
        self.addCleanup(srv.shutdown)
        url = f"http://127.0.0.1:{srv.server_address[1]}/slow"
        with self.assertRaisesRegex(WrapperError, "took longer than"):
            launcher.download(url, 10**7, {}, allow_loopback_http=True, deadline=0.3)

    def test_each_redirect_hop_gets_its_own_proxy_decision(self):
        srv = serve(self, mirror(self))
        # A proxy that does not exist, bypassed for the loopback hop: the
        # first hop (and the redirect) are direct, so this works.
        env = {"HTTPS_PROXY": "http://127.0.0.1:9", "NO_PROXY": "127.0.0.1"}
        body = launcher.download(f"{srv.url}/redirect/v{VERSION}/{NAME}.sha256", 1000, env, allow_loopback_http=True)
        self.assertIn(NAME.encode(), body)
        self.assertEqual(srv.requests, [f"/redirect/v{VERSION}/{NAME}.sha256", f"/v{VERSION}/{NAME}.sha256"])


class Cache(unittest.TestCase):
    def cache(self):
        return os.path.join(tmpdir(self), "cache")

    def test_an_unsupported_platform_says_how_to_install_otherwise(self):
        opts = options("http://127.0.0.1:9", self.cache(), target=None, system="Windows", machine="AMD64")
        with self.assertRaises(WrapperError) as e:
            ensure_binary(**opts)
        self.assertRegex(str(e.exception), "no prebuilt wats for Windows AMD64")
        self.assertRegex(str(e.exception), "cargo install")
        self.assertRegex(str(e.exception), "WSL")

    def test_a_good_release_is_downloaded_verified_cached_and_runs(self):
        srv = serve(self, mirror(self))
        cache = self.cache()
        binary = ensure_binary(**options(f"{srv.url}/redirect", cache))
        self.assertEqual(binary, os.path.join(cache, VERSION, LINUX, "wats"))
        self.assertEqual(len(srv.requests), 8, "four files, each through a redirect")
        if POSIX:
            self.assertEqual(os.stat(cache).st_mode & 0o777, 0o700)
            self.assertEqual(os.stat(os.path.join(cache, VERSION)).st_mode & 0o777, 0o700)
            self.assertEqual(os.stat(binary).st_mode & 0o777, 0o755)
        self.assertEqual([f for f in os.listdir(cache) if f.startswith(".")], [])
        out = subprocess.run([binary, "--version"], stdout=subprocess.PIPE, check=True).stdout
        self.assertEqual(out.decode().strip(), f"wats {VERSION}")

    def test_a_cache_hit_works_offline(self):
        srv = serve(self, mirror(self))
        cache = self.cache()
        ensure_binary(**options(srv.url, cache))
        srv.close()
        self.assertTrue(os.path.exists(ensure_binary(**options(srv.url, cache))))

    def test_a_corrupt_cache_is_downloaded_again_and_refused_offline(self):
        srv = serve(self, mirror(self))
        cache = self.cache()
        binary = ensure_binary(**options(srv.url, cache))
        with open(binary, "a", encoding="utf-8") as f:
            f.write("\necho tampered\n")
        logs = []
        self.assertEqual(ensure_binary(**options(srv.url, cache, log=logs.append)), binary)
        self.assertEqual(len(srv.requests), 8, "downloaded twice")
        self.assertTrue(any("differs from the signed archive" in m for m in logs), logs)
        self.assertNotIn("tampered", read(binary))

        sig = os.path.join(cache, VERSION, LINUX, f"{NAME}.minisig")
        text = read(sig).replace("timestamp:", "timestamp:9")
        with open(sig, "w", encoding="utf-8") as f:
            f.write(text)
        srv.close()
        with self.assertRaisesRegex(WrapperError, "cannot download.*Nothing was run"):
            ensure_binary(**options(srv.url, cache))
        self.assertFalse(os.path.exists(os.path.join(cache, VERSION, LINUX)), "the bad entry is gone")

    def refused(self, overrides, error):
        srv = serve(self, mirror(self, overrides))
        cache = self.cache()
        with self.assertRaisesRegex(WrapperError, error):
            ensure_binary(**options(srv.url, cache))
        self.assertFalse(os.path.exists(os.path.join(cache, VERSION)), "nothing cached")
        self.assertEqual(os.listdir(cache), [], "no staging directory left behind")

    def test_a_bad_signature_is_refused(self):
        self.refused({f"{NAME}.minisig": "other-key"}, "does not verify with the release key: refusing to run it")

    def test_a_wrong_trusted_comment_file_name_is_refused(self):
        self.refused({f"{NAME}.minisig": "wrong-file"}, "made for another file")

    def test_a_sha256_mismatch_is_refused(self):
        self.refused({f"{NAME}.sha256": "bad-sha256", f"{NAME}.sha256.minisig": "bad-sha256"}, "SHA-256 mismatch")

    def test_a_binary_of_another_version_is_refused(self):
        files = [NAME, f"{NAME}.minisig", f"{NAME}.sha256", f"{NAME}.sha256.minisig"]
        self.refused({f: "wrong-version" for f in files}, r"is not wats 9\.9\.9 \(it says: wats 9\.9\.8\)")

    def test_a_version_that_is_not_a_release_or_a_missing_release_is_refused(self):
        srv = serve(self, mirror(self))
        cache = self.cache()
        with self.assertRaisesRegex(WrapperError, "not a wats release"):
            ensure_binary(**options(srv.url, cache, version="9.9.9-rc.1"))
        with self.assertRaisesRegex(WrapperError, r"HTTP 404.*release v9\.9\.8 exists"):
            ensure_binary(**options(srv.url, cache, version="9.9.8"))

    @unittest.skipUnless(POSIX, "POSIX modes")
    def test_a_group_readable_cache_root_is_set_to_0700_and_kept(self):
        srv = serve(self, mirror(self))
        cache = self.cache()
        ensure_binary(**options(srv.url, cache))
        os.chmod(cache, 0o750)
        ensure_binary(**options(srv.url, cache))
        self.assertEqual(os.stat(cache).st_mode & 0o777, 0o700)
        self.assertEqual(len(srv.requests), 4, "the entry was kept")

    @unittest.skipUnless(POSIX, "POSIX modes")
    def test_a_cache_root_writable_by_others_is_emptied_and_downloaded_again(self):
        srv = serve(self, mirror(self))
        cache = self.cache()
        ensure_binary(**options(srv.url, cache))
        os.chmod(cache, 0o777)
        with open(os.path.join(cache, "planted"), "w", encoding="utf-8") as f:
            f.write("x")
        logs = []
        binary = ensure_binary(**options(srv.url, cache, log=logs.append))
        self.assertEqual(os.stat(cache).st_mode & 0o777, 0o700)
        self.assertFalse(os.path.exists(os.path.join(cache, "planted")))
        self.assertEqual(len(srv.requests), 8, "downloaded again")
        self.assertTrue(any("writable by group or others: emptying it" in m for m in logs), logs)
        self.assertTrue(os.path.exists(binary))

    @unittest.skipUnless(POSIX, "symlinks")
    def test_a_symlinked_cache_root_is_refused(self):
        srv = serve(self, mirror(self))
        d = tmpdir(self)
        os.mkdir(os.path.join(d, "elsewhere"), 0o700)
        os.symlink(os.path.join(d, "elsewhere"), os.path.join(d, "cache"))
        with self.assertRaisesRegex(WrapperError, "is a symlink"):
            ensure_binary(**options(srv.url, os.path.join(d, "cache")))
        self.assertEqual(srv.requests, [])

    @unittest.skipUnless(POSIX, "owners")
    def test_a_cache_root_of_another_user_is_refused(self):
        srv = serve(self, mirror(self))
        with self.assertRaisesRegex(WrapperError, "belongs to another user"):
            ensure_binary(**options(srv.url, self.cache(), uid=os.getuid() + 1))
        self.assertEqual(srv.requests, [])

    @unittest.skipUnless(POSIX, "POSIX modes and symlinks")
    def test_a_loose_or_symlinked_entry_directory_is_downloaded_again(self):
        srv = serve(self, mirror(self))
        cache = self.cache()
        binary = ensure_binary(**options(srv.url, cache))
        os.chmod(os.path.join(cache, VERSION), 0o777)
        ensure_binary(**options(srv.url, cache))
        self.assertEqual(os.stat(os.path.join(cache, VERSION)).st_mode & 0o777, 0o700)
        self.assertEqual(len(srv.requests), 8)

        copy = os.path.join(tmpdir(self), "copy")
        shutil.copytree(os.path.dirname(binary), copy)
        shutil.rmtree(os.path.dirname(binary))
        os.symlink(copy, os.path.dirname(binary))
        ensure_binary(**options(srv.url, cache))
        self.assertFalse(os.path.islink(os.path.dirname(binary)))
        self.assertEqual(len(srv.requests), 12)

    @unittest.skipUnless(POSIX, "owners")
    def test_an_entry_of_another_user_does_not_verify(self):
        srv = serve(self, mirror(self))
        binary = ensure_binary(**options(srv.url, self.cache()))
        with self.assertRaisesRegex(WrapperError, "belongs to another user"):
            launcher.verify_cached(os.path.dirname(binary), NAME, TEST_KEY, os.getuid() + 1)
        os.close(launcher.verify_cached(os.path.dirname(binary), NAME, TEST_KEY, os.getuid()).fd)

    @unittest.skipUnless(POSIX, "inodes")
    def test_a_binary_swapped_after_verification_is_not_run(self):
        srv = serve(self, mirror(self))
        binary = ensure_binary(**options(srv.url, self.cache()))
        handle = launcher.verify_cached(os.path.dirname(binary), NAME, TEST_KEY, os.getuid())
        self.addCleanup(os.close, handle.fd)
        launcher.check_same(handle)
        shutil.copy(binary, binary + ".other")
        os.rename(binary + ".other", binary)
        with self.assertRaisesRegex(WrapperError, "changed after it was verified"):
            launcher.check_same(handle)

    def test_stale_staging_and_trash_directories_are_cleaned_up(self):
        srv = serve(self, mirror(self))
        cache = self.cache()
        os.mkdir(cache, 0o700)
        old = time.time() - 2 * 3600
        for d in (".tmp-old", ".trash-old", ".tmp-new"):
            os.mkdir(os.path.join(cache, d))
        os.utime(os.path.join(cache, ".tmp-old"), (old, old))
        os.utime(os.path.join(cache, ".trash-old"), (old, old))
        ensure_binary(**options(srv.url, cache))
        self.assertEqual(sorted(f for f in os.listdir(cache) if f.startswith(".")), [".tmp-new"])

    def security(self, fixture):
        srv = serve(self, mirror(self, security=fixture))
        cache = self.cache()
        opts = options(srv.url, cache)
        ensure_binary(**opts)
        opts.pop("root")
        warning = launcher.security_warning(cache, **opts)
        before = len(srv.requests)
        self.assertEqual(launcher.security_warning(cache, **opts), warning, "cached, verified again")
        self.assertEqual(len(srv.requests), before, "at most once a day")
        return warning

    def test_a_version_below_the_security_floor_gets_a_warning(self):
        self.assertRegex(
            self.security("security-high"),
            r"SECURITY: wats 9\.9\.9 is older than 9\.9\.10.*Running 9\.9\.9 anyway",
        )

    def test_a_version_above_the_floor_or_a_bad_signature_gets_none(self):
        self.assertIsNone(self.security("security-low"))
        self.assertIsNone(self.security("security-other-key"))
        self.assertIsNone(self.security(None))


@unittest.skipUnless(POSIX, "exec and the shell-script stand-in need POSIX")
class Command(unittest.TestCase):
    def run_harness(self, args, input_text="", overrides=None, **kwargs):
        srv = serve(self, mirror(self, overrides))
        cache = os.path.join(tmpdir(self), "cache")
        opts = {
            "version": VERSION,
            "target": LINUX,
            "public_key": TEST_KEY,
            "base_url": srv.url,
            "latest_url": f"{srv.url}/latest",
            "allow_loopback_http": True,
            "root": cache,
        }
        env = {
            "PATH": os.environ.get("PATH", ""),
            "HOME": os.path.dirname(cache),
            "WATS_PYPI_TEST_OPTS": json.dumps(opts),
        }
        return subprocess.Popen(
            [sys.executable, "-I", HARNESS, *args],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            **kwargs,
        )

    def test_args_stdin_and_stdout_pass_through(self):
        p = self.run_harness(["echo-args", "mcp", "--profile", "a b"])
        out, err = p.communicate(b"", timeout=30)
        self.assertEqual(p.returncode, 0, err)
        self.assertEqual(out, b"[mcp]\n[--profile]\n[a b]\n")
        self.assertIn(b"verified wats 9.9.9", err)

        p = self.run_harness(["cat"])
        out, _ = p.communicate(b'{"jsonrpc":"2.0"}\n', timeout=30)
        self.assertEqual(out, b'{"jsonrpc":"2.0"}\n')

        p = self.run_harness(["env"])
        out, _ = p.communicate(b"", timeout=30)
        self.assertEqual(out, b"WATS_WRAPPER=pypi\n")

    def test_the_exit_status_is_propagated(self):
        p = self.run_harness(["exit", "7"])
        p.communicate(b"", timeout=30)
        self.assertEqual(p.returncode, 7)

    def test_a_failure_exits_1_with_a_message_and_runs_nothing(self):
        p = self.run_harness(["echo-args", "ran"], overrides={f"{NAME}.minisig": "other-key"})
        out, err = p.communicate(b"", timeout=30)
        self.assertEqual(p.returncode, 1)
        self.assertEqual(out, b"")
        self.assertRegex(err.decode(), r"(?m)^wats \(PyPI wrapper\): .*refusing to run it")

    def test_signals_reach_wats(self):
        p = self.run_harness(["wait-term"])
        self.assertEqual(p.stdout.readline(), b"ready\n")
        p.send_signal(signal.SIGTERM)
        out, _ = p.communicate(timeout=30)
        self.assertIn(b"got TERM", out)
        self.assertEqual(p.returncode, 42)


if __name__ == "__main__":
    unittest.main()
