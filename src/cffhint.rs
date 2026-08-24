//! Type2 (CFF / `Type1C`) hint stripping — the CFF analogue of
//! [`crate::truetype::strip_hinting`].
//!
//! A Type2 charstring carries two interleaved programs: the *outline*
//! (movetos, linetos, curvetos) and the *hints* (`hstem`, `vstem`,
//! `hstemhm`, `vstemhm`, and the `hintmask`/`cntrmask` hint-substitution
//! masks). Hints exist only to guide a rasterizer's grid fitting at small
//! ppem; the outline they annotate is complete without them. Dropping them
//! is the same trade `--strip-hinting` already makes for TrueType, and it is
//! gated behind that same flag.
//!
//! ## What this does, and does not, rewrite
//!
//! The rewrite is *token-level*, not interpretive: each charstring is walked
//! byte by byte, the hint operators and the operands that feed them are
//! deleted, the leading width operand is re-folded onto whatever
//! stack-clearing operator survives first, and every remaining byte — every
//! coordinate delta, in its original integer encoding — is copied verbatim.
//! No outline is re-encoded, no subroutine is inlined or dropped, no
//! coordinate is recomputed. That is deliberate: it makes outline identity a
//! near-syntactic property rather than something that rests on a re-encoder's
//! rounding.
//!
//! ## Preconditions (anything else declines, fail-safe)
//!
//! * The font is a plain (non-CID) Type2 CFF with one font in its Name INDEX.
//! * No reachable subroutine contains a hint operator. Hints inside subrs
//!   would leave stem counts — and hence `hintmask` operand sizes — spread
//!   across call boundaries that this in-place rewrite does not follow. (The
//!   hunt-4 probe's two arxiv failures were exactly this shape.)
//! * At every hint operator, the operand stack was built only by literal
//!   number tokens in the same charstring since the last stack-clearing
//!   operator. A subroutine call anywhere in between poisons the stack for
//!   this purpose and declines that glyph — the font still ships, with that
//!   one charstring untouched.
//!
//! ## Verification
//!
//! Stripping is not trusted on its own. Every glyph is traced before and
//! after by [`trace`], a Type2 interpreter that follows `callsubr`/
//! `callgsubr` and emits the width plus the sequence of drawing operators
//! with their resolved operands; the strip is kept only if every glyph's
//! trace is unchanged. The whole re-emitted font is then re-parsed and traced
//! again from its own bytes, so what ships — not just what was computed — is
//! what was verified.

use crate::cffmerge::{be16, dict_get, parse_dict, read_index, DictEntry};
use crate::type1::{cff_index, dict_int32, dict_op};

// Type2 operators this module needs to name.
const OP_HSTEM: u16 = 1;
const OP_VSTEM: u16 = 3;
const OP_VMOVETO: u16 = 4;
const OP_CALLSUBR: u16 = 10;
const OP_RETURN: u16 = 11;
const OP_ENDCHAR: u16 = 14;
const OP_HSTEMHM: u16 = 18;
const OP_HINTMASK: u16 = 19;
const OP_CNTRMASK: u16 = 20;
const OP_RMOVETO: u16 = 21;
const OP_HMOVETO: u16 = 22;
const OP_VSTEMHM: u16 = 23;
const OP_CALLGSUBR: u16 = 29;
const OP_DOTSECTION: u16 = 0x0C00;

/// True for the operators this pass deletes.
fn is_hint(op: u16) -> bool {
    matches!(
        op,
        OP_HSTEM | OP_VSTEM | OP_HSTEMHM | OP_VSTEMHM | OP_HINTMASK | OP_CNTRMASK | OP_DOTSECTION
    )
}

/// Operand count of the stack-clearing operators that may carry a width, or
/// `None` when the operator takes a variable number of stem pairs.
fn width_nargs(op: u16) -> Option<usize> {
    match op {
        OP_RMOVETO => Some(2),
        OP_HMOVETO | OP_VMOVETO => Some(1),
        _ => None,
    }
}

/// True when `op` clears the operand stack (T.81's Type2 counterpart: the
/// hint operators, the movetos, and `endchar`).
fn clears_stack(op: u16) -> bool {
    is_hint(op)
        || matches!(op, OP_RMOVETO | OP_HMOVETO | OP_VMOVETO | OP_ENDCHAR)
        // Path-construction operators also consume everything on the stack.
        || matches!(op, 5..=8 | 24..=27 | 30 | 31)
        // hflex, flex, hflex1, flex1.
        || matches!(op, 0x0C22..=0x0C25)
}

/// A charstring token: a literal number, an operator, or a `hintmask` /
/// `cntrmask` mask operand. Numbers and operators carry their byte range in
/// the charstring — the strip splices spans, it never re-encodes a value.
enum Tok {
    Num((usize, usize)),
    Op(u16, (usize, usize)),
    Mask,
}

/// Read the number token starting at `cs[i]`, returning its value and the
/// offset just past it. Callers have already established `cs[i]` starts one.
fn read_number(cs: &[u8], i: usize) -> Option<(f64, usize)> {
    let b = *cs.get(i)?;
    Some(match b {
        28 => (
            f64::from(i16::from_be_bytes([*cs.get(i + 1)?, *cs.get(i + 2)?])),
            i + 3,
        ),
        32..=246 => (f64::from(i32::from(b) - 139), i + 1),
        247..=250 => (
            f64::from((i32::from(b) - 247) * 256 + i32::from(*cs.get(i + 1)?) + 108),
            i + 2,
        ),
        251..=254 => (
            f64::from(-(i32::from(b) - 251) * 256 - i32::from(*cs.get(i + 1)?) - 108),
            i + 2,
        ),
        255 => (
            f64::from(i32::from_be_bytes([
                *cs.get(i + 1)?,
                *cs.get(i + 2)?,
                *cs.get(i + 3)?,
                *cs.get(i + 4)?,
            ])) / 65536.0,
            i + 5,
        ),
        _ => return None,
    })
}

/// Tokenize one charstring. `stems` is threaded in and out so `hintmask`
/// operand sizes are known; the count is exact only when the caller has
/// established that no operand ever arrives from a subroutine (see the module
/// docs), which is why this is private to the strip path.
fn tokenize(cs: &[u8]) -> Option<Vec<Tok>> {
    let mut out = Vec::new();
    let mut stems = 0usize;
    let mut pending = 0usize;
    let mut i = 0usize;
    while i < cs.len() {
        let b = cs[i];
        if b >= 32 || b == 28 {
            let (_, next) = read_number(cs, i)?;
            out.push(Tok::Num((i, next)));
            pending += 1;
            i = next;
            continue;
        }
        let (op, next) = if b == 12 {
            (0x0C00 | u16::from(*cs.get(i + 1)?), i + 2)
        } else {
            (u16::from(b), i + 1)
        };
        out.push(Tok::Op(op, (i, next)));
        let mut after = next;
        match op {
            OP_HSTEM | OP_VSTEM | OP_HSTEMHM | OP_VSTEMHM => {
                stems += pending / 2;
                pending = 0;
            }
            OP_HINTMASK | OP_CNTRMASK => {
                // Operands still on the stack are an implicit `vstem`.
                stems += pending / 2;
                pending = 0;
                let mask = stems.div_ceil(8).max(1);
                if next + mask > cs.len() {
                    return None;
                }
                out.push(Tok::Mask);
                after = next + mask;
            }
            _ => {
                if clears_stack(op) {
                    pending = 0;
                } else if op == OP_CALLSUBR || op == OP_CALLGSUBR {
                    pending = pending.saturating_sub(1);
                } else {
                    // Any other operator (arithmetic, storage, `random`, …)
                    // makes the stack unpredictable for this simple model.
                    return None;
                }
            }
        }
        i = after;
    }
    Some(out)
}

/// One item of a glyph's drawing trace: an operator and its resolved
/// operands. Hint operators never appear — they are exactly what the strip
/// removes, so including them would make the comparison vacuous.
#[derive(PartialEq, Debug)]
struct TraceItem(u16, Vec<f64>);

/// A glyph's verification fingerprint: its advance width (as the charstring
/// expresses it — `None` means "the Private DICT default") and its drawing
/// operators in order.
#[derive(PartialEq, Debug)]
pub(crate) struct Trace {
    width: Option<f64>,
    items: Vec<TraceItem>,
}

/// Bias applied to a `callsubr`/`callgsubr` index (Type2 §4.7).
fn bias(n: usize) -> i32 {
    if n < 1240 {
        107
    } else if n < 33900 {
        1131
    } else {
        32768
    }
}

/// Interpret a charstring, following subroutine calls, and record the width
/// and every drawing operator with its operands.
///
/// This is the verification oracle: two charstrings with equal traces draw
/// the same outline with the same advance. `None` on anything the model does
/// not cover (arithmetic/storage operators, an unresolvable subroutine index,
/// runaway recursion), which declines the glyph rather than guessing.
fn trace(cs: &[u8], lsubrs: &[Vec<u8>], gsubrs: &[Vec<u8>]) -> Option<Trace> {
    const MAX_DEPTH: usize = 10;
    const MAX_STEPS: usize = 200_000;

    let mut frames: Vec<(&[u8], usize)> = vec![(cs, 0)];
    let mut stack: Vec<f64> = Vec::new();
    let mut items: Vec<TraceItem> = Vec::new();
    let mut width: Option<f64> = None;
    let mut first_clear = true;
    let mut stems = 0usize;
    let mut steps = 0usize;

    while let Some(&mut (code, ref mut pos)) = frames.last_mut() {
        if *pos >= code.len() {
            frames.pop();
            continue;
        }
        steps += 1;
        if steps > MAX_STEPS {
            return None;
        }
        let i = *pos;
        let b = code[i];
        if b >= 32 || b == 28 {
            let (v, next) = read_number(code, i)?;
            stack.push(v);
            *pos = next;
            continue;
        }
        let (op, next) = if b == 12 {
            (0x0C00 | u16::from(*code.get(i + 1)?), i + 2)
        } else {
            (u16::from(b), i + 1)
        };
        *pos = next;

        match op {
            OP_CALLSUBR | OP_CALLGSUBR => {
                let subrs = if op == OP_CALLSUBR { lsubrs } else { gsubrs };
                let idx = stack.pop()?;
                let idx = i32::try_from(idx as i64).ok()? + bias(subrs.len());
                let sub = subrs.get(usize::try_from(idx).ok()?)?;
                if frames.len() >= MAX_DEPTH {
                    return None;
                }
                frames.push((sub.as_slice(), 0));
            }
            OP_RETURN => {
                frames.pop();
            }
            _ => {
                // Fold the width off the first stack-clearing operator.
                if clears_stack(op) && first_clear {
                    first_clear = false;
                    let has_width = match op {
                        OP_ENDCHAR => !matches!(stack.len(), 0 | 4),
                        _ => match width_nargs(op) {
                            Some(n) => stack.len() > n,
                            // Hint operators take stem pairs: an odd count
                            // means the first operand is the width.
                            None if is_hint(op) => stack.len() % 2 == 1,
                            None => return None,
                        },
                    };
                    if has_width {
                        if stack.is_empty() {
                            return None;
                        }
                        width = Some(stack.remove(0));
                    }
                }
                if is_hint(op) {
                    if matches!(op, OP_HINTMASK | OP_CNTRMASK) {
                        stems += stack.len() / 2;
                        let mask = stems.div_ceil(8).max(1);
                        let (code, pos) = frames.last_mut()?;
                        if *pos + mask > code.len() {
                            return None;
                        }
                        *pos += mask;
                    } else {
                        stems += stack.len() / 2;
                    }
                    stack.clear();
                } else if clears_stack(op) {
                    items.push(TraceItem(op, std::mem::take(&mut stack)));
                    if op == OP_ENDCHAR {
                        break;
                    }
                } else {
                    // Nothing else is modelled; decline instead of guessing.
                    return None;
                }
            }
        }
    }
    Some(Trace { width, items })
}

/// Strip the hint operators from one charstring.
///
/// `Some(bytes)` is a rewritten charstring; `None` means "keep this glyph's
/// original bytes" — the glyph is declined, not the font.
fn strip_charstring(cs: &[u8]) -> Option<Vec<u8>> {
    let toks = tokenize(cs)?;
    let mut out: Vec<u8> = Vec::with_capacity(cs.len());
    // Byte span of the width operand, if the first stack-clearing operator
    // turns out to be a hint operator that carries one.
    let mut width_span: Option<(usize, usize)> = None;
    let mut pending: Vec<(usize, usize)> = Vec::new();
    let mut first_clear = true;
    // True once a subroutine call has made the operand stack unknowable.
    let mut foreign = false;
    let mut changed = false;

    let mut k = 0usize;
    while k < toks.len() {
        match &toks[k] {
            Tok::Num(span) => {
                pending.push(*span);
                k += 1;
            }
            Tok::Mask => return None, // only reachable right after its operator
            Tok::Op(op, span) => {
                let op = *op;
                if op == OP_CALLSUBR || op == OP_CALLGSUBR {
                    // The subroutine consumes its index and may leave
                    // anything behind: everything pending is emitted as-is
                    // and the stack is no longer ours to reason about.
                    for s in pending.drain(..) {
                        out.extend_from_slice(&cs[s.0..s.1]);
                    }
                    out.extend_from_slice(&cs[span.0..span.1]);
                    foreign = true;
                    k += 1;
                    continue;
                }
                let is_hint_op = is_hint(op);
                if is_hint_op && foreign {
                    return None; // cannot prove which operands are the stem args
                }
                if clears_stack(op) && first_clear {
                    first_clear = false;
                    if is_hint_op && pending.len() % 2 == 1 {
                        // Odd stem-pair count: the leading operand is the
                        // width. It has to survive the strip.
                        width_span = Some(pending[0]);
                    }
                }
                if is_hint_op {
                    // Drop the operator, its operands, and any mask bytes.
                    pending.clear();
                    changed = true;
                    k += 1;
                    if matches!(op, OP_HINTMASK | OP_CNTRMASK) {
                        if !matches!(toks.get(k), Some(Tok::Mask)) {
                            return None;
                        }
                        k += 1;
                    }
                    continue;
                }
                for s in pending.drain(..) {
                    out.extend_from_slice(&cs[s.0..s.1]);
                }
                out.extend_from_slice(&cs[span.0..span.1]);
                k += 1;
            }
        }
    }
    for s in pending.drain(..) {
        out.extend_from_slice(&cs[s.0..s.1]);
    }
    if !changed {
        return None;
    }
    if let Some(w) = width_span {
        let mut with_width = Vec::with_capacity(out.len() + (w.1 - w.0));
        with_width.extend_from_slice(&cs[w.0..w.1]);
        with_width.extend_from_slice(&out);
        out = with_width;
    }
    Some(out)
}

/// True when a subroutine's byte stream contains a hint operator. Deliberately
/// naive — a `hintmask` mask byte that happens to look like an operator can
/// only produce a false *positive*, which declines the font.
fn subr_has_hints(sub: &[u8]) -> bool {
    let mut i = 0usize;
    while i < sub.len() {
        let b = sub[i];
        if b >= 32 || b == 28 {
            match read_number(sub, i) {
                Some((_, next)) => i = next,
                None => return true,
            }
            continue;
        }
        let (op, next) = if b == 12 {
            match sub.get(i + 1) {
                Some(&b1) => (0x0C00 | u16::from(b1), i + 2),
                None => return true,
            }
        } else {
            (u16::from(b), i + 1)
        };
        if is_hint(op) {
            return true;
        }
        i = next;
    }
    false
}

// Top / Private DICT operators.
const OP_CHARSET: u16 = 15;
const OP_ENCODING: u16 = 16;
const OP_CHARSTRINGS: u16 = 17;
const OP_PRIVATE: u16 = 18;
const OP_SUBRS: u16 = 19;
const OP_CHARSTRING_TYPE: u16 = 0x0C06;
const OP_ROS: u16 = 0x0C1E;
const OP_FD_ARRAY: u16 = 0x0C24;
const OP_FD_SELECT: u16 = 0x0C25;

/// Private DICT operators that carry nothing but hinting parameters:
/// `BlueValues`, `OtherBlues`, `FamilyBlues`, `FamilyOtherBlues`, `StdHW`,
/// `StdVW`, `BlueScale`, `BlueShift`, `BlueFuzz`, `StemSnapH`, `StemSnapV`,
/// `ForceBold`, `ExpansionFactor`.
const PRIVATE_HINT_OPS: [u16; 13] = [
    6, 7, 8, 9, 10, 11, 0x0C09, 0x0C0A, 0x0C0B, 0x0C0C, 0x0C0D, 0x0C0E, 0x0C12,
];

/// Byte length of the charset table at `at` for a font with `n_glyphs`
/// glyphs.
fn charset_len(data: &[u8], at: usize, n_glyphs: usize) -> Option<usize> {
    match *data.get(at)? {
        0 => Some(1 + (n_glyphs - 1) * 2),
        fmt @ (1 | 2) => {
            let step = if fmt == 1 { 3 } else { 4 };
            let mut covered = 1usize; // .notdef is implicit
            let mut i = at + 1;
            while covered < n_glyphs {
                let n_left = if fmt == 1 {
                    usize::from(*data.get(i + 2)?)
                } else {
                    usize::from(be16(data, i + 2)?)
                };
                covered = covered.checked_add(n_left + 1)?;
                i += step;
            }
            Some(i - at)
        }
        _ => None,
    }
}

/// Byte length of the custom encoding table at `at`.
fn encoding_len(data: &[u8], at: usize) -> Option<usize> {
    let fmt = *data.get(at)?;
    let n = usize::from(*data.get(at + 1)?);
    let base = match fmt & 0x7F {
        0 => 2 + n,
        1 => 2 + n * 2,
        _ => return None,
    };
    if fmt & 0x80 == 0 {
        return Some(base);
    }
    let n_sups = usize::from(*data.get(at + base)?);
    Some(base + 1 + n_sups * 3)
}

/// Everything the re-emitter needs from the input font.
struct Font {
    name: Vec<u8>,
    top: Vec<DictEntry>,
    top_raw: Vec<u8>,
    strings: Vec<Vec<u8>>,
    gsubrs: Vec<Vec<u8>>,
    lsubrs: Vec<Vec<u8>>,
    charstrings: Vec<Vec<u8>>,
    charset: Vec<u8>,
    encoding: Vec<u8>,
    /// Private DICT entries with their raw bytes, minus `Subrs`.
    private: Vec<DictEntry>,
    private_raw: Vec<u8>,
    has_lsubrs: bool,
}

fn parse(data: &[u8]) -> Option<Font> {
    if *data.first()? != 1 {
        return None;
    }
    let hdr = usize::from(*data.get(2)?);
    let (names, pos) = read_index(data, hdr)?;
    if names.len() != 1 {
        return None;
    }
    let (tops, pos) = read_index(data, pos)?;
    if tops.len() != 1 {
        return None;
    }
    let (strings, pos) = read_index(data, pos)?;
    let (gsubrs, _) = read_index(data, pos)?;
    let top = parse_dict(&tops[0])?;

    // CID-keyed fonts carry per-FD Private DICTs and a different charset
    // meaning; out of scope.
    if dict_get(&top, OP_ROS).is_some()
        || dict_get(&top, OP_FD_ARRAY).is_some()
        || dict_get(&top, OP_FD_SELECT).is_some()
    {
        return None;
    }
    if let Some(e) = dict_get(&top, OP_CHARSTRING_TYPE) {
        if e.operands != [2.0] {
            return None;
        }
    }

    let charstrings_at = match dict_get(&top, OP_CHARSTRINGS)?.operands.as_slice() {
        [v] if *v >= 0.0 => *v as usize,
        _ => return None,
    };
    let (charstrings, _) = read_index(data, charstrings_at)?;
    if charstrings.is_empty() {
        return None;
    }

    // Charset and built-in encoding are copied verbatim; predefined ones
    // (operand < 3 / < 2) have no table to copy and keep their operand.
    let charset = match dict_get(&top, OP_CHARSET) {
        None => Vec::new(),
        Some(e) => match e.operands.as_slice() {
            [v] if *v >= 3.0 => {
                let at = *v as usize;
                data.get(at..at + charset_len(data, at, charstrings.len())?)?
                    .to_vec()
            }
            [v] if *v >= 0.0 => Vec::new(),
            _ => return None,
        },
    };
    let encoding = match dict_get(&top, OP_ENCODING) {
        None => Vec::new(),
        Some(e) => match e.operands.as_slice() {
            [v] if *v >= 2.0 => {
                let at = *v as usize;
                data.get(at..at + encoding_len(data, at)?)?.to_vec()
            }
            [v] if *v >= 0.0 => Vec::new(),
            _ => return None,
        },
    };

    let (private_size, private_at) = match dict_get(&top, OP_PRIVATE)?.operands.as_slice() {
        [size, at] if *size >= 0.0 && *at >= 0.0 => (*size as usize, *at as usize),
        _ => return None,
    };
    let private_raw = data
        .get(private_at..private_at.checked_add(private_size)?)?
        .to_vec();
    let private = parse_dict(&private_raw)?;
    let mut lsubrs = Vec::new();
    let mut has_lsubrs = false;
    if let Some(e) = dict_get(&private, OP_SUBRS) {
        let rel = match e.operands.as_slice() {
            [v] if *v >= 0.0 => *v as usize,
            _ => return None,
        };
        lsubrs = read_index(data, private_at.checked_add(rel)?)?.0;
        has_lsubrs = true;
    }

    Some(Font {
        name: names[0].clone(),
        top,
        top_raw: tops[0].clone(),
        strings,
        gsubrs,
        lsubrs,
        charstrings,
        charset,
        encoding,
        private,
        private_raw,
        has_lsubrs,
    })
}

/// Re-emit a font with new charstrings (and optionally a Private DICT with
/// its hinting parameters removed). Every other table is byte-spliced.
fn emit(font: &Font, charstrings: &[Vec<u8>], drop_private_hints: bool) -> Option<Vec<u8>> {
    // Private DICT: the original entries verbatim, minus `Subrs` (re-added
    // with a fixed-width offset) and minus the hint keys when asked.
    let mut private: Vec<u8> = Vec::with_capacity(font.private_raw.len());
    for e in &font.private {
        if e.op == OP_SUBRS || (drop_private_hints && PRIVATE_HINT_OPS.contains(&e.op)) {
            continue;
        }
        private.extend_from_slice(&font.private_raw[e.span.0..e.span.1]);
    }
    if font.has_lsubrs {
        let subrs_at = private.len() + 5 + 1;
        dict_int32(&mut private, u32::try_from(subrs_at).ok()?);
        dict_op(&mut private, OP_SUBRS);
    }

    // Top DICT: everything except the four offset operators, spliced
    // verbatim, then those four re-encoded at fixed width.
    let mut top_prefix: Vec<u8> = Vec::new();
    let mut has_charset = false;
    let mut has_encoding = false;
    for e in &font.top {
        match e.op {
            OP_CHARSET => has_charset = true,
            OP_ENCODING => has_encoding = true,
            OP_CHARSTRINGS | OP_PRIVATE => {}
            _ => {
                top_prefix.extend_from_slice(&font.top_raw[e.span.0..e.span.1]);
                continue;
            }
        }
    }
    // A predefined charset/encoding has no table: keep the original operand
    // instead of pointing at bytes that do not exist.
    let charset_operand = (!font.charset.is_empty()).then_some(());
    let encoding_operand = (!font.encoding.is_empty()).then_some(());
    let predefined = |op: u16| -> Option<Vec<u8>> {
        let e = dict_get(&font.top, op)?;
        Some(font.top_raw[e.span.0..e.span.1].to_vec())
    };
    if has_charset && charset_operand.is_none() {
        top_prefix.extend_from_slice(&predefined(OP_CHARSET)?);
    }
    if has_encoding && encoding_operand.is_none() {
        top_prefix.extend_from_slice(&predefined(OP_ENCODING)?);
    }

    let n_offsets = usize::from(has_charset && charset_operand.is_some())
        + usize::from(has_encoding && encoding_operand.is_some());
    let top_dict_len = top_prefix.len() + (5 + 1) * (n_offsets + 1) + (5 + 5 + 1);

    let charstrings_index = cff_index(charstrings)?;
    let lsubr_index = cff_index(&font.lsubrs)?;
    let gsubr_index = cff_index(&font.gsubrs)?;
    let header = [1u8, 0, 4, 4];
    let name_index = cff_index(std::slice::from_ref(&font.name))?;
    let top_index_size = cff_index(&[vec![0u8; top_dict_len]])?.len();
    let string_index = cff_index(&font.strings)?;

    let fixed =
        header.len() + name_index.len() + top_index_size + string_index.len() + gsubr_index.len();
    let charset_at = fixed;
    let encoding_at = charset_at + font.charset.len();
    let charstrings_at = encoding_at + font.encoding.len();
    let private_at = charstrings_at + charstrings_index.len();

    let mut top_dict = top_prefix;
    if has_charset && charset_operand.is_some() {
        dict_int32(&mut top_dict, u32::try_from(charset_at).ok()?);
        dict_op(&mut top_dict, OP_CHARSET);
    }
    if has_encoding && encoding_operand.is_some() {
        dict_int32(&mut top_dict, u32::try_from(encoding_at).ok()?);
        dict_op(&mut top_dict, OP_ENCODING);
    }
    dict_int32(&mut top_dict, u32::try_from(charstrings_at).ok()?);
    dict_op(&mut top_dict, OP_CHARSTRINGS);
    dict_int32(&mut top_dict, u32::try_from(private.len()).ok()?);
    dict_int32(&mut top_dict, u32::try_from(private_at).ok()?);
    dict_op(&mut top_dict, OP_PRIVATE);
    if top_dict.len() != top_dict_len {
        return None;
    }
    let top_index = cff_index(&[top_dict])?;

    let mut out = Vec::with_capacity(private_at + private.len() + lsubr_index.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&name_index);
    out.extend_from_slice(&top_index);
    out.extend_from_slice(&string_index);
    out.extend_from_slice(&gsubr_index);
    out.extend_from_slice(&font.charset);
    out.extend_from_slice(&font.encoding);
    out.extend_from_slice(&charstrings_index);
    out.extend_from_slice(&private);
    if font.has_lsubrs {
        out.extend_from_slice(&lsubr_index);
    }
    Some(out)
}

/// Every glyph's verification trace, in GID order.
fn traces(font: &Font) -> Option<Vec<Trace>> {
    font.charstrings
        .iter()
        .map(|cs| trace(cs, &font.lsubrs, &font.gsubrs))
        .collect()
}

/// Strip Type2 hints from a `Type1C` font program.
///
/// Returns the rewritten program, or `None` to mean "ship the original bytes"
/// — for any structure outside the preconditions in the module docs, when
/// nothing was strippable, when the result is not strictly smaller, or when
/// the re-emitted font does not trace glyph-for-glyph identically to the
/// input.
///
/// `drop_private_hints` additionally removes the Private DICT hinting keys
/// (`BlueValues` and friends), which no longer describe anything once the
/// charstring hints are gone.
pub(crate) fn strip_hints(data: &[u8], drop_private_hints: bool) -> Option<Vec<u8>> {
    let font = parse(data)?;
    // Hints inside subroutines would spread stem counts across call
    // boundaries this in-place rewrite does not follow.
    if font
        .lsubrs
        .iter()
        .chain(&font.gsubrs)
        .any(|s| subr_has_hints(s))
    {
        return None;
    }
    let before = traces(&font)?;

    let mut charstrings: Vec<Vec<u8>> = Vec::with_capacity(font.charstrings.len());
    let mut stripped_any = false;
    for cs in &font.charstrings {
        match strip_charstring(cs) {
            // Per-glyph decline: keep the original bytes, keep the font.
            Some(new) => {
                stripped_any = true;
                charstrings.push(new);
            }
            None => charstrings.push(cs.clone()),
        }
    }
    if !stripped_any {
        return None;
    }

    let out = emit(&font, &charstrings, drop_private_hints)?;
    if out.len() >= data.len() {
        return None;
    }
    // Verify what actually ships: re-parse the emitted bytes and trace them.
    let back = parse(&out)?;
    if back.charstrings.len() != font.charstrings.len() {
        return None;
    }
    let after = traces(&back)?;
    (after == before).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::type1::{dict_number, t2_number};

    /// Build a minimal one-font CFF around the given charstrings.
    fn build(charstrings: &[Vec<u8>], subrs: &[Vec<u8>]) -> Vec<u8> {
        let mut strings: Vec<Vec<u8>> = Vec::new();
        let mut charset = vec![0u8];
        for i in 1..charstrings.len() {
            strings.push(format!("g{i}").into_bytes());
            let sid = u16::try_from(391 + strings.len() - 1).unwrap();
            charset.extend_from_slice(&sid.to_be_bytes());
        }
        let mut private = Vec::new();
        // A couple of Private DICT hint keys, so the drop path has something
        // to remove, plus the width parameters.
        dict_number(&mut private, -20.0).unwrap();
        dict_number(&mut private, 0.0).unwrap();
        dict_op(&mut private, 6); // BlueValues
        dict_number(&mut private, 60.0).unwrap();
        dict_op(&mut private, 10); // StdHW
        dict_number(&mut private, 0.0).unwrap();
        dict_op(&mut private, 20); // defaultWidthX
        dict_number(&mut private, 0.0).unwrap();
        dict_op(&mut private, 21); // nominalWidthX
        let has_subrs = !subrs.is_empty();
        if has_subrs {
            let at = private.len() + 5 + 1;
            dict_int32(&mut private, u32::try_from(at).unwrap());
            dict_op(&mut private, 19);
        }

        let mut top_prefix = Vec::new();
        for v in [0.0, -200.0, 1000.0, 900.0] {
            dict_number(&mut top_prefix, v).unwrap();
        }
        dict_op(&mut top_prefix, 5); // FontBBox
        let top_dict_len = top_prefix.len() + (5 + 1) * 2 + (5 + 5 + 1);
        let header = [1u8, 0, 4, 4];
        let name_index = cff_index(&[b"Test".to_vec()]).unwrap();
        let top_index_size = cff_index(&[vec![0u8; top_dict_len]]).unwrap().len();
        let string_index = cff_index(&strings).unwrap();
        let gsubr_index = cff_index(&[]).unwrap();
        let charstrings_index = cff_index(charstrings).unwrap();
        let fixed = header.len()
            + name_index.len()
            + top_index_size
            + string_index.len()
            + gsubr_index.len();
        let charset_at = fixed;
        let charstrings_at = charset_at + charset.len();
        let private_at = charstrings_at + charstrings_index.len();
        let mut top_dict = top_prefix;
        dict_int32(&mut top_dict, u32::try_from(charset_at).unwrap());
        dict_op(&mut top_dict, 15);
        dict_int32(&mut top_dict, u32::try_from(charstrings_at).unwrap());
        dict_op(&mut top_dict, 17);
        dict_int32(&mut top_dict, u32::try_from(private.len()).unwrap());
        dict_int32(&mut top_dict, u32::try_from(private_at).unwrap());
        dict_op(&mut top_dict, 18);
        assert_eq!(top_dict.len(), top_dict_len);
        let top_index = cff_index(&[top_dict]).unwrap();

        let mut out = Vec::new();
        out.extend_from_slice(&header);
        out.extend_from_slice(&name_index);
        out.extend_from_slice(&top_index);
        out.extend_from_slice(&string_index);
        out.extend_from_slice(&gsubr_index);
        out.extend_from_slice(&charset);
        out.extend_from_slice(&charstrings_index);
        out.extend_from_slice(&private);
        if has_subrs {
            out.extend_from_slice(&cff_index(subrs).unwrap());
        }
        out
    }

    fn num(v: f64) -> Vec<u8> {
        let mut b = Vec::new();
        t2_number(&mut b, v).unwrap();
        b
    }

    fn cs(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    /// `w 20 40 hstem 30 60 vstem 10 10 rmoveto 50 rlineto endchar`
    /// with a width operand folded onto the first (hint) operator.
    fn hinted_glyph_with_width() -> Vec<u8> {
        cs(&[
            &num(55.0), // width delta
            &num(20.0),
            &num(40.0),
            &[OP_HSTEM as u8],
            &num(30.0),
            &num(60.0),
            &[OP_VSTEM as u8],
            &num(10.0),
            &num(10.0),
            &[OP_RMOVETO as u8],
            &num(50.0),
            &[5u8], // rlineto
            &[OP_ENDCHAR as u8],
        ])
    }

    #[test]
    fn strip_removes_hints_and_refolds_the_width() {
        let original = hinted_glyph_with_width();
        let stripped = strip_charstring(&original).unwrap();
        assert!(stripped.len() < original.len());
        // The width must survive, now carried by rmoveto (3 operands).
        let a = trace(&original, &[], &[]).unwrap();
        let b = trace(&stripped, &[], &[]).unwrap();
        assert_eq!(a, b, "outline and width must be unchanged");
        assert_eq!(a.width, Some(55.0));
        assert_eq!(
            a.items.iter().map(|i| i.0).collect::<Vec<_>>(),
            vec![OP_RMOVETO, 5, OP_ENDCHAR]
        );
    }

    #[test]
    fn hintmask_and_its_operand_bytes_are_removed() {
        // 2 stems declared by hstemhm, then hintmask with a 1-byte mask, and
        // a second hintmask later in the outline (hint replacement).
        let original = cs(&[
            &num(20.0),
            &num(40.0),
            &num(30.0),
            &num(60.0),
            &[OP_HSTEMHM as u8],
            &[OP_HINTMASK as u8, 0b1100_0000],
            &num(10.0),
            &num(10.0),
            &[OP_RMOVETO as u8],
            &[OP_HINTMASK as u8, 0b0011_0000],
            &num(50.0),
            &[5u8],
            &[OP_ENDCHAR as u8],
        ]);
        let stripped = strip_charstring(&original).unwrap();
        assert_eq!(
            trace(&original, &[], &[]).unwrap(),
            trace(&stripped, &[], &[]).unwrap()
        );
        // Nothing but the outline is left: 10 10 rmoveto 50 rlineto endchar.
        assert_eq!(
            stripped,
            cs(&[
                &num(10.0),
                &num(10.0),
                &[OP_RMOVETO as u8],
                &num(50.0),
                &[5u8],
                &[OP_ENDCHAR as u8],
            ])
        );
    }

    #[test]
    fn font_round_trip_is_smaller_and_traces_identically() {
        let glyphs = vec![
            cs(&[&num(0.0), &[OP_ENDCHAR as u8]]), // .notdef
            hinted_glyph_with_width(),
            cs(&[
                &num(10.0),
                &num(20.0),
                &num(30.0),
                &num(40.0),
                &[OP_VSTEMHM as u8],
                &[OP_HINTMASK as u8, 0b1000_0000],
                &num(5.0),
                &[OP_HMOVETO as u8],
                &num(7.0),
                &num(8.0),
                &num(9.0),
                &num(10.0),
                &num(11.0),
                &num(12.0),
                &[8u8], // rrcurveto
                &[OP_ENDCHAR as u8],
            ]),
        ];
        let font = build(&glyphs, &[]);
        let stripped = strip_hints(&font, false).expect("must strip");
        assert!(stripped.len() < font.len());
        let a = traces(&parse(&font).unwrap()).unwrap();
        let b = traces(&parse(&stripped).unwrap()).unwrap();
        assert_eq!(a, b);
        // The Private DICT keys are untouched with the flag off.
        let with_private = strip_hints(&font, true).expect("must strip");
        assert!(
            with_private.len() < stripped.len(),
            "dropping BlueValues/StdHW must save more"
        );
        assert_eq!(traces(&parse(&with_private).unwrap()).unwrap(), a);
    }

    #[test]
    fn a_hint_inside_a_subroutine_declines_the_font() {
        let glyphs = vec![
            cs(&[&num(0.0), &[OP_ENDCHAR as u8]]),
            cs(&[
                &num(-107.0), // subr 0 with the standard bias
                &[OP_CALLSUBR as u8],
                &num(10.0),
                &num(10.0),
                &[OP_RMOVETO as u8],
                &[OP_ENDCHAR as u8],
            ]),
        ];
        let subrs = vec![cs(&[
            &num(20.0),
            &num(40.0),
            &[OP_HSTEM as u8],
            &[OP_RETURN as u8],
        ])];
        assert!(strip_hints(&build(&glyphs, &subrs), false).is_none());
    }

    #[test]
    fn a_subroutine_before_a_hint_declines_that_glyph() {
        // The subr's stack effect is unknown, so the operands feeding the
        // later hstem cannot be identified: that glyph must be left alone.
        let program = cs(&[
            &num(-107.0),
            &[OP_CALLSUBR as u8],
            &num(20.0),
            &num(40.0),
            &[OP_HSTEM as u8],
            &num(10.0),
            &num(10.0),
            &[OP_RMOVETO as u8],
            &[OP_ENDCHAR as u8],
        ]);
        assert!(strip_charstring(&program).is_none());
    }

    #[test]
    fn unhinted_charstring_declines() {
        let program = cs(&[
            &num(10.0),
            &num(10.0),
            &[OP_RMOVETO as u8],
            &[OP_ENDCHAR as u8],
        ]);
        assert!(strip_charstring(&program).is_none());
    }

    /// Real-corpus harness: point `AMATL_CFF_DIR` at a directory of raw
    /// `Type1C` programs (every `/FontFile3` payload extracted from amatl's
    /// own output) and this strips each one, re-verifying every glyph's
    /// outline and advance from the *emitted* bytes and reporting the size
    /// delta. Ignored by default — it needs an external corpus, and the fonts
    /// in question are not redistributable.
    #[test]
    #[ignore = "needs AMATL_CFF_DIR corpus"]
    fn corpus_report() {
        let dir = std::env::var("AMATL_CFF_DIR").unwrap_or_else(|_| "target/scratch/h5/cff".into());
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        entries.sort();
        let (mut cur, mut new, mut done, mut skipped, mut glyphs) = (0usize, 0usize, 0, 0, 0usize);
        for path in &entries {
            let data = std::fs::read(path).unwrap();
            cur += data.len();
            match strip_hints(&data, true) {
                Some(out) => {
                    // strip_hints already gates on this; assert it here too so
                    // a corpus run is a real verification, not just a report.
                    let before = traces(&parse(&data).unwrap()).unwrap();
                    let after = traces(&parse(&out).unwrap()).unwrap();
                    assert_eq!(before, after, "outline mismatch in {}", path.display());
                    glyphs += before.len();
                    new += out.len();
                    done += 1;
                }
                None => {
                    new += data.len();
                    skipped += 1;
                    println!("declined {}", path.display());
                }
            }
        }
        println!(
            "{done} programs stripped, {skipped} declined; {glyphs} glyphs outline-verified; \
             {cur} -> {new} (save {})",
            cur - new
        );
    }

    #[test]
    fn garbage_declines() {
        assert!(strip_hints(b"", false).is_none());
        assert!(strip_hints(&[1u8; 64], false).is_none());
        assert!(strip_hints(&[0xFFu8; 256], false).is_none());
    }
}
