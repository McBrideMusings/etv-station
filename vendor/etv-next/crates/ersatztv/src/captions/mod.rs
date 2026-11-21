//! CEA-608 closed captions for the HDHomeRun TS stream, made from the
//! session's WebVTT segments.
//!
//! Plex Live TV reads `/stream/<N>.ts` and shows captions only when they ride
//! inside the H.264 as A/53 SEI; it lists but never draws a DVB subtitle
//! stream. HLS clients keep the WebVTT rendition, which nothing here changes.

mod cea608;
mod ts;

use std::collections::VecDeque;

use cea608::{PADDING, Pair};
pub use ts::Rewriter;

/// PTS ticks per second.
const HZ: i64 = 90_000;

/// PTS wraps at 2^33.
const PTS_WRAP: i64 = 1 << 33;

/// A cue that continues one the previous segment's file cut at its end comes
/// back with the same text and a start this close to that end.
const CONTINUATION_TOLERANCE: i64 = HZ / 4;

/// Field-1 pairs 608 carries per second: one per frame at 29.97 fps.
const PAIRS_PER_SECOND: f64 = 30_000.0 / 1_001.0;

/// The most pairs one frame carries, so a low frame rate keeps 608's rate.
const MAX_PAIRS_PER_FRAME: usize = 3;

/// One caption on the stream's PTS timeline (90 kHz, unwrapped).
#[derive(Clone, Debug, PartialEq)]
struct Cue {
    start: i64,
    end: i64,
    text: String,
}

/// Turns cues into the 608 pairs each frame carries, in display order.
///
/// Each cue is loaded into hidden memory as soon as the previous one has been
/// shown, shown at its start and cleared at its end unless the next cue
/// replaces it then. Pairs leave at 608's rate, so a load that cannot finish
/// before its cue's start shows it late rather than garbled.
#[derive(Default)]
pub struct Captioner {
    upcoming: VecDeque<Cue>,
    /// Loaded into hidden memory; its End Of Caption not yet queued.
    loaded: Option<Cue>,
    /// Shown (or queued to show); the end is when it must clear.
    shown: Option<Cue>,
    queue: VecDeque<Pair>,
    /// The pair the previous frame sent last.
    last_sent: Option<Pair>,
    /// Pairs the frame rate has earned but not spent.
    credit: f64,
    last_pts: Option<i64>,
    /// The stream left captioning: send what clears the screen, then nothing.
    closing: bool,
}

impl Captioner {
    /// Add the cues of one WebVTT segment file. A cue the previous file cut
    /// at its end extends that cue instead of starting another.
    pub fn add_vtt(&mut self, body: &str) {
        let reference = self.last_pts;
        for cue in parse_vtt(body) {
            let start = unwrap(cue.start, reference.unwrap_or(cue.start));
            let cue = Cue {
                start,
                end: start + (cue.end - cue.start),
                text: cue.text,
            };
            if let Some(last) = self.last_cue_mut()
                && last.text == cue.text
                && (cue.start - last.end).abs() <= CONTINUATION_TOLERANCE
            {
                last.end = last.end.max(cue.end);
                continue;
            }
            self.upcoming.push_back(cue);
        }
    }

    fn last_cue_mut(&mut self) -> Option<&mut Cue> {
        if let Some(last) = self.upcoming.back_mut() {
            return Some(last);
        }
        self.loaded.as_mut().or(self.shown.as_mut())
    }

    /// Stop captioning: drop every cue not yet shown and clear the screen if
    /// a caption is on it. Afterwards [`Captioner::frame`] carries only that
    /// clear, then nothing.
    pub fn close(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.upcoming.clear();
        self.loaded = None;
        self.queue.clear();
        if self.shown.take().is_some() {
            self.queue.extend(cea608::erase());
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closing
    }

    /// The pairs for the frame shown at `pts` (33-bit). Frames must arrive in
    /// display order. While captioning, always at least one pair, padding when
    /// idle, so a decoder probing the stream sees the caption channel from its
    /// start; once closed and the clear is out, `None`: no caption data.
    pub fn frame(&mut self, pts: u64) -> Option<Vec<Pair>> {
        let reference = self.last_pts.unwrap_or(pts as i64);
        let pts = unwrap(pts as i64, reference);
        let elapsed = self.last_pts.map_or(3003, |last| (pts - last).max(0));
        self.last_pts = Some(pts);

        if !self.closing {
            self.schedule(pts);
        }

        self.credit = (self.credit + elapsed as f64 * PAIRS_PER_SECOND / HZ as f64)
            .min(MAX_PAIRS_PER_FRAME as f64);
        let budget = (self.credit + 1e-6).floor() as usize;
        let take = budget.min(self.queue.len());
        let out: Vec<Pair> = self.queue.drain(..take).collect();
        self.credit -= out.len() as f64;
        if self.queue.is_empty() {
            // Idle time earns no burst later.
            self.credit = 0.0;
        }
        if let Some(last) = out.last() {
            self.last_sent = Some(*last);
        }
        if out.is_empty() {
            return (!self.closing).then(|| vec![PADDING]);
        }
        Some(out)
    }

    fn schedule(&mut self, pts: i64) {
        loop {
            if self.loaded.is_none() {
                while self.upcoming.front().is_some_and(|c| c.end <= pts) {
                    self.upcoming.pop_front();
                }
                if let Some(next) = self.upcoming.pop_front() {
                    self.queue.extend(cea608::load(&next.text));
                    self.loaded = Some(next);
                }
            }
            match &self.loaded {
                Some(cue) if cue.start <= pts => {
                    self.queue.extend(cea608::display());
                    self.shown = self.loaded.take();
                }
                _ => break,
            }
        }
        if self.shown.as_ref().is_some_and(|c| c.end <= pts) {
            self.shown = None;
            if self.queue.contains(&EOC_PAIR) {
                // Its End Of Caption has not gone out yet: clear after it.
                self.queue.extend(cea608::erase());
            } else {
                // Clearing is the time-critical one, and it leaves hidden
                // memory alone, so it goes ahead of a load in progress: after
                // the second half of a doubled code the last frame began.
                let at = usize::from(
                    self.last_sent
                        .is_some_and(|last| self.queue.front() == Some(&last)),
                );
                for (i, p) in cea608::erase().into_iter().enumerate() {
                    self.queue.insert(at + i, p);
                }
            }
        }
    }
}

/// The parity-coded End Of Caption pair, to tell when the shown cue is out.
const EOC_PAIR: Pair = [0x94, 0x2F];

/// `pts` moved by whole wraps to land nearest `reference`.
fn unwrap(pts: i64, reference: i64) -> i64 {
    let mut pts = pts.rem_euclid(PTS_WRAP) + (reference.div_euclid(PTS_WRAP)) * PTS_WRAP;
    if pts - reference > PTS_WRAP / 2 {
        pts -= PTS_WRAP;
    } else if reference - pts > PTS_WRAP / 2 {
        pts += PTS_WRAP;
    }
    pts
}

/// The cues of one WebVTT segment, timed in 33-bit PTS by its
/// `X-TIMESTAMP-MAP`, text stripped of markup. A file without the map has
/// no timeline to place cues on and yields none.
fn parse_vtt(body: &str) -> Vec<Cue> {
    let Some(map) = body
        .lines()
        .find_map(|line| line.trim().strip_prefix("X-TIMESTAMP-MAP="))
    else {
        return Vec::new();
    };
    let mut local = None;
    let mut mpegts = None;
    for field in map.split(',') {
        if let Some(v) = field.trim().strip_prefix("LOCAL:") {
            local = vtt_time(v);
        } else if let Some(v) = field.trim().strip_prefix("MPEGTS:") {
            mpegts = v.trim().parse::<i64>().ok();
        }
    }
    let (Some(local), Some(mpegts)) = (local, mpegts) else {
        return Vec::new();
    };
    let at = |ms: i64| mpegts + (ms - local) * HZ / 1000;

    let mut cues = Vec::new();
    let mut lines = body.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((start, rest)) = line.split_once("-->") else {
            continue;
        };
        let end = rest.split_whitespace().next().unwrap_or("");
        let (Some(start), Some(end)) = (vtt_time(start.trim()), vtt_time(end)) else {
            continue;
        };
        let mut text = Vec::new();
        while let Some(next) = lines.next_if(|l| !l.trim().is_empty()) {
            text.push(clean_text(next));
        }
        let text = text.join("\n");
        if end > start && !text.trim().is_empty() {
            cues.push(Cue {
                start: at(start),
                end: at(end),
                text,
            });
        }
    }
    cues
}

/// `[hh:]mm:ss.mmm` in milliseconds.
fn vtt_time(s: &str) -> Option<i64> {
    let (clock, ms) = s.split_once('.')?;
    let ms: i64 = ms.parse().ok()?;
    let mut seconds = 0i64;
    for part in clock.split(':') {
        seconds = seconds * 60 + part.parse::<i64>().ok()?;
    }
    Some(seconds * 1000 + ms)
}

/// One cue line without its tags (`<i>`, `<c.x>`, `<00:01.000>`), entities
/// decoded.
fn clean_text(line: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in line.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&lrm;", "")
        .replace("&rlm;", "")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: u64 = 3003;

    fn vtt(mpegts: u64, cues: &str) -> String {
        format!("WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:{mpegts}\n\n{cues}")
    }

    /// Feed frames `from..to` (frame numbers) and return the non-padding
    /// pairs with the frame each went out on.
    fn run(c: &mut Captioner, base: u64, frames: std::ops::Range<u64>) -> Vec<(u64, Pair)> {
        let mut out = Vec::new();
        for f in frames {
            for p in c.frame(base + f * FRAME).unwrap() {
                if p != PADDING {
                    out.push((f, p));
                }
            }
        }
        out
    }

    #[test]
    fn parse_vtt_times_cues_by_the_timestamp_map_and_strips_markup() {
        let body = vtt(
            900_000,
            "00:00:01.500 --> 00:00:03.000\n<i>Hello</i> &amp; bye\nline two\n\n1\n00:04.000 --> 00:05.000 align:start\nx\n\n",
        );
        assert_eq!(
            parse_vtt(&body),
            vec![
                Cue {
                    start: 900_000 + 135_000,
                    end: 900_000 + 270_000,
                    text: "Hello & bye\nline two".into()
                },
                Cue {
                    start: 900_000 + 360_000,
                    end: 900_000 + 450_000,
                    text: "x".into()
                },
            ]
        );
        assert!(parse_vtt("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nx\n").is_empty());
        assert!(parse_vtt(&vtt(0, "")).is_empty());
    }

    #[test]
    fn a_cue_loads_before_its_start_shows_at_it_and_clears_at_its_end() {
        let base = 1_000_000;
        let mut c = Captioner::default();
        // Cue from frame 60 to frame 120.
        c.add_vtt(&vtt(base, "00:00:02.002 --> 00:00:04.004\nHI\n\n"));
        let sent = run(&mut c, base, 0..200);
        let eoc = sent.iter().find(|(_, p)| *p == EOC_PAIR).unwrap().0;
        let edm = sent.iter().find(|(_, p)| *p == [0x94, 0x2C]).unwrap().0;
        assert_eq!(eoc, 60, "{sent:?}");
        assert_eq!(edm, 120, "{sent:?}");
        // The load (9 pairs) went out at one pair a frame from frame 0.
        assert_eq!(sent.first().unwrap().0, 0);
        assert!(sent.iter().filter(|(f, _)| *f < 60).count() == 9);
    }

    #[test]
    fn back_to_back_cues_swap_without_a_clear_between() {
        let base = 0;
        let mut c = Captioner::default();
        c.add_vtt(&vtt(
            base,
            "00:00:01.001 --> 00:00:03.003\nA\n\n00:00:03.003 --> 00:00:05.005\nB\n\n",
        ));
        let sent = run(&mut c, base, 0..200);
        let clears: Vec<u64> = sent
            .iter()
            .filter(|(_, p)| *p == [0x94, 0x2C])
            .map(|(f, _)| *f)
            .collect();
        assert_eq!(clears, vec![150, 151], "only B's end clears: {sent:?}");
    }

    #[test]
    fn a_cue_split_across_two_segment_files_is_one_caption() {
        let mut c = Captioner::default();
        let seg = 4 * HZ as u64;
        c.add_vtt(&vtt(0, "00:00:03.000 --> 00:00:04.000\nSPLIT\n\n"));
        c.add_vtt(&vtt(seg, "00:00:00.000 --> 00:00:01.000\nSPLIT\n\n"));
        assert_eq!(c.upcoming.len(), 1);
        assert_eq!(c.upcoming[0].end, 5 * HZ);
    }

    #[test]
    fn frames_must_come_in_display_order_and_pts_wraps() {
        assert_eq!(unwrap(10, PTS_WRAP - 10), PTS_WRAP + 10);
        assert_eq!(unwrap(PTS_WRAP - 10, PTS_WRAP + 10), PTS_WRAP - 10);
        assert_eq!(unwrap(5, 7), 5);
        // A cue across the wrap still shows on time.
        let base = PTS_WRAP as u64 - 30 * FRAME;
        let mut c = Captioner::default();
        c.frame(base);
        c.add_vtt(&vtt(base, "00:00:02.002 --> 00:00:03.003\nW\n\n"));
        let mut eoc_frame = None;
        for f in 1..120u64 {
            let pts = (base + f * FRAME) % PTS_WRAP as u64;
            if c.frame(pts).unwrap().contains(&EOC_PAIR) {
                eoc_frame.get_or_insert(f);
            }
        }
        assert_eq!(eoc_frame, Some(60));
    }

    #[test]
    fn an_idle_frame_carries_one_padding_pair() {
        let mut c = Captioner::default();
        assert_eq!(c.frame(0), Some(vec![PADDING]));
        assert_eq!(c.frame(FRAME), Some(vec![PADDING]));
    }

    #[test]
    fn closing_clears_a_shown_caption_then_sends_nothing() {
        let mut c = Captioner::default();
        c.add_vtt(&vtt(
            0,
            "00:00:00.000 --> 00:00:10.000\nSTUCK\n\n00:00:20.000 --> 00:00:21.000\nLATER\n\n",
        ));
        let sent = run(&mut c, 0, 0..30);
        assert!(sent.iter().any(|(_, p)| *p == EOC_PAIR));
        c.close();
        let edm = [0x94, 0x2C];
        assert_eq!(c.frame(30 * FRAME), Some(vec![edm]));
        assert_eq!(c.frame(31 * FRAME), Some(vec![edm]));
        assert_eq!(c.frame(32 * FRAME), None);
        assert_eq!(c.frame(700 * FRAME), None, "LATER was dropped");

        let mut idle = Captioner::default();
        idle.close();
        assert_eq!(idle.frame(0), None);
    }
}
