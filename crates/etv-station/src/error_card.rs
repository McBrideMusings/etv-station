//! The on-screen card that takes a slot when its file cannot be played.
//!
//! A 24/7 channel does not stop broadcasting because one tape in the library is
//! snapped. When a file cannot be probed or opened, the item keeps its place and
//! its length and this module supplies what fills it instead: black, with the
//! title that was meant to air and the reason it is not.
//!
//! # Why it is a `lavfi` source and not a rendered image
//!
//! An authored `lavfi` item is already a first-class source — the smoke channel
//! is nothing but lavfi — so a card built as one needs no new concept anywhere
//! downstream. The emitter, the playout JSON schema, and ETV-next all handle it
//! as the ordinary item it looks like. Rendering a PNG would mean a file to
//! write, own, clean up, and keep in sync with the item it belongs to.

use std::path::Path;
use std::time::Duration;

use crate::config::SourceConfig;
use crate::resolve::ResolvedItem;

/// Where the card gets its typeface. The runtime image ships Noto;
/// `ETV_STATION_ERROR_FONT` overrides it for anyone running the daemon outside
/// that image. A path that does not exist is not fatal — ffmpeg's `drawtext`
/// falls back through fontconfig — so a card still renders on a host without
/// this exact file.
const DEFAULT_ERROR_FONT: &str = "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf";

/// The card's fixed render size, also passed to ffmpeg's `color=s=`.
const CARD_WIDTH_PX: u32 = 1280;
const CARD_HEIGHT_PX: u32 = 720;

/// Left edge every line is drawn from (`x=` in the filter graph). Applied
/// again on the right when working out how many characters fit, so text
/// never touches the frame edge it's approaching.
const TEXT_MARGIN_PX: u32 = 60;

const HEADLINE_FONTSIZE: u32 = 52;
const SUBJECT_FONTSIZE: u32 = 30;
const REASON_FONTSIZE: u32 = 24;

const HEADLINE_Y: u32 = 60;
const SUBJECT_Y: u32 = 150;
const REASON_Y: u32 = 210;

/// Vertical gap between wrapped reason lines. Comfortably more than
/// `REASON_FONTSIZE` so descenders on one line don't touch ascenders on the
/// next.
const REASON_LINE_HEIGHT_PX: u32 = 32;

/// The reason wraps across at most this many rows before the remainder is
/// dropped with a trailing `...`. Three rows at `REASON_FONTSIZE` comfortably
/// fits under the card's 720px height starting from `REASON_Y`.
const REASON_MAX_LINES: usize = 3;

/// Average glyph width for NotoSans (and the fontconfig sans-serif it falls
/// back to), as a fraction of fontsize. Text is proportional, not monospace,
/// so this is an estimate rather than a measurement — chosen conservatively
/// (rounding the character budget down) so a wider-than-average string still
/// can't reach the frame edge.
const CHAR_WIDTH_FACTOR: f32 = 0.55;

/// How many sanitized characters fit on one line at `fontsize`, given the
/// card's fixed width and margins. Derived rather than hard-coded so a future
/// change to the card size or a line's fontsize keeps the cut correct.
fn max_chars_per_line(fontsize: u32) -> usize {
    let usable_px = (CARD_WIDTH_PX.saturating_sub(TEXT_MARGIN_PX * 2)) as f32;
    let char_width_px = fontsize as f32 * CHAR_WIDTH_FACTOR;
    ((usable_px / char_width_px).floor() as usize).max(1)
}

/// Rewrite `item` in place into a black card that says, on screen, which file
/// failed and what ffmpeg said about it — sized to exactly the slot the real
/// item would have filled, so nothing downstream shifts.
///
/// The item keeps its `program` metadata, so the guide still lists the film that
/// was meant to air. A viewer who tunes in sees the title they expected and the
/// reason it is not playing, rather than a channel that went quiet.
pub fn make_error_card(item: &mut ResolvedItem, path: &Path, reason: &str, slot: Duration) {
    let title = item
        .program
        .as_ref()
        .and_then(|p| p.title.clone())
        .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| item.id.clone());

    item.source = SourceConfig::Lavfi {
        params: playback_error_params(&title, reason),
    };
    item.in_point = Some(Duration::ZERO);
    item.out_point = Some(slot);
    item.error_card = true;
}

/// The `lavfi` params behind [`make_error_card`]'s screen, for a caller that
/// has no [`ResolvedItem`] to rewrite.
///
/// The reconciliation sweep ([`crate::reconcile`]) patches items in playout
/// JSON that has already been written, where the item is a
/// `PlayoutItem` — the emitted shape, past the point where a `ResolvedItem`
/// exists. It needs the same screen, so it takes the same params rather than
/// growing a second card that drifts from this one.
pub(crate) fn playback_error_params(title: &str, reason: &str) -> String {
    card_params("PLAYBACK ERROR", title, reason)
}

/// Build one segment of the card a channel airs when its **own loop** has died
/// — a different trigger from [`make_error_card`] above (nothing is wrong with
/// any single file; the thing that schedules them stopped running), rendered the
/// same way so a viewer sees one consistent failure screen.
///
/// Returned as a fresh item rather than applied to an existing one because there
/// is no resolved item to rewrite: the channel failed before it produced any.
pub fn make_channel_card(id: String, channel: &str, reason: &str, slot: Duration) -> ResolvedItem {
    ResolvedItem {
        block: 0,
        id,
        source: SourceConfig::Lavfi {
            params: card_params("CHANNEL UNAVAILABLE", channel, reason),
        },
        in_point: Some(Duration::ZERO),
        out_point: Some(slot),
        // The guide should say what the screen says, not leave a blank cell.
        program: Some(ersatztv_playout::playout::ProgramMetadata {
            title: Some(format!("{channel} unavailable")),
            description: Some(reason.to_string()),
            ..Default::default()
        }),
        catalog_duration: None,
        error_card: true,
        metadata: None,
        guide: None,
        guide_fields: crate::guide::GuideFields::default(),
    }
}

/// Stacked lines on black, plus silence. The filter-graph shape — two
/// labelled outputs — is the same one an authored `lavfi` item uses, so nothing
/// further down needs to know this item is special.
///
/// The headline and subject are short by construction (a fixed label, a
/// title) and get one line each, clamped to what fits. The reason is
/// arbitrary — often ffmpeg's own message or a config error naming a file —
/// so it wraps across up to [`REASON_MAX_LINES`] rows instead of being cut to
/// a single row, which is what used to push the informative part of a long
/// reason off the right edge of the frame entirely.
fn card_params(headline: &str, subject: &str, reason: &str) -> String {
    let font =
        std::env::var("ETV_STATION_ERROR_FONT").unwrap_or_else(|_| DEFAULT_ERROR_FONT.to_string());
    let draw = |text: &str, size: u32, y: u32| {
        format!(
            "drawtext=fontfile={font}:text={text}:fontcolor=white:fontsize={size}:x={TEXT_MARGIN_PX}:y={y}"
        )
    };

    let headline_line = draw(
        &clamp_to_width(headline, HEADLINE_FONTSIZE),
        HEADLINE_FONTSIZE,
        HEADLINE_Y,
    );
    let subject_line = draw(
        &clamp_to_width(subject, SUBJECT_FONTSIZE),
        SUBJECT_FONTSIZE,
        SUBJECT_Y,
    );
    let reason_lines: Vec<String> = wrap_to_width(reason, REASON_FONTSIZE, REASON_MAX_LINES)
        .iter()
        .enumerate()
        .map(|(i, line)| {
            draw(
                line,
                REASON_FONTSIZE,
                REASON_Y + i as u32 * REASON_LINE_HEIGHT_PX,
            )
        })
        .collect();

    format!(
        "color=c=black:s={CARD_WIDTH_PX}x{CARD_HEIGHT_PX}:r=30,{headline_line},{subject_line},{} [out0]; \
         anullsrc=channel_layout=stereo:sample_rate=48000 [out1]",
        reason_lines.join(","),
    )
}

/// Reduce arbitrary text to something an ffmpeg filter-graph argument can carry
/// literally, without yet deciding how much of it fits on a line.
///
/// ffmpeg's filter syntax gives meaning to `:` `,` `;` `[` `]` `'` `\` and `%`,
/// and the escaping rules differ by nesting depth — which is exactly the sort of
/// thing that turns a helpful error message into a broken filter graph and a
/// dead channel. Rather than escape, this drops the whole problem: anything not
/// plainly printable becomes a space. "moov atom not found" survives intact,
/// which is the part worth reading.
///
/// The whitelist is ASCII-only, so a later length cut can never land inside a
/// multi-byte character.
fn sanitize_for_drawtext(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = false;
    for ch in text.chars() {
        let keep = ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '.' | '-' | '_' | '(' | ')');
        let ch = if keep { ch } else { ' ' };
        if ch == ' ' {
            if last_space {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

/// Sanitize `text` and cut it to whatever fits on one line at `fontsize`. Used
/// for the headline and subject, which are short by construction and don't
/// need wrapping — just a guarantee they can't run past the frame edge.
fn clamp_to_width(text: &str, fontsize: u32) -> String {
    let sanitized = sanitize_for_drawtext(text);
    let max_chars = max_chars_per_line(fontsize);
    let clamped = if sanitized.len() > max_chars {
        // ASCII-only (sanitize_for_drawtext guarantees it), so byte length is
        // char count and any byte offset is a char boundary.
        sanitized[..max_chars].trim_end()
    } else {
        sanitized.trim_end()
    };
    if clamped.is_empty() {
        "unknown".to_string()
    } else {
        clamped.to_string()
    }
}

/// Sanitize `text` and word-wrap it into at most `max_lines` rows that each
/// fit at `fontsize`. A single word longer than one line is hard-split rather
/// than left to overflow. If the text still doesn't fit in `max_lines`, the
/// last row is cut and marked with a trailing `...` rather than silently
/// dropping the remainder without any sign text is missing.
fn wrap_to_width(text: &str, fontsize: u32, max_lines: usize) -> Vec<String> {
    let sanitized = sanitize_for_drawtext(text);
    let max_chars = max_chars_per_line(fontsize);

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in sanitized.split_whitespace() {
        if word.len() > max_chars {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            let mut rest = word;
            while rest.len() > max_chars {
                let (head, tail) = rest.split_at(max_chars);
                lines.push(head.to_string());
                rest = tail;
            }
            if !rest.is_empty() {
                current = rest.to_string();
            }
            continue;
        }

        let candidate_len = if current.is_empty() {
            word.len()
        } else {
            current.len() + 1 + word.len()
        };
        if candidate_len > max_chars {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }

    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            const ELLIPSIS: &str = "...";
            let budget = max_chars.saturating_sub(ELLIPSIS.len());
            if last.len() > budget {
                last.truncate(budget);
            }
            last.push_str(ELLIPSIS);
        }
    }

    if lines.is_empty() || lines.iter().all(|line| line.trim().is_empty()) {
        return vec!["unknown".to_string()];
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_what_would_break_the_filter_graph() {
        // ffmpeg gives meaning to : , ; [ ] ' \ % — none may survive.
        let out = sanitize_for_drawtext("moov atom not found: /media/a'b[c],d;e\\f%g");
        assert!(!out.contains(':'));
        assert!(!out.contains('\''));
        assert!(!out.contains('['));
        assert!(!out.contains(';'));
        assert!(!out.contains('%'));
        assert!(!out.contains('\\'));
        assert!(
            out.starts_with("moov atom not found"),
            "the readable part survives: {out}"
        );
    }

    #[test]
    fn clamp_and_wrap_never_yield_an_empty_argument() {
        // An all-punctuation reason would otherwise produce `text=`, which is a
        // filter-graph parse error and would take the channel down — the exact
        // failure this whole path exists to prevent.
        assert_eq!(clamp_to_width(":::", REASON_FONTSIZE), "unknown");
        assert_eq!(clamp_to_width("", REASON_FONTSIZE), "unknown");
        assert_eq!(
            wrap_to_width(":::", REASON_FONTSIZE, REASON_MAX_LINES),
            vec!["unknown"]
        );
        assert_eq!(
            wrap_to_width("", REASON_FONTSIZE, REASON_MAX_LINES),
            vec!["unknown"]
        );
    }

    #[test]
    fn clamp_to_width_cuts_long_text_without_splitting_a_character() {
        let max_chars = max_chars_per_line(REASON_FONTSIZE);
        let out = clamp_to_width(&"a".repeat(500), REASON_FONTSIZE);
        assert!(out.len() <= max_chars);
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn max_chars_per_line_shrinks_as_fontsize_grows() {
        // Bigger glyphs, same 1280px card, fewer characters fit per row.
        assert!(max_chars_per_line(HEADLINE_FONTSIZE) < max_chars_per_line(REASON_FONTSIZE));
    }

    #[test]
    fn no_wrapped_or_clamped_line_can_reach_the_frame_edge() {
        // The acceptance bar from #147: nothing rendered may extend past the
        // right edge of the frame at the card's configured width and fontsize.
        let long_reason =
            "block #0: a query entry needs the catalog, which is not available ".repeat(4);
        for line in wrap_to_width(&long_reason, REASON_FONTSIZE, REASON_MAX_LINES) {
            assert!(line.len() <= max_chars_per_line(REASON_FONTSIZE));
        }
        let long_subject = "a".repeat(500);
        assert!(
            clamp_to_width(&long_subject, SUBJECT_FONTSIZE).len()
                <= max_chars_per_line(SUBJECT_FONTSIZE)
        );
    }

    #[test]
    fn channel_level_reason_is_fully_visible_behind_a_long_path() {
        // #147: a config error's useful text sits at the *end*, behind a long
        // absolute path. A single-row, front-truncated card always lost it;
        // wrapping must keep the whole sentence somewhere on screen.
        let path = "/private/tmp/claude-501/-Users-pierce-Projects-etv-station/006ca49c-a17d";
        assert_eq!(
            path.len(),
            72,
            "path fixture should stay realistic-looking; adjust if this drifts"
        );
        let reason = format!(
            "unsupported config at {path} block #0: a query entry needs the catalog, which is not available"
        );
        let lines = wrap_to_width(&reason, REASON_FONTSIZE, REASON_MAX_LINES);
        let rendered = lines.join(" ");
        // '#' isn't in the drawtext whitelist (sanitize_for_drawtext turns it
        // into a space), so "#0" survives as "0".
        assert!(
            rendered.contains("block 0 a query entry needs the catalog which is not available"),
            "the sentence describing what broke must survive wrapping: {rendered}"
        );
    }

    #[test]
    fn per_item_reason_still_leads_with_the_useful_part() {
        // #117's shipped per-item card: the useful text (ffmpeg's message) is
        // at the *front*, and must still be readable after this change.
        let lines = wrap_to_width("moov atom not found", REASON_FONTSIZE, REASON_MAX_LINES);
        assert!(lines[0].starts_with("moov atom not found"));
    }

    #[test]
    fn wrap_to_width_hard_splits_a_single_word_longer_than_one_line() {
        let max_chars = max_chars_per_line(REASON_FONTSIZE);
        let one_giant_word = "a".repeat(max_chars * 2 + 5);
        let lines = wrap_to_width(&one_giant_word, REASON_FONTSIZE, REASON_MAX_LINES);
        for line in &lines {
            assert!(line.len() <= max_chars);
        }
        assert!(lines.len() > 1);
    }

    #[test]
    fn wrap_to_width_marks_truncation_past_max_lines() {
        let max_chars = max_chars_per_line(REASON_FONTSIZE);
        // Enough distinct words to overflow REASON_MAX_LINES rows.
        let words: Vec<String> = (0..(max_chars * (REASON_MAX_LINES + 2)))
            .map(|i| format!("w{i}"))
            .collect();
        let reason = words.join(" ");
        let lines = wrap_to_width(&reason, REASON_FONTSIZE, REASON_MAX_LINES);
        assert_eq!(lines.len(), REASON_MAX_LINES);
        assert!(lines.last().unwrap().ends_with("..."));
    }

    #[test]
    fn card_params_keeps_reason_lines_comma_joined_for_the_filter_graph() {
        let params = card_params("PLAYBACK ERROR", "Some Title", "moov atom not found");
        assert!(params.starts_with(&format!(
            "color=c=black:s={CARD_WIDTH_PX}x{CARD_HEIGHT_PX}:r=30,"
        )));
        assert!(params.contains("[out0]; anullsrc=channel_layout=stereo:sample_rate=48000 [out1]"));
        assert!(params.contains("moov atom not found"));
    }
}
