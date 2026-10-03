import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from huahuo_character.cli import EXAMPLE, export_profiles, load_characters, main


class CliTests(unittest.TestCase):
    def test_offline_demo_serializes_two_scoped_characters_without_live_connection(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = main(["demo"])
        self.assertEqual(code, 0)
        data = json.loads(output.getvalue())
        self.assertEqual(data["backend"], "mock")
        self.assertFalse(data["live_game_connected"])
        self.assertEqual(len({value["character_id"] for value in data["utterances"]}), 2)
        for action in data["actions"]:
            self.assertEqual(action["conversation_id"], data["conversation_id"])
            self.assertEqual(action["turn_id"], data["turn_id"])

    def test_profile_export_creates_personas_and_refuses_to_replace_them(self):
        specs, _ = load_characters(EXAMPLE)
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "profiles"
            export_profiles(specs, destination)
            manifest = json.loads((destination / "manifest.json").read_text())
            self.assertFalse(manifest["live_profiles_modified"])
            files = [destination / spec.hermes_profile / "SOUL.md" for spec in specs]
            before = [path.read_bytes() for path in files]
            self.assertNotEqual(before[0], before[1])
            with self.assertRaises(FileExistsError):
                export_profiles(specs, destination)
            self.assertEqual([path.read_bytes() for path in files], before)

    def test_invalid_config_cannot_traverse_profile_output(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "characters.json"
            config.write_text(json.dumps({"characters": [{
                "character_id": "demo", "hermes_profile": "../escape", "persona": "demo"
            }]}))
            with self.assertRaises(ValueError):
                load_characters(config)

    def test_hermes_cli_requires_profile_token_map_without_printing_secrets(self):
        secret = "test-only-token-must-not-appear"
        error = io.StringIO()
        with patch.dict("os.environ", {"HUAHUO_HERMES_API_TOKENS": json.dumps({
            "huahuo-paimon": secret
        })}, clear=True), contextlib.redirect_stderr(error):
            code = main(["demo", "--backend", "hermes"])
        self.assertEqual(code, 1)
        self.assertNotIn(secret, error.getvalue())
        self.assertEqual(error.getvalue().strip(), "character runtime failed: ValueError")


if __name__ == "__main__":
    unittest.main()
