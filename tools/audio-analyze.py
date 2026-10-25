#!/usr/bin/env python3
"""Audio facts for one media file, printed as one JSON object on stdout.

Usage: audio-analyze.py <file>

The station execs this once per catalog entry (crates/etv-station/src/audio.rs).
Essentia supplies tempo, beat ticks and key; librosa supplies the onset strength
and RMS curves. Everything else here is arithmetic over those: the steady beat
run, the line fitted through it, the downbeat phase, the per-bar energy and the
landing bar.

Output keys (all always present; a value the file cannot give is null):
  bpm               60 / slope of the fitted grid, else Essentia's tempo
  beat_grid         {slope, intercept, start_s, end_s, mean_residual_s, beats}
                    time of run beat k = intercept + slope * k, k = 0..beats-1;
                    extended past both ends it is the grid for the whole file
  downbeat_phase    0-3: run beat k is a downbeat when (k - phase) % 4 == 0
  landing_bar       first bar index, a multiple of 8 and >= 8, whose 4-bar mean
                    energy is at least 80% of the track's peak 4-bar energy;
                    bar 0 is the first downbeat at or after 0 s
  landing_bar_s     that bar's start time in seconds
  key, scale        e.g. "C", "major"
  camelot           e.g. "8B"
  key_strength      Essentia's 0-1 key correlation
  beat_confidence   Essentia RhythmExtractor2013 confidence (0-5.3)
  bar_energy        mean RMS^2 of each bar, bar 0 first
  rms_mean          mean RMS over the file

Nothing is thresholded: a short steady run or low confidence is reported as-is
so the caller applies its own floor. Exit 0 with the JSON, 1 with a message on
stderr when the file cannot be decoded or a library is missing.
"""
import json
import subprocess
import sys

SR = 44100
HOP = 512
STEADY_TOLERANCE = 0.04  # an interval within 4% of the median is steady
LANDING_FRACTION = 0.8
BEATS_PER_BAR = 4

CAMELOT = {
    ("C", "major"): "8B", ("G", "major"): "9B", ("D", "major"): "10B",
    ("A", "major"): "11B", ("E", "major"): "12B", ("B", "major"): "1B",
    ("F#", "major"): "2B", ("C#", "major"): "3B", ("G#", "major"): "4B",
    ("D#", "major"): "5B", ("A#", "major"): "6B", ("F", "major"): "7B",
    ("A", "minor"): "8A", ("E", "minor"): "9A", ("B", "minor"): "10A",
    ("F#", "minor"): "11A", ("C#", "minor"): "12A", ("G#", "minor"): "1A",
    ("D#", "minor"): "2A", ("A#", "minor"): "3A", ("F", "minor"): "4A",
    ("C", "minor"): "5A", ("G", "minor"): "6A", ("D", "minor"): "7A",
}
FLAT_TO_SHARP = {"Db": "C#", "Eb": "D#", "Gb": "F#", "Ab": "G#", "Bb": "A#"}


def decode(path):
    """Mono float32 samples at SR, via the ffmpeg the image already carries."""
    import numpy as np

    proc = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", path, "-vn", "-ac", "1", "-ar", str(SR),
         "-f", "f32le", "-"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    if proc.returncode != 0 or not proc.stdout:
        raise RuntimeError("ffmpeg could not decode audio: " + proc.stderr.decode(errors="replace").strip()[-300:])
    return np.frombuffer(proc.stdout, dtype=np.float32)


def steady_run(ticks):
    """Longest run of consecutive beats whose intervals are all steady.

    Returns (first_index, beat_count) into `ticks`; (0, 0) with fewer than 2.
    """
    import numpy as np

    if len(ticks) < 2:
        return 0, 0
    iv = np.diff(ticks)
    median = float(np.median(iv))
    ok = np.abs(iv - median) <= STEADY_TOLERANCE * median
    best = (0, 0)  # (first interval, interval count)
    start = None
    for i, good in enumerate(list(ok) + [False]):
        if good and start is None:
            start = i
        elif not good and start is not None:
            if i - start > best[1]:
                best = (start, i - start)
            start = None
    first, count = best
    return (first, count + 1) if count else (0, 0)


def fit_grid(run):
    """Least-squares line through run beat times; k is the beat's index in the run."""
    import numpy as np

    k = np.arange(len(run))
    slope, intercept = np.polyfit(k, run, 1)
    residual = float(np.mean(np.abs(run - (intercept + slope * k))))
    return float(slope), float(intercept), residual


def downbeat_phase(run, onset_times, onset_env):
    """The beat-in-bar phase (0-3) with the strongest mean onset strength."""
    import numpy as np

    strength = np.interp(run, onset_times, onset_env)
    sums = [float(np.mean(strength[p::BEATS_PER_BAR])) if len(strength[p::BEATS_PER_BAR]) else 0.0
            for p in range(BEATS_PER_BAR)]
    return int(np.argmax(sums))


def bar_energies(grid, phase, rms, rms_times, duration):
    """(bar_start_times, mean RMS^2 per bar) with bar 0 the first downbeat >= 0 s."""
    import numpy as np

    slope, intercept = grid
    bar_s = slope * BEATS_PER_BAR
    k0 = phase
    while intercept + slope * k0 < 0:
        k0 += BEATS_PER_BAR
    while intercept + slope * (k0 - BEATS_PER_BAR) >= 0:
        k0 -= BEATS_PER_BAR
    first = intercept + slope * k0
    starts, energy = [], []
    t = first
    while t + bar_s <= duration + 1e-6:
        mask = (rms_times >= t) & (rms_times < t + bar_s)
        energy.append(float(np.mean(rms[mask] ** 2)) if mask.any() else 0.0)
        starts.append(t)
        t += bar_s
    return starts, energy


def landing_bar(energy):
    """First 8-aligned bar >= 8 whose 4-bar mean reaches 80% of the peak 4-bar mean."""
    if len(energy) < 4:
        return None
    windows = [sum(energy[i:i + 4]) / 4 for i in range(len(energy) - 3)]
    peak = max(windows)
    if peak <= 0:
        return None
    for bar in range(8, len(windows), 8):
        if windows[bar] >= LANDING_FRACTION * peak:
            return bar
    return None


def analyze(path):
    import essentia.standard as es
    import librosa
    import numpy as np

    audio = decode(path)
    duration = len(audio) / SR

    bpm, ticks, confidence, _est, _iv = es.RhythmExtractor2013(method="multifeature")(audio)
    key, scale, strength = es.KeyExtractor()(audio)
    key = FLAT_TO_SHARP.get(key, key)

    onset_env = librosa.onset.onset_strength(y=audio, sr=SR, hop_length=HOP)
    onset_times = librosa.times_like(onset_env, sr=SR, hop_length=HOP)
    rms = librosa.feature.rms(y=audio, hop_length=HOP)[0]
    rms_times = librosa.times_like(rms, sr=SR, hop_length=HOP)

    out = {
        "bpm": float(bpm) if bpm else None,
        "beat_grid": None,
        "downbeat_phase": None,
        "landing_bar": None,
        "landing_bar_s": None,
        "key": key,
        "scale": scale,
        "camelot": CAMELOT.get((key, scale)),
        "key_strength": float(strength),
        "beat_confidence": float(confidence),
        "bar_energy": [],
        "rms_mean": float(np.mean(rms)) if len(rms) else None,
    }

    ticks = np.asarray(ticks, dtype=float)
    first, count = steady_run(ticks)
    if count >= 2:
        run = ticks[first:first + count]
        slope, intercept, residual = fit_grid(run)
        out["bpm"] = 60.0 / slope
        out["beat_grid"] = {
            "slope": slope,
            "intercept": intercept,
            "start_s": float(run[0]),
            "end_s": float(run[-1]),
            "mean_residual_s": residual,
            "beats": int(count),
        }
        phase = downbeat_phase(run, onset_times, onset_env)
        out["downbeat_phase"] = phase
        starts, energy = bar_energies((slope, intercept), phase, rms, rms_times, duration)
        out["bar_energy"] = energy
        bar = landing_bar(energy)
        if bar is not None:
            out["landing_bar"] = bar
            out["landing_bar_s"] = starts[bar]
    return out


def main(argv):
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    try:
        result = analyze(argv[1])
    except ImportError as e:
        print(f"audio-analyze: missing library: {e}", file=sys.stderr)
        return 1
    except Exception as e:  # decode failures and analyzer errors alike
        print(f"audio-analyze: {e}", file=sys.stderr)
        return 1
    json.dump(result, sys.stdout)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
