# Writing a plugin

This page is for someone writing their first Rhai plugin. It says what to write, what the
station hands the script, what it expects back, and the two mistakes that turn a
millisecond generation into a six-minute one. [Config schema](./schema.md) is the
field-by-field reference; the [ADRs](./adr/) say why the contract has this shape.

A plugin is a `.rhai` script in the plugins folder. A channel file points at it. The
station calls named functions in it while it builds a schedule; the script never touches
the clock, the network or the disk except through the doors the channel opens for it.

## Which hook do you need

A script declares which hooks it implements in a `hooks()` function. The station wires
only the declared ones.

| Hook | The station calls | It receives | It returns | Named in the channel by |
|---|---|---|---|---|
| `pool_provider` | `sources()`, `pick(ctx)`, `audit(ctx, picks, workspace)` | the matched catalog items (`ctx.sets`), the aired tail (`ctx.recent`), `ctx.seed`, `ctx.temperature`, `ctx.config`, `ctx.target_count` | `pick()` returns `#{ picks: [...], workspace: ... }`; `audit()` returns a map of stage records | a pool's `plugin:` |
| `sequencer` | `arrange(ctx)` | the block's already-resolved pools as full item maps (`ctx.pools`), the window (`ctx.window`), read-only resume state (`ctx.resume`, `ctx.cursor`), `ctx.seed`, `ctx.temperature` | the block's timeline: an array of `entry_id`s in air order | a block's `sequencer:` |

Two more hooks exist, `annotate` (adds metadata to every scheduled item) and `bumper`
(builds the filler between programmes). They are covered in
[Config schema](./schema.md#channel-annotate-—-a-plugin-attaches-metadata-to-every-item);
the rest of this page is about the first two.

Most channels need exactly one of them, and many need none:

| What the channel wants | Hook | Why |
|---|---|---|
| Plain pools, interleaved by a `pattern` | none | CEL `expr` pools and a `pattern` already cover it |
| A pool ranked by something a CEL expression cannot say (taste, distance from the last pick, an external history) | `pool_provider` | the script ranks; the pattern still interleaves |
| An arrangement the pattern cannot express (one pool resumes where it stopped while another restarts every generation, or a clock-dependent order) | `sequencer` | the script owns the interleave; the pools can be plain `expr` pools |
| A ranked pool and a custom interleave of it with others | both | a `pool_provider` pool feeds a block that names a `sequencer:` |

A block names `sequencer:` or `pattern:`, never both. A pool names exactly one of `expr`,
`plugin` or `groups`.

## Declaring hooks and capabilities

The declaration lives in the script, so swapping one scorer for another needs no YAML edit.

```rhai
fn hooks() { ["pool_provider"] }

// Optional. Omit it and the script declares nothing.
fn capabilities() { ["catalog_read", "http", #{ datastore: "taste" }] }
```

Both functions are read at config load: the station compiles the script and calls just that
one function, before the catalog exists. `sources()`, `pick()` and `arrange()` do not run.

**Nothing is ambient.** Without a declaration and a grant, a script sees only `ctx.pool`,
`ctx.config`, `ctx.target_count`, `ctx.now`, `ctx.recent`, `ctx.seed`, `ctx.temperature`,
`ctx.account_id`, and the global `parse_json(text)`. Everything else is a capability:

| Capability | Opens | Declared as | Granted in the pool as |
|---|---|---|---|
| `catalog_read` | `ctx.sets` (the items each `sources()` query matched) and `ctx.entry_id_for(source, id)` | `"catalog_read"` | `capabilities: [catalog_read]` |
| `http` | `ctx.http(url, method, body)`: 10 seconds per call, 30 seconds per `pick()` together | `"http"` | `capabilities: [http]` |
| a named datastore | `ctx.datastore("name")`, a read-only handle on the plex-db-ex reader | `#{ datastore: "name" }` | `datastores: [{ name: name, path: "${VAR}" }]` |

The channel grants them on the same pool that names the script:

```yaml
pools:
  - name: movies
    plugin: "../plugins/taste-cosine.rhai"
    capabilities: [catalog_read, http]
    datastores:
      - name: taste
        path: "${PLEXDB_SNAPSHOT_PATH}"
```

The declaration and the grant must match exactly, in both directions. These are the
failures you will hit, all at config load unless noted:

| Mistake | What happens |
|---|---|
| `plugin:` names a script that does not declare `pool_provider` | refused, naming the script and listing the hooks it does declare |
| `hooks()` names something that is not a hook | refused, listing the hooks that exist (`pool_provider`, `sequencer`, `annotate`, `bumper`) |
| `hooks()` returns `[]` | refused: a plugin must declare at least one hook |
| the script declares a capability the pool does not grant | refused, naming the plugin and the capability |
| the pool grants a capability the script never declares | refused the same way: a grant nobody asked for is a typo or a stale config |
| a datastore path that cannot be opened | refused, naming the datastore and the underlying error |
| the script reads `ctx.sets` or `ctx.http` without the capability | the `pick()` call fails when it reads the field, naming the capability. This one only shows at run time, because the reach is a script call |
| a `pool_provider` with no `audit()` function | refused, naming the missing function |

`examples/plugins/taste-cosine.rhai` declares all three kinds and is the worked example.

## The return shape

`pick()` returns an envelope, not a bare array:

```rhai
#{ picks: [ "ghost", #{ entry_id: "alien", metadata: #{ reason: "won an Oscar" }, take: 3 } ],
   workspace: () }
```

- **`picks`** is most-wanted first. Each element is a bare `entry_id` string, or a record:
  - `entry_id`: the item.
  - `metadata`: an opaque blob. The station reads nothing in it and carries it to that
    airing's entry in the playout JSON, where an overlay script can draw it. A non-finite
    float anywhere inside (`.inf`, `.nan`) fails the generation, naming the key.
  - `take`: overrides the pattern step's `take` for that entry's series, for example
    letting an unseen show sample three episodes while a watched one plays its full run.
    It applies under `rotate: "visit"` only, and zero or negative fails the generation.
- **`workspace`** is any value you want handed to `audit()` unread. It lives for one
  generation and nothing carries to the next. Use `()` when there is nothing to hand over.

`audit()` returns a map keyed by `entry_id`, one stage record per pick you want to explain:

```rhai
#{ "ghost": #{ stage: "pool", by: "plugin:example", verdict: "ranked highest", detail: #{ score: 0.82 } } }
```

`stage` is `pool` for a `pool_provider`. `verdict` and `detail` are in your own words and
the station never reads them. `guide` is optional (at most 200 characters) and is the
sentence shown in the guide when the channel sets `scoring.explain`. An `entry_id` that
`pick()` did not return is refused.

A `sequencer`'s `arrange(ctx)` returns an array of `entry_id`s. Every id must belong to one
of the block's pools, or the generation fails naming it. A timeline that overruns the
window is cut at the boundary; one that falls short is not padded.
`examples/plugins/foryou-sequencer.rhai` is the worked example.

## The determinism contract

A plugin must be a pure function of `(catalog, config, resume state, seed, external-store snapshot)`.

- Draw randomness from `ctx.seed`, with ordinary arithmetic. It is an integer, mixed with
  the pool's name, so two pools sharing a script do not shuffle in lockstep.
- Read time from `ctx.now`. Rhai's `timestamp()` and `elapsed()` read a clock fixed to the
  generation's start, so they are safe, but they carry no seed of their own.
- Do not rely on an iteration order Rhai does not guarantee, such as the key order of a map.

**What it costs when broken.** Two generations of the same channel from the same inputs
produce different schedules. Both look valid, so nothing errors and nothing on screen says
which one is right. A restart, a config reload and a catch-up regeneration all regenerate,
so the channel's upcoming schedule changes under the viewer for no reason anyone can find.

**The check.** Generate the channel twice and diff:

```sh
etv-station --config <station.yaml> --check-determinism <channel>
```

It exits 0 when the two schedules (and their audit trails) are identical. Otherwise it
exits 1 and names the first position where they disagree, with both entry ids there. Run
it after any change to a script's ranking.

## Two performance rules

`pick()` runs once per generation per pool, and the channel airs nothing until it returns.
Rhai is an interpreter, so the shape of the loop decides whether that takes milliseconds
or minutes. Both rules were paid for once, while writing `taste-cosine.rhai`.

### Build id-keyed maps before the item loop

A loop over `ctx.sets.<name>` runs its body once per item, which on a real library is tens
of thousands of times. If the body also walks a fetched history or `ctx.recent` looking for
a match, the two multiply.

The code that caused it, a linear scan per item:

```rhai
for item in ctx.sets.movies {
    let last = ();
    for event in history {                       // walked once per item
        if event.entry_id == item.entry_id { last = event.watched_at; }
    }
    // ...
}
```

Measured on an 84,722-item library with about 1,000 watch rows and a 200-deep aired tail:
roughly 101 million interpreted steps to choose four things, 6 minutes 34 seconds.

Build a map keyed by `entry_id` once, before the loop, and read it inside:

```rhai
let watched = #{};
for event in history { watched[event.entry_id] = event.watched_at; }
for item in ctx.sets.movies {
    let last = watched[item.entry_id];
    // ...
}
```

### Keep per-item scoring inline

Rhai passes arguments by value. A call like `score_item(item, ctx)` copies the whole of
`ctx` (the entire watch history and aired tail) every time it runs.

The code that caused it:

```rhai
fn score_item(item, ctx) { /* the scoring arithmetic */ }
for item in ctx.sets.movies { scores.push(score_item(item, ctx)); }   // ctx cloned per item
```

The same scoring arithmetic measured 124 seconds split across helper functions and 13
seconds written inline in the loop body. Keep per-item work in the loop. Use named
functions for things called once, such as reading a tunable out of `ctx.config`.

## Replay policy is the plugin's

The station computes no replay policy for a plugin pool. It hands the script `ctx.recent`
(what the channel aired most recently, oldest first) and takes back whatever order comes
out. Whether the same title can air in two generations running is a property of the script:

- A script that suppresses what it recently returned holds a title back for as long as its
  own policy says.
- A script that does not suppress hands back its same top-ranked item every time. With
  `advance: restart` nothing else in the config stops that, and the result is a valid
  schedule that plays one film forever.

Swapping the script swaps this behavior, and nothing in the YAML says which kind you have.

The exception is a pool that declares `constraints`. `no_repeat_within: N` is applied to
the list the plugin returned, inside pool resolution and before the pattern draws from it,
so it holds whatever the script does. The two do not layer: a no-repeat rule marches the
pool forward through its returned set, which leaves the script's own `ctx.recent`
suppression less to hold back. Set `constraints` on a plugin pool when the config is the
only thing guarding replay, and leave it unset when the script is. The decision behind this
is [ADR 0002](./adr/0002-scorer-plugin-replaces-a-pool-expr.md).

## Before you deploy a script

- Run `etv-station --check-determinism <channel>` against a channel that uses it.
- Run `admin plugin-check` before `admin deploy image` whenever the contract changed: it
  walks the plugin folder that is on the host right now and reports every script that
  would refuse to load. A script the station refuses to load fails config load, which
  takes every channel off air, not one. See
  [Changing the plugin contract](./schema.md#changing-the-plugin-contract-is-a-breaking-change-for-deployed-scripts).
