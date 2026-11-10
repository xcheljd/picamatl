//! Lossless JBIG2 generic-region coding: an MQ arithmetic encoder and a
//! matching decoder, plus the PDF embedded-stream framing (ITU-T T.88 /
//! ISO/IEC 14492; ISO 32000-1 §7.4.7).
//!
//! Scope is deliberately one shape: a single immediate generic region
//! (segment type 38, arithmetic coded, `MMR = 0`) covering the whole image,
//! preceded by a page-information segment. No symbol dictionaries, no
//! refinement, no `/JBIG2Globals`. Implemented from the standard: the MQ
//! coder is T.88 Annex E (the same coder JPEG 2000 adopted), the generic
//! region decoding procedure is T.88 §6.2.5.7 (fixed templates of Figures
//! 3-6, adaptive-template pixels, typical prediction `TPGDON`).
//!
//! Bitmap polarity is JBIG2's: a 1 bit is a black pixel. The PDF
//! `/JBIG2Decode` filter outputs the inverse (1 = black ⇒ sample 0, the
//! DeviceGray black), so a PDF sample row maps onto a JBIG2 bitmap row by
//! bitwise NOT — see [`samples_to_bitmap`].
//!
//! Untrusted-input posture: the decoder only ever produces the declared
//! `width × height` region, allocated once up front by the caller's
//! already-capped dimensions; reading past the end of the data yields
//! `0xFF` bytes (the T.88 marker convention), so it always terminates after
//! exactly `width × height` decisions. Nothing here panics on hostile data.

/// Pixel ceiling for one region, the same 2^28 as the bitonal pass's
/// `MAX_BITONAL_PIXELS`: the decoder allocates `ceil(w / 8) × h` bytes up
/// front (at most 32 MiB), and refuses anything larger before allocating.
pub(crate) const MAX_PIXELS: u64 = 1 << 28;

/// One row of T.88 Table E.1: `(Qe, NMPS, NLPS, SWITCH)`.
const QE_TABLE: [(u32, u8, u8, bool); 47] = [
    (0x5601, 1, 1, true),
    (0x3401, 2, 6, false),
    (0x1801, 3, 9, false),
    (0x0AC1, 4, 12, false),
    (0x0521, 5, 29, false),
    (0x0221, 38, 33, false),
    (0x5601, 7, 6, true),
    (0x5401, 8, 14, false),
    (0x4801, 9, 14, false),
    (0x3801, 10, 14, false),
    (0x3001, 11, 17, false),
    (0x2401, 12, 18, false),
    (0x1C01, 13, 20, false),
    (0x1601, 29, 21, false),
    (0x5601, 15, 14, true),
    (0x5401, 16, 14, false),
    (0x5101, 17, 15, false),
    (0x4801, 18, 16, false),
    (0x3801, 19, 17, false),
    (0x3401, 20, 18, false),
    (0x3001, 21, 19, false),
    (0x2801, 22, 19, false),
    (0x2401, 23, 20, false),
    (0x2201, 24, 21, false),
    (0x1C01, 25, 22, false),
    (0x1801, 26, 23, false),
    (0x1601, 27, 24, false),
    (0x1401, 28, 25, false),
    (0x1201, 29, 26, false),
    (0x1101, 30, 27, false),
    (0x0AC1, 31, 28, false),
    (0x09C1, 32, 29, false),
    (0x08A1, 33, 30, false),
    (0x0521, 34, 31, false),
    (0x0441, 35, 32, false),
    (0x02A1, 36, 33, false),
    (0x0221, 37, 34, false),
    (0x0141, 38, 35, false),
    (0x0111, 39, 36, false),
    (0x0085, 40, 37, false),
    (0x0049, 41, 38, false),
    (0x0025, 42, 39, false),
    (0x0015, 43, 40, false),
    (0x0009, 44, 41, false),
    (0x0005, 45, 42, false),
    (0x0001, 45, 43, false),
    (0x5601, 46, 46, false),
];

/// A context's adaptive state: low 7 bits = index into [`QE_TABLE`], bit 7
/// = the current MPS value. Zero (index 0, MPS 0) is the T.88 initial state.
type Cx = u8;

/// T.88 Annex E.2 MQ encoder (software conventions, Figures E.3-E.10).
struct MqEncoder {
    a: u32,
    c: u32,
    ct: u32,
    /// The byte `B` under construction; `None` before the first BYTEOUT
    /// (the virtual byte at `BPST - 1`, which is never emitted).
    b: Option<u8>,
    out: Vec<u8>,
}

impl MqEncoder {
    fn new() -> Self {
        MqEncoder {
            a: 0x8000,
            c: 0,
            ct: 12,
            b: None,
            out: Vec::new(),
        }
    }

    /// BP = BP + 1; B = v — the previous byte is final.
    fn advance(&mut self, v: u8) {
        if let Some(prev) = self.b {
            self.out.push(prev);
        }
        self.b = Some(v);
    }

    fn byte_out(&mut self) {
        let b = self.b.unwrap_or(0);
        if b == 0xFF {
            self.advance((self.c >> 20) as u8);
            self.c &= 0xF_FFFF;
            self.ct = 7;
        } else if self.c < 0x800_0000 {
            self.advance((self.c >> 19) as u8);
            self.c &= 0x7_FFFF;
            self.ct = 8;
        } else {
            // Carry into B. It can never reach the virtual first byte: the
            // interval starts at [0, 0x8000) and only shrinks, so after the
            // first 12 shifts C + A <= 0x8000 << 12 = 0x8000000.
            let nb = b + 1;
            self.b = Some(nb);
            if nb == 0xFF {
                self.c &= 0x7FF_FFFF;
                self.advance((self.c >> 20) as u8);
                self.c &= 0xF_FFFF;
                self.ct = 7;
            } else {
                self.advance((self.c >> 19) as u8);
                self.c &= 0x7_FFFF;
                self.ct = 8;
            }
        }
    }

    fn renorm(&mut self) {
        loop {
            self.a <<= 1;
            self.c <<= 1;
            self.ct -= 1;
            if self.ct == 0 {
                self.byte_out();
            }
            if self.a & 0x8000 != 0 {
                break;
            }
        }
    }

    fn encode(&mut self, cx: &mut Cx, d: u32) {
        let index = (*cx & 0x7F) as usize;
        let mps = u32::from(*cx >> 7);
        let (qe, nmps, nlps, switch) = QE_TABLE[index];
        self.a -= qe;
        if d == mps {
            // CODEMPS
            if self.a & 0x8000 == 0 {
                if self.a < qe {
                    self.a = qe;
                } else {
                    self.c += qe;
                }
                *cx = nmps | (*cx & 0x80);
                self.renorm();
            } else {
                self.c += qe;
            }
        } else {
            // CODELPS
            if self.a < qe {
                self.c += qe;
            } else {
                self.a = qe;
            }
            let mps = if switch { 1 - mps } else { mps };
            *cx = nlps | ((mps as u8) << 7);
            self.renorm();
        }
    }

    /// FLUSH (Figure E.11) including the trailing `0xFF 0xAC` marker the
    /// standard's own PDF example (ISO 32000-1 §7.4.7, EXAMPLE 2) carries.
    fn finish(mut self) -> Vec<u8> {
        // SETBITS
        let temp = self.c + self.a;
        self.c |= 0xFFFF;
        if self.c >= temp {
            self.c -= 0x8000;
        }
        self.c <<= self.ct;
        self.byte_out();
        self.c <<= self.ct;
        self.byte_out();
        let last = self.b;
        if let Some(prev) = last {
            self.out.push(prev);
        }
        if last != Some(0xFF) {
            self.out.push(0xFF);
        }
        self.out.push(0xAC);
        self.out
    }
}

/// T.88 Annex E.3 MQ decoder. Bytes past the end of `data` read as `0xFF`,
/// which the BYTEIN procedure treats as a marker (no advance, 1-bits fed),
/// so decoding never reads out of bounds and never stalls.
struct MqDecoder<'a> {
    data: &'a [u8],
    pos: usize,
    a: u32,
    c: u32,
    ct: u32,
}

impl<'a> MqDecoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        let mut d = MqDecoder {
            data,
            pos: 0,
            a: 0x8000,
            c: 0,
            ct: 0,
        };
        d.c = u32::from(d.byte(0)) << 16;
        d.byte_in();
        d.c <<= 7;
        d.ct -= 7;
        d.a = 0x8000;
        d
    }

    fn byte(&self, i: usize) -> u8 {
        self.data.get(i).copied().unwrap_or(0xFF)
    }

    fn byte_in(&mut self) {
        if self.byte(self.pos) == 0xFF {
            let b1 = self.byte(self.pos + 1);
            if b1 > 0x8F {
                self.c = self.c.wrapping_add(0xFF00);
                self.ct = 8;
            } else {
                self.pos += 1;
                self.c = self.c.wrapping_add(u32::from(b1) << 9);
                self.ct = 7;
            }
        } else {
            self.pos += 1;
            self.c = self.c.wrapping_add(u32::from(self.byte(self.pos)) << 8);
            self.ct = 8;
        }
    }

    fn renorm(&mut self) {
        loop {
            if self.ct == 0 {
                self.byte_in();
            }
            self.a <<= 1;
            self.c <<= 1;
            self.ct -= 1;
            if self.a & 0x8000 != 0 {
                break;
            }
        }
    }

    fn decode(&mut self, cx: &mut Cx) -> u32 {
        let index = (*cx & 0x7F) as usize;
        let mps = u32::from(*cx >> 7);
        let (qe, nmps, nlps, switch) = QE_TABLE[index];
        self.a -= qe;
        let lps = |cx: &mut Cx| {
            let m = if switch { 1 - mps } else { mps };
            *cx = nlps | ((m as u8) << 7);
            1 - mps
        };
        if (self.c >> 16) < qe {
            // Lower sub-interval: the LPS, unless conditionally exchanged.
            let d = if self.a < qe {
                *cx = nmps | (*cx & 0x80);
                mps
            } else {
                lps(cx)
            };
            self.a = qe;
            self.renorm();
            d
        } else {
            self.c -= qe << 16;
            if self.a & 0x8000 == 0 {
                let d = if self.a < qe {
                    lps(cx)
                } else {
                    *cx = nmps | (*cx & 0x80);
                    mps
                };
                self.renorm();
                d
            } else {
                mps
            }
        }
    }
}

/// Generic-region coding parameters: the fixed template (`GBTEMPLATE`
/// 0-3), typical prediction (`TPGDON`), and the adaptive-template pixel
/// offsets (`GBAT`; template 0 uses four, templates 1-3 use the first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GenericParams {
    pub template: u8,
    pub tpgd: bool,
    pub at: [(i8, i8); 4],
}

impl GenericParams {
    /// The nominal AT positions of T.88 §6.2.5.4 for `template`.
    pub(crate) const fn nominal(template: u8, tpgd: bool) -> Self {
        let at = match template {
            0 => [(3, -1), (-3, -1), (2, -2), (-2, -2)],
            1 => [(3, -1), (0, 0), (0, 0), (0, 0)],
            _ => [(2, -1), (0, 0), (0, 0), (0, 0)],
        };
        GenericParams { template, tpgd, at }
    }

    /// The same parameters with the first AT pixel moved to `(x, y)`.
    pub(crate) const fn with_a1(mut self, x: i8, y: i8) -> Self {
        self.at[0] = (x, y);
        self
    }

    const fn at_count(&self) -> usize {
        if self.template == 0 {
            4
        } else {
            1
        }
    }

    /// Every AT pixel must name an already-coded pixel (above, or left on
    /// the current row) — T.88 §6.2.5.4's restriction. `None` for an
    /// unsupported template.
    fn valid(&self) -> bool {
        self.template <= 3
            && self.at[..self.at_count()]
                .iter()
                .all(|&(x, y)| y < 0 || (y == 0 && x < 0))
    }
}

/// The context value T.88 §6.2.5.7 step 3(b) codes `SLTP` in, per template.
const TPGD_CONTEXT: [usize; 4] = [0x9B25, 0x0795, 0x00E5, 0x0195];

/// A 1-bit image, rows packed MSB-first, `stride = ceil(width / 8)`, JBIG2
/// polarity (1 = black). Padding bits past `width` are always 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bitmap {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub data: Vec<u8>,
}

impl Bitmap {
    #[inline]
    fn bit(&self, x: isize, y: isize) -> u32 {
        if x < 0 || y < 0 || x >= self.width as isize || y >= self.height as isize {
            return 0;
        }
        let (x, y) = (x as usize, y as usize);
        u32::from(self.data[y * self.stride + x / 8] >> (7 - x % 8)) & 1
    }

    fn row(&self, y: usize) -> &[u8] {
        &self.data[y * self.stride..(y + 1) * self.stride]
    }
}

/// Map PDF sample rows (1 bit, packed, `/Decode [0 1]` sense: 0 = black
/// under DeviceGray) to a JBIG2 bitmap: bitwise NOT, padding cleared.
/// `None` when the buffer does not hold exactly `height` rows.
pub(crate) fn samples_to_bitmap(samples: &[u8], width: usize, height: usize) -> Option<Bitmap> {
    let stride = width.div_ceil(8);
    if width == 0 || height == 0 || samples.len() != stride.checked_mul(height)? {
        return None;
    }
    let pad_mask = if width.is_multiple_of(8) {
        0xFF
    } else {
        0xFFu8 << (8 - width % 8)
    };
    let mut data: Vec<u8> = samples.iter().map(|b| !b).collect();
    for row in data.chunks_exact_mut(stride) {
        row[stride - 1] &= pad_mask;
    }
    Some(Bitmap {
        width,
        height,
        stride,
        data,
    })
}

/// Shared generic-region coding loop: the encoder and the decoder walk the
/// same pixels and contexts; only the per-decision step differs.
trait Coder {
    const ENCODING: bool;
    fn code(&mut self, cx: &mut Cx, bit: u32) -> u32;
}

impl Coder for MqEncoder {
    const ENCODING: bool = true;
    #[inline]
    fn code(&mut self, cx: &mut Cx, bit: u32) -> u32 {
        self.encode(cx, bit);
        bit
    }
}

impl Coder for MqDecoder<'_> {
    const ENCODING: bool = false;
    #[inline]
    fn code(&mut self, cx: &mut Cx, _bit: u32) -> u32 {
        self.decode(cx)
    }
}

/// Fixed-pixel window layout for one template: for rows y-2 / y-1 / y, the
/// leftmost x offset, the pixel count, and the bit shift inside CONTEXT;
/// then the bit slot of each AT pixel. Bit order is T.88's (pixels left to
/// right within a row, upper rows in higher bits, AT pixels in the slots
/// the spec's figures give them), which fixes where `TPGD_CONTEXT` lands.
struct Layout {
    rows: [(isize, u32, u32); 3], // (x0, len, shift) for dy = -2, -1, 0
    at_shift: [u32; 4],
    bits: u32,
}

fn layout(template: u8) -> Layout {
    match template {
        0 => Layout {
            rows: [(-1, 3, 12), (-2, 5, 5), (-4, 4, 0)],
            at_shift: [4, 10, 11, 15],
            bits: 16,
        },
        1 => Layout {
            rows: [(-1, 4, 9), (-2, 5, 4), (-3, 3, 0)],
            at_shift: [3, 0, 0, 0],
            bits: 13,
        },
        2 => Layout {
            rows: [(-1, 3, 7), (-2, 4, 3), (-2, 2, 0)],
            at_shift: [2, 0, 0, 0],
            bits: 10,
        },
        _ => Layout {
            rows: [(0, 0, 0), (-3, 5, 5), (-4, 4, 0)],
            at_shift: [4, 0, 0, 0],
            bits: 10,
        },
    }
}

fn run<C: Coder>(bm: &mut Bitmap, p: &GenericParams, coder: &mut C) {
    let lay = layout(p.template);
    let mut cx = vec![0 as Cx; 1usize << lay.bits];
    let (w, h) = (bm.width as isize, bm.height as isize);
    let n_at = p.at_count();
    let mut ltp = false;
    for y in 0..h {
        if p.tpgd {
            let typical = C::ENCODING
                && if y == 0 {
                    bm.row(0).iter().all(|&b| b == 0)
                } else {
                    bm.row(y as usize) == bm.row(y as usize - 1)
                };
            let sltp = coder.code(
                &mut cx[TPGD_CONTEXT[p.template as usize]],
                u32::from(typical != ltp),
            );
            ltp ^= sltp == 1;
            if ltp {
                if !C::ENCODING && y > 0 {
                    let s = bm.stride;
                    let yu = y as usize;
                    bm.data.copy_within((yu - 1) * s..yu * s, yu * s);
                }
                continue;
            }
        }
        // Rolling windows over the fixed pixels of rows y-2 and y-1, primed
        // for x = 0 (the pixel at x0 + len - 1 enters on each step).
        let mut win = [0u32; 2];
        for (k, wk) in win.iter_mut().enumerate() {
            let (x0, len, _) = lay.rows[k];
            for i in 0..len as isize - 1 {
                *wk = (*wk << 1) | bm.bit(x0 + i, y - 2 + k as isize);
            }
        }
        let mut cur = 0u32;
        let masks = [
            (1u32 << lay.rows[0].1) - 1,
            (1u32 << lay.rows[1].1) - 1,
            (1u32 << lay.rows[2].1) - 1,
        ];
        let stride = bm.stride;
        let row_off = y as usize * stride;
        for x in 0..w {
            for (k, wk) in win.iter_mut().enumerate() {
                let (x0, len, _) = lay.rows[k];
                *wk =
                    ((*wk << 1) | bm.bit(x + x0 + len as isize - 1, y - 2 + k as isize)) & masks[k];
            }
            let mut ctx = (win[0] << lay.rows[0].2) | (win[1] << lay.rows[1].2) | cur;
            for (i, &(ax, ay)) in p.at[..n_at].iter().enumerate() {
                ctx |= bm.bit(x + ax as isize, y + ay as isize) << lay.at_shift[i];
            }
            let xu = x as usize;
            let bit = u32::from(bm.data[row_off + xu / 8] >> (7 - xu % 8)) & 1;
            let v = coder.code(&mut cx[ctx as usize], bit);
            if !C::ENCODING && v == 1 {
                bm.data[row_off + xu / 8] |= 0x80 >> (xu % 8);
            }
            cur = ((cur << 1) | v) & masks[2];
        }
    }
}

/// Arithmetic-code `bm` as a generic region; returns the MQ data (flushed,
/// marker-terminated). `None` for invalid parameters.
///
/// `bm` is only read: it is borrowed mutably because the decoder writes
/// through the same coding loop, and borrowing beats cloning a bitmap of up
/// to 32 MiB per candidate.
pub(crate) fn encode_generic(bm: &mut Bitmap, p: &GenericParams) -> Option<Vec<u8>> {
    if !p.valid() {
        return None;
    }
    let mut enc = MqEncoder::new();
    run(bm, p, &mut enc);
    Some(enc.finish())
}

/// Decode `width × height` pixels of generic-region MQ data. Always
/// terminates after exactly that many decisions; `None` for invalid
/// parameters or dimensions.
pub(crate) fn decode_generic(
    data: &[u8],
    width: usize,
    height: usize,
    p: &GenericParams,
) -> Option<Bitmap> {
    if !p.valid() || width == 0 || height == 0 {
        return None;
    }
    if (width as u64).checked_mul(height as u64)? > MAX_PIXELS {
        return None;
    }
    let stride = width.div_ceil(8);
    let len = stride.checked_mul(height)?;
    let mut bm = Bitmap {
        width,
        height,
        stride,
        data: vec![0; len],
    };
    let mut dec = MqDecoder::new(data);
    run(&mut bm, p, &mut dec);
    Some(bm)
}

const SEG_PAGE_INFO: u8 = 48;
const SEG_IMMEDIATE_GENERIC: u8 = 38;

/// One segment header (T.88 §7.2): number, type flags (1-byte page
/// association), no referred-to segments, page 1, data length.
fn segment_header(out: &mut Vec<u8>, number: u32, kind: u8, data_len: u32) {
    out.extend_from_slice(&number.to_be_bytes());
    out.push(kind);
    out.push(0);
    out.push(1);
    out.extend_from_slice(&data_len.to_be_bytes());
}

/// Fixed bytes the embedded stream spends beyond the MQ data: page
/// information segment (11 + 19) plus the generic-region segment header and
/// fields (11 + 17 + 1 + AT bytes).
pub(crate) fn framing_len(p: &GenericParams) -> usize {
    30 + 29 + 2 * p.at_count()
}

/// Frame MQ data as a PDF `/JBIG2Decode` stream: page information segment
/// then one immediate generic region, in embedded organization, without the
/// file header and end-of-page / end-of-file segments (ISO 32000-1 §7.4.7).
pub(crate) fn embed(width: u32, height: u32, p: &GenericParams, mq: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(framing_len(p) + mq.len());
    segment_header(&mut out, 0, SEG_PAGE_INFO, 19);
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // X resolution: unknown
    out.extend_from_slice(&0u32.to_be_bytes()); // Y resolution: unknown
    out.push(0x01); // eventually lossless; default pixel 0; combop OR
    out.extend_from_slice(&0u16.to_be_bytes()); // not striped
    let region_len = 17 + 1 + 2 * p.at_count() + mq.len();
    segment_header(
        &mut out,
        1,
        SEG_IMMEDIATE_GENERIC,
        u32::try_from(region_len).ok()?,
    );
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // X location
    out.extend_from_slice(&0u32.to_be_bytes()); // Y location
    out.push(0); // external combination operator OR
    out.push((p.template << 1) | (u8::from(p.tpgd) << 3)); // MMR 0
    for &(x, y) in &p.at[..p.at_count()] {
        out.push(x as u8);
        out.push(y as u8);
    }
    out.extend_from_slice(mq);
    Some(out)
}

fn be32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

/// Strict inverse of [`embed`]: parse exactly the two-segment shape this
/// module writes and decode it, checking every header field against the
/// expected image geometry. Anything else is `None` — this is the
/// decode-back gate for our own output, not a general JBIG2 reader.
pub(crate) fn decode_embedded(stream: &[u8], width: u32, height: u32) -> Option<Bitmap> {
    // Page information segment.
    if be32(stream, 0)? != 0 || stream.get(4..7)? != [SEG_PAGE_INFO, 0, 1] {
        return None;
    }
    if be32(stream, 7)? != 19
        || be32(stream, 11)? != width
        || be32(stream, 15)? != height
        || stream.get(27)? & 0x04 != 0 // default pixel must be 0
        || stream.get(28..30)? != [0, 0]
    {
        return None;
    }
    // Generic region segment.
    let g = 30;
    if be32(stream, g)? != 1 || stream.get(g + 4..g + 7)? != [SEG_IMMEDIATE_GENERIC, 0, 1] {
        return None;
    }
    let data_len = be32(stream, g + 7)? as usize;
    let body = stream.get(g + 11..)?;
    if body.len() != data_len
        || be32(body, 0)? != width
        || be32(body, 4)? != height
        || be32(body, 8)? != 0
        || be32(body, 12)? != 0
        || *body.get(16)? != 0
    {
        return None;
    }
    let flags = *body.get(17)?;
    if flags & !0x0E != 0 {
        return None; // MMR or EXTTEMPLATE or reserved bits set
    }
    let template = (flags >> 1) & 3;
    let mut p = GenericParams::nominal(template, flags & 0x08 != 0);
    let n = p.at_count();
    let at = body.get(18..18 + 2 * n)?;
    for i in 0..n {
        p.at[i] = (at[2 * i] as i8, at[2 * i + 1] as i8);
    }
    decode_generic(&body[18 + 2 * n..], width as usize, height as usize, &p)
}

/// Encode `bm` under each candidate parameter set and return the smallest
/// embedded stream that [`decode_embedded`] reproduces exactly (ties keep
/// the earlier candidate). Only the running smallest stream is held, and
/// only the winner is decoded back; should it fail, every candidate is
/// re-run with its own decode-back, so a defect costs time, never bytes or
/// correctness. `None` when no candidate verifies.
pub(crate) fn encode_best(bm: &mut Bitmap, candidates: &[GenericParams]) -> Option<Vec<u8>> {
    let (w, h) = (
        u32::try_from(bm.width).ok()?,
        u32::try_from(bm.height).ok()?,
    );
    let mut best: Option<Vec<u8>> = None;
    for p in candidates {
        let Some(stream) = encode_generic(bm, p).and_then(|mq| embed(w, h, p, &mq)) else {
            continue;
        };
        if best.as_ref().is_none_or(|b| stream.len() < b.len()) {
            best = Some(stream);
        }
    }
    if let Some(b) = &best {
        if decode_embedded(b, w, h).as_ref() == Some(&*bm) {
            return best;
        }
    }
    let mut best: Option<Vec<u8>> = None;
    for p in candidates {
        let Some(stream) = encode_generic(bm, p).and_then(|mq| embed(w, h, p, &mq)) else {
            continue;
        };
        if best.as_ref().is_none_or(|b| stream.len() < b.len())
            && decode_embedded(&stream, w, h).as_ref() == Some(&*bm)
        {
            best = Some(stream);
        }
    }
    best
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) struct Lcg(pub u64);
    impl Lcg {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    pub(crate) fn bitmap_from(
        width: usize,
        height: usize,
        f: impl Fn(usize, usize) -> bool,
    ) -> Bitmap {
        let stride = width.div_ceil(8);
        let mut data = vec![0u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                if f(x, y) {
                    data[y * stride + x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        Bitmap {
            width,
            height,
            stride,
            data,
        }
    }

    fn all_params() -> Vec<GenericParams> {
        let mut v = Vec::new();
        for t in 0..4 {
            for tpgd in [false, true] {
                v.push(GenericParams::nominal(t, tpgd));
            }
        }
        // Moved AT pixels, including a current-row and a far-above one.
        let mut p = GenericParams::nominal(0, true);
        p.at[0] = (-1, 0);
        p.at[1] = (5, -3);
        v.push(p);
        let mut p = GenericParams::nominal(2, true);
        p.at[0] = (-6, -9);
        v.push(p);
        v
    }

    fn round_trip(mut bm: Bitmap) {
        let bm = &mut bm;
        for p in all_params() {
            let mq = encode_generic(bm, &p).unwrap();
            let back = decode_generic(&mq, bm.width, bm.height, &p).unwrap();
            assert_eq!(
                &back, &*bm,
                "round trip failed for {p:?} on {}x{}",
                bm.width, bm.height
            );
            let s = embed(bm.width as u32, bm.height as u32, &p, &mq).unwrap();
            assert_eq!(s.len(), framing_len(&p) + mq.len());
            assert_eq!(
                decode_embedded(&s, bm.width as u32, bm.height as u32).as_ref(),
                Some(&*bm)
            );
        }
    }

    fn text_like(width: usize, height: usize, seed: u64) -> Bitmap {
        let mut rng = Lcg(seed);
        let mut cells = vec![false; width * height];
        let mut y = 3;
        while y + 8 < height {
            let mut x = 2 + (rng.next() % 7) as usize;
            while x + 6 < width {
                let gw = 1 + (rng.next() % 5) as usize;
                let gh = 2 + (rng.next() % 6) as usize;
                for dy in 0..gh {
                    for dx in 0..gw {
                        cells[(y + dy) * width + x + dx] = true;
                    }
                }
                x += gw + 1 + (rng.next() % 6) as usize;
            }
            y += 9 + (rng.next() % 5) as usize;
        }
        bitmap_from(width, height, |x, y| cells[y * width + x])
    }

    #[test]
    fn round_trips_random_white_black_and_edge_shapes() {
        let mut rng = Lcg(1);
        let noise: Vec<bool> = (0..97 * 61).map(|_| rng.next() & 1 == 1).collect();
        round_trip(bitmap_from(97, 61, |x, y| noise[y * 97 + x]));
        round_trip(bitmap_from(64, 40, |_, _| false));
        round_trip(bitmap_from(64, 40, |_, _| true));
        round_trip(bitmap_from(1, 1, |_, _| true));
        round_trip(bitmap_from(1, 1, |_, _| false));
        round_trip(bitmap_from(1, 300, |_, y| y % 3 == 0));
        round_trip(bitmap_from(300, 1, |x, _| x % 5 < 2));
        round_trip(bitmap_from(33, 17, |x, y| (x * y) % 7 == 1));
        round_trip(bitmap_from(31, 9, |x, y| (x + y) % 2 == 0));
        // Duplicate rows (exercises TPGD's typical-row path both ways).
        round_trip(bitmap_from(77, 50, |x, y| (y / 10) % 2 == 0 && x % 9 < 4));
        round_trip(text_like(203, 120, 9));
    }

    #[test]
    fn mq_coder_round_trips_long_skewed_sequences() {
        // Stresses carry propagation and 0xFF stuffing: long MPS runs, then
        // bursts, through a handful of contexts.
        let mut rng = Lcg(5);
        let bits: Vec<(usize, u32)> = (0..200_000)
            .map(|i| {
                let ctx = (rng.next() % 4) as usize;
                let p = if i % 50_000 < 25_000 { 2 } else { 60 };
                (ctx, u32::from(rng.next() % 100 < p))
            })
            .collect();
        let mut enc = MqEncoder::new();
        let mut cx = [0 as Cx; 4];
        for &(c, b) in &bits {
            enc.encode(&mut cx[c], b);
        }
        let data = enc.finish();
        assert!(data.ends_with(&[0xFF, 0xAC]));
        let mut dec = MqDecoder::new(&data);
        let mut cx = [0 as Cx; 4];
        for (i, &(c, b)) in bits.iter().enumerate() {
            assert_eq!(dec.decode(&mut cx[c]), b, "decision {i}");
        }
    }

    /// The MQ coder test sequence of T.88 Annex H.2: 256 bits coded in one
    /// context; the standard lists the resulting code bytes.
    #[test]
    fn mq_encoder_matches_t88_annex_h2_vector() {
        const INPUT: [u8; 32] = [
            0x00, 0x02, 0x00, 0x51, 0x00, 0x00, 0x00, 0xC0, 0x03, 0x52, 0x87, 0x2A, 0xAA, 0xAA,
            0xAA, 0xAA, 0x82, 0xC0, 0x20, 0x00, 0xFC, 0xD7, 0x9E, 0xF6, 0xBF, 0x7F, 0xED, 0x90,
            0x4F, 0x46, 0xA3, 0xBF,
        ];
        const OUTPUT: [u8; 30] = [
            0x84, 0xC7, 0x3B, 0xFC, 0xE1, 0xA1, 0x43, 0x04, 0x02, 0x20, 0x00, 0x00, 0x41, 0x0D,
            0xBB, 0x86, 0xF4, 0x31, 0x7F, 0xFF, 0x88, 0xFF, 0x37, 0x47, 0x1A, 0xDB, 0x6A, 0xDF,
            0xFF, 0xAC,
        ];
        let mut enc = MqEncoder::new();
        let mut cx: Cx = 0;
        for byte in INPUT {
            for i in (0..8).rev() {
                enc.encode(&mut cx, u32::from(byte >> i) & 1);
            }
        }
        assert_eq!(enc.finish(), OUTPUT);
        let mut dec = MqDecoder::new(&OUTPUT);
        let mut cx: Cx = 0;
        for byte in INPUT {
            for i in (0..8).rev() {
                assert_eq!(dec.decode(&mut cx), u32::from(byte >> i) & 1);
            }
        }
    }

    #[test]
    fn samples_map_to_inverted_bitmap_with_clean_padding() {
        // 10 px wide: samples 0 = black -> bitmap 1. Padding bits of the
        // inverted samples must not leak in as black pixels.
        let samples = [0b0011_1111, 0b1111_1111, 0b1111_1111, 0b0100_0000];
        let bm = samples_to_bitmap(&samples, 10, 2).unwrap();
        assert_eq!(
            bm.data,
            vec![0b1100_0000, 0b0000_0000, 0b0000_0000, 0b1000_0000]
        );
        assert!(samples_to_bitmap(&samples, 10, 3).is_none());
        assert!(samples_to_bitmap(&[], 0, 0).is_none());
    }

    #[test]
    fn invalid_at_pixels_and_templates_are_refused() {
        let mut bm = bitmap_from(8, 8, |x, _| x == 3);
        let bm = &mut bm;
        let mut p = GenericParams::nominal(0, false);
        p.at[2] = (0, 0); // the pixel being coded
        assert!(encode_generic(bm, &p).is_none());
        assert!(decode_generic(&[0; 4], 8, 8, &p).is_none());
        p.at[2] = (1, 1); // a future row
        assert!(encode_generic(bm, &p).is_none());
        let mut p = GenericParams::nominal(0, false);
        p.template = 4;
        assert!(encode_generic(bm, &p).is_none());
    }

    #[test]
    fn decoder_is_bounded_on_garbage_and_truncation() {
        let mut bm = text_like(120, 80, 3);
        let p = GenericParams::nominal(0, true);
        let s = embed(120, 80, &p, &encode_generic(&mut bm, &p).unwrap()).unwrap();
        // Truncated stream: the framing check refuses it (length mismatch).
        assert!(decode_embedded(&s[..s.len() - 5], 120, 80).is_none());
        // Geometry mismatch.
        assert!(decode_embedded(&s, 121, 80).is_none());
        assert!(decode_embedded(&s, 120, 79).is_none());
        // Raw garbage through the region decoder terminates with a bitmap of
        // exactly the declared size.
        let mut rng = Lcg(11);
        let junk: Vec<u8> = (0..300).map(|_| rng.next() as u8).collect();
        for p in all_params() {
            let out = decode_generic(&junk, 50, 40, &p).unwrap();
            assert_eq!(out.data.len(), 7 * 40);
            let out = decode_generic(&[], 50, 40, &p).unwrap();
            assert_eq!(out.data.len(), 7 * 40);
        }
        assert!(decode_generic(&junk, 0, 40, &p).is_none());
        assert!(decode_generic(&junk, usize::MAX, 2, &p).is_none());
    }

    #[test]
    fn encode_best_picks_the_smallest_verified_candidate() {
        let mut bm = text_like(300, 200, 4);
        let cands = all_params();
        let best = encode_best(&mut bm, &cands).unwrap();
        for p in &cands {
            let s = embed(300, 200, p, &encode_generic(&mut bm, p).unwrap()).unwrap();
            assert!(best.len() <= s.len());
        }
        assert_eq!(decode_embedded(&best, 300, 200).as_ref(), Some(&bm));
    }
}
