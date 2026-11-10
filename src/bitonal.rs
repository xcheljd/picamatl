//! B-M1: lossless recompression of bitonal (1-bit) images to CCITT G4.
//!
//! Two source shapes feed the same G4 re-encode (docs/PHASE3-PLAN.md §B):
//! CCITT-stored images (G4 `/K < 0`, or EOL-framed G3 1D `/K 0` with
//! `/EndOfLine true`) and Flate-stored 1-bit images. Both decode to the same
//! intermediate — packed 1-bit sample rows, exactly the PDF image-data layout
//! — which is re-encoded as G4 with normalized `/DecodeParms` (`/K -1`,
//! `/BlackIs1 false`). Pixels are never resampled; `/Width`/`/Height` never
//! change; the stream is replaced only when the G4 payload is strictly
//! smaller (including the `/DecodeParms` overhead) AND a decode-back pass
//! reproduces the source samples bit-for-bit.
//!
//! Eligibility posture mirrors A-M1: every gate failure returns `None` and the
//! image is left byte-identical. Gates encode the `fax` 0.3.0 decoder contract
//! verified in the vetting spike (§B.3.1): `decode_g3` is 1D-only and
//! EOL-framed, so `/K > 0` and EOL-less `/K 0` streams are skipped; the G4
//! decoder has no fill-bit handling, so `/EncodedByteAlign true` is skipped;
//! `decode_g4`'s lenient tail handling is bypassed by driving `Group4Decoder`
//! directly and demanding exactly `/Height` rows followed by a clean EOFB.

use std::convert::Infallible;

use fax::decoder::{pels, DecodeStatus, Group3Decoder, Group4Decoder};
use fax::encoder::Encoder;
use fax::{Color, VecWriter};
use lopdf::{Document, Object, ObjectId};
use rayon::prelude::*;

use crate::deflate::inflate_capped;
use crate::jbig2;
use crate::{classify_filter, num, resolve, FilterClass};

/// Pixel-count ceiling for the bitonal pass: 2^28 ≈ 268M pixels (32 MiB of
/// packed samples), roughly 2× an A4 page scanned at 1200 dpi. Guards the
/// sample-buffer allocation before any decoding happens.
const MAX_BITONAL_PIXELS: u64 = 1 << 28;

/// Serialized cost of the dictionary edits a replacement forces on the image:
/// `/Filter /CCITTFaxDecode` plus the normalized `/DecodeParms` dict
/// (`/K -1 /Columns n /Rows n /BlackIs1 false`, ~60 bytes). The never-larger
/// guard charges the new payload for it, mirroring the Flate path's
/// `PARMS_OVERHEAD` accounting.
const CCITT_PARMS_OVERHEAD: usize = 80;

/// What a planned bitonal replacement writes: a CCITT G4 payload with the
/// normalized `/DecodeParms` (`columns`/`rows` echo the unchanged
/// `/Width`/`/Height`), or a JBIG2 generic-region payload, which carries
/// its own geometry and needs no parameters.
pub(crate) enum BitonalEncoding {
    G4 { columns: i64, rows: i64 },
    Jbig2,
}

/// A planned bitonal replacement, computed read-only before mutating the doc.
pub(crate) struct BitonalReplacement {
    pub id: ObjectId,
    pub content: Vec<u8>,
    pub encoding: BitonalEncoding,
}

/// Which re-encodings the bitonal planner may produce this run.
#[derive(Clone, Copy)]
pub(crate) struct BitonalTargets {
    /// Opt-in G4 recompression (`--recompress-bitonal-images`): CCITT and
    /// Flate 1-bit sources, DeviceGray / ImageMask, identity `/Decode`.
    pub g4: bool,
    /// JBIG2 generic region (default on): CCITT sources, plus Flate sources
    /// whenever `g4` is on (see [`plan_one`] for why).
    pub jbig2: bool,
}

/// Memory budget for concurrent bitonal planning, mirroring
/// `JPEG_REWRITE_BUDGET`: each image holds its samples, the JBIG2 bitmap
/// and a decode-back buffer at once (~4x the packed sample size, up to 32
/// MiB each), so the work runs in parallel batches whose summed estimate
/// stays under 1 GiB. Nothing is skipped; an image over budget alone runs
/// in a batch by itself.
const BITONAL_PLAN_BUDGET: usize = 1 << 30;

/// Plan bitonal re-encodes for every eligible image XObject. Runs over all
/// image streams, not just painted ones: the transforms are lossless, so
/// placement (and `/SMask`/`/Mask` presence) is irrelevant — the sample data
/// is preserved bit-for-bit under any use.
pub(crate) fn plan_bitonal_recompressions(
    doc: &Document,
    targets: BitonalTargets,
) -> Vec<BitonalReplacement> {
    if !targets.g4 && !targets.jbig2 {
        return Vec::new();
    }
    let ids: Vec<(ObjectId, usize)> = doc
        .objects
        .iter()
        .filter_map(|(&id, obj)| {
            let stream = obj.as_stream().ok()?;
            matches!(
                stream.dict.get(b"Subtype").map(|s| resolve(doc, s)),
                Ok(Object::Name(n)) if n == b"Image"
            )
            .then(|| (id, memory_estimate(doc, &stream.dict)))
        })
        .collect();
    crate::budget_batches(ids, BITONAL_PLAN_BUDGET, |&(_, cost)| cost)
        .into_iter()
        .flat_map(|batch| {
            batch
                .par_iter()
                .filter_map(|&(id, _)| plan_one(doc, id, targets))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Peak-memory estimate for planning one image, from its dictionary alone:
/// 4x the packed 1-bit sample buffer (0 for anything `plan_one` will
/// decline before allocating).
fn memory_estimate(doc: &Document, dict: &lopdf::Dictionary) -> usize {
    let dim = |k: &[u8]| {
        dict.get(k)
            .ok()
            .map(|o| resolve(doc, o))
            .and_then(|o| o.as_i64().ok())
            .filter(|&v| v > 0)
    };
    match (dim(b"Width"), dim(b"Height")) {
        (Some(w), Some(h)) if (w as u64).saturating_mul(h as u64) <= MAX_BITONAL_PIXELS => {
            (w as usize).div_ceil(8) * h as usize * 4
        }
        _ => 0,
    }
}

/// JBIG2 generic-region candidates tried per image; the smallest verified
/// stream is kept. All four fixed templates with typical prediction off and
/// on (nominal AT pixels), plus template 0 with its first AT pixel moved to
/// the three positions that won on some images. Measured in-pipeline over
/// the 7 non-PDF/A CCITT scans that decode (default output bytes):
/// template 0 alone 20,956,319 B; best of the eight nominal 20,430,935 B
/// (the smaller templates win on low-resolution scans, where fewer
/// contexts adapt faster); with the six AT variants 20,419,539 B, a further
/// 11,396 B at about twice the encode time — kept, compression outranks
/// speed. Every other AT position tried lost.
const JBIG2_CANDIDATES: [jbig2::GenericParams; 14] = [
    jbig2::GenericParams::nominal(0, false),
    jbig2::GenericParams::nominal(0, true),
    jbig2::GenericParams::nominal(1, false),
    jbig2::GenericParams::nominal(1, true),
    jbig2::GenericParams::nominal(2, false),
    jbig2::GenericParams::nominal(2, true),
    jbig2::GenericParams::nominal(3, false),
    jbig2::GenericParams::nominal(3, true),
    jbig2::GenericParams::nominal(0, false).with_a1(4, -1),
    jbig2::GenericParams::nominal(0, true).with_a1(4, -1),
    jbig2::GenericParams::nominal(0, false).with_a1(-1, -2),
    jbig2::GenericParams::nominal(0, true).with_a1(-1, -2),
    jbig2::GenericParams::nominal(0, false).with_a1(5, -1),
    jbig2::GenericParams::nominal(0, true).with_a1(5, -1),
];

/// Decode one bitonal image to packed samples, re-encode it under every
/// enabled target, and return the smallest replacement that is strictly
/// smaller than the stored stream and decodes back bit-for-bit. `None` =
/// untouched.
///
/// Idempotence when both targets are on: the costs compared are the ones
/// the next run would compare. A G4 result ships only when strictly cheaper
/// than the JBIG2 one (payload + `CCITT_PARMS_OVERHEAD` vs payload), so on
/// the next run the JBIG2 candidate — the same bytes, since the samples are
/// the same — cannot beat that G4 stream either. That is also why Flate
/// sources get a JBIG2 candidate whenever G4 is on: an image this run turns
/// into G4 is a CCITT source on the next run.
fn plan_one(doc: &Document, id: ObjectId, targets: BitonalTargets) -> Option<BitonalReplacement> {
    let stream = doc.get_object(id).ok()?.as_stream().ok()?;
    let dict = &stream.dict;

    // 1-bit only. `/ImageMask true` implies 1 bit per sample, so an absent
    // `/BitsPerComponent` is acceptable there; anything else must say 1.
    let image_mask = matches!(
        dict.get(b"ImageMask").map(|o| resolve(doc, o)),
        Ok(Object::Boolean(true))
    );
    let bpc = dict
        .get(b"BitsPerComponent")
        .ok()
        .map(|o| resolve(doc, o))
        .and_then(|o| o.as_i64().ok());
    match bpc {
        Some(1) => {}
        None if image_mask => {}
        _ => return None,
    }

    let width = dict
        .get(b"Width")
        .ok()
        .map(|o| resolve(doc, o))
        .and_then(|o| o.as_i64().ok())?;
    let height = dict
        .get(b"Height")
        .ok()
        .map(|o| resolve(doc, o))
        .and_then(|o| o.as_i64().ok())?;
    if width <= 0 || height <= 0 {
        return None;
    }
    if (width as u64).checked_mul(height as u64)? > MAX_BITONAL_PIXELS {
        return None;
    }
    let (w, h) = (width as u32, height as usize);

    let class = classify_filter(doc, dict.get(b"Filter").ok()?);
    let g4_ok = targets.g4 && g4_dictionary_ok(doc, dict, image_mask);
    // JBIG2 preserves the samples exactly, so `/ColorSpace` and `/Decode`
    // apply to the same values afterwards and need no gate.
    let jbig2_ok = targets.jbig2
        && match class {
            FilterClass::CcittOnly => true,
            FilterClass::FlateOnly => g4_ok,
            _ => false,
        };
    if !g4_ok && !jbig2_ok {
        return None;
    }

    let samples = match class {
        FilterClass::CcittOnly => decode_ccitt_source(doc, stream, w, h)?,
        FilterClass::FlateOnly => decode_flate_bitonal(stream, w, h)?,
        _ => return None,
    };
    let stored = stream.content.len();

    let g4 = g4_ok
        .then(|| encode_g4_from_samples(&samples, w, h))
        // Never-larger guard: the new payload plus its forced dictionary
        // edits must be strictly smaller than the bytes it replaces.
        .filter(|g4| g4.len() + CCITT_PARMS_OVERHEAD < stored);

    // The JBIG2 dictionary only shrinks: `/Filter /JBIG2Decode` is no
    // longer than either source filter name (and replaces any array form),
    // and `/DecodeParms` is dropped. A strictly smaller payload is
    // therefore a strictly smaller object.
    let jb2 = jbig2_ok
        .then(|| {
            let mut bm = jbig2::samples_to_bitmap(&samples, w as usize, h)?;
            // `encode_best` decodes every candidate back through the
            // in-house decoder and keeps only exact reproductions.
            jbig2::encode_best(&mut bm, &JBIG2_CANDIDATES)
        })
        .flatten()
        .filter(|jb2| jb2.len() < stored);

    let pick_g4 = match (&g4, &jb2) {
        (Some(g), Some(j)) => g.len() + CCITT_PARMS_OVERHEAD < j.len(),
        (Some(_), None) => true,
        _ => false,
    };
    if pick_g4 {
        let g4 = g4?;
        // Decode-back verification: the replacement ships only if decoding it
        // under the exact `/DecodeParms` we will write reproduces the source
        // samples bit-for-bit. Turns any encoder defect into a skip, never a
        // corrupt image.
        if decode_g4_strict_to_samples(&g4, w, h, false)? != samples {
            return None;
        }
        return Some(BitonalReplacement {
            id,
            content: g4,
            encoding: BitonalEncoding::G4 {
                columns: width,
                rows: height,
            },
        });
    }
    Some(BitonalReplacement {
        id,
        content: jb2?,
        encoding: BitonalEncoding::Jbig2,
    })
}

/// The G4 target's extra dictionary gates: DeviceGray or the implicit
/// ImageMask colorspace, and an identity `/Decode`.
fn g4_dictionary_ok(doc: &Document, dict: &lopdf::Dictionary, image_mask: bool) -> bool {
    // DeviceGray or the implicit ImageMask colorspace only. Indexed and
    // friends remap samples; out of scope.
    match dict.get(b"ColorSpace") {
        Err(_) if image_mask => {}
        Ok(cs) => {
            if !matches!(resolve(doc, cs), Object::Name(n) if n == b"DeviceGray") {
                return false;
            }
        }
        Err(_) => return false,
    }

    // `/Decode` remaps sample values; only the identity [0 1] (a no-op) is
    // accepted. `[1 0]` could be normalized by inversion, but that changes a
    // dictionary the rest of the toolchain may key on — out of M1 scope.
    if let Ok(decode) = dict.get(b"Decode") {
        let Object::Array(items) = resolve(doc, decode) else {
            return false;
        };
        if items.len() != 2 || num(&items[0]) != 0.0 || num(&items[1]) != 1.0 {
            return false;
        }
    }
    true
}

/// Decode a CCITT-stored source stream to packed samples, honoring its
/// `/DecodeParms`. Every unsupported parameter is a gate-skip, not an error.
fn decode_ccitt_source(
    doc: &Document,
    stream: &lopdf::Stream,
    w: u32,
    h: usize,
) -> Option<Vec<u8>> {
    // `/DecodeParms` as a dictionary, a reference to one, or the
    // one-element array matching a one-element `/Filter` array (`[null]`
    // = defaults). Anything else is a gate-skip.
    let parms = match stream.dict.get(b"DecodeParms").map(|o| resolve(doc, o)) {
        Err(_) => None,
        Ok(Object::Dictionary(d)) => Some(d),
        Ok(Object::Array(a)) if a.len() == 1 => match resolve(doc, &a[0]) {
            Object::Dictionary(d) => Some(d),
            Object::Null => None,
            _ => return None,
        },
        Ok(_) => return None,
    };
    let get_i = |key: &[u8], default: i64| {
        parms
            .and_then(|p| p.get(key).ok().and_then(|o| o.as_i64().ok()))
            .unwrap_or(default)
    };
    let get_b = |key: &[u8], default: bool| {
        parms
            .and_then(|p| p.get(key).ok().and_then(|o| o.as_bool().ok()))
            .unwrap_or(default)
    };

    // `/Columns` (default 1728) and a present `/Rows` must agree with the
    // image dictionary, or the stream's own geometry can't be trusted.
    if get_i(b"Columns", 1728) != i64::from(w) {
        return None;
    }
    let rows_parm = get_i(b"Rows", 0);
    if rows_parm != 0 && rows_parm != h as i64 {
        return None;
    }
    // fax's G4 decoder reads a continuous bitstream — it has no fill-bit
    // handling between coding lines — so byte-aligned streams are undecodable
    // by it. Fail-safe skip (§B.3.1).
    if get_b(b"EncodedByteAlign", false) {
        return None;
    }
    // Our strict decoders demand proper termination (EOFB / RTC); a stream
    // declaring `/EndOfBlock false` promises neither. Skip.
    if !get_b(b"EndOfBlock", true) {
        return None;
    }
    // Lenient damaged-row recovery is the opposite of our replacement bar.
    if get_i(b"DamagedRowsBeforeError", 0) != 0 {
        return None;
    }
    let black_is_1 = get_b(b"BlackIs1", false);

    let k = get_i(b"K", 0);
    if k < 0 {
        decode_g4_strict_to_samples(&stream.content, w, h, black_is_1)
    } else if k == 0 {
        // `decode_g3` requires EOL framing (mandatory initial EOL, RTC
        // terminator); `/K 0` streams without `/EndOfLine true` are misframed
        // by it (§B.3.1), and `/K > 0` (mixed 2D) is unsupported outright.
        if !get_b(b"EndOfLine", false) {
            return None;
        }
        decode_g3_strict_to_samples(&stream.content, w, h, black_is_1)
    } else {
        None
    }
}

/// Inflate a Flate-stored 1-bit image to its packed sample rows. Only plain
/// deflate (no predictor) is in scope; the strict capped inflate plus an exact
/// length check rejects truncated or lying streams.
fn decode_flate_bitonal(stream: &lopdf::Stream, w: u32, h: usize) -> Option<Vec<u8>> {
    match stream.dict.get(b"DecodeParms") {
        Err(_) => {}
        Ok(Object::Dictionary(d)) => {
            if d.get(b"Predictor")
                .ok()
                .and_then(|o| o.as_i64().ok())
                .unwrap_or(1)
                != 1
            {
                return None;
            }
        }
        Ok(_) => return None,
    }
    let stride = (w as usize).div_ceil(8);
    let expected = stride.checked_mul(h)?;
    let data = inflate_capped(&stream.content, expected)?;
    (data.len() == expected).then_some(data)
}

/// Pack one decoded CCITT line (color-transition list) into a pre-zeroed
/// sample row. Sample value = `(pel is black) == black_is_1`, i.e. under the
/// default `BlackIs1 false`, black pels are 0 — matching DeviceGray.
fn pack_row(row: &mut [u8], transitions: &[u32], width: u32, black_is_1: bool) {
    for (x, color) in pels(transitions, width).enumerate() {
        let bit = ((color == Color::Black) == black_is_1) as u8;
        row[x / 8] |= bit << (7 - x % 8);
    }
}

/// Strict G4 decode to packed samples: exactly `height` coding lines followed
/// by a clean EOFB. Early EOFB (fax's `decode_g4` would silently white-pad),
/// surplus rows, and any coding error all return `None` — a damaged original
/// is never re-encoded as pristine-wrong data.
fn decode_g4_strict_to_samples(
    data: &[u8],
    width: u32,
    height: usize,
    black_is_1: bool,
) -> Option<Vec<u8>> {
    let reader = data.iter().cloned().map(Result::<u8, Infallible>::Ok);
    let mut dec = Group4Decoder::new(reader, width).ok()?;
    let stride = (width as usize).div_ceil(8);
    let mut out = vec![0u8; stride.checked_mul(height)?];
    for r in 0..height {
        match dec.advance().ok()? {
            DecodeStatus::Incomplete => pack_row(
                &mut out[r * stride..(r + 1) * stride],
                dec.transition(),
                width,
                black_is_1,
            ),
            DecodeStatus::End => return None, // EOFB before /Height rows
        }
    }
    match dec.advance().ok()? {
        DecodeStatus::End => Some(out),
        DecodeStatus::Incomplete => None, // stream codes more rows than /Height
    }
}

/// Strict G3 1D decode to packed samples: exactly `height` rows, each coding
/// every pel (runs sum exactly to the width — the decoder's final transition
/// for a clean T.4 row always lands on `width`), terminated by RTC.
fn decode_g3_strict_to_samples(
    data: &[u8],
    width: u32,
    height: usize,
    black_is_1: bool,
) -> Option<Vec<u8>> {
    let reader = data.iter().cloned().map(Result::<u8, Infallible>::Ok);
    let mut dec = Group3Decoder::new(reader).ok()?;
    let stride = (width as usize).div_ceil(8);
    let mut out = vec![0u8; stride.checked_mul(height)?];
    let mut r = 0;
    loop {
        let status = dec.advance().ok()?;
        let transitions = dec.transitions();
        if status == DecodeStatus::End && transitions.is_empty() {
            // RTC reached with no pending row data.
            break;
        }
        if r >= height
            || transitions.last().copied() != Some(width)
            || transitions.iter().any(|&t| t > width)
        {
            return None;
        }
        pack_row(
            &mut out[r * stride..(r + 1) * stride],
            transitions,
            width,
            black_is_1,
        );
        r += 1;
        if status == DecodeStatus::End {
            break;
        }
    }
    (r == height).then_some(out)
}

/// Re-encode packed sample rows as CCITT G4. Sample 0 encodes as a black pel,
/// the inverse of `pack_row` under the `BlackIs1 false` parms we write.
fn encode_g4_from_samples(samples: &[u8], width: u32, height: usize) -> Vec<u8> {
    let stride = (width as usize).div_ceil(8);
    let mut enc = Encoder::new(VecWriter::new());
    for r in 0..height {
        let row = &samples[r * stride..(r + 1) * stride];
        enc.encode_line(
            (0..width as usize).map(|x| {
                if row[x / 8] >> (7 - x % 8) & 1 == 0 {
                    Color::Black
                } else {
                    Color::White
                }
            }),
            width,
        )
        .unwrap(); // Infallible writer
    }
    enc.finish().unwrap().finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deflate::deflate_level9;
    use crate::{optimize_with_options, OptimizeOptions};
    use fax::BitWriter;
    use lopdf::{dictionary, Dictionary, Document, Stream};

    /// The G4 target alone: these tests pin G4 behavior, so the default-on
    /// JBIG2 competitor is switched off (it has its own tests below).
    fn opts() -> OptimizeOptions {
        OptimizeOptions::default()
            .with_recompress_bitonal_images(true)
            .with_jbig2_bitonal_images(false)
    }

    /// Deterministic LCG so failures reproduce (same as tests/ccitt_vendor_contract.rs).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    /// Text-like bitonal ink pattern (true = black ink) — the realistic
    /// scanned-document shape where G4 beats deflate (§B.3 measurements).
    fn doc_rows(width: usize, height: usize) -> Vec<Vec<bool>> {
        let mut rng = Lcg(42);
        let mut rows = vec![vec![false; width]; height];
        let mut y = 8;
        while y < height.saturating_sub(4) {
            let band_h = 3 + (rng.next() % 5) as usize;
            let mut x = 16 + (rng.next() % 24) as usize;
            while x < width - 16 {
                let gw = 1 + (rng.next() % 6) as usize;
                let gh = 1 + (rng.next() % band_h as u64) as usize;
                for dy in 0..gh.min(band_h) {
                    for dx in 0..gw {
                        if x + dx < width && y + dy < height {
                            rows[y + dy][x + dx] = true;
                        }
                    }
                }
                x += gw + 2 + (rng.next() % 10) as usize;
            }
            y += band_h + 6 + (rng.next() % 12) as usize;
        }
        rows
    }

    fn noise_rows(width: usize, height: usize) -> Vec<Vec<bool>> {
        let mut rng = Lcg(7);
        (0..height)
            .map(|_| (0..width).map(|_| rng.next() & 1 == 1).collect())
            .collect()
    }

    /// Pack ink rows into PDF 1-bit sample rows under the given `/BlackIs1`:
    /// sample = (ink is black) == black_is_1.
    fn pack_samples(rows: &[Vec<bool>], black_is_1: bool) -> Vec<u8> {
        let width = rows[0].len();
        let stride = width.div_ceil(8);
        let mut out = vec![0u8; stride * rows.len()];
        for (r, row) in rows.iter().enumerate() {
            for (x, &black) in row.iter().enumerate() {
                let bit = (black == black_is_1) as u8;
                out[r * stride + x / 8] |= bit << (7 - x % 8);
            }
        }
        out
    }

    /// Encode ink rows as G4 with fax (true = black pel).
    fn g4_of(rows: &[Vec<bool>]) -> Vec<u8> {
        let mut enc = Encoder::new(VecWriter::new());
        for row in rows {
            enc.encode_line(
                row.iter()
                    .map(|&b| if b { Color::Black } else { Color::White }),
                row.len() as u32,
            )
            .unwrap();
        }
        enc.finish().unwrap().finish()
    }

    /// Encode ink rows as EOL-framed G3 1D using fax's public code tables
    /// (same construction as tests/ccitt_vendor_contract.rs — fax ships no G3 encoder).
    fn g3_of(rows: &[Vec<bool>]) -> Vec<u8> {
        use fax::maps::{black, white, EOL};
        fn write_run(w: &mut VecWriter, color: Color, mut n: u32) {
            let table = match color {
                Color::White => &white::ENTRIES,
                Color::Black => &black::ENTRIES,
            };
            let emit = |w: &mut VecWriter, n: u32| {
                let idx = if n >= 64 { 63 + n / 64 } else { n } as usize;
                let (v, bits) = table[idx];
                assert_eq!(v as u32, n);
                w.write(bits).unwrap();
            };
            while n >= 2560 {
                emit(w, 2560);
                n -= 2560;
            }
            if n >= 64 {
                let d = n & !63;
                emit(w, d);
                n -= d;
            }
            emit(w, n);
        }
        let mut w = VecWriter::new();
        w.write(EOL).unwrap();
        for row in rows {
            let mut color = Color::White;
            let mut run = 0u32;
            for &px in row {
                let c = if px { Color::Black } else { Color::White };
                if c == color {
                    run += 1;
                } else {
                    write_run(&mut w, color, run);
                    color = c;
                    run = 1;
                }
            }
            write_run(&mut w, color, run);
            w.write(EOL).unwrap();
        }
        for _ in 0..5 {
            w.write(EOL).unwrap();
        }
        w.finish()
    }

    /// One-page PDF around a single image XObject built from the given dict.
    fn bitonal_pdf(image_dict: Dictionary, content: Vec<u8>) -> Vec<u8> {
        bitonal_pdf_version(image_dict, content, "1.5")
    }

    fn bitonal_pdf_version(image_dict: Dictionary, content: Vec<u8>, version: &str) -> Vec<u8> {
        let mut doc = Document::with_version(version);
        let img_id = doc.add_object(Stream::new(image_dict, content));
        let page_content = b"q 200 0 0 200 0 0 cm /Im0 Do Q".to_vec();
        let content_id = doc.add_object(Stream::new(dictionary! {}, page_content));
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! {
                "XObject" => dictionary! { "Im0" => img_id },
            },
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        if version < "1.5" {
            // A classic table keeps lopdf's plain save at the given version
            // (its default xref stream would raise the header to 1.5).
            doc.reference_table.cross_reference_type = lopdf::xref::XrefType::CrossReferenceTable;
        }
        doc.trailer.set("Root", catalog_id);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    /// Standard eligible dict for a W×H bitonal image with the given filter.
    fn image_dict(w: usize, h: usize, filter: &str, parms: Option<Dictionary>) -> Dictionary {
        let mut d = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => w as i64,
            "Height" => h as i64,
            "ColorSpace" => "DeviceGray",
            "BitsPerComponent" => 1,
            "Filter" => filter,
        };
        if let Some(p) = parms {
            d.set("DecodeParms", Object::Dictionary(p));
        }
        d
    }

    fn g3_parms(w: usize, h: usize) -> Dictionary {
        dictionary! {
            "K" => 0,
            "EndOfLine" => true,
            "Columns" => w as i64,
            "Rows" => h as i64,
        }
    }

    /// The single image stream in an optimized PDF.
    fn image_stream(pdf: &[u8]) -> lopdf::Stream {
        let doc = Document::load_mem(pdf).unwrap();
        doc.objects
            .values()
            .find_map(|o| {
                let s = o.as_stream().ok()?;
                matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image")
                    .then(|| s.clone())
            })
            .expect("image stream present")
    }

    /// Assert the image was rewritten to G4 with normalized parms and that its
    /// decoded samples equal `want`; returns the stream for extra checks.
    fn assert_g4_with_samples(pdf: &[u8], w: usize, h: usize, want: &[u8]) -> lopdf::Stream {
        let img = image_stream(pdf);
        assert_eq!(
            img.dict.get(b"Filter").unwrap().as_name().unwrap(),
            b"CCITTFaxDecode"
        );
        assert_eq!(img.dict.get(b"Width").unwrap().as_i64().unwrap(), w as i64);
        assert_eq!(img.dict.get(b"Height").unwrap().as_i64().unwrap(), h as i64);
        let parms = img.dict.get(b"DecodeParms").unwrap().as_dict().unwrap();
        assert_eq!(parms.get(b"K").unwrap().as_i64().unwrap(), -1);
        assert_eq!(parms.get(b"Columns").unwrap().as_i64().unwrap(), w as i64);
        assert_eq!(parms.get(b"Rows").unwrap().as_i64().unwrap(), h as i64);
        assert!(!parms.get(b"BlackIs1").unwrap().as_bool().unwrap());
        let decoded = decode_g4_strict_to_samples(&img.content, w as u32, h, false)
            .expect("output stream decodes cleanly");
        assert_eq!(decoded, want, "samples changed by recompression");
        img
    }

    const W: usize = 1728;
    const H: usize = 512;

    #[test]
    fn flate_bitonal_recompressed_to_g4() {
        let rows = doc_rows(W, H);
        let samples = pack_samples(&rows, false);
        let flate = deflate_level9(&samples).unwrap();
        let pdf = bitonal_pdf(image_dict(W, H, "FlateDecode", None), flate);
        let out = optimize_with_options(&pdf, opts());
        assert!(out.len() < pdf.len(), "output must be smaller");
        assert_g4_with_samples(&out, W, H, &samples);
    }

    #[test]
    fn option_off_by_default_leaves_bitonal_untouched() {
        let samples = pack_samples(&doc_rows(W, H), false);
        let flate = deflate_level9(&samples).unwrap();
        let pdf = bitonal_pdf(image_dict(W, H, "FlateDecode", None), flate);
        let out = optimize_with_options(&pdf, OptimizeOptions::default());
        assert_eq!(out, pdf, "default options must not touch bitonal images");
    }

    #[test]
    fn g3_source_recompressed_to_g4() {
        let rows = doc_rows(W, 256);
        let samples = pack_samples(&rows, false);
        let g3 = g3_of(&rows);
        let pdf = bitonal_pdf(
            image_dict(W, 256, "CCITTFaxDecode", Some(g3_parms(W, 256))),
            g3,
        );
        let out = optimize_with_options(&pdf, opts());
        assert!(out.len() < pdf.len());
        assert_g4_with_samples(&out, W, 256, &samples);
    }

    #[test]
    fn filter_array_form_is_accepted() {
        let rows = doc_rows(W, 256);
        let samples = pack_samples(&rows, false);
        let g3 = g3_of(&rows);
        let mut dict = image_dict(W, 256, "CCITTFaxDecode", Some(g3_parms(W, 256)));
        dict.set(
            "Filter",
            Object::Array(vec![Object::Name(b"CCITTFaxDecode".to_vec())]),
        );
        let out = optimize_with_options(&bitonal_pdf(dict, g3), opts());
        assert_g4_with_samples(&out, W, 256, &samples);
    }

    #[test]
    fn already_g4_is_untouched_never_larger() {
        // Re-encoding our own G4 reproduces the same bytes; the strictly-
        // smaller guard must then leave the stream alone.
        let rows = doc_rows(W, 256);
        let g4 = g4_of(&rows);
        let parms = dictionary! { "K" => -1, "Columns" => W as i64, "Rows" => 256 };
        let pdf = bitonal_pdf(image_dict(W, 256, "CCITTFaxDecode", Some(parms)), g4);
        let out = optimize_with_options(&pdf, opts());
        assert_eq!(out, pdf, "already-optimal G4 must be untouched");
    }

    #[test]
    fn noise_never_larger() {
        // G4 expands on noise; the guard must leave the flate original alone.
        let samples = pack_samples(&noise_rows(256, 256), false);
        let flate = deflate_level9(&samples).unwrap();
        let pdf = bitonal_pdf(image_dict(256, 256, "FlateDecode", None), flate);
        let out = optimize_with_options(&pdf, opts());
        assert_eq!(out, pdf, "noise must never be replaced by a larger G4");
    }

    #[test]
    fn optimize_is_idempotent() {
        let rows = doc_rows(W, H);
        let flate = deflate_level9(&pack_samples(&rows, false)).unwrap();
        let pdf = bitonal_pdf(image_dict(W, H, "FlateDecode", None), flate);
        let once = optimize_with_options(&pdf, opts());
        let twice = optimize_with_options(&once, opts());
        assert_eq!(once, twice, "second pass must be a no-op");
    }

    #[test]
    fn blackis1_both_polarities_round_trip() {
        // The same CCITT bitstream under either /BlackIs1 produces different
        // sample data; recompression must preserve each faithfully while
        // normalizing the output parms to BlackIs1 false.
        let rows = doc_rows(W, 128);
        let g3 = g3_of(&rows);
        for black_is_1 in [false, true] {
            let want = pack_samples(&rows, black_is_1);
            let mut parms = g3_parms(W, 128);
            parms.set("BlackIs1", black_is_1);
            let pdf = bitonal_pdf(
                image_dict(W, 128, "CCITTFaxDecode", Some(parms)),
                g3.clone(),
            );
            let out = optimize_with_options(&pdf, opts());
            assert_g4_with_samples(&out, W, 128, &want);
        }
    }

    #[test]
    fn encoded_byte_align_is_skipped() {
        // Otherwise-eligible stream; the flag alone must gate it out.
        let rows = doc_rows(W, 256);
        let mut parms = g3_parms(W, 256);
        parms.set("EncodedByteAlign", true);
        let pdf = bitonal_pdf(
            image_dict(W, 256, "CCITTFaxDecode", Some(parms)),
            g3_of(&rows),
        );
        let out = optimize_with_options(&pdf, opts());
        assert_eq!(out, pdf, "/EncodedByteAlign true must be a fail-safe skip");
    }

    #[test]
    fn unsupported_k_modes_are_skipped() {
        let rows = doc_rows(W, 128);
        let g3 = g3_of(&rows);
        // K > 0 (mixed 2D): unsupported by fax's G3 decoder.
        let mut parms = g3_parms(W, 128);
        parms.set("K", 1);
        let pdf = bitonal_pdf(
            image_dict(W, 128, "CCITTFaxDecode", Some(parms)),
            g3.clone(),
        );
        assert_eq!(optimize_with_options(&pdf, opts()), pdf, "K > 0 must skip");
        // K == 0 without /EndOfLine true: misframed by fax's EOL-based decoder.
        let mut parms = g3_parms(W, 128);
        parms.remove(b"EndOfLine");
        let pdf = bitonal_pdf(image_dict(W, 128, "CCITTFaxDecode", Some(parms)), g3);
        assert_eq!(
            optimize_with_options(&pdf, opts()),
            pdf,
            "K == 0 without /EndOfLine true must skip"
        );
    }

    #[test]
    fn corrupt_streams_return_exact_original_bytes() {
        let rows = doc_rows(W, 256);
        let g4 = g4_of(&rows);
        let g4_parms = || dictionary! { "K" => -1, "Columns" => W as i64, "Rows" => 256 };
        // Truncated G4.
        let pdf = bitonal_pdf(
            image_dict(W, 256, "CCITTFaxDecode", Some(g4_parms())),
            g4[..g4.len() / 2].to_vec(),
        );
        assert_eq!(optimize_with_options(&pdf, opts()), pdf, "truncated G4");
        // Pure garbage as G4.
        let garbage: Vec<u8> = (0..4096u32).map(|i| (i * 37 + 13) as u8).collect();
        let pdf = bitonal_pdf(
            image_dict(W, 256, "CCITTFaxDecode", Some(g4_parms())),
            garbage,
        );
        assert_eq!(optimize_with_options(&pdf, opts()), pdf, "garbage G4");
        // Truncated G3.
        let g3 = g3_of(&rows);
        let pdf = bitonal_pdf(
            image_dict(W, 256, "CCITTFaxDecode", Some(g3_parms(W, 256))),
            g3[..g3.len() / 2].to_vec(),
        );
        assert_eq!(optimize_with_options(&pdf, opts()), pdf, "truncated G3");
        // Truncated Flate.
        let flate = deflate_level9(&pack_samples(&rows, false)).unwrap();
        let pdf = bitonal_pdf(
            image_dict(W, 256, "FlateDecode", None),
            flate[..flate.len() / 2].to_vec(),
        );
        assert_eq!(optimize_with_options(&pdf, opts()), pdf, "truncated Flate");
    }

    #[test]
    fn height_mismatch_is_skipped() {
        // Stream codes 200 rows but the dict claims 256: fax's lenient
        // decode_g4 would white-pad; the strict path must skip instead.
        let rows = doc_rows(W, 200);
        let g4 = g4_of(&rows);
        let parms = dictionary! { "K" => -1, "Columns" => W as i64, "Rows" => 256 };
        let pdf = bitonal_pdf(image_dict(W, 256, "CCITTFaxDecode", Some(parms)), g4);
        assert_eq!(
            optimize_with_options(&pdf, opts()),
            pdf,
            "row-count mismatch must never be re-encoded as padded data"
        );
    }

    #[test]
    fn image_mask_with_implicit_colorspace_recompressed() {
        let rows = doc_rows(W, H);
        let samples = pack_samples(&rows, false);
        let flate = deflate_level9(&samples).unwrap();
        let dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => W as i64,
            "Height" => H as i64,
            "ImageMask" => true,
            "Filter" => "FlateDecode",
        };
        let out = optimize_with_options(&bitonal_pdf(dict, flate), opts());
        assert_g4_with_samples(&out, W, H, &samples);
    }

    #[test]
    fn non_identity_decode_is_skipped() {
        let samples = pack_samples(&doc_rows(W, H), false);
        let flate = deflate_level9(&samples).unwrap();
        let mut dict = image_dict(W, H, "FlateDecode", None);
        dict.set("Decode", vec![1.into(), 0.into()]);
        let pdf = bitonal_pdf(dict, flate);
        assert_eq!(
            optimize_with_options(&pdf, opts()),
            pdf,
            "a non-identity /Decode remap must gate the image out"
        );
    }

    // ---- JBIG2 target -------------------------------------------------

    fn g4_parms(w: usize, h: usize) -> Dictionary {
        dictionary! { "K" => -1, "Columns" => w as i64, "Rows" => h as i64 }
    }

    /// Assert the image was re-stored as JBIG2 (no `/DecodeParms`) and that
    /// its stream decodes back to the PDF samples `want`.
    fn assert_jbig2_with_samples(pdf: &[u8], w: usize, h: usize, want: &[u8]) -> lopdf::Stream {
        let img = image_stream(pdf);
        assert_eq!(
            img.dict.get(b"Filter").unwrap().as_name().unwrap(),
            b"JBIG2Decode"
        );
        assert!(
            img.dict.get(b"DecodeParms").is_err(),
            "JBIG2 needs no parms"
        );
        assert_eq!(img.dict.get(b"Width").unwrap().as_i64().unwrap(), w as i64);
        assert_eq!(img.dict.get(b"Height").unwrap().as_i64().unwrap(), h as i64);
        let got = jbig2::decode_embedded(&img.content, w as u32, h as u32)
            .expect("output stream decodes");
        assert_eq!(
            Some(got),
            jbig2::samples_to_bitmap(want, w, h),
            "samples changed by the JBIG2 re-store"
        );
        img
    }

    #[test]
    fn ccitt_g4_is_restored_as_jbig2_by_default() {
        let rows = doc_rows(W, H);
        let pdf = bitonal_pdf(
            image_dict(W, H, "CCITTFaxDecode", Some(g4_parms(W, H))),
            g4_of(&rows),
        );
        let out = optimize_with_options(&pdf, OptimizeOptions::default());
        assert!(out.len() < pdf.len(), "output must be smaller");
        assert_jbig2_with_samples(&out, W, H, &pack_samples(&rows, false));
    }

    #[test]
    fn jbig2_opt_out_leaves_ccitt_untouched() {
        let rows = doc_rows(W, H);
        let pdf = bitonal_pdf(
            image_dict(W, H, "CCITTFaxDecode", Some(g4_parms(W, H))),
            g4_of(&rows),
        );
        let out = optimize_with_options(
            &pdf,
            OptimizeOptions::default().with_jbig2_bitonal_images(false),
        );
        assert_eq!(out, pdf, "opt-out must leave the CCITT image alone");
    }

    #[test]
    fn jbig2_preserves_samples_under_both_blackis1_polarities() {
        // Same bitstream, opposite /BlackIs1: different samples, and the
        // JBIG2 stream must carry each one exactly (G3 and G4 sources).
        let rows = doc_rows(W, 128);
        for black_is_1 in [false, true] {
            let want = pack_samples(&rows, black_is_1);
            for (data, mut parms) in [
                (g4_of(&rows), g4_parms(W, 128)),
                (g3_of(&rows), g3_parms(W, 128)),
            ] {
                parms.set("BlackIs1", black_is_1);
                let pdf = bitonal_pdf(image_dict(W, 128, "CCITTFaxDecode", Some(parms)), data);
                let out = optimize_with_options(&pdf, OptimizeOptions::default());
                assert_jbig2_with_samples(&out, W, 128, &want);
            }
        }
    }

    #[test]
    fn jbig2_keeps_decode_colorspace_and_image_mask_keys() {
        // JBIG2 preserves samples, so /Decode, a non-DeviceGray /ColorSpace
        // and /ImageMask stay exactly as they were (they apply to the same
        // values afterwards).
        let rows = doc_rows(W, 256);
        let samples = pack_samples(&rows, false);
        let mut d = image_dict(W, 256, "CCITTFaxDecode", Some(g4_parms(W, 256)));
        d.set("Decode", vec![1.into(), 0.into()]);
        d.set(
            "ColorSpace",
            vec![
                Object::Name(b"CalGray".to_vec()),
                Object::Dictionary(
                    dictionary! { "WhitePoint" => vec![0.9505.into(), 1.into(), 1.089.into()] },
                ),
            ],
        );
        let out = optimize_with_options(&bitonal_pdf(d, g4_of(&rows)), OptimizeOptions::default());
        let img = assert_jbig2_with_samples(&out, W, 256, &samples);
        let dec = img.dict.get(b"Decode").unwrap().as_array().unwrap();
        assert_eq!((num(&dec[0]), num(&dec[1])), (1.0, 0.0));
        assert!(img.dict.get(b"ColorSpace").unwrap().as_array().is_ok());

        let mask = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => W as i64,
            "Height" => 256,
            "ImageMask" => true,
            "Filter" => "CCITTFaxDecode",
            "DecodeParms" => g4_parms(W, 256),
        };
        let out =
            optimize_with_options(&bitonal_pdf(mask, g4_of(&rows)), OptimizeOptions::default());
        let img = assert_jbig2_with_samples(&out, W, 256, &samples);
        assert_eq!(img.dict.get(b"ImageMask").unwrap(), &Object::Boolean(true));
    }

    #[test]
    fn jbig2_accepts_array_and_reference_decode_parms() {
        let rows = doc_rows(W, 200);
        let samples = pack_samples(&rows, false);
        // `/Filter [/CCITTFaxDecode] /DecodeParms [<< ... >>]`.
        let mut d = image_dict(W, 200, "CCITTFaxDecode", None);
        d.set("Filter", vec![Object::Name(b"CCITTFaxDecode".to_vec())]);
        d.set("DecodeParms", vec![Object::Dictionary(g4_parms(W, 200))]);
        let out = optimize_with_options(&bitonal_pdf(d, g4_of(&rows)), OptimizeOptions::default());
        assert_jbig2_with_samples(&out, W, 200, &samples);
        // `/DecodeParms` as an indirect reference.
        let mut doc = Document::load_mem(&bitonal_pdf(
            image_dict(W, 200, "CCITTFaxDecode", None),
            g4_of(&rows),
        ))
        .unwrap();
        let parms_id = doc.add_object(g4_parms(W, 200));
        for obj in doc.objects.values_mut() {
            if let Object::Stream(s) = obj {
                if s.dict.has(b"Width") {
                    s.dict.set("DecodeParms", parms_id);
                }
            }
        }
        let mut pdf = Vec::new();
        doc.save_to(&mut pdf).unwrap();
        let out = optimize_with_options(&pdf, OptimizeOptions::default());
        assert_jbig2_with_samples(&out, W, 200, &samples);
    }

    #[test]
    fn jbig2_is_idempotent() {
        let rows = doc_rows(W, H);
        let pdf = bitonal_pdf(
            image_dict(W, H, "CCITTFaxDecode", Some(g4_parms(W, H))),
            g4_of(&rows),
        );
        let once = optimize_with_options(&pdf, OptimizeOptions::default());
        assert_ne!(once, pdf);
        let twice = optimize_with_options(&once, OptimizeOptions::default());
        assert_eq!(once, twice, "a JBIG2 image is never touched again");
    }

    #[test]
    fn jbig2_never_replaces_with_a_larger_stream() {
        // A tiny G4 image is a handful of bytes; the JBIG2 framing alone is
        // ~70. Strictly-smaller fails, so the image stays as stored.
        let rows = doc_rows(64, 16);
        let pdf = bitonal_pdf(
            image_dict(64, 16, "CCITTFaxDecode", Some(g4_parms(64, 16))),
            g4_of(&rows),
        );
        assert_eq!(optimize_with_options(&pdf, OptimizeOptions::default()), pdf);
    }

    #[test]
    fn existing_jbig2_images_are_never_touched() {
        let rows = doc_rows(W, H);
        let mut bm = jbig2::samples_to_bitmap(&pack_samples(&rows, false), W, H).unwrap();
        let stream =
            jbig2::encode_best(&mut bm, &[jbig2::GenericParams::nominal(3, false)]).unwrap();
        let mut d = image_dict(W, H, "JBIG2Decode", None);
        d.set("Filter", "JBIG2Decode");
        let pdf = bitonal_pdf(d, stream);
        let out = optimize_with_options(
            &pdf,
            OptimizeOptions::default().with_recompress_bitonal_images(true),
        );
        assert_eq!(
            out, pdf,
            "JBIG2 sources are out of every bitonal target's scope"
        );
    }

    #[test]
    fn jbig2_declines_documents_claiming_pdfa() {
        let rows = doc_rows(W, H);
        let mut doc = Document::load_mem(&bitonal_pdf(
            image_dict(W, H, "CCITTFaxDecode", Some(g4_parms(W, H))),
            g4_of(&rows),
        ))
        .unwrap();
        let xmp = doc.add_object(Stream::new(
            dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
            b"<x:xmpmeta><rdf:Description xmlns:pdfaid=\"http://www.aiim.org/pdfa/ns/id/\"><pdfaid:part>1</pdfaid:part></rdf:Description></x:xmpmeta>".to_vec(),
        ));
        doc.catalog_mut().unwrap().set("Metadata", xmp);
        let mut pdf = Vec::new();
        doc.save_to(&mut pdf).unwrap();
        let out = optimize_with_options(&pdf, OptimizeOptions::default());
        let img = image_stream(&out);
        assert_eq!(
            img.dict.get(b"Filter").unwrap().as_name().unwrap(),
            b"CCITTFaxDecode",
            "PDF/A claims decline the JBIG2 re-store"
        );
    }

    #[test]
    fn jbig2_raises_a_pre_1_4_header_on_the_classic_save_path() {
        let rows = doc_rows(W, H);
        let pdf = bitonal_pdf_version(
            image_dict(W, H, "CCITTFaxDecode", Some(g4_parms(W, H))),
            g4_of(&rows),
            "1.3",
        );
        assert!(pdf.starts_with(b"%PDF-1.3"));
        let out = optimize_with_options(
            &pdf,
            OptimizeOptions::default().with_pack_object_streams(false),
        );
        assert!(out.starts_with(b"%PDF-1.4"), "JBIG2Decode needs PDF 1.4");
        assert_jbig2_with_samples(&out, W, H, &pack_samples(&rows, false));
        let packed = optimize_with_options(&pdf, OptimizeOptions::default());
        assert!(packed.starts_with(b"%PDF-1.5"));
    }

    #[test]
    fn g4_and_jbig2_together_pick_the_smaller_and_stay_idempotent() {
        // With both targets on, a Flate source gets both candidates; text
        // compresses better as JBIG2, and a second pass changes nothing.
        let rows = doc_rows(W, H);
        let samples = pack_samples(&rows, false);
        let flate = deflate_level9(&samples).unwrap();
        let pdf = bitonal_pdf(image_dict(W, H, "FlateDecode", None), flate);
        let both = OptimizeOptions::default().with_recompress_bitonal_images(true);
        let once = optimize_with_options(&pdf, both);
        assert_jbig2_with_samples(&once, W, H, &samples);
        assert_eq!(optimize_with_options(&once, both), once);
        // And a G4 source already smaller than its JBIG2 form stays G4: the
        // next run compares the same two candidates and declines again.
        let g4_only = optimize_with_options(&pdf, opts());
        assert!(optimize_with_options(&g4_only, both).len() <= g4_only.len());
        let again = optimize_with_options(&g4_only, both);
        assert_eq!(optimize_with_options(&again, both), again);
    }

    #[test]
    fn flate_bitonal_is_not_a_jbig2_source_without_the_g4_opt_in() {
        let samples = pack_samples(&doc_rows(W, H), false);
        let flate = deflate_level9(&samples).unwrap();
        let pdf = bitonal_pdf(image_dict(W, H, "FlateDecode", None), flate);
        assert_eq!(optimize_with_options(&pdf, OptimizeOptions::default()), pdf);
    }

    #[test]
    fn hostile_dimensions_are_declined_before_allocating() {
        let garbage: Vec<u8> = (0..4096u32).map(|i| (i * 37 + 13) as u8).collect();
        for (w, h) in [
            (1_i64 << 20, 1_i64 << 20),
            (-5, 100),
            (100, 0),
            (i64::MAX, 2),
        ] {
            let mut d = image_dict(1, 1, "CCITTFaxDecode", None);
            d.set("Width", w);
            d.set("Height", h);
            d.set("DecodeParms", dictionary! { "K" => -1, "Columns" => w });
            let pdf = bitonal_pdf(d, garbage.clone());
            assert_eq!(optimize_with_options(&pdf, OptimizeOptions::default()), pdf);
        }
    }
}
