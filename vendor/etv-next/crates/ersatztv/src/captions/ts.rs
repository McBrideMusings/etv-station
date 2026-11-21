//! Insert 608 pairs into an MPEG-TS segment's H.264 as A/53 SEI.
//!
//! Each video PES (one access unit) gets an SEI NAL
//! (`user_data_registered_itu_t_t35`, `GA94` `cc_data`) just before its first
//! slice, then is cut back into TS packets in the slots the original used, so
//! PCR and random-access flags stay where they were and the extra bytes go in
//! packets right after. Every other PID passes through untouched.

use super::Captioner;
use super::cea608::Pair;

const PACKET: usize = 188;
const SYNC: u8 = 0x47;
const STREAM_TYPE_H264: u8 = 0x1B;

/// Rewrites a stream's segments in order, keeping what spans them: the PIDs
/// the PMT named and the video PID's continuity counter.
#[derive(Default)]
pub struct Rewriter {
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    /// The continuity counter the next video packet with payload carries.
    next_cc: Option<u8>,
}

/// One packet's parsed header.
struct Header {
    pusi: bool,
    pid: u16,
    has_payload: bool,
    /// Adaptation field bytes after its length byte, stuffing removed.
    adaptation: Option<Vec<u8>>,
    payload_at: usize,
}

fn header(p: &[u8]) -> Option<Header> {
    if p.len() != PACKET || p[0] != SYNC {
        return None;
    }
    let pusi = p[1] & 0x40 != 0;
    let pid = u16::from(p[1] & 0x1F) << 8 | u16::from(p[2]);
    let control = (p[3] >> 4) & 0x3;
    let has_payload = control & 0x1 != 0;
    let mut payload_at = 4;
    let mut adaptation = None;
    if control & 0x2 != 0 {
        let len = usize::from(p[4]);
        if 5 + len > PACKET {
            return None;
        }
        payload_at = 5 + len;
        adaptation = Some(adaptation_fields(&p[5..5 + len]));
    }
    Some(Header {
        pusi,
        pid,
        has_payload,
        adaptation,
        payload_at,
    })
}

/// An adaptation field's flags and the optional fields they announce, with
/// trailing stuffing cut.
fn adaptation_fields(af: &[u8]) -> Vec<u8> {
    let Some(&flags) = af.first() else {
        return Vec::new();
    };
    let mut len = 1;
    if flags & 0x10 != 0 {
        len += 6; // PCR
    }
    if flags & 0x08 != 0 {
        len += 6; // OPCR
    }
    if flags & 0x04 != 0 {
        len += 1; // splice countdown
    }
    if flags & 0x02 != 0 {
        len += 1 + af.get(len).map_or(0, |&n| usize::from(n)); // private data
    }
    if flags & 0x01 != 0 {
        len += 1 + af.get(len).map_or(0, |&n| usize::from(n)); // extension
    }
    af[..len.min(af.len())].to_vec()
}

/// One video PES: the packets that carry it and its bytes.
struct Pes {
    slots: Vec<usize>,
    bytes: Vec<u8>,
}

impl Rewriter {
    /// `segment` with `captioner`'s pairs in every H.264 access unit. A
    /// segment that is not whole 188-byte packets, or has no H.264 PID yet,
    /// comes back unchanged.
    pub fn rewrite(&mut self, segment: &[u8], captioner: &mut Captioner) -> Vec<u8> {
        if !segment.len().is_multiple_of(PACKET) {
            return segment.to_vec();
        }
        let packets: Vec<&[u8]> = segment.chunks(PACKET).collect();
        let Some(headers) = packets
            .iter()
            .map(|p| header(p))
            .collect::<Option<Vec<Header>>>()
        else {
            return segment.to_vec();
        };

        for (p, h) in packets.iter().zip(&headers) {
            if h.pusi && h.pid == 0 {
                self.pmt_pid = pat_pmt_pid(&p[h.payload_at..]);
            } else if h.pusi && Some(h.pid) == self.pmt_pid {
                self.video_pid = pmt_h264_pid(&p[h.payload_at..]);
            }
        }
        let Some(video) = self.video_pid else {
            return segment.to_vec();
        };

        // Group the video packets into PES, each starting at a PUSI.
        let mut pes: Vec<Pes> = Vec::new();
        let mut owner = vec![None; packets.len()];
        for (i, h) in headers.iter().enumerate() {
            if h.pid != video || !h.has_payload {
                continue;
            }
            if h.pusi {
                pes.push(Pes {
                    slots: Vec::new(),
                    bytes: Vec::new(),
                });
            }
            if let Some(current) = pes.last_mut() {
                current.slots.push(i);
                current.bytes.extend_from_slice(&packets[i][h.payload_at..]);
                owner[i] = Some(pes.len() - 1);
            }
        }

        // 608 pairs follow display order, not the decode order PES come in.
        let mut order: Vec<(u64, usize)> = pes
            .iter()
            .enumerate()
            .filter_map(|(i, p)| pes_pts(&p.bytes).map(|pts| (pts, i)))
            .collect();
        let reference = order.first().map_or(0, |&(pts, _)| pts as i64);
        order.sort_by_key(|&(pts, _)| super::unwrap(pts as i64, reference));
        let mut pairs: Vec<Option<Vec<Pair>>> = vec![None; pes.len()];
        for (pts, i) in order {
            pairs[i] = captioner.frame(pts);
        }

        // Cut each PES, captioned or not, back into its slots.
        let mut cut: Vec<std::collections::VecDeque<Vec<u8>>> = pes
            .iter()
            .zip(&pairs)
            .map(|(p, pairs)| {
                let bytes = match pairs {
                    Some(pairs) => with_sei(&p.bytes, pairs),
                    None => p.bytes.clone(),
                };
                repacketize(&p.slots, &headers, &bytes)
            })
            .collect();

        let mut out = Vec::with_capacity(segment.len() + pes.len() * PACKET);
        for (i, (p, h)) in packets.iter().zip(&headers).enumerate() {
            if h.pid != video {
                out.extend_from_slice(p);
                continue;
            }
            if !h.has_payload {
                let mut p = p.to_vec();
                let cc = self
                    .next_cc
                    .map_or(p[3] & 0x0F, |n| n.wrapping_sub(1) & 0x0F);
                p[3] = (p[3] & 0xF0) | cc;
                out.extend_from_slice(&p);
                continue;
            }
            let mut emit = |mut p: Vec<u8>| {
                let cc = *self.next_cc.get_or_insert(p[3] & 0x0F);
                p[3] = (p[3] & 0xF0) | cc;
                self.next_cc = Some((cc + 1) & 0x0F);
                out.extend_from_slice(&p);
            };
            match owner[i] {
                None => emit(p.to_vec()),
                Some(n) => {
                    let last = pes[n].slots.last() == Some(&i);
                    let queue = &mut cut[n];
                    if let Some(packet) = queue.pop_front() {
                        emit(packet);
                    }
                    if last {
                        while let Some(packet) = queue.pop_front() {
                            emit(packet);
                        }
                    }
                }
            }
        }
        out
    }
}

/// The PMT PID a PAT section names first.
fn pat_pmt_pid(payload: &[u8]) -> Option<u16> {
    let section = payload.get(1 + usize::from(*payload.first()?)..)?;
    let length = (usize::from(section.get(1)? & 0x0F) << 8 | usize::from(*section.get(2)?))
        .min(section.len().saturating_sub(3));
    // Program loop after the 8-byte header, before the 4-byte CRC.
    let programs = section.get(8..(3 + length).saturating_sub(4))?;
    programs.chunks_exact(4).find_map(|entry| {
        let number = u16::from(entry[0]) << 8 | u16::from(entry[1]);
        (number != 0).then(|| u16::from(entry[2] & 0x1F) << 8 | u16::from(entry[3]))
    })
}

/// The PID of the first H.264 stream a PMT section lists.
fn pmt_h264_pid(payload: &[u8]) -> Option<u16> {
    let section = payload.get(1 + usize::from(*payload.first()?)..)?;
    let length = (usize::from(section.get(1)? & 0x0F) << 8 | usize::from(*section.get(2)?))
        .min(section.len().saturating_sub(3));
    let end = (3 + length).saturating_sub(4);
    let info_length = usize::from(section.get(10)? & 0x0F) << 8 | usize::from(*section.get(11)?);
    let mut at = 12 + info_length;
    while at + 5 <= end {
        let stream_type = section[at];
        let pid = u16::from(section[at + 1] & 0x1F) << 8 | u16::from(section[at + 2]);
        if stream_type == STREAM_TYPE_H264 {
            return Some(pid);
        }
        at += 5 + (usize::from(section[at + 3] & 0x0F) << 8 | usize::from(section[at + 4]));
    }
    None
}

/// A PES's 33-bit PTS, when its header has one.
fn pes_pts(pes: &[u8]) -> Option<u64> {
    if pes.len() < 14 || pes[..3] != [0, 0, 1] || pes[7] & 0x80 == 0 {
        return None;
    }
    let b = &pes[9..14];
    Some(
        (u64::from(b[0] >> 1) & 0x07) << 30
            | u64::from(b[1]) << 22
            | (u64::from(b[2]) >> 1) << 15
            | u64::from(b[3]) << 7
            | u64::from(b[4]) >> 1,
    )
}

/// `pes` with an SEI NAL carrying `pairs` before its first slice. A PES with
/// no slice is returned unchanged.
pub(super) fn with_sei(pes: &[u8], pairs: &[Pair]) -> Vec<u8> {
    if pes.len() < 9 || pes[..3] != [0, 0, 1] {
        return pes.to_vec();
    }
    let es_at = 9 + usize::from(pes[8]);
    let Some(slice_at) = first_slice(pes, es_at) else {
        return pes.to_vec();
    };
    let sei = sei_nal(pairs);
    let mut out = Vec::with_capacity(pes.len() + sei.len());
    out.extend_from_slice(&pes[..slice_at]);
    out.extend_from_slice(&sei);
    out.extend_from_slice(&pes[slice_at..]);
    let length = usize::from(pes[4]) << 8 | usize::from(pes[5]);
    if length != 0 {
        // A video PES may say 0 (unbounded) when it outgrows 16 bits.
        let length = length + sei.len();
        let length = if length > 0xFFFF { 0 } else { length };
        out[4] = (length >> 8) as u8;
        out[5] = length as u8;
    }
    out
}

/// Where the start code of the first slice NAL (types 1-5) begins, counting
/// a leading zero byte of a four-byte start code.
fn first_slice(pes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 < pes.len() {
        if pes[i] == 0 && pes[i + 1] == 0 && pes[i + 2] == 1 {
            let nal_type = pes[i + 3] & 0x1F;
            if (1..=5).contains(&nal_type) {
                return Some(if i > from && pes[i - 1] == 0 {
                    i - 1
                } else {
                    i
                });
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    None
}

/// An Annex B SEI NAL with one `user_data_registered_itu_t_t35` message
/// holding ATSC A/53 `cc_data` for field-1 608 `pairs`.
pub(super) fn sei_nal(pairs: &[Pair]) -> Vec<u8> {
    let count = pairs.len().min(31);
    let mut payload = vec![
        0xB5, // itu_t_t35_country_code: United States
        0x00,
        0x31, // itu_t_t35_provider_code: ATSC
        b'G',
        b'A',
        b'9',
        b'4',               // user_identifier
        0x03,               // user_data_type_code: cc_data
        0xC0 | count as u8, // process_cc_data_flag, cc_count
        0xFF,               // em_data
    ];
    for pair in &pairs[..count] {
        // marker bits, cc_valid, cc_type 0 (NTSC field 1)
        payload.extend_from_slice(&[0xFC, pair[0], pair[1]]);
    }
    payload.push(0xFF); // marker_bits

    let mut rbsp = vec![0x04]; // payloadType 4
    let mut size = payload.len();
    while size >= 255 {
        rbsp.push(0xFF);
        size -= 255;
    }
    rbsp.push(size as u8);
    rbsp.extend_from_slice(&payload);
    rbsp.push(0x80); // rbsp_trailing_bits

    let mut nal = vec![0x00, 0x00, 0x01, 0x06];
    let mut zeros = 0;
    for byte in rbsp {
        if zeros >= 2 && byte <= 0x03 {
            nal.push(0x03);
            zeros = 0;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        nal.push(byte);
    }
    nal
}

/// `bytes` cut into TS packets: one per original slot, each keeping its
/// header and adaptation fields, then as many more as the extra bytes need.
/// The last packet is padded with adaptation-field stuffing.
fn repacketize(
    slots: &[usize],
    headers: &[Header],
    bytes: &[u8],
) -> std::collections::VecDeque<Vec<u8>> {
    let mut out = std::collections::VecDeque::new();
    let Some(&first) = slots.first() else {
        return out;
    };
    let pid = headers[first].pid;
    let mut rest = bytes;
    let mut slot = 0;
    while !rest.is_empty() {
        let (pusi, adaptation) = match slots.get(slot) {
            Some(&i) => (slot == 0 && headers[i].pusi, headers[i].adaptation.clone()),
            None => (false, None),
        };
        slot += 1;
        let af_len = adaptation.as_ref().map_or(0, |a| 1 + a.len());
        let room = PACKET - 4 - af_len;
        let take = rest.len().min(room);
        let stuffing = room - take;

        let mut p = Vec::with_capacity(PACKET);
        p.push(SYNC);
        p.push(if pusi { 0x40 } else { 0 } | (pid >> 8) as u8 & 0x1F);
        p.push(pid as u8);
        let has_af = adaptation.is_some() || stuffing > 0;
        p.push(if has_af { 0x30 } else { 0x10 });
        if has_af {
            let total = af_len + stuffing; // with its length byte
            let mut af = adaptation.unwrap_or_default();
            if af.is_empty() && total >= 2 {
                af.push(0x00); // flags
            }
            let fill = total - 1 - af.len();
            af.extend(std::iter::repeat_n(0xFF, fill));
            p.push(af.len() as u8);
            p.extend_from_slice(&af);
        }
        p.extend_from_slice(&rest[..take]);
        debug_assert_eq!(p.len(), PACKET);
        rest = &rest[take..];
        out.push_back(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sei_is_a53_cc_data_with_emulation_prevention() {
        let nal = sei_nal(&[[0x80, 0x80], [0x94, 0x2F]]);
        assert_eq!(
            nal,
            vec![
                0x00, 0x00, 0x01, 0x06, 0x04, 17, 0xB5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03,
                0xC2, 0xFF, 0xFC, 0x80, 0x80, 0xFC, 0x94, 0x2F, 0xFF, 0x80,
            ]
        );
        // 00 00 followed by a byte <= 3 gets an 0x03 between.
        let mut zeros = 0;
        for &b in &nal[4..] {
            assert!(!(zeros >= 2 && b <= 0x03), "{nal:x?}");
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
    }

    /// A PES with header length `hl`, an AUD, SPS and a slice.
    fn pes_with_slice() -> Vec<u8> {
        let mut pes = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x80, 0x05];
        pes.extend_from_slice(&[0x21, 0x00, 0x01, 0x00, 0x01]); // PTS 0
        pes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x09, 0xF0]); // AUD
        pes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x42]); // SPS
        pes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84]); // IDR slice
        pes
    }

    #[test]
    fn the_sei_goes_right_before_the_first_slice() {
        let pes = pes_with_slice();
        let out = with_sei(&pes, &[[0x80, 0x80]]);
        let sei = sei_nal(&[[0x80, 0x80]]);
        let slice_at = pes.len() - 7;
        assert_eq!(&out[..slice_at], &pes[..slice_at]);
        assert_eq!(&out[slice_at..slice_at + sei.len()], &sei[..]);
        assert_eq!(&out[slice_at + sei.len()..], &pes[slice_at..]);
        // No slice: nothing to anchor to, unchanged.
        let no_slice = &pes[..pes.len() - 7];
        assert_eq!(with_sei(no_slice, &[[0x80, 0x80]]), no_slice);
    }

    #[test]
    fn a_bounded_pes_length_grows_by_the_sei() {
        let mut pes = pes_with_slice();
        let len = pes.len() - 6;
        pes[4] = (len >> 8) as u8;
        pes[5] = len as u8;
        let out = with_sei(&pes, &[[0x80, 0x80]]);
        assert_eq!(
            usize::from(out[4]) << 8 | usize::from(out[5]),
            out.len() - 6
        );
    }

    #[test]
    fn pes_pts_reads_the_33_bit_field() {
        let mut pes = pes_with_slice();
        // PTS 0x1_2345_6789 encoded per ISO 13818-1.
        let pts: u64 = 0x1_2345_6789;
        pes[9] = 0x21 | (((pts >> 30) & 0x07) << 1) as u8;
        pes[10] = (pts >> 22) as u8;
        pes[11] = (((pts >> 15) & 0x7F) << 1) as u8 | 1;
        pes[12] = (pts >> 7) as u8;
        pes[13] = ((pts & 0x7F) << 1) as u8 | 1;
        assert_eq!(pes_pts(&pes), Some(pts));
    }

    /// One PES as TS packets on `pid`, the first carrying a PCR when asked,
    /// the last padded with adaptation-field stuffing.
    fn packetize(pid: u16, cc: &mut u8, pes: &[u8], pcr: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let mut rest = pes;
        let mut first = true;
        while !rest.is_empty() {
            let mut af: Vec<u8> = if first && pcr {
                vec![0x10, 0, 0, 0x12, 0x34, 0x7E, 0]
            } else {
                Vec::new()
            };
            let af_total = if af.is_empty() { 0 } else { 1 + af.len() };
            let take = rest.len().min(184 - af_total);
            let pad = 184 - af_total - take;
            if af.is_empty() && pad >= 2 {
                af.push(0x00);
                af.extend(std::iter::repeat_n(0xFF, pad - 2));
            } else if !af.is_empty() {
                af.extend(std::iter::repeat_n(0xFF, pad));
            }
            let has_af = af_total > 0 || pad > 0;
            out.push(SYNC);
            out.push(if first { 0x40 } else { 0 } | (pid >> 8) as u8);
            out.push(pid as u8);
            out.push(if has_af { 0x30 } else { 0x10 } | *cc);
            if has_af {
                out.push(af.len() as u8);
                out.extend_from_slice(&af);
            }
            out.extend_from_slice(&rest[..take]);
            assert_eq!(out.len() % PACKET, 0);
            rest = &rest[take..];
            *cc = (*cc + 1) & 0x0F;
            first = false;
        }
        out
    }

    fn psi(pid: u16, section: &[u8]) -> Vec<u8> {
        let mut p = vec![SYNC, 0x40 | (pid >> 8) as u8, pid as u8, 0x10, 0x00];
        p.extend_from_slice(section);
        p.resize(PACKET, 0xFF);
        p
    }

    fn video_pes(pts: u64, slice_len: usize) -> Vec<u8> {
        let mut pes = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x80, 0x05];
        pes.extend_from_slice(&[
            0x21 | (((pts >> 30) & 0x07) << 1) as u8,
            (pts >> 22) as u8,
            (((pts >> 15) & 0x7F) << 1) as u8 | 1,
            (pts >> 7) as u8,
            ((pts & 0x7F) << 1) as u8 | 1,
        ]);
        pes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x09, 0xF0]); // AUD
        pes.extend_from_slice(&[0x00, 0x00, 0x01, 0x41]); // non-IDR slice
        pes.extend(std::iter::repeat_n(0x88, slice_len));
        pes
    }

    /// PAT, PMT (H.264 on 0x101, AAC on 0x102), then video PES in decode
    /// order with B-frame PTS (3003, 12012, 6006, 9009) and an audio packet.
    /// The first PES fills its two packets exactly, so its SEI needs a third.
    fn segment() -> Vec<u8> {
        let mut ts = Vec::new();
        ts.extend(psi(
            0,
            &[
                0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xE1, 0x00, 0, 0, 0, 0,
            ],
        ));
        ts.extend(psi(
            0x100,
            &[
                0x02, 0xB0, 0x17, 0x00, 0x01, 0xC1, 0x00, 0x00, 0xE1, 0x01, 0xF0, 0x00, 0x1B, 0xE1,
                0x01, 0xF0, 0x00, 0x0F, 0xE1, 0x02, 0xF0, 0x00, 0, 0, 0, 0,
            ],
        ));
        let mut cc = 0;
        let big = video_pes(3003, 176 + 184 - 24);
        assert_eq!(big.len(), 176 + 184);
        ts.extend(packetize(0x101, &mut cc, &big, true));
        let mut audio_cc = 0;
        ts.extend(packetize(
            0x102,
            &mut audio_cc,
            &[0, 0, 1, 0xC0, 0, 0, 0x80, 0, 0, 0xAA],
            false,
        ));
        for pts in [12012, 6006, 9009] {
            ts.extend(packetize(0x101, &mut cc, &video_pes(pts, 20), false));
        }
        ts
    }

    /// The first 608 pair of each video PES's SEI, in the order the PES come,
    /// with every video packet's continuity counter.
    fn read_back(ts: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut pes: Vec<Vec<u8>> = Vec::new();
        let mut ccs = Vec::new();
        for p in ts.chunks(PACKET) {
            let h = header(p).unwrap();
            if h.pid != 0x101 {
                continue;
            }
            ccs.push(p[3] & 0x0F);
            if h.pusi {
                pes.push(Vec::new());
            }
            pes.last_mut()
                .unwrap()
                .extend_from_slice(&p[h.payload_at..]);
        }
        let firsts = pes
            .iter()
            .map(|b| {
                let at = b.windows(4).position(|w| w == b"GA94").unwrap();
                assert_eq!(b[at + 4], 0x03);
                b[at + 8] & 0x7F
            })
            .collect();
        (firsts, ccs)
    }

    #[test]
    fn captions_go_in_display_order_and_the_packets_stay_valid() {
        let input = segment();
        let mut captioner = Captioner::default();
        // A cue far ahead: its load (RCL RCL ENM ENM ...) starts on frame one.
        captioner.add_vtt("WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:0\n\n00:01:40.000 --> 00:01:41.000\nX\n");
        let out = Rewriter::default().rewrite(&input, &mut captioner);

        assert_eq!(out.len() % PACKET, 0);
        assert!(out.chunks(PACKET).all(|p| p[0] == SYNC));
        // One packet more than the input: the first PES overflowed.
        assert_eq!(out.len(), input.len() + PACKET);

        let (firsts, ccs) = read_back(&out);
        // Display order 3003 RCL, 6006 RCL, 9009 ENM, 12012 ENM; decode order
        // is 3003, 12012, 6006, 9009.
        assert_eq!(firsts, vec![0x14, 0x14, 0x14, 0x14]);
        let video: Vec<u8> = out
            .chunks(PACKET)
            .filter_map(|p| {
                header(p)
                    .filter(|h| h.pid == 0x101)
                    .map(|h| p[h.payload_at..].to_vec())
            })
            .flatten()
            .collect();
        let mut seconds = Vec::new();
        let mut at = 0;
        while let Some(i) = video[at..].windows(4).position(|w| w == b"GA94") {
            seconds.push(video[at + i + 9] & 0x7F);
            at += i + 4;
        }
        assert_eq!(seconds, vec![0x20, 0x2E, 0x20, 0x2E], "PES in decode order");
        assert_eq!(ccs, (0..ccs.len() as u8).collect::<Vec<_>>());

        // PCR and the PSI and audio packets come through untouched.
        let first_video = out.chunks(PACKET).nth(2).unwrap();
        assert_eq!(&first_video[4..12], &input[2 * PACKET + 4..2 * PACKET + 12]);
        assert_eq!(&out[..2 * PACKET], &input[..2 * PACKET]);
        assert!(
            out.chunks(PACKET)
                .any(|p| p == &input[4 * PACKET..5 * PACKET])
        );
    }

    #[test]
    fn a_closed_idle_captioner_leaves_the_segment_unchanged() {
        let input = segment();
        let mut captioner = Captioner::default();
        captioner.close();
        assert_eq!(Rewriter::default().rewrite(&input, &mut captioner), input);
    }
}
