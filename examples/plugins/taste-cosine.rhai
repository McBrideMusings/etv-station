// taste-cosine — ranks a movie pool by how close its subject matter sits to
// the house's pooled taste, not by whether the house has already watched it
// (#254).
//
// Four functions make up the contract:
//   hooks()         -> [hook_name, …]                 which hooks this script implements
//   capabilities()  -> [capability_name, …]           which host capabilities this script needs
//   sources()       -> #{ name: cel_expression }      run once, up front
//   pick(ctx)       -> [#{ entry_id, metadata }, …]   most-wanted first (#166's widened shape)
//
// **Movies only.** TMDB keywords sit on movies and on shows, never on an
// episode directly, and a plugin cannot currently resolve an episode to its
// show's keywords — that gap is #274 and is not a prerequisite here.
// `sources()` below offers only `movies`, so this script never sees an
// episode regardless of what a channel grants it.
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

fn sources() {
    #{
        movies: `item.type == "movie"`,
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
    let items = ctx.sets.movies;
    let ds = ctx.datastore(datastore_name());
    let ns = enrichment_namespace();

    // Every candidate's id, gathered once so enrichment can be fetched in a
    // single bulk call before the loop below (plex-db-ex#40) — #172 measured
    // the per-candidate alternative at 101 million steps and 6m34s on an
    // 84,722-item library.
    let enrichment = ds.enrichment_for_many(items.map(|item| item.entry_id), ns);

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

    // The scoring is written out here rather than called as a helper, and
    // that is a performance decision: Rhai passes arguments by value, so a
    // helper taking the fetched enrichment map or the weights map would
    // clone one of them on every candidate — #172 measured exactly this
    // shape of helper call at 124s against 13s for the same arithmetic
    // inlined, on an 84,722-item library. `mix32`/`hash_str`/`unit_at` above
    // take only a handful of scalars and one short string each, so calling
    // them per candidate below costs nothing comparable — nothing here
    // holds a copy of a library-sized collection.
    let scored = [];
    for item in items {
        let id = item.entry_id;
        let facts = enrichment[id];
        let score = 0.0;
        let eligible = false;
        // The specific keywords this candidate shares with the pooled
        // profile — not needed for ranking, only so a ranking can be
        // explained: see the `metadata` attached below.
        let on_profile = [];
        if facts != () {
            eligible = true;
            let n = facts.len;
            let sum = 0.0;
            for f in facts {
                if f.namespace == ns {
                    let w = weights[f.key + "\x01" + f.value];
                    if w != () {
                        sum += w;
                        on_profile.push(f.value);
                    }
                }
            }
            // Cosine: the sum of pooled weights over the candidate's own
            // keywords, divided by the square root of its keyword count —
            // not the raw sum (a heavily-tagged title would win on tag
            // count alone, regardless of subject) and not the plain mean
            // (one lucky keyword would tie the maximum possible score, and
            // a fifth of this library's movies have no keywords at all).
            // See #254's worked table.
            if n > 0 { score = sum / n.to_float().sqrt(); }
        }
        scored.push(#{
            id: id,
            score: score,
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

    // The candidates an exploration slot may draw from — only ones the
    // store has at least one `tmdb_keywords` fact for (#254). A title with
    // none is not "on-profile with weight zero"; it is a title this scorer
    // cannot say anything about, and handing it to a surprise slot reads as
    // the recommender being broken rather than as a surprise.
    let explorable = scored.filter(|row| row.eligible);
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
    // Ask for a little more than the generation needs, so a pattern that
    // draws unevenly across pools does not run short.
    let out_len = ctx.target_count + 10;

    let out = [];
    let used = #{};
    let rank_cursor = 0;
    let explore_cursor = 0;
    while out.len < out_len {
        // Which slot this is — one row lands per pass, so the count already
        // written is the slot's own index, and it is the nonce that makes
        // each slot's coin flip independent of its neighbours'.
        let slot = out.len;
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
        // The score, the specific on-profile keywords behind it, and
        // whether this slot came from the ranking or the seeded surprise
        // draw travel along as `metadata` (#166) so a pick can be explained
        // rather than just trusted — never read back by etv-station itself
        // (ADR 0002: the score stays internal to the plugin), only carried
        // through untouched to the emitted playout JSON.
        out.push(#{
            entry_id: row.id,
            metadata: #{ score: row.score, on_profile_keywords: row.on_profile, source: source },
        });
    }

    out
}
