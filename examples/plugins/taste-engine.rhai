// taste-engine — the worked example scorer plugin (#74).
//
// Ranks a pool by what the server has been watching lately, biased toward
// things nobody has seen in a while and away from anything this channel just
// aired. Every number below is this script's opinion, not the station's: swap
// this file for another and nothing in etv-station changes.
//
// Two functions make up the contract:
//   sources() -> #{ name: cel_expression }   run once, up front
//   pick(ctx) -> [entry_id, …]               most-wanted first
//
// ctx carries:
//   ctx.sets.<name>   the items each source matched (every catalog column,
//                     plus genres / cast / labels / … as arrays)
//   ctx.target_count  how many items this generation needs
//   ctx.history       [#{entry_id, watched_at}] recent server-wide watches
//   ctx.recent        entry_ids this channel aired most recently, oldest first
//   ctx.now           unix seconds at generation time

// Tunables. Written as functions, not `const`, because a top-level constant is
// only in scope once the script's body has run — and the station calls
// `sources()` and `pick()` directly, so the body never does.
fn day() { 86400 }

// How long a watch keeps boosting a show. Watching one episode of something
// says "I am interested in this" for about a fortnight.
fn affinity_window_days() { 14 }

// How long an airing suppresses a repeat. Nothing this channel played inside
// this window comes back.
fn replay_ttl_days() { 30 }

fn sources() {
    #{
        movies: `item.type == "movie"`,
        episodes: `item.type == "episode"`,
    }
}

fn pick(ctx) {
    let scored = [];
    for name in ctx.sets.keys() {
        for item in ctx.sets[name] {
            // Kept as two statements: an object literal whose value is itself a
            // call trips Rhai's expression-complexity limit.
            let s = score_item(item, ctx);
            scored.push(#{ id: item.entry_id, score: s });
        }
    }

    // Rhai's comparator returns an ordering, not a bool: negative keeps `a`
    // first. Highest score first, entry_id breaking ties so the same inputs
    // always produce the same order.
    scored.sort(|a, b| {
        if a.score > b.score { -1 }
        else if a.score < b.score { 1 }
        else if a.id < b.id { -1 }
        else if a.id > b.id { 1 }
        else { 0 }
    });

    let out = [];
    for row in scored {
        if row.score > 0.0 { out.push(row.id); }
    }

    // Ask for a little more than the generation needs, so a pattern that draws
    // unevenly across pools does not run short.
    out.truncate(ctx.target_count + 10);
    out
}

fn score_item(item, ctx) {
    // Anything this channel aired recently is out entirely. `ctx.recent` is
    // ordered oldest-first, so a position near the end is a very recent airing.
    let aired_at = index_of_recent(ctx.recent, item.entry_id);
    if aired_at >= 0 {
        let staleness = ctx.recent.len - aired_at;
        if staleness < replay_ttl_days() { return 0.0; }
    }

    let score = 1.0;

    // Watched lately, or belongs to a show watched lately → surface it. An
    // episode inherits its show's affinity, which is what makes a show somebody
    // started keep showing up.
    let affinity = watch_affinity(item, ctx);
    score += affinity * 3.0;

    // Newly added earns a look even with no watch signal behind it.
    if item.year != () && item.year >= 2024 { score += 0.5; }

    // Episodes early in a series are cheaper to start than ones deep in it —
    // a new viewer joining at S4E11 has nothing to hold onto.
    if item.season != () && item.season == 1 { score += 0.25; }

    score
}

// How strongly the server's recent watching points at this item, 0.0–1.0,
// decaying linearly over affinity_window_days().
fn watch_affinity(item, ctx) {
    let best = 0.0;
    for event in ctx.history {
        if event.entry_id != item.entry_id { continue; }
        let age_days = (ctx.now - event.watched_at) / day();
        if age_days >= affinity_window_days() { continue; }
        let window = affinity_window_days().to_float();
        let weight = 1.0 - (age_days.to_float() / window);
        if weight > best { best = weight; }
    }
    best
}

fn index_of_recent(recent, id) {
    let i = 0;
    for entry in recent {
        if entry == id { return i; }
        i += 1;
    }
    -1
}
