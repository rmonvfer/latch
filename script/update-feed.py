"""Write a channel and architecture specific Sparkle appcast."""

import argparse
import base64
import datetime
import pathlib
import re
import xml.etree.ElementTree as ET

SPARKLE = "http://www.andymatuschak.org/xml-namespaces/sparkle"
REPOSITORY = "https://github.com/rmonvfer/latch"
ET.register_namespace("sparkle", SPARKLE)


def create_feed(
    channel, architecture, tag=None, build=None, archive=None, signature=None
):
    if channel not in ("production", "beta"):
        raise ValueError("Unknown release channel")
    if architecture not in ("arm64", "x86_64"):
        raise ValueError("Unknown macOS architecture")
    rss = ET.Element("rss", {"version": "2.0"})
    feed = ET.SubElement(rss, "channel")
    ET.SubElement(feed, "title").text = f"Latch {channel} ({architecture})"
    ET.SubElement(feed, "link").text = f"{REPOSITORY}/releases"
    ET.SubElement(feed, "description").text = "Latch for macOS"
    ET.SubElement(feed, "language").text = "en"
    if tag is None:
        return rss
    pattern = r"v(\d+\.\d+\.\d+)(-beta\.\d+)?"
    match = re.fullmatch(pattern, tag)
    if not match or bool(match[2]) != (channel == "beta"):
        raise ValueError("Release tag does not match its update channel")
    if not build or not re.fullmatch(r"[1-9][0-9]*", build):
        raise ValueError("Build number must be a positive integer")
    expected = f"Latch-{tag}-macos-{architecture}.zip"
    if archive is None or archive.name != expected or not archive.is_file():
        raise ValueError("Archive does not match its tag and architecture")
    if signature is None or len(base64.b64decode(signature, validate=True)) != 64:
        raise ValueError("Expected an Ed25519 archive signature")
    item = ET.SubElement(feed, "item")
    ET.SubElement(item, "title").text = f"Latch {tag.removeprefix('v')}"
    ET.SubElement(item, "pubDate").text = datetime.datetime.now(datetime.UTC).strftime(
        "%a, %d %b %Y %H:%M:%S GMT"
    )
    ET.SubElement(item, f"{{{SPARKLE}}}version").text = build
    ET.SubElement(item, f"{{{SPARKLE}}}shortVersionString").text = tag[1:]
    ET.SubElement(item, f"{{{SPARKLE}}}minimumSystemVersion").text = "13.0"
    ET.SubElement(
        item,
        "enclosure",
        {
            "url": f"{REPOSITORY}/releases/download/{tag}/{expected}",
            "length": str(archive.stat().st_size),
            "type": "application/octet-stream",
            f"{{{SPARKLE}}}edSignature": signature,
            f"{{{SPARKLE}}}os": "macos",
        },
    )
    return rss


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--channel", required=True)
    parser.add_argument("--architecture", required=True)
    parser.add_argument("--tag")
    parser.add_argument("--build")
    parser.add_argument("--archive", type=pathlib.Path)
    parser.add_argument("--signature")
    parser.add_argument("--output", required=True, type=pathlib.Path)
    args = parser.parse_args()
    try:
        feed = create_feed(
            args.channel,
            args.architecture,
            args.tag,
            args.build,
            args.archive,
            args.signature,
        )
    except (ValueError, OSError) as error:
        parser.error(str(error))
    ET.indent(feed)
    ET.ElementTree(feed).write(args.output, encoding="utf-8", xml_declaration=True)


if __name__ == "__main__":
    main()
