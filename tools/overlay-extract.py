#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyyaml>=6.0"]
# ///
"""Resolve a channel's overlay to a standalone OverlaySpec YAML that
`etv-overlay run`/`watch`/`render-still` can load with `--config`.

The resolution itself is done by `etv-station --dump-overlay`, not here. Since
ADR 0008 a channel may declare only `overlay: {extend: {layers: [...]}}`, which
carries no geometry and means nothing without the station-level spec above it —
so reading the channel file alone can no longer produce a spec. Rather than
teach this script the station -> channel -> block cascade (the drift this
file's previous version explicitly warned against), it shells out to the daemon
binary, which resolves it with `config::overlay`'s own functions.

Takes either shape a channel comes in: a directory holding `channel.yaml`
(deploy/appdata's per-channel layout) or a standalone channel YAML file
(examples/channels' flat layout).

Usage:
    tools/overlay-extract.py deploy/appdata/channels/054-dragon-ball --out /tmp/spec.yaml
    tools/overlay-extract.py examples/channels/diehard.yaml --out /tmp/spec.yaml
    tools/overlay-extract.py 030-comedy --out /tmp/spec.yaml --print-kind
"""
import argparse
import subprocess
import sys
from pathlib import Path

import yaml

REPO = Path(__file__).resolve().parent.parent
STATION_BINARY = REPO / "target/debug/etv-station"

# Which station config a channel path belongs to. deploy/appdata's channels sit
# under the station file's own directory; examples' flat channels belong to the
# dev station.
STATIONS = [
    (REPO / "deploy/appdata/channels", REPO / "deploy/appdata/station.yaml"),
    (REPO / "examples/channels", REPO / "examples/station.yaml"),
    (REPO / "examples/samples", REPO / "examples/station.yaml"),
]


def channel_identity(channel_path: Path) -> tuple[str, Path]:
    """Return (name the station knows this channel by, its station config)."""
    resolved = channel_path.resolve()
    name = resolved.stem if resolved.is_file() else resolved.name
    for root, station in STATIONS:
        if resolved == root or root in resolved.parents:
            return name, station
    sys.exit(
        f"{channel_path} is not under a known station: "
        + ", ".join(str(r) for r, _ in STATIONS)
    )


def dump_spec(name: str, station: Path) -> dict:
    if not STATION_BINARY.exists():
        sys.exit(
            f"{STATION_BINARY} not built — run `cargo build -p etv-station` first "
            "(tools/overlay-watch.sh does this for you)"
        )
    proc = subprocess.run(
        [str(STATION_BINARY), "--config", str(station), "--dump-overlay", name],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        sys.exit(proc.stderr.strip() or f"could not resolve an overlay for {name}")
    return yaml.safe_load(proc.stdout)


def override_config(spec: dict, assignments: list[str]) -> dict:
    """Merge `KEY=VALUE` assignments into the spec's free-form `config:` map.

    The map is handed to the Rhai script unread, so this needs no knowledge of
    which keys any script accepts. VALUE goes through the YAML loader, so `12`
    arrives as an int and `false` as a bool — a script comparing against a
    number would silently take its default if this passed strings through.
    """
    if not assignments:
        return spec
    config = dict(spec.get("config") or {})
    for pair in assignments:
        if "=" not in pair:
            sys.exit(f"--set-config expects KEY=VALUE, got {pair!r}")
        key, _, raw = pair.partition("=")
        config[key.strip()] = yaml.safe_load(raw)
    spec["config"] = config
    return spec


def guess_kind(channel_path: Path) -> str:
    """"series" or "film", from what the channel's blocks actually schedule.

    A preview has no station running, so nothing supplies real program
    metadata and the stand-in has to be guessed. `program.categories` is the
    channel author's own statement of what it airs, which is the closest thing
    to an answer that exists without generating a playout.
    """
    resolved = channel_path.resolve()
    yaml_path = resolved / "channel.yaml" if resolved.is_dir() else resolved
    try:
        doc = yaml.safe_load(yaml_path.read_text()) or {}
    except OSError:
        return "film"
    blocks = ((doc.get("rule") or {}).get("blocks")) or []
    for block in blocks:
        categories = ((block.get("program") or {}).get("categories")) or []
        if any(str(c).lower() in ("series", "episode", "show") for c in categories):
            return "series"
    # Also believe an entries/pool expression that filters to episodes.
    if '"episode"' in yaml_path.read_text():
        return "series"
    return "film"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "channel_path",
        type=Path,
        help="a deploy/appdata/channels/<N>-<name> dir, or an examples/channels/<name>.yaml file",
    )
    parser.add_argument("--out", type=Path, help="where to write the spec YAML")
    parser.add_argument(
        "--set-config",
        action="append",
        default=[],
        metavar="KEY=VALUE",
        help=(
            "override one key in the spec's `config:` map, repeatable. Preview-only "
            "retiming: a script's on-air interval is minutes long, which is not "
            "watchable. VALUE is parsed as YAML, so numbers stay numbers."
        ),
    )
    parser.add_argument(
        "--print-kind",
        action="store_true",
        help="print 'film' or 'series' for this channel and exit, nothing else",
    )
    args = parser.parse_args()

    if args.print_kind:
        print(guess_kind(args.channel_path))
        return

    if args.out is None:
        parser.error("--out is required unless --print-kind is given")

    name, station = channel_identity(args.channel_path)
    spec = override_config(dump_spec(name, station), args.set_config)
    args.out.write_text(yaml.safe_dump(spec, sort_keys=False))
    # Consumed by the calling shell script, which needs to know which file's
    # mtime to poll for edits. The channel's own file is the one a human edits
    # for a channel-specific change; an edit to the station spec or the shared
    # script it names is picked up by the same poll only if it happens to touch
    # this file too, which is the known limit of the 1Hz relay.
    resolved = args.channel_path.resolve()
    print(resolved / "channel.yaml" if resolved.is_dir() else resolved)


if __name__ == "__main__":
    main()
