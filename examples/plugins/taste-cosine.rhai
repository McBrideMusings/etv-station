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
// **A pool steers the ranking with a taste profile** (etv-station-sctf): signed
// weights on keywords, catalog tags, single items and CEL-defined sets,
// authored as the pool's `profile:` / `profile_files:` and resolved by the
// station into `ctx.profile`. The station resolves each reference; this script
// does all the weight math (ADR 0002). Entries are summed per (namespace,
// value); an item or set entry spreads its weight over its members' keywords
// and genres by the share of members carrying each. What nets positive becomes
// the FAVOR map, added on top of the score; what nets negative becomes the
// DISFAVOR map, added underneath it — so no weight, however negative, can drive
// a score below zero. Nothing is gated: a title the profile says nothing about
// competes on taste alone. See the score assembly in pick().
//
// capabilities() declares catalog_read (for ctx.sets, built from sources()
// below), watch_history, and a named datastore grant (#181). The channel
// pool that names this script must grant all three under that pool's
// `capabilities:` / `datastores:`, or the load fails naming whichever is
// missing.
//
// ctx carries (see docs/schema.md § Pool `capabilities` / `datastores`):
//   ctx.sets.movies      the movie candidates sources() matched
//   ctx.profile          the pool's resolved taste profile, one map per entry
//                        (kind, namespace, value, items, weight, reference,
//                        origin), files first then inline — empty when the
//                        pool authored none
//   ctx.exclude_keywords the pool's `exclude_keywords:`, already normalized
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

// How much the account's watch-history taste counts against the taste
// profile. The taste vector is divided by its own strongest weight before
// scoring, so its top keyword weighs exactly 1.0 on every account — which is
// what lets a hand-authored profile weight of 1 mean "as strong as my
// strongest taste" rather than a number on a play-count scale.
//
// At 0 the pool ranks on the profile alone and the taste vector is never
// fetched: a pool built around a seed list with no watch history involved.
// 1.0 by default. Refused below 0: a negative weight would drive the
// numerator, and so the score, negative.
fn taste_weight(ctx) {
    let w = tunable(ctx, "taste_weight", 1.0).to_float();
    if w < 0.0 { throw "config.taste_weight must be 0 or more, got " + w; }
    w
}

// The namespace ctx.profile names keyword entries under. Candidate keywords
// read from the datastore (enrichment_namespace() below) are filed under the
// same name, so a profile keyword and a candidate keyword meet in one bucket.
fn keyword_bucket() { "keywords" }

// The catalog tag an item or set entry spreads its weight over, beside its
// members' keywords.
fn genre_bucket() { "genres" }

// Which half of the library this pool may rank, by whether the account has
// ever played it (#411). Three values:
//
//   "any"      every candidate — the default, and what every pool did before
//   "only"     only units the account has played: comfort viewing
//   "exclude"  only units it has not: discovery
//
// This partitions CANDIDATES; it does not score them. #254's rule — Plex
// watch state never lowers a score — is about ranking, and is untouched: a
// title inside the half a pool may draw from is ranked exactly as before.
//
// **The partition is what makes two movie pools legal in one block.** Pools
// are blind to each other (schema.md § Pool `constraints`), so two pools that
// can resolve the same entry_id may both air it in one day and neither one's
// no-repeat rule will notice. "only" and "exclude" cannot overlap, so a
// comfort pool and a discovery pool are disjoint by construction.
//
// The source is the datastore's lifetime play set, not `ctx.history` — that
// is the newest 1000 Tautulli rows, so a film watched three years ago would
// read as unseen and land in the discovery half.
fn seen_mode(ctx) { tunable(ctx, "seen", "any") }

// How much a candidate is discounted for sitting close to what the WHOLE
// HOUSE watches (#411) — the "unusual choice" knob. The score is divided by
// `1 + unusual_weight * house cosine`, so a positive value pushes toward
// titles nobody here has a habit of watching.
//
// The terms together read as a sentence: rank by what this account likes,
// tilt toward the profile's favor and away from its disfavor, and discount the
// house's well-worn middle. This one is scale-free — it is a ratio against
// 1.0, not against the house vector's magnitude — so 1.0 means "halve the
// score of a title whose house cosine is 1.0" whatever the library.
// See the score assembly in pick() for why it divides rather than subtracts.
//
// Meaningful only on a `taste_scope: single_user` channel. On a pooled
// channel the taste vector IS the house vector, so this subtracts a scaled
// copy of the first term and only flattens the ranking.
//
// Zero by default.
fn unusual_weight(ctx) { tunable(ctx, "unusual_weight", 0.0).to_float() }

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

// The datastore row a profile member's keywords live on: the title itself, or
// for an episode, its show — an episode carries no keywords of its own.
fn member_key(item) {
    if item.type == "episode" && item.show_id != () { item.show_id } else { item.entry_id }
}

// A catalog tag field as a list of lowercased values. Most tag fields are
// arrays; `studio` and `content_rating` are single strings, or unit when
// absent. Lowercased to meet the station's lowercased profile values.
fn tag_values(field) {
    if field == () { return []; }
    if type_of(field) != "array" { return [field.to_lower()]; }
    let out = [];
    for v in field { out.push(v.to_lower()); }
    out
}

// Profile map keys (`bucket \x01 value`) as `bucket: value`, for the audit.
fn display_keys(keys) {
    let out = [];
    for k in keys {
        let parts = k.split("\x01");
        out.push(parts[0] + ": " + parts[1]);
    }
    out
}

fn hooks() { ["pool_provider"] }

fn capabilities() { ["catalog_read", "watch_history", #{ datastore: datastore_name() }] }

// Both sets this script can rank. A pool picks one with `unit:` in its
// `config:` and SHOULD narrow `sources:` to match — a pool that overrides
// neither resolves both sets, which on a shows pool means loading 11,543
// movies it will never look at.
//
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
    let kw = keyword_bucket();

    // Keyword values this pool drops from scoring entirely, matched whole —
    // `duringcreditsstinger` drops that tag and nothing else, never a
    // substring like `stinger`. Dropped at every place a keyword enters — the
    // taste, house and profile maps AND each candidate's own keyword list — so
    // an excluded tag counts neither toward a match nor toward a candidate's
    // length in the sqrt divisor or the document-frequency pass. The station
    // normalizes them (etv-station-sctf.2); this is a lookup of them.
    // A list under `config:` is refused rather than ignored: `config:` is
    // opaque to the station, so nothing else would notice the exclusions had
    // stopped applying.
    if ctx.config["exclude_keywords"] != () {
        throw "exclude_keywords is a pool field beside `profile:`, not a `config:` key — move it up one level";
    }
    let excluded = #{};
    for k in ctx.exclude_keywords { excluded[k] = true; }

    // THE TASTE PROFILE, netted into two maps keyed `bucket \x01 value`, where
    // a bucket is `keywords` or the item-map key a catalog tag sits under
    // (`genres`, `directors`, `studio`, …).
    //
    //   keyword / tag entry → its weight onto its one (bucket, value)
    //   item / set entry    → weight × (members carrying it / member count)
    //                         onto every keyword and genre its members carry
    //
    // Spreading by SHARE is what makes a 20-film set and a 200-film set at the
    // same weight mean the same thing. Summing before splitting is what makes
    // entries that disagree net out: `+2 heist` plus `−1 Heat`, where Heat
    // carries heist, leaves heist favored by 1.
    //
    // `touched` remembers which entries fed each key, by position in
    // ctx.profile, so a pick's audit can name the entries behind its score.
    let entries = ctx.profile;
    // A member's keywords live on the datastore row for the title — for an
    // episode, that is its show. One bulk fetch for every member of every
    // item and set entry, the #172 rule the candidate fetch below follows.
    let member_ids = [];
    for e in entries {
        if e.kind == "item" || e.kind == "set" {
            for item in e.items { member_ids.push(member_key(item)); }
        }
    }
    let member_enrichment = if member_ids.len > 0 { ds.enrichment_for_many(member_ids, ns) } else { #{} };

    let net = #{};
    let touched = #{};
    for i in 0..entries.len {
        let e = entries[i];
        // How much of this entry's weight lands on each key it reaches.
        let shares = #{};
        if e.kind == "keyword" {
            if excluded[e.value] == () { shares[kw + "\x01" + e.value] = 1.0; }
        } else if e.kind == "tag" {
            shares[e.namespace + "\x01" + e.value] = 1.0;
        } else {
            let inc = 1.0 / e.items.len.to_float();
            for item in e.items {
                // Each key once per member, however many times the store or
                // the catalog lists it, so a share never exceeds 1.0.
                let own = #{};
                let facts = member_enrichment[member_key(item)];
                if facts != () {
                    for f in facts {
                        if f.namespace == ns && excluded[f.value] == () { own[kw + "\x01" + f.value] = true; }
                    }
                }
                for g in tag_values(item[genre_bucket()]) { own[genre_bucket() + "\x01" + g] = true; }
                for k in own.keys() {
                    let c = shares[k];
                    shares[k] = if c == () { inc } else { c + inc };
                }
            }
        }
        for k in shares.keys() {
            let add = e.weight * shares[k];
            let c = net[k];
            net[k] = if c == () { add } else { c + add };
            if touched[k] == () { touched[k] = []; }
            touched[k].push(i);
        }
    }
    let favor = #{};
    let disfavor = #{};
    // The catalog tag buckets the profile names — each one a list of values
    // the unit pass below has to collect per candidate. Keywords are not in
    // it: every candidate's keywords are fetched regardless, for taste.
    let tag_buckets = #{};
    for k in net.keys() {
        let w = net[k];
        if w > 0.0 { favor[k] = w; } else if w < 0.0 { disfavor[k] = -w; } else { continue; }
        let b = k.split("\x01")[0];
        if b != kw { tag_buckets[b] = true; }
    }
    let tag_bucket_list = tag_buckets.keys();

    // WHAT ONE ROW OF THE RANKING IS, and which catalog entries air when that
    // row is picked.
    //
    //   unit = "movie"   a unit is one film; airing it airs that one entry.
    //   unit = "show"    a unit is a SERIES, scored on the show's own
    //                    keywords, and airing it makes every episode of that
    //                    show available in season/episode order.
    //   unit = "season"  ranks series exactly as "show" does, but lays the
    //                    output out season-major: every picked show's season
    //                    1 in ranked order, then every show's season 2, and
    //                    so on. For a pattern whose visit is a whole season
    //                    (`take: all`, `group_by: season`) — where "show"
    //                    ordering would air every season of the top show
    //                    before reaching the second show at all, turning a
    //                    recommendation channel into a marathon channel.
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

    // The lifetime play set, as a lookup, when this pool takes only one half
    // of it (#411). Fetched before the unit pass below so the filter is one
    // map probe per candidate rather than a query per candidate.
    //
    // Scoped to this channel's account when it has one, and to the house when
    // it does not — a pooled channel has no single "have I seen this", and
    // the house's set is the closest true answer rather than a guess.
    let mode = seen_mode(ctx);
    let watched = #{};
    if mode != "any" {
        let ids = if ctx.account_id != () {
            ds.watched_units_for(ctx.account_id)
        } else {
            ds.watched_units()
        };
        for id in ids { watched[id] = true; }
    }

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
    // Each unit's values in the profile's catalog tag buckets, as
    // `unit -> bucket -> value -> true`. Only the buckets the profile names are
    // collected, so a pool whose profile weights no tag builds nothing here.
    // A show's values are the union over its episodes: every episode carries
    // its show's genres (measured on the catalog: all 1,619 shows with tagged
    // episodes have one genre set across every episode), while cast and
    // directors genuinely vary by episode.
    let cand_tags = #{};

    if unit_kind != "movie" {
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
            // The partition keys on the SERIES, matching the unit being
            // ranked: "have I watched Columbo", not "have I watched this
            // episode of it".
            if mode != "any" {
                let was_seen = watched[sid] != ();
                if mode == "only" && !was_seen { continue; }
                if mode == "exclude" && was_seen { continue; }
            }
            if members[sid] == () {
                units.push(sid);
                members[sid] = [];
                if tag_bucket_list.len > 0 {
                    cand_tags[sid] = #{};
                    for b in tag_bucket_list { cand_tags[sid][b] = #{}; }
                }
            }
            for b in tag_bucket_list {
                for v in tag_values(ep[b]) { cand_tags[sid][b][v] = true; }
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
            if mode != "any" {
                let was_seen = watched[id] != ();
                if mode == "only" && !was_seen { continue; }
                if mode == "exclude" && was_seen { continue; }
            }
            units.push(id);
            members[id] = [[-1, -1, id]];
            if tag_bucket_list.len > 0 {
                cand_tags[id] = #{};
                for b in tag_bucket_list {
                    cand_tags[id][b] = #{};
                    for v in tag_values(item[b]) { cand_tags[id][b][v] = true; }
                }
            }
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
    //
    // Not fetched at all at taste_weight 0, where the pool ranks on its
    // profile alone. Otherwise divided by its own strongest weight, so the
    // top keyword weighs 1.0 whatever the account's play count — the scale a
    // hand-authored profile weight is written against.
    let alpha = taste_weight(ctx);
    let weights = #{};
    if alpha != 0.0 {
        let taste = if ctx.account_id != () {
            ds.taste_vector_for(ctx.account_id)
        } else {
            ds.pooled_taste_vector()
        };
        let top = 0.0;
        for attr in taste.attributes {
            if attr.namespace == ns && excluded[attr.value] == () {
                weights[attr.key + "\x01" + attr.value] = attr.weight;
                if attr.weight > top { top = attr.weight; }
            }
        }
        if top > 0.0 {
            for k in weights.keys() { weights[k] = weights[k] / top; }
        }
    }

    // The house's own profile, for the "unusual choice" term (#411). Fetched
    // only when a pool asks for it, and skipped outright on a pooled channel,
    // where it would be the same vector `weights` already holds.
    let gamma = unusual_weight(ctx);
    let house_weights = #{};
    if gamma != 0.0 && ctx.account_id != () {
        let house = ds.pooled_taste_vector();
        for attr in house.attributes {
            if attr.namespace == ns && excluded[attr.value] == () {
                house_weights[attr.key + "\x01" + attr.value] = attr.weight;
            }
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
            // A candidate whose every keyword is excluded is not a document:
            // it has nothing left for this scorer to read.
            let kept = 0;
            for f in facts {
                if f.namespace == ns && excluded[f.value] == () {
                    kept += 1;
                    let k = f.key + "\x01" + f.value;
                    let c = df[k];
                    df[k] = if c == () { 1 } else { c + 1 };
                }
            }
            if kept > 0 { doc_count += 1; }
        }
    }
    // The same per catalog tag bucket the profile names: idf is computed per
    // namespace, so a genre shared by half the pool and a keyword shared by
    // half the pool scale alike. Counted only for values the profile weights —
    // nothing reads the df of any other.
    let tag_df = #{};
    let tag_docs = #{};
    for b in tag_bucket_list { tag_docs[b] = 0; }
    for unit in units {
        if tag_bucket_list.len == 0 { break; }
        for b in tag_bucket_list {
            let vals = cand_tags[unit][b].keys();
            if vals.len > 0 { tag_docs[b] += 1; }
            for v in vals {
                let k = b + "\x01" + v;
                if favor[k] != () || disfavor[k] != () {
                    let c = tag_df[k];
                    tag_df[k] = if c == () { 1 } else { c + 1 };
                }
            }
        }
    }

    let scored = [];
    for id in units {
        let facts = enrichment[id];
        let taste_score = 0.0;
        let favor_score = 0.0;
        let disfavor_score = 0.0;
        let house_score = 0.0;
        let eligible = false;
        // The specific keywords this candidate shares with the pooled
        // profile — not needed for ranking, only so a ranking can be
        // explained: see the `metadata` attached below.
        let on_profile = [];
        // Each on_profile keyword's contribution to the score, index for
        // index, so the guide line can name the keywords that weighed most
        // rather than whichever the catalog happened to list first.
        let on_profile_w = [];
        // The same for the taste profile: the `bucket \x01 value` keys this
        // candidate matched in the favor and disfavor maps. The favor
        // contributions ride along for the guide line; the audit turns both
        // key lists into the entries behind them.
        let on_favor = [];
        let on_favor_w = [];
        let on_disfavor = [];
        let on_disfavor_w = [];
        if facts != () {
            let n = 0;
            let sum = 0.0;
            let fav_sum = 0.0;
            let dis_sum = 0.0;
            let house_sum = 0.0;
            for f in facts {
                if f.namespace == ns && excluded[f.value] == () {
                    n += 1;
                    let k = f.key + "\x01" + f.value;
                    let pk = kw + "\x01" + f.value;
                    let w = weights[k];
                    let fw = favor[pk];
                    let dw = disfavor[pk];
                    let hw = house_weights[k];
                    // The idf is computed once for the keyword and shared by
                    // every map, and only when at least one of them has
                    // something to say about it — a keyword none of them
                    // carries must not pay for an ln() call on every
                    // candidate (#172).
                    if w != () || fw != () || dw != () || hw != () {
                        // idf: a keyword every candidate carries scales by
                        // exactly 1.0 (doc_count / df == 1, ln == 0); a
                        // keyword only a few candidates carry scales up —
                        // ln(doc_count / df) grows as df shrinks. df[k] is
                        // always >= 1 here, since this very candidate
                        // contributed to it above, so no divide-by-zero.
                        let idf = 1.0 + (doc_count.to_float() / df[k].to_float()).ln();
                        if w != () {
                            sum += w * idf;
                            on_profile.push(f.value);
                            on_profile_w.push(alpha * w * idf);
                        }
                        if fw != () {
                            fav_sum += fw * idf;
                            on_favor.push(pk);
                            on_favor_w.push(fw * idf);
                        }
                        if dw != () {
                            dis_sum += dw * idf;
                            on_disfavor.push(pk);
                            on_disfavor_w.push(dw * idf);
                        }
                        // No keyword list for the house term: it is a
                        // penalty, and naming the keywords a title was
                        // penalised for would be the longest list in the
                        // audit and the least useful.
                        if hw != () { house_sum += hw * idf; }
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
            //
            // The profile's keyword cosines use the same divisor, so the
            // keyword half of favor and disfavor sits on the taste scale.
            // Eligible for exploration only with a keyword left after
            // exclude_keywords, for the same reason as the df pass above.
            eligible = n > 0;
            if n > 0 {
                let divisor = n.to_float().sqrt();
                taste_score = sum / divisor;
                favor_score = fav_sum / divisor;
                disfavor_score = dis_sum / divisor;
                house_score = house_sum / divisor;
            }
        }
        // The profile's catalog tag buckets, one cosine each, in the same
        // shape: idf-scaled weights over the candidate's own values in that
        // bucket, divided by the square root of how many values it has there.
        // Summed across buckets, so a genre match and a keyword match add.
        for b in tag_bucket_list {
            let vals = cand_tags[id][b].keys();
            if vals.len == 0 { continue; }
            let fav_sum = 0.0;
            let dis_sum = 0.0;
            for v in vals {
                let pk = b + "\x01" + v;
                let fw = favor[pk];
                let dw = disfavor[pk];
                if fw == () && dw == () { continue; }
                let idf = 1.0 + (tag_docs[b].to_float() / tag_df[pk].to_float()).ln();
                if fw != () {
                    fav_sum += fw * idf;
                    on_favor.push(pk);
                    on_favor_w.push(fw * idf);
                }
                if dw != () {
                    dis_sum += dw * idf;
                    on_disfavor.push(pk);
                    on_disfavor_w.push(dw * idf);
                }
            }
            let divisor = vals.len.to_float().sqrt();
            favor_score += fav_sum / divisor;
            disfavor_score += dis_sum / divisor;
        }
        // The cosine before recency, kept alongside the damped one so the
        // audit can show both. A title that fell twenty places because it
        // aired last night only reads as a decision if both halves are
        // visible; one number alone reads as the scorer changing its mind.
        //
        // Four terms: what this account likes, plus what the profile favors,
        // over what the profile disfavors and how close it sits to the
        // house's well-worn middle (#411). With no profile and no
        // `unusual_weight` this is exactly the taste cosine — the divisor is
        // 1.0.
        //
        // **Disfavor and the house term divide where favor adds**, and the
        // asymmetry is deliberate. Subtracting drives scores negative,
        // which breaks two things at once: a negative score sorts below a
        // title carrying no keywords at all — ranking "too mainstream" under
        // "we know nothing about this" — and the recency factor below is a
        // multiply by a number in (0, 1], so damping a negative score raises
        // it, turning "this aired last night" into a promotion. Clamping the
        // subtraction at zero fixes neither: it collapses every penalised
        // title into one indistinguishable bucket, which is exactly where the
        // term is meant to be discriminating hardest.
        //
        // `house_score` and `disfavor_score` are sums of non-negative weights,
        // so the divisor is always >= 1.0: never zero, never negative, and the
        // result keeps the sign and the ordering that the rest of this script
        // assumes. A disfavor-only profile can push a score toward zero but
        // never past it. `gamma` is scale-free for the same reason — it is a
        // ratio against 1.0 rather than against the house vector's magnitude.
        let base_score = (alpha * taste_score + favor_score) / (1.0 + gamma * house_score + disfavor_score);
        let aired_rank = unit_recency[id];
        let factor = recency_factor(aired_rank, half_life);
        let score = base_score * factor;
        scored.push(#{
            id: id,
            score: score,
            base_score: base_score,
            taste_score: taste_score,
            favor_score: favor_score,
            disfavor_score: disfavor_score,
            house_score: house_score,
            // Unit for a title this channel's remembered tail does not hold
            // — which is most of them, and is what `explorable` below tests.
            aired_rank: aired_rank,
            recency_factor: factor,
            eligible: eligible,
            on_profile: on_profile,
            on_profile_w: on_profile_w,
            on_favor: on_favor,
            on_favor_w: on_favor_w,
            on_disfavor: on_disfavor,
            on_disfavor_w: on_disfavor_w,
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
    let want = if unit_kind != "movie" {
        show_count(ctx)
    } else {
        ctx.target_count + 10
    };

    let out = [];
    let ws_rows = #{};
    // Units in the order they were selected, and the audit record each one
    // carries. Selection and layout are two passes, not one: `unit = season`
    // has to see every picked show before it can interleave their seasons,
    // and a single pass that emitted as it picked could not.
    let selected = [];
    let stages = #{};
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
        // bounded list captured above. One record per unit, attached in the
        // layout pass to every entry that unit emits.
        // The profile entries behind this pick's favor and disfavor, each once,
        // in ctx.profile order: what a channel author tunes against. Built
        // here, for picks only, never for every candidate.
        let matched = row.on_favor + row.on_disfavor;
        let entry_hit = #{};
        for k in matched {
            for i in touched[k] { entry_hit["" + i] = i; }
        }
        let profile_entries = [];
        for i in 0..entries.len {
            if entry_hit["" + i] == () { continue; }
            let e = entries[i];
            profile_entries.push(#{ kind: e.kind, reference: e.reference, origin: e.origin, weight: e.weight });
        }
        stages[row.id] = #{
            score: row.score,
            base_score: row.base_score,
            // The terms `base_score` is built from, and the weight on taste.
            // Without them, a pick that only aired because the profile liked
            // it is indistinguishable from one the account's own history
            // chose — which is the whole question a steered pool raises.
            taste_score: row.taste_score,
            taste_weight: alpha,
            favor_score: row.favor_score,
            disfavor_score: row.disfavor_score,
            // The five pairs that weighed most on each side — a heavily
            // tagged title can match dozens, and the audit record has a byte
            // budget (ADR 0011).
            favor_matches: top_keywords(display_keys(row.on_favor), row.on_favor_w, 5),
            disfavor_matches: top_keywords(display_keys(row.on_disfavor), row.on_disfavor_w, 5),
            profile_entries: profile_entries,
            // The house term and the half of the library this pool drew from
            // (#411), so an audit line says which of the two movie pools a
            // pick came out of without the reader knowing the config.
            house_score: row.house_score,
            unusual_weight: gamma,
            seen: mode,
            recency_factor: row.recency_factor,
            aired_rank: row.aired_rank,
            rank: row.rank,
            // Which series was ranked, on a shows pool — unit on a movies
            // pool, where the entry and the unit are the same thing. Without
            // it an audit line reads as though the episode itself scored 7.7,
            // when what scored is the show it belongs to.
            unit: if unit_kind == "movie" { () } else { row.id },
            draw: if source == "explore" { "exploration" } else { "ranked" },
            near_misses: near_misses,
            on_profile: row.on_profile,
            on_profile_w: row.on_profile_w,
            on_favor: row.on_favor,
            on_favor_w: row.on_favor_w,
            source: source,
        };
        selected.push(row.id);
    }

    // LAYOUT. The station will not re-sort what a plugin returns, and it
    // groups the returned list into series by first appearance (pattern.rs),
    // so the order below is broadcast order AND rotation order at once.
    //
    // A show hands back ALL of its episodes rather than a slice because
    // `advance: resume` locates the last-aired episode in this list and
    // continues past it: a list holding only the first few would never
    // contain the episode the ledger resumes from, and the show would restart
    // from episode one forever.
    let blocks = [];
    for i in 0..selected.len {
        let uid = selected[i];
        let rows = members[uid];
        if unit_kind == "movie" {
            blocks.push(#{ season: 0, order: i, uid: uid, ids: [uid] });
            continue;
        }
        // Season, then episode, then id — the order a series is broadcast in.
        // Sorted here, for the handful of shows actually picked, never for
        // all 1,633 of them.
        rows.sort(|a, b| {
            if a[0] < b[0] { -1 } else if a[0] > b[0] { 1 }
            else if a[1] < b[1] { -1 } else if a[1] > b[1] { 1 }
            else if a[2] < b[2] { -1 } else if a[2] > b[2] { 1 }
            else { 0 }
        });
        // Cut the show into one block per season. `unit = "show"` will
        // re-join them untouched; `unit = "season"` interleaves them.
        let cur = ();
        let block = [];
        for m in rows {
            if cur == () { cur = m[0]; }
            if m[0] != cur {
                blocks.push(#{ season: cur, order: i, uid: uid, ids: block });
                block = [];
                cur = m[0];
            }
            block.push(m[2]);
        }
        if block.len > 0 {
            blocks.push(#{ season: cur, order: i, uid: uid, ids: block });
        }
    }

    if unit_kind == "season" {
        // Season-major: every picked show's season 1 in ranked order, then
        // every show's season 2, and so on. Under `group_by: season` the
        // station makes one series per season, so this is the rotation
        // visiting a different SHOW each time rather than working through one
        // show's seasons back to back.
        blocks.sort(|a, b| {
            if a.season < b.season { -1 } else if a.season > b.season { 1 }
            else if a.order < b.order { -1 } else if a.order > b.order { 1 }
            else { 0 }
        });
    }

    for b in blocks {
        let stage = stages[b.uid];
        for id in b.ids {
            // The score, the specific on-profile keywords behind it, and
            // whether this slot came from the ranking or the seeded surprise
            // draw travel along as `metadata` (#166) so a pick can be
            // explained rather than just trusted — never read back by
            // etv-station itself (ADR 0002: the score stays internal to the
            // plugin), only carried through untouched to the playout JSON.
            out.push(#{
                entry_id: id,
                metadata: #{
                    score: stage.score,
                    on_profile_keywords: stage.on_profile,
                    // Two floats, not the entry list: the profile entries
                    // behind them are in the audit detail, where bytes are
                    // budgeted for explanation. These are here so a reader of
                    // the playout JSON alone can still see that the profile
                    // moved a pick and which way.
                    favor_score: stage.favor_score,
                    disfavor_score: stage.disfavor_score,
                    source: stage.source,
                },
            });
            // Keyed by the EMITTED entry_id, since audit() is handed the
            // picks array and looks each one up by that — an episode, not the
            // show it was ranked as.
            ws_rows[id] = stage;
        }
    }

    #{ picks: out, workspace: #{ rows: ws_rows, candidate_count: scored.len } }
}

// The `n` words whose paired weights are largest, heaviest first, each once —
// the keywords that added most to a pick's score, or the profile pairs that
// weighed most on one side of it.
fn top_keywords(words, weights, n) {
    let pairs = [];
    for i in 0..words.len {
        pairs.push(#{ k: words[i], c: weights[i] });
    }
    pairs.sort(|a, b| {
        if a.c > b.c { -1 } else if a.c < b.c { 1 }
        else if a.k < b.k { -1 } else if a.k > b.k { 1 }
        else { 0 }
    });
    let out = [];
    for p in pairs {
        if out.len >= n { break; }
        if !out.contains(p.k) { out.push(p.k); }
    }
    out
}

// The one sentence a viewer reads in the guide (etv-station-puhx): the
// audit's verdict in plain words, with the rank and the keywords behind it.
// The station prints it as-is and refuses anything over 200 characters, so
// the keyword list is capped at three and each keyword at 30 characters.
fn guide_line(r, candidate_count) {
    if r.draw == "exploration" {
        return "Picked for you: a deliberate wildcard outside your usual taste";
    }
    let noun = if r.unit == () { "films" } else { "series" };
    let line = "Picked for you: #" + r.rank + " of " + candidate_count + " " + noun + " by taste match";
    // Taste matches and favored profile values compete on what each added to
    // the score's numerator, so a pool steered by its profile names what the
    // profile liked, not only what watch history did.
    let words = r.on_profile;
    let contribs = r.on_profile_w;
    for i in 0..r.on_favor.len {
        words.push(r.on_favor[i].split("\x01")[1]);
        contribs.push(r.on_favor_w[i]);
    }
    let kws = top_keywords(words, contribs, 3);
    if kws.len > 0 {
        let short = [];
        for k in kws {
            short.push(if k.len > 30 { k.sub_string(0, 30) } else { k });
        }
        let joined = short[0];
        for i in 1..short.len {
            joined += if i == short.len - 1 { " and " } else { ", " };
            joined += short[i];
        }
        line += " (" + joined + ")";
    }
    if r.seen == "only" {
        line += ", one you've watched before";
    } else if r.seen == "exclude" {
        line += ", one you haven't seen";
    }
    line
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
                // The terms `base_score` is built from:
                // (taste_weight · taste_score + favor_score)
                //   / (1 + unusual_weight · house_score + disfavor_score).
                // favor_score and disfavor_score are 0.0 on a pool with no
                // profile, where `base_score` is the taste cosine alone.
                // `profile_entries` names each entry that fed either term for
                // this pick, with the file it came from.
                taste_score: r.taste_score,
                taste_weight: r.taste_weight,
                favor_score: r.favor_score,
                disfavor_score: r.disfavor_score,
                favor_matches: r.favor_matches,
                disfavor_matches: r.disfavor_matches,
                profile_entries: r.profile_entries,
                house_score: r.house_score,
                unusual_weight: r.unusual_weight,
                seen: r.seen,
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
        } else if r != () && r.taste_weight == 0.0 {
            "ranked by the taste profile alone"
        } else {
            "ranked by keyword cosine against pooled taste"
        };
        // The tilts are clauses on whichever verdict above applies, not
        // verdicts of their own: a pick can be damped AND tilted, and a reader
        // needs both facts. Each is added only when this candidate actually
        // matched that side of the profile — a steered pool still airs titles
        // the profile had no opinion about, and saying otherwise would be a
        // lie about every one of them.
        if r != () && r.favor_score > 0.0 {
            verdict = verdict + ", tilted toward the taste profile's favor";
        }
        if r != () && r.disfavor_score > 0.0 {
            verdict = verdict + ", held back by the taste profile's disfavor";
        }
        // Which half of the library the pool drew from, named on every pick
        // that came from a partitioned pool. Not conditional on the pick
        // itself the way the tilt clause is: the partition applied to this
        // pick by definition, since it is why the candidate was there at all.
        if r != () && r.seen == "only" {
            verdict = verdict + ", from titles this account has already played";
        } else if r != () && r.seen == "exclude" {
            verdict = verdict + ", from titles this account has never played";
        }
        if r != () && r.unusual_weight > 0.0 && r.house_score > 0.0 {
            verdict = verdict + ", pushed away from what the house watches";
        }
        let record = #{
            stage: "pool",
            by: "plugin:taste-cosine",
            verdict: verdict,
            detail: detail,
        };
        if r != () {
            record.guide = guide_line(r, workspace.candidate_count);
        }
        out[p.entry_id] = record;
    }
    out
}
