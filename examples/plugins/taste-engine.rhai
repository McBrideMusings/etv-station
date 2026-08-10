// taste-engine — the worked example scorer plugin (#74).
//
// Ranks a pool by what the server has been watching lately, biased toward
// things nobody has seen in a while and away from anything this channel just
// aired. Every number below is this script's opinion, not the station's: swap
// this file for another and nothing in etv-station changes.
//
// Four functions make up the contract:
//   hooks()         -> [hook_name, …]              which hooks this script implements
//   capabilities()  -> [capability_name, …]        which host capabilities this script needs
//   sources()       -> #{ name: cel_expression }   run once, up front
//   pick(ctx)       -> [entry_id, …]               most-wanted first
//
// hooks() is read at config load time, before the catalog exists — it names
// this script a pool_provider, which is the only hook a `plugin:` pool has
// ever meant (#159). Declaring it is what lets a channel that misuses this
// script (wants a hook it doesn't implement) fail at load instead of on air.
//
// capabilities() is read at the same time, alongside hooks() (#167). This
// script reaches for ctx.sets (via sources()) and ctx.history below, so it
// declares catalog_read and watch_history — and the channel pointing a pool
// at this file must grant both under that pool's `capabilities:`, or the
// load fails naming whichever is missing. Nothing else changed to add this:
// pick() and sources() are exactly what they were before #167.
//
// ctx carries:
//   ctx.sets.<name>   the items each source matched (every catalog column,
//                     plus genres / cast / labels / … as arrays)
//   ctx.pool          the name of the pool asking — one script can serve
//                     several pools of the same channel
//   ctx.target_count  how many items this generation needs
//   ctx.history       [#{entry_id, watched_at}] recent server-wide watches
//   ctx.recent        entry_ids this channel aired most recently, oldest first
//   ctx.now           unix seconds at generation time

// Tunables. Written as functions, not `const`, because a top-level constant is
// only in scope once the script's body has run — and the station calls
// `sources()` and `pick()` directly, so the body never does.
//
// Each one reads the pool's `config:` block first and falls back to the value
// written here. The station passes that block through without looking at it, so
// these key names are this script's vocabulary and nothing else's — a channel
// that sets `affinity_window_days:` is talking to this file, not to ETV. A key
// this script does not read is ignored, and a mistyped one reads as unset and
// silently takes the fallback below.
fn day() { 86400 }

// `ctx.config` is always a map — an absent `config:` block arrives empty — so a
// missing key is unit and every lookup below can be unconditional.
fn tunable(ctx, key, fallback) {
    let v = ctx.config[key];
    if v == () { fallback } else { v }
}

// How long a watch keeps boosting a show. Watching one episode of something
// says "I am interested in this" for about a fortnight.
fn affinity_window_days(ctx) { tunable(ctx, "affinity_window_days", 14) }

// How long an airing suppresses a repeat. Nothing this channel played inside
// this window comes back.
fn replay_ttl_days(ctx) { tunable(ctx, "replay_ttl_days", 30) }

fn hooks() { ["pool_provider"] }

fn capabilities() { ["catalog_read", "watch_history"] }

fn sources() {
    #{
        movies: `item.type == "movie"`,
        episodes: `item.type == "episode"`,
    }
}

fn pick(ctx) {
    // One script serves several pools of the same channel, so the pool asking
    // decides which half of the library it gets. A pool named anything else
    // gets everything, which is the sensible default for a single-pool channel.
    let wanted = if ctx.pool == "movies" { ["movies"] }
                 else if ctx.pool == "shows" { ["episodes"] }
                 else { ctx.sets.keys() };

    // Both lookups the scoring loop needs, built once here as maps keyed by
    // entry_id.
    //
    // They used to be linear scans *inside* the loop — `index_of_recent` walked
    // the whole aired tail and `watch_affinity` walked the whole watch history,
    // once per item. On a library of 84,722 entries against 1,000 history rows
    // and a 200-deep tail that is about 101 million interpreted steps to choose
    // four things, and it measured at 6 minutes 34 seconds. Neither scan ever
    // depended on the item — only on its id — so both collapse to a map read and
    // the whole pass is 1,200 steps plus one lookup per item.
    let watched = #{};
    for event in ctx.history {
        let prev = watched[event.entry_id];
        // Keep the most recent watch of each entry: that is the one
        // `watch_affinity` would have picked as its strongest signal.
        if prev == () || event.watched_at > prev {
            watched[event.entry_id] = event.watched_at;
        }
    }

    // Position in the recently-aired tail, oldest first — the number
    // `index_of_recent` returned, and -1 for anything absent.
    //
    // One deliberate difference: an id that appears twice in the tail keeps its
    // *last* position here, where the old linear search returned the first. The
    // last one is the more recent airing, which is what the suppression below is
    // actually asking about — the old order let a thing aired twice look staler
    // than it was and replay too soon.
    let aired_at = #{};
    let i = 0;
    for entry in ctx.recent {
        aired_at[entry] = i;
        i += 1;
    }

    // Every tunable read once, as a plain number, before the loop starts.
    let window_days = affinity_window_days(ctx);
    let replay_ttl  = replay_ttl_days(ctx);
    let sec_per_day = day();
    let recent_len  = ctx.recent.len;
    let now         = ctx.now;
    let window      = window_days.to_float();

    // The scoring is written out here rather than called as `score_item(item,
    // ctx, …)`, and that is a performance decision, not a style one.
    //
    // Rhai passes arguments by value: a call copies each one. `ctx` holds the
    // whole watch history and the whole aired tail, so every helper call cloned
    // both — and the old shape made about five of those per item (`score_item`,
    // `watch_affinity`, and a `tunable` lookup inside each). Across 84,722 items
    // that copying, not the scoring, was where the time went: measured against
    // this library it is 124 s with the helpers and 13 s with the same
    // arithmetic inline.
    //
    // Anything genuinely per-channel still lives in the named functions above.
    // They are called once each, right here, where a copy costs nothing.
    let scored = [];
    for name in wanted {
        for item in ctx.sets[name] {
            let id = item.entry_id;

            // Where this item sits in the recently-aired tail: both the score
            // and the all-suppressed fallback below need it. -1 for absent.
            let a = aired_at[id];
            if a == () { a = -1; }

            let s = 0.0;
            // Anything this channel aired recently is out entirely. `ctx.recent`
            // is ordered oldest-first, so a position near the end is a very
            // recent airing.
            if a >= 0 && (recent_len - a) < replay_ttl {
                s = 0.0;
            } else {
                s = 1.0;

                // Watched lately, or belongs to a show watched lately → surface
                // it. An episode inherits its show's affinity, which is what
                // makes a show somebody started keep showing up. The newest
                // watch is always the strongest signal, since the weight decays
                // with age, so the newest is the only one worth reading.
                let last = watched[id];
                if last != () {
                    let age_days = (now - last) / sec_per_day;
                    if age_days < window_days {
                        s += (1.0 - (age_days.to_float() / window)) * 3.0;
                    }
                }

                // Newly added earns a look even with no watch signal behind it.
                if item.year != () && item.year >= 2024 { s += 0.5; }

                // Episodes early in a series are cheaper to start than ones deep
                // in it — a new viewer joining at S4E11 has nothing to hold onto.
                if item.season != () && item.season == 1 { s += 0.25; }
            }

            scored.push(#{ id: id, score: s, aired: a });
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

    // Everything was suppressed — a library small enough that the replay window
    // covers all of it. Falling through with nothing would fail the generation,
    // so play the stalest thing instead: suppression is a preference, not a
    // reason to take the channel off the air. Ranked by how long ago each
    // aired, longest-ago first.
    if out.is_empty() {
        let stale = [];
        for row in scored { stale.push(row); }
        stale.sort(|a, b| {
            if a.aired < b.aired { -1 }
            else if a.aired > b.aired { 1 }
            else if a.id < b.id { -1 }
            else if a.id > b.id { 1 }
            else { 0 }
        });
        for row in stale { out.push(row.id); }
    }

    // Ask for a little more than the generation needs, so a pattern that draws
    // unevenly across pools does not run short.
    out.truncate(ctx.target_count + 10);
    out
}
