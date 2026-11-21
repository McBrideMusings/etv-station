//! CEA-608 caption channel 1 (CC1, field 1) byte pairs for pop-on captions.
//!
//! A pop-on caption is loaded into the decoder's hidden (non-displayed)
//! memory, then shown all at once by End Of Caption, which swaps hidden and
//! displayed memory. Every control code, special character and extended
//! character is sent twice, as 608 requires; a decoder drops the repeat.

/// One 608 byte pair, odd parity already applied.
pub type Pair = [u8; 2];

/// The pair a frame carries when nothing is waiting: two parity-coded nulls.
pub const PADDING: Pair = [0x80, 0x80];

/// Columns on one caption row.
pub const ROW_CHARS: usize = 32;

/// Rows one caption may use; a cue with more lines keeps its first four.
pub const MAX_ROWS: usize = 4;

/// Resume Caption Loading: chars that follow go to hidden memory (pop-on).
const RCL: [u8; 2] = [0x14, 0x20];
/// Erase Non-displayed Memory.
const ENM: [u8; 2] = [0x14, 0x2E];
/// End Of Caption: swap hidden and displayed memory.
const EOC: [u8; 2] = [0x14, 0x2F];
/// Erase Displayed Memory.
const EDM: [u8; 2] = [0x14, 0x2C];

/// `byte` with bit 7 set or cleared so the byte has odd parity.
pub fn parity(byte: u8) -> u8 {
    let byte = byte & 0x7F;
    if byte.count_ones().is_multiple_of(2) {
        byte | 0x80
    } else {
        byte
    }
}

fn pair(hi: u8, lo: u8) -> Pair {
    [parity(hi), parity(lo)]
}

fn doubled(code: [u8; 2]) -> [Pair; 2] {
    let p = pair(code[0], code[1]);
    [p, p]
}

/// The pairs that load `text` into hidden memory, bottom-aligned and centred.
pub fn load(text: &str) -> Vec<Pair> {
    let mut out = Vec::new();
    out.extend(doubled(RCL));
    out.extend(doubled(ENM));
    let rows = layout(text);
    let first_row = 16 - rows.len();
    for (i, glyphs) in rows.iter().enumerate() {
        let column = (ROW_CHARS - glyphs.len()) / 2;
        out.extend(doubled(preamble(first_row + i, column - column % 4)));
        if !column.is_multiple_of(4) {
            // Tab Offset 1-3 moves the cursor past the PAC's 4-column indent.
            out.extend(doubled([0x17, 0x20 + (column % 4) as u8]));
        }
        encode_row(glyphs, &mut out);
    }
    out
}

/// The pairs that show the loaded caption.
pub fn display() -> Vec<Pair> {
    doubled(EOC).to_vec()
}

/// The pairs that clear the shown caption.
pub fn erase() -> Vec<Pair> {
    doubled(EDM).to_vec()
}

/// Preamble Address Code: put the cursor on `row` (1-15) at `indent` (a
/// multiple of 4), white, no underline.
fn preamble(row: usize, indent: usize) -> [u8; 2] {
    const ROWS: [(u8, u8); 15] = [
        (0x11, 0x40),
        (0x11, 0x60),
        (0x12, 0x40),
        (0x12, 0x60),
        (0x15, 0x40),
        (0x15, 0x60),
        (0x16, 0x40),
        (0x16, 0x60),
        (0x17, 0x40),
        (0x17, 0x60),
        (0x10, 0x40),
        (0x13, 0x40),
        (0x13, 0x60),
        (0x14, 0x40),
        (0x14, 0x60),
    ];
    let (hi, base) = ROWS[row - 1];
    [hi, base + 0x10 + (indent / 4) as u8 * 2]
}

/// How one character goes over the wire.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Glyph {
    /// A basic character: one byte, two to a pair.
    Basic(u8),
    /// A special character (`0x11 lo`), its own doubled pair.
    Special(u8),
    /// An extended character (`hi lo`, hi `0x12` or `0x13`). The decoder
    /// shows `fallback` first and the extended pair overwrites it.
    Extended { fallback: u8, hi: u8, lo: u8 },
}

fn encode_row(glyphs: &[Glyph], out: &mut Vec<Pair>) {
    fn push_basic(b: u8, out: &mut Vec<Pair>, pending: &mut Option<u8>) {
        match pending.take() {
            Some(first) => out.push(pair(first, b)),
            None => *pending = Some(b),
        }
    }
    let mut pending: Option<u8> = None;
    for glyph in glyphs {
        match *glyph {
            Glyph::Basic(b) => push_basic(b, out, &mut pending),
            Glyph::Special(lo) => {
                if let Some(first) = pending.take() {
                    out.push(pair(first, 0));
                }
                out.extend(doubled([0x11, lo]));
            }
            Glyph::Extended { fallback, hi, lo } => {
                push_basic(fallback, out, &mut pending);
                if let Some(first) = pending.take() {
                    out.push(pair(first, 0));
                }
                out.extend(doubled([hi, lo]));
            }
        }
    }
    if let Some(first) = pending {
        out.push(pair(first, 0));
    }
}

/// `text` split into at most [`MAX_ROWS`] rows of at most [`ROW_CHARS`]
/// glyphs, wrapping long lines at spaces. Characters 608 cannot show are
/// dropped.
fn layout(text: &str) -> Vec<Vec<Glyph>> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let mut row: Vec<Glyph> = Vec::new();
        for word in line.split_whitespace() {
            let word: Vec<Glyph> = word.chars().flat_map(glyphs).collect();
            if word.is_empty() {
                continue;
            }
            if !row.is_empty() && row.len() + 1 + word.len() > ROW_CHARS {
                rows.push(std::mem::take(&mut row));
            }
            if !row.is_empty() {
                row.push(Glyph::Basic(b' '));
            }
            for chunk in word.chunks(ROW_CHARS) {
                if row.len() + chunk.len() > ROW_CHARS {
                    rows.push(std::mem::take(&mut row));
                }
                row.extend_from_slice(chunk);
            }
        }
        if !row.is_empty() {
            rows.push(row);
        }
    }
    rows.truncate(MAX_ROWS);
    rows
}

/// The glyphs that show `c`: usually one, none for a character 608 lacks,
/// several for one spelled out (`…`).
fn glyphs(c: char) -> Vec<Glyph> {
    let basic = |b| vec![Glyph::Basic(b)];
    let ext = |fallback: char, hi, lo| {
        vec![Glyph::Extended {
            fallback: fallback as u8,
            hi,
            lo,
        }]
    };
    match c {
        // ASCII minus the nine codes 608's basic set gives other characters.
        ' '..='~' if !"*\\^_`{|}~".contains(c) => basic(c as u8),
        'á' => basic(0x2A),
        'é' => basic(0x5C),
        'í' => basic(0x5E),
        'ó' => basic(0x5F),
        'ú' => basic(0x60),
        'ç' => basic(0x7B),
        '÷' => basic(0x7C),
        'Ñ' => basic(0x7D),
        'ñ' => basic(0x7E),
        '█' => basic(0x7F),
        '®' => vec![Glyph::Special(0x30)],
        '°' => vec![Glyph::Special(0x31)],
        '½' => vec![Glyph::Special(0x32)],
        '¿' => vec![Glyph::Special(0x33)],
        '™' => vec![Glyph::Special(0x34)],
        '¢' => vec![Glyph::Special(0x35)],
        '£' => vec![Glyph::Special(0x36)],
        '♪' | '♫' => vec![Glyph::Special(0x37)],
        'à' => vec![Glyph::Special(0x38)],
        'è' => vec![Glyph::Special(0x3A)],
        'â' => vec![Glyph::Special(0x3B)],
        'ê' => vec![Glyph::Special(0x3C)],
        'î' => vec![Glyph::Special(0x3D)],
        'ô' => vec![Glyph::Special(0x3E)],
        'û' => vec![Glyph::Special(0x3F)],
        'Á' => ext('A', 0x12, 0x20),
        'É' => ext('E', 0x12, 0x21),
        'Ó' => ext('O', 0x12, 0x22),
        'Ú' => ext('U', 0x12, 0x23),
        'Ü' => ext('U', 0x12, 0x24),
        'ü' => ext('u', 0x12, 0x25),
        '¡' => ext('!', 0x12, 0x27),
        '*' => ext(' ', 0x12, 0x28),
        '©' => ext('c', 0x12, 0x2B),
        'À' => ext('A', 0x12, 0x30),
        'Â' => ext('A', 0x12, 0x31),
        'Ç' => ext('C', 0x12, 0x32),
        'È' => ext('E', 0x12, 0x33),
        'Ê' => ext('E', 0x12, 0x34),
        'Ë' => ext('E', 0x12, 0x35),
        'ë' => ext('e', 0x12, 0x36),
        'Î' => ext('I', 0x12, 0x37),
        'Ï' => ext('I', 0x12, 0x38),
        'ï' => ext('i', 0x12, 0x39),
        'Ô' => ext('O', 0x12, 0x3A),
        'Ù' => ext('U', 0x12, 0x3B),
        'ù' => ext('u', 0x12, 0x3C),
        'Û' => ext('U', 0x12, 0x3D),
        '«' => ext('"', 0x12, 0x3E),
        '»' => ext('"', 0x12, 0x3F),
        'Ã' => ext('A', 0x13, 0x20),
        'ã' => ext('a', 0x13, 0x21),
        'Í' => ext('I', 0x13, 0x22),
        'Ì' => ext('I', 0x13, 0x23),
        'ì' => ext('i', 0x13, 0x24),
        'Ò' => ext('O', 0x13, 0x25),
        'ò' => ext('o', 0x13, 0x26),
        'Õ' => ext('O', 0x13, 0x27),
        'õ' => ext('o', 0x13, 0x28),
        '{' => ext('(', 0x13, 0x29),
        '}' => ext(')', 0x13, 0x2A),
        '\\' => ext('/', 0x13, 0x2B),
        '^' => ext(' ', 0x13, 0x2C),
        '_' => ext('-', 0x13, 0x2D),
        '|' => ext(' ', 0x13, 0x2E),
        '~' => ext('-', 0x13, 0x2F),
        'Ä' => ext('A', 0x13, 0x30),
        'ä' => ext('a', 0x13, 0x31),
        'Ö' => ext('O', 0x13, 0x32),
        'ö' => ext('o', 0x13, 0x33),
        'ß' => ext('s', 0x13, 0x34),
        'Å' => ext('A', 0x13, 0x38),
        'å' => ext('a', 0x13, 0x39),
        'Ø' => ext('O', 0x13, 0x3A),
        'ø' => ext('o', 0x13, 0x3B),
        '‘' | '’' | '`' | '´' => basic(b'\''),
        '“' | '”' => basic(b'"'),
        '–' | '—' | '‐' => basic(b'-'),
        '…' => vec![Glyph::Basic(b'.'); 3],
        '\u{a0}' => basic(b' '),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(pairs: &[Pair]) -> Vec<[u8; 2]> {
        pairs.iter().map(|p| [p[0] & 0x7F, p[1] & 0x7F]).collect()
    }

    #[test]
    fn every_byte_has_odd_parity() {
        for pair in load("Héllo, Wörld ♪ {x} ½") {
            for byte in pair {
                assert_eq!(byte.count_ones() % 2, 1, "{byte:#04x}");
            }
        }
        assert_eq!(parity(0x14), 0x94);
        assert_eq!(parity(0x20), 0x20);
        assert_eq!(parity(0x00), 0x80);
    }

    #[test]
    fn a_one_line_cue_loads_centred_on_the_bottom_row() {
        // "HI" is 2 wide: column 15 = indent 12 + tab offset 3, row 15.
        let pairs = strip(&load("HI"));
        assert_eq!(
            pairs,
            vec![
                [0x14, 0x20],
                [0x14, 0x20],
                [0x14, 0x2E],
                [0x14, 0x2E],
                [0x14, 0x76],
                [0x14, 0x76],
                [0x17, 0x23],
                [0x17, 0x23],
                *b"HI",
            ]
        );
        assert_eq!(strip(&display()), vec![[0x14, 0x2F]; 2]);
        assert_eq!(strip(&erase()), vec![[0x14, 0x2C]; 2]);
    }

    #[test]
    fn two_lines_take_rows_14_and_15_and_an_odd_char_is_padded() {
        let pairs = strip(&load("abc\nd"));
        // Row 14 PAC hi is 0x14 base 0x40; row 15 base 0x60.
        assert!(pairs.contains(&[0x14, 0x56]), "{pairs:x?}");
        assert!(pairs.contains(&[b'c', 0x00]), "{pairs:x?}");
        assert_eq!(*pairs.last().unwrap(), [b'd', 0x00]);
    }

    #[test]
    fn characters_outside_ascii_use_the_608_tables() {
        let pairs = strip(&load("é♪*"));
        let body = &pairs[pairs.len() - 6..];
        assert_eq!(
            body,
            &[
                // é is basic 0x5C, flushed with a null before the special.
                [0x5C, 0x00],
                [0x11, 0x37],
                [0x11, 0x37],
                // * is extended: a space fallback, then 0x12 0x28 twice.
                [b' ', 0x00],
                [0x12, 0x28],
                [0x12, 0x28],
            ]
        );
    }

    #[test]
    fn long_lines_wrap_at_spaces_and_cues_keep_four_rows() {
        let rows = layout("the quick brown fox jumps over the lazy dog again and again");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.len() <= ROW_CHARS));
        assert_eq!(layout("a\nb\nc\nd\ne").len(), MAX_ROWS);
        assert!(layout("  \n").is_empty());
    }
}
