//! Union-merge of same-family CFF (`Type1C`) subset fragments.
//!
//! Producers that subset per page section leave many small fragments of the
//! same original font, each carrying its own copy of the shared subroutines
//! and overlapping charstrings. For *simple* (non-CID) PDF fonts every glyph
//! lookup goes PDF `/Encoding` → glyph *name* → CFF charset, so fragments can
//! share one union program with no content-stream rewriting at all.
//!
//! The merge is byte-conservative, not interpretive: a family merges only
//! when every fragment's global and local subroutine INDEXes are
//! byte-identical, their Private DICTs agree on everything except the
//! width parameters (`defaultWidthX`/`nominalWidthX`) and the `Subrs`
//! offset, and every shared glyph name resolves to the same charstring
//! after normalizing the leading width operand to its absolute value.
//! (The width operand is relative to the fragment's own `nominalWidthX`,
//! so byte-different charstrings are routinely the *same* glyph.) Any
//! other difference — or any structural doubt while parsing — declines
//! the whole family, fail-safe.
//!
//! The merged program keeps the base fragment's tables verbatim wherever
//! possible: subrs and Private DICT entries are byte-spliced, not
//! re-encoded; only appended charstrings get their width operand
//! re-expressed against the base's width parameters.

use crate::type1::{cff_index, dict_int32, dict_number, dict_op, t2_number};
use std::collections::BTreeMap;

/// A glyph name as the merge keys it: standard SIDs (< 391) name the same
/// glyph in every CFF, custom SIDs go through the fragment's String INDEX.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum NameKey {
    Std(u16),
    Custom(Vec<u8>),
}

/// One glyph, width-normalized: `width` is absolute (the fragment's
/// `nominalWidthX` already applied), `tail` is the charstring with the
/// leading width operand removed.
struct Glyph {
    width: f64,
    tail: Vec<u8>,
}

/// One parsed fragment.
struct Fragment {
    name: Vec<u8>,
    /// Token span of `FontMatrix` operands in the Top DICT (for splicing).
    font_matrix: Option<Vec<u8>>,
    font_matrix_values: Vec<f64>,
    font_bbox: Vec<f64>,
    /// Glyphs in this fragment's GID order, `.notdef` first.
    order: Vec<NameKey>,
    glyphs: BTreeMap<NameKey, Glyph>,
    gsubrs: Vec<Vec<u8>>,
    lsubrs: Vec<Vec<u8>>,
    /// Private DICT bytes minus the `Subrs`/`defaultWidthX`/`nominalWidthX`
    /// entries (the family-equality comparand).
    private_cmp: Vec<u8>,
    /// Private DICT bytes minus only the `Subrs` entry (the emission base).
    private_body: Vec<u8>,
    has_lsubrs: bool,
    default_width: f64,
    nominal_width: f64,
}

fn be16(d: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*d.get(off)?, *d.get(off + 1)?]))
}

/// Read a CFF INDEX at `pos`; returns (items, next offset).
fn read_index(data: &[u8], pos: usize) -> Option<(Vec<Vec<u8>>, usize)> {
    let count = usize::from(be16(data, pos)?);
    if count == 0 {
        return Some((Vec::new(), pos + 2));
    }
    let off_size = usize::from(*data.get(pos + 2)?);
    if !(1..=4).contains(&off_size) {
        return None;
    }
    let offsets_at = pos + 3;
    let read_off = |i: usize| -> Option<usize> {
        let mut v = 0usize;
        for k in 0..off_size {
            v = (v << 8) | usize::from(*data.get(offsets_at + i * off_size + k)?);
        }
        Some(v)
    };
    let data_at = offsets_at + (count + 1) * off_size - 1;
    let mut items = Vec::with_capacity(count);
    let mut prev = read_off(0)?;
    if prev != 1 {
        return None;
    }
    for i in 1..=count {
        let next = read_off(i)?;
        if next < prev {
            return None;
        }
        items.push(data.get(data_at + prev..data_at + next)?.to_vec());
        prev = next;
    }
    Some((items, data_at + prev))
}

/// One parsed DICT entry: operator, numeric operands, and the raw byte span
/// of the operands + operator (for verbatim splicing).
struct DictEntry {
    op: u16,
    operands: Vec<f64>,
    span: (usize, usize),
}

fn parse_dict(data: &[u8]) -> Option<Vec<DictEntry>> {
    let mut out = Vec::new();
    let mut operands: Vec<f64> = Vec::new();
    let mut i = 0usize;
    let mut entry_start = 0usize;
    while i < data.len() {
        let b = *data.get(i)?;
        match b {
            32..=246 => {
                operands.push(f64::from(i32::from(b) - 139));
                i += 1;
            }
            247..=250 => {
                let b1 = *data.get(i + 1)?;
                operands.push(f64::from((i32::from(b) - 247) * 256 + i32::from(b1) + 108));
                i += 2;
            }
            251..=254 => {
                let b1 = *data.get(i + 1)?;
                operands.push(f64::from(-(i32::from(b) - 251) * 256 - i32::from(b1) - 108));
                i += 2;
            }
            28 => {
                let v = i16::from_be_bytes([*data.get(i + 1)?, *data.get(i + 2)?]);
                operands.push(f64::from(v));
                i += 3;
            }
            29 => {
                let v = i32::from_be_bytes([
                    *data.get(i + 1)?,
                    *data.get(i + 2)?,
                    *data.get(i + 3)?,
                    *data.get(i + 4)?,
                ]);
                operands.push(f64::from(v));
                i += 5;
            }
            30 => {
                // Real: BCD nibbles until 0xf terminator.
                let mut text = String::new();
                let mut j = i + 1;
                'outer: loop {
                    let byte = *data.get(j)?;
                    j += 1;
                    for nib in [byte >> 4, byte & 0xf] {
                        match nib {
                            0..=9 => text.push((b'0' + nib) as char),
                            0xa => text.push('.'),
                            0xb => text.push('E'),
                            0xc => text.push_str("E-"),
                            0xe => text.push('-'),
                            0xf => break 'outer,
                            _ => return None,
                        }
                    }
                }
                operands.push(text.parse().ok()?);
                i = j;
            }
            0..=21 => {
                let op = if b == 12 {
                    let b1 = *data.get(i + 1)?;
                    i += 2;
                    0x0C00 | u16::from(b1)
                } else {
                    i += 1;
                    u16::from(b)
                };
                out.push(DictEntry {
                    op,
                    operands: std::mem::take(&mut operands),
                    span: (entry_start, i),
                });
                entry_start = i;
            }
            _ => return None,
        }
    }
    if operands.is_empty() {
        Some(out)
    } else {
        None
    }
}

fn dict_get(dict: &[DictEntry], op: u16) -> Option<&DictEntry> {
    dict.iter().find(|e| e.op == op)
}

/// Split a Type2 charstring into (absolute width, tail after the width
/// operand). `None` when the leading token structure cannot prove where the
/// width ends (e.g. the first operator is a subroutine call).
fn split_width(cs: &[u8], default_w: f64, nominal_w: f64) -> Option<(f64, Vec<u8>)> {
    let mut i = 0usize;
    let mut nums: Vec<f64> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    while i < cs.len() {
        let b = cs[i];
        if b >= 32 || b == 28 {
            starts.push(i);
            match b {
                28 => {
                    let v = i16::from_be_bytes([*cs.get(i + 1)?, *cs.get(i + 2)?]);
                    nums.push(f64::from(v));
                    i += 3;
                }
                32..=246 => {
                    nums.push(f64::from(i32::from(b) - 139));
                    i += 1;
                }
                247..=250 => {
                    let b1 = *cs.get(i + 1)?;
                    nums.push(f64::from((i32::from(b) - 247) * 256 + i32::from(b1) + 108));
                    i += 2;
                }
                251..=254 => {
                    let b1 = *cs.get(i + 1)?;
                    nums.push(f64::from(-(i32::from(b) - 251) * 256 - i32::from(b1) - 108));
                    i += 2;
                }
                _ => {
                    // 255: 16.16 fixed
                    let v = i32::from_be_bytes([
                        *cs.get(i + 1)?,
                        *cs.get(i + 2)?,
                        *cs.get(i + 3)?,
                        *cs.get(i + 4)?,
                    ]);
                    nums.push(f64::from(v) / 65536.0);
                    i += 5;
                }
            }
            continue;
        }
        // First operator decides whether a width operand is present
        // (Type2 appendix: hstem/vstem[hm] and masks take even counts,
        // rmoveto 2, h/vmoveto 1, endchar 0 or 4).
        let has_width = match b {
            1 | 3 | 18 | 23 | 19 | 20 => nums.len() % 2 == 1,
            21 => nums.len() > 2,
            22 | 4 => nums.len() > 1,
            14 => !matches!(nums.len(), 0 | 4),
            _ => return None, // incl. callsubr/callgsubr/12-escape: unprovable
        };
        let tail_at = if has_width {
            *starts.get(1).unwrap_or(&i)
        } else {
            *starts.first().unwrap_or(&i)
        };
        let width = if has_width {
            nominal_w + nums[0]
        } else {
            default_w
        };
        return Some((width, cs[tail_at..].to_vec()));
    }
    None
}

const OP_CHARSET: u16 = 15;
const OP_CHARSTRINGS: u16 = 17;
const OP_PRIVATE: u16 = 18;
const OP_SUBRS: u16 = 19;
const OP_DEFAULT_WIDTH: u16 = 20;
const OP_NOMINAL_WIDTH: u16 = 21;
const OP_CHARSTRING_TYPE: u16 = 0x0C06;
const OP_FONT_MATRIX: u16 = 0x0C07;
const OP_PAINT_TYPE: u16 = 0x0C05;
const OP_FONT_BBOX: u16 = 5;
const OP_ROS: u16 = 0x0C1E;
const OP_FD_ARRAY: u16 = 0x0C24;
const OP_FD_SELECT: u16 = 0x0C25;

fn parse_fragment(data: &[u8]) -> Option<Fragment> {
    // Header: major 1, hdrSize.
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
    // Shapes the merge cannot represent: CID-keyed, non-Type2, painted.
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
    if let Some(e) = dict_get(&top, OP_PAINT_TYPE) {
        if e.operands != [0.0] {
            return None;
        }
    }
    let font_matrix_values = match dict_get(&top, OP_FONT_MATRIX) {
        Some(e) if e.operands.len() == 6 => e.operands.clone(),
        Some(_) => return None,
        None => vec![0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
    };
    let font_matrix = dict_get(&top, OP_FONT_MATRIX).map(|e| tops[0][e.span.0..e.span.1].to_vec());
    let font_bbox = match dict_get(&top, OP_FONT_BBOX) {
        Some(e) if e.operands.len() == 4 => e.operands.clone(),
        _ => return None,
    };

    let charstrings_at = match dict_get(&top, OP_CHARSTRINGS)?.operands.as_slice() {
        [v] if *v >= 0.0 => *v as usize,
        _ => return None,
    };
    let (charstrings, _) = read_index(data, charstrings_at)?;
    if charstrings.is_empty() {
        return None;
    }

    // Charset (formats 0/1/2; predefined charsets declined).
    let charset_at = match dict_get(&top, OP_CHARSET)?.operands.as_slice() {
        [v] if *v >= 3.0 => *v as usize,
        _ => return None,
    };
    let n_glyphs = charstrings.len();
    let mut sids: Vec<u16> = Vec::with_capacity(n_glyphs);
    sids.push(0); // .notdef
    match *data.get(charset_at)? {
        0 => {
            for i in 0..n_glyphs - 1 {
                sids.push(be16(data, charset_at + 1 + i * 2)?);
            }
        }
        fmt @ (1 | 2) => {
            let mut at = charset_at + 1;
            while sids.len() < n_glyphs {
                let first = be16(data, at)?;
                let n_left = if fmt == 1 {
                    usize::from(*data.get(at + 2)?)
                } else {
                    usize::from(be16(data, at + 2)?)
                };
                at += if fmt == 1 { 3 } else { 4 };
                for k in 0..=n_left {
                    if sids.len() == n_glyphs {
                        return None;
                    }
                    sids.push(first.checked_add(u16::try_from(k).ok()?)?);
                }
            }
        }
        _ => return None,
    }

    let name_of = |sid: u16| -> Option<NameKey> {
        if sid < 391 {
            Some(NameKey::Std(sid))
        } else {
            Some(NameKey::Custom(
                strings.get(usize::from(sid) - 391)?.clone(),
            ))
        }
    };

    // Private DICT + local subrs.
    let (private_size, private_at) = match dict_get(&top, OP_PRIVATE)?.operands.as_slice() {
        [size, at] if *size >= 0.0 && *at >= 0.0 => (*size as usize, *at as usize),
        _ => return None,
    };
    let private_raw = data.get(private_at..private_at.checked_add(private_size)?)?;
    let private = parse_dict(private_raw)?;
    let mut default_width = 0.0f64;
    let mut nominal_width = 0.0f64;
    if let Some(e) = dict_get(&private, OP_DEFAULT_WIDTH) {
        default_width = *e.operands.first()?;
    }
    if let Some(e) = dict_get(&private, OP_NOMINAL_WIDTH) {
        nominal_width = *e.operands.first()?;
    }
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
    let splice = |skip: &[u16]| -> Vec<u8> {
        let mut out = Vec::with_capacity(private_raw.len());
        for e in &private {
            if !skip.contains(&e.op) {
                out.extend_from_slice(&private_raw[e.span.0..e.span.1]);
            }
        }
        out
    };
    let private_cmp = splice(&[OP_SUBRS, OP_DEFAULT_WIDTH, OP_NOMINAL_WIDTH]);
    let private_body = splice(&[OP_SUBRS]);

    // Width-normalize every glyph; duplicate names decline the fragment.
    let mut order = Vec::with_capacity(n_glyphs);
    let mut glyphs = BTreeMap::new();
    for (i, cs) in charstrings.iter().enumerate() {
        let key = name_of(sids[i])?;
        let (width, tail) = split_width(cs, default_width, nominal_width)?;
        if glyphs.insert(key.clone(), Glyph { width, tail }).is_some() {
            return None;
        }
        order.push(key);
    }

    Some(Fragment {
        name: names[0].clone(),
        font_matrix,
        font_matrix_values,
        font_bbox,
        order,
        glyphs,
        gsubrs,
        lsubrs,
        private_cmp,
        private_body,
        has_lsubrs,
        default_width,
        nominal_width,
    })
}

/// Merge same-family CFF subset fragments into one union program every
/// fragment's PDF font dictionary can share. `None` unless every safety
/// precondition holds (see module docs); the caller then keeps all fragments
/// untouched. Requires at least two fragments.
///
/// The caller must guarantee PDF-side preconditions: every font dictionary
/// carries an explicit `/Encoding` whose base is a *named* encoding (the
/// merged program drops the fragments' built-in encodings), and `/Widths`
/// covers the shown codes (intrinsic advance widths of glyphs appended from
/// non-base fragments are re-based, byte-exactly, on the base's width
/// parameters).
pub(crate) fn merge_type1c(fragments: &[&[u8]]) -> Option<Vec<u8>> {
    if fragments.len() < 2 {
        return None;
    }
    let parsed: Vec<Fragment> = fragments
        .iter()
        .map(|f| parse_fragment(f))
        .collect::<Option<_>>()?;

    // Base: most glyphs, ties to the earliest (deterministic given the
    // caller's sorted input).
    let base_idx = (0..parsed.len()).max_by_key(|&i| (parsed[i].glyphs.len(), usize::MAX - i))?;
    let base = &parsed[base_idx];

    // Family-wide equality requirements.
    for f in &parsed {
        if f.gsubrs != base.gsubrs
            || f.lsubrs != base.lsubrs
            || f.has_lsubrs != base.has_lsubrs
            || f.private_cmp != base.private_cmp
            || f.font_matrix_values != base.font_matrix_values
        {
            return None;
        }
    }

    // Union of glyphs: shared names must agree on (width, tail) exactly.
    let mut union: BTreeMap<&NameKey, &Glyph> = BTreeMap::new();
    for f in &parsed {
        for (key, glyph) in &f.glyphs {
            match union.get(key) {
                Some(g) if g.width == glyph.width && g.tail == glyph.tail => {}
                Some(_) => return None,
                None => {
                    union.insert(key, glyph);
                }
            }
        }
    }
    // GID order: the base's own order, then appended names sorted.
    let mut order: Vec<&NameKey> = base.order.iter().collect();
    for key in union.keys() {
        if !base.glyphs.contains_key(key) {
            order.push(key);
        }
    }
    if *order.first()? != &NameKey::Std(0) || order.len() > usize::from(u16::MAX) {
        return None;
    }

    // Re-encode each charstring's width against the base's parameters.
    let mut charstrings: Vec<Vec<u8>> = Vec::with_capacity(order.len());
    for key in &order {
        let glyph = union.get(*key)?;
        let mut cs = Vec::with_capacity(glyph.tail.len() + 3);
        if glyph.width != base.default_width {
            t2_number(&mut cs, glyph.width - base.nominal_width)?;
        }
        cs.extend_from_slice(&glyph.tail);
        charstrings.push(cs);
    }

    // Strings: custom names in first-use order.
    let mut strings: Vec<Vec<u8>> = Vec::new();
    let mut charset = vec![0u8]; // format 0
    for key in &order[1..] {
        let sid = match key {
            NameKey::Std(sid) => *sid,
            NameKey::Custom(name) => {
                let pos = match strings.iter().position(|s| s == name) {
                    Some(p) => p,
                    None => {
                        strings.push(name.clone());
                        strings.len() - 1
                    }
                };
                u16::try_from(391 + pos).ok()?
            }
        };
        charset.extend_from_slice(&sid.to_be_bytes());
    }

    // FontBBox: element-wise union (appended glyphs may exceed the base's).
    let bbox: Vec<f64> = (0..4)
        .map(|i| {
            let vals = parsed.iter().map(|f| f.font_bbox[i]);
            if i < 2 {
                vals.fold(f64::INFINITY, f64::min)
            } else {
                vals.fold(f64::NEG_INFINITY, f64::max)
            }
        })
        .collect();

    // Top DICT: FontMatrix (spliced verbatim from the base) + FontBBox +
    // fixed-width charset/CharStrings/Private offsets. No Encoding operator:
    // the predefined standard encoding stands in for the dropped built-ins,
    // which the caller's explicit-/Encoding precondition makes unreachable.
    let mut top_prefix = Vec::new();
    if let Some(fm) = &base.font_matrix {
        top_prefix.extend_from_slice(fm);
    }
    for v in &bbox {
        dict_number(&mut top_prefix, *v)?;
    }
    dict_op(&mut top_prefix, OP_FONT_BBOX);

    // Private DICT: base's entries verbatim, plus a fixed-width Subrs offset
    // pointing just past the DICT when local subrs exist.
    let mut private = base.private_body.clone();
    if base.has_lsubrs {
        let subrs_at = private.len() + 5 + 1;
        dict_int32(&mut private, u32::try_from(subrs_at).ok()?);
        dict_op(&mut private, OP_SUBRS);
    }

    let charstrings_index = cff_index(&charstrings)?;
    let lsubr_index = cff_index(&base.lsubrs)?;
    let gsubr_index = cff_index(&base.gsubrs)?;

    let top_dict_len = top_prefix.len() + (5 + 1) * 2 + (5 + 5 + 1);
    let header = [1u8, 0, 4, 4];
    let name_index = cff_index(std::slice::from_ref(&base.name))?;
    let top_index_size = cff_index(&[vec![0u8; top_dict_len]])?.len();
    let string_index = cff_index(&strings)?;

    let fixed =
        header.len() + name_index.len() + top_index_size + string_index.len() + gsubr_index.len();
    let charset_at = fixed;
    let charstrings_at = charset_at + charset.len();
    let private_at = charstrings_at + charstrings_index.len();

    let mut top_dict = top_prefix;
    dict_int32(&mut top_dict, u32::try_from(charset_at).ok()?);
    dict_op(&mut top_dict, OP_CHARSET);
    dict_int32(&mut top_dict, u32::try_from(charstrings_at).ok()?);
    dict_op(&mut top_dict, OP_CHARSTRINGS);
    dict_int32(&mut top_dict, u32::try_from(private.len()).ok()?);
    dict_int32(&mut top_dict, u32::try_from(private_at).ok()?);
    dict_op(&mut top_dict, OP_PRIVATE);
    debug_assert_eq!(top_dict.len(), top_dict_len);
    let top_index = cff_index(&[top_dict])?;

    let mut out = Vec::with_capacity(private_at + private.len() + lsubr_index.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&name_index);
    out.extend_from_slice(&top_index);
    out.extend_from_slice(&string_index);
    out.extend_from_slice(&gsubr_index);
    out.extend_from_slice(&charset);
    out.extend_from_slice(&charstrings_index);
    out.extend_from_slice(&private);
    if base.has_lsubrs {
        out.extend_from_slice(&lsubr_index);
    }

    // Round-trip sanity: the merged program must parse back with exactly the
    // union's glyphs, widths, and tails.
    let back = parse_fragment(&out)?;
    if back.order.len() != order.len() {
        return None;
    }
    for key in &order {
        let a = back.glyphs.get(*key)?;
        let b = union.get(*key)?;
        if a.width != b.width || a.tail != b.tail {
            return None;
        }
    }
    Some(out)
}

/// Number of glyphs shared with the base if merged — used by the caller only
/// for logging/diagnostics. (Kept minimal; the merge re-validates.)
#[cfg(test)]
mod tests {
    use super::*;

    /// Build a small one-font CFF via the merge emitter itself is circular;
    /// instead exercise split_width directly and round-trip merge on two
    /// hand-built fragments produced by `build_fragment`.
    fn build_fragment(glyphs: &[(&[u8], f64, &[u8])], default_w: f64, nominal_w: f64) -> Vec<u8> {
        // Charstrings with width deltas against (default_w, nominal_w).
        let charstrings: Vec<Vec<u8>> = glyphs
            .iter()
            .map(|(_, w, tail)| {
                let mut cs = Vec::new();
                if *w != default_w {
                    t2_number(&mut cs, *w - nominal_w).unwrap();
                }
                cs.extend_from_slice(tail);
                cs
            })
            .collect();
        let mut strings: Vec<Vec<u8>> = Vec::new();
        let mut charset = vec![0u8];
        for (name, _, _) in &glyphs[1..] {
            strings.push(name.to_vec());
            let sid = u16::try_from(391 + strings.len() - 1).unwrap();
            charset.extend_from_slice(&sid.to_be_bytes());
        }
        let mut private = Vec::new();
        dict_number(&mut private, default_w).unwrap();
        dict_op(&mut private, 20);
        dict_number(&mut private, nominal_w).unwrap();
        dict_op(&mut private, 21);

        let charstrings_index = cff_index(&charstrings).unwrap();
        let mut top_prefix = Vec::new();
        for v in [0.0, -200.0, 1000.0, 900.0] {
            dict_number(&mut top_prefix, v).unwrap();
        }
        dict_op(&mut top_prefix, 5);
        let top_dict_len = top_prefix.len() + (5 + 1) * 2 + (5 + 5 + 1);
        let header = [1u8, 0, 4, 4];
        let name_index = cff_index(&[b"Test".to_vec()]).unwrap();
        let top_index_size = cff_index(&[vec![0u8; top_dict_len]]).unwrap().len();
        let string_index = cff_index(&strings).unwrap();
        let gsubr_index = cff_index(&[]).unwrap();
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
        out
    }

    // endchar-terminated stub outlines.
    const T1: &[u8] = &[139 + 10, 139 + 10, 21, 14]; // 10 10 rmoveto endchar
    const T2: &[u8] = &[139 + 20, 139 + 20, 21, 14];
    const T3: &[u8] = &[139 + 30, 139 + 30, 21, 14];

    #[test]
    fn split_width_detects_presence_by_parity() {
        // width 250 relative to nominal 200 -> operand 50, then rmoveto.
        let mut cs = Vec::new();
        t2_number(&mut cs, 50.0).unwrap();
        cs.extend_from_slice(T1);
        let (w, tail) = split_width(&cs, 500.0, 200.0).unwrap();
        assert_eq!(w, 250.0);
        assert_eq!(tail, T1);
        // No width -> defaultWidthX.
        let (w, tail) = split_width(T1, 500.0, 200.0).unwrap();
        assert_eq!(w, 500.0);
        assert_eq!(tail, T1);
    }

    #[test]
    fn merge_unions_glyphs_and_rebases_widths() {
        let a = build_fragment(
            &[(b".notdef".as_slice(), 0.0, T1), (b"A", 600.0, T2)],
            0.0,
            0.0,
        );
        // Same glyph A with the same absolute width under different width
        // parameters (byte-different charstring), plus a new glyph B.
        let b = build_fragment(
            &[
                (b".notdef".as_slice(), 0.0, T1),
                (b"A", 600.0, T2),
                (b"B", 700.0, T3),
            ],
            100.0,
            300.0,
        );
        let merged = merge_type1c(&[&a, &b]).unwrap();
        let f = parse_fragment(&merged).unwrap();
        assert_eq!(f.order.len(), 3);
        let ga = f.glyphs.get(&NameKey::Custom(b"A".to_vec())).unwrap();
        assert_eq!((ga.width, ga.tail.as_slice()), (600.0, T2));
        let gb = f.glyphs.get(&NameKey::Custom(b"B".to_vec())).unwrap();
        assert_eq!((gb.width, gb.tail.as_slice()), (700.0, T3));
    }

    #[test]
    fn merge_declines_conflicting_glyphs() {
        let a = build_fragment(
            &[(b".notdef".as_slice(), 0.0, T1), (b"A", 600.0, T2)],
            0.0,
            0.0,
        );
        let b = build_fragment(
            &[(b".notdef".as_slice(), 0.0, T1), (b"A", 600.0, T3)],
            0.0,
            0.0,
        );
        assert!(merge_type1c(&[&a, &b]).is_none());
    }
}
