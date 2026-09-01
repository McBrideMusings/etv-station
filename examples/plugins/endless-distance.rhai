// endless-distance — the Endless channel's pool provider (#176).
//
// Picks each item by keyword-cosine distance from the item picked *before*
// it, inside a declared min/max band, and slides that band forward a little
// on every step so the channel wanders through the library instead of
// settling into the two titles closest to each other and orbiting them
// forever. No ADR is filed for it; the design is explained inline below and
// in the issue.
//
// Movies only (#176's scoping note): keywords sit on movies and shows, never
// on episodes. That is now a choice rather than a wall — #274 made the
// catalog's `show_id` GUID-derived, and taste-cosine.rhai uses it to score an
// episode's show. This script has no equivalent yet because its walk is a
// chain of single titles: the unit it steps between would have to become a
// series, and what "distance from the last thing played" means across a
// season is an open question, not a port of the same code.
//
// Five functions make up the contract (see docs/schema.md "Pool `plugin`"):
//   hooks()               -> ["pool_provider"]
//   capabilities()        -> ["catalog_read", #{ datastore: "taste_db" }]
//   sources()             -> #{ movies: cel_expression }
//   pick(ctx)             -> #{ picks: [#{ entry_id, metadata }, …], workspace }
//   audit(ctx, picks, ws) -> #{ entry_id: stage_record }
//
// ctx carries (docs/schema.md § capabilities()):
//   ctx.sets.movies   every movie the sources() query matched
//   ctx.datastore("taste_db")
//                     the plex-db-ex reader handle this pool was granted
//   ctx.recent        entry_ids this channel aired most recently, oldest first
//   ctx.target_count  how many items this generation needs
//   ctx.config        this pool's `config:` block
//   ctx.seed          this pool's reproducible seed (#255, ADR 0005)

// ---- tunables --------------------------------------------------------------
//
// `ctx.config` is always a map — an absent `config:` block arrives empty — so
// a missing key is unit and every lookup below can be unconditional. Written
// as functions rather than `const` for the same reason taste-engine.rhai
// does: a top-level constant is only in scope once the script's body has
// run, and the station calls these functions directly.
fn tunable(ctx, key, fallback) {
    let v = ctx.config[key];
    if v == () { fallback } else { v }
}

// The distance band a candidate's jump from the previous pick must land
// inside. Below `min_distance` the two titles read as near-duplicates;
// above `max_distance` the cut reads as a non-sequitur. Tuned against the
// real store (10,089 keyworded movies): most keyword-sharing pairs land
// under 0.85, so a 0.30–0.70 band is wide enough to almost always find a
// candidate while still ruling out the closest handful and the totally
// unrelated tail.
fn min_distance(ctx) { tunable(ctx, "min_distance", 0.30) }
fn max_distance(ctx) { tunable(ctx, "max_distance", 0.70) }

// How much the band's lower and upper edges both climb per step, before
// wrapping back down. This is what stops the band from being satisfied by
// the same neighbourhood forever: ten steps in, a pair that was comfortably
// inside the band at step 0 may no longer be far enough away. Once the
// climb would push `max_distance` past 1.0 (the farthest two titles can
// ever be under this metric) it wraps back to the base band rather than
// clamping, so the walk keeps cycling through progressively more adventurous
// jumps instead of getting stuck pinned to the ceiling.
fn drift(ctx) { tunable(ctx, "drift", 0.02) }

// How many of a step's runners-up get named in that step's pick's audit
// trail as near misses (#393). Bounded by the script, not the station —
// overridable per pool via `config:` like every other tunable here. A full
// candidate dump stays taste-debug's job.
fn near_miss_limit(ctx) { tunable(ctx, "near_miss_limit", 3).to_int() }

// Which datastore grant this script reaches for — see
// docs/schema.md "Pool `capabilities` / `datastores`". A fixed name, not a
// tunable: the channel author names the *location* under `datastores:`, not
// which handle the script asks for.
fn datastore_name() { "taste_db" }

fn hooks() { ["pool_provider"] }

fn capabilities() { ["catalog_read", #{ datastore: datastore_name() }] }

fn sources() { #{ movies: `item.type == "movie"` } }

fn pick(ctx) {
    let ds = ctx.datastore(datastore_name());

    // Every movie this pool offers, in the order ctx.sets handed them over —
    // that order is what makes the seeded picks below reproducible run to
    // run against the same catalog snapshot.
    let ids = [];
    for item in ctx.sets.movies { ids.push(item.entry_id); }

    // One bulk fetch over the whole candidate set (plex-db-ex#40, #172's
    // measured rule) — never one call per candidate, and never repeated
    // inside the walk below. The store connection behind `ds` was opened
    // once for this whole generation, so this is a single, consistent read
    // of it — a snapshot, not a live poll.
    let grouped = ds.enrichment_for_many(ids, "tmdb_keywords");

    // Build every id-keyed structure the walk needs *before* the walk, per
    // #172: `keywords` (this title's tokens, one `key|value` string per
    // enrichment fact), `index_of` (this id's position, for the seeded
    // tie-break), and `token_index` (which candidates carry a given token —
    // the walk's whole reason for staying fast: at each step it only ever
    // has to look at candidates sharing at least one keyword with the
    // current item, never the full library).
    //
    // A title absent from `grouped` carries zero enrichment rows under this
    // namespace — 2,373 of this library's movies, normally — and is dropped
    // here rather than kept with an empty vector, so it can never be chosen
    // and never scores a manufactured distance of 1.0 against everything
    // else; it simply never enters `candidates`.
    let keywords = #{};
    let candidates = [];
    let index_of = #{};
    let token_index = #{};
    for id in ids {
        let facts = grouped[id];
        if facts == () || facts.len == 0 { continue; }
        let toks = [];
        for f in facts { toks.push(f.key + "|" + f.value); }
        keywords[id] = toks;
        candidates.push(id);
        index_of[id] = candidates.len - 1;
        for t in toks {
            if token_index[t] == () { token_index[t] = []; }
            token_index[t].push(id);
        }
    }

    if candidates.len == 0 {
        throw "endless-distance: no candidate in this pool's sources carries tmdb_keywords";
    }

    // The walk's starting point: the most recently aired item this pool
    // could plausibly continue from — the last entry in ctx.recent (oldest
    // first) that is itself one of this pool's keyworded candidates. A
    // channel with mixed pools, or one that has never aired, will often
    // find nothing there; the seeded fallback below picks a reproducible
    // start instead of failing the generation over a cold start.
    let anchor = ();
    let ri = ctx.recent.len - 1;
    while ri >= 0 {
        let rid = ctx.recent[ri];
        if keywords[rid] != () { anchor = rid; break; }
        ri -= 1;
    }
    if anchor == () {
        // ctx.seed is a non-negative i64 (ADR 0005), so a plain `%` is an
        // ordinary non-negative modulus.
        anchor = candidates[ctx.seed % candidates.len];
    }

    let base_min = min_distance(ctx);
    let base_max = max_distance(ctx);
    let d = drift(ctx);
    let headroom = 1.0 - base_max;

    let used = #{};
    used[anchor] = true;
    let ws_rows = #{};
    let nm_limit = near_miss_limit(ctx);

    // Ask for a little more than the generation needs, so a pattern that draws
    // unevenly across pools does not run short.
    let want = ctx.target_count + 10;
    let out = [];
    let cur = anchor;
    let cur_kw = keywords[cur];
    let cur_n = cur_kw.len;
    let step = 0;

    while out.len < want {
        // The band for this step: both edges climb by `drift` per step and
        // wrap at the headroom between `max_distance` and 1.0, so the
        // required jump keeps growing until it resets rather than pinning
        // at the ceiling forever.
        let offset = if headroom <= 0.0 { 0.0 } else { (d * step.to_float()) % headroom };
        let band_min = base_min + offset;
        let band_max = base_max + offset;

        // Only candidates sharing at least one keyword with `cur` can score
        // under 1.0 at all (disjoint keyword sets always give cosine 0), so
        // this is the complete search space for anything inside the band —
        // not a sampled subset of it.
        let seen = #{};
        let pool_ids = [];
        for t in cur_kw {
            let lst = token_index[t];
            if lst == () { continue; }
            for cid in lst {
                if cid == cur || used[cid] == true || seen[cid] == true { continue; }
                seen[cid] = true;
                pool_ids.push(cid);
            }
        }

        // A quick membership map over `cur`'s own tokens, built once per
        // step rather than re-scanned per candidate.
        let cur_map = #{};
        for t in cur_kw { cur_map[t] = true; }

        let best_id = ();
        let best_key = -1;
        let best_dist = 0.0;
        let fb_id = ();
        let fb_delta = ();
        let fb_dist = 0.0;

        // This step's runners-up (#393), kept ordered by closeness to the
        // band (smallest delta first) and capped at nm_limit + 1 while the
        // scan runs — one spare slot, because the eventual winner is
        // scanned right alongside everything else here and only known once
        // the loop below finishes; it is dropped once `chosen` is set.
        let near_misses = [];

        for cid in pool_ids {
            let kw = keywords[cid];
            let n = kw.len;
            let inter = 0;
            for t in kw { if cur_map[t] == true { inter += 1; } }
            let denom = (cur_n.to_float() * n.to_float()).sqrt();
            let cos = if denom <= 0.0 { 0.0 } else { inter.to_float() / denom };
            let dist = 1.0 - cos;

            let in_band = dist >= band_min && dist <= band_max;
            if in_band {
                // A deterministic, reproducible tie-break among every
                // in-band match: mixes this generation's seed, the step,
                // and the candidate's stable position in `candidates` —
                // never Rhai's own clock (the determinism check, #168,
                // exists to catch exactly that).
                let key = (index_of[cid] * 2654435761 + ctx.seed + step * 40503) % 2147483647;
                if key > best_key { best_key = key; best_id = cid; best_dist = dist; }
            } else {
                let delta = if dist > band_max { dist - band_max } else { band_min - dist };
                if fb_id == () || delta < fb_delta {
                    fb_id = cid; fb_delta = delta; fb_dist = dist;
                }
            }

            // Record this candidate as a runner-up, regardless of whether
            // it ends up chosen below — the winner is dropped from the list
            // once known. In-band candidates sort first (delta 0); the
            // reason names which branch above this candidate took.
            let nm_delta = if in_band { 0.0 } else if dist > band_max { dist - band_max } else { band_min - dist };
            let nm_reason = if in_band {
                "inside the band but lost the seeded tie-break"
            } else {
                "outside this step's distance band"
            };
            let ins = near_misses.len;
            while ins > 0 && near_misses[ins - 1].delta > nm_delta {
                ins -= 1;
            }
            near_misses.insert(ins, #{ id: cid, distance: dist, reason: nm_reason, delta: nm_delta });
            if near_misses.len > nm_limit + 1 {
                near_misses.pop();
            }
        }

        let chosen = ();
        let chosen_dist = 0.0;
        let chosen_draw = "";
        if best_id != () {
            chosen = best_id;
            chosen_dist = best_dist;
            chosen_draw = "in_band";
        } else if fb_id != () {
            // Nothing in this step's band, but something shares a keyword
            // with `cur` — take whichever candidate sits closest to the
            // band rather than stall the walk on a strict miss.
            chosen = fb_id;
            chosen_dist = fb_dist;
            chosen_draw = "nearest_band_miss";
        } else {
            // `cur` shares no keyword with anything left at all: the local
            // neighbourhood is exhausted. Jump to a fresh, seeded candidate
            // elsewhere in the library — which, sharing nothing with `cur`,
            // is exactly distance 1.0 away. This is what keeps a small,
            // tightly-linked cluster from trapping the walk: once every
            // title in it has been used, there is nothing left to stay
            // close to, and the walk is forced outward.
            let start = (ctx.seed + step * 2654435761) % candidates.len;
            let i = 0;
            while i < candidates.len {
                let probe = candidates[(start + i) % candidates.len];
                if probe != cur && used[probe] != true {
                    chosen = probe;
                    chosen_dist = 1.0;
                    chosen_draw = "seeded_jump";
                    break;
                }
                i += 1;
            }
        }

        if chosen == () { break; } // every candidate has been used

        // Drop the winner's own id from the runner-up list — it was scanned
        // like every other candidate above, before the winner was known —
        // then trim the delta field and the spare slot (#393). The
        // seeded_jump path never populated `near_misses` at all (nothing was
        // scanned), so this stays an empty array there — a uniform key
        // shape rather than a missing one.
        let chosen_near_misses = [];
        for nm in near_misses {
            if nm.id != chosen && chosen_near_misses.len < nm_limit {
                chosen_near_misses.push(#{ id: nm.id, distance: nm.distance, reason: nm.reason });
            }
        }

        out.push(#{ entry_id: chosen, metadata: #{ distance: chosen_dist } });
        // What audit() will report for this pick (#392): the distance
        // walked, the band it was walked against, and which of the three
        // selection paths chose it — set here, next to the assignment, not
        // re-derived afterwards. `near_misses` is #393: the step's bounded
        // runner-up list captured above.
        ws_rows[chosen] = #{
            distance: chosen_dist,
            step: step,
            band_min: band_min,
            band_max: band_max,
            draw: chosen_draw,
            near_misses: chosen_near_misses,
        };
        used[chosen] = true;
        cur = chosen;
        cur_kw = keywords[cur];
        cur_n = cur_kw.len;
        step += 1;
    }

    if out.len == 0 {
        throw "endless-distance: could not find a candidate within reach of the starting point";
    }
    #{ picks: out, workspace: ws_rows }
}

// The fifth pool_provider contract function (#389, ADR 0011, ADR 0013):
// called once per generation, immediately after pick(), handed back the list
// pick() returned plus the workspace pick() built above — the distance
// walked to reach each pick, and the band and selection path behind it.
// Also reports the bounded runner-up list pick() captured for that step
// (#393) — the candidates that were scanned and did not win it.
fn audit(ctx, picks, workspace) {
    let out = #{};
    for p in picks {
        let r = workspace[p.entry_id];
        let detail = if r != () {
            #{
                distance: r.distance,
                step: r.step,
                band_min: r.band_min,
                band_max: r.band_max,
                draw: r.draw,
                near_misses: r.near_misses,
            }
        } else {
            #{}
        };
        let verdict = if r == () {
            "walked to the nearest in-band keyword neighbour"
        } else if r.draw == "in_band" {
            "walked to a candidate inside the distance band"
        } else if r.draw == "nearest_band_miss" {
            "walked to the closest candidate outside the distance band"
        } else {
            "jumped to a fresh, seeded candidate after exhausting the local neighbourhood"
        };
        out[p.entry_id] = #{
            stage: "pool",
            by: "plugin:endless-distance",
            verdict: verdict,
            detail: detail,
        };
    }
    out
}
