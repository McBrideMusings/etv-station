# A scorer plugin replaces a pool's `expr`, not its `order`

A pool names either a CEL expression or a plugin script, never both. A plugin pool hands the script a `query()` function, a target count, the recent watch history, and the recently-aired tail, and takes back an ordered list of `entry_id`s. Everything downstream — `select`, `rotate`, `advance`, `on_short`, and the pattern's `take` — operates on that list exactly as it does on a CEL-resolved one.

## Why

The obvious place to put a scorer is the sort, and that is where #74 originally put it: `order = "score"`. #108 deleted the `Order::Score` keyword before any of it was built, because `Order`'s contract is that every variant is computable from the ids being ordered, and a relevance score is not. That left the question of where scoring belongs genuinely open.

Sorting turns out to be the wrong half of the job anyway. A "For You" pool cannot be written as an `expr` plus a sort, because the expression that gathers the candidates is the same judgment as the ranking: *which* shows are worth surfacing, whether to reach for something already in progress, whether recently-aired material is eligible at all. Splitting that across a hand-written CEL expression and a plugin sort means the config author has to write the half they least know how to write. The plugin has to own gathering.

Once it owns gathering it is a source of items, which is what a pool's `expr` already is — so it belongs there, and the pool keeps everything else.

## What we chose, and the rejected alternatives

- **A pool's `expr`, not a pool's `order`.** As an `order`, the plugin only reorders a set someone else chose with a CEL expression. That is the split the paragraph above rules out.
- **A pool, not a new block entry kind.** `kind: plugin`, producing one finished ordered list, was the design right up until pools landed. It cannot participate in a pattern, so a channel using it gets no interleave, no `take`, and no `advance: resume` — and "two movies, then three episodes of one show" would have to be reimplemented inside the script. The pattern engine already does that correctly.
- **A tagged `order` string (`order: "plugin:path.rhai"`), rejected before pools existed.** It parses fine and it does carry the plugin path, so it does not reopen #108's objection directly. It still only ranks.
- **The script returns ids, not scores.** Every existing order variant returns a `Vec<String>` of `entry_id`s and keeps its sort key private — `Fields` reads `year` inside SQL and only ids come back. A plugin returning floats would widen a contract that does not need widening; the score is internal to the plugin.
- **The script gets a `query()` function, not the catalog.** Handing over every row would push the CEL engine's job into script and materialize the whole library per generation. Handing over a query function lets the plugin narrow the corpus itself, several times if it wants, and keeps the data volume proportional to what the algorithm actually reads.
- **ETV passes a target count.** The plugin picks the corpus, so nothing else can know how big it should be, and it cannot derive the window duration itself. Returning too few would drain the pool and surface as a short channel at runtime.

## Consequences

**Replay policy stayed with the plugin, and that is a dependency the config cannot see.** Because the plugin owns gathering and ranking, it also owns whether anything it recently returned is eligible again. ETV supplies `ctx.recent` and takes back an ordered list; it computes no replay policy and cannot inspect the one the script has. So a scorer that suppresses recent airings produces a channel that varies, and a scorer written without suppression produces a channel that hands back its same top item every generation — with `advance: restart`, the same film forever. Both are legitimate plugins under this ADR, the schedule is valid either way, and nothing in the YAML distinguishes them. `taste-engine.rhai`, the scorer we ship, has its own replay TTL; that is a property of that script, not of the contract.

**`constraints` on a pool (#115) is the channel author's opt-in floor over that, not a change to the split.** `no_repeat_within: N` is applied to the ordered list the plugin returned, inside pool resolution and before the pattern draws, so a channel can guarantee spacing regardless of what the script does. It is deliberately not defaulted: a plugin pool with no `constraints` still gets no floor, and replay remains the plugin's business, exactly as decided above. The field makes the dependency *expressible* by the config; it does not move ownership.

The two also do **not** layer, so this is not a free safety net to add everywhere. `cycles` is derived to drain the largest pool once, so a no-repeat rule marches a plugin pool through its whole returned set inside one window — which leaves the script's own recency suppression nothing to hold back and defeats the ranking the plugin exists to produce. The rule is: set `constraints` where the config is the only guard, leave it unset where the script is. `examples/samples/foryou.yaml` declines it for that reason (#116).

## What this does not decide

Per-user taste (#112) and on-screen attribution of who is watching (#113) are both deferred. There is no `taste_scope` field: the only behavior is the server-wide pooled history, so a config field with one legal value would do nothing. `ChannelConfig` has `deny_unknown_fields`, so #112 can add the field without invalidating configs written before it.
