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

/// Rewrite `item` in place into a black card that says, on screen, which file
/// failed and what ffmpeg said about it — sized to exactly the slot the real
/// item would have filled, so nothing downstream shifts.
///
/// The item keeps its `program` metadata, so the guide still lists the film that
/// was meant to air. A viewer who tunes in sees the title they expected and the
/// reason it is not playing, rather than a channel that went quiet.
pub fn make_error_card(item: &mut ResolvedItem, path: &Path, reason: &str, slot: Duration) {
    let font =
        std::env::var("ETV_STATION_ERROR_FONT").unwrap_or_else(|_| DEFAULT_ERROR_FONT.to_string());

    let title = item
        .program
        .as_ref()
        .and_then(|p| p.title.clone())
        .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| item.id.clone());

    let line = |text: &str, size: u32, y: u32| {
        format!(
            "drawtext=fontfile={font}:text={}:fontcolor=white:fontsize={size}:x=60:y={y}",
            drawtext_safe(text)
        )
    };

    // Three stacked lines on black, plus silence. The filter-graph shape — two
    // labelled outputs — is the same one an authored `lavfi` item uses, so
    // nothing further down needs to know this item is special.
    let params = format!(
        "color=c=black:s=1280x720:r=30,{},{},{} [out0]; \
         anullsrc=channel_layout=stereo:sample_rate=48000 [out1]",
        line("PLAYBACK ERROR", 52, 60),
        line(&title, 30, 150),
        line(reason, 24, 210),
    );

    item.source = SourceConfig::Lavfi { params };
    item.in_point = Some(Duration::ZERO);
    item.out_point = Some(slot);
    item.error_card = true;
}

/// Reduce arbitrary text to something an ffmpeg filter-graph argument can carry
/// literally.
///
/// ffmpeg's filter syntax gives meaning to `:` `,` `;` `[` `]` `'` `\` and `%`,
/// and the escaping rules differ by nesting depth — which is exactly the sort of
/// thing that turns a helpful error message into a broken filter graph and a
/// dead channel. Rather than escape, this drops the whole problem: anything not
/// plainly printable becomes a space. "moov atom not found" survives intact,
/// which is the part worth reading.
///
/// The whitelist is ASCII-only, so the length cut can never land inside a
/// multi-byte character.
fn drawtext_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(120));
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
        if out.len() >= 110 {
            break;
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drawtext_safe_strips_what_would_break_the_filter_graph() {
        // ffmpeg gives meaning to : , ; [ ] ' \ % — none may survive.
        let out = drawtext_safe("moov atom not found: /media/a'b[c],d;e\\f%g");
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
    fn drawtext_safe_never_yields_an_empty_argument() {
        // An all-punctuation reason would otherwise produce `text=`, which is a
        // filter-graph parse error and would take the channel down — the exact
        // failure this whole path exists to prevent.
        assert_eq!(drawtext_safe(":::"), "unknown");
        assert_eq!(drawtext_safe(""), "unknown");
    }

    #[test]
    fn drawtext_safe_cuts_long_text_without_splitting_a_character() {
        let out = drawtext_safe(&"a".repeat(500));
        assert!(out.len() <= 110);
        assert!(out.is_char_boundary(out.len()));
    }
}
