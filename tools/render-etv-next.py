#!/usr/bin/env python3
"""Render ETV-next's lineup.json + channelN.json from the station config.

This is the single-source-of-truth generator for the ETV-next side of the
shared-folder contract (B1). Instead of hand-authoring each channel's
`playout.folder` to match what the station writes, we DERIVE it: the station
binary's `--list-folders` prints each channel's resolved output folder in
channel order, and we emit ETV-next config that reads exactly those folders.

What the station owns (derived here): the channel roster, channel numbers
(station order), and each `playout.folder`.

What ETV-next owns (supplied here, NOT from the station config): display names
and the `normalization` / `ffmpeg` playback block — none of which are the
station's concern. Defaults come from `normalization.default.json`; per-channel
display names and playback overrides come from an optional, gitignored
`presentation.json` keyed by channel identity, each value `{name?, config?}`
where `config` is a partial deep-merged onto the default channel body (see
presentation.example.json for the format).

Outputs (all gitignored):
  {out_dir}/lineup.json
  {out_dir}/channel{N}.json   (N = 1..channel count, in station order)

Config via environment:
  ETV_BIND_ADDRESS   lineup server bind (default 0.0.0.0)
  ETV_PORT           lineup server port (default 8409)
  ETV_HLS_OUTPUT     ETV-next HLS output folder (default tmp/hls)
  STATION_CONFIG     station config path (default examples/station.yaml)
  ETV_NEXT_DIR       output dir (default examples/etv-next)
  ETV_STATION_BIN    prebuilt station binary; if unset, `cargo run` is used
"""

import json
import os
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


def env(name, default):
    v = os.environ.get(name)
    return v if v not in (None, "") else default


def list_folders(station_config):
    """Return each channel's resolved output folder, in channel order, by
    asking the station binary — so these can never disagree with where the
    daemon actually writes."""
    bin_override = os.environ.get("ETV_STATION_BIN")
    if bin_override:
        cmd = [bin_override]
    else:
        cmd = ["cargo", "run", "-q", "-p", "etv-station", "--"]
    cmd += ["--config", str(station_config), "--list-folders"]
    result = subprocess.run(cmd, cwd=REPO_ROOT, capture_output=True, text=True)
    if result.returncode != 0:
        # Surface the station's own diagnostic instead of a bare
        # CalledProcessError — the stderr is where the real config error lives.
        detail = result.stderr.strip() or f"exit code {result.returncode}"
        sys.exit(f"[render-etv-next] station --list-folders failed:\n{detail}")
    return [line for line in result.stdout.splitlines() if line.strip()]


def deep_merge(base, override):
    """Recursively merge override onto a copy of base (dicts only)."""
    result = dict(base)
    for key, val in override.items():
        if isinstance(val, dict) and isinstance(result.get(key), dict):
            result[key] = deep_merge(result[key], val)
        else:
            result[key] = val
    return result


def main():
    bind = env("ETV_BIND_ADDRESS", "0.0.0.0")
    port_str = env("ETV_PORT", "8409")
    try:
        port = int(port_str)
    except ValueError:
        sys.exit(f"[render-etv-next] ETV_PORT must be an integer, got {port_str!r}")
    hls_output = env("ETV_HLS_OUTPUT", "tmp/hls")
    station_config = env("STATION_CONFIG", "examples/station.yaml")
    out_dir = REPO_ROOT / env("ETV_NEXT_DIR", "examples/etv-next")

    default_path = out_dir / "normalization.default.json"
    if not default_path.exists():
        sys.exit(f"[render-etv-next] missing {default_path}")
    default_body = json.loads(default_path.read_text())

    presentation_path = out_dir / "presentation.json"
    presentation = {}
    if presentation_path.exists():
        presentation = json.loads(presentation_path.read_text())

    folders = list_folders(station_config)
    if not folders:
        sys.exit(f"[render-etv-next] no channels resolved from {station_config}")

    # Drop any previously generated channel files so a shrunk roster (or the
    # legacy un-numbered channel.json) can't leave orphans behind.
    for stale in out_dir.glob("channel*.json"):
        stale.unlink()

    lineup_channels = []
    for i, folder in enumerate(folders, start=1):
        identity = os.path.basename(folder.rstrip("/"))
        overrides = presentation.get(identity, {})
        display = overrides.get("name", identity)

        # Absolute so ETV-next reads exactly where the station writes,
        # regardless of ETV-next's own working directory. A relative folder
        # from --list-folders is resolved against the repo root (where the
        # station runs); an absolute one (e.g. from ETV_STATION_OUTPUT_BASE)
        # passes through unchanged.
        playout_folder = str((REPO_ROOT / folder).resolve())

        channel = deep_merge(default_body, overrides.get("config", {}))
        # The station owns playout.folder — inject it AFTER the merge so a
        # `playout` key in the default body or a presentation override can never
        # clobber the derived folder (it may still carry other playout.* keys).
        channel.setdefault("playout", {})["folder"] = playout_folder
        (out_dir / f"channel{i}.json").write_text(json.dumps(channel, indent=2) + "\n")

        lineup_channels.append(
            {"number": str(i), "name": display, "config": f"./channel{i}.json"}
        )

    lineup = {
        "server": {"bind_address": bind, "port": port},
        "output": {"folder": hls_output},
        "channels": lineup_channels,
    }
    (out_dir / "lineup.json").write_text(json.dumps(lineup, indent=2) + "\n")

    print(
        f"[render-etv-next] {out_dir}/lineup.json + {len(folders)} channel file(s) "
        f"(bind={bind} port={port})"
    )


if __name__ == "__main__":
    main()
