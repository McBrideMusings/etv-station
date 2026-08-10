// foryou-sequencer — the worked example sequencer plugin (#169).
//
// Two pools, one script, two different advance policies — the case a
// `pattern` block cannot express, because `pattern` applies one `advance`
// setting per pool but has no way to let the *arrangement itself* differ
// pool to pool. Here the script decides both.
//
//   - `spotlight` resumes: it continues the same show from where the ledger
//     last left off, one episode per generation, so a viewer following it
//     sees the next episode each time this channel refreshes.
//   - `discovery` restarts: every generation re-ranks by how recently each
//     movie was added, ignoring anything about what played before.
//
// The two are interleaved 1 spotlight episode, then 2 discovery movies,
// repeating until the window is filled — an arrangement this script owns
// completely; the station only supplies the resolved pools and the window.
//
// Four functions make up the contract:
//   hooks()     -> [hook_name, …]     which hooks this script implements
//   arrange(ctx) -> [entry_id, …]      the block's timeline, in air order
//
// hooks() is read at config load time, before the catalog exists — it names
// this script a sequencer, which is the only thing `sequencer:` on a block
// has ever meant (#159). Declaring it is what lets a channel that misuses
// this script (wants a hook it doesn't implement) fail at load instead of on
// air.
//
// ctx carries:
//   ctx.pools.<name>       the pool's resolved items — full item maps
//                          (entry_id, title, duration_ms, show, season,
//                          tags, …), already sorted/constrained by the
//                          block's own `order` / `bucket_order` /
//                          `constraints`
//   ctx.window.from        unix seconds this generation begins airing
//   ctx.window.fill_secs   seconds of airtime this generation should cover,
//                          or () when unbounded
//   ctx.resume.<pool>.next the series whose turn was next as of the last
//                          generation, or () — read-only; the station derives
//                          the value for the *next* generation from what this
//                          one actually draws
//   ctx.cursor.<series>    the last entry_id the play-history ledger recorded
//                          for that series (keyed by show_id), or absent
//   ctx.now                unix seconds at generation time
//
// The station enforces three things regardless of what this script returns:
// an id outside every pool fails the generation naming it, a short timeline
// is not padded, and an overrunning one is truncated at the item that
// crosses the window boundary. None of that is this script's job.

fn hooks() { ["sequencer"] }

// How many discovery movies follow each spotlight episode.
fn discovery_per_visit() { 2 }

fn arrange(ctx) {
    let out = [];

    // `discovery`: every generation re-ranks by recency and ignores
    // ctx.resume/ctx.cursor entirely — the "restart" half of the pair.
    // `ctx.pools.discovery` already carries the block's own `order`
    // (newest first), so the script's job here is only to draw from it.
    let discovery = [];
    for item in ctx.pools.discovery { discovery.push(item.entry_id); }

    // `spotlight`: continue the series the ledger says was playing, one
    // episode per visit — the "resume" half. `ctx.cursor` is keyed by
    // show_id, same as `HistoryDb::series_cursor`; a show with no cursor
    // yet starts at its first episode.
    let spotlight = [];
    for item in ctx.pools.spotlight { spotlight.push(item); }
    let last = ctx.cursor[spotlight[0].show_id];
    let start = 0;
    if last != () {
        for i in 0..spotlight.len {
            if spotlight[i].entry_id == last {
                start = i + 1;
                break;
            }
        }
        if start >= spotlight.len { start = 0; }
    }

    // Interleave: one spotlight episode, then `discovery_per_visit()` movies,
    // until either pool runs dry — the station's own short-timeline handling
    // covers the rest (falls through to the channel's normal short-window
    // behavior rather than this script padding anything).
    let per_visit = discovery_per_visit();
    let si = start;
    let di = 0;
    while si < spotlight.len || di < discovery.len {
        if si < spotlight.len {
            out.push(spotlight[si].entry_id);
            si += 1;
        }
        let taken = 0;
        while taken < per_visit && di < discovery.len {
            out.push(discovery[di]);
            di += 1;
            taken += 1;
        }
    }

    out
}
