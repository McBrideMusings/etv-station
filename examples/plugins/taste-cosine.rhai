// taste-cosine — ranks a movie pool by how close its subject matter sits to
// the house's pooled taste, not by whether the house has already watched it
// (#254), then damps whatever this channel aired recently (`ctx.recent`).
//
// Those two are different axes, and only the second is a repeat guard.
// #254's rule stands unchanged: a title someone loves and has rewatched five
// times should still rank at the top, so Plex watch state never lowers a
// score. What #254 did not cover is that the cosine is a pure function of
// (catalog, taste vector), and both are near-constant between generations —
// so the ranking came out identical every run and the head of it aired on a
// loop. The seeded exploration slot below does not help: its permutation and
// its coin flips are seeded off `ctx.seed` alone, which is also constant, so
// it varies picks WITHIN one returned list and never BETWEEN two of them.
//
// The recency factor is therefore the only input to this script that moves
// as the channel plays, and it is what makes a later generation reach
// further down the ranking instead of replaying the top of it. It damps
// rather than excludes: a title in the tail can still win if its cosine is
// far enough ahead, which is the judgement a hard cooldown window cannot
// express.
//
// Five functions make up the contract:
//   hooks()               -> [hook_name, …]                    which hooks this script implements
//   capabilities()        -> [capability_name, …]              which host capabilities this script needs
//   sources()             -> #{ name: cel_expression }         run once, up front
//   pick(ctx)             -> #{ picks: [...], workspace }      picks, most-wanted first (#166's
//                                                               widened shape), plus an opaque
//                                                               working set for audit() (#389,
//                                                               ADR 0013)
//   audit(ctx, picks, ws) -> #{ entry_id: stage_record }       explains each pick (#389, ADR 0011)
//
// **Movies or shows, one pool's `unit:` picks which.** TMDB keywords sit on
// movies and on shows, never on an episode directly. #274 made the catalog's
// `show_id` GUID-derived instead of title-derived, which is what lets this
// script resolve an episode to its show and score the show's keywords — that
// was the gap that made #254 ship movies-only, and it is closed. Verified
// against the live snapshot: all 1,633 distinct catalog `show_id`s match a
// `type='show'` row in the plex-db-ex store, 1,550 of them carrying
// tmdb_keywords.
//
// A shows pool therefore ranks SERIES, not episodes: an episode has no
// keywords to score, and ordering episodes by anything but season/episode
// would air a season shuffled.
//
// capabilities() declares catalog_read (for ctx.sets, built from sources()
// below), watch_history, and a named datastore grant (#181). The channel
// pool that names this script must grant all three under that pool's
// `capabilities:` / `datastores:`, or the load fails naming whichever is
// missing.
//
// ctx carries (see docs/schema.md § Pool `capabilities` / `datastores`):
//   ctx.sets.movies      the movie candidates sources() matched
//   ctx.pool             the name of the pool asking
//   ctx.target_count     how many items this generation needs
//   ctx.config           this pool's `config:` block, or an empty map
//   ctx.seed             the channel's resolved seed, already mixed with
//                        this pool's name (#255, ADR 0005) — the one source
//                        of reproducible entropy inside the sandbox
//   ctx.account_id       the numeric account id this channel is scoped to
//                        under `taste_scope: single_user` (#278, ADR 0005),
//                        or unit `()` on a pooled (`all_users`) channel —
//                        this script never resolves a username itself; the
//                        station already did, from watch history it holds
//                        and this sandbox cannot reach
//   ctx.recent           the entry_ids this channel aired most recently,
//                        oldest first and newest LAST, capped at the
//                        channel's `scoring.recent_depth` (default 200).
//                        Airings of THIS channel, not Plex watch events —
//                        `ctx.history` is the other thing and this script
//                        does not read it. Ambient: no capability gates it.
//   ctx.datastore(name)  a handle onto the plex-db-ex reader (#181),
//                        exposing enrichment_for_many / pooled_taste_vector
//                        among others

// Tunables. Written as functions, not `const`: a top-level constant is only
// in scope once the script's body has run, and the station calls
// capabilities()/sources()/pick() directly, never the body.
//
// `ctx.config` is always a map — an absent `config:` block arrives empty —
// so a missing key is unit and this lookup is unconditional.
fn tunable(ctx, key, fallback) {
    let v = ctx.config[key];
    if v == () { fallback } else { v }
}

// The share of output slots filled by a seeded random pick instead of by
// rank (#254, ADR 0005 — the one exception that widens ctx for this script).
// Not a fixed stride through the ranking: ADR 0005 rejected that shape
// because neither a band boundary nor a stride is a number anyone could
// derive or notice going stale. This is a per-slot coin flip instead, seeded
// so it reproduces, which lands ~1 slot in 5 off-profile at the default.
fn exploration_fraction(ctx) { tunable(ctx, "exploration_fraction", 0.2).to_float() }

// How many candidates ranked immediately above a pick get named in that
// pick's audit trail as near misses (#393). Bounded by the script, not the
// station — overridable per pool via `config:` like every other tunable
// here. A full candidate dump stays taste-debug's job; this is just enough
// to answer "what did this beat" without growing the chunk file per item.
fn near_miss_limit(ctx) { tunable(ctx, "near_miss_limit", 3).to_int() }

// How many airings back a title's damping is half spent. At the default 25,
// a channel laying ~16 movies a day has a title back to 89% of its cosine
// after a week and 96% after two.
//
// A half-life rather than a cutoff, and measured in POSITIONS rather than
// hours, because `ctx.recent` is a bare id list with no timestamps on it —
// see recency_factor() below. A dark day and a busy day therefore decay
// identically; widening the station's history tail to carry `start_ns` is
// what a true time decay would need.
fn recency_half_life(ctx) { tunable(ctx, "recency_half_life", 25.0).to_float() }

// How much of a candidate's cosine survives, given how recently this channel
// aired it. `r` is its position in the airing tail counted back from the end
// — 1 for the last thing that aired — or unit for a title the tail does not
// hold at all.
//
// r / (r + half_life): 1.0 for a title never aired, 0.5 for one exactly
// half_life airings ago, climbing back toward 1.0 beyond that. Monotonic, so
// two titles that aired at different times never swap on the factor alone,
// and bounded in (0, 1], so nothing is ever zeroed out by having aired.
//
// Takes two scalars, so calling it per candidate is free — the #172 rule it
// has to respect is about a helper taking a library-sized collection by
// value (Rhai passes arguments by value and would clone one per candidate),
// which this does not.
fn recency_factor(r, half_life) {
    if r == () { 1.0 } else { r.to_float() / (r.to_float() + half_life) }
}

// How many shows a `unit = "show"` pool hands back. Not `target_count`: a
// show expands to every episode it has, so asking for one show per scheduled
// item would return tens of thousands of ids. This is how many series the
// station's rotation gets to walk before it wraps — the ranking's whole
// effect on a shows pool, since the rotation follows the order units come
// back in.
//
// 12 is deliberately more than a day's rotation and far less than the
// library: enough that a channel does not cycle the same three shows, few
// enough that the 13th-ranked show has to earn its way up.
fn show_count(ctx) { tunable(ctx, "show_count", 12).to_int() }

// The datastore grant name this script reaches for (#181). A channel names
// its own grant under the pool's `datastores:`; this is this script's half
// of that contract.
fn datastore_name() { "taste" }

// The one enrichment namespace this scorer reads today. Named once so the
// exploration eligibility rule below reads "has a fact in a namespace this
// scorer reads" rather than "has TMDB keywords" — a second namespace this
// scorer grows into later is one more line here, not a rewritten rule.
fn enrichment_namespace() { "tmdb_keywords" }

fn hooks() { ["pool_provider"] }

fn capabilities() { ["catalog_read", "watch_history", #{ datastore: datastore_name() }] }

// Both sets this script can rank. A pool picks one with `unit:` in its
// `config:` and SHOULD narrow `sources:` to match — a pool that overrides
// neither resolves both sets, which on a shows pool means loading 11,543
// movies it will never look at.
fn sources() {
    #{
        movies: `item.type == "movie"`,
        shows: `item.type == "episode"`,
    }
}

// A fixed integer hash mix, kept inside the 32-bit range at every step so
// Rhai's checked arithmetic (this build carries no `unchecked` feature)
// never sees a multiply overflow. Two masked xorshift-multiply rounds are
// enough diffusion for a reproducible coin flip; nothing here claims
// cryptographic quality.
fn mix32(x) {
    let v = x & 0xffffffff;
    v = ((v ^ (v >> 16)) * 0x45d9f3b) & 0xffffffff;
    v = ((v ^ (v >> 16)) * 0x45d9f3b) & 0xffffffff;
    v ^ (v >> 16)
}

// A small string hash, masked to 32 bits at every step for the same
// overflow reason as mix32 — used only to turn a candidate's own entry_id
// into a nonce for unit_at, never as an identity of its own.
fn hash_str(s) {
    let h = 0;
    for ch in s {
        h = ((h * 131) + ch.to_int()) & 0xffffffff;
    }
    h
}

// A reproducible draw in [0, 1) for one (seed, nonce) pair. `seed` is
// `ctx.seed`; `nonce` separates one draw from another — a slot position, or
// a candidate's own id hash — so their draws are independent of each other.
fn unit_at(seed, nonce) {
    let h = mix32((seed & 0xffffffff) ^ mix32(nonce));
    h.to_float() / 4294967296.0
}

fn pick(ctx) {
    let ds = ctx.datastore(datastore_name());
    let ns = enrichment_namespace();

    // WHAT ONE ROW OF THE RANKING IS, and which catalog entries air when that
    // row is picked.
    //
    //   unit = "movie"  a unit is one film; airing it airs that one entry.
    //   unit = "show"   a unit is a SERIES, scored on the show's own
    //                   keywords, and airing it makes every episode of that
    //                   show available in season/episode order.
    //
    // Everything between here and the emit loop operates on units and knows
    // nothing about which kind it has: one document-frequency pass, one
    // cosine, one recency damping, one exploration draw. Only this block and
    // the emit loop differ, which is why shows did not need a second script.
    //
    // Read from `config:` rather than inferred from which set came back: a
    // pool that does not override `sources:` receives BOTH sets, so
    // inference would have to guess, and guessing wrong silently ranks the
    // wrong library. A channel says which it wants.
    let unit_kind = tunable(ctx, "unit", "movie");
    let units = [];
    let members = #{};

    // The airing tail, as a distance back from the newest: 1 is the last
    // thing that aired, `recent.len` the oldest still remembered.
    // `ctx.recent` is newest-LAST, so the distance is measured from the end.
    // Bounded by `scoring.recent_depth` (200 by default), never by the
    // library.
    let recent = ctx.recent;
    let recent_n = recent.len;
    let aired = #{};
    for i in 0..recent_n {
        // A title aired more than once inside the tail keeps its most recent
        // position: later iterations overwrite earlier ones, and later means
        // newer.
        aired[recent[i]] = recent_n - i;
    }
    // The same distance, per UNIT. For a show that means its most recently
    // aired episode: damping keys on the series, not the episode, because a
    // specific episode almost never repeats — damping the episode id would
    // do nothing at all, while damping the series is exactly "Columbo has
    // been on a lot lately".
    let unit_recency = #{};

    if unit_kind == "show" {
        // Episodes carry `show_id` (registered on every item in score.rs),
        // and since #274 it is GUID-derived — the same id space plex-db-ex
        // keys its `items` rows on, which is what makes this join possible
        // at all. Verified against the live snapshot: all 1,633 distinct
        // show_ids in the catalog match a `type='show'` row there, 1,550 of
        // them carrying tmdb_keywords.
        //
        // One pass over the episodes, grouping each under its show. An
        // episode with no `show_id` is dropped rather than aired as a series
        // of one — a shows pool exists to air series.
        //
        // Members are pushed in place (`members[sid].push(...)`) rather than
        // read-modify-written: on a 74,815-episode catalog the latter clones
        // a growing array per episode, which is #172's cost all over again.
        // Each member is a compact `[season, episode, entry_id]` triple, not
        // the whole item map, for the same reason.
        for ep in ctx.sets.shows {
            let sid = ep.show_id;
            if sid == () { continue; }
            if members[sid] == () {
                units.push(sid);
                members[sid] = [];
            }
            members[sid].push([
                if ep.season == () { -1 } else { ep.season },
                if ep.episode == () { -1 } else { ep.episode },
                ep.entry_id,
            ]);
            // Roll this episode's airing distance up to its show, keeping the
            // most recent. Checked inside the pass that already touches every
            // episode, so no second walk of the library is needed to find
            // which shows the 200-entry tail belongs to.
            let d = aired[ep.entry_id];
            if d != () {
                let cur = unit_recency[sid];
                if cur == () || d < cur { unit_recency[sid] = d; }
            }
        }
    } else {
        for item in ctx.sets.movies {
            let id = item.entry_id;
            units.push(id);
            members[id] = [[-1, -1, id]];
            let d = aired[id];
            if d != () { unit_recency[id] = d; }
        }
    }

    // Every unit's id, gathered once so enrichment can be fetched in a
    // single bulk call before the loop below (plex-db-ex#40) — #172 measured
    // the per-candidate alternative at 101 million steps and 6m34s on an
    // 84,722-item library. On a shows pool this asks for 1,633 show ids
    // rather than 74,815 episode ids, since an episode carries no keywords
    // of its own.
    let enrichment = ds.enrichment_for_many(units, ns);

    // One account's taste vector when the channel is `single_user`-scoped
    // (#278) and the station resolved that account's id; the house's pooled
    // vector otherwise (#254, plex-db-ex#39) — one call either way, not one
    // per candidate. This branch is the only difference a personal channel
    // makes to this script: the cosine, the exploration slot and the
    // eligibility rule below are identical for "For You" and "For Pierce".
    // The station resolves the id, never this script — see ctx.account_id
    // above.
    let taste = if ctx.account_id != () {
        ds.taste_vector_for(ctx.account_id)
    } else {
        ds.pooled_taste_vector()
    };
    let weights = #{};
    for attr in taste.attributes {
        if attr.namespace == ns {
            weights[attr.key + "\x01" + attr.value] = attr.weight;
        }
    }

    let seed = ctx.seed;

    let half_life = recency_half_life(ctx);

    // Document-frequency pass (#294): how many candidates carry each
    // keyword, and how many candidates carry any `ns` fact at all. Feeds the
    // idf factor below, which is what keeps a title with one keyword shared
    // by every candidate in the pool from tying a title with several rare,
    // distinctive ones.
    //
    // This pass is written out here rather than called as a helper, and so
    // is the scoring loop below it — that is a performance decision: Rhai
    // passes arguments by value, so a helper taking the fetched enrichment
    // map or the weights map would clone one of them on every candidate —
    // #172 measured exactly this shape of helper call at 124s against 13s
    // for the same arithmetic inlined, on an 84,722-item library.
    // `mix32`/`hash_str`/`unit_at` above take only a handful of scalars and
    // one short string each, so calling them per candidate below costs
    // nothing comparable — nothing here holds a copy of a library-sized
    // collection.
    let df = #{};
    let doc_count = 0;
    for unit in units {
        let facts = enrichment[unit];
        if facts != () {
            doc_count += 1;
            for f in facts {
                if f.namespace == ns {
                    let k = f.key + "\x01" + f.value;
                    let c = df[k];
                    df[k] = if c == () { 1 } else { c + 1 };
                }
            }
        }
    }

    let scored = [];
    for id in units {
        let facts = enrichment[id];
        let score = 0.0;
        let eligible = false;
        // The specific keywords this candidate shares with the pooled
        // profile — not needed for ranking, only so a ranking can be
        // explained: see the `metadata` attached below.
        let on_profile = [];
        if facts != () {
            eligible = true;
            let n = 0;
            let sum = 0.0;
            for f in facts {
                if f.namespace == ns {
                    n += 1;
                    let k = f.key + "\x01" + f.value;
                    let w = weights[k];
                    if w != () {
                        // idf: a keyword every candidate carries scales by
                        // exactly 1.0 (doc_count / df == 1, ln == 0); a
                        // keyword only a few candidates carry scales up —
                        // ln(doc_count / df) grows as df shrinks. df[k] is
                        // always >= 1 here, since this very candidate
                        // contributed to it above, so no divide-by-zero.
                        sum += w * (1.0 + (doc_count.to_float() / df[k].to_float()).ln());
                        on_profile.push(f.value);
                    }
                }
            }
            // Cosine: the sum of idf-scaled pooled weights over the
            // candidate's own in-namespace keywords, divided by the square
            // root of that same keyword count — not the raw sum (a
            // heavily-tagged title would win on tag count alone, regardless
            // of subject) and not the plain mean (one lucky keyword would
            // tie the maximum possible score, and a fifth of this library's
            // movies have no keywords at all). The idf factor above is what
            // keeps a title whose one keyword sits on every candidate from
            // tying a title with several keywords few candidates share
            // (#294) — the sqrt divisor is unchanged and still does its own
            // job of not over-rewarding tag count. See #254's worked table.
            if n > 0 { score = sum / n.to_float().sqrt(); }
        }
        // The cosine before recency, kept alongside the damped one so the
        // audit can show both. A title that fell twenty places because it
        // aired last night only reads as a decision if both halves are
        // visible; one number alone reads as the scorer changing its mind.
        let base_score = score;
        let aired_rank = unit_recency[id];
        let factor = recency_factor(aired_rank, half_life);
        score = base_score * factor;
        scored.push(#{
            id: id,
            score: score,
            base_score: base_score,
            // Unit for a title this channel's remembered tail does not hold
            // — which is most of them, and is what `explorable` below tests.
            aired_rank: aired_rank,
            recency_factor: factor,
            eligible: eligible,
            on_profile: on_profile,
            // The seeded key this candidate draws if an exploration slot
            // reaches it — computed once here, not per slot, since a
            // candidate's own id and the seed are all it depends on.
            explore_key: unit_at(seed, hash_str(id)),
        });
    }

    // Highest score first; entry_id breaks a tie so the same inputs always
    // produce the same order.
    scored.sort(|a, b| {
        if a.score > b.score { -1 }
        else if a.score < b.score { 1 }
        else if a.id < b.id { -1 }
        else if a.id > b.id { 1 }
        else { 0 }
    });

    // Stamp each row's ranked position before anything copies the list —
    // `explorable` below is built by `.filter()`, which copies rows, so an
    // exploration draw only knows its own rank if it is already stamped on
    // when that copy is taken. One inline pass over `scored`, not a second
    // id -> rank map (#172's rule: no per-candidate helper taking a
    // library-sized collection by value).
    for i in 0..scored.len {
        scored[i].rank = i + 1;
    }

    // The candidates an exploration slot may draw from — only ones the
    // store has at least one `tmdb_keywords` fact for (#254). A title with
    // none is not "on-profile with weight zero"; it is a title this scorer
    // cannot say anything about, and handing it to a surprise slot reads as
    // the recommender being broken rather than as a surprise.
    //
    // Anything in the airing tail is excluded outright here rather than
    // damped. The exploration slot draws in a fixed seeded order that
    // ignores score, so damping buys it nothing — and spending the one
    // off-profile slot in five on a title from three days ago is the exact
    // failure this whole change is about. The rank cursor is unaffected and
    // still sees every candidate. If the tail swallows the entire eligible
    // set on a small channel this list runs empty and every slot falls
    // through to the ranking, which the loop below already handles.
    let explorable = scored.filter(|row| row.eligible && row.aired_rank == ());
    // A fixed random permutation of the eligible candidates — the order an
    // exploration slot draws from, one at a time, seeded so it reproduces.
    explorable.sort(|a, b| {
        if a.explore_key < b.explore_key { -1 }
        else if a.explore_key > b.explore_key { 1 }
        else if a.id < b.id { -1 }
        else if a.id > b.id { 1 }
        else { 0 }
    });

    let fraction = exploration_fraction(ctx);
    // How many UNITS to hand back. For movies a unit is one film, so this is
    // the generation's item count plus slack, so a pattern that draws
    // unevenly across pools does not run short. For shows a unit expands to
    // a whole series, so what matters instead is how many series the
    // rotation gets to walk — see show_count() above.
    let want = if unit_kind == "show" {
        show_count(ctx)
    } else {
        ctx.target_count + 10
    };

    let out = [];
    let ws_rows = #{};
    let used = #{};
    let rank_cursor = 0;
    let explore_cursor = 0;
    let picked = 0;
    while picked < want {
        // Which slot this is — one UNIT lands per pass, so the count already
        // picked is the slot's own index, and it is the nonce that makes
        // each slot's coin flip independent of its neighbours'.
        let slot = picked;
        // One slot in five, at the default fraction — decided by a seeded
        // coin flip for this slot, not a fixed stride through the ranking
        // (see exploration_fraction() above for why).
        let row = ();
        let source = "rank";
        if unit_at(seed, slot) < fraction {
            while explore_cursor < explorable.len && used[explorable[explore_cursor].id] != () {
                explore_cursor += 1;
            }
            if explore_cursor < explorable.len {
                row = explorable[explore_cursor];
                source = "explore";
                explore_cursor += 1;
            }
        }
        if row == () {
            // Either this slot did not draw explore, or the eligible pool
            // is exhausted — either way, fall through to the next
            // best-ranked candidate not yet used.
            while rank_cursor < scored.len && used[scored[rank_cursor].id] != () {
                rank_cursor += 1;
            }
            if rank_cursor < scored.len {
                row = scored[rank_cursor];
                rank_cursor += 1;
            }
        }
        if row == () {
            // Both cursors exhausted — every candidate has already been
            // returned. Nothing left to add.
            break;
        }
        used[row.id] = true;
        picked += 1;
        // The candidates ranked immediately above this pick, captured right
        // now — while `used` reflects exactly what has aired so far in this
        // generation, and before anything copies or truncates `scored`
        // (#392's carried hazard). Capturing this later, e.g. by re-walking
        // `scored` inside audit(), would read `used` as it looks at the END
        // of the generation and mislabel a candidate that airs in a later
        // slot as "already placed" here. Bounded by construction: the walk
        // stops at near_miss_limit(ctx) entries, so nothing library-sized is
        // ever built and then thrown away (#393).
        let near_misses = [];
        let nm_idx = row.rank - 2;
        let nm_limit = near_miss_limit(ctx);
        while nm_idx >= 0 && near_misses.len < nm_limit {
            let cand = scored[nm_idx];
            let reason = if used[cand.id] != () {
                "already placed by an earlier slot in this generation"
            } else {
                "passed over: this slot drew the seeded exploration coin flip"
            };
            near_misses.push(#{ id: cand.id, score: cand.score, reason: reason });
            nm_idx -= 1;
        }
        // The score, the specific on-profile keywords behind it, and
        // whether this slot came from the ranking or the seeded surprise
        // draw travel along as `metadata` (#166) so a pick can be explained
        // rather than just trusted — never read back by etv-station itself
        // (ADR 0002: the score stays internal to the plugin), only carried
        // through untouched to the emitted playout JSON.
        //
        // The numbers audit() will report (#392): the unit's score, its
        // position in the full ranked set, and whether this slot drew it by
        // rank or by the seeded exploration draw. `near_misses` is #393: the
        // bounded list captured above. One record per unit, attached below to
        // every entry that unit emits.
        let stage = #{
            score: row.score,
            base_score: row.base_score,
            recency_factor: row.recency_factor,
            aired_rank: row.aired_rank,
            rank: row.rank,
            // Which series was ranked, on a shows pool — unit on a movies
            // pool, where the entry and the unit are the same thing. Without
            // it an audit line reads as though the episode itself scored 7.7,
            // when what scored is the show it belongs to.
            unit: if unit_kind == "show" { row.id } else { () },
            draw: if source == "explore" { "exploration" } else { "ranked" },
            near_misses: near_misses,
        };
        // Emit the unit's members. A movie is its own single member; a show
        // hands back every episode it has, in season/episode order.
        //
        // That order IS broadcast order. The station will not re-sort what a
        // plugin returns, and it groups the returned list into series by
        // first appearance (pattern.rs) — so the order units were ranked in
        // becomes the series rotation, and the order inside a unit becomes
        // the order its episodes play.
        //
        // A show hands back ALL of its episodes rather than a slice because
        // `advance: resume` finds the last-aired episode in this list and
        // continues past it: a list holding only the first few would never
        // contain the episode the ledger resumes from, and the show would
        // restart from episode one forever. Sorting happens here, for the
        // handful of shows actually picked, not for all 1,633 of them.
        let member_rows = members[row.id];
        if unit_kind == "show" {
            member_rows.sort(|a, b| {
                if a[0] < b[0] { -1 } else if a[0] > b[0] { 1 }
                else if a[1] < b[1] { -1 } else if a[1] > b[1] { 1 }
                else if a[2] < b[2] { -1 } else if a[2] > b[2] { 1 }
                else { 0 }
            });
        }
        for m in member_rows {
            let mid = m[2];
            // The score, the specific on-profile keywords behind it, and
            // whether this slot came from the ranking or the seeded surprise
            // draw travel along as `metadata` (#166) so a pick can be
            // explained rather than just trusted — never read back by
            // etv-station itself (ADR 0002: the score stays internal to the
            // plugin), only carried through untouched to the playout JSON.
            out.push(#{
                entry_id: mid,
                metadata: #{ score: row.score, on_profile_keywords: row.on_profile, source: source },
            });
            // Keyed by the EMITTED entry_id, since audit() is handed the
            // picks array and looks each one up by that — an episode, not the
            // show it was ranked as.
            ws_rows[mid] = stage;
        }
    }

    #{ picks: out, workspace: #{ rows: ws_rows, candidate_count: scored.len } }
}

// The fifth pool_provider contract function (#389, ADR 0011, ADR 0013):
// called once per generation, immediately after pick(), handed back the list
// pick() returned plus the workspace pick() built above. Reports the score,
// the undamped cosine and the recency factor multiplied into it, the
// rank, candidate-set size, and draw kind pick() already computed for each
// pick — an exploration draw is a deliberate product state this channel is
// FOR, not a fallback, so it gets its own verdict rather than reading like a
// low-ranked accident. Also reports the bounded near-miss list pick()
// captured for each pick (#393) — the candidates ranked immediately above it
// and the script's own reason each one did not air.
fn audit(ctx, picks, workspace) {
    let out = #{};
    for p in picks {
        let r = workspace.rows[p.entry_id];
        let detail = if r != () {
            #{
                score: r.score,
                // The cosine before the recency factor, and the factor
                // itself, so `admin audit` can show why a pick ranks where
                // it does rather than only that it does. `aired_rank` is
                // unit for a title outside the channel's remembered tail,
                // which is the common case and the one where the factor is
                // exactly 1.0.
                base_score: r.base_score,
                recency_factor: r.recency_factor,
                aired_rank: r.aired_rank,
                // The series this entry was ranked as, on a shows pool.
                // Unit on a movies pool, where the entry IS the unit.
                unit: r.unit,
                rank: r.rank,
                candidate_count: workspace.candidate_count,
                draw: r.draw,
                near_misses: r.near_misses,
            }
        } else {
            #{}
        };
        // An exploration draw never carries an `aired_rank` — `explorable`
        // excludes the airing tail outright — so these three branches are
        // mutually exclusive and the order between them does not matter.
        let verdict = if r != () && r.draw == "exploration" {
            "drawn off-profile by the exploration slot"
        } else if r != () && r.aired_rank != () {
            "ranked by keyword cosine, damped for a recent airing on this channel"
        } else {
            "ranked by keyword cosine against pooled taste"
        };
        out[p.entry_id] = #{
            stage: "pool",
            by: "plugin:taste-cosine",
            verdict: verdict,
            detail: detail,
        };
    }
    out
}
