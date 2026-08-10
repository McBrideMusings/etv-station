---
name: upstream-sync
description: "Absorb upstream ErsatzTV/next changes into the vendored etv-next tree under vendor/etv-next. Covers the survey, the two files that need special handling, the conflict doctrine, and the ffmpeg pin."
user_invocable: true
---

# /upstream-sync — absorb upstream ErsatzTV/next into `vendor/etv-next`

Upstream is a live project we do not control. This walks the absorb: survey what
changed, merge it, decide what to do where this project has diverged, and verify
the result.

## When to use this skill

- "merge upstream", "sync with ErsatzTV", "what's new upstream", "absorb upstream changes"
- Before starting work in `vendor/etv-next/` that touches a subsystem upstream is also moving
- After a long gap, to find out whether the vendored tree has drifted somewhere expensive

## One repo, one tree

`vendor/etv-next/` is [ErsatzTV/next](https://github.com/ErsatzTV/next) plus this
project's modifications, as ordinary tracked files in `etv-station`. There is no
fork repository and no submodule — a change to it is a normal commit here.

**Never push to upstream.** Pulling from it is the entire point.

**Keep a `vendor/etv-next/` change in its own commit**, separate from station-side
changes. Mixing them makes the next merge much harder to read.

The remote (add once per clone):

```sh
git -C ~/Projects/etv-station remote add etv-upstream https://github.com/ErsatzTV/next
```

## Pre-flight

Tree clean, on `main`:

```sh
git -C ~/Projects/etv-station status --short
git -C ~/Projects/etv-station branch --show-current
```

## 1. Survey before merging

Never merge blind — the survey is what tells you which of the two special files
below are in play, and it is also the answer to "tell me what's new".

`git subtree` has no survey mode, so compare against the last absorbed upstream
commit, which is recorded in the log:

```sh
git -C ~/Projects/etv-station fetch etv-upstream
git -C ~/Projects/etv-station log --grep='git-subtree-split' -1 --format=%b   # last absorbed SHA
git -C ~/Projects/etv-station log --oneline <last-absorbed>..etv-upstream/main
git -C ~/Projects/etv-station diff --stat <last-absorbed>..etv-upstream/main
```

Sort what you find into three buckets, and say which is which when reporting:

- **Reaches beyond the vendored tree** — anything touching `schema/playout.json`
  or the ffmpeg pin (below). These have consequences for the station crates.
- **Matters to us** — playback, HLS, m3u/XMLTV output, channel lifecycle, error
  reporting. Report these individually.
- **Doesn't apply to this hardware** — rkmpp is ARM Rockchip, VAAPI/radeonsi is
  AMD. The deploy target is Unraid + a Mac dev box. Say so and move on rather
  than describing them as if they mattered.

## 2. The two files that need special handling

### `vendor/etv-next/schema/playout.json` — the pinned contract

This is the interface the station crates write against. Read the diff before
merging:

```sh
git -C ~/Projects/etv-station diff <last-absorbed>..etv-upstream/main -- schema/playout.json
```

- **Additive** (new optional field, new definition) → station JSON stays valid,
  nothing forced. Note it as something the station *could* adopt.
- **Anything else** (renamed, removed, newly required) → the station's emitter
  must move in the same change. Do not land the merge and leave that for later;
  the station will emit JSON the new code rejects.

### The ffmpeg pin — it lives in two places

Upstream pins ffmpeg in `docker/Dockerfile`; etv-station pins it independently:

```sh
git -C ~/Projects/etv-station show etv-upstream/main:docker/Dockerfile | grep ersatztv-ffmpeg
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
git -C ~/Projects/etv-station subtree pull --prefix=vendor/etv-next etv-upstream main --squash
```

Conflicts appear as normal working-tree conflicts under `vendor/etv-next/`.

On a conflict, first answer one question: **is this two edits to one file, or two
different implementations of the same thing?**

```sh
git -C ~/Projects/etv-station show HEAD:vendor/etv-next/<path> | wc -l
git -C ~/Projects/etv-station show etv-upstream/main:<path> | wc -l
git -C ~/Projects/etv-station log --oneline -- vendor/etv-next/<path>
```

Wildly different sizes, or a history here that *created* the file, means the
second. Resolving that hunk-by-hunk produces a chimera that compiles and is
nobody's design.

For two implementations: **keep ours, then port upstream's individual fixes on
top, checking whether each even applies.** Commit the merge and the ports
separately so a port can be reverted alone.

> **Worked example — `crates/ersatztv/src/xmltv.rs`.** This project wrote
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
git -C ~/Projects/etv-station show <sha> --stat
git -C ~/Projects/etv-station diff etv-upstream/main -- vendor/etv-next/<each-other-path>
```

## 4. Verify

The vendored tree is its own cargo workspace, and the station workspace builds
against it. Both have to pass:

```sh
cargo build --manifest-path ~/Projects/etv-station/vendor/etv-next/Cargo.toml --workspace --all-features
cargo test  --manifest-path ~/Projects/etv-station/vendor/etv-next/Cargo.toml --workspace
cargo clippy --manifest-path ~/Projects/etv-station/vendor/etv-next/Cargo.toml --locked --workspace --all-features --all-targets -- -D clippy::all

cargo build  --manifest-path ~/Projects/etv-station/Cargo.toml --workspace
cargo test   --manifest-path ~/Projects/etv-station/Cargo.toml --workspace
cargo clippy --manifest-path ~/Projects/etv-station/Cargo.toml --workspace --all-targets -- -D clippy::all
```

Format only the crates you touched (`cargo +nightly fmt -p <crate>`). A
workspace-wide format inside a merge buries the reconciliation in reformatting.

If the ffmpeg pin moved, change `Dockerfile` in the **same commit** as the merge —
they are one decision, and splitting them leaves a window where the station
builds new code on the old base image.

## Reporting back

Lead with what reaches beyond the vendored tree (schema, ffmpeg pin), then the
fixes that matter to us, then a one-line dismissal of the hardware-specific ones.
If a conflict was resolved by keeping ours, say which upstream changes were
ported, which were checked and found inapplicable, and why — "not ported"
without the reason reads as something skipped on a whim.
