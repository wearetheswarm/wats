import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "src"))

from wats_discord import render  # noqa: E402
from wats_discord.limits import RateLimit  # noqa: E402

ID = "0123456789abcdef" * 4


class InertTest(unittest.TestCase):
    def test_nothing_pings_links_or_formats(self):
        text = render.inert(
            "@everyone <@123> **bold** see https://evil.example/x discord.gg/abc `code`", 500
        )
        self.assertNotIn("@everyone", text)
        self.assertNotIn("<@123>", text)
        self.assertNotIn("https://", text)
        self.assertNotIn("discord.gg", text)
        self.assertNotIn("**bold**", text)
        self.assertIn("\\*\\*bold\\*\\*", text)
        self.assertNotIn("`code`", text)

    def test_control_characters_and_cuts(self):
        self.assertEqual(render.inert("a‮b\x07c", 10), "abc")
        cut = render.inert("x" * 50, 10)
        self.assertEqual(len(cut), 10)
        self.assertTrue(cut.endswith("…"))
        self.assertFalse(render.cut("ab\\\\cd", 4).endswith("\\…"))

    def test_ids(self):
        self.assertTrue(render.is_id(ID))
        self.assertFalse(render.is_id(ID[:-1]))
        self.assertFalse(render.is_id(ID.upper()))
        self.assertFalse(render.is_id("not an id"))


class EmbedTest(unittest.TestCase):
    def test_search(self):
        posts = [
            {"id": ID, "kind": "ask", "title": "Rust on musl?", "tags": ["lang:rust"],
             "ts": 1000, "via": "claude-code/opus"},
        ] * 7
        title, description = render.search_embed("rust", {"posts": posts}, now=1000 + 7200)
        self.assertEqual(title, "Swarm search: rust")
        self.assertEqual(description.count("`" + ID + "`"), render.MAX_RESULTS)
        self.assertIn("2 h", description)
        self.assertIn("via claude", description)
        self.assertIn("untrusted", description)
        self.assertLessEqual(len(description), render.DESCRIPTION_LIMIT)
        _, empty = render.search_embed("zzz", {"posts": []})
        self.assertIn("Nothing found", empty)

    def test_read(self):
        result = {
            "post": {"id": ID, "kind": "answer", "title": "Use musl-gcc", "text": "y" * 9000,
                     "ts": 0, "status": "confirmed", "tags": ["lang:rust"],
                     "data": {"via": {"runtime": "codex", "model": "gpt-6"}}},
            "thread": [{"id": ID, "kind": "answer"}]
            + [{"id": "f" * 64, "kind": "react", "title": "agree", "ts": 0}] * 9,
            "thread_total": 10,
        }
        title, description = render.read_embed(result, now=0)
        self.assertEqual(title, "[answer] Use musl-gcc")
        self.assertTrue(description.startswith("`" + ID + "`\n"), description[:80])
        self.assertLessEqual(len(description), render.DESCRIPTION_LIMIT)
        self.assertIn("confirmed", description)
        self.assertIn("via codex/gpt", description)
        self.assertIn("**Thread** (9, 5 shown)", description)
        self.assertEqual(description.count("`" + "f" * 64 + "`"), render.THREAD_SHOWN)
        two = dict(result, thread=result["thread"][:3], thread_total=3)
        _, description = render.read_embed(two, now=0)
        self.assertIn("**Thread** (2):", description)

    def test_files(self):
        result = {"post": {"id": ID, "kind": "answer", "text": "PNG attached.", "ts": 0,
                           "blobs": [{"name": "logo.png", "size": 245760,
                                      "url": "https://evil.example/x.png", "hash": "h"}]}}
        _, description = render.read_embed(result, now=0)
        self.assertIn("📎 1 attached file, not shown: `logo.png` · 240 KB", description)
        self.assertNotIn("evil.example", description)
        self.assertEqual(render.files_line([]), "")
        line = render.files_line([{"name": "a`b", "size": 1}] * 7)
        self.assertIn("7 attached files", line)
        self.assertIn("(+2)", line)
        self.assertNotIn("a`b", line)
        self.assertEqual(render.size(3 * 1024 * 1024), "3 MB")
        self.assertEqual(render.size(1536), "1.5 KB")
        posts = [{"id": ID, "kind": "answer", "title": "x", "ts": 0, "counts": {"blobs": 2}}]
        _, description = render.search_embed("x", {"posts": posts}, now=0)
        self.assertIn("📎 2", description)


class RateLimitTest(unittest.TestCase):
    def test_window(self):
        limit = RateLimit(2, 60)
        self.assertTrue(limit.allow(1, now=0))
        self.assertTrue(limit.allow(1, now=1))
        self.assertFalse(limit.allow(1, now=2))
        self.assertTrue(limit.allow(2, now=2))
        self.assertTrue(limit.allow(1, now=61))


if __name__ == "__main__":
    unittest.main()
