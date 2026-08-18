#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyyaml>=6.0"]
# ///
"""Pull a channel's `overlay:` decl out of its channel.yaml as a standalone
OverlaySpec YAML that `etv-overlay run`/`watch`/`render-still` can load with
`--config`.

Every channel here declares its overlay one of two ways (verified against
deploy/appdata/channels/*/channel.yaml, see #316):
  overlay:
    file: "some/relative/path.yaml"     # resolved against channel.yaml's dir
  overlay:
    width: ...
    height: ...
    ...                                  # written out inline

No channel here relies on the station -> channel -> block cascade
(config/overlay.rs's `resolve_decl`) — no station-level default, no
per-block override — so this file-vs-inline branch is the whole resolution,
not a partial reimplementation racing to drift from it. If a channel ever
needs the real cascade (a block override, a station default), extend
etv-station with a `--dump-overlay` mode that reuses `config::overlay`
directly instead of teaching this script the cascade.

Usage:
    tools/overlay-extract.py deploy/appdata/channels/054-dragon-ball --out /tmp/spec.yaml
"""
import argparse
import sys
from pathlib import Path

import yaml


def extract(channel_dir: Path) -> tuple[dict, Path]:
    """Return (resolved spec dict, the file it should be watched for edits)."""
    channel_yaml = channel_dir / "channel.yaml"
    if not channel_yaml.exists():
        sys.exit(f"no channel.yaml under {channel_dir}")

    doc = yaml.safe_load(channel_yaml.read_text())
    decl = doc.get("overlay")
    if decl is None:
        sys.exit(f"{channel_yaml} has no `overlay:` key")
    if decl == "clear":
        sys.exit(f"{channel_yaml} declares `overlay: clear` — nothing to preview")

    if "file" in decl:
        ref = (channel_yaml.parent / decl["file"]).resolve()
        if not ref.exists():
            sys.exit(f"{channel_yaml} overlay.file points at missing {ref}")
        spec = yaml.safe_load(ref.read_text())
        return spec, ref

    return decl, channel_yaml


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("channel_dir", type=Path, help="deploy/appdata/channels/<N>-<name>")
    parser.add_argument("--out", type=Path, required=True, help="where to write the spec YAML")
    args = parser.parse_args()

    spec, watch_target = extract(args.channel_dir.resolve())
    args.out.write_text(yaml.safe_dump(spec, sort_keys=False))
    # Consumed by the calling shell script, which needs to know which file's
    # mtime to poll for edits — the inline channel.yaml, or the shared file
    # a `file:` ref points at.
    print(watch_target)


if __name__ == "__main__":
    main()
