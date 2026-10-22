"""Shared by the tests: the signed fixtures (packages/fixtures, a throwaway
key, never the release key) and a local HTTP mirror serving them as
<base>/v<version>/<file>. Nothing here reaches the network."""

from __future__ import annotations

import http.server
import os
import shutil
import socketserver
import sys
import tempfile
import threading

HERE = os.path.dirname(os.path.abspath(__file__))
PKG = os.path.dirname(HERE)
ROOT = os.path.dirname(os.path.dirname(PKG))
FIX = os.path.join(ROOT, "packages", "fixtures")
SRC = os.path.join(PKG, "src")
if SRC not in sys.path:
    sys.path.insert(0, SRC)

VERSION = "9.9.9"
LINUX = "x86_64-unknown-linux-musl"
DARWIN = "aarch64-apple-darwin"
NAME = f"wats-{VERSION}-{LINUX}.tar.gz"

with open(os.path.join(FIX, "test.pub"), encoding="utf-8") as _f:
    TEST_KEY = _f.read().strip()


def fixture(directory: str, file: str) -> bytes:
    with open(os.path.join(FIX, directory, file), "rb") as f:
        return f.read()


def good_files(name: str = NAME, directory: str = "good") -> dict:
    return {
        "archive": fixture(directory, name),
        "archive_sig": fixture(directory, f"{name}.minisig").decode(),
        "sha256": fixture(directory, f"{name}.sha256").decode(),
        "sha256_sig": fixture(directory, f"{name}.sha256.minisig").decode(),
    }


def tmpdir(test) -> str:
    """A temporary directory removed after `test`."""
    d = tempfile.mkdtemp(prefix="wats-pypi-test-")
    test.addCleanup(shutil.rmtree, d, True)
    return d


def mirror(test, overrides=None) -> str:
    """A mirror holding the good release files of both targets, with the
    files of `overrides` ({file: fixture dir}) put over the Linux ones."""
    d = tmpdir(test)
    v = os.path.join(d, f"v{VERSION}")
    os.mkdir(v)
    for f in os.listdir(os.path.join(FIX, "good")):
        shutil.copy(os.path.join(FIX, "good", f), os.path.join(v, f))
    for f, src in (overrides or {}).items():
        shutil.copy(os.path.join(FIX, src, f), os.path.join(v, f))
    return d


class LocalHTTPServer(http.server.ThreadingHTTPServer):
    """http.server without its reverse DNS lookup of the address (getfqdn),
    which is slow, and a lookup all the same."""

    daemon_threads = True

    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]

    def handle_error(self, request, client_address):
        pass  # a client that hangs up (the refused-certificate case) is fine


class Server:
    """Serves a directory on 127.0.0.1; /redirect/<path> answers 302 to
    /<path>. `requests` lists the paths asked for."""

    def __init__(self, directory: str):
        self.requests = []
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):  # noqa: N802 (http.server's name)
                outer.requests.append(self.path)
                if self.path.startswith("/redirect/"):
                    self.send_response(302)
                    self.send_header("Location", self.path[len("/redirect"):])
                    self.end_headers()
                    return
                path = os.path.realpath(os.path.join(directory, self.path.lstrip("/")))
                if not path.startswith(os.path.realpath(directory)) or not os.path.isfile(path):
                    self.send_response(404)
                    self.end_headers()
                    return
                with open(path, "rb") as f:
                    body = f.read()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        self.httpd = LocalHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        self.thread = threading.Thread(target=self.httpd.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)
        self.thread.start()

    def close(self):
        if self.httpd is not None:
            self.httpd.shutdown()
            self.httpd.server_close()
            self.httpd = None


def serve(test, directory: str) -> Server:
    s = Server(directory)
    test.addCleanup(s.close)
    return s


def options(base_url: str, root: str, **extra) -> dict:
    """ensure_binary's arguments against a mirror, in a throwaway cache."""
    opts = {
        "version": VERSION,
        "target": LINUX,
        "public_key": TEST_KEY,
        "base_url": base_url,
        "root": root,
        "env": {"PATH": os.environ.get("PATH", ""), "HOME": os.path.dirname(root)},
        "log": lambda msg: None,
    }
    opts.update(extra)
    return opts
