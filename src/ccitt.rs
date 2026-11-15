//! Strict CCITT Group 4 (T.6) decoder with `/EncodedByteAlign` support.
//!
//! The `fax` crate's `Group4Decoder` reads one continuous bitstream and
//! hides its bit position, so it cannot skip the fill bits a byte-aligned
//! stream puts before each coding line. This decoder does the 2D coding
//! itself (reference-line changing elements, pass / horizontal / vertical
//! modes, per ITU-T T.4 §4.2 and T.6) over its own bit reader, which knows
//! its exact position. Only the code tables come from `fax`
//! (`fax::maps::{mode, white, black}`, read through `fax`'s public
//! `BitReader` trait).
//!
//! The decoder is deliberately stricter than a viewer's, because its output
//! replaces the original: anything a conforming encoder never writes makes
//! it return `None`, and the caller leaves the image untouched. Rejected:
//! - nonzero fill bits (the PDF spec calls them "extra 0 bits");
//! - an uncompressed-mode (extension) code, or an EOL / EOFB code inside a
//!   coding line;
//! - a coding step that does not move a0 strictly right (zero-length runs
//!   are only valid as the white run opening a line, or as the second run
//!   when the first one already reached the line end); a pass mode with no
//!   real b2; a vertical a1 outside the line;
//! - fewer or more coding lines than the declared height;
//! - anything but an EOFB right after the last line (byte-aligned when the
//!   stream is), and, for byte-aligned streams, any byte after the EOFB.
//!
//! Untrusted input is bounded: every coding step moves a0 right, so a line
//! takes at most `width + 1` steps; each byte-aligned line consumes at least
//! one byte, so a stream declaring more lines than it has bytes is rejected
//! before the sample buffer is allocated; makeup-code chains are capped.

use std::convert::Infallible;

use fax::maps::{black, mode, white, Mode};
use fax::BitReader;

/// EOFB: two consecutive EOL codes (`000000000001` twice).
const EOFB: [u8; 3] = [0x00, 0x10, 0x01];

/// A bit reader over a byte slice that knows its exact position. Reads past
/// the end see zero bits; the decoder checks the position against the data
/// length after every coding line, so a code that ran off the end is
/// rejected rather than decoded from padding.
struct SliceBits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl SliceBits<'_> {
    fn bit_len(&self) -> usize {
        self.data.len() * 8
    }

    fn overrun(&self) -> bool {
        self.pos > self.bit_len()
    }

    /// Skip to the next byte boundary; the skipped fill bits must be zero.
    fn align(&mut self) -> Option<()> {
        let fill = ((8 - self.pos % 8) % 8) as u8;
        if fill > 0 {
            if self.peek(fill)? != 0 {
                return None;
            }
            self.pos += usize::from(fill);
        }
        Some(())
    }
}

impl BitReader for SliceBits<'_> {
    type Error = Infallible;

    fn peek(&self, bits: u8) -> Option<u16> {
        if bits > 16 {
            return None;
        }
        if bits == 0 {
            return Some(0);
        }
        // The (up to) 16 bits at `pos` live in the three bytes from
        // `pos / 8`; bytes past the end read as zero.
        let byte = self.pos / 8;
        let get = |k: usize| u32::from(self.data.get(byte + k).copied().unwrap_or(0));
        let window = get(0) << 16 | get(1) << 8 | get(2);
        let shift = 24 - self.pos % 8 - usize::from(bits);
        Some(((window >> shift) & ((1u32 << bits) - 1)) as u16)
    }

    fn consume(&mut self, bits: u8) -> Result<(), Infallible> {
        self.pos += usize::from(bits);
        Ok(())
    }

    fn bits_to_byte_boundary(&self) -> u8 {
        ((8 - self.pos % 8) % 8) as u8
    }
}

/// One run length in `white` or `black`: makeup codes (64 and up) followed
/// by one terminating code (0..=63). A line holds at most `width` pels, so a
/// chain longer than `width / 64 + 2` codes (2560 is the largest makeup) is
/// already invalid; the cap keeps a hostile chain from running long.
fn run_length(reader: &mut SliceBits<'_>, white_run: bool, width: u32) -> Option<u32> {
    let max_codes = width / 64 + 2;
    let mut sum: u32 = 0;
    for _ in 0..max_codes {
        let n = if white_run {
            white::decode(reader)?
        } else {
            black::decode(reader)?
        };
        sum = sum.checked_add(u32::from(n))?;
        if n < 64 {
            return Some(sum);
        }
    }
    None
}

/// Decode one coding line against `reference` (its changing elements,
/// strictly increasing, all `< width`), writing this line's changing
/// elements into `current`.
fn decode_line(
    reader: &mut SliceBits<'_>,
    reference: &[u32],
    current: &mut Vec<u32>,
    width: u32,
) -> Option<()> {
    current.clear();
    let w = i64::from(width);
    // a0 starts on the imaginary white element just before the line.
    let mut a0: i64 = -1;
    let mut white_run = true;
    // First reference element strictly right of a0; a0 only moves right,
    // so this pointer only moves forward.
    let mut p = 0usize;
    loop {
        let m = mode::decode(reader)?;
        while p < reference.len() && i64::from(reference[p]) <= a0 {
            p += 1;
        }
        // b1 = first element right of a0 of the colour opposite to a0's:
        // elements alternate, even indices are white->black changes.
        let i = if p < reference.len() && p.is_multiple_of(2) != white_run {
            p + 1
        } else {
            p
        };
        let elem = |k: usize| reference.get(k).map_or(w, |&v| i64::from(v));
        let (b1, b2) = (elem(i), elem(i + 1));
        match m {
            Mode::Pass => {
                if b2 >= w {
                    return None;
                }
                a0 = b2;
            }
            Mode::Vertical(delta) => {
                let a1 = b1 + i64::from(delta);
                if a1 <= a0 || a1 > w {
                    return None;
                }
                if a1 < w {
                    current.push(a1 as u32);
                }
                a0 = a1;
                white_run = !white_run;
            }
            Mode::Horizontal => {
                let r1 = i64::from(run_length(reader, white_run, width)?);
                let r2 = i64::from(run_length(reader, !white_run, width)?);
                let start = a0.max(0);
                let a1 = start + r1;
                let a2 = a1 + r2;
                // A zero first run only opens a line (black first pel); a
                // zero second run only follows a first run that hit the end.
                if (r1 == 0 && a0 >= 0) || (r2 == 0 && a1 < w) || a2 > w {
                    return None;
                }
                if a1 < w {
                    current.push(a1 as u32);
                }
                if a2 < w {
                    current.push(a2 as u32);
                }
                a0 = a2;
            }
            Mode::Extension | Mode::EOF => return None,
        }
        if a0 >= w {
            return Some(());
        }
    }
}

/// Write one line's pels into a pre-zeroed packed sample row. Sample value
/// = `(pel is black) == black_is_1` (black pels are 0 under the default
/// `/BlackIs1 false`); padding bits past `width` stay zero.
fn pack_line(row: &mut [u8], changes: &[u32], width: u32, black_is_1: bool) {
    // Runs alternate white, black, white, ... between changing elements.
    let mut start = 0u32;
    let mut black = false;
    for &end in changes.iter().chain(std::iter::once(&width)) {
        if black == black_is_1 {
            set_bits(row, start as usize, end as usize);
        }
        start = end;
        black = !black;
    }
}

/// Set bits `[start, end)` of a packed MSB-first row.
fn set_bits(row: &mut [u8], start: usize, end: usize) {
    let mut x = start;
    while x < end && !x.is_multiple_of(8) {
        row[x / 8] |= 0x80 >> (x % 8);
        x += 1;
    }
    while x + 8 <= end {
        row[x / 8] = 0xFF;
        x += 8;
    }
    while x < end {
        row[x / 8] |= 0x80 >> (x % 8);
        x += 1;
    }
}

/// Decode a G4 stream to packed 1-bit sample rows (`ceil(width/8)` bytes
/// per row, padding bits zero), or `None` for anything outside the strict
/// contract in the module docs. With `byte_align` the stream is read as
/// `/EncodedByteAlign true`: each coding line and the EOFB start on a byte
/// boundary and no byte may follow the EOFB. Without it (used by the
/// differential fuzz target against `fax`), the EOFB follows the last line
/// directly and bytes after it are ignored, as the existing strict path
/// does. The caller bounds `width * height` (the bitonal pixel cap).
pub(crate) fn decode_g4(
    data: &[u8],
    width: u32,
    height: usize,
    black_is_1: bool,
    byte_align: bool,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    // Every byte-aligned line occupies at least one byte, and the EOFB
    // three more; a longer declared height cannot be honest. Checked before
    // the sample buffer is allocated.
    if byte_align && height > data.len().saturating_sub(EOFB.len()) {
        return None;
    }
    let stride = (width as usize).div_ceil(8);
    let mut out = vec![0u8; stride.checked_mul(height)?];
    let mut reader = SliceBits { data, pos: 0 };
    let mut reference: Vec<u32> = Vec::new();
    let mut current: Vec<u32> = Vec::new();
    for row in out.chunks_exact_mut(stride) {
        if byte_align {
            reader.align()?;
        }
        decode_line(&mut reader, &reference, &mut current, width)?;
        if reader.overrun() {
            return None;
        }
        pack_line(row, &current, width, black_is_1);
        std::mem::swap(&mut reference, &mut current);
    }
    if byte_align {
        reader.align()?;
        let at = reader.pos / 8;
        return (data.get(at..) == Some(&EOFB[..])).then_some(out);
    }
    let eol = |r: &SliceBits<'_>, off: usize| {
        let probe = SliceBits {
            data: r.data,
            pos: r.pos + off,
        };
        probe.peek(12) == Some(1)
    };
    (reader.pos + 24 <= reader.bit_len() && eol(&reader, 0) && eol(&reader, 12)).then_some(out)
}

/// Byte-aligned G4 encoder for the tests and the fuzz harness (never used
/// by the pipeline, which only decodes byte-aligned streams): fax's own G4
/// encoder writing through a bit writer that zero-fills to a byte boundary
/// before each line after the first and before the EOFB.
#[cfg(any(test, feature = "fuzzing"))]
mod aligned_encoder {
    use super::Infallible;
    use fax::encoder::Encoder;
    use fax::{BitWriter, Bits, Color};
    use std::cell::Cell;
    use std::rc::Rc;

    struct AlignWriter {
        bits: Vec<bool>,
        pad: Rc<Cell<bool>>,
        /// Bit positions of every fill bit written.
        fill: Vec<usize>,
    }
    impl BitWriter for AlignWriter {
        type Error = Infallible;
        fn write(&mut self, b: Bits) -> Result<(), Infallible> {
            if self.pad.replace(false) {
                while !self.bits.len().is_multiple_of(8) {
                    self.fill.push(self.bits.len());
                    self.bits.push(false);
                }
            }
            for i in (0..b.len).rev() {
                self.bits.push((b.data >> i) & 1 == 1);
            }
            Ok(())
        }
    }

    /// Encode packed sample rows (`/BlackIs1 false`: sample 0 = black) as
    /// byte-aligned G4. Returns the stream and the bit positions of all
    /// its fill bits.
    pub(crate) fn encode(samples: &[u8], width: u32, height: usize) -> (Vec<u8>, Vec<usize>) {
        let stride = (width as usize).div_ceil(8);
        let pad = Rc::new(Cell::new(false));
        let mut enc = Encoder::new(AlignWriter {
            bits: Vec::new(),
            pad: pad.clone(),
            fill: Vec::new(),
        });
        for row in samples.chunks_exact(stride).take(height) {
            let pels = (0..width as usize).map(|x| {
                if row[x / 8] >> (7 - x % 8) & 1 == 0 {
                    Color::Black
                } else {
                    Color::White
                }
            });
            let Ok(()) = enc.encode_line(pels, width);
            pad.set(true);
        }
        let Ok(w) = enc.finish();
        // finish() wrote the EOFB after a pad, so it ends on a byte.
        debug_assert!(w.bits.len().is_multiple_of(8));
        let bytes = w
            .bits
            .chunks(8)
            .map(|c| {
                c.iter()
                    .enumerate()
                    .fold(0u8, |acc, (i, &b)| acc | (u8::from(b) << (7 - i)))
            })
            .collect();
        (bytes, w.fill)
    }
}

#[cfg(feature = "fuzzing")]
pub(crate) use aligned_encoder::encode as encode_g4_aligned;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use fax::encoder::Encoder;
    use fax::{Color, VecWriter};

    fn to_bytes(bits: &[bool]) -> Vec<u8> {
        bits.chunks(8)
            .map(|c| {
                c.iter()
                    .enumerate()
                    .fold(0u8, |acc, (i, &b)| acc | (u8::from(b) << (7 - i)))
            })
            .collect()
    }

    /// Encode ink rows (true = black) as G4, byte-aligned or not.
    pub(crate) fn g4_encode(rows: &[Vec<bool>], aligned: bool) -> Vec<u8> {
        let width = rows[0].len() as u32;
        if !aligned {
            let mut enc = Encoder::new(VecWriter::new());
            for r in rows {
                let pels = r
                    .iter()
                    .map(|&b| if b { Color::Black } else { Color::White });
                enc.encode_line(pels, width).unwrap();
            }
            return enc.finish().unwrap().finish();
        }
        g4_encode_aligned(rows).0
    }

    /// Byte-aligned G4 (fill before every line and before the EOFB), plus
    /// the bit positions of all fill bits.
    pub(crate) fn g4_encode_aligned(rows: &[Vec<bool>]) -> (Vec<u8>, Vec<usize>) {
        let (data, fill) =
            aligned_encoder::encode(&samples(rows, false), rows[0].len() as u32, rows.len());
        assert!(data.ends_with(&EOFB));
        (data, fill)
    }

    fn samples(rows: &[Vec<bool>], black_is_1: bool) -> Vec<u8> {
        let stride = rows[0].len().div_ceil(8);
        let mut out = vec![0u8; stride * rows.len()];
        for (y, r) in rows.iter().enumerate() {
            for (x, &b) in r.iter().enumerate() {
                if b == black_is_1 {
                    out[y * stride + x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        out
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Text-like rows plus edge cases: black first/last pels, full black
    /// rows, isolated pels, long runs (makeup codes).
    fn rows(width: usize, height: usize, seed: u64) -> Vec<Vec<bool>> {
        let mut rng = Rng(seed);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| match y % 7 {
                        0 => false,
                        1 => true,
                        2 => x == 0 || x == width - 1,
                        3 => rng.next().is_multiple_of(3),
                        4 => (x / 5).is_multiple_of(2),
                        _ => (x * 7 + y * 3) % 11 < 4 && !rng.next().is_multiple_of(5),
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn aligned_round_trip_matches_samples() {
        for &(w, h) in &[
            (1, 1),
            (7, 9),
            (8, 8),
            (9, 30),
            (64, 21),
            (281, 60),
            (2600, 14),
        ] {
            let rs = rows(w, h, (w * 31 + h) as u64);
            let data = g4_encode(&rs, true);
            for bi1 in [false, true] {
                assert_eq!(
                    decode_g4(&data, w as u32, h, bi1, true),
                    Some(samples(&rs, bi1)),
                    "{w}x{h} BlackIs1 {bi1}"
                );
            }
            // Unless every line happens to end on a byte boundary, the
            // fill bits make this a different stream from the continuous
            // one, which the unaligned reading must not mistake for it.
            if g4_encode(&rs, false) != data {
                assert_ne!(
                    decode_g4(&data, w as u32, h, false, false),
                    Some(samples(&rs, false))
                );
            }
        }
    }

    #[test]
    fn unaligned_mode_agrees_with_fax_on_random_images() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for case in 0..200 {
            let w = 1 + (rng.next() % 300) as usize;
            let h = 1 + (rng.next() % 40) as usize;
            let rs = rows(w, h, rng.next());
            let data = g4_encode(&rs, false);
            let want = samples(&rs, false);
            assert_eq!(
                decode_g4(&data, w as u32, h, false, false),
                Some(want),
                "case {case}: {w}x{h}"
            );
        }
    }

    #[test]
    fn nonzero_fill_bits_are_rejected() {
        for &(w, h, seed) in &[(13usize, 10usize, 7u64), (281, 40, 9), (3, 25, 1)] {
            let rs = rows(w, h, seed);
            let (mut data, fills) = g4_encode_aligned(&rs);
            assert!(!fills.is_empty());
            assert!(decode_g4(&data, w as u32, h, false, true).is_some());
            // Every single fill bit set to 1 must reject the whole stream.
            for &bit in &fills {
                data[bit / 8] ^= 0x80 >> (bit % 8);
                assert_eq!(
                    decode_g4(&data, w as u32, h, false, true),
                    None,
                    "{w}x{h}: fill bit {bit} set"
                );
                data[bit / 8] ^= 0x80 >> (bit % 8);
            }
        }
    }

    #[test]
    fn framing_violations_are_rejected() {
        let (w, h) = (40usize, 12usize);
        let rs = rows(w, h, 3);
        let data = g4_encode(&rs, true);
        let wu = w as u32;
        assert!(decode_g4(&data, wu, h, false, true).is_some());
        // Wrong height either way.
        assert!(decode_g4(&data, wu, h - 1, false, true).is_none());
        assert!(decode_g4(&data, wu, h + 1, false, true).is_none());
        // Trailing byte after the EOFB, even a zero.
        let mut t = data.clone();
        t.push(0);
        assert!(decode_g4(&t, wu, h, false, true).is_none());
        // Truncated EOFB, or no EOFB.
        assert!(decode_g4(&data[..data.len() - 1], wu, h, false, true).is_none());
        assert!(decode_g4(&data[..data.len() - 3], wu, h, false, true).is_none());
        // Empty input, zero geometry, more lines than bytes.
        assert!(decode_g4(&[], wu, h, false, true).is_none());
        assert!(decode_g4(&data, 0, h, false, true).is_none());
        assert!(decode_g4(&data, wu, 0, false, true).is_none());
        assert!(decode_g4(&data, wu, data.len(), false, true).is_none());
    }

    #[test]
    fn unaligned_eofb_in_an_aligned_stream_is_rejected() {
        // One white 5-pel line (V0 = '1') then an EOFB straight after it,
        // without the fill bits: 1 000000000001 000000000001 + pad.
        let mut bits = vec![true];
        for _ in 0..2 {
            bits.extend(std::iter::repeat_n(false, 11));
            bits.push(true);
        }
        let data = to_bytes(&bits);
        assert_eq!(decode_g4(&data, 5, 1, false, false), Some(vec![0xF8]));
        assert!(decode_g4(&data, 5, 1, false, true).is_none());
    }

    #[test]
    fn extension_and_eol_codes_inside_a_line_are_rejected() {
        // Extension mode 0000001 + 3 bits as the first code of line 1.
        let mut bits = vec![
            false, false, false, false, false, false, true, false, false, false,
        ];
        while !bits.len().is_multiple_of(8) {
            bits.push(false);
        }
        bits.extend(
            EOFB.iter()
                .flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1 == 1)),
        );
        assert!(decode_g4(&to_bytes(&bits), 8, 1, false, true).is_none());
        // An EOFB where line 1 should be.
        assert!(decode_g4(&[0x00, 0x10, 0x01, 0x00, 0x10, 0x01], 8, 1, false, true).is_none());
    }

    #[test]
    fn hostile_inputs_stay_bounded() {
        // Huge declared geometry on a few bytes: rejected before allocating.
        assert!(decode_g4(&[0x80; 8], 1 << 20, 1 << 20, false, true).is_none());
        // Long all-zero and all-one inputs.
        assert!(decode_g4(&vec![0u8; 4096], 1000, 1000, false, true).is_none());
        assert!(decode_g4(&vec![0xFF; 4096], 1000, 1000, false, true).is_none());
        // A blank wide image decodes: one V0 byte per line.
        let mut blank = vec![0x80u8; 500];
        blank.extend_from_slice(&EOFB);
        let out = decode_g4(&blank, 30_000, 500, false, true).unwrap();
        assert!(out.chunks(3750).all(|r| r.iter().all(|&b| b == 0xFF)));
    }

    #[test]
    fn pack_line_matches_fax_pels() {
        let mut rng = Rng(42);
        for _ in 0..500 {
            let width = 1 + (rng.next() % 70) as u32;
            let mut changes: Vec<u32> = (0..width)
                .filter(|_| rng.next().is_multiple_of(4))
                .collect();
            changes.dedup();
            for bi1 in [false, true] {
                let stride = (width as usize).div_ceil(8);
                let mut a = vec![0u8; stride];
                pack_line(&mut a, &changes, width, bi1);
                let mut b = vec![0u8; stride];
                for (x, c) in fax::decoder::pels(&changes, width).enumerate() {
                    if (c == Color::Black) == bi1 {
                        b[x / 8] |= 0x80 >> (x % 8);
                    }
                }
                assert_eq!(a, b, "width {width} changes {changes:?}");
            }
        }
    }
}
