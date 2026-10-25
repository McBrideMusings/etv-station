#!/usr/bin/env python3
"""Check tools/audio-analyze.py against two synthetic fixtures.

Usage: tools/audio-analyze-check.py

Needs numpy, essentia, librosa and ffmpeg on the machine; exits 0 with a skip
line when any is missing. Builds into a temp dir and deletes it.

  steady.wav   120 BPM kick on every beat, accented on beat 1 of each bar, over
               a sustained C major chord, 60 s. Expect bpm within 0.5 of 120, a
               steady span covering >= 90% of the file, key C major.
  freetime.wav the same bed with 20 s of kicks at irregular intervals first.
               Expect the steady span to start after the intro's first half
               (Essentia extrapolates the grid a few beats backward).
"""
import importlib.util
import json
import os
import subprocess
import sys
import tempfile

try:
    import numpy as np
    import essentia  # noqa: F401
    import librosa  # noqa: F401
except ImportError as e:
    print(f"skip: {e}")
    sys.exit(0)

SR = 44100
BPM = 120.0
BEAT = 60.0 / BPM
LENGTH = 60.0
INTRO = 20.0


def kick(accent):
    t = np.arange(int(0.12 * SR)) / SR
    freq = 150 * np.exp(-t * 30) + 45
    return (1.0 if accent else 0.6) * np.sin(2 * np.pi * np.cumsum(freq) / SR) * np.exp(-t * 28)


def bed():
    t = np.arange(int(LENGTH * SR)) / SR
    chord = sum(np.sin(2 * np.pi * f * t) for f in (130.81, 164.81, 196.0, 261.63, 329.63, 392.0))
    return 0.08 * chord


def render(onsets):
    out = bed()
    for i, (at, accent) in enumerate(onsets):
        k = kick(accent)
        s = int(at * SR)
        out[s:s + len(k)] += k[: len(out) - s]
    return np.clip(out, -1, 1)


def write_wav(path, samples):
    pcm = (samples * 32767).astype("<i2").tobytes()
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-f", "s16le", "-ar", str(SR), "-ac", "1", "-i", "-", path],
        input=pcm, check=True,
    )


def main():
    spec = importlib.util.spec_from_file_location(
        "analyze", os.path.join(os.path.dirname(os.path.abspath(__file__)), "audio-analyze.py"))
    analyze = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(analyze)

    rng = np.random.default_rng(7)
    steady = [(i * BEAT, i % 4 == 0) for i in range(int(LENGTH / BEAT) - 1)]
    intro, t = [], 0.5
    while t < INTRO:
        intro.append((t, False))
        t += float(rng.uniform(0.25, 0.9))
    resume = int(np.ceil((INTRO + BEAT) / BEAT))
    freetime = intro + [(i * BEAT, i % 4 == 0) for i in range(resume, int(LENGTH / BEAT) - 1)]

    failures = []
    with tempfile.TemporaryDirectory() as d:
        a, b = os.path.join(d, "steady.wav"), os.path.join(d, "freetime.wav")
        write_wav(a, render(steady))
        write_wav(b, render(freetime))
        s, f = analyze.analyze(a), analyze.analyze(b)
    print(json.dumps({"steady": {k: v for k, v in s.items() if k != "bar_energy"},
                      "freetime": {k: v for k, v in f.items() if k != "bar_energy"}}, indent=1))

    grid = s["beat_grid"]
    if abs(s["bpm"] - BPM) > 0.5:
        failures.append(f"steady bpm {s['bpm']} not within 0.5 of {BPM}")
    if not grid or (grid["end_s"] - grid["start_s"]) < 0.9 * LENGTH:
        failures.append(f"steady span {grid} covers under 90% of {LENGTH}s")
    if (s["key"], s["scale"]) != ("C", "major"):
        failures.append(f"steady key {s['key']} {s['scale']} is not C major")
    fg = f["beat_grid"]
    if not fg or fg["start_s"] < INTRO / 2:
        failures.append(f"free-time span {fg} starts in the first half of the {INTRO} s intro")
    for line in failures:
        print("FAIL", line, file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
