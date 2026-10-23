import asyncio
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "src"))

from wats_discord.mcp_client import McpClient, McpError  # noqa: E402

FAKE = [sys.executable, os.path.join(os.path.dirname(__file__), "fake_wats.py")]


class McpClientTest(unittest.TestCase):
    def test_calls_and_errors(self):
        async def run():
            client = McpClient(FAKE, timeout=10)
            try:
                found = await client.call("search", {"text": "rust", "limit": 5})
                self.assertEqual(found["posts"][0]["title"], "Hi")
                self.assertEqual(found["args"], {"text": "rust", "limit": 5})
                read = await client.call("read", {"id": "a" * 64})
                self.assertEqual(read["post"]["kind"], "answer")
                with self.assertRaises(McpError):
                    await client.call("nope", {})
                # The server exits: the call fails, the next one restarts it.
                with self.assertRaises(McpError):
                    await client.call("exit", {})
                again = await client.call("search", {"text": "x"})
                self.assertEqual(len(again["posts"]), 1)
            finally:
                await client.close()

        asyncio.run(run())


if __name__ == "__main__":
    unittest.main()


@unittest.skipUnless(os.environ.get("WATS_BIN"), "WATS_BIN: a wats binary to run against")
class RealWatsTest(unittest.TestCase):
    """Against `wats mcp --in-process`, hermetic, in a throwaway directory."""

    def test_search_and_a_missing_read(self):
        import tempfile

        async def run(base):
            env = dict(os.environ, WATS_HERMETIC="1", HOME=base, WATS_RUN_DIR=base + "/run")
            argv = [
                os.environ["WATS_BIN"], "--infra", "none", "--no-default-bootstrap",
                "--dir", base + "/data", "mcp", "--in-process", "--profile", "discord",
            ]
            client = McpClient(argv, env=env, timeout=60)
            try:
                found = await client.call("search", {"text": "rust", "limit": 5})
                self.assertEqual(found.get("posts"), [])
                with self.assertRaises(McpError):
                    await client.call("read", {"id": "0" * 64})
            finally:
                await client.close()

        with tempfile.TemporaryDirectory() as base:
            asyncio.run(run(base))
