import base64
import importlib.util
import pathlib
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    "update_feed", pathlib.Path(__file__).with_name("update-feed.py")
)
FEED = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FEED)


class UpdateFeedTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.archive = pathlib.Path(self.directory.name) / (
            "Latch-v1.2.3-macos-arm64.zip"
        )
        self.archive.write_bytes(b"signed archive contents")
        self.signature = base64.b64encode(bytes(64)).decode()

    def feed(self, **changes):
        arguments = {
            "channel": "production",
            "architecture": "arm64",
            "tag": "v1.2.3",
            "build": "42",
            "archive": self.archive,
            "signature": self.signature,
        }
        arguments.update(changes)
        return FEED.create_feed(**arguments)

    def test_exact_archive_url_size_and_signature(self):
        feed = self.feed()
        item = feed.find("channel/item")
        enclosure = item.find("enclosure")
        self.assertEqual(
            enclosure.get("url"),
            "https://github.com/rmonvfer/latch/releases/download/"
            "v1.2.3/Latch-v1.2.3-macos-arm64.zip",
        )
        self.assertEqual(enclosure.get("length"), str(self.archive.stat().st_size))
        self.assertEqual(
            enclosure.get(f"{{{FEED.SPARKLE}}}edSignature"), self.signature
        )
        self.assertEqual(item.findtext(f"{{{FEED.SPARKLE}}}version"), "42")
        self.assertEqual(
            item.findtext(f"{{{FEED.SPARKLE}}}minimumSystemVersion"), "13.0"
        )

    def test_beta_is_separate_from_production(self):
        beta_archive = self.archive.with_name("Latch-v1.2.3-beta.42-macos-arm64.zip")
        beta_archive.write_bytes(self.archive.read_bytes())
        self.assertIsNotNone(
            self.feed(channel="beta", tag="v1.2.3-beta.42", archive=beta_archive).find(
                "channel/item"
            )
        )
        with self.assertRaises(ValueError):
            self.feed(channel="beta")
        with self.assertRaises(ValueError):
            self.feed(tag="v1.2.3-beta.42", archive=beta_archive)

    def test_architecture_and_tag_cannot_change_archive(self):
        for changes in (
            {"architecture": "x86_64"},
            {"architecture": "../arm64"},
            {"tag": "../../archive"},
            {"channel": "nightly"},
            {"archive": self.archive.with_name("missing.zip")},
        ):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                self.feed(**changes)

    def test_missing_or_invalid_signature_and_build_are_rejected(self):
        for changes in (
            {"signature": None},
            {"signature": "invalid"},
            {"signature": base64.b64encode(bytes(32)).decode()},
            {"build": "0"},
            {"build": "-1"},
            {"build": None},
        ):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                self.feed(**changes)

    def test_bootstrap_feed_cannot_offer_an_update(self):
        for channel in ("production", "beta"):
            for architecture in ("arm64", "x86_64"):
                self.assertIsNone(
                    FEED.create_feed(channel, architecture).find("channel/item")
                )


if __name__ == "__main__":
    unittest.main()
