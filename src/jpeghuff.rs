//! Lossless Huffman-table re-optimization for baseline JPEG streams.
//!
//! Producers routinely emit the ITU-T T.81 Annex K example Huffman tables
//! instead of tables fitted to the image's own symbol statistics. Rebuilding
//! those tables is what `jpegtran -optimize` does, and it is *strictly*
//! lossless: the DCT coefficients are never decoded, dequantized, or
//! re-quantized — only the entropy coding of an unchanged symbol sequence
//! changes. Two JPEGs that differ only in their Huffman tables decode to
//! bit-identical pixels by construction.
//!
//! The pass is syntactic, not photometric. A scan is decoded into its token
//! sequence — for every block, the DC magnitude symbol plus its additional
//! bits, then the run/size AC symbols plus theirs — the per-table symbol
//! frequencies are counted, optimal (length-limited, canonical) tables are
//! generated with libjpeg's `jpeg_gen_optimal_table` algorithm, and the same
//! token sequence is re-emitted against them. Nothing else in the file moves:
//! `APPn`, `COM`, `DQT`, `SOF`, `DRI` and the `SOS` headers are copied
//! verbatim, restart markers land at the same MCU boundaries, and only the
//! `DHT` segments are replaced.
//!
//! Scope: baseline and extended sequential Huffman frames (`SOF0`/`SOF1`) at
//! 8-bit precision. Progressive (`SOF2`) carries a per-coefficient refinement
//! state this token model does not reproduce, and arithmetic-coded or
//! hierarchical frames are a different codec; all of them decline, leaving
//! the stream byte-identical.
//!
//! Fail-safe contract: [`optimize`] returns `None` — meaning "ship the
//! original bytes" — on *any* parse surprise, on any structure outside the
//! scope above, when the rebuilt stream is not strictly smaller, and when the
//! rebuilt stream does not decode back to exactly the token sequence that was
//! read out of the input. That last check is a full round trip: it proves the
//! output's entropy-coded data carries the same symbols and the same
//! additional bits, i.e. the same coefficients, as the input.

/// One entropy-coded token.
///
/// [`Token::Sym`] is a Huffman symbol plus the additional bits that follow it;
/// `table` is `class << 4 | id` (class 0 = DC, 1 = AC). [`Token::Raw`] is a
/// group of bits carrying no Huffman symbol at all: progressive
/// successive-approximation scans emit DC refinement bits and AC correction
/// bits directly into the entropy stream, outside any table. Raw bits are
/// copied through untouched, so they contribute nothing to the symbol
/// statistics and re-emit at exactly the same bit positions.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Token {
    Sym {
        table: u8,
        sym: u8,
        bits: u16,
        nbits: u8,
    },
    Raw {
        bits: u32,
        nbits: u8,
    },
}

/// Append one raw bit, coalescing into the trailing [`Token::Raw`] group while
/// it still has room. Refinement scans emit long runs of these; packing them
/// keeps the token vector proportional to bytes rather than bits.
fn push_raw_bit(run: &mut Vec<Token>, bit: u32) {
    if let Some(Token::Raw { bits, nbits }) = run.last_mut() {
        if *nbits < 32 {
            *bits = (*bits << 1) | bit;
            *nbits += 1;
            return;
        }
    }
    run.push(Token::Raw { bits: bit, nbits: 1 });
}

/// A frame component as declared in `SOF`.
struct Component {
    h: u8,
    v: u8,
}

struct Frame {
    x: u16,
    y: u16,
    comps: Vec<Component>,
    /// Component index by component id, for `SOS` lookup.
    ids: Vec<u8>,
    /// Total blocks in each component's own grid — the grid every
    /// non-interleaved scan walks, so it indexes the progressive coefficient
    /// state consistently across a frame's scans.
    blocks: Vec<usize>,
    progressive: bool,
}

/// A decoded scan: its `SOS` payload verbatim and the tokens of each
/// restart interval (one entry when the scan has no restart markers).
struct Scan {
    header: Vec<u8>,
    runs: Vec<Vec<Token>>,
}

/// A canonical Huffman table in decode form: `(length, code) -> value`,
/// stored as per-length first-code / first-index bases.
#[derive(Clone, Default)]
struct DecodeTable {
    /// `mincode[l]`, `maxcode[l]`, `valptr[l]` for code length `l` (1..=16);
    /// `maxcode[l] < 0` marks an unused length.
    mincode: [i32; 17],
    maxcode: [i32; 17],
    valptr: [usize; 17],
    values: Vec<u8>,
}

impl DecodeTable {
    /// Build from the `BITS`/`HUFFVAL` form carried in a `DHT` segment.
    fn build(counts: &[u8; 16], values: Vec<u8>) -> Option<Self> {
        let mut t = DecodeTable {
            values,
            ..Default::default()
        };
        let mut code: i32 = 0;
        let mut k = 0usize;
        for l in 1..=16usize {
            let n = usize::from(counts[l - 1]);
            if n == 0 {
                t.maxcode[l] = -1;
                code <<= 1;
                continue;
            }
            t.valptr[l] = k;
            t.mincode[l] = code;
            k += n;
            code += i32::try_from(n).ok()?;
            t.maxcode[l] = code - 1;
            if code > (1 << l) {
                return None; // over-subscribed table
            }
            code <<= 1;
        }
        if k != t.values.len() {
            return None;
        }
        Some(t)
    }
}

/// MSB-first reader over an entropy-coded segment with `FF 00` unstuffing.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u32,
    cnt: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        BitReader {
            data,
            pos,
            buf: 0,
            cnt: 0,
        }
    }

    /// Next entropy byte, unstuffing `FF 00`. `None` at a real marker or EOF.
    fn next_byte(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        if b != 0xFF {
            self.pos += 1;
            return Some(b);
        }
        if *self.data.get(self.pos + 1)? != 0x00 {
            return None;
        }
        self.pos += 2;
        Some(0xFF)
    }

    fn bit(&mut self) -> Option<u32> {
        if self.cnt == 0 {
            self.buf = u32::from(self.next_byte()?);
            self.cnt = 8;
        }
        self.cnt -= 1;
        Some((self.buf >> self.cnt) & 1)
    }

    fn bits(&mut self, n: u8) -> Option<u16> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        u16::try_from(v).ok()
    }

    fn decode(&mut self, t: &DecodeTable) -> Option<u8> {
        let mut code = i32::try_from(self.bit()?).ok()?;
        for l in 1..=16usize {
            if t.maxcode[l] >= 0 && code <= t.maxcode[l] {
                let idx = t.valptr[l] + usize::try_from(code - t.mincode[l]).ok()?;
                return t.values.get(idx).copied();
            }
            code = (code << 1) | i32::try_from(self.bit()?).ok()?;
        }
        None
    }

    /// Drop any partial byte and consume the expected `RSTn` marker.
    fn restart(&mut self, n: u8) -> Option<()> {
        self.cnt = 0;
        if *self.data.get(self.pos)? != 0xFF || *self.data.get(self.pos + 1)? != 0xD0 + (n & 7) {
            return None;
        }
        self.pos += 2;
        Some(())
    }
}

/// MSB-first writer with `FF 00` stuffing.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    buf: u32,
    cnt: u8,
}

impl BitWriter {
    fn put(&mut self, code: u32, len: u8) {
        for i in (0..len).rev() {
            self.buf = (self.buf << 1) | ((code >> i) & 1);
            self.cnt += 1;
            if self.cnt == 8 {
                let b = u8::try_from(self.buf & 0xFF).unwrap_or(0);
                self.out.push(b);
                if b == 0xFF {
                    self.out.push(0x00);
                }
                self.cnt = 0;
                self.buf = 0;
            }
        }
    }

    /// Pad the final partial byte with 1 bits, per T.81 F.1.2.3.
    fn flush(&mut self) {
        if self.cnt > 0 {
            let pad = 8 - self.cnt;
            self.put((1u32 << pad) - 1, pad);
        }
    }
}

/// A generated table in `DHT` form plus its encode lookup.
struct EncodeTable {
    counts: [u8; 16],
    values: Vec<u8>,
    /// `code[sym]`, `len[sym]`; `len == 0` means the symbol is not coded.
    code: [u32; 256],
    len: [u8; 256],
}

/// libjpeg's `jpeg_gen_optimal_table`: build a length-limited (<= 16 bit)
/// canonical Huffman table from symbol frequencies. Symbol 256 is reserved
/// with frequency 1 so the all-ones code is never assigned to a real symbol,
/// which some decoders cannot represent.
fn gen_optimal_table(freq_in: &[u64; 256]) -> Option<EncodeTable> {
    let mut freq = [0u64; 257];
    freq[..256].copy_from_slice(freq_in);
    freq[256] = 1;
    let mut codesize = [0u8; 257];
    let mut others = [-1i32; 257];

    loop {
        // Two least-frequent live entries; `<=` picks the highest index on a
        // tie, matching libjpeg so the generated tables are identical.
        let mut v1: i32 = -1;
        let mut c1 = u64::MAX;
        for (i, &f) in freq.iter().enumerate() {
            if f != 0 && f <= c1 {
                c1 = f;
                v1 = i32::try_from(i).ok()?;
            }
        }
        let mut v2: i32 = -1;
        let mut c2 = u64::MAX;
        for (i, &f) in freq.iter().enumerate() {
            if f != 0 && f <= c2 && i32::try_from(i).ok()? != v1 {
                c2 = f;
                v2 = i32::try_from(i).ok()?;
            }
        }
        if v2 < 0 {
            break;
        }
        let (mut a, mut b) = (usize::try_from(v1).ok()?, usize::try_from(v2).ok()?);
        freq[a] += freq[b];
        freq[b] = 0;
        codesize[a] = codesize[a].checked_add(1)?;
        while others[a] >= 0 {
            a = usize::try_from(others[a]).ok()?;
            codesize[a] = codesize[a].checked_add(1)?;
        }
        // Chain onto the END of v1's list, matching libjpeg's in-place walk.
        others[a] = v2;
        codesize[b] = codesize[b].checked_add(1)?;
        while others[b] >= 0 {
            b = usize::try_from(others[b]).ok()?;
            codesize[b] = codesize[b].checked_add(1)?;
        }
    }

    // Histogram of code lengths, then the classic length-limiting shuffle.
    let mut bits = [0u32; 33];
    for &cs in codesize.iter() {
        if cs != 0 {
            if usize::from(cs) > 32 {
                return None;
            }
            bits[usize::from(cs)] += 1;
        }
    }
    for i in (17..=32usize).rev() {
        while bits[i] > 0 {
            let mut j = i - 2;
            while bits[j] == 0 {
                j = j.checked_sub(1)?;
                if j == 0 {
                    return None;
                }
            }
            bits[i] -= 2;
            bits[i - 1] += 1;
            bits[j + 1] += 2;
            bits[j] -= 1;
        }
    }
    // Remove the reserved symbol: it holds the longest code by construction.
    let mut top = 16usize;
    while top > 0 && bits[top] == 0 {
        top -= 1;
    }
    if top == 0 {
        return None;
    }
    bits[top] -= 1;

    let mut counts = [0u8; 16];
    for l in 1..=16usize {
        counts[l - 1] = u8::try_from(bits[l]).ok()?;
    }
    // Symbol order is by the *unadjusted* code size, then by value: the
    // length-limiting shuffle above rewrites the length histogram but keeps
    // that ordering a valid canonical assignment (libjpeg does the same).
    let mut values: Vec<u8> = Vec::new();
    for l in 1..=32u8 {
        for (sym, &cs) in codesize.iter().enumerate().take(256) {
            if cs == l {
                values.push(u8::try_from(sym).ok()?);
            }
        }
    }
    if values.len() != bits[1..=16].iter().sum::<u32>() as usize {
        return None;
    }

    // Canonical code assignment over the (length, value) order just built.
    let mut code = [0u32; 256];
    let mut len = [0u8; 256];
    let mut next = 0u32;
    let mut k = 0usize;
    for l in 1..=16usize {
        for _ in 0..counts[l - 1] {
            let sym = usize::from(values[k]);
            code[sym] = next;
            len[sym] = u8::try_from(l).ok()?;
            next += 1;
            k += 1;
        }
        next <<= 1;
    }

    Some(EncodeTable {
        counts,
        values,
        code,
        len,
    })
}

/// Table slot for a `class << 4 | id` table selector: DC ids occupy 0..=3,
/// AC ids 4..=7. Both nibbles are range-checked before any selector is
/// built, so this is total.
fn slot_of(table: u8) -> usize {
    usize::from((table >> 4) * 4 + (table & 0xF))
}

/// Blocks-per-MCU layout, MCU count and spectral parameters for one scan.
struct ScanPlan {
    /// For each scan component: (frame component index, blocks per MCU,
    /// dc table, ac table).
    comps: Vec<(usize, usize, u8, u8)>,
    mcus: usize,
    /// Spectral selection and successive approximation from the `SOS` header.
    /// Sequential scans are always `0..=63` at `Ah = Al = 0`.
    ss: u8,
    se: u8,
    ah: u8,
}

fn ceil_div(a: usize, b: usize) -> Option<usize> {
    if b == 0 {
        return None;
    }
    Some(a.div_ceil(b))
}

/// Work out the MCU geometry a scan walks, from the frame and `SOS` header.
fn plan_scan(frame: &Frame, sos: &[u8]) -> Option<ScanPlan> {
    // SOS payload (after the 2 length bytes): Ns, (Cs Td|Ta)*Ns, Ss, Se, Ah|Al.
    let ns = usize::from(*sos.get(2)?);
    if ns == 0 || ns > 4 || sos.len() != 2 + 1 + ns * 2 + 3 {
        return None;
    }
    let (ss, se) = (*sos.get(2 + 1 + ns * 2)?, *sos.get(2 + 2 + ns * 2)?);
    let ahal = *sos.get(2 + 3 + ns * 2)?;
    let (ah, al) = (ahal >> 4, ahal & 0xF);

    if frame.progressive {
        // T.81 G.1.1.1.1: a DC scan is Ss = Se = 0 and may interleave; an AC
        // scan is a single component over a band of 1..=63. Al bounds the
        // point transform to a shift the 16-bit coefficient range can hold,
        // and Ah is either 0 (first pass) or exactly Al + 1 (refinement).
        if al > 13 || (ah != 0 && ah != al + 1) {
            return None;
        }
        if ss == 0 {
            if se != 0 {
                return None;
            }
        } else if ns != 1 || se < ss || se > 63 {
            return None;
        }
    } else if ss != 0 || se != 63 || ahal != 0 {
        // Sequential scans are always the full 0..=63 spectrum at zero shift.
        return None;
    }

    let hmax = usize::from(frame.comps.iter().map(|c| c.h).max()?);
    let vmax = usize::from(frame.comps.iter().map(|c| c.v).max()?);
    let x = usize::from(frame.x);
    let y = usize::from(frame.y);
    if x == 0 || y == 0 {
        return None;
    }

    let mut comps = Vec::with_capacity(ns);
    for i in 0..ns {
        let cs = *sos.get(3 + i * 2)?;
        let t = *sos.get(4 + i * 2)?;
        let idx = frame.ids.iter().position(|&id| id == cs)?;
        if comps.iter().any(|c: &(usize, usize, u8, u8)| c.0 == idx) {
            return None; // the same component twice in one scan
        }
        let (td, ta) = (t >> 4, t & 0xF);
        if td > 3 || ta > 3 {
            return None;
        }
        let c = frame.comps.get(idx)?;
        comps.push((idx, usize::from(c.h) * usize::from(c.v), td, ta));
    }

    let mcus = if ns == 1 {
        // Non-interleaved: one block per MCU, over the component's own grid.
        comps[0].1 = 1;
        *frame.blocks.get(comps[0].0)?
    } else {
        let mx = ceil_div(x, 8 * hmax)?;
        let my = ceil_div(y, 8 * vmax)?;
        mx.checked_mul(my)?
    };
    Some(ScanPlan {
        comps,
        mcus,
        ss,
        se,
        ah,
    })
}

/// Per-coefficient state a progressive frame carries between its scans: one
/// bit per coefficient of every block, set once that coefficient has become
/// nonzero. AC refinement scans need exactly this and nothing more — whether
/// a coefficient is already nonzero decides whether the next bit in the
/// stream is a correction bit for it — so a `u64` mask per block is the whole
/// state, at one eighth of a byte per pixel.
type Nonzero = Vec<Vec<u64>>;

/// One block's worth of an AC first pass (`Ah == 0`), following T.81 G.1.2.2.
fn decode_ac_first(
    reader: &mut BitReader,
    run: &mut Vec<Token>,
    ac: &DecodeTable,
    ta: u8,
    nz: &mut u64,
    plan: &ScanPlan,
    eobrun: &mut u32,
) -> Option<()> {
    if *eobrun > 0 {
        *eobrun -= 1;
        return Some(());
    }
    let mut k = usize::from(plan.ss);
    let se = usize::from(plan.se);
    while k <= se {
        let rs = reader.decode(ac)?;
        let (r, sz) = (rs >> 4, rs & 0xF);
        if sz != 0 {
            let extra = reader.bits(sz)?;
            run.push(Token::Sym {
                table: 0x10 | ta,
                sym: rs,
                bits: extra,
                nbits: sz,
            });
            k += usize::from(r);
            if k > se {
                return None;
            }
            *nz |= 1u64 << k;
            k += 1;
        } else if r != 15 {
            // EOB run: 2^r blocks, plus r additional bits, this one included.
            let extra = reader.bits(r)?;
            run.push(Token::Sym {
                table: 0x10 | ta,
                sym: rs,
                bits: extra,
                nbits: r,
            });
            *eobrun = (1u32 << r) + u32::from(extra) - 1;
            return Some(());
        } else {
            // ZRL: sixteen zero coefficients, no additional bits.
            run.push(Token::Sym {
                table: 0x10 | ta,
                sym: rs,
                bits: 0,
                nbits: 0,
            });
            k += 16;
        }
    }
    Some(())
}

/// One block's worth of an AC refinement pass (`Ah > 0`), following T.81
/// G.1.2.3. Coefficients already nonzero take a correction bit; newly coded
/// ones take a sign bit. Only the nonzero mask is tracked — the correction
/// bits themselves are copied through as raw bits.
fn decode_ac_refine(
    reader: &mut BitReader,
    run: &mut Vec<Token>,
    ac: &DecodeTable,
    ta: u8,
    nz: &mut u64,
    plan: &ScanPlan,
    eobrun: &mut u32,
) -> Option<()> {
    let se = usize::from(plan.se);
    let mut k = usize::from(plan.ss);
    if *eobrun == 0 {
        while k <= se {
            let rs = reader.decode(ac)?;
            let (mut r, sz) = (i32::from(rs >> 4), rs & 0xF);
            let mut newly_nonzero = false;
            if sz != 0 {
                // Only magnitude 1 can appear: refinement adds one bit plane,
                // so a coefficient can only ever cross into +-1 << Al.
                if sz != 1 {
                    return None;
                }
                run.push(Token::Sym {
                    table: 0x10 | ta,
                    sym: rs,
                    bits: 0,
                    nbits: 0,
                });
                push_raw_bit(run, reader.bit()?); // sign of the new coefficient
                newly_nonzero = true;
            } else if r != 15 {
                let extra = reader.bits(u8::try_from(r).ok()?)?;
                run.push(Token::Sym {
                    table: 0x10 | ta,
                    sym: rs,
                    bits: extra,
                    nbits: u8::try_from(r).ok()?,
                });
                *eobrun = (1u32 << r) + u32::from(extra);
                break;
            } else {
                run.push(Token::Sym {
                    table: 0x10 | ta,
                    sym: rs,
                    bits: 0,
                    nbits: 0,
                });
            }
            // Walk forward over `r` history-zero coefficients, taking a
            // correction bit for each already-nonzero one on the way.
            while k <= se {
                if *nz & (1u64 << k) != 0 {
                    push_raw_bit(run, reader.bit()?);
                } else {
                    r -= 1;
                    if r < 0 {
                        break;
                    }
                }
                k += 1;
            }
            if newly_nonzero {
                if k > se {
                    return None; // no room left for the coefficient just coded
                }
                *nz |= 1u64 << k;
            }
            k += 1;
        }
    }
    if *eobrun > 0 {
        // Inside an EOB run every remaining nonzero coefficient still takes a
        // correction bit; only the run/size symbols are suppressed.
        while k <= se {
            if *nz & (1u64 << k) != 0 {
                push_raw_bit(run, reader.bit()?);
            }
            k += 1;
        }
        *eobrun -= 1;
    }
    Some(())
}

/// Decode one scan's entropy-coded data into tokens, grouped by restart
/// interval. Returns the tokens and the offset of the byte just past the
/// scan's entropy data. `nonzero` carries progressive coefficient state
/// across the frame's scans and is unused for sequential frames.
fn decode_scan(
    data: &[u8],
    start: usize,
    plan: &ScanPlan,
    dri: usize,
    tables: &[Option<DecodeTable>; 8],
    progressive: bool,
    nonzero: &mut Nonzero,
) -> Option<(Vec<Vec<Token>>, usize)> {
    let mut reader = BitReader::new(data, start);
    let mut runs: Vec<Vec<Token>> = Vec::new();
    let mut run: Vec<Token> = Vec::new();
    let mut restarts = 0usize;
    let mut eobrun = 0u32;
    let ac_band = progressive && plan.ss != 0;

    for mcu in 0..plan.mcus {
        if dri > 0 && mcu > 0 && mcu % dri == 0 {
            runs.push(std::mem::take(&mut run));
            reader.restart(u8::try_from(restarts & 7).ok()?)?;
            restarts += 1;
            // T.81 G.1.2.2: EOB runs never cross a restart boundary.
            eobrun = 0;
        }
        if ac_band {
            // AC scans are single-component and non-interleaved, so the MCU
            // index is the block index in that component's own grid.
            let &(idx, _, _, ta) = plan.comps.first()?;
            let ac = tables[slot_of(0x10 | ta)].as_ref()?;
            let mut nz = *nonzero.get(idx)?.get(mcu)?;
            if plan.ah == 0 {
                decode_ac_first(&mut reader, &mut run, ac, ta, &mut nz, plan, &mut eobrun)?;
            } else {
                decode_ac_refine(&mut reader, &mut run, ac, ta, &mut nz, plan, &mut eobrun)?;
            }
            *nonzero.get_mut(idx)?.get_mut(mcu)? = nz;
            continue;
        }
        for &(_, blocks, td, ta) in &plan.comps {
            for _ in 0..blocks {
                if progressive && plan.ah != 0 {
                    // DC refinement: one raw bit per block, no Huffman at all.
                    push_raw_bit(&mut run, reader.bit()?);
                    continue;
                }
                let dc = tables[slot_of(td)].as_ref()?;
                let sym = reader.decode(dc)?;
                if sym > 15 {
                    return None;
                }
                let extra = reader.bits(sym)?;
                run.push(Token::Sym {
                    table: td,
                    sym,
                    bits: extra,
                    nbits: sym,
                });
                if progressive {
                    continue; // a progressive DC scan stops at the DC term
                }

                let ac = tables[slot_of(0x10 | ta)].as_ref()?;
                let mut k = 1usize;
                while k < 64 {
                    let rs = reader.decode(ac)?;
                    let (r, s) = (rs >> 4, rs & 0xF);
                    let extra = reader.bits(s)?;
                    run.push(Token::Sym {
                        table: 0x10 | ta,
                        sym: rs,
                        bits: extra,
                        nbits: s,
                    });
                    if s == 0 {
                        if r != 15 {
                            break; // EOB
                        }
                        k += 16;
                    } else {
                        k += usize::from(r) + 1;
                    }
                }
                if k > 64 {
                    return None;
                }
            }
        }
    }
    runs.push(run);
    // Everything after the final MCU must be pad bits up to the next marker.
    reader.cnt = 0;
    Some((runs, reader.pos))
}

/// Re-encode a decoded scan against freshly generated tables. Returns the
/// tables the scan needs (by `class * 4 + id` slot) and the entropy-coded
/// data; the caller assembles the `DHT` segment.
fn encode_scan(runs: &[Vec<Token>]) -> Option<(Vec<Option<EncodeTable>>, Vec<u8>)> {
    let mut freqs: [[u64; 256]; 8] = [[0; 256]; 8];
    let mut used = [false; 8];
    for run in runs {
        for t in run {
            let Token::Sym { table, sym, .. } = t else {
                continue; // raw refinement bits carry no symbol
            };
            let slot = slot_of(*table);
            freqs[slot][usize::from(*sym)] += 1;
            used[slot] = true;
        }
    }
    let mut tables: Vec<Option<EncodeTable>> = Vec::with_capacity(8);
    for (slot, &is_used) in used.iter().enumerate() {
        tables.push(if is_used {
            Some(gen_optimal_table(&freqs[slot])?)
        } else {
            None
        });
    }

    let mut w = BitWriter::default();
    let mut entropy: Vec<u8> = Vec::new();
    for (i, run) in runs.iter().enumerate() {
        if i > 0 {
            entropy.push(0xFF);
            entropy.push(0xD0 + u8::try_from((i - 1) & 7).ok()?);
        }
        w.out.clear();
        w.buf = 0;
        w.cnt = 0;
        for t in run {
            match *t {
                Token::Sym {
                    table,
                    sym,
                    bits,
                    nbits,
                } => {
                    let table = tables.get(slot_of(table))?.as_ref()?;
                    let len = table.len[usize::from(sym)];
                    if len == 0 {
                        return None;
                    }
                    w.put(table.code[usize::from(sym)], len);
                    if nbits > 0 {
                        w.put(u32::from(bits), nbits);
                    }
                }
                Token::Raw { bits, nbits } => w.put(bits, nbits),
            }
        }
        w.flush();
        entropy.extend_from_slice(&w.out);
    }
    Some((tables, entropy))
}

/// Assemble the `DHT` segment a scan needs, given the tables already in
/// effect. `in_effect` is updated to what the decoder will hold after this
/// segment. Returns an empty vector when nothing has to be (re)defined — a
/// progressive DC refinement scan codes no symbols at all.
fn dht_segment(
    tables: &[Option<EncodeTable>],
    in_effect: &mut [Option<(Box<[u8; 16]>, Vec<u8>)>; 8],
    dedupe: bool,
) -> Option<Vec<u8>> {
    let mut payload: Vec<u8> = Vec::new();
    for (slot, table) in tables.iter().enumerate() {
        let Some(t) = table else { continue };
        let fresh = (Box::new(t.counts), t.values.clone());
        if dedupe && in_effect.get(slot)?.as_ref() == Some(&fresh) {
            continue;
        }
        let class = u8::try_from(slot / 4).ok()?;
        let id = u8::try_from(slot % 4).ok()?;
        payload.push((class << 4) | id);
        payload.extend_from_slice(&t.counts);
        payload.extend_from_slice(&t.values);
        *in_effect.get_mut(slot)? = Some(fresh);
    }
    if payload.is_empty() {
        return Some(Vec::new());
    }
    let mut dht = vec![0xFF, 0xC4];
    dht.extend_from_slice(&u16::try_from(payload.len() + 2).ok()?.to_be_bytes());
    dht.extend_from_slice(&payload);
    Some(dht)
}

/// Parse a `DHT` segment payload into decode tables, slotted by
/// `class * 4 + id`.
fn read_dht(payload: &[u8], tables: &mut [Option<DecodeTable>; 8]) -> Option<()> {
    let mut i = 0usize;
    while i < payload.len() {
        let tc_th = *payload.get(i)?;
        let (class, id) = (tc_th >> 4, tc_th & 0xF);
        if class > 1 || id > 3 {
            return None;
        }
        let mut counts = [0u8; 16];
        counts.copy_from_slice(payload.get(i + 1..i + 17)?);
        let total: usize = counts.iter().map(|&c| usize::from(c)).sum();
        if total > 256 {
            return None;
        }
        let values = payload.get(i + 17..i + 17 + total)?.to_vec();
        tables[usize::from(class * 4 + id)] = Some(DecodeTable::build(&counts, values)?);
        i += 17 + total;
    }
    Some(())
}

/// The whole file decomposed into what the rebuild needs: the byte ranges to
/// copy verbatim and the decoded scans.
struct Parsed {
    /// Output segments in file order, already assembled except for scans.
    prefix: Vec<Vec<u8>>,
    scans: Vec<Scan>,
    /// Index into `prefix` after which each scan is written.
    scan_at: Vec<usize>,
    trailer: Vec<u8>,
    progressive: bool,
}

fn parse(data: &[u8]) -> Option<Parsed> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut prefix: Vec<Vec<u8>> = vec![vec![0xFF, 0xD8]];
    let mut scans: Vec<Scan> = Vec::new();
    let mut scan_at: Vec<usize> = Vec::new();
    let mut tables: [Option<DecodeTable>; 8] = Default::default();
    let mut frame: Option<Frame> = None;
    let mut nonzero: Nonzero = Vec::new();
    let mut dri = 0usize;
    let mut i = 2usize;

    loop {
        if *data.get(i)? != 0xFF {
            return None;
        }
        // Fill bytes: any number of 0xFF may precede a marker code.
        let mut m = *data.get(i + 1)?;
        let mut j = i + 1;
        while m == 0xFF {
            j += 1;
            m = *data.get(j)?;
        }
        let seg = j + 1;
        match m {
            0xD9 => {
                // EOI: everything after it ships unchanged.
                return Some(Parsed {
                    prefix,
                    scans,
                    scan_at,
                    trailer: data.get(seg..)?.to_vec(),
                    progressive: frame.is_some_and(|f| f.progressive),
                });
            }
            0x01 | 0xD0..=0xD7 => return None, // stray marker outside a scan
            0xC0 | 0xC1 | 0xC2 => {
                let len = usize::from(u16::from_be_bytes([*data.get(seg)?, *data.get(seg + 1)?]));
                let payload = data.get(seg + 2..seg + len)?;
                if frame.is_some() || *payload.first()? != 8 {
                    return None; // multiple frames, or not 8-bit
                }
                let y = u16::from_be_bytes([*payload.get(1)?, *payload.get(2)?]);
                let x = u16::from_be_bytes([*payload.get(3)?, *payload.get(4)?]);
                let nf = usize::from(*payload.get(5)?);
                if nf == 0 || nf > 4 || payload.len() != 6 + nf * 3 {
                    return None;
                }
                let mut comps = Vec::with_capacity(nf);
                let mut ids = Vec::with_capacity(nf);
                for c in 0..nf {
                    let id = *payload.get(6 + c * 3)?;
                    let hv = *payload.get(7 + c * 3)?;
                    let (h, v) = (hv >> 4, hv & 0xF);
                    if h == 0 || v == 0 || h > 4 || v > 4 || ids.contains(&id) {
                        return None;
                    }
                    ids.push(id);
                    comps.push(Component { h, v });
                }
                // Per-component block grids, and the coefficient state a
                // progressive frame's AC scans thread through.
                let hmax = usize::from(comps.iter().map(|c| c.h).max()?);
                let vmax = usize::from(comps.iter().map(|c| c.v).max()?);
                if x == 0 || y == 0 {
                    return None;
                }
                let mut blocks = Vec::with_capacity(nf);
                for c in &comps {
                    let bw = ceil_div(
                        ceil_div(usize::from(x) * usize::from(c.h), hmax)?,
                        8,
                    )?;
                    let bh = ceil_div(
                        ceil_div(usize::from(y) * usize::from(c.v), vmax)?,
                        8,
                    )?;
                    blocks.push(bw.checked_mul(bh)?);
                }
                let progressive = m == 0xC2;
                if progressive {
                    nonzero = blocks.iter().map(|&n| vec![0u64; n]).collect();
                }
                frame = Some(Frame {
                    x,
                    y,
                    comps,
                    ids,
                    blocks,
                    progressive,
                });
                prefix.push(data.get(i..seg + len)?.to_vec());
                i = seg + len;
            }
            // Anything else in the SOF family — lossless, arithmetic,
            // hierarchical — is out of scope.
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF | 0xCC | 0xDE | 0xDF => return None,
            0xC4 => {
                let len = usize::from(u16::from_be_bytes([*data.get(seg)?, *data.get(seg + 1)?]));
                read_dht(data.get(seg + 2..seg + len)?, &mut tables)?;
                // Dropped from the output: the rebuild emits its own.
                i = seg + len;
            }
            0xDD => {
                let len = usize::from(u16::from_be_bytes([*data.get(seg)?, *data.get(seg + 1)?]));
                if len != 4 {
                    return None;
                }
                dri = usize::from(u16::from_be_bytes([
                    *data.get(seg + 2)?,
                    *data.get(seg + 3)?,
                ]));
                prefix.push(data.get(i..seg + len)?.to_vec());
                i = seg + len;
            }
            0xDA => {
                let len = usize::from(u16::from_be_bytes([*data.get(seg)?, *data.get(seg + 1)?]));
                let header = data.get(seg..seg + len)?.to_vec();
                let frame = frame.as_ref()?;
                let plan = plan_scan(frame, &header)?;
                let (runs, end) = decode_scan(
                    data,
                    seg + len,
                    &plan,
                    dri,
                    &tables,
                    frame.progressive,
                    &mut nonzero,
                )?;
                scans.push(Scan { header, runs });
                scan_at.push(prefix.len());
                i = end;
                // The scan must be followed by a marker (RST markers are
                // consumed inside the scan, so this is a real one).
                if *data.get(i)? != 0xFF {
                    return None;
                }
            }
            _ => {
                let len = usize::from(u16::from_be_bytes([*data.get(seg)?, *data.get(seg + 1)?]));
                if len < 2 {
                    return None;
                }
                prefix.push(data.get(i..seg + len)?.to_vec());
                i = seg + len;
            }
        }
    }
}

fn rebuild(p: &Parsed) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut in_effect: [Option<(Box<[u8; 16]>, Vec<u8>)>; 8] = Default::default();
    let mut next_scan = 0usize;
    for (idx, seg) in p.prefix.iter().enumerate() {
        out.extend_from_slice(seg);
        while next_scan < p.scans.len() && p.scan_at[next_scan] == idx + 1 {
            let scan = &p.scans[next_scan];
            let (tables, entropy) = encode_scan(&scan.runs)?;
            let dht = dht_segment(&tables, &mut in_effect, p.progressive)?;
            out.extend_from_slice(&dht);
            out.extend_from_slice(&[0xFF, 0xDA]);
            out.extend_from_slice(&scan.header);
            out.extend_from_slice(&entropy);
            next_scan += 1;
        }
    }
    if next_scan != p.scans.len() {
        return None;
    }
    out.extend_from_slice(&[0xFF, 0xD9]);
    out.extend_from_slice(&p.trailer);
    Some(out)
}

/// Rebuild `data`'s Huffman tables from its own symbol statistics.
///
/// Returns the smaller, coefficient-identical stream, or `None` to mean
/// "keep the original bytes" — for anything out of scope (see the module
/// docs), any parse surprise, a non-shrinking result, or a round trip that
/// does not reproduce the exact input token sequence.
pub(crate) fn optimize(data: &[u8]) -> Option<Vec<u8>> {
    let parsed = parse(data)?;
    if parsed.scans.is_empty() {
        return None;
    }
    let out = rebuild(&parsed)?;
    if out.len() >= data.len() {
        return None;
    }
    // Verification gate: re-read the rebuilt stream and require the same
    // scan structure and the same tokens — the same Huffman symbols and the
    // same additional bits, hence the same DCT coefficients.
    let back = parse(&out)?;
    if back.scans.len() != parsed.scans.len() {
        return None;
    }
    for (a, b) in back.scans.iter().zip(&parsed.scans) {
        if a.header != b.header || a.runs.len() != b.runs.len() {
            return None;
        }
        if a.runs.iter().zip(&b.runs).any(|(x, y)| x != y) {
            return None;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Symbol frequencies must produce a table no code longer than 16 bits,
    /// with every used symbol assigned and the all-ones code left free.
    /// Assemble a baseline JPEG whose Huffman tables are deliberately flat:
    /// every DC symbol gets a 4-bit code and every AC symbol a 3-bit one,
    /// regardless of how often it occurs. Legal, decodable, and exactly the
    /// shape a producer that ships fixed tables emits.
    ///
    /// `dri` sets the restart interval in MCUs (0 = none). The image is
    /// `blocks * 8` pixels wide and 8 tall, single component, so one MCU is
    /// one block.
    fn flat_table_jpeg(blocks: usize, dri: usize) -> Vec<u8> {
        const DC_SYMS: usize = 12;
        const AC_VALUES: [u8; 4] = [0x00, 0x01, 0x11, 0x21];
        let dc_code = |sym: u8| (u32::from(sym), 4u8);
        let ac_code = |sym: u8| {
            (
                u32::try_from(AC_VALUES.iter().position(|&v| v == sym).unwrap()).unwrap(),
                3u8,
            )
        };

        let mut out: Vec<u8> = vec![0xFF, 0xD8];
        // DQT: one all-ones table (the pass never reads it; the decoder does).
        out.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
        out.extend_from_slice(&[1u8; 64]);
        if dri > 0 {
            out.extend_from_slice(&[0xFF, 0xDD, 0x00, 0x04]);
            out.extend_from_slice(&u16::try_from(dri).unwrap().to_be_bytes());
        }
        // SOF0: 8-bit, 8 rows, blocks*8 columns, one 1x1 component.
        out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x08]);
        out.extend_from_slice(&u16::try_from(blocks * 8).unwrap().to_be_bytes());
        // Nf = 1; component id 1, sampling 1x1, quant table 0.
        out.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
        // DHT: DC table 0 (12 four-bit codes), AC table 0 (4 three-bit codes).
        let mut dht: Vec<u8> = vec![0x00];
        dht.extend_from_slice(&[0, 0, 0, u8::try_from(DC_SYMS).unwrap(), 0, 0, 0, 0]);
        dht.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        dht.extend((0..u8::try_from(DC_SYMS).unwrap()).collect::<Vec<u8>>());
        dht.push(0x10);
        dht.extend_from_slice(&[0, 0, 4, 0, 0, 0, 0, 0]);
        dht.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        dht.extend_from_slice(&AC_VALUES);
        out.extend_from_slice(&[0xFF, 0xC4]);
        out.extend_from_slice(&u16::try_from(dht.len() + 2).unwrap().to_be_bytes());
        out.extend_from_slice(&dht);
        // SOS: one component, DC/AC table 0, full spectrum.
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);

        // Entropy data: a strongly skewed symbol mix, so optimal tables are
        // much shorter than the flat ones above.
        let mut w = BitWriter::default();
        let mut restarts = 0usize;
        for b in 0..blocks {
            if dri > 0 && b > 0 && b % dri == 0 {
                w.flush();
                out.extend_from_slice(&w.out);
                w.out.clear();
                out.push(0xFF);
                out.push(0xD0 + u8::try_from(restarts & 7).unwrap());
                restarts += 1;
            }
            // DC: magnitude 1 (one additional bit) on all but every 7th block.
            let (c, l) = if b % 7 == 0 { dc_code(0) } else { dc_code(1) };
            w.put(c, l);
            if b % 7 != 0 {
                w.put(u32::try_from(b & 1).unwrap(), 1);
            }
            // AC: one (run 1, size 1) coefficient on every third block, then
            // end-of-block.
            if b % 3 == 0 {
                let (c, l) = ac_code(0x11);
                w.put(c, l);
                w.put(1, 1);
            }
            let (c, l) = ac_code(0x00);
            w.put(c, l);
        }
        w.flush();
        out.extend_from_slice(&w.out);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    /// End to end on a synthetic baseline JPEG: the rebuild is smaller, and
    /// an independent decoder (the `image` crate) sees the same pixels.
    #[test]
    fn flat_tables_shrink_and_pixels_are_identical() {
        for dri in [0usize, 5] {
            let jpeg = flat_table_jpeg(40, dri);
            let out = optimize(&jpeg).expect("flat tables must be improvable");
            assert!(
                out.len() < jpeg.len(),
                "dri {dri}: {} vs {}",
                out.len(),
                jpeg.len()
            );
            let a = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
                .expect("original decodes");
            let b = image::load_from_memory_with_format(&out, image::ImageFormat::Jpeg)
                .expect("rebuild decodes");
            assert_eq!(a.to_rgb8().into_raw(), b.to_rgb8().into_raw(), "dri {dri}");
        }
    }

    /// Running the pass on its own output must be a fixpoint: the tables are
    /// already optimal, so there is nothing left to win and the stream is
    /// left byte-identical.
    #[test]
    fn optimized_output_is_a_fixpoint() {
        let jpeg = flat_table_jpeg(40, 0);
        let once = optimize(&jpeg).unwrap();
        assert!(optimize(&once).is_none(), "second pass must decline");
    }

    /// Progressive frames are out of scope and must leave the bytes alone.
    #[test]
    fn progressive_declines() {
        let mut jpeg = flat_table_jpeg(8, 0);
        // Retag SOF0 as SOF2 (the entropy data is then nonsense, which is
        // the point: the frame type alone must stop the pass).
        let at = jpeg
            .windows(2)
            .position(|w| w == [0xFF, 0xC0])
            .expect("SOF0");
        jpeg[at + 1] = 0xC2;
        assert!(optimize(&jpeg).is_none());
    }

    #[test]
    fn optimal_table_is_length_limited_and_complete() {
        let mut freq = [0u64; 256];
        for (i, f) in freq.iter_mut().enumerate() {
            // Geometric spread steep enough that the unlimited Huffman code
            // runs past 16 bits, so the length-limiting shuffle has to act.
            *f = 1u64 << (i % 20);
        }
        let t = gen_optimal_table(&freq).unwrap();
        assert!(t.len.iter().all(|&l| l <= 16));
        assert!(t.len.iter().all(|&l| l != 0));
        assert_eq!(
            t.values.len(),
            t.counts.iter().map(|&c| usize::from(c)).sum::<usize>()
        );
        // Kraft sum over the real symbols must stay under 1: the reserved
        // 257th symbol keeps the all-ones code out of the alphabet.
        let kraft: f64 = t.len.iter().map(|&l| 0.5f64.powi(i32::from(l))).sum();
        assert!(kraft < 1.0, "kraft {kraft}");
    }

    #[test]
    fn single_symbol_table_is_valid() {
        let mut freq = [0u64; 256];
        freq[0] = 100;
        let t = gen_optimal_table(&freq).unwrap();
        assert_eq!(t.len[0], 1);
        assert_eq!(t.values, vec![0]);
    }

    /// Generated tables must round-trip: symbols encoded with the `code`/`len`
    /// lookup decode back through a `DecodeTable` built from the same
    /// `BITS`/`HUFFVAL` the `DHT` segment would carry.
    #[test]
    fn generated_table_round_trips() {
        let mut state = 0x1234_5678u32;
        let mut rnd = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        for trial in 0..40 {
            let mut freq = [0u64; 256];
            let live = 1 + (trial * 7) % 200;
            let mut syms: Vec<u8> = Vec::new();
            for _ in 0..4000 {
                let s = u8::try_from(rnd() as usize % live).unwrap();
                freq[usize::from(s)] += 1;
                syms.push(s);
            }
            let t = gen_optimal_table(&freq).unwrap();
            let mut w = BitWriter::default();
            for &s in &syms {
                assert_ne!(t.len[usize::from(s)], 0, "unassigned symbol");
                w.put(t.code[usize::from(s)], t.len[usize::from(s)]);
            }
            w.flush();
            let d = match DecodeTable::build(&t.counts, t.values.clone()) {
                Some(d) => d,
                None => panic!(
                    "trial {trial} live {live} counts {:?} values {}",
                    t.counts,
                    t.values.len()
                ),
            };
            let mut r = BitReader::new(&w.out, 0);
            for &s in &syms {
                assert_eq!(r.decode(&d).unwrap(), s, "trial {trial}");
            }
        }
    }

    #[test]
    fn bit_writer_stuffs_ff() {
        let mut w = BitWriter::default();
        w.put(0xFF, 8);
        w.flush();
        assert_eq!(w.out, vec![0xFF, 0x00]);
    }

    /// Corpus harness: point `AMATL_JPEG_DIR` at a directory of raw JPEG
    /// streams (e.g. every `/DCTDecode` payload extracted from amatl's own
    /// output) and this reports the aggregate saving and, when `jpegtran` is
    /// on PATH, how the rebuilt stream compares to it. Ignored by default —
    /// it needs an external corpus.
    #[test]
    #[ignore = "needs AMATL_JPEG_DIR corpus"]
    fn corpus_report() {
        let dir =
            std::env::var("AMATL_JPEG_DIR").unwrap_or_else(|_| "target/scratch/h5/jpg".into());
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        entries.sort();
        let (mut cur, mut new, mut done, mut skipped) = (0usize, 0usize, 0usize, 0usize);
        for path in entries {
            let data = std::fs::read(&path).unwrap();
            cur += data.len();
            match optimize(&data) {
                Some(out) => {
                    println!(
                        "{:>9} -> {:>9} ({:+}) {}",
                        data.len(),
                        out.len(),
                        out.len() as i64 - data.len() as i64,
                        path.display()
                    );
                    new += out.len();
                    done += 1;
                }
                None => {
                    new += data.len();
                    skipped += 1;
                }
            }
        }
        println!(
            "streams {done} optimized, {skipped} declined; {cur} -> {new} (save {})",
            cur - new
        );
    }

    #[test]
    fn garbage_declines() {
        assert!(optimize(b"").is_none());
        assert!(optimize(b"\xFF\xD8\xFF\xD9").is_none());
        assert!(optimize(&[0xFFu8; 512]).is_none());
    }
}
