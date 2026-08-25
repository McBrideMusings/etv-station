// daypart-sequencer — the reusable dayparting sequencer (#14, ADR 0004).
//
// A network-mirror channel is one block whose pools are its dayparts plus a
// default pool. This script places each daypart pool in its declared hour
// range on its declared weekdays, and fills every hour no daypart claims
// from the default pool — the shape ADR 0004 settled instead of a
// block-level `airs:` schema field. No channel-specific logic lives here:
// every network mirror points its `sequencer:` at this same file and
// expresses its own schedule entirely through each pool's existing
// `config:` block (ADR 0002's generic pool carrier — not a new schema
// field).
//
// # Pool config
//
// A daypart pool declares the hours it claims:
//
//   pools:
//     - name: latenight
//       expr: 'item.show in ["Rick and Morty", "The Boondocks"]'
//       config:
//         weekdays: [sun, mon, tue, wed, thu, fri, sat]   # default: every day
//         hour_start: 21   # local hour, 0-23
//         hour_end: 6      # < hour_start means the range wraps past midnight
//
// `hour_end` is exclusive. `hour_start <= hour_end` is a same-day range
// (`[hour_start, hour_end)`); `hour_start > hour_end` wraps past local
// midnight, and the weekday list names the day the range *starts* on — a
// `sat` `21:00`-`06:00` daypart is still "sat" at Sunday 02:00 local.
//
// **Exactly one pool in the block declares no `hour_start`/`hour_end`** — the
// default, which fills every hour no daypart claims. A block with zero or
// more than one such pool fails `arrange()`, naming what's wrong.
//
// # What this script does not do
//
// It restarts every generation: each pool's queue is walked from its own
// start (`ctx.pools.<name>`'s own order), not resumed from `ctx.resume` or
// `ctx.cursor`. A show that wants to continue across days needs `advance:
// resume` wired into a future revision of this script — out of scope for
// #14, which only asks that a pool land in its hour and the channel never go
// dark. Network-mirror schedule fidelity (real historical dayparts, promos,
// bumpers) is also out of scope (#85, #14's own "out of scope" list).
//
// The window this script fills, per generation, is `ctx.window.fill_secs`
// when the station gives one, or 24 hours at the stateless entry point
// (`ctx.window.fill_secs == ()`), which has no real chunk to size against.

fn hooks() { ["sequencer"] }

// A function, not a top-level `const` — Rhai functions run in their own
// isolated scope and cannot see a script's global variables or constants, so
// every place that needs the weekday list calls this instead.
fn all_weekdays() { ["mon", "tue", "wed", "thu", "fri", "sat", "sun"] }

fn prev_weekday(w) {
    let days = all_weekdays();
    let idx = days.index_of(w);
    days[(idx + 6) % 7]
}

// Whether local (weekday, hour) falls inside daypart `dp` — the one place
// the wrap-past-midnight rule is decided, so validation and placement can
// never disagree about what a daypart claims.
fn in_daypart(weekday, hour, dp) {
    if dp.hour_start <= dp.hour_end {
        dp.weekdays.contains(weekday) && hour >= dp.hour_start && hour < dp.hour_end
    } else if hour < dp.hour_end {
        dp.weekdays.contains(prev_weekday(weekday))
    } else {
        dp.weekdays.contains(weekday) && hour >= dp.hour_start
    }
}

// The first (weekday, hour) slot both dayparts claim, or `()` when they never
// collide — decided by `in_daypart` itself, so the wrap-past-midnight rule
// has exactly one definition.
fn overlap_slot(a, b) {
    for wd in all_weekdays() {
        for h in 0..24 {
            if in_daypart(wd, h, a) && in_daypart(wd, h, b) { return wd + " " + h; }
        }
    }
    ()
}

// Read each pool's `config:` into a daypart record, or name it the default.
// Fails naming the block's problem: no default, more than one default, or a
// config the station could not parse (`hour_start` without `hour_end`, an
// out-of-range hour).
fn read_dayparts(ctx) {
    let dayparts = [];
    let default_pool = ();
    for name in ctx.pools.keys() {
        let cfg = ctx.pool_config[name];
        if cfg.hour_start == () && cfg.hour_end == () {
            if default_pool != () {
                throw "sequencer: pools \"" + default_pool + "\" and \"" + name
                    + "\" both declare no hour range — exactly one pool must be the "
                    + "default that fills unclaimed hours";
            }
            default_pool = name;
            continue;
        }
        if cfg.hour_start == () || cfg.hour_end == () {
            throw "sequencer: pool \"" + name + "\" declares only one of "
                + "hour_start/hour_end — a daypart needs both, or neither to be the default";
        }
        if cfg.hour_start < 0 || cfg.hour_start > 23 || cfg.hour_end < 0 || cfg.hour_end > 24 {
            throw "sequencer: pool \"" + name + "\" has hour_start/hour_end outside 0-23/0-24";
        }
        let weekdays = cfg.weekdays;
        if weekdays == () { weekdays = all_weekdays(); }
        dayparts.push(#{
            name: name,
            weekdays: weekdays,
            hour_start: cfg.hour_start,
            hour_end: cfg.hour_end,
        });
    }
    if default_pool == () {
        throw "sequencer: no default pool — every pool declared an hour range; add one pool "
            + "with no config.hour_start/hour_end to fill the hours no daypart claims";
    }

    // Two dayparts claiming the same (weekday, hour) is a config error, not
    // a priority to resolve — nothing here decides which one wins.
    for i in 0..dayparts.len {
        for j in (i + 1)..dayparts.len {
            let a = dayparts[i];
            let b = dayparts[j];
            let slot = overlap_slot(a, b);
            if slot != () {
                throw "sequencer: dayparts \"" + a.name + "\" and \"" + b.name
                    + "\" both claim " + slot + " local — narrow one of their "
                    + "hour ranges or weekday lists";
            }
        }
    }

    #{ dayparts: dayparts, default_pool: default_pool }
}

fn active_pool_at(secs, dayparts, default_pool) {
    let t = local_time(secs);
    for dp in dayparts {
        if in_daypart(t.weekday, t.hour, dp) { return dp.name; }
    }
    default_pool
}

// The next instant, at or after `cursor`, whose active pool differs from
// `current`. Daypart boundaries only ever fall on a local hour mark (every
// `hour_start`/`hour_end` is a whole hour), so walking forward one local
// hour at a time is exact — the first mark where `active_pool_at` changes
// *is* the boundary, not an approximation of one.
//
// Capped at a week of hours: if nothing ever differs (a lone default pool,
// or one daypart claiming all 168 hours) there is no real boundary, and the
// cap is returned as a stand-in for "far enough away that no item's
// duration will be rejected for it".
fn next_boundary(cursor, current, dayparts, default_pool) {
    let t0 = local_time(cursor);
    let probe = cursor + (60 - t0.minute) * 60;
    for hour in 0..(24 * 7) {
        if active_pool_at(probe, dayparts, default_pool) != current { return probe; }
        probe += 3600;
    }
    probe
}

// An item the catalog has no measured duration for is charged this nominal
// length — 30 minutes — everywhere a duration is needed: both the fit-check
// below and the cursor advance in `arrange()` use the same number, so an
// unmeasured item is judged by the length it will actually be charged for
// rather than looking free to the fit-check and then costing 30 minutes of
// real cursor time anyway. A function, not a top-level `const` — Rhai
// functions cannot see a script's global constants, the same reason
// `all_weekdays()` is a function.
fn unmeasured_duration_ms() { 1_800_000 }

// The index of the next item this pool airs: whichever of its resolved
// items fits inside `remaining_secs`, scanned in the pool's own order
// starting at `ptr` so a pool that fits nothing this visit still advances
// instead of stalling on the same item forever. When nothing fits, the item
// at the pointer plays anyway — the daypart starts late rather than
// truncating it or leaving the slot dark (#14 decision 3). `()` only when
// the pool has no items at all.
//
// Returns the chosen index rather than the item, and takes `ptr` as a plain
// value rather than the pointer map itself — a Rhai script function receives
// its arguments by value, so a map parameter it wrote into (`pointers[name]
// = ...`) would only ever mutate a copy that is dropped when the call
// returns (#211). `arrange()` owns the `pointers` map and advances it itself
// after this returns, which is what makes that bug unrepresentable here
// rather than merely fixed.
fn pick_index(name, pools, ptr, remaining_secs) {
    let items = pools[name];
    let n = items.len;
    if n == 0 { return (); }
    let remaining_ms = remaining_secs * 1000;
    let chosen = -1;
    for k in 0..n {
        let idx = (ptr + k) % n;
        let dur = items[idx].duration_ms;
        if dur == () || dur <= 0 { dur = unmeasured_duration_ms(); }
        if dur <= remaining_ms {
            chosen = idx;
            break;
        }
    }
    if chosen == -1 { chosen = ptr % n; }
    chosen
}

fn arrange(ctx) {
    let parsed = read_dayparts(ctx);
    let dayparts = parsed.dayparts;
    let default_pool = parsed.default_pool;

    let fill_secs = ctx.window.fill_secs;
    if fill_secs == () { fill_secs = 86400; }

    let pointers = #{};
    for name in ctx.pools.keys() { pointers[name] = 0; }

    let cursor = ctx.window.from;
    let stop = ctx.window.from + fill_secs;
    let out = [];
    let safety = 0;
    while cursor < stop && safety < 2000 {
        safety += 1;
        let active = active_pool_at(cursor, dayparts, default_pool);
        let boundary = next_boundary(cursor, active, dayparts, default_pool);
        let remaining = boundary - cursor;
        if remaining < 1 { remaining = 1; }

        let chosen_pool = active;
        let idx = pick_index(chosen_pool, ctx.pools, pointers[chosen_pool], remaining);
        if idx == () && active != default_pool {
            // This daypart's own pool is empty — the default fills the slot
            // rather than leaving it dark. The pointer that advances below
            // must be the default pool's, not the empty daypart's.
            chosen_pool = default_pool;
            idx = pick_index(chosen_pool, ctx.pools, pointers[chosen_pool], remaining);
        }
        if idx == () { break; } // every pool involved is empty; nothing to lay.

        let items = ctx.pools[chosen_pool];
        pointers[chosen_pool] = (idx + 1) % items.len;
        let item = items[idx];

        out.push(item.entry_id);
        let dur_ms = item.duration_ms;
        if dur_ms == () || dur_ms <= 0 { dur_ms = unmeasured_duration_ms(); }
        cursor += (dur_ms / 1000);
    }

    out
}
