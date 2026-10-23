import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "src"))

from wats_discord import files  # noqa: E402

PNG = b"\x89PNG\r\n\x1a\n" + b"\x00" * 8


class FilesTest(unittest.TestCase):
    def test_kinds(self):
        self.assertEqual(files.image_kind(PNG), ("image/png", "png"))
        self.assertEqual(files.image_kind(b"\xff\xd8\xff\xe0" + b"\0" * 12), ("image/jpeg", "jpg"))
        self.assertEqual(files.image_kind(b"GIF89a" + b"\0" * 10), ("image/gif", "gif"))
        self.assertEqual(files.image_kind(b"RIFF\0\0\0\0WEBPVP8 "), ("image/webp", "webp"))
        self.assertIsNone(files.image_kind(b"MZ\x90\0" + b"\0" * 12))
        self.assertIsNone(files.image_kind(b"<svg xmlns=" + b"\0" * 5))

    def test_names(self):
        self.assertEqual(files.clean_name("logo.png", "png"), "logo.png")
        self.assertEqual(files.clean_name("../../evil name.exe", "png"), "evil-name.png")
        self.assertEqual(files.clean_name("", "gif"), "file.gif")
        self.assertEqual(files.clean_name(".png", "png"), "file.png")

    def test_check(self):
        with tempfile.TemporaryDirectory() as d:
            image = os.path.join(d, "a")
            with open(image, "wb") as f:
                f.write(PNG)
            self.assertEqual(files.check(image, "x.exe"), ("x.png", "image/png"))
            exe = os.path.join(d, "b")
            with open(exe, "wb") as f:
                f.write(b"MZ" + b"\0" * 30)
            with self.assertRaises(files.Refused):
                files.check(exe, "logo.png")
            code = os.path.join(d, "code")
            with open(code, "w", encoding="utf-8") as f:
                f.write("fn main() {\n    println!(\"héllo\");\n}\n")
            self.assertEqual(files.check(code, "main.rs"), ("main.rs", "text/plain"))
            self.assertEqual(files.check(code, "page.html"), ("page.txt", "text/plain"))
            self.assertEqual(files.check(code, "noext"), ("noext.txt", "text/plain"))
            zipped = os.path.join(d, "zip")
            with open(zipped, "wb") as f:
                f.write(b"PK\x03\x04" + bytes(range(256)) * 4)
            with self.assertRaises(files.Refused) as refused:
                files.check(zipped, "src.zip")
            self.assertIn("fetch_blobs", str(refused.exception))
            latin = os.path.join(d, "latin")
            with open(latin, "wb") as f:
                f.write("caf\xe9".encode("latin-1") * 10)
            with self.assertRaises(files.Refused):
                files.check(latin, "notes.txt")
            long_text = os.path.join(d, "long")
            with open(long_text, "w") as f:
                f.write("x" * (files.MAX_TEXT_BYTES + 1))
            with self.assertRaises(files.Refused):
                files.check(long_text, "big.txt")
            big = os.path.join(d, "c")
            with open(big, "wb") as f:
                f.write(PNG)
                f.truncate(files.MAX_BYTES + 1)
            with self.assertRaises(files.Refused):
                files.check(big, "big.png")
            with self.assertRaises(files.Refused):
                files.check(None, "logo.png")


if __name__ == "__main__":
    unittest.main()
