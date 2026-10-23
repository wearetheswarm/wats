import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "src"))

from wats_discord import envfile  # noqa: E402


class EnvFileTest(unittest.TestCase):
    def test_parse(self):
        pairs = envfile.parse(
            "# comment\nDISCORD_TOKEN=abc.def\nexport GUILD_ID = '123'\n"
            'WATS_DIR="/x y"\nnot a line\n=nokey\n'
        )
        self.assertEqual(
            pairs, {"DISCORD_TOKEN": "abc.def", "GUILD_ID": "123", "WATS_DIR": "/x y"}
        )

    def test_load_never_overrides_and_needs_600(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, ".env")
            with open(path, "w") as f:
                f.write("DISCORD_TOKEN=t\nGUILD_ID=1\nEMPTY=\n")
            os.chmod(path, 0o600)
            env = {"GUILD_ID": "2"}
            self.assertEqual(str(envfile.load(path, env)), path)
            self.assertEqual(env, {"GUILD_ID": "2", "DISCORD_TOKEN": "t"})
            if os.name == "posix":
                os.chmod(path, 0o644)
                with self.assertRaises(envfile.EnvFileError):
                    envfile.load(path, {})
            self.assertIsNone(envfile.load(os.path.join(d, "none"), {}))


if __name__ == "__main__":
    unittest.main()
