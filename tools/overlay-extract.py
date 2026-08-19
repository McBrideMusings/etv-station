#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyyaml>=6.0"]
# ///
"""Pull a channel's `overlay:` decl out of its channel.yaml as a standalone
OverlaySpec YAML that `etv-overlay run`/`watch`/`render-still` can load with
`--config`.

Every channel declares its overlay one of two ways (verified against
deploy/appdata/channels/*/channel.yaml and examples/channels/*.yaml, see
#316):
  overlay:
    file: "some/relative/path.yaml"     # resolved against the channel file's dir
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

Takes either shape a channel comes in: a directory holding `channel.yaml`
(deploy/appdata's per-channel layout) or a standalone channel YAML file
(examples/channels' flat layout).

Usage:
    tools/overlay-extract.py deploy/appdata/channels/054-dragon-ball --out /tmp/spec.yaml
    tools/overlay-extract.py examples/channels/diehard.yaml --out /tmp/spec.yaml
"""
import argparse
import sys
from pathlib import Path

import yaml


def extract(channel_path: Path) -> tuple[dict, Path]:
    """Return (resolved spec dict, the file it should be watched for edits)."""
    channel_yaml = channel_path / "channel.yaml" if channel_path.is_dir() else channel_path
    if not channel_yaml.exists():
        sys.exit(f"no channel config at {channel_yaml}")

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
        return absolutize(spec, ref.parent), ref

    return absolutize(decl, channel_yaml.parent), channel_yaml


def absolutize(spec: dict, base: Path) -> dict:
    """Rewrite the spec's relative asset paths to absolute, anchored at `base`.

    etv-overlay resolves `script:` and a layer's `path:` against the directory
    of the spec file it was handed. This tool hands it a *copy* written to a
    temp dir, so every relative path in an inline overlay (`path: logo.png` —
    the shape 60 of the 64 deployed channels use) resolved against the temp
    dir and failed to open. Anchoring them here to where they were authored
    means the copy names the same files the original did, rather than the
    caller having to keep the copy adjacent to the channel dir.
    """
    if not isinstance(spec, dict):
        return spec
    if isinstance(spec.get("script"), str):
        spec["script"] = str((base / spec["script"]).resolve())
    for layer in spec.get("layers") or []:
        if isinstance(layer, dict) and isinstance(layer.get("path"), str):
            layer["path"] = str((base / layer["path"]).resolve())
    return spec


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "channel_path",
        type=Path,
        help="a deploy/appdata/channels/<N>-<name> dir, or an examples/channels/<name>.yaml file",
    )
    parser.add_argument("--out", type=Path, required=True, help="where to write the spec YAML")
    args = parser.parse_args()

    spec, watch_target = extract(args.channel_path.resolve())
    args.out.write_text(yaml.safe_dump(spec, sort_keys=False))
    # Consumed by the calling shell script, which needs to know which file's
    # mtime to poll for edits — the inline channel.yaml, or the shared file
    # a `file:` ref points at.
    print(watch_target)


if __name__ == "__main__":
    main()
