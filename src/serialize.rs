//! Document serialization: the two save paths and their post-passes.
//!
//! `save_document` is the single exit every optimized document takes. The
//! classic path is lopdf's own writer; the packed path adds PDF 1.5 object
//! streams and a binary xref stream. Both finish with byte-level post-passes
//! (xref-stream deflate, `ObjStm` zopfli re-compression, real-literal splicing)
//! that each re-parse their own output before returning it.

use lopdf::Document;

use crate::deflate::{
    deflate_backend, deflate_zopfli, inflate_capped, png_defilter, png_up_filter,
};
use crate::{reals, DeflateBackend, OptimizeOptions, MAX_REDEFLATE_BYTES};

/// Serialize the document, optionally using PDF 1.5 object-stream packing when
/// `options.pack_object_streams` is true. The packed path produces smaller
/// output for object-heavy documents but is more complex; the classic path is
/// the always-available fallback and matches what lopdf ships.
///
/// Either path finishes by splicing the input's own real literals back in (see
/// [`reals`]), so f32 parsing never changes a number's printed value.
pub(crate) fn save_document(
    doc: &mut Document,
    options: OptimizeOptions,
    literals: &reals::RealLiterals,
) -> Result<Vec<u8>, lopdf::Error> {
    let out = if options.pack_object_streams {
        pack_and_save(doc)?
    } else {
        let mut out: Vec<u8> = Vec::new();
        doc.save_to(&mut out)?;
        out
    };
    // Put back every real literal lopdf's f32 object model rounded off, while
    // the cross-reference section this shifts is still uncompressed. A no-op
    // unless the input actually carried a literal f32 cannot hold.
    let out = reals::restore(out, literals);
    // The zopfli backend can re-deflate the ObjStm the writer just emitted
    // (lopdf deflates it internally at zlib level 9, out of reach of the
    // final re-deflate pass). Must run BEFORE the xref compression below:
    // the patch reads and rewrites the still-uncompressed xref rows.
    let out = if options.deflate_backend == DeflateBackend::Zopfli {
        rezopfli_objstm(out)
    } else {
        out
    };
    // Both save paths emit an uncompressed cross-reference stream; deflate it
    // (PNG-Up-predicted when that is smaller). Strictly LAST: `reals::restore`
    // and `rezopfli_objstm` above both rewrite raw xref rows in place and
    // decline any xref stream that already carries a `/Filter`, so neither may
    // ever see the compressed or predicted form.
    Ok(compress_xref_stream(out, options.deflate_backend))
}

/// Deflate the cross-reference stream of a just-serialized document.
///
/// lopdf hardcodes `XRefStreamFilter::None` in
/// `writer.rs::write_cross_reference_stream` — there is no `SaveOptions` knob
/// for it, and `doc.compress()` cannot reach the object because it does not
/// exist in `Document::objects`: the writer synthesizes it during save, after
/// every document-level pass has run. So picamatl's output shipped a raw
/// 7-bytes-per-entry xref stream (17,479 B on the 2,497-object NASA reference,
/// where Ghostscript spends 5,375 B).
///
/// Two candidates are built from the same raw rows: plain deflate, and deflate
/// over PNG Up-filtered rows declared as `/DecodeParms<</Columns <W sum>
/// /Predictor 12>>`. The predicted one ships only when strictly smaller; both
/// are decoded back to the exact raw rows first.
///
/// Patching the saved bytes is sound *because it is the last object in the
/// file*: `startxref` points at its start offset, and its own xref entry
/// records that same start offset, so rewriting only its dictionary and
/// payload moves no offset that anything records. Nothing before `xref_start`
/// is touched.
///
/// Fail-safe: every structural assumption is checked against the bytes actually
/// present (object header, `>>stream`, the declared `/Length`, the closing
/// `endstream`, and a full inflate-back of the new payload). Any mismatch —
/// including a classic cross-reference *table*, which has nothing to compress —
/// returns the input unchanged.
fn compress_xref_stream(out: Vec<u8>, backend: DeflateBackend) -> Vec<u8> {
    match try_compress_xref_stream(&out, backend) {
        Some(patched) => patched,
        None => out,
    }
}

fn try_compress_xref_stream(out: &[u8], backend: DeflateBackend) -> Option<Vec<u8>> {
    // `\nstartxref\n<offset>\n%%EOF` is the last thing the writer emits.
    let sx = out.windows(9).rposition(|w| w == b"startxref")?;
    let mut p = sx + 9;
    while out.get(p).is_some_and(|b| b.is_ascii_whitespace()) {
        p += 1;
    }
    let digits = p;
    while out.get(p).is_some_and(|b| b.is_ascii_digit()) {
        p += 1;
    }
    let xref_start: usize = std::str::from_utf8(out.get(digits..p)?)
        .ok()?
        .parse()
        .ok()?;
    if xref_start >= sx {
        return None;
    }

    // "<id> <gen> obj\n<<" — exactly what Writer::write_indirect_object emits
    // ahead of a stream object. Anything else (notably a `xref` table keyword)
    // means there is no xref stream here.
    let mut q = skip_ascii_digits(out, xref_start)?;
    if out.get(q) != Some(&b' ') {
        return None;
    }
    q = skip_ascii_digits(out, q + 1)?;
    if out.get(q..q + 5)? != b" obj\n" {
        return None;
    }
    let dict_start = q + 5;
    if out.get(dict_start..dict_start + 2)? != b"<<" {
        return None;
    }

    // Writer::write_stream emits the dictionary and then `stream\n` with no
    // separator, so the dictionary ends at the first `>>stream\n`.
    let dict_end = find_sub(out, b">>stream\n", dict_start)? + 2;
    let content_start = dict_end + b"stream\n".len();
    let dict = out.get(dict_start..dict_end)?;
    // A `/DecodeParms` already in the dictionary would be read as describing
    // the payload this patch is about to install — it does not (this patch
    // writes its own, or none), so decline rather than emit a dictionary that
    // lies about its own stream.
    if find_sub(dict, b"/Type/XRef", 0).is_none()
        || find_sub(dict, b"/Filter", 0).is_some()
        || find_sub(dict, b"/DecodeParms", 0).is_some()
    {
        return None;
    }

    // `/Length <n>`: names are written unescaped and integer values get exactly
    // one leading space, so the trailing space also rules out `/Length1`.
    let len_key = find_sub(dict, b"/Length ", 0)?;
    let val_start = len_key + b"/Length ".len();
    let val_end = skip_ascii_digits(dict, val_start)?;
    let content_len: usize = std::str::from_utf8(dict.get(val_start..val_end)?)
        .ok()?
        .parse()
        .ok()?;

    // Cross-check the declared length against the real framing before touching
    // anything: if `endstream` is not exactly there, this is not the object we
    // think it is.
    let content = out.get(content_start..content_start.checked_add(content_len)?)?;
    if !out
        .get(content_start + content_len..)?
        .starts_with(b"\nendstream")
    {
        return None;
    }

    let deflated = deflate_backend(content, backend)?;
    if deflated.len() >= content.len() {
        return None; // never-larger, per the crate contract
    }
    if inflate_capped(&deflated, content.len())?.as_slice() != content {
        return None;
    }

    let patch_with = |payload: &[u8], decode_parms: &[u8]| {
        let mut patched = Vec::with_capacity(out.len());
        patched.extend_from_slice(&out[..dict_start + len_key]);
        patched.extend_from_slice(decode_parms);
        patched.extend_from_slice(b"/Filter/FlateDecode/Length ");
        patched.extend_from_slice(payload.len().to_string().as_bytes());
        patched.extend_from_slice(&out[dict_start + val_end..content_start]);
        patched.extend_from_slice(payload);
        patched.extend_from_slice(&out[content_start + content_len..]);
        patched
    };
    let plain = patch_with(&deflated, b"");

    // The PNG-Up-predicted candidate. Consecutive rows differ only in their
    // low offset bytes (type 1) or a +1 index (type 2), so Up turns the table
    // into long zero runs. It ships only when the WHOLE patched file is
    // strictly smaller than the plain one — its `/DecodeParms` entry costs
    // ~40 B of dictionary, which a short table's payload saving can miss.
    match predicted_xref_payload(dict, content, backend) {
        Some((payload, parms)) => {
            let predicted = patch_with(&payload, &parms);
            Some(if predicted.len() < plain.len() {
                predicted
            } else {
                plain
            })
        }
        None => Some(plain),
    }
}

/// Deflate the raw xref rows under PNG Up prediction (`/Predictor 12`, one
/// filter-tag byte per row) and return the payload together with the exact
/// `/DecodeParms` entry that describes it.
///
/// The row width comes from the dictionary's own `/W` and must tile the
/// payload exactly; the candidate is decoded back (inflate, then the crate's
/// spec-correct PNG defilter, not lopdf's) and must reproduce `content`
/// byte-for-byte. Any doubt returns `None` and the plain deflate ships.
fn predicted_xref_payload(
    dict: &[u8],
    content: &[u8],
    backend: DeflateBackend,
) -> Option<(Vec<u8>, Vec<u8>)> {
    let widths = parse_int_array(dict, b"/W[")?;
    if widths.len() != 3 || widths.iter().any(|&w| w > 8) {
        return None;
    }
    let row: usize = widths.iter().sum();
    if row == 0 || content.is_empty() || !content.len().is_multiple_of(row) {
        return None;
    }
    let filtered = png_up_filter(content, u32::try_from(row).ok()?, 1);
    if filtered.len() != content.len() + content.len() / row {
        return None;
    }
    let payload = deflate_backend(&filtered, backend)?;
    let inflated = inflate_capped(&payload, filtered.len())?;
    if png_defilter(&inflated, 1, row)?.as_slice() != content {
        return None;
    }
    let parms = format!("/DecodeParms<</Columns {row}/Predictor 12>>").into_bytes();
    Some((payload, parms))
}

/// Re-deflate every just-written `ObjStm` payload with zopfli.
///
/// lopdf's writer deflates the object stream itself (zlib level 9) during
/// `save_with_options`, after every document-level pass has run, so the final
/// re-deflate pass never sees it. On the NASA reference the payload is
/// dictionary-heavy text that zopfli encodes 22% smaller (41,322 → 32,148 B).
///
/// Patching the saved bytes is sound because everything the shrink
/// invalidates is rewritten in the same patch: the ObjStm's `/Length`, the
/// type-1 offsets in the (still uncompressed) cross-reference stream for
/// every object that starts after the ObjStm, and the `startxref` offset.
/// Type-2 entries are (stream id, index) pairs — no byte offsets — and the
/// payload inflates back byte-identically, so `/N`/`/First` still hold.
///
/// Fail-safe: every structural assumption is checked against the bytes
/// actually present, the new payload must be strictly smaller AND inflate
/// back byte-identically, no xref offset may point inside the patched
/// region, and the patched file must re-parse with lopdf (which walks every
/// rewritten offset). Any doubt returns the input unchanged.
fn rezopfli_objstm(out: Vec<u8>) -> Vec<u8> {
    // Files above `max_objects_per_stream` objects get several ObjStms, so patch
    // every one of them. Strictly last-to-first: a patch only ever shifts bytes
    // *after* the stream it rewrites, so the tag positions of the streams still
    // to come — all of which sit earlier — stay valid in the patched buffer.
    // Each patch is independently validated and re-parsed, so a stream that
    // declines simply stays zlib without affecting the others.
    let tag = b"/Type/ObjStm";
    let mut tags = Vec::new();
    let mut from = 0;
    while let Some(at) = find_sub(&out, tag, from) {
        tags.push(at);
        from = at + tag.len();
    }
    let mut cur = out;
    for &tag_at in tags.iter().rev() {
        if let Some(patched) = try_rezopfli_objstm(&cur, tag_at) {
            cur = patched;
        }
    }
    cur
}

/// Re-deflate the single `ObjStm` whose `/Type/ObjStm` token sits at `tag_at`.
fn try_rezopfli_objstm(out: &[u8], tag_at: usize) -> Option<Vec<u8>> {
    // "<id> <gen> obj\n<<" — the dict opens right after the object header,
    // and the /Type/ObjStm entry must sit inside THIS dict.
    let hdr = rfind_sub(out, b" obj\n<<", tag_at)?;
    let dict_start = hdr + b" obj\n".len();
    let obj_start = {
        // Walk back over "<id> <gen>": generation digits, one space, id digits.
        let mut i = hdr;
        while i > 0 && out[i - 1].is_ascii_digit() {
            i -= 1;
        }
        let gen_start = i;
        if gen_start == hdr || i == 0 || out[i - 1] != b' ' {
            return None;
        }
        i -= 1;
        let id_end = i;
        while i > 0 && out[i - 1].is_ascii_digit() {
            i -= 1;
        }
        if i == id_end {
            return None;
        }
        i
    };
    let dict_end = find_sub(out, b">>stream\n", dict_start)? + 2;
    if tag_at >= dict_end {
        return None;
    }
    let dict = out.get(dict_start..dict_end)?;
    // Plain Flate with no predictor: exactly what lopdf's writer emits.
    if find_sub(dict, b"/Filter/FlateDecode", 0).is_none()
        || find_sub(dict, b"/DecodeParms", 0).is_some()
    {
        return None;
    }
    let content_start = dict_end + b"stream\n".len();
    let len_key = find_sub(dict, b"/Length ", 0)?;
    let val_start = len_key + b"/Length ".len();
    let val_end = skip_ascii_digits(dict, val_start)?;
    let content_len: usize = std::str::from_utf8(dict.get(val_start..val_end)?)
        .ok()?
        .parse()
        .ok()?;
    let content = out.get(content_start..content_start.checked_add(content_len)?)?;
    let content_end = content_start + content_len;
    if !out.get(content_end..)?.starts_with(b"\nendstream") {
        return None;
    }

    // The rewrite itself, guarded like every other deflate in the crate.
    let plain = inflate_capped(content, MAX_REDEFLATE_BYTES)?;
    let new_content = deflate_zopfli(&plain)?;
    if new_content.len() >= content.len() {
        return None;
    }
    if inflate_capped(&new_content, plain.len())?.as_slice() != plain.as_slice() {
        return None;
    }
    let new_digits = new_content.len().to_string();
    // Both terms shrink or hold: the payload is strictly smaller and its
    // /Length value therefore never gains digits.
    let delta = (content.len() - new_content.len()) + (val_end - val_start - new_digits.len());

    // Locate the (still uncompressed) cross-reference stream via startxref.
    // It must live after the ObjStm — the patch shifts everything past the
    // patched region. lopdf's writer always emits it last.
    let sx = out.windows(9).rposition(|w| w == b"startxref")?;
    let mut p = sx + 9;
    while out.get(p).is_some_and(|b| b.is_ascii_whitespace()) {
        p += 1;
    }
    let sx_digits = p;
    let sx_end = skip_ascii_digits(out, sx_digits)?;
    let xref_start: usize = std::str::from_utf8(out.get(sx_digits..sx_end)?)
        .ok()?
        .parse()
        .ok()?;
    if xref_start <= content_end || xref_start >= sx {
        return None;
    }
    let mut q = skip_ascii_digits(out, xref_start)?;
    if out.get(q) != Some(&b' ') {
        return None;
    }
    q = skip_ascii_digits(out, q + 1)?;
    if out.get(q..q + 5)? != b" obj\n" {
        return None;
    }
    let xdict_start = q + 5;
    if out.get(xdict_start..xdict_start + 2)? != b"<<" {
        return None;
    }
    let xdict_end = find_sub(out, b">>stream\n", xdict_start)? + 2;
    let xdict = out.get(xdict_start..xdict_end)?;
    if find_sub(xdict, b"/Type/XRef", 0).is_none() || find_sub(xdict, b"/Filter", 0).is_some() {
        return None;
    }
    let xcontent_start = xdict_end + b"stream\n".len();
    let xlen_key = find_sub(xdict, b"/Length ", 0)?;
    let xval_start = xlen_key + b"/Length ".len();
    let xval_end = skip_ascii_digits(xdict, xval_start)?;
    let xcontent_len: usize = std::str::from_utf8(xdict.get(xval_start..xval_end)?)
        .ok()?
        .parse()
        .ok()?;
    let xcontent = out.get(xcontent_start..xcontent_start.checked_add(xcontent_len)?)?;
    let xcontent_end = xcontent_start + xcontent_len;
    if !out.get(xcontent_end..)?.starts_with(b"\nendstream") {
        return None;
    }

    // /W[1 n 2]-style row layout: a 1-byte type field is what lopdf writes,
    // and required here to tell offset rows (type 1) from the rest.
    let w = parse_int_array(xdict, b"/W[")?;
    let [w0, w1, w2] = w.as_slice() else {
        return None;
    };
    if *w0 != 1 || *w1 == 0 || *w1 > 8 {
        return None;
    }
    let row = w0 + w1 + w2;
    if row == 0 || xcontent.len() % row != 0 {
        return None;
    }

    // Rewrite the type-1 offsets. An offset at or before the ObjStm header is
    // untouched; one past the patched region shifts back by `delta`; one
    // INSIDE the ObjStm object means the file is not shaped the way this
    // patch assumes, so hand it back untouched.
    let mut new_xcontent = xcontent.to_vec();
    for chunk in new_xcontent.chunks_mut(row) {
        if chunk[0] != 1 {
            continue;
        }
        let mut off: u64 = 0;
        for &b in &chunk[*w0..w0 + w1] {
            off = (off << 8) | u64::from(b);
        }
        let off = usize::try_from(off).ok()?;
        if off <= obj_start {
            continue;
        }
        if off < content_end {
            return None;
        }
        let mut v = (off - delta) as u64;
        for b in chunk[*w0..w0 + w1].iter_mut().rev() {
            *b = (v & 0xff) as u8;
            v >>= 8;
        }
        if v != 0 {
            return None;
        }
    }

    let mut patched = Vec::with_capacity(out.len() - delta);
    patched.extend_from_slice(&out[..dict_start + val_start]);
    patched.extend_from_slice(new_digits.as_bytes());
    patched.extend_from_slice(&out[dict_start + val_end..content_start]);
    patched.extend_from_slice(&new_content);
    patched.extend_from_slice(&out[content_end..xcontent_start]);
    patched.extend_from_slice(&new_xcontent);
    patched.extend_from_slice(&out[xcontent_end..sx_digits]);
    patched.extend_from_slice((xref_start - delta).to_string().as_bytes());
    patched.extend_from_slice(&out[sx_end..]);

    // Final proof: lopdf must re-parse the patched file, which walks every
    // rewritten offset and re-reads every packed object out of the new
    // payload. Anything short of a full parse means no patch.
    Document::load_mem(&patched).ok()?;
    Some(patched)
}

/// Last index of `needle` in `haystack` strictly before `before`.
fn rfind_sub(haystack: &[u8], needle: &[u8], before: usize) -> Option<usize> {
    haystack
        .get(..before)?
        .windows(needle.len())
        .rposition(|w| w == needle)
}

/// Parse `key[int int ...]` out of a dictionary's bytes (lopdf's writer emits
/// integer arrays with single spaces and no line breaks).
fn parse_int_array(dict: &[u8], key: &[u8]) -> Option<Vec<usize>> {
    let at = find_sub(dict, key, 0)?;
    let close = find_sub(dict, b"]", at)?;
    std::str::from_utf8(dict.get(at + key.len()..close)?)
        .ok()?
        .split_ascii_whitespace()
        .map(|t| t.parse().ok())
        .collect()
}

/// Index just past the digit run starting at `from`, or `None` if there is no
/// digit there.
fn skip_ascii_digits(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while data.get(i).is_some_and(|b| b.is_ascii_digit()) {
        i += 1;
    }
    (i > from).then_some(i)
}

/// First index of `needle` in `haystack` at or after `from`.
pub(crate) fn find_sub(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

/// Serialize the document with PDF 1.5 object-stream packing: eligible
/// non-stream objects are packed into a single `ObjStm` stream and the
/// cross-reference table is emitted as a binary xref stream. Uses lopdf's own
/// `save_with_options` (not a hand-rolled writer and not qpdf); the output is
/// strictly `qpdf --check`-clean with no post-pass.
///
/// Reached only when `OptimizeOptions.pack_object_streams` is true. See the
/// `pack_object_streams` field docs on [`OptimizeOptions`] for the
/// cost/benefit trade-off.
fn pack_and_save(doc: &mut Document) -> Result<Vec<u8>, lopdf::Error> {
    // Pack non-stream objects into an ObjStm + cross-reference stream via
    // lopdf's own writer. `renumber_objects()` (done by the caller) clears the
    // hard "invalid object stream" errors a contiguous id space avoids.
    //
    // lopdf <= 0.41 omitted the xref stream's own self-entry (`Xref::size` was
    // stale when `create_xref_steam` ran), which needed a byte-patching
    // post-pass to satisfy `qpdf --check`. Fixed upstream in J-F-Liu/lopdf#501
    // and released in 0.42, so we now pack and save directly.
    let options = lopdf::SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(true)
        // Cap objects per stream at 65_535: lopdf writes each xref type-2 entry's
        // object-stream index as a u16 (`writer.rs`: `index_in_stream as u16`) under
        // /W[1 4 2]. A stream packing more than 65_535 objects wraps its higher
        // indices mod 2^16, silently pointing xref entries at the wrong objects
        // (51_264 wrong entries on the 756-page Adobe spec corpus). lopdf starts a
        // new ObjStm whenever this cap is reached, so every index stays
        // representable in the 2-byte /W field.
        .max_objects_per_stream(65_535)
        .compression_level(9)
        .build();
    let mut out: Vec<u8> = Vec::new();
    doc.save_with_options(&mut out, options)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Object};

    use crate::tests::{build_pdf, build_pdf_duplicate_images, xref_object_header};
    use crate::{optimize_with_options, DeflateBackend, OptimizeOptions};

    #[test]
    fn pack_object_streams_produces_loadable_output() {
        // With packing on, the output must still be a valid, loadable PDF whose
        // image survives. (Strict qpdf-cleanliness is validated separately via
        // the real-file/archive runs. As of lopdf 0.42 the packed xref is
        // complete, so qpdf --check reports no warnings.)
        let pdf = build_pdf(400, 100);
        let opts = OptimizeOptions::default().with_pack_object_streams(true);
        let out = optimize_with_options(&pdf, opts);

        let doc = Document::load_mem(&out).expect("packed output must load");
        let has_image = doc.objects.values().any(|o| {
            matches!(o, Object::Stream(s)
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image"))
        });
        assert!(has_image, "image must survive packing");
    }

    /// Task 1: lopdf writes the cross-reference stream with no `/Filter` (see
    /// `compress_xref_stream`). Both save paths must ship it deflated, and the
    /// patched file must still load.
    #[test]
    fn xref_stream_is_flate_compressed_in_both_save_paths() {
        // Enough objects that the xref payload is worth deflating at all.
        let pdf = build_pdf_duplicate_images(6, 400);
        for pack in [true, false] {
            let opts = OptimizeOptions::default().with_pack_object_streams(pack);
            let out = optimize_with_options(&pdf, opts);
            let header = xref_object_header(&out);
            let shown = String::from_utf8_lossy(&header).to_string();
            assert!(
                find_sub(&header, b"/Type/XRef", 0).is_some(),
                "pack={pack}: expected an xref stream, got {shown}"
            );
            assert!(
                find_sub(&header, b"/Filter/FlateDecode", 0).is_some(),
                "pack={pack}: xref stream must be deflated, got {shown}"
            );
            let doc = Document::load_mem(&out)
                .unwrap_or_else(|e| panic!("pack={pack}: patched output must load: {e}"));
            assert!(
                !doc.get_pages().is_empty(),
                "pack={pack}: pages must survive"
            );
        }
    }

    /// The patch must decline anything that is not exactly the object shape it
    /// expects — including a file it has already compressed (no `/Filter`
    /// twice) and truncated garbage.
    #[test]
    fn xref_compression_declines_unrecognized_tails() {
        let pdf = build_pdf_duplicate_images(6, 400);
        let out = optimize_with_options(&pdf, OptimizeOptions::default());
        assert_eq!(
            compress_xref_stream(out.clone(), DeflateBackend::Zlib),
            out,
            "an already-deflated xref stream must be left alone"
        );
        for junk in [
            b"".to_vec(),
            b"%PDF-1.5\nstartxref\n999999\n%%EOF".to_vec(),
            b"%PDF-1.5\nstartxref\nnotanumber\n%%EOF".to_vec(),
            out[..out.len() / 2].to_vec(),
        ] {
            assert_eq!(
                compress_xref_stream(junk.clone(), DeflateBackend::Zlib),
                junk,
                "malformed input must pass through untouched"
            );
        }
    }

    /// Synthetic `/W[1 4 2]` xref rows in the shape lopdf writes: the free
    /// head entry, `type1` ascending byte offsets, then `type2` packed entries
    /// (one object stream, ascending index).
    fn synthetic_xref_rows(type1: usize, type2: usize) -> Vec<u8> {
        let mut rows = vec![0u8, 0, 0, 0, 0, 0xff, 0xff];
        let mut offset: u32 = 15;
        for i in 0..type1 {
            rows.push(1);
            rows.extend_from_slice(&offset.to_be_bytes());
            rows.extend_from_slice(&[0, 0]);
            offset += 40 + (i as u32 * 7919) % 900;
        }
        for i in 0..type2 {
            rows.push(2);
            rows.extend_from_slice(&7u32.to_be_bytes());
            rows.extend_from_slice(&(i as u16).to_be_bytes());
        }
        rows
    }

    /// The predicted payload must decode back to the exact raw rows — through
    /// this crate's own defilter AND through lopdf's reader (an independent
    /// implementation, and the one pass 2 actually loads with).
    #[test]
    fn xref_up_predictor_round_trips_rows() {
        let rows = synthetic_xref_rows(300, 500);
        let dict = format!("<</Type/XRef/W[1 4 2]/Length {}>>", rows.len()).into_bytes();
        for backend in [DeflateBackend::Zlib, DeflateBackend::Zopfli] {
            let (payload, parms) =
                predicted_xref_payload(&dict, &rows, backend).expect("rows tile /W exactly");
            assert_eq!(parms, b"/DecodeParms<</Columns 7/Predictor 12>>".to_vec());

            let filtered = inflate_capped(&payload, rows.len() * 2).unwrap();
            assert_eq!(
                filtered.len(),
                rows.len() + rows.len() / 7,
                "one tag per row"
            );
            assert!(
                filtered.chunks(8).all(|r| r[0] == 2),
                "every row is Up-filtered"
            );
            assert_eq!(png_defilter(&filtered, 1, 7).unwrap(), rows);

            let mut stream = lopdf::Stream::new(
                dictionary! {
                    "Type" => "XRef",
                    "Filter" => "FlateDecode",
                    "DecodeParms" => dictionary! { "Columns" => 7, "Predictor" => 12 },
                },
                payload.clone(),
            );
            stream.allows_compression = false;
            assert_eq!(stream.decompressed_content().unwrap(), rows, "lopdf decode");

            // The whole point: this table is smaller predicted than plain.
            let plain = deflate_backend(&rows, backend).unwrap();
            assert!(
                payload.len() < plain.len(),
                "{backend:?}: predicted {} vs plain {}",
                payload.len(),
                plain.len()
            );
        }

        // Shapes the predictor must refuse: a payload /W does not tile, a
        // /W that is not three fields, a field wider than 8 bytes, no /W.
        let z = DeflateBackend::Zlib;
        assert!(predicted_xref_payload(&dict, &rows[..rows.len() - 1], z).is_none());
        assert!(predicted_xref_payload(b"<</W[1 4 2 1]>>", &rows, z).is_none());
        assert!(predicted_xref_payload(b"<</W[1 9 2]>>", &rows, z).is_none());
        assert!(predicted_xref_payload(b"<</W[0 0 0]>>", &rows, z).is_none());
        assert!(predicted_xref_payload(b"<</Type/XRef>>", &rows, z).is_none());
        assert!(predicted_xref_payload(&dict, &[], z).is_none());
    }

    /// A document of `n` small dictionaries, saved through the packed writer
    /// with its raw (uncompressed) cross-reference stream still in place.
    fn raw_packed_save(n: usize) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages = doc.new_object_id();
        let refs: Vec<Object> = (0..n)
            .map(|i| doc.add_object(dictionary! { "N" => i as i64 }).into())
            .collect();
        // One stream object too, so the table carries type-1 offset rows next
        // to the type-2 packed rows.
        let content = doc.add_object(lopdf::Stream::new(
            dictionary! {},
            b"0 0 m 1 1 l S".to_vec(),
        ));
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages, "Contents" => content,
        });
        doc.objects.insert(
            pages,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            }),
        );
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages, "Extra" => refs,
        });
        doc.trailer.set("Root", catalog);
        let raw = pack_and_save(&mut doc).unwrap();
        let header = xref_object_header(&raw);
        assert!(
            find_sub(&header, b"/Filter", 0).is_none(),
            "premise: lopdf writes the xref stream raw"
        );
        raw
    }

    /// The xref stream's payload as written (between `stream\n` and
    /// `\nendstream`).
    fn xref_payload(pdf: &[u8]) -> Vec<u8> {
        let header = xref_object_header(pdf);
        let start = find_sub(pdf, &header, 0).unwrap() + header.len() + b">>stream\n".len();
        let end = find_sub(pdf, b"\nendstream", start).unwrap();
        pdf[start..end].to_vec()
    }

    /// The predictor is strictly-smaller-or-absent: the predicted encoding
    /// ships only when its patched FILE (payload + `/DecodeParms` entry) is
    /// strictly smaller than the plain-deflate file this patch shipped before
    /// it existed, so no output grows. `/DecodeParms` is declared exactly
    /// when the payload is predicted, and a table whose deflate does not
    /// shrink is left raw. Covers tables too short to deflate, too short for
    /// Up to pay, and long enough for it to.
    #[test]
    fn xref_compression_ships_the_strictly_smaller_candidate() {
        let (mut saw_predicted, mut saw_plain) = (false, false);
        for n in [0usize, 2, 10, 40, 2_000] {
            let raw = raw_packed_save(n);
            let rows = xref_payload(&raw);
            let dict = xref_object_header(&raw);
            // The file size each candidate patch produces, from first
            // principles: swap the payload, add the dictionary entries, and
            // re-print `/Length`.
            let digits = |n: usize| n.to_string().len();
            let size_with = |payload: usize, parms: usize| {
                raw.len() - rows.len()
                    + payload
                    + parms
                    + b"/Filter/FlateDecode".len()
                    + digits(payload)
                    - digits(rows.len())
            };
            for backend in [DeflateBackend::Zlib, DeflateBackend::Zopfli] {
                let out = compress_xref_stream(raw.clone(), backend);
                let plain = deflate_backend(&rows, backend).unwrap();
                let plain_size = size_with(plain.len(), 0);
                let (pred, parms) = predicted_xref_payload(&dict, &rows, backend).unwrap();
                let pred_size = size_with(pred.len(), parms.len());
                let tag = format!("n={n} {backend:?}");

                let (want_size, want_payload, want_parms) = if pred_size < plain_size {
                    (pred_size, pred, true)
                } else {
                    (plain_size, plain.clone(), false)
                };
                if plain.len() >= rows.len() {
                    assert_eq!(out, raw, "{tag}: deflate does not shrink, ship the input");
                    continue;
                }
                assert_eq!(out.len(), want_size, "{tag}: smallest candidate ships");
                assert!(out.len() <= plain_size, "{tag}: never larger than plain");
                if want_parms {
                    assert!(out.len() < plain_size, "{tag}: predictor strictly smaller");
                }
                assert_eq!(xref_payload(&out), want_payload, "{tag}");
                let header = xref_object_header(&out);
                assert_eq!(
                    find_sub(&header, b"/DecodeParms", 0).is_some(),
                    want_parms,
                    "{tag}: /DecodeParms iff predicted: {}",
                    String::from_utf8_lossy(&header)
                );
                saw_predicted |= want_parms;
                saw_plain |= !want_parms;

                let doc =
                    Document::load_mem(&out).unwrap_or_else(|e| panic!("{tag}: must load: {e}"));
                let before = Document::load_mem(&raw).unwrap();
                assert_eq!(doc.objects.len(), before.objects.len(), "{tag}");
                for (&id, object) in &before.objects {
                    if object.type_name().ok() == Some(b"XRef".as_slice()) {
                        continue; // the patched object itself
                    }
                    assert_eq!(
                        doc.get_object(id).ok(),
                        Some(object),
                        "{tag}: object {id:?}"
                    );
                }
            }
        }
        // (A table whose deflate does not shrink cannot come out of lopdf's
        // writer — even the empty document's six rows deflate — so the raw
        // branch is exercised by `xref_compression_declines_unrecognized_tails`.)
        assert!(
            saw_predicted && saw_plain,
            "premise: both encodings are exercised (predicted {saw_predicted}, plain {saw_plain})"
        );
    }

    /// The ObjStm zopfli patch: on a packed save it must produce output that
    /// is never larger, re-parses, and decodes to the same objects; on
    /// anything that is not exactly a packed lopdf tail it must decline.
    #[test]
    fn objstm_zopfli_patch_is_never_larger_and_reparses() {
        let pdf = build_pdf_duplicate_images(6, 400);
        let zlib_out = optimize_with_options(
            &pdf,
            OptimizeOptions::default().with_pack_object_streams(true),
        );
        let zop_out = optimize_with_options(
            &pdf,
            OptimizeOptions::default()
                .with_pack_object_streams(true)
                .with_deflate_backend(DeflateBackend::Zopfli),
        );
        assert!(
            zop_out.len() <= zlib_out.len(),
            "zopfli save must never lose to zlib: {} vs {}",
            zlib_out.len(),
            zop_out.len()
        );
        let doc = Document::load_mem(&zop_out).unwrap();
        assert_eq!(doc.get_pages().len(), 1, "the page must survive the patch");

        // Fail-safe: junk and non-packed tails pass through untouched.
        for junk in [
            b"".to_vec(),
            b"%PDF-1.5\nno objstm here\nstartxref\n9\n%%EOF".to_vec(),
            zlib_out[..zlib_out.len() / 2].to_vec(),
        ] {
            assert_eq!(
                rezopfli_objstm(junk.clone()),
                junk,
                "malformed input must pass through untouched"
            );
        }
    }
}
