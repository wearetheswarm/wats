"""HTTPS_PROXY end to end, on loopback only: a CONNECT proxy and an HTTPS
server with a throwaway self-signed certificate for `wats.test` (made with
the openssl CLI; skipped without it). The client never resolves wats.test:
the proxy maps it to 127.0.0.1."""

import base64
import http.server
import os
import select
import socket
import ssl
import subprocess
import threading
import unittest

from helpers import LocalHTTPServer, tmpdir

from wats_wrapper import launcher
from wats_wrapper.launcher import WrapperError


def make_cert(test):
    d = tmpdir(test)
    key, cert = os.path.join(d, "key.pem"), os.path.join(d, "cert.pem")
    try:
        r = subprocess.run(
            ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", key, "-out", cert, "-subj", "/CN=wats.test",
             "-addext", "subjectAltName=DNS:wats.test", "-days", "1"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False,
        )
    except OSError:
        return None
    return (key, cert) if r.returncode == 0 else None


def start(server):
    t = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)
    t.start()
    return server.server_address[1]


class Proxy(unittest.TestCase):
    def test_downloads_go_through_an_https_proxy_tunnel_with_proxy_auth(self):
        tls = make_cert(self)
        if tls is None:
            self.skipTest("no openssl")
        key, cert = tls

        class Origin(http.server.BaseHTTPRequestHandler):
            def do_GET(self):  # noqa: N802
                body = f"hello {self.path}".encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        origin = LocalHTTPServer(("127.0.0.1", 0), Origin)
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(cert, key)
        origin.socket = ctx.wrap_socket(origin.socket, server_side=True)
        origin_port = start(origin)
        self.addCleanup(origin.server_close)
        self.addCleanup(origin.shutdown)

        connects = []

        class Tunnel(http.server.BaseHTTPRequestHandler):
            def do_CONNECT(self):  # noqa: N802
                connects.append((self.path, self.headers.get("Proxy-Authorization")))
                if self.path.startswith("refused.test"):
                    self.send_response(403)
                    self.end_headers()
                    return
                upstream = socket.create_connection(("127.0.0.1", origin_port))
                self.send_response(200, "Connection Established")
                self.end_headers()
                socks = [self.connection, upstream]
                try:
                    while True:
                        ready, _, _ = select.select(socks, [], [], 1)
                        if not ready:
                            break
                        for s in ready:
                            data = s.recv(65536)
                            if not data:
                                return
                            (upstream if s is self.connection else self.connection).sendall(data)
                finally:
                    upstream.close()

            def log_message(self, *args):
                pass

        proxy = LocalHTTPServer(("127.0.0.1", 0), Tunnel)
        proxy_port = start(proxy)
        self.addCleanup(proxy.server_close)
        self.addCleanup(proxy.shutdown)

        env = {"HTTPS_PROXY": f"http://u:p@127.0.0.1:{proxy_port}"}
        body = launcher.download(f"https://wats.test:{origin_port}/file", 100, env, cafile=cert)
        self.assertEqual(body, b"hello /file")
        self.assertEqual(
            connects, [(f"wats.test:{origin_port}", "Basic " + base64.b64encode(b"u:p").decode())]
        )

        # The certificate is checked inside the tunnel.
        with self.assertRaisesRegex(OSError, "(?i)certificate"):
            launcher.download(f"https://wats.test:{origin_port}/file", 100, env)
        # A proxy that refuses says so.
        with self.assertRaisesRegex(OSError, "403"):
            launcher.download("https://refused.test/file", 100, env, cafile=cert)
        # And a proxy that is not http:// is refused before anything.
        with self.assertRaisesRegex(WrapperError, "http:// proxy"):
            launcher.download("https://wats.test/file", 100, {"HTTPS_PROXY": "socks5://127.0.0.1:1"})


if __name__ == "__main__":
    unittest.main()
