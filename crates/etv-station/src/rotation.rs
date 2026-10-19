//! Slot rotations (etv-station-xm8p.1): a `rule.slots` entry that lists other
//! slots with `slots:` and airs one of them per placement.
//!
//! A *placement* is one range of one schedule on one local day — `19..23` on
//! Tuesday is one placement, `19..23` on Wednesday the next. A rotation draws
//! once per placement, so every generation that opens inside a placement airs
//! the member the first one drew. The draw is a function of the config, the
//! channel seed, the rotation's name and its [`RotationResume`], so a rerun, a
//! restart that reloads `.resume`, and the Review simulation all draw the same
//! member.
//!
//! `select`, `order`, `weight` and `constraints.no_repeat_within` are the
//! pool's words and keep the pool's meaning: `round_robin` visits every member
//! once per pass (`order: random` reshuffles each pass from the seed), `random`
//! draws by weight, and `no_repeat_within` keeps a member off the air for that
//! many placements or that much time. When it leaves no member eligible, the
//! least recently drawn one airs and the draw says the constraint was relaxed.

use std::collections::BTreeMap;

use time::OffsetDateTime;

use crate::config::{BlockInclude, NoRepeatWithin, Order, RuleConfig, Select};
use crate::pattern::mix_seed;
use crate::resume::{RotationDraw, RotationResume};

/// How many draws a rotation remembers. Bounds the sidecar; more than any
/// `no_repeat_within` in a real config reaches back.
const RECENT_CAP: usize = 64;

/// One draw's account of itself, for the `calendar.select` log and the
/// rotation preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawReason {
    /// The rotation slot that drew.
    pub rotation: String,
    /// The member it drew.
    pub member: String,
    /// `true` when the placement had already drawn and this is that draw again.
    pub reused: bool,
    /// `round_robin` or `random`.
    pub select: &'static str,
    /// The cursor position (`round_robin`) or the random draw and its range.
    pub detail: String,
    /// Members `no_repeat_within` passed over.
    pub skipped: Vec<String>,
    /// `no_repeat_within` left no eligible member and the least recently drawn
    /// one aired.
    pub relaxed: bool,
}

impl DrawReason {
    pub fn describe(&self) -> String {
        let mut out = if self.reused {
            format!("{}, placement already drawn", self.select)
        } else {
            format!("{} {}", self.select, self.detail)
        };
        if !self.skipped.is_empty() {
            out.push_str(&format!(
                "; skipped by no_repeat_within: {}",
                self.skipped.join(", ")
            ));
        }
        if self.relaxed {
            out.push_str("; no_repeat_within relaxed, least recently drawn airs");
        }
        out
    }
}

/// The chain a placement of a rotation resolved through, outermost first. The
/// last entry's `member` is the slot that airs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drawn {
    pub chain: Vec<DrawReason>,
}

impl Drawn {
    /// The slot that airs: the last member drawn.
    pub fn leaf(&self) -> &str {
        &self
            .chain
            .last()
            .expect("a draw has at least one step")
            .member
    }
}

/// Resolve rotation `slot`, placed over the placement starting at `start`
/// (a unix timestamp), down to the slot that airs, drawing at each rotation on
/// the way and recording each draw in `rotations`.
///
/// `seed_at` answers the channel seed at an instant; the draw reads it at the
/// placement's start, so every generation inside a placement sees one seed.
pub fn resolve(
    rule: &RuleConfig,
    slot: &str,
    start: i64,
    seed_at: &dyn Fn(OffsetDateTime) -> u64,
    rotations: &mut BTreeMap<String, RotationResume>,
) -> Drawn {
    let at = OffsetDateTime::from_unix_timestamp(start).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let seed = seed_at(at);
    let mut chain = Vec::new();
    let mut current = slot;
    while let Some(block) = rule.slots.get(current).filter(|b| b.is_rotation()) {
        let state = rotations.entry(current.to_string()).or_default();
        let reason = draw(current, block, state, seed, start, at);
        current = block
            .slots
            .iter()
            .find(|m| **m == reason.member)
            .map(String::as_str)
            .expect("a draw names one of the rotation's members");
        chain.push(reason);
    }
    Drawn { chain }
}

/// One rotation's draw for the placement starting at `start`.
fn draw(
    name: &str,
    block: &BlockInclude,
    state: &mut RotationResume,
    seed: u64,
    start: i64,
    at: OffsetDateTime,
) -> DrawReason {
    let select = block.select.unwrap_or_default();
    let select_name = match select {
        Select::RoundRobin => "round_robin",
        Select::Random => "random",
    };
    // A placement already in the draw record keeps its member: a generation
    // that opens mid-placement, a restart, and the preview of a placement
    // generated ahead of the clock all read the draw back instead of making a
    // second one. A member the config has since dropped is drawn again.
    if let Some(member) = state
        .recent
        .iter()
        .find(|d| d.at == at && block.slots.contains(&d.member))
        .map(|d| d.member.clone())
    {
        return DrawReason {
            rotation: name.to_string(),
            member,
            reused: true,
            select: select_name,
            detail: String::new(),
            skipped: Vec::new(),
            relaxed: false,
        };
    }

    let gap = block
        .constraints
        .as_ref()
        .and_then(|c| c.no_repeat_within)
        .unwrap_or_default();
    let eligible = |member: &str| is_eligible(&state.recent, member, gap, at);
    let name_salt = fnv1a(name);
    let base = mix_seed(seed, name_salt);

    let mut skipped = Vec::new();
    let mut relaxed = false;
    let (member, detail) = match select {
        Select::RoundRobin => {
            if block.slots.iter().all(|m| state.drawn.contains(m)) {
                state.cycle += 1;
                state.drawn.clear();
            }
            let order = cycle_order(block, base, state.cycle);
            let remaining: Vec<&String> =
                order.iter().filter(|m| !state.drawn.contains(*m)).collect();
            let pick = match remaining.iter().position(|m| eligible(m)) {
                Some(i) => {
                    skipped = remaining[..i].iter().map(|m| (*m).clone()).collect();
                    remaining[i].clone()
                }
                None => {
                    relaxed = true;
                    least_recent(&state.recent, remaining.iter().copied())
                }
            };
            let position = state.drawn.len() + 1;
            state.drawn.push(pick.clone());
            (
                pick,
                format!(
                    "cursor {position}/{} of cycle {}",
                    block.slots.len(),
                    state.cycle
                ),
            )
        }
        Select::Random => {
            let candidates: Vec<&String> = block.slots.iter().filter(|m| eligible(m)).collect();
            skipped = block
                .slots
                .iter()
                .filter(|m| !candidates.contains(m))
                .cloned()
                .collect();
            if candidates.is_empty() {
                relaxed = true;
                skipped.clear();
                let pick = least_recent(&state.recent, block.slots.iter());
                (
                    pick,
                    String::from("draw, every member within no_repeat_within"),
                )
            } else {
                let weight = |m: &String| block.weight.get(m).copied().unwrap_or(1.0);
                let total: f64 = candidates.iter().map(|m| weight(m)).sum();
                let roll = unit(mix_seed(base, start as u64));
                let mut target = roll * total;
                let mut pick = candidates[candidates.len() - 1];
                for m in &candidates {
                    target -= weight(m);
                    if target < 0.0 {
                        pick = m;
                        break;
                    }
                }
                (
                    pick.clone(),
                    format!("draw {roll:.4} of {} eligible", candidates.len()),
                )
            }
        }
    };

    state.recent.push(RotationDraw {
        member: member.clone(),
        at,
    });
    // Keep what the constraint can still read: at least RECENT_CAP draws, and
    // every draw a positional gap or a time gap reaches back to.
    let keep = match gap {
        NoRepeatWithin::Positions(n) => RECENT_CAP.max(n),
        NoRepeatWithin::Duration(_) => usize::MAX,
    };
    let horizon = match gap {
        NoRepeatWithin::Duration(d) => Some(at - d),
        _ => None,
    };
    while state.recent.len() > RECENT_CAP
        && (state.recent.len() > keep || horizon.is_some_and(|h| state.recent[0].at < h))
    {
        state.recent.remove(0);
    }
    DrawReason {
        rotation: name.to_string(),
        member,
        reused: false,
        select: select_name,
        detail,
        skipped,
        relaxed,
    }
}

/// Whether `member` may air at `at` under `gap`, measured from the start of
/// one draw to the start of the next.
fn is_eligible(
    recent: &[RotationDraw],
    member: &str,
    gap: NoRepeatWithin,
    at: OffsetDateTime,
) -> bool {
    match gap {
        NoRepeatWithin::Positions(0) => true,
        NoRepeatWithin::Positions(n) => !recent.iter().rev().take(n).any(|d| d.member == member),
        NoRepeatWithin::Duration(d) => !recent
            .iter()
            .any(|r| r.member == member && at >= r.at && (at - r.at).unsigned_abs() < d),
    }
}

/// The member drawn longest ago among `from`; one never drawn counts as
/// longest ago. Ties keep `from`'s order.
fn least_recent<'a>(recent: &[RotationDraw], from: impl Iterator<Item = &'a String>) -> String {
    from.min_by_key(|m| {
        recent
            .iter()
            .rposition(|d| d.member == **m)
            .map_or(0, |i| i + 1)
    })
    .expect("a rotation has members")
    .clone()
}

/// The members in the order cycle `cycle` visits them: as written, or a seeded
/// shuffle under `order: random`.
fn cycle_order(block: &BlockInclude, base: u64, cycle: u64) -> Vec<String> {
    let mut order = block.slots.clone();
    if matches!(block.order, Some(Order::Random)) {
        let seed = mix_seed(base, cycle);
        for i in (1..order.len()).rev() {
            let j = (mix_seed(seed, i as u64) % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }
    }
    order
}

/// A `u64` as a float in `[0, 1)`.
fn unit(bits: u64) -> f64 {
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

/// FNV-1a, so a name salts the seed the same way on every build.
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// One upcoming placement of a rotation and the member it will draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRow {
    pub rotation: String,
    pub schedule: String,
    pub start: OffsetDateTime,
    /// The member this rotation draws.
    pub member: String,
    /// The slot that airs, when `member` is itself a rotation.
    pub airs: String,
    pub reason: String,
}

/// Which member airs on each of the next `n` placements of every rotation a
/// schedule places, read straight from the rotation's state with no generation
/// run. A placement already drawn (the one on air) shows the draw it made.
///
/// Placements are found by walking the calendar from `from`, so the rows come
/// out in time order across rotations, at most `n` per rotation.
pub fn preview(
    rule: &RuleConfig,
    calendar: &crate::calendar::Calendar<'_>,
    seed_at: &dyn Fn(OffsetDateTime) -> u64,
    rotations: &BTreeMap<String, RotationResume>,
    from: OffsetDateTime,
    n: usize,
) -> Vec<PreviewRow> {
    let mut state = rotations.clone();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut seen: std::collections::BTreeSet<(String, i64)> = Default::default();
    let mut rows = Vec::new();
    let wanted: std::collections::BTreeSet<&str> = rule
        .schedules
        .iter()
        .flat_map(|s| s.hours.values())
        .map(String::as_str)
        .filter(|slot| rule.slots.get(*slot).is_some_and(|b| b.is_rotation()))
        .collect();
    if n == 0 || wanted.is_empty() {
        return rows;
    }
    let mut t = from;
    let horizon = from + time::Duration::days(366);
    let mut finished = 0usize;
    while t < horizon && finished < wanted.len() {
        if let crate::calendar::OnAir::Slot {
            slot,
            schedule,
            placement: Some(start),
            ..
        } = calendar.on_air(t)
            && seen.insert((slot.to_string(), start))
        {
            let count = counts.entry(slot.to_string()).or_default();
            if *count < n {
                *count += 1;
                if *count == n {
                    finished += 1;
                }
                let drawn = resolve(rule, slot, start, seed_at, &mut state);
                let first = &drawn.chain[0];
                rows.push(PreviewRow {
                    rotation: slot.to_string(),
                    schedule: schedule.to_string(),
                    start: OffsetDateTime::from_unix_timestamp(start)
                        .unwrap_or(OffsetDateTime::UNIX_EPOCH),
                    member: first.member.clone(),
                    airs: drawn.leaf().to_string(),
                    reason: drawn
                        .chain
                        .iter()
                        .map(DrawReason::describe)
                        .collect::<Vec<_>>()
                        .join(" -> "),
                });
            }
        }
        t += time::Duration::MINUTE;
    }
    rows
}

/// [`preview`] for a loaded channel: the calendar and the seed come from its
/// own config, so the rows match what the resolver will draw. Empty for a
/// channel with no rotation placed by a schedule.
pub fn preview_channel(
    config: &crate::config::ChannelConfig,
    tz: &'static time_tz::Tz,
    rotations: &BTreeMap<String, RotationResume>,
    from: OffsetDateTime,
    n: usize,
) -> Vec<PreviewRow> {
    let Some(calendar) = crate::calendar::Calendar::new(&config.rule, tz) else {
        return Vec::new();
    };
    let variety = crate::variety::Variety::new(config, tz);
    preview(
        &config.rule,
        &calendar,
        &|at| variety.dice(at, None).seed,
        rotations,
        from,
        n,
    )
}

/// The rotation state a channel's `.resume` sidecar holds, or none when there
/// is no readable sidecar — the preview of a channel that has never run starts
/// every rotation at its first member. A sidecar [`crate::resume::load`] would
/// discard (unparseable, other version) yields none, the same as the rest of
/// the resume state.
pub fn read_rotations(output_folder: &std::path::Path) -> BTreeMap<String, RotationResume> {
    std::fs::read(crate::resume::sidecar_path(output_folder))
        .ok()
        .map(|bytes| crate::resume::parse(&bytes).0.rotations)
        .unwrap_or_default()
}

/// The preview as the JSON the Review view and `--audit --format json` carry.
pub fn rows_json(rows: &[PreviewRow]) -> serde_json::Value {
    let rfc3339 = |t: OffsetDateTime| {
        t.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    };
    serde_json::Value::Array(
        rows.iter()
            .map(|r| {
                serde_json::json!({
                    "rotation": r.rotation,
                    "schedule": r.schedule,
                    "start": rfc3339(r.start),
                    "member": r.member,
                    "airs": r.airs,
                    "reason": r.reason,
                })
            })
            .collect(),
    )
}

/// The preview as the text block `--audit` prints; empty when there are no rows.
pub fn render_text(rows: &[PreviewRow]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut out =
        String::from("\n== rotation preview: which member airs on each upcoming placement ==\n");
    for r in rows {
        let airs = if r.airs == r.member {
            String::new()
        } else {
            format!(" (airs {})", r.airs)
        };
        out.push_str(&format!(
            "{} {} [{}] -> {}{airs}\n    {}\n",
            r.start.date(),
            r.start.time(),
            r.rotation,
            r.member,
            r.reason,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn read_rotations_honours_the_sidecar_version() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = crate::resume::sidecar_path(dir.path());
        let current = crate::resume::ResumeMap::new().version;
        let body = |version: u32| {
            format!(r#"{{"version":{version},"rotations":{{"prime":{{"cycle":3}}}}}}"#)
        };
        std::fs::write(&sidecar, body(current)).unwrap();
        assert_eq!(read_rotations(dir.path())["prime"].cycle, 3);
        std::fs::write(&sidecar, body(current + 1)).unwrap();
        assert!(read_rotations(dir.path()).is_empty());
    }

    fn rule(yaml: &str) -> RuleConfig {
        serde_norway::from_str(yaml).unwrap()
    }

    const CHANNEL: &str = r#"
blocks: [{ entries: [] }]
slots:
  prime:
    slots: [a, b, c]
    select: round_robin
  a: { entries: [] }
  b: { entries: [] }
  c: { entries: [] }
schedules:
  - name: prime
    hours: { "19..23": prime }
"#;

    fn seed(_: OffsetDateTime) -> u64 {
        7
    }

    fn days(n: usize) -> Vec<i64> {
        let base = datetime!(2026-10-19 19:00 UTC).unix_timestamp();
        (0..n as i64).map(|d| base + d * 86_400).collect()
    }

    fn members(
        r: &RuleConfig,
        starts: &[i64],
        state: &mut BTreeMap<String, RotationResume>,
    ) -> Vec<String> {
        starts
            .iter()
            .map(|s| resolve(r, "prime", *s, &seed, state).leaf().to_string())
            .collect()
    }

    #[test]
    fn round_robin_visits_every_member_once_per_cycle() {
        let r = rule(CHANNEL);
        let mut state = BTreeMap::new();
        let got = members(&r, &days(6), &mut state);
        assert_eq!(got, ["a", "b", "c", "a", "b", "c"]);
    }

    #[test]
    fn a_shuffled_cycle_still_covers_every_member() {
        let mut r = rule(CHANNEL);
        r.slots.get_mut("prime").unwrap().order = Some(Order::Random);
        let mut state = BTreeMap::new();
        let got = members(&r, &days(9), &mut state);
        for cycle in got.chunks(3) {
            let mut sorted = cycle.to_vec();
            sorted.sort();
            assert_eq!(sorted, ["a", "b", "c"], "cycle {cycle:?}");
        }
    }

    #[test]
    fn a_placement_keeps_the_member_its_first_generation_drew() {
        let r = rule(CHANNEL);
        let mut state = BTreeMap::new();
        let start = days(1)[0];
        let first = resolve(&r, "prime", start, &seed, &mut state);
        let again = resolve(&r, "prime", start, &seed, &mut state);
        assert_eq!(first.leaf(), again.leaf());
        assert!(again.chain[0].reused);
        assert_eq!(state["prime"].recent.len(), 1, "one draw, not two");
    }

    #[test]
    fn draws_are_identical_after_the_state_is_written_and_read_back() {
        let r = rule(CHANNEL);
        let mut live = BTreeMap::new();
        let starts = days(8);
        let mut got = members(&r, &starts[..4], &mut live);
        let stored = serde_json::to_string(&live).unwrap();
        let mut reloaded: BTreeMap<String, RotationResume> = serde_json::from_str(&stored).unwrap();
        got.extend(members(&r, &starts[4..], &mut reloaded));
        let mut straight = BTreeMap::new();
        assert_eq!(got, members(&r, &starts, &mut straight));
    }

    #[test]
    fn random_honours_no_repeat_within() {
        let mut r = rule(CHANNEL);
        let p = r.slots.get_mut("prime").unwrap();
        p.select = Some(Select::Random);
        p.constraints = Some(crate::config::Constraints {
            no_repeat_within: Some(NoRepeatWithin::Positions(2)),
            ..Default::default()
        });
        let mut state = BTreeMap::new();
        let got = members(&r, &days(30), &mut state);
        for w in got.windows(3) {
            assert!(
                w[0] != w[1] && w[0] != w[2] && w[1] != w[2],
                "three placements in a row repeat a member: {w:?}"
            );
        }
    }

    #[test]
    fn a_duration_gap_measures_from_draw_to_draw() {
        let mut r = rule(CHANNEL);
        let p = r.slots.get_mut("prime").unwrap();
        p.select = Some(Select::Random);
        p.constraints = Some(crate::config::Constraints {
            no_repeat_within: Some(NoRepeatWithin::Duration(std::time::Duration::from_secs(
                36 * 3600,
            ))),
            ..Default::default()
        });
        let mut state = BTreeMap::new();
        let got = members(&r, &days(20), &mut state);
        // 36h reaches back one daily placement, never two.
        for w in got.windows(2) {
            assert_ne!(w[0], w[1]);
        }
    }

    #[test]
    fn with_no_eligible_member_the_least_recently_drawn_airs_and_says_so() {
        let mut r = rule(CHANNEL);
        let p = r.slots.get_mut("prime").unwrap();
        p.select = Some(Select::Random);
        p.constraints = Some(crate::config::Constraints {
            no_repeat_within: Some(NoRepeatWithin::Positions(5)),
            ..Default::default()
        });
        let mut state = BTreeMap::new();
        let starts = days(4);
        let first_three = members(&r, &starts[..3], &mut state);
        let fourth = resolve(&r, "prime", starts[3], &seed, &mut state);
        assert!(fourth.chain[0].relaxed, "{:?}", fourth.chain[0]);
        assert_eq!(fourth.leaf(), first_three[0], "the oldest draw airs again");
    }

    #[test]
    fn weight_skews_a_random_rotation() {
        let mut r = rule(CHANNEL);
        let p = r.slots.get_mut("prime").unwrap();
        p.select = Some(Select::Random);
        p.weight.insert("a".into(), 20.0);
        let mut state = BTreeMap::new();
        let got = members(&r, &days(200), &mut state);
        let a = got.iter().filter(|m| *m == "a").count();
        assert!(a > 120, "a weighs 20 of 22 and aired {a} of 200");
    }

    #[test]
    fn a_nested_rotation_resolves_to_the_slot_that_airs() {
        let r = rule(
            r#"
blocks: [{ entries: [] }]
slots:
  prime: { slots: [movies, a] }
  movies: { slots: [m1, m2] }
  a: { entries: [] }
  m1: { entries: [] }
  m2: { entries: [] }
schedules:
  - name: prime
    hours: { "19..23": prime }
"#,
        );
        let mut state = BTreeMap::new();
        let starts = days(4);
        let got: Vec<Drawn> = starts
            .iter()
            .map(|s| resolve(&r, "prime", *s, &seed, &mut state))
            .collect();
        let leaves: Vec<&str> = got.iter().map(Drawn::leaf).collect();
        assert_eq!(leaves, ["m1", "a", "m2", "a"]);
        assert_eq!(got[0].chain.len(), 2);
        assert_eq!(got[0].chain[0].rotation, "prime");
        assert_eq!(got[0].chain[1].rotation, "movies");
    }

    #[test]
    fn a_wrapping_range_is_one_placement_across_midnight() {
        let r = rule(
            r#"
blocks: [{ entries: [] }]
slots:
  prime: { slots: [a, b] }
  a: { entries: [] }
  b: { entries: [] }
schedules:
  - name: late
    hours: { "21..02": prime }
"#,
        );
        let cal = crate::calendar::Calendar::new(&r, time_tz::timezones::db::UTC).unwrap();
        let mut state = BTreeMap::new();
        let mut leaf = |at| {
            let (on_air, drawn) = cal.on_air_drawn(at, &seed, &mut state);
            assert!(matches!(on_air, crate::calendar::OnAir::Slot { .. }));
            drawn.unwrap().leaf().to_string()
        };
        let before = leaf(datetime!(2026-10-19 22:00 UTC));
        let after = leaf(datetime!(2026-10-20 01:00 UTC));
        assert_eq!(before, after, "one placement, one draw");
        assert_ne!(before, leaf(datetime!(2026-10-20 22:00 UTC)));
    }

    #[test]
    fn the_preview_lists_the_next_placements_without_touching_the_state() {
        let r = rule(CHANNEL);
        let cal = crate::calendar::Calendar::new(&r, time_tz::timezones::db::UTC).unwrap();
        let state = BTreeMap::new();
        let rows = preview(&r, &cal, &seed, &state, datetime!(2026-10-19 12:00 UTC), 4);
        let got: Vec<(&str, OffsetDateTime)> =
            rows.iter().map(|r| (r.airs.as_str(), r.start)).collect();
        assert_eq!(
            got,
            [
                ("a", datetime!(2026-10-19 19:00 UTC)),
                ("b", datetime!(2026-10-20 19:00 UTC)),
                ("c", datetime!(2026-10-21 19:00 UTC)),
                ("a", datetime!(2026-10-22 19:00 UTC)),
            ]
        );
        assert!(state.is_empty());
    }
}
