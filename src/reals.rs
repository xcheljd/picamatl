//! Exact restoration of PDF real literals that lopdf's `f32` object model
//! cannot round-trip.
//!
//! `Object::Real` holds an `f32` (lopdf 0.44 `src/object.rs:42`) and the writer
//! prints it with `{}` (`src/writer.rs:594`), i.e. Rust's shortest decimal that
//! round-trips *as an `f32`*. A literal needing more than ~7 significant digits
//! is therefore a different number after a load/save:
//!
//! ```text
//!   841.91998  ->  f32 841.9199829101562  ->  written back as "841.92"
//! ```
//!
//! Viewers parse reals as doubles, so to them the value simply moved by 2e-5.
//! On `/MediaBox` that shifts the page-to-device origin and re-grid-fits every
//! glyph on the page; on `/BBox`, `/Rect`, `/W` and `/Bounds` it moves a clip,
//! a hit region, an advance or a shading stop by the same amount. See
//! `docs/upstream-lopdf-f32-reals.md` for the upstream report.
//!
//! The digits are destroyed at parse time, so there is nothing to fix inside
//! the `Document`: no `Object` variant can hold `841.91998`, and no writer
//! setting can print it. The only place both the original literal and the
//! finished file exist is around the save, so that is where this works:
//!
//! 1. [`capture`] reads the *raw input bytes* and records, for every real
//!    literal lopdf cannot round-trip, the exact decimal text, keyed by the
//!    `f32` bit pattern lopdf will hold.
//! 2. [`restore`] rewrites the just-serialized output, replacing lopdf's
//!    shortened print of those `f32`s with the captured literal — inside
//!    object streams as well as plain object bodies — and repairs every byte
//!    offset the length change invalidates.
//!
//! Both halves are keyed by *value*, not by dictionary key or object id, so
//! this covers every real in the document (the corpus census in
//! `docs/upstream-lopdf-f32-reals.md` finds `/Rect`, `/XYZ`, `/BBox`, `/W`,
//! `/MediaBox`, `/FontBBox`, `/Bounds`, `/Domain`, ... in that order of
//! frequency) rather than a hand-maintained list of "load-bearing" keys.
//!
//! Safety posture, in order of strength:
//!
//! * Every replacement has the *same `f32` bits* as what it replaces. Nothing
//!   that reads the file as `f32` — including lopdf itself — sees any change;
//!   the only readers affected are the ones that parse reals as doubles, and
//!   for those the value moves from lopdf's rounding back to the input's.
//! * A value is restored only when the input maps it *unambiguously*: if two
//!   different literals share one `f32`, or if the shortened form itself also
//!   occurs literally in the input, that value is dropped from the map and
//!   left exactly as lopdf wrote it.
//! * An empty map is an early return, so a document with no drifting literal
//!   is byte-identical to what it was before this pass existed.
//! * The patched bytes must re-parse, and must parse to the same objects as
//!   the unpatched bytes, or the unpatched bytes are handed back.

use std::collections::{HashMap, HashSet};

use lopdf::{Document, Object};

use crate::{deflate_level9, find_sub, inflate_capped, MAX_REDEFLATE_BYTES};

/// The exact decimal text of every real literal in the input whose value
/// lopdf's `f32` cannot represent, keyed by that `f32`'s bit pattern.
#[derive(Default, Debug)]
pub(crate) struct RealLiterals {
    exact: HashMap<u32, Vec<u8>>,
}

impl RealLiterals {
    pub(crate) fn is_empty(&self) -> bool {
        self.exact.is_empty()
    }

    /// The literal to emit in place of `printed`, when `printed` is exactly
    /// lopdf's shortest `f32` print of a captured value and differs from the
    /// input's own text for it.
    fn replacement(&self, printed: &[u8]) -> Option<&[u8]> {
        let text = std::str::from_utf8(printed).ok()?;
        let value: f32 = text.parse().ok()?;
        if !value.is_finite() {
            return None;
        }
        let exact = self.exact.get(&value.to_bits())?;
        // Only ever lengthen lopdf's own shortest print: a token that is
        // already the full literal (or any other spelling) is left alone.
        if format!("{value}").as_bytes() != printed || exact.as_slice() == printed {
            return None;
        }
        Some(exact)
    }
}

/// Record the input's real literals that will not survive lopdf's `f32`.
///
/// Scans the raw bytes — plain object bodies plus the inflated payload of
/// every `FlateDecode` object stream — for real tokens, skipping comments,
/// strings and stream payloads. Anything that cannot be read (an encrypted or
/// predictor-filtered object stream, a `/Length` this scanner cannot follow)
/// simply contributes nothing: the reals inside it are then left as lopdf
/// writes them, which is the behaviour without this pass at all.
pub(crate) fn capture(input: &[u8]) -> RealLiterals {
    let mut exact: HashMap<u32, (f64, Vec<u8>)> = HashMap::new();
    let mut poisoned: HashSet<u32> = HashSet::new();

    let mut note = |token: &[u8]| {
        let Ok(text) = std::str::from_utf8(token) else {
            return;
        };
        if !text.contains('.') {
            return;
        }
        let (Ok(wide), Ok(narrow)) = (text.parse::<f64>(), text.parse::<f32>()) else {
            return;
        };
        if !narrow.is_finite() {
            return;
        }
        let bits = narrow.to_bits();
        if format!("{narrow}").parse::<f64>() == Ok(wide) {
            // lopdf round-trips this literal exactly. Restoring some *other*
            // literal for the same f32 would rewrite these occurrences too, so
            // this value is off limits.
            poisoned.insert(bits);
            return;
        }
        match exact.get(&bits) {
            Some((seen, _)) if *seen == wide => {}
            Some(_) => {
                poisoned.insert(bits);
            }
            None => {
                exact.insert(bits, (wide, token.to_vec()));
            }
        }
    };

    scan_reals(input, true, &mut note);
    for payload in objstm_payloads(input) {
        scan_reals(&payload, false, &mut note);
    }

    RealLiterals {
        exact: exact
            .into_iter()
            .filter(|(bits, _)| !poisoned.contains(bits))
            .map(|(bits, (_, text))| (bits, text))
            .collect(),
    }
}

/// Put the captured literals back into a just-serialized document.
///
/// Returns `out` untouched whenever there is nothing to restore or anything at
/// all about the file's shape is not what lopdf's writer emits.
pub(crate) fn restore(out: Vec<u8>, literals: &RealLiterals) -> Vec<u8> {
    if std::env::var_os("AMATL_DEBUG_REALS").is_some() {
        eprintln!("restore entered: {} literals, {} bytes", literals.exact.len(), out.len());
    }
    if literals.is_empty() {
        return out;
    }
    match try_restore(&out, literals) {
        Some(patched) => patched,
        None => out,
    }
}

/// One byte-range replacement in the output.
#[derive(Debug)]
struct Edit {
    at: usize,
    len: usize,
    text: Vec<u8>,
}

fn try_restore(out: &[u8], literals: &RealLiterals) -> Option<Vec<u8>> {
    let mut edits: Vec<Edit> = Vec::new();

    // Plain object bodies: page dicts on the unpacked save path, and every
    // stream dictionary (`/BBox`, `/Matrix`, `/Decode`, ...) on both paths,
    // since streams are never packed into an object stream.
    scan_reals_positions(out, true, &mut |at, token| {
        if let Some(text) = literals.replacement(token) {
            edits.push(Edit {
                at,
                len: token.len(),
                text: text.to_vec(),
            });
        }
    });

    // Packed objects live inside `ObjStm` payloads; each one that changes is
    // re-packed, re-deflated and its `/Length` and `/First` rewritten.
    let mut from = 0;
    while let Some(tag_at) = find_sub(out, b"/Type/ObjStm", from) {
        from = tag_at + 1;
        if let Some(mut more) = objstm_edits(out, tag_at, literals) {
            edits.append(&mut more);
        }
    }

    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|e| e.at);
    // Overlapping edits would make the shift map ambiguous; the two producers
    // above work on disjoint regions, so this is a structural check.
    for pair in edits.windows(2) {
        if pair[0].at + pair[0].len > pair[1].at {
            return None;
        }
    }

    let dbg = std::env::var_os("AMATL_DEBUG_REALS").is_some();
    if dbg { eprintln!("edits: {}", edits.len()); }
    let patched = apply_edits(out, &edits);
    let Some(patched) = repair_offsets(patched, out, &edits) else {
        if dbg { eprintln!("repair_offsets failed"); }
        return None;
    };

    // Proof, in the same shape as the crate's other byte patches: the result
    // must re-parse, and must parse to the same objects as the bytes we
    // started from. The literals differ; every `f32` lopdf holds does not.
    let before = Document::load_mem(out).ok()?;
    let Ok(after) = Document::load_mem(&patched) else {
        if dbg { eprintln!("patched failed to load"); }
        return None;
    };
    if !same_objects(&before, &after) {
        if dbg {
            eprintln!("objects differ: {} vs {}", before.objects.len(), after.objects.len());
            if before.trailer != after.trailer { eprintln!("trailer differs"); }
            for (id, l) in before.objects.iter() {
                match after.objects.get(id) {
                    Some(r) if !equivalent(l, r) => {
                        eprintln!("  {:?}\n   L={:?}\n   R={:?}", id, l, r);
                        break;
                    }
                    None => { eprintln!("  missing {:?}", id); break; }
                    _ => {}
                }
            }
        }
        return None;
    }
    Some(patched)
}

/// Rebuild one object stream with its reals restored.
///
/// Yields edits for the deflated payload plus the `/Length` and `/First` values
/// that describe it, or `None` when nothing inside it changed (or the stream is
/// not shaped the way lopdf writes one).
fn objstm_edits(out: &[u8], tag_at: usize, literals: &RealLiterals) -> Option<Vec<Edit>> {
    let dbg = std::env::var_os("AMATL_DEBUG_REALS").is_some();
    let Some((dict_start, dict_end, content_start, content_len)) = locate_stream(out, tag_at) else {
        if dbg { eprintln!("objstm: locate_stream failed"); }
        return None;
    };
    let dict = out.get(dict_start..dict_end)?;
    if find_sub(dict, b"/Filter/FlateDecode", 0).is_none()
        || find_sub(dict, b"/DecodeParms", 0).is_some()
    {
        return None;
    }
    let content = out.get(content_start..content_start.checked_add(content_len)?)?;
    let payload = inflate_capped(content, MAX_REDEFLATE_BYTES).or_else(|| { if dbg { eprintln!("objstm: inflate failed"); } None })?;

    let count = usize::try_from(int_value(dict, b"/N ")?.0).ok()?;
    let first = usize::try_from(int_value(dict, b"/First ")?.0).ok()?;
    let header = std::str::from_utf8(payload.get(..first)?).ok()?;
    let mut pairs = header.split_ascii_whitespace();
    let mut ids: Vec<i64> = Vec::with_capacity(count);
    let mut offsets: Vec<usize> = Vec::with_capacity(count);
    for _ in 0..count {
        ids.push(pairs.next()?.parse().ok()?);
        offsets.push(pairs.next()?.parse().ok()?);
    }
    if pairs.next().is_some() || offsets.first() != Some(&0) {
        if dbg { eprintln!("objstm: header shape n={count} first={first}"); }
        return None;
    }

    // Slice the payload into per-object bodies and restore inside each. The
    // slices are contiguous, so whatever separators lopdf wrote between
    // objects travel along with the body they follow.
    let bodies_at = first;
    let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(count);
    let mut changed = false;
    for i in 0..offsets.len() {
        let start = bodies_at.checked_add(offsets[i])?;
        let end = match offsets.get(i + 1) {
            Some(next) => bodies_at.checked_add(*next)?,
            None => payload.len(),
        };
        let body = payload.get(start..end)?;
        let mut inner: Vec<Edit> = Vec::new();
        scan_reals_positions(body, false, &mut |at, token| {
            if let Some(text) = literals.replacement(token) {
                inner.push(Edit {
                    at,
                    len: token.len(),
                    text: text.to_vec(),
                });
            }
        });
        if inner.is_empty() {
            bodies.push(body.to_vec());
        } else {
            changed = true;
            bodies.push(apply_edits(body, &inner));
        }
    }
    if !changed {
        if dbg { eprintln!("objstm: no body changed ({} bodies)", bodies.len()); }
        return None;
    }

    // Re-emit the offset header. Offsets are relative to `/First`, so the
    // header's own new length only moves `/First` itself.
    let mut new_header: Vec<u8> = Vec::with_capacity(first);
    let mut running = 0usize;
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            new_header.push(b' ');
        }
        new_header.extend_from_slice(id.to_string().as_bytes());
        new_header.push(b' ');
        new_header.extend_from_slice(running.to_string().as_bytes());
        running = running.checked_add(bodies[i].len())?;
    }
    new_header.push(b'\n');

    let mut new_payload = new_header;
    let new_first = new_payload.len();
    for body in &bodies {
        new_payload.extend_from_slice(body);
    }
    if dbg { eprintln!("objstm: rebuilt payload {} bytes", new_payload.len()); }
    let new_content = deflate_level9(&new_payload).or_else(|| { if dbg { eprintln!("objstm: deflate failed"); } None })?;
    if inflate_capped(&new_content, new_payload.len())?.as_slice() != new_payload.as_slice() {
        if dbg { eprintln!("objstm: inflate-back mismatch"); }
        return None;
    }
    if dbg { eprintln!("objstm: new content {} bytes (was {})", new_content.len(), content_len); }

    let (_, len_at, len_len) = int_value(dict, b"/Length ")?;
    let (_, first_at, first_len) = int_value(dict, b"/First ")?;
    Some(vec![
        Edit {
            at: dict_start + len_at,
            len: len_len,
            text: new_content.len().to_string().into_bytes(),
        },
        Edit {
            at: dict_start + first_at,
            len: first_len,
            text: new_first.to_string().into_bytes(),
        },
        Edit {
            at: content_start,
            len: content_len,
            text: new_content,
        },
    ])
}

/// `(dict_start, dict_end, content_start, content_len)` for the stream object
/// whose dictionary contains `inside`. Shaped for exactly what lopdf's writer
/// emits: `<id> <gen> obj\n<<...>>stream\n<payload>\nendstream`.
fn locate_stream(out: &[u8], inside: usize) -> Option<(usize, usize, usize, usize)> {
    let hdr = out
        .get(..inside)?
        .windows(b" obj\n<<".len())
        .rposition(|w| w == b" obj\n<<")?;
    let dict_start = hdr + b" obj\n".len();
    let dict_end = find_sub(out, b">>stream\n", dict_start)? + 2;
    if inside >= dict_end {
        return None;
    }
    let content_start = dict_end + b"stream\n".len();
    let dict = out.get(dict_start..dict_end)?;
    let (len, _, _) = int_value(dict, b"/Length ")?;
    let content_len = usize::try_from(len).ok()?;
    let content_end = content_start.checked_add(content_len)?;
    if !out.get(content_end..)?.starts_with(b"\nendstream") {
        return None;
    }
    Some((dict_start, dict_end, content_start, content_len))
}

/// `(value, index_just_past_the_digits)` for the non-negative integer at `at`.
fn parse_int_at(data: &[u8], at: usize) -> Option<(i64, usize)> {
    let mut end = at;
    while data.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if end == at {
        return None;
    }
    let value = std::str::from_utf8(data.get(at..end)?).ok()?.parse().ok()?;
    Some((value, end))
}

/// `(value, offset_of_digits_in_dict, digit_count)` for `key` followed by a
/// non-negative integer.
fn int_value(dict: &[u8], key: &[u8]) -> Option<(i64, usize, usize)> {
    let at = find_sub(dict, key, 0)? + key.len();
    let mut end = at;
    while dict.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if end == at {
        return None;
    }
    let value: i64 = std::str::from_utf8(dict.get(at..end)?).ok()?.parse().ok()?;
    Some((value, at, end - at))
}

/// Splice `edits` (sorted, non-overlapping) into `data`.
fn apply_edits(data: &[u8], edits: &[Edit]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut cursor = 0;
    for edit in edits {
        out.extend_from_slice(&data[cursor..edit.at]);
        out.extend_from_slice(&edit.text);
        cursor = edit.at + edit.len;
    }
    out.extend_from_slice(&data[cursor..]);
    out
}

/// Where a byte offset into the pre-patch file lands after the edits.
/// `None` if the offset points *inside* something that was rewritten.
fn shift(edits: &[Edit], offset: usize) -> Option<usize> {
    let mut moved = offset as i64;
    for edit in edits {
        if edit.at + edit.len <= offset {
            moved += edit.text.len() as i64 - edit.len as i64;
        } else if edit.at < offset {
            return None;
        } else {
            break;
        }
    }
    usize::try_from(moved).ok()
}

/// Rewrite every byte offset the patch invalidated: the cross-reference
/// section's object offsets and the trailing `startxref`.
fn repair_offsets(patched: Vec<u8>, original: &[u8], edits: &[Edit]) -> Option<Vec<u8>> {
    let sx = original.windows(9).rposition(|w| w == b"startxref")?;
    let mut p = sx + 9;
    while original.get(p).is_some_and(u8::is_ascii_whitespace) {
        p += 1;
    }
    let digits_at = p;
    let mut digits_end = p;
    while original.get(digits_end).is_some_and(u8::is_ascii_digit) {
        digits_end += 1;
    }
    if digits_end == digits_at {
        return None;
    }
    let xref_at: usize = std::str::from_utf8(original.get(digits_at..digits_end)?)
        .ok()?
        .parse()
        .ok()?;

    // The cross-reference section is the last thing lopdf writes, so it sits
    // after every edit and moves as a whole.
    let new_xref_at = shift(edits, xref_at)?;
    let sx_shift = shift(edits, digits_at)?;

    let mut patched = patched;
    if original.get(xref_at..xref_at + 4) == Some(b"xref") {
        rewrite_classic_xref(&mut patched, new_xref_at, edits)?;
    } else {
        rewrite_xref_stream(&mut patched, new_xref_at, edits)?;
    }

    // `startxref`'s own digit count can change; splice it last so the offsets
    // computed above are still valid while the table is rewritten.
    let tail_edit = Edit {
        at: sx_shift,
        len: digits_end - digits_at,
        text: new_xref_at.to_string().into_bytes(),
    };
    Some(apply_edits(&patched, std::slice::from_ref(&tail_edit)))
}

/// Classic `xref` table: fixed-width `nnnnnnnnnn ggggg n` rows, so every
/// offset is rewritten in place.
fn rewrite_classic_xref(patched: &mut [u8], xref_at: usize, edits: &[Edit]) -> Option<()> {
    let mut p = xref_at + 4;
    loop {
        while patched.get(p).is_some_and(u8::is_ascii_whitespace) {
            p += 1;
        }
        if patched.get(p..p + 7) == Some(b"trailer") {
            return Some(());
        }
        // "<start> <count>" subsection header
        let (_, start_end) = parse_int_at(patched, p)?;
        if patched.get(start_end) != Some(&b' ') {
            return None;
        }
        let (count, mut q) = parse_int_at(patched, start_end + 1)?;
        while patched.get(q).is_some_and(u8::is_ascii_whitespace) {
            q += 1;
        }
        for _ in 0..count {
            let row = patched.get(q..q + 20)?;
            if row[17] == b'n' {
                let offset: usize = std::str::from_utf8(&row[..10]).ok()?.parse().ok()?;
                let moved = shift(edits, offset)?;
                if moved > 9_999_999_999 {
                    return None;
                }
                let text = format!("{moved:010}");
                patched.get_mut(q..q + 10)?.copy_from_slice(text.as_bytes());
            } else if row[17] != b'f' {
                return None;
            }
            q += 20;
        }
        p = q;
    }
}

/// Cross-reference stream, still uncompressed at this point in the save (the
/// crate deflates it in a later pass). Type-1 rows carry byte offsets.
fn rewrite_xref_stream(patched: &mut [u8], xref_at: usize, edits: &[Edit]) -> Option<()> {
    let mut q = xref_at;
    while patched.get(q).is_some_and(u8::is_ascii_digit) {
        q += 1;
    }
    if patched.get(q) != Some(&b' ') {
        return None;
    }
    q += 1;
    while patched.get(q).is_some_and(u8::is_ascii_digit) {
        q += 1;
    }
    if patched.get(q..q + 5)? != b" obj\n" {
        return None;
    }
    let dict_start = q + 5;
    if patched.get(dict_start..dict_start + 2)? != b"<<" {
        return None;
    }
    let dict_end = find_sub(patched, b">>stream\n", dict_start)? + 2;
    let dict = patched.get(dict_start..dict_end)?.to_vec();
    if find_sub(&dict, b"/Type/XRef", 0).is_none() || find_sub(&dict, b"/Filter", 0).is_some() {
        return None;
    }
    let content_start = dict_end + b"stream\n".len();
    let (len, _, _) = int_value(&dict, b"/Length ")?;
    let content_len = usize::try_from(len).ok()?;
    let content_end = content_start.checked_add(content_len)?;
    if !patched.get(content_end..)?.starts_with(b"\nendstream") {
        return None;
    }

    let widths = width_array(&dict)?;
    let [w0, w1, w2] = widths.as_slice() else {
        return None;
    };
    if *w0 != 1 || *w1 == 0 || *w1 > 8 {
        return None;
    }
    let row = w0 + w1 + w2;
    let content = patched.get_mut(content_start..content_end)?;
    if row == 0 || content.len() % row != 0 {
        return None;
    }
    for chunk in content.chunks_mut(row) {
        if chunk[0] != 1 {
            continue;
        }
        let mut offset: u64 = 0;
        for &b in &chunk[*w0..w0 + w1] {
            offset = (offset << 8) | u64::from(b);
        }
        let moved = shift(edits, usize::try_from(offset).ok()?)? as u64;
        let mut value = moved;
        for b in chunk[*w0..w0 + w1].iter_mut().rev() {
            *b = (value & 0xff) as u8;
            value >>= 8;
        }
        if value != 0 {
            return None;
        }
    }
    Some(())
}

/// `/W[a b c]` out of a cross-reference stream dictionary.
fn width_array(dict: &[u8]) -> Option<Vec<usize>> {
    let at = find_sub(dict, b"/W[", 0)? + b"/W[".len();
    let close = find_sub(dict, b"]", at)?;
    std::str::from_utf8(dict.get(at..close)?)
        .ok()?
        .split_ascii_whitespace()
        .map(|t| t.parse().ok())
        .collect()
}

/// Do two parsed documents hold the same objects?
///
/// `Stream::start_position` records where the payload sat in the file, which
/// this patch moves by design, so streams are compared on their dictionary and
/// content only.
fn same_objects(a: &Document, b: &Document) -> bool {
    if a.objects.len() != b.objects.len() || a.trailer != b.trailer {
        return false;
    }
    a.objects.iter().all(|(id, left)| match b.objects.get(id) {
        // The object stream and the cross-reference stream are the containers
        // this patch deliberately rewrites, so their own bytes differ by
        // construction. Skipping them costs nothing: lopdf hands back every
        // object they carry as a first-class entry of `objects`, and each of
        // those IS compared — which is the stronger statement anyway.
        Some(_) if is_container(left) => true,
        Some(right) => equivalent(left, right),
        None => false,
    })
}

/// An `ObjStm` or `XRef` stream — file structure rather than document content.
fn is_container(object: &Object) -> bool {
    matches!(object, Object::Stream(stream)
        if stream.dict.has_type(b"ObjStm") || stream.dict.has_type(b"XRef"))
}

fn equivalent(left: &Object, right: &Object) -> bool {
    match (left, right) {
        (Object::Stream(l), Object::Stream(r)) => l.dict == r.dict && l.content == r.content,
        (Object::Array(l), Object::Array(r)) => {
            l.len() == r.len() && l.iter().zip(r).all(|(a, b)| equivalent(a, b))
        }
        (Object::Dictionary(l), Object::Dictionary(r)) => {
            l.len() == r.len()
                && l.iter()
                    .zip(r.iter())
                    .all(|((lk, lv), (rk, rv))| lk == rk && equivalent(lv, rv))
        }
        _ => left == right,
    }
}

/// Inflated payload of every `FlateDecode` object stream in a raw PDF.
///
/// Deliberately crude: `/Length` may be an indirect reference at this point, so
/// the payload is handed to the inflater from `stream` onwards and the zlib
/// stream ends where it ends. Anything that does not inflate is skipped.
fn objstm_payloads(input: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(tag_at) = find_sub(input, b"/ObjStm", from) {
        from = tag_at + 1;
        let Some(dict_start) = input
            .get(..tag_at)
            .and_then(|head| head.windows(3).rposition(|w| w == b"obj"))
        else {
            continue;
        };
        let Some(stream_at) = find_sub(input, b"stream", tag_at) else {
            continue;
        };
        let Some(dict) = input.get(dict_start..stream_at) else {
            continue;
        };
        if find_sub(dict, b"FlateDecode", 0).is_none() || find_sub(dict, b"/DecodeParms", 0).is_some()
        {
            continue;
        }
        let mut at = stream_at + b"stream".len();
        if input.get(at) == Some(&b'\r') {
            at += 1;
        }
        if input.get(at) == Some(&b'\n') {
            at += 1;
        }
        let Some(rest) = input.get(at..) else {
            continue;
        };
        if let Some(payload) = inflate_capped(rest, MAX_REDEFLATE_BYTES) {
            out.push(payload);
        }
    }
    out
}

/// Call `on_real` with the text of every real-number token in `data`.
fn scan_reals(data: &[u8], skip_streams: bool, on_real: &mut dyn FnMut(&[u8])) {
    scan_reals_positions(data, skip_streams, &mut |_, token| on_real(token));
}

/// Walk PDF syntax and report every real-number token as `(offset, text)`.
///
/// Comments, literal strings and hex strings are skipped so their contents
/// cannot be mistaken for numbers (pdfTeX's `/PTEX.Fullbanner` famously
/// contains `3.14159265`). With `skip_streams`, stream payloads are skipped
/// too — binary image data is full of byte sequences that look like reals.
/// This is a lexer, not a parser: it has no opinion about dictionaries,
/// because the restoration is keyed by value rather than by key.
fn scan_reals_positions(data: &[u8], skip_streams: bool, on_real: &mut dyn FnMut(usize, &[u8])) {
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            b'%' => {
                while i < data.len() && data[i] != b'\n' && data[i] != b'\r' {
                    i += 1;
                }
            }
            b'(' => {
                let mut depth = 1usize;
                i += 1;
                while i < data.len() && depth > 0 {
                    match data[i] {
                        b'\\' => i += 2,
                        b'(' => {
                            depth += 1;
                            i += 1;
                        }
                        b')' => {
                            depth -= 1;
                            i += 1;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'<' => {
                if data.get(i + 1) == Some(&b'<') {
                    i += 2;
                } else {
                    i += 1;
                    while i < data.len() && data[i] != b'>' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b's' if skip_streams
                && data[i..].starts_with(b"stream")
                && is_boundary(data, i)
                && data.get(i + b"stream".len()).is_some_and(|b| {
                    matches!(*b, b'\r' | b'\n')
                }) =>
            {
                match find_sub(data, b"endstream", i) {
                    Some(end) => i = end + b"endstream".len(),
                    None => return,
                }
            }
            b'0'..=b'9' | b'+' | b'-' | b'.' if is_boundary(data, i) => {
                let start = i;
                while i < data.len() && matches!(data[i], b'0'..=b'9' | b'+' | b'-' | b'.') {
                    i += 1;
                }
                // A token glued to a letter (`1e-5`, `12R`) is not a plain
                // real; leave it to whoever wrote it.
                if i >= data.len() || is_delimiter(data[i]) {
                    on_real(start, &data[start..i]);
                }
            }
            _ => i += 1,
        }
    }
}

/// Is the byte before `at` a delimiter, i.e. does a token start here?
fn is_boundary(data: &[u8], at: usize) -> bool {
    at == 0 || is_delimiter(data[at - 1])
}

/// PDF white-space and delimiter characters (ISO 32000-1 table 1 and 2).
fn is_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || byte == 0
        || byte == 0x0c
        || matches!(
            byte,
            b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
        )
}

#[doc(hidden)]
pub(crate) fn debug_probe(input: &[u8]) {
    let lits = capture(input);
    eprintln!("captured {} literals", lits.exact.len());
    for (i, (bits, text)) in lits.exact.iter().enumerate().take(8) {
        eprintln!("  {} -> {}", f32::from_bits(*bits), String::from_utf8_lossy(text));
        let _ = i;
    }
    let out = crate::optimize(input);
    let mut n = 0;
    scan_reals_positions(&out, true, &mut |_, tok| {
        if lits.replacement(tok).is_some() { n += 1 }
    });
    eprintln!("plain tokens still needing restore: {n}");
    let mut m = 0;
    for payload in objstm_payloads(&out) {
        scan_reals_positions(&payload, false, &mut |_, tok| {
            if lits.replacement(tok).is_some() { m += 1 }
        });
    }
    eprintln!("objstm tokens still needing restore: {m}");
    let patched = restore(out.clone(), &lits);
    eprintln!("restore: {} -> {} bytes", out.len(), patched.len());
}
