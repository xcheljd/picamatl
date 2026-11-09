//! Lossless transcode of legacy stream filters to `/FlateDecode`.
//!
//! Old producers (Acrobat Capture era) wrote content streams with
//! `/LZWDecode`, often behind an `/ASCII85Decode` layer, and some files carry
//! `/ASCIIHexDecode` or `/RunLengthDecode` streams. Every one of these is a
//! pure byte transform: decoding the whole chain gives the exact bytes a
//! viewer hands to the next stage, and re-encoding those bytes as one zlib
//! stream is render-identical by construction. Any `/DecodeParms` predictor
//! on the LAST filter carries over unchanged — LZW and Flate share the
//! predictor parameters (ISO 32000-1 Table 8), and the decoded payload is
//! still the predicted one, so the same predictor reverses it.
//!
//! Posture (each rule is a decline, never a guess):
//! - Only chains made entirely of LZW / ASCII85 / ASCIIHex / RunLength /
//!   Flate, with at least one non-Flate member. Image-semantic filters (DCT,
//!   JPX, JBIG2, CCITT) and abbreviated inline-image names never qualify.
//! - `/DecodeParms` only on the last filter (LZW or Flate), with keys this
//!   module understands; `/EarlyChange` must be 0 or 1. Indirect `/Filter`
//!   or `/DecodeParms` values, external-file streams (`/F`), object and xref
//!   streams are left alone.
//! - Strict decoders: malformed input, or output past the per-layer cap,
//!   declines the stream.
//! - Decode-back verified (the Flate payload re-inflates to the decoded
//!   bytes) and strictly smaller counting the dictionary text it changes.
//! - Encrypted and PDF/A-declared documents are declined wholesale, the same
//!   posture as the content minifier and the final re-deflate (unless
//!   `--strip-metadata` removes the PDF/A claim anyway; see `plan`).

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::deflate::{deflate_backend, inflate_capped};
use crate::lzw::lzw_decode;
use crate::DeflateBackend;

/// Longest filter chain considered; real files use one or two.
const MAX_CHAIN: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Filter {
    Lzw,
    Ascii85,
    AsciiHex,
    RunLength,
    Flate,
}

impl Filter {
    fn from_name(n: &[u8]) -> Option<Self> {
        Some(match n {
            b"LZWDecode" => Self::Lzw,
            b"ASCII85Decode" => Self::Ascii85,
            b"ASCIIHexDecode" => Self::AsciiHex,
            b"RunLengthDecode" => Self::RunLength,
            b"FlateDecode" => Self::Flate,
            _ => return None,
        })
    }
}

/// One planned rewrite: new Flate payload and the `/DecodeParms` it needs.
pub(crate) struct Transcode {
    id: ObjectId,
    content: Vec<u8>,
    parms: Option<Dictionary>,
}

/// Plan every qualifying stream, read-only. `images` selects image XObjects
/// (`true`) or every other stream (`false`): the caller applies the two
/// halves at different points in the pipeline (see `try_optimize`).
///
/// `metadata_stripped` is the `strip_metadata` option: the output then
/// carries no PDF/A claim, so the PDF/A decline does not apply. Without
/// this, a second pass over our own output (XMP gone) would transcode what
/// the first pass declined — not idempotent.
pub(crate) fn plan(
    doc: &Document,
    backend: DeflateBackend,
    images: bool,
    metadata_stripped: bool,
    limit: usize,
) -> Vec<Transcode> {
    if doc.is_encrypted() || (!metadata_stripped && crate::fonts::pdfa_blocked(doc)) {
        return Vec::new();
    }
    let mut plans = Vec::new();
    for (&id, obj) in doc.objects.iter() {
        let Object::Stream(s) = obj else { continue };
        let is_image = matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image");
        if is_image != images {
            continue;
        }
        if let Some((content, parms)) = plan_stream(s, backend, limit) {
            plans.push(Transcode { id, content, parms });
        }
    }
    plans
}

/// Apply planned transcodes: new payload, scalar `/Filter /FlateDecode`,
/// and the carried-over parms (or no `/DecodeParms` at all). Every other
/// dictionary key is untouched.
pub(crate) fn apply(doc: &mut Document, plans: Vec<Transcode>) {
    for p in plans {
        if let Ok(Object::Stream(s)) = doc.get_object_mut(p.id) {
            s.set_content(p.content);
            s.dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
            match p.parms {
                Some(parms) => s.dict.set("DecodeParms", Object::Dictionary(parms)),
                None => {
                    s.dict.remove(b"DecodeParms");
                }
            }
        }
    }
}

/// The transcode for one stream, or `None` when it does not qualify or
/// would not be strictly smaller.
fn plan_stream(
    s: &lopdf::Stream,
    backend: DeflateBackend,
    limit: usize,
) -> Option<(Vec<u8>, Option<Dictionary>)> {
    let dict = &s.dict;
    if matches!(dict.get(b"Type"), Ok(Object::Name(n)) if n == b"ObjStm" || n == b"XRef") {
        return None;
    }
    // External-file stream: the stream bytes are not the data.
    if dict.has(b"F") {
        return None;
    }
    let filter_obj = dict.get(b"Filter").ok()?;
    let chain: Vec<Filter> = match filter_obj {
        Object::Name(n) => vec![Filter::from_name(n)?],
        Object::Array(items) => items
            .iter()
            .map(|o| match o {
                Object::Name(n) => Filter::from_name(n),
                _ => None,
            })
            .collect::<Option<_>>()?,
        _ => return None,
    };
    if chain.is_empty() || chain.len() > MAX_CHAIN || chain.iter().all(|&f| f == Filter::Flate) {
        return None;
    }
    let parms_obj = dict.get(b"DecodeParms").ok();
    let parms = chain_parms(parms_obj, chain.len())?;
    let last = chain.len() - 1;

    // Validate parms position by position; only the last filter may carry
    // a predictor, and only LZW/Flate take parms at all.
    let mut early_change = vec![true; chain.len()];
    let mut out_parms: Option<Dictionary> = None;
    for (i, (&f, p)) in chain.iter().zip(&parms).enumerate() {
        let Some(p) = p else { continue };
        let mut kept = Dictionary::new();
        for (k, v) in p.iter() {
            match k.as_slice() {
                b"EarlyChange" if f == Filter::Lzw => match v {
                    Object::Integer(0) => early_change[i] = false,
                    Object::Integer(1) => {}
                    _ => return None,
                },
                b"Predictor" | b"Colors" | b"BitsPerComponent" | b"Columns"
                    if i == last && matches!(f, Filter::Lzw | Filter::Flate) =>
                {
                    if !matches!(v, Object::Integer(_)) {
                        return None;
                    }
                    kept.set(k.clone(), v.clone());
                }
                _ => return None,
            }
        }
        if i == last && !kept.is_empty() {
            out_parms = Some(kept);
        }
    }

    // Decode layer by layer. `zlib_in` keeps the last layer's input when
    // that layer is Flate: those bytes already are a valid zlib stream for
    // the decoded payload and compete with a fresh deflate.
    let mut data: Vec<u8> = s.content.clone();
    let mut zlib_in: Option<Vec<u8>> = None;
    for (i, &f) in chain.iter().enumerate() {
        let next = match f {
            Filter::Lzw => lzw_decode(&data, early_change[i], limit)?,
            Filter::Ascii85 => ascii85_decode(&data, limit)?,
            Filter::AsciiHex => ascii_hex_decode(&data, limit)?,
            Filter::RunLength => run_length_decode(&data, limit)?,
            Filter::Flate => inflate_capped(&data, limit)?,
        };
        if i == last && f == Filter::Flate {
            zlib_in = Some(std::mem::take(&mut data));
        }
        data = next;
    }

    let fresh = deflate_backend(&data, backend)?;
    if inflate_capped(&fresh, data.len())? != data {
        return None;
    }
    let content = match zlib_in {
        Some(z) if z.len() <= fresh.len() => z,
        _ => fresh,
    };

    // Whole-dictionary accounting: the new `/Filter` name and parms against
    // the old entries' text.
    let old_cost = s.content.len()
        + text_len(filter_obj)
        + parms_obj.map_or(0, |p| b"/DecodeParms".len() + text_len(p));
    let new_parms_cost = out_parms
        .as_ref()
        .map_or(0, |p| b"/DecodeParms".len() + dict_text_len(p));
    let new_cost = content.len() + b"/FlateDecode".len() + new_parms_cost;
    (new_cost < old_cost).then_some((content, out_parms))
}

/// Per-filter parms from `/DecodeParms`: absent/null → none for all; a
/// dictionary only for a one-filter chain (applying one dict to several
/// filters is ambiguous across viewers); an array must line up with the
/// chain, each entry a direct dictionary or null. References decline.
fn chain_parms(parms: Option<&Object>, n: usize) -> Option<Vec<Option<&Dictionary>>> {
    match parms {
        None | Some(Object::Null) => Some(vec![None; n]),
        Some(Object::Dictionary(d)) if n == 1 => Some(vec![Some(d)]),
        Some(Object::Array(items)) if items.len() == n => items
            .iter()
            .map(|o| match o {
                Object::Null => Some(None),
                Object::Dictionary(d) => Some(Some(d)),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// Approximate serialized length of a direct PDF object, for the size
/// guard. Only shapes that occur in `/Filter` and `/DecodeParms` matter;
/// the guard compares like with like, so separator conventions cancel out.
fn text_len(o: &Object) -> usize {
    match o {
        Object::Null => 4,
        Object::Boolean(b) => {
            if *b {
                4
            } else {
                5
            }
        }
        Object::Integer(i) => i.to_string().len(),
        Object::Real(r) => r.to_string().len(),
        Object::Name(n) => 1 + n.len(),
        Object::Array(a) => 2 + a.iter().map(text_len).sum::<usize>() + a.len().saturating_sub(1),
        Object::Dictionary(d) => dict_text_len(d),
        Object::Reference((n, g)) => n.to_string().len() + g.to_string().len() + 3,
        Object::String(s, _) => 2 + s.len(),
        Object::Stream(_) => usize::MAX / 4,
    }
}

fn dict_text_len(d: &Dictionary) -> usize {
    4 + d
        .iter()
        .map(|(k, v)| 1 + k.len() + 1 + text_len(v))
        .sum::<usize>()
}

fn is_pdf_whitespace(b: u8) -> bool {
    matches!(b, b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

/// Strict `/ASCII85Decode` (ISO 32000-1 §7.4.3). Declines characters
/// outside `!`..`u`, a `z` inside a group, a group value past 2^32 - 1, a
/// lone trailing character, and `~` not followed by `>`. A missing `~>` is
/// accepted (the data simply ends), as viewers do.
pub(crate) fn ascii85_decode(data: &[u8], limit: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut group = [0u8; 5];
    let mut n = 0usize;
    let mut i = 0usize;
    let push = |out: &mut Vec<u8>, bytes: &[u8]| -> Option<()> {
        if out.len().checked_add(bytes.len())? > limit {
            return None;
        }
        out.extend_from_slice(bytes);
        Some(())
    };
    while i < data.len() {
        let c = data[i];
        i += 1;
        if is_pdf_whitespace(c) {
            continue;
        }
        if c == b'~' {
            while i < data.len() && is_pdf_whitespace(data[i]) {
                i += 1;
            }
            if data.get(i) != Some(&b'>') {
                return None;
            }
            break;
        }
        if c == b'z' {
            if n != 0 {
                return None;
            }
            push(&mut out, &[0; 4])?;
            continue;
        }
        if !(b'!'..=b'u').contains(&c) {
            return None;
        }
        group[n] = c - b'!';
        n += 1;
        if n == 5 {
            push(&mut out, &group_value(&group)?.to_be_bytes())?;
            n = 0;
        }
    }
    match n {
        0 => {}
        1 => return None,
        _ => {
            for g in group.iter_mut().skip(n) {
                *g = b'u' - b'!';
            }
            let bytes = group_value(&group)?.to_be_bytes();
            push(&mut out, &bytes[..n - 1])?;
        }
    }
    Some(out)
}

fn group_value(group: &[u8; 5]) -> Option<u32> {
    let v = group.iter().fold(0u64, |acc, &d| acc * 85 + u64::from(d));
    u32::try_from(v).ok()
}

/// Strict `/ASCIIHexDecode` (§7.4.2): hex digits and whitespace up to `>`;
/// an odd final digit is followed by an implied 0. Any other character
/// declines; a missing `>` is accepted.
pub(crate) fn ascii_hex_decode(data: &[u8], limit: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut hi: Option<u8> = None;
    for &c in data {
        if is_pdf_whitespace(c) {
            continue;
        }
        if c == b'>' {
            break;
        }
        let v = (c as char).to_digit(16)? as u8;
        match hi.take() {
            None => hi = Some(v),
            Some(h) => {
                if out.len() >= limit {
                    return None;
                }
                out.push(h << 4 | v);
            }
        }
    }
    if let Some(h) = hi {
        if out.len() >= limit {
            return None;
        }
        out.push(h << 4);
    }
    Some(out)
}

/// Strict `/RunLengthDecode` (§7.4.5). A truncated run declines; a missing
/// EOD (128) is accepted.
pub(crate) fn run_length_decode(data: &[u8], limit: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        let len = data[i];
        i += 1;
        match len {
            128 => break,
            0..=127 => {
                let n = usize::from(len) + 1;
                let run = data.get(i..i + n)?;
                if out.len() + n > limit {
                    return None;
                }
                out.extend_from_slice(run);
                i += n;
            }
            _ => {
                let n = 257 - usize::from(len);
                let &b = data.get(i)?;
                if out.len() + n > limit {
                    return None;
                }
                out.resize(out.len() + n, b);
                i += 1;
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lzw::tests::lzw_encode;
    use lopdf::{dictionary, Stream};

    const CAP: usize = 1 << 24;

    fn a85_encode(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in data.chunks(4) {
            let mut b = [0u8; 4];
            b[..chunk.len()].copy_from_slice(chunk);
            let mut v = u32::from_be_bytes(b);
            if chunk.len() == 4 && v == 0 {
                out.push(b'z');
                continue;
            }
            let mut g = [0u8; 5];
            for d in g.iter_mut().rev() {
                *d = (v % 85) as u8 + b'!';
                v /= 85;
            }
            out.extend_from_slice(&g[..chunk.len() + 1]);
        }
        out.extend_from_slice(b"~>");
        out
    }

    #[test]
    fn ascii85_known_answers() {
        assert_eq!(
            ascii85_decode(b"87cURD]i,\"Ebo80~>", CAP).unwrap(),
            b"Hello World!"
        );
        assert_eq!(ascii85_decode(b"z~>", CAP).unwrap(), [0; 4]);
        assert_eq!(
            ascii85_decode(b"87cUR\nD]i,\"Ebo80", CAP).unwrap(),
            b"Hello World!"
        );
        assert_eq!(ascii85_decode(b"~>", CAP).unwrap(), b"");
        let data: Vec<u8> = (0..=255u8).chain([0, 0, 0, 0, 1]).collect();
        assert_eq!(ascii85_decode(&a85_encode(&data), CAP).unwrap(), data);
        // Declines: bad char, z mid-group, lone trailing char, overflow,
        // `~` without `>`, output over the cap.
        assert!(ascii85_decode(b"87cU{~>", CAP).is_none());
        assert!(ascii85_decode(b"87z~>", CAP).is_none());
        assert!(ascii85_decode(b"87cUR8~>", CAP).is_none());
        assert!(ascii85_decode(b"uuuuu~>", CAP).is_none());
        assert!(ascii85_decode(b"87cURx", CAP).is_none());
        assert!(ascii85_decode(b"zzz~>", 11).is_none());
        assert_eq!(ascii85_decode(b"zzz~>", 12).unwrap().len(), 12);
    }

    #[test]
    fn ascii_hex_known_answers() {
        assert_eq!(ascii_hex_decode(b"48 65\n6c6C6f>", CAP).unwrap(), b"Hello");
        assert_eq!(ascii_hex_decode(b"4>", CAP).unwrap(), [0x40]);
        assert_eq!(ascii_hex_decode(b"414", CAP).unwrap(), [0x41, 0x40]);
        assert!(ascii_hex_decode(b"4G>", CAP).is_none());
        assert!(ascii_hex_decode(b"414243>", 2).is_none());
    }

    #[test]
    fn run_length_known_answers() {
        assert_eq!(
            run_length_decode(&[2, b'a', b'b', b'c', 254, b'x', 128, 9, 9], CAP).unwrap(),
            b"abcxxx"
        );
        assert_eq!(run_length_decode(&[0, b'q'], CAP).unwrap(), b"q");
        assert!(run_length_decode(&[5, b'a'], CAP).is_none());
        assert!(run_length_decode(&[200], CAP).is_none());
        assert!(run_length_decode(&[129, b'z'], 127).is_none());
        assert_eq!(
            run_length_decode(&[129, b'z'], 128).unwrap(),
            vec![b'z'; 128]
        );
    }

    /// Compressible content-stream-like payload that LZW stores weakly.
    fn content(n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        let mut i = 0u32;
        while v.len() < n {
            v.extend_from_slice(
                format!(
                    "BT /F1 {} Tf {} {} Td (line {i}) Tj ET\n",
                    9 + i % 4,
                    72 + i % 7,
                    700 - i % 600
                )
                .as_bytes(),
            );
            i += 1;
        }
        v
    }

    fn inflate(d: &[u8]) -> Vec<u8> {
        inflate_capped(d, CAP).unwrap()
    }

    #[test]
    fn lzw_stream_becomes_smaller_flate_with_same_payload() {
        let payload = content(20_000);
        let s = Stream::new(
            dictionary! { "Filter" => "LZWDecode", "Foo" => 7 },
            lzw_encode(&payload, true, true),
        );
        let (out, parms) = plan_stream(&s, DeflateBackend::Zlib, CAP).unwrap();
        assert!(out.len() < s.content.len());
        assert_eq!(inflate(&out), payload);
        assert!(parms.is_none());
    }

    #[test]
    fn early_change_zero_is_honored_and_dropped() {
        let payload = content(20_000);
        let s = Stream::new(
            dictionary! {
                "Filter" => vec![Object::Name(b"LZWDecode".to_vec())],
                "DecodeParms" => dictionary! { "EarlyChange" => 0 },
            },
            lzw_encode(&payload, false, true),
        );
        let (out, parms) = plan_stream(&s, DeflateBackend::Zlib, CAP).unwrap();
        assert_eq!(inflate(&out), payload);
        assert!(parms.is_none());
    }

    #[test]
    fn predictor_parms_carry_over_without_early_change() {
        // The LZW payload is PNG-predicted rows; the Flate output must keep
        // the predicted bytes and the same predictor parms.
        let mut predicted = Vec::new();
        for r in 0..200u32 {
            predicted.push(2); // Up
            predicted.extend((0..30).map(|c| ((r * c) % 3) as u8));
        }
        let s = Stream::new(
            dictionary! {
                "Filter" => "LZWDecode",
                "DecodeParms" => dictionary! {
                    "Predictor" => 12, "Columns" => 30, "EarlyChange" => 1,
                },
            },
            lzw_encode(&predicted, true, true),
        );
        let (out, parms) = plan_stream(&s, DeflateBackend::Zlib, CAP).unwrap();
        assert_eq!(inflate(&out), predicted);
        let parms = parms.unwrap();
        assert_eq!(parms.get(b"Predictor").unwrap(), &Object::Integer(12));
        assert_eq!(parms.get(b"Columns").unwrap(), &Object::Integer(30));
        assert!(!parms.has(b"EarlyChange"));
    }

    #[test]
    fn ascii85_lzw_chain_decodes_both_layers() {
        let payload = content(20_000);
        let s = Stream::new(
            dictionary! {
                "Filter" => vec![
                    Object::Name(b"ASCII85Decode".to_vec()),
                    Object::Name(b"LZWDecode".to_vec()),
                ],
            },
            a85_encode(&lzw_encode(&payload, true, true)),
        );
        let (out, _) = plan_stream(&s, DeflateBackend::Zlib, CAP).unwrap();
        assert_eq!(inflate(&out), payload);
    }

    #[test]
    fn ascii85_flate_chain_strips_the_text_layer() {
        let payload = content(20_000);
        let z = crate::deflate::deflate_level9(&payload).unwrap();
        let s = Stream::new(
            dictionary! {
                "Filter" => vec![
                    Object::Name(b"ASCII85Decode".to_vec()),
                    Object::Name(b"FlateDecode".to_vec()),
                ],
                "DecodeParms" => vec![Object::Null, Object::Null],
            },
            a85_encode(&z),
        );
        let (out, parms) = plan_stream(&s, DeflateBackend::Zlib, CAP).unwrap();
        assert!(out.len() <= z.len());
        assert_eq!(inflate(&out), payload);
        assert!(parms.is_none());
    }

    #[test]
    fn declined_shapes() {
        let payload = content(20_000);
        let lzw = lzw_encode(&payload, true, true);
        let declined = |dict: Dictionary, data: &[u8]| {
            plan_stream(&Stream::new(dict, data.to_vec()), DeflateBackend::Zlib, CAP).is_none()
        };
        // Image-semantic filter in the chain.
        assert!(declined(
            dictionary! { "Filter" => vec![Object::Name(b"LZWDecode".to_vec()), Object::Name(b"DCTDecode".to_vec())] },
            &lzw
        ));
        // Plain Flate is not this pass's business.
        assert!(declined(
            dictionary! { "Filter" => "FlateDecode" },
            &crate::deflate::deflate_level9(&payload).unwrap()
        ));
        // Abbreviated (inline-image) names, unknown parms, bad EarlyChange,
        // predictor on a non-last filter, one dict for a two-filter chain,
        // indirect parms, external-file stream, object stream.
        assert!(declined(dictionary! { "Filter" => "LZW" }, &lzw));
        assert!(declined(
            dictionary! { "Filter" => "LZWDecode", "DecodeParms" => dictionary! { "Foo" => 1 } },
            &lzw
        ));
        assert!(declined(
            dictionary! { "Filter" => "LZWDecode", "DecodeParms" => dictionary! { "EarlyChange" => 2 } },
            &lzw
        ));
        assert!(declined(
            dictionary! {
                "Filter" => vec![Object::Name(b"LZWDecode".to_vec()), Object::Name(b"ASCIIHexDecode".to_vec())],
                "DecodeParms" => vec![Object::Dictionary(dictionary! { "Predictor" => 2 }), Object::Null],
            },
            &lzw
        ));
        assert!(declined(
            dictionary! {
                "Filter" => vec![Object::Name(b"ASCII85Decode".to_vec()), Object::Name(b"LZWDecode".to_vec())],
                "DecodeParms" => dictionary! { "EarlyChange" => 1 },
            },
            &a85_encode(&lzw)
        ));
        assert!(declined(
            dictionary! { "Filter" => "LZWDecode", "DecodeParms" => Object::Reference((9, 0)) },
            &lzw
        ));
        assert!(declined(
            dictionary! { "Filter" => "LZWDecode", "F" => "ext.bin" },
            &lzw
        ));
        assert!(declined(
            dictionary! { "Filter" => "LZWDecode", "Type" => "ObjStm" },
            &lzw
        ));
        // Malformed LZW and a bomb past the cap.
        assert!(declined(
            dictionary! { "Filter" => "LZWDecode" },
            &[0xff; 64]
        ));
        let bomb = lzw_encode(&vec![b'A'; 1 << 20], true, true);
        assert!(plan_stream(
            &Stream::new(dictionary! { "Filter" => "LZWDecode" }, bomb),
            DeflateBackend::Zlib,
            1 << 16
        )
        .is_none());
    }

    #[test]
    fn never_larger_counting_dictionary_text() {
        // A tiny LZW stream whose Flate form is not smaller once the longer
        // filter name is counted stays as it is.
        let s = Stream::new(
            dictionary! { "Filter" => "LZWDecode" },
            lzw_encode(b"q Q", true, true),
        );
        assert!(plan_stream(&s, DeflateBackend::Zlib, CAP).is_none());
    }
}
