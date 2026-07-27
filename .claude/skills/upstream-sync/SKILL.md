---
name: upstream-sync
description: "Absorb upstream ErsatzTV/next changes into the private etv-next fork, then land them in etv-station via the submodule pointer. Covers the survey, the two files that need special handling, the conflict doctrine, and the ffmpeg pin."
user_invocable: true
---

# /upstream-sync — reconcile the private etv-next fork with upstream

Upstream is a live project we do not control. This walks the absorb: survey what
changed, merge it, decide what to do where the fork has diverged, and carry the
result into etv-station.

## When to use this skill

- "merge upstream", "sync with ErsatzTV", "what's new upstream", "absorb upstream changes"
- Before starting work in `etv-next` that touches a subsystem upstream is also moving
- After a long gap, to find out whether the fork has drifted somewhere expensive

## Two repos, and which is which

| | |
|---|---|
| `~/Projects/etv-next` | the fork. `origin` = `McBrideMusings/etv-next-private`, `upstream` = `ErsatzTV/next`. Default branch **`pierce-main`**, not `main`. |
| `~/Projects/etv-station` | consumes the fork as the `etv-next/` submodule. |

**Never push to `upstream`.** Pulling from it is the entire point; pushing is
never correct. See `AGENTS.md` in the fork for the full fork-safety rules.

**Never edit files under `etv-station/etv-next/`.** That checkout exists only to
be pinned. Changes go in `~/Projects/etv-next` and reach the station by bumping
the submodule SHA — anything edited in place is silently lost on the next bump.

## Pre-flight

Both trees clean, fork on `pierce-main`:

```sh
git -C ~/Projects/etv-next status --short
git -C ~/Projects/etv-next branch --show-current
git -C ~/Projects/etv-station status --short
```

## 1. Survey before merging

Never merge blind — the survey is what tells you which of the two special files
below are in play, and it is also the answer to "tell me what's new".

```sh
git -C ~/Projects/etv-next fetch upstream
git -C ~/Projects/etv-next log --oneline pierce-main..upstream/main
git -C ~/Projects/etv-next diff --stat pierce-main...upstream/main
```

Sort what you find into three buckets, and say which is which when reporting:

- **Reaches beyond this repo** — anything touching `schema/playout.json` or the
  ffmpeg pin (below). These have consequences in etv-station.
- **Matters to us** — playback, HLS, m3u/XMLTV output, channel lifecycle, error
  reporting. Report these individually.
- **Doesn't apply to this hardware** — rkmpp is ARM Rockchip, VAAPI/radeonsi is
  AMD. The deploy target is Unraid + a Mac dev box. Say so and move on rather
  than describing them as if they mattered.

## 2. The two files that need special handling

### `schema/playout.json` — the pinned contract

This is the interface etv-station writes against. Read the diff before merging:

```sh
git -C ~/Projects/etv-next diff pierce-main...upstream/main -- schema/playout.json
```

- **Additive** (new optional field, new definition) → station JSON stays valid,
  nothing forced. Note it as something the station *could* adopt.
- **Anything else** (renamed, removed, newly required) → the station's emitter
  must move in the same change. Do not land the merge and leave that for later;
  the station will emit JSON the new code rejects.

### The ffmpeg pin — it lives in two places

Upstream pins ffmpeg in `docker/Dockerfile`; etv-station pins it independently:

```sh
git -C ~/Projects/etv-next show upstream/main:docker/Dockerfile | grep ersatztv-ffmpeg
grep ersatztv-ffmpeg ~/Projects/etv-station/Dockerfile
```

If upstream moved, etv-station moves with it — the merged code is what upstream
builds and tests against that version. Leaving the station behind runs new code
on an ffmpeg it was never exercised with.

Before committing a base-image bump, prove the runtime stage still resolves on
it. This takes about a minute and skips compiling Rust:

```sh
docker manifest inspect ghcr.io/ersatztv/ersatztv-ffmpeg:<new-tag>
```

then build a throwaway image `FROM` that tag with just etv-station's runtime
`apt-get install` line plus `fc-cache -f` and the `groupadd`/`useradd`, and run
`ffmpeg -version`. A major base bump can drop packages; that is the risk worth
one minute.

## 3. Merge, and the conflict doctrine

```sh
git -C ~/Projects/etv-next merge upstream/main -m "Merge upstream ErsatzTV/next"
```

On a conflict, first answer one question: **is this two edits to one file, or two
different implementations of the same thing?**

```sh
git -C ~/Projects/etv-next show HEAD:<path> | wc -l
git -C ~/Projects/etv-next show upstream/main:<path> | wc -l
git -C ~/Projects/etv-next log --oneline upstream/main..HEAD -- <path>
```

Wildly different sizes, or a fork history that *created* the file, means the
second. Resolving that hunk-by-hunk produces a chimera that compiles and is
nobody's design.

For two implementations: **keep ours, then port upstream's individual fixes on
top, checking whether each even applies.** Commit the merge and the ports
separately so a port can be reverted alone.

> **Worked example — `crates/ersatztv/src/xmltv.rs`.** The fork wrote
> `/xmltv.xml` from scratch and drives it from playout JSON: 475 lines against
> upstream's 194, which never reads the playout folder at all. Upstream landed
> two changes there. One (multiple `display-name` forms) was worth porting —
> clients match on different forms and we emitted only the name. The other (an
> entity-preservation fix) **did not apply at all**: it repairs a copy loop that
> re-emits an existing XMLTV document, where quick-xml surfaces entities as
> separate `GeneralRef` events the loop dropped. Ours is a builder — no parser,
> every value through `BytesText::new`, which escapes on write, and an existing
> test already pinned `"A & B < C"` emitting as `A &amp; B &lt; C`.
>
> The lesson is the check, not the answer: **before porting a fix, confirm the
> code it repairs exists here.** Porting it anyway would have meant importing a
> parser we do not use.

Note also that one upstream commit can span several files. Taking `--ours` on one
conflicted file still absorbs that commit's changes everywhere else — check what
actually landed before reporting a fix as "not taken":

```sh
git -C ~/Projects/etv-next show <sha> --stat
git -C ~/Projects/etv-next diff upstream/main -- <each-other-path>
```

## 4. Verify

```sh
cd ~/Projects/etv-next
cargo build --workspace --all-features
cargo test --workspace
cargo clippy --locked --workspace --all-features --all-targets -- -D clippy::all
```

Format only the crates you touched (`cargo +nightly fmt -p <crate>`). A
workspace-wide format inside a merge buries the reconciliation in reformatting.

## 5. Land it in etv-station

The absorb is not finished until the station points at it:

```sh
git -C ~/Projects/etv-station/etv-next fetch origin
git -C ~/Projects/etv-station/etv-next checkout <new-sha>
cd ~/Projects/etv-station
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D clippy::all
git add etv-next
```

If the ffmpeg pin moved, change `Dockerfile` in the **same commit** — they are
one decision, and splitting them leaves a window where the station builds new
code on the old base image.

## Reporting back

Lead with what reaches beyond the repo (schema, ffmpeg pin), then the fixes that
matter to us, then a one-line dismissal of the hardware-specific ones. If a
conflict was resolved by keeping ours, say which upstream changes were ported,
which were checked and found inapplicable, and why — "not ported" without the
reason reads as something skipped on a whim.
