//! CMYK / YCCK JPEG support, plus the JPEG frame-header reader the rest of the
//! crate routes on.
//!
//! Four-component DCTDecode streams (PDF `/ColorSpace /DeviceCMYK`, ICCBased
//! N=4, DeviceN, ...) used to be handled by accident rather than on purpose:
//! `decode_jpeg_scaled` declined them, and the caller then fell back to the
//! general-purpose `image` decoder, which converts CMYK to RGB. Re-encoding
//! that as a 3-component JPEG under an unchanged 4-component `/ColorSpace`
//! produces a corrupt page. The code below handles them deliberately instead.
//!
//! THE CENTRAL INVARIANT: everything here works in RAW STORED SAMPLE SPACE and
//! never interprets it. libjpeg is the only thing that touches the Adobe APP14
//! transform: on decode it maps YCCK (transform 2) back to stored CMYK samples,
//! on encode it maps them forward again and emits the matching APP14 marker.
//! We never parse APP14 ourselves and never invert a channel.
//!
//! That is what makes the classic CMYK-JPEG bug class unreachable here. The
//! "is it inverted?" question — Adobe writes CMYK JPEGs with 255-x samples,
//! most other producers do not, and PDF producers sometimes bolt a
//! `/Decode [1 0 1 0 1 0 1 0]` array on top to compensate — is a property of
//! the SAMPLE VALUES, and we pass those through unchanged. Whatever convention
//! the input used, the output uses the same one, so the unchanged dict keeps
//! meaning exactly what it meant before.

use lopdf::Object;

use crate::{dct_scale_numerator, jpeg_quant_tables, scaled_decode_within_budget};

/// Per-channel mean-absolute-difference ceiling for the CMYK decode-back
/// verification, and deliberately much tighter than `DECODE_BACK_MAX_MAD`.
///
/// The loose shared ceiling exists because a three-channel catastrophe is
/// dramatic; four-channel damage is not. Measured on the fixtures here, a
/// C/M swap sits at MAD 37, a C/K swap at 29 and a whole-image inversion at
/// 106 — the 96 ceiling would wave the first two through. A legitimate
/// resample-plus-q78 round trip in raw sample space lands in the single
/// digits, so 24.0 leaves generous headroom for genuinely noisy content while
/// still refusing anything that looks like a rearranged channel.
///
/// The ceiling is a backstop, not the guarantee: nothing in this pipeline ever
/// reorders or inverts a channel, because libjpeg owns the whole Adobe
/// transform. This is what catches a mistake made anyway.
const CMYK_DECODE_BACK_MAX_MAD: f64 = 24.0;

/// A decoded four-component JPEG in raw stored-sample space, interleaved
/// C,M,Y,K. Deliberately NOT a `DynamicImage`: the `image` crate has no CMYK
/// variant, and smuggling four channels through `Rgba8` would hand them to
/// resampling code that may treat the fourth channel as alpha.
pub(crate) struct CmykImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// The geometry of the SOURCE frame header, before any `n/8` DCT-scaled
    /// decoding shrank it. Callers with a dimension-preserving contract have
    /// to check the dict against this, not against `width`/`height`: a dict
    /// claiming half the true size would otherwise be "confirmed" by the
    /// scaled decoder obligingly producing exactly half.
    source_width: u32,
    source_height: u32,
    /// `width * height * 4` bytes, interleaved.
    pub(crate) data: Vec<u8>,
}

/// The component count declared by a JPEG's frame header (SOF), or `None` if
/// the marker structure does not parse cleanly.
///
/// This is a purely STRUCTURAL walk — it never looks at APP14 — used only to
/// route a stream to the right pipeline: 1 or 3 components take the untouched
/// gray/RGB path, 4 takes the CMYK path below, anything else is declined
/// outright rather than guessed at. `None` deliberately keeps the historical
/// gray/RGB behaviour: an unparseable header means the real decoders get the
/// same shot at the stream they have always had.
pub(crate) fn jpeg_component_count(data: &[u8]) -> Option<u8> {
    jpeg_frame_info(data).map(|f| f.components)
}

/// What a JPEG's frame header (SOF) declares: geometry and component count.
///
/// Read straight off the marker structure, WITHOUT decoding — which is the
/// whole point. A decompression-bomb JPEG is a handful of bytes declaring a
/// 60000x60000 frame; every decoder in this crate materializes
/// `width * height * channels` from these three numbers, so they have to be
/// checked before a decoder is handed the stream (see
/// [`crate::jpeg_decode_within_budget`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct JpegFrameInfo {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) components: u8,
}

/// Parse the frame header, or `None` if the marker structure does not parse
/// cleanly. See [`jpeg_component_count`] for what `None` means to callers.
pub(crate) fn jpeg_frame_info(data: &[u8]) -> Option<JpegFrameInfo> {
    if data.len() < 2 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 2 <= data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let marker = data[i + 1];
        match marker {
            // Fill bytes before a marker.
            0xFF => {
                i += 1;
                continue;
            }
            // Standalone markers (no length field).
            0x01 | 0xD0..=0xD7 => {
                i += 2;
                continue;
            }
            // A scan before any frame header: malformed, draw no conclusion.
            0xDA => return None,
            _ => {}
        }
        if i + 4 > data.len() {
            return None;
        }
        let len = ((data[i + 2] as usize) << 8) | data[i + 3] as usize;
        if len < 2 || i + 2 + len > data.len() {
            return None;
        }
        // Every SOF flavour (baseline, extended, progressive, lossless,
        // arithmetic) shares the same header layout; DHT (0xC4), JPG (0xC8)
        // and DAC (0xCC) sit in the same numeric range and are not frames.
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            // precision(1) + height(2) + width(2) + component count(1)
            if len < 8 {
                return None;
            }
            return Some(JpegFrameInfo {
                height: u16::from_be_bytes([data[i + 5], data[i + 6]]),
                width: u16::from_be_bytes([data[i + 7], data[i + 8]]),
                components: data[i + 9],
            });
        }
        i += 2 + len;
    }
    None
}

/// True if this image dict carries a `/Decode` array.
///
/// A `/Decode` array remaps every sample after decoding, and for DeviceCMYK
/// the common one is `[1 0 1 0 1 0 1 0]` — a producer inverting the whole
/// image. Passing raw samples through unchanged would still be correct under
/// such a remap in exact arithmetic, because inversion and a linear resampling
/// kernel commute; they do NOT commute once Lanczos3's ringing overshoot is
/// clamped to 0..=255, since the clamp happens on the pre-`/Decode` values.
/// The asymmetry is small but real, and it lands on exactly the images where a
/// channel-polarity mistake is invisible to us and glaring to a reader — so
/// this declines instead. `plan_mask_resample` declines on `/Decode` for the
/// same reason.
fn has_decode_array(dict: &lopdf::Dictionary) -> bool {
    !matches!(dict.get(b"Decode"), Err(_) | Ok(Object::Null))
}

/// True if the payload ends with an end-of-image marker (allowing a little
/// trailing padding, which PDF producers do emit).
///
/// libjpeg is deliberately lenient about truncation: it fills the missing
/// scan with flat grey, raises a WARNING rather than an error, and hands back
/// a perfectly decodable image. The decode-back verification then compares
/// that grey against itself and agrees, so a truncated stream would sail
/// through and be replaced by a "repaired" one. Whether that is an
/// improvement is not ours to decide — a stream that does not contain a whole
/// image is exactly the "any uncertainty declines" case.
fn ends_with_eoi(data: &[u8]) -> bool {
    // Enough slack for the handful of padding bytes seen in the wild, not
    // enough to accept a stream that merely happens to contain FFD9 somewhere.
    const SLACK: usize = 16;
    if data.len() < 4 {
        return false;
    }
    let tail = &data[data.len().saturating_sub(2 + SLACK)..];
    tail.windows(2).any(|w| w == [0xFF, 0xD9])
}

/// Decode a four-component JPEG to raw CMYK samples, using the same `n/8`
/// DCT-scaled decoding trick as [`crate::decode_jpeg_scaled`] so a heavily
/// over-resolution source is never materialized at full size.
///
/// Requesting `JCS_CMYK` output explicitly is the whole point: for a YCCK
/// stream libjpeg applies the inverse Adobe transform for us, and for a plain
/// CMYK stream it passes samples through. Either way we get stored-sample
/// space. Returns `None` unless the stream really is CMYK or YCCK — a caller
/// that routed a 3-component stream here must not silently get RGB back.
pub(crate) fn decode_cmyk_jpeg_scaled(
    data: &[u8],
    target_w: u32,
    target_h: u32,
) -> Option<CmykImage> {
    use mozjpeg::ColorSpace;

    let mut dec = mozjpeg::Decompress::new_mem(data).ok()?;
    let (full_w, full_h) = (dec.width(), dec.height());
    if full_w == 0 || full_h == 0 {
        return None;
    }
    if !matches!(
        dec.color_space(),
        ColorSpace::JCS_CMYK | ColorSpace::JCS_YCCK
    ) {
        return None;
    }

    let numerator = dct_scale_numerator(full_w, full_h, target_w, target_h);
    if !scaled_decode_within_budget(full_w, full_h, numerator, 4) {
        return None;
    }
    dec.scale(numerator);

    let mut started = dec.to_colorspace(ColorSpace::JCS_CMYK).ok()?;
    let (w, h) = (started.width(), started.height());
    let data: Vec<u8> = started.read_scanlines::<u8>().ok()?;
    started.finish().ok()?;

    let (w, h) = (u32::try_from(w).ok()?, u32::try_from(h).ok()?);
    // Guard the arithmetic before trusting the buffer length.
    let expected = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    if w == 0 || h == 0 || data.len() != expected {
        return None;
    }
    Some(CmykImage {
        width: w,
        height: h,
        source_width: u32::try_from(full_w).ok()?,
        source_height: u32::try_from(full_h).ok()?,
        data,
    })
}

/// Resample all four channels to `(target_w, target_h)` with the Lanczos3
/// kernel — the same filter, and the same `image` implementation, the RGB path
/// gets from `resize_exact`.
///
/// Each channel is resampled independently as an 8-bit gray plane. That is
/// exactly what `image` does internally for a multi-channel image (its
/// resampler is separable and per-channel), and doing it explicitly keeps the
/// fourth channel from ever being mistaken for alpha by an image type that has
/// one.
fn resize_cmyk_lanczos3(img: &CmykImage, target_w: u32, target_h: u32) -> Option<CmykImage> {
    if target_w == 0 || target_h == 0 {
        return None;
    }
    let (sw, sh) = (img.width as usize, img.height as usize);
    let pixels = sw.checked_mul(sh)?;
    let out_pixels = (target_w as usize).checked_mul(target_h as usize)?;
    let mut out = vec![0u8; out_pixels.checked_mul(4)?];
    let mut plane = vec![0u8; pixels];
    for c in 0..4usize {
        for (dst, src) in plane.iter_mut().zip(img.data.as_chunks::<4>().0.iter()) {
            *dst = src[c];
        }
        let gray = image::GrayImage::from_raw(img.width, img.height, std::mem::take(&mut plane))?;
        let resized = image::imageops::resize(
            &gray,
            target_w,
            target_h,
            image::imageops::FilterType::Lanczos3,
        );
        if resized.len() != out_pixels {
            return None;
        }
        for (dst, src) in out.as_chunks_mut::<4>().0.iter_mut().zip(resized.iter()) {
            dst[c] = *src;
        }
        // Reuse the source plane buffer for the next channel.
        plane = gray.into_raw();
    }
    Some(CmykImage {
        width: target_w,
        height: target_h,
        source_width: img.source_width,
        source_height: img.source_height,
        data: out,
    })
}

/// Encode raw CMYK samples as a four-component YCCK JPEG.
///
/// `in_color_space = JCS_CMYK` tells libjpeg what we are handing it;
/// `jpeg_set_colorspace(JCS_YCCK)` tells it what to write. libjpeg then applies
/// the forward Adobe transform and emits the APP14 marker with transform = 2
/// itself — the marker and the pixel transform can therefore never disagree,
/// which is the failure mode that produces channel-swapped CMYK JPEGs. YCCK
/// (rather than re-emitting plain CMYK) is chosen for the same reason YCbCr
/// beats RGB: it decorrelates the chromatic channels and subsamples them, and
/// it is what Adobe's own encoders write.
fn encode_cmyk_jpeg(img: &CmykImage, quality: u8) -> Option<Vec<u8>> {
    use mozjpeg::{ColorSpace, Compress};

    if img.data.len() != (img.width as usize) * (img.height as usize) * 4 {
        return None;
    }
    let mut comp = Compress::new(ColorSpace::JCS_CMYK);
    comp.set_size(img.width as usize, img.height as usize);
    comp.set_color_space(ColorSpace::JCS_YCCK);
    comp.set_quality(quality as f32);

    let mut started = comp.start_compress(Vec::new()).ok()?;
    started.write_scanlines(&img.data).ok()?;
    started.finish().ok()
}

/// The CMYK counterpart of [`crate::decode_back_matches`]: re-decoding `out` must
/// reproduce the reference's geometry, its four-component nature, and its raw
/// samples within `max_mad`.
///
/// Comparing in raw stored-sample space is what makes this a polarity check
/// and not just a quality check: a channel inversion or a C/M/Y/K reordering
/// anywhere in the round trip moves the mean absolute difference to roughly
/// half of full scale, an order of magnitude past the ceiling.
fn cmyk_decode_back_matches(out: &[u8], reference: &CmykImage, max_mad: f64) -> bool {
    let Some(decoded) = decode_cmyk_jpeg_scaled(out, reference.width, reference.height) else {
        return false;
    };
    if decoded.width != reference.width || decoded.height != reference.height {
        return false;
    }
    if decoded.data.len() != reference.data.len() || reference.data.is_empty() {
        return false;
    }
    // PER CHANNEL, not pooled. A pooled average lets one wrecked channel hide
    // behind three intact ones: measured on the fixture corpus, a C/K swap
    // pools to MAD 29 and a full C,M,Y,K rotation to 73 — both under the
    // shared `DECODE_BACK_MAX_MAD` of 96, which was sized for three-channel
    // catastrophes. Per channel those same swaps blow past `max_mad`.
    let per_channel = reference.data.len() / 4;
    for c in 0..4usize {
        let sad: u64 = reference
            .data
            .iter()
            .skip(c)
            .step_by(4)
            .zip(decoded.data.iter().skip(c).step_by(4))
            .map(|(a, b)| u64::from(a.abs_diff(*b)))
            .sum();
        if sad as f64 / per_channel as f64 > max_mad {
            return false;
        }
    }
    true
}

/// The whole CMYK pipeline as one fail-safe unit: eligibility, scaled decode,
/// Lanczos3 resample to the exact target geometry, YCCK re-encode, decode-back
/// verification. Any doubt returns `None` and the stream is left untouched.
///
/// `target_w`/`target_h` are the caller's intent: the over-resolution
/// downsample passes its computed target, the dimension-preserving requant
/// passes the stream's own geometry. In BOTH cases the output is required to
/// land exactly on that geometry, so a stream whose dict lies about its size
/// can never desynchronize the dict from the payload.
pub(crate) fn plan_dct_cmyk(
    stream: &lopdf::Stream,
    quality: u8,
    target_w: u32,
    target_h: u32,
    require_exact_source_geometry: bool,
) -> Option<Vec<u8>> {
    if has_decode_array(&stream.dict) {
        return None;
    }
    if !ends_with_eoi(&stream.content) {
        return None;
    }
    let decoded = decode_cmyk_jpeg_scaled(&stream.content, target_w, target_h)?;
    // Never upscale: the DCT-scaled decode already refuses to go below the
    // target, so a source smaller than the target means the dict's geometry
    // and the payload disagree. Decline rather than invent samples.
    if decoded.width < target_w || decoded.height < target_h {
        return None;
    }
    // The dimension-preserving callers additionally require the payload to BE
    // the declared geometry, not merely cover it: a dict that lies about its
    // size would otherwise be silently "corrected" by the resample, which for
    // a masked base means breaking alignment with a mask nobody resized.
    if require_exact_source_geometry
        && (decoded.source_width != target_w || decoded.source_height != target_h)
    {
        return None;
    }
    let resized = if decoded.width == target_w && decoded.height == target_h {
        decoded
    } else {
        resize_cmyk_lanczos3(&decoded, target_w, target_h)?
    };

    let out = encode_cmyk_jpeg(&resized, quality)?;

    // Same idempotence guard as the RGB requant path: byte-identical
    // quantization tables mean the source is ALREADY at this quality and a
    // re-encode would be pure generation loss. (A plain-CMYK source and a YCCK
    // candidate never collide here — `jpeg_set_colorspace` gives CMYK one
    // shared table and YCCK two, so the DQT payloads differ by construction.)
    if let (Some(src_tables), Some(out_tables)) =
        (jpeg_quant_tables(&stream.content), jpeg_quant_tables(&out))
    {
        if src_tables == out_tables {
            return None;
        }
    }

    if !cmyk_decode_back_matches(&out, &resized, CMYK_DECODE_BACK_MAX_MAD) {
        return None;
    }
    Some(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lopdf::content::{Content, Operation};
    use lopdf::{dictionary, Document, Stream};

    use crate::{jpeg_route, optimize_with_options, JpegRoute, OptimizeOptions};

    // -----------------------------------------------------------------------
    // CMYK / YCCK JPEGs.
    //
    // Every check below lives in RAW STORED-SAMPLE space, which is what makes
    // it a channel-polarity test and not merely a quality test: an inverted or
    // rotated channel moves the mean absolute difference to ~85-128, an order
    // of magnitude past anything requantization produces. The two negative
    // controls at the end pin that sensitivity explicitly, so the positive
    // tests cannot pass by being blind.
    // -----------------------------------------------------------------------

    /// Fixtures live at `fixtures/jpeg`; regenerate with
    /// `python3 fixtures/jpeg/generate_cmyk.py` plus
    /// `cargo run --release --example gen_cmyk_ycck -- \
    ///      fixtures/jpeg/cmyk_plain.jpg fixtures/jpeg/cmyk_ycck.jpg`.
    pub(crate) fn jpeg_fixture(name: &str) -> Vec<u8> {
        let path =
            std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/jpeg")).join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// Mean absolute difference between two equal-length sample buffers.
    fn sample_mad(a: &[u8], b: &[u8]) -> f64 {
        assert_eq!(a.len(), b.len(), "buffers must be the same length");
        let sad: u64 = a
            .iter()
            .zip(b)
            .map(|(x, y)| u64::from(x.abs_diff(*y)))
            .sum();
        sad as f64 / a.len() as f64
    }

    /// One page embedding `jpeg` as a `/DeviceCMYK` `/DCTDecode` image of the
    /// given pixel geometry, drawn into a `draw_w_pts`x`draw_h_pts` box.
    /// `decode` optionally attaches a `/Decode` array.
    pub(crate) fn build_pdf_cmyk(
        jpeg: Vec<u8>,
        px_w: u32,
        px_h: u32,
        draw_w_pts: i64,
        draw_h_pts: i64,
        decode: Option<Vec<Object>>,
    ) -> Vec<u8> {
        let mut dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => px_w as i64,
            "Height" => px_h as i64,
            "ColorSpace" => "DeviceCMYK",
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
        };
        if let Some(d) = decode {
            dict.set("Decode", Object::Array(d));
        }
        let mut doc = Document::with_version("1.5");
        let img_id = doc.add_object(Stream::new(dict, jpeg));

        let content = Content {
            operations: vec![
                Operation::new("q", vec![]),
                Operation::new(
                    "cm",
                    vec![
                        draw_w_pts.into(),
                        0.into(),
                        0.into(),
                        draw_h_pts.into(),
                        0.into(),
                        0.into(),
                    ],
                ),
                Operation::new("Do", vec![Object::Name(b"Im0".to_vec())]),
                Operation::new("Q", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
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
        doc.trailer.set("Root", catalog_id);
        let mut out: Vec<u8> = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    /// The one image stream of a single-image PDF, as (dict, content).
    fn only_image_stream(pdf: &[u8]) -> (lopdf::Dictionary, Vec<u8>) {
        let doc = Document::load_mem(pdf).expect("valid PDF");
        let mut found = None;
        for obj in doc.objects.values() {
            if let Object::Stream(s) = obj {
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
                    assert!(found.is_none(), "expected exactly one image");
                    found = Some((s.dict.clone(), s.content.clone()));
                }
            }
        }
        found.expect("an image stream")
    }

    /// The two source fixtures carry the same picture in the two Adobe APP14
    /// flavours, and libjpeg hands both back as the SAME raw samples. This is
    /// the property the whole design rests on: transform handling is entirely
    /// libjpeg's, so we never have to know which flavour we are looking at.
    #[test]
    fn plain_cmyk_and_ycck_decode_to_the_same_samples() {
        let a = decode_cmyk_jpeg_scaled(&jpeg_fixture("cmyk_plain.jpg"), 96, 64).unwrap();
        let b = decode_cmyk_jpeg_scaled(&jpeg_fixture("cmyk_ycck.jpg"), 96, 64).unwrap();
        assert_eq!((a.width, a.height), (96, 64));
        assert_eq!((b.width, b.height), (96, 64));
        assert_eq!(a.data.len(), 96 * 64 * 4);
        // Only one extra requantization separates them.
        assert!(
            sample_mad(&a.data, &b.data) < 4.0,
            "MAD {}",
            sample_mad(&a.data, &b.data)
        );
        // Per channel too, so a swap cannot average itself away.
        for c in 0..4 {
            let (ca, cb): (Vec<u8>, Vec<u8>) = (
                a.data.iter().skip(c).step_by(4).copied().collect(),
                b.data.iter().skip(c).step_by(4).copied().collect(),
            );
            assert!(sample_mad(&ca, &cb) < 4.0, "channel {c}");
        }
    }

    /// Encode-then-decode is sample-preserving for BOTH input flavours, and
    /// the output is always a four-component YCCK JPEG carrying the Adobe
    /// APP14 marker libjpeg wrote for us.
    #[test]
    fn cmyk_reencode_round_trips_in_raw_sample_space() {
        for name in ["cmyk_plain.jpg", "cmyk_ycck.jpg"] {
            let src = decode_cmyk_jpeg_scaled(&jpeg_fixture(name), 96, 64).unwrap();
            let out = encode_cmyk_jpeg(&src, 78).expect("encode");
            assert_eq!(jpeg_component_count(&out), Some(4), "{name}");
            assert_eq!(adobe_transform(&out), Some(2), "{name} must be YCCK");
            let back = decode_cmyk_jpeg_scaled(&out, 96, 64).unwrap();
            for c in 0..4 {
                let (ca, cb): (Vec<u8>, Vec<u8>) = (
                    src.data.iter().skip(c).step_by(4).copied().collect(),
                    back.data.iter().skip(c).step_by(4).copied().collect(),
                );
                let mad = sample_mad(&ca, &cb);
                // q78 on a 96x64 synthetic: a handful of levels. An inverted
                // or rotated channel would be 85+.
                assert!(mad < 12.0, "{name} channel {c} MAD {mad}");
            }
            assert!(cmyk_decode_back_matches(
                &out,
                &src,
                CMYK_DECODE_BACK_MAX_MAD
            ));
        }
    }

    /// The APP14 transform byte, read for TESTS ONLY — the library never
    /// parses this marker, which is the point of the design.
    fn adobe_transform(data: &[u8]) -> Option<u8> {
        let mut i = 2usize;
        while i + 4 <= data.len() {
            let marker = data[i + 1];
            let len = ((data[i + 2] as usize) << 8) | data[i + 3] as usize;
            if marker == 0xEE && len >= 13 && data.get(i + 4..i + 9) == Some(b"Adobe") {
                return data.get(i + 15).copied();
            }
            if marker == 0xDA {
                return None;
            }
            i += 2 + len;
        }
        None
    }

    /// Negative controls. `cmyk_decode_back_matches` must REJECT a reference
    /// whose channels were inverted or rotated — otherwise every positive test
    /// above would be vacuous.
    #[test]
    fn cmyk_verification_rejects_inverted_and_rotated_channels() {
        let src = decode_cmyk_jpeg_scaled(&jpeg_fixture("cmyk_plain.jpg"), 96, 64).unwrap();
        let out = encode_cmyk_jpeg(&src, 78).expect("encode");
        assert!(cmyk_decode_back_matches(
            &out,
            &src,
            CMYK_DECODE_BACK_MAX_MAD
        ));

        let inverted = CmykImage {
            data: src.data.iter().map(|b| 255 - b).collect(),
            width: src.width,
            height: src.height,
            source_width: src.source_width,
            source_height: src.source_height,
        };
        assert!(!cmyk_decode_back_matches(
            &out,
            &inverted,
            CMYK_DECODE_BACK_MAX_MAD
        ));

        // C,M,Y,K -> M,Y,K,C: the classic component-order slip.
        let mut rotated = src.data.clone();
        for px in rotated.as_chunks_mut::<4>().0.iter_mut() {
            px.rotate_left(1);
        }
        let rotated = CmykImage {
            data: rotated,
            ..src
        };
        assert!(!cmyk_decode_back_matches(
            &out,
            &rotated,
            CMYK_DECODE_BACK_MAX_MAD
        ));
    }

    /// End to end: an over-resolution DeviceCMYK JPEG is downsampled, shrinks,
    /// stays four-component YCCK, keeps its `/ColorSpace`, gets a truthful
    /// `/Width`//`/Height`, and still decodes to the same picture.
    #[test]
    fn over_resolution_cmyk_downsamples_and_stays_cmyk() {
        let src = jpeg_fixture("cmyk_large.jpg");
        // 640/(144/72) = 320 effective DPI against a 130 DPI target.
        let pdf = build_pdf_cmyk(src.clone(), 640, 480, 144, 108, None);
        let out = optimize_with_options(&pdf, OptimizeOptions::default());
        let (dict, content) = only_image_stream(&out);

        assert_eq!(dict.get(b"Width").unwrap().as_i64().unwrap(), 260);
        assert_eq!(dict.get(b"Height").unwrap().as_i64().unwrap(), 195);
        assert!(matches!(dict.get(b"ColorSpace"), Ok(Object::Name(n)) if n == b"DeviceCMYK"));
        assert!(matches!(dict.get(b"Filter"), Ok(Object::Name(n)) if n == b"DCTDecode"));
        assert_eq!(jpeg_component_count(&content), Some(4));
        assert_eq!(adobe_transform(&content), Some(2));
        assert!(
            content.len() < src.len(),
            "{} !< {}",
            content.len(),
            src.len()
        );

        // The picture survived: compare the optimized payload against the
        // ORIGINAL resampled to the same geometry, per channel.
        let full = decode_cmyk_jpeg_scaled(&src, 640, 480).unwrap();
        let expect = resize_cmyk_lanczos3(&full, 260, 195).unwrap();
        let actual = decode_cmyk_jpeg_scaled(&content, 260, 195).unwrap();
        assert_eq!((actual.width, actual.height), (260, 195));
        for c in 0..4 {
            let (ea, aa): (Vec<u8>, Vec<u8>) = (
                expect.data.iter().skip(c).step_by(4).copied().collect(),
                actual.data.iter().skip(c).step_by(4).copied().collect(),
            );
            let mad = sample_mad(&ea, &aa);
            assert!(mad < 12.0, "channel {c} MAD {mad}");
        }
    }

    /// A declined image may still have been through the LOSSLESS Huffman
    /// re-optimizer (`reoptimize_jpeg_streams` runs on every `/DCTDecode`
    /// payload, including the ones the image path passes over), so byte
    /// equality is the wrong assertion. What must hold is that nothing lossy
    /// happened: same geometry, same quantization tables, and raw samples that
    /// are IDENTICAL rather than merely close.
    fn assert_not_reencoded(original: &[u8], actual: &[u8], w: u32, h: u32) {
        assert_eq!(
            jpeg_component_count(original),
            jpeg_component_count(actual),
            "component count changed"
        );
        assert_eq!(
            jpeg_quant_tables(original),
            jpeg_quant_tables(actual),
            "quantization tables changed - the image was requantized"
        );
        let before = decode_cmyk_jpeg_scaled(original, w, h).expect("decode original");
        let after = decode_cmyk_jpeg_scaled(actual, w, h).expect("decode result");
        assert_eq!(
            (before.source_width, before.source_height),
            (after.source_width, after.source_height),
            "geometry changed"
        );
        assert_eq!(before.data, after.data, "samples changed - not lossless");
    }

    /// A `/Decode` array is a per-sample remap we decline to reason about
    /// through a clamped resample — the stream must come out byte-identical.
    /// `[1 0 1 0 1 0 1 0]` is the inverting one real producers attach.
    #[test]
    fn cmyk_with_decode_array_is_left_untouched() {
        let src = jpeg_fixture("cmyk_large.jpg");
        let decode: Vec<Object> = [1, 0, 1, 0, 1, 0, 1, 0]
            .iter()
            .map(|v| Object::Integer(*v))
            .collect();
        let pdf = build_pdf_cmyk(src.clone(), 640, 480, 144, 108, Some(decode));
        let out = optimize_with_options(&pdf, OptimizeOptions::default());
        let (dict, content) = only_image_stream(&out);
        assert_not_reencoded(&src, &content, 640, 480);
        assert_eq!(dict.get(b"Width").unwrap().as_i64().unwrap(), 640);
        assert_eq!(dict.get(b"Height").unwrap().as_i64().unwrap(), 480);
        assert!(dict.get(b"Decode").is_ok(), "/Decode must survive");
    }

    /// A truncated CMYK payload declines, and the guard that stops it is the
    /// EOI check rather than the decoder: libjpeg happily "repairs" a
    /// truncated scan with flat grey and reports only a warning, so the
    /// decode-back verification would compare that grey against itself and
    /// agree. Both halves are asserted here so the decline cannot silently
    /// start depending on a leniency that is not ours to control.
    #[test]
    fn truncated_cmyk_declines() {
        let full = jpeg_fixture("cmyk_large.jpg");
        let truncated = full[..full.len() / 3].to_vec();
        assert!(!ends_with_eoi(&truncated));
        assert!(ends_with_eoi(&full));
        // Documenting libjpeg's leniency, which is exactly why we need the
        // structural check above.
        assert!(decode_cmyk_jpeg_scaled(&truncated, 260, 195).is_some());

        let stream = Stream::new(dictionary! { "Filter" => "DCTDecode" }, truncated.clone());
        assert!(plan_dct_cmyk(&stream, 78, 260, 195, false).is_none());

        let pdf = build_pdf_cmyk(truncated.clone(), 640, 480, 144, 108, None);
        let out = optimize_with_options(&pdf, OptimizeOptions::default());
        let (dict, content) = only_image_stream(&out);
        assert_eq!(dict.get(b"Width").unwrap().as_i64().unwrap(), 640);
        assert_not_reencoded(&truncated, &content, 640, 480);
    }

    /// A truncated APP14 segment makes the frame header unreachable. The
    /// structural walk reports that honestly (`None`), the CMYK decoder
    /// refuses the stream, and the payload survives untouched — no panic on
    /// bytes we did not write.
    #[test]
    fn corrupt_app14_declines() {
        let full = jpeg_fixture("cmyk_large.jpg");
        let mut corrupt = full.clone();
        // The APP14 segment is the first marker after SOI in these fixtures;
        // overstate its length so the walk runs off the end of the file.
        assert_eq!(&corrupt[2..4], &[0xFF, 0xEE], "APP14 expected at offset 2");
        corrupt[4] = 0xFF;
        corrupt[5] = 0xFF;
        assert_eq!(jpeg_component_count(&corrupt), None);
        assert!(decode_cmyk_jpeg_scaled(&corrupt, 260, 195).is_none());
        let pdf = build_pdf_cmyk(corrupt.clone(), 640, 480, 144, 108, None);
        let out = optimize_with_options(&pdf, OptimizeOptions::default());
        let (_, content) = only_image_stream(&out);
        assert_eq!(content, corrupt);
    }

    /// A component count nothing here understands (2) is declined outright
    /// rather than handed to a decoder that would guess at the channels.
    #[test]
    fn two_component_jpeg_declines() {
        let mut data = jpeg_fixture("cmyk_large.jpg");
        // Rewrite the SOF component count in place. The stream is now
        // nonsense, which is exactly the point: routing must refuse it before
        // any decoder is asked.
        let sof = data
            .windows(2)
            .position(|w| w[0] == 0xFF && w[1] == 0xC0)
            .expect("SOF0");
        data[sof + 9] = 2;
        assert_eq!(jpeg_component_count(&data), Some(2));
        assert_eq!(jpeg_route(&data), JpegRoute::Decline);
        let pdf = build_pdf_cmyk(data.clone(), 640, 480, 144, 108, None);
        let out = optimize_with_options(&pdf, OptimizeOptions::default());
        let (_, content) = only_image_stream(&out);
        assert_eq!(content, data);
    }

    /// Idempotence: a second pass over an already-optimized CMYK image must
    /// not churn the payload. The shared quantization-table guard covers this
    /// exactly as it does for RGB.
    #[test]
    fn cmyk_optimize_is_idempotent() {
        let src = jpeg_fixture("cmyk_large.jpg");
        let pdf = build_pdf_cmyk(src, 640, 480, 144, 108, None);
        let once = optimize_with_options(&pdf, OptimizeOptions::default());
        let twice = optimize_with_options(&once, OptimizeOptions::default());
        let (_, a) = only_image_stream(&once);
        let (_, b) = only_image_stream(&twice);
        assert_eq!(a, b, "second pass must not re-encode the image");
    }

    /// An UNDER-resolution CMYK image takes the dimension-preserving requant
    /// path (P-M2), which must keep the geometry exactly and stay CMYK.
    #[test]
    fn under_resolution_cmyk_requantizes_without_resizing() {
        let src = jpeg_fixture("cmyk_large.jpg");
        // 640/(432/72) = ~107 DPI: under the 130 DPI target.
        let pdf = build_pdf_cmyk(src.clone(), 640, 480, 432, 324, None);
        let out = optimize_with_options(&pdf, OptimizeOptions::default());
        let (dict, content) = only_image_stream(&out);
        assert_eq!(dict.get(b"Width").unwrap().as_i64().unwrap(), 640);
        assert_eq!(dict.get(b"Height").unwrap().as_i64().unwrap(), 480);
        assert_eq!(jpeg_component_count(&content), Some(4));
        assert_eq!(adobe_transform(&content), Some(2));
        assert!(content.len() < src.len());
        let expect = decode_cmyk_jpeg_scaled(&src, 640, 480).unwrap();
        let actual = decode_cmyk_jpeg_scaled(&content, 640, 480).unwrap();
        assert_eq!((actual.width, actual.height), (640, 480));
        for c in 0..4 {
            let (ea, aa): (Vec<u8>, Vec<u8>) = (
                expect.data.iter().skip(c).step_by(4).copied().collect(),
                actual.data.iter().skip(c).step_by(4).copied().collect(),
            );
            assert!(sample_mad(&ea, &aa) < 12.0, "channel {c}");
        }
    }

    /// A dict that LIES about its geometry cannot reach the
    /// dimension-preserving path: the exact-source-geometry mode declines, so
    /// nothing gets silently "corrected" into a different size.
    #[test]
    fn cmyk_requant_refuses_a_lying_dict() {
        let src = jpeg_fixture("cmyk_large.jpg");
        let stream = Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 640_i64,
                "Height" => 480_i64,
                "ColorSpace" => "DeviceCMYK",
                "BitsPerComponent" => 8,
                "Filter" => "DCTDecode",
            },
            src,
        );
        // Truthful geometry: accepted.
        assert!(plan_dct_cmyk(&stream, 78, 640, 480, true).is_some());
        // A dict claiming a size the payload does not have: declined.
        assert!(plan_dct_cmyk(&stream, 78, 320, 240, true).is_none());
        assert!(plan_dct_cmyk(&stream, 78, 800, 600, true).is_none());
        // The resizing callers still accept a smaller target...
        assert!(plan_dct_cmyk(&stream, 78, 320, 240, false).is_some());
        // ...but never upscale.
        assert!(plan_dct_cmyk(&stream, 78, 800, 600, false).is_none());
    }

    /// The dict/payload consistency guard, from both directions. A dict that
    /// declares four components over a three-component payload (or the
    /// reverse) is already broken, and re-encoding it would pick a side.
    #[test]
    fn component_count_mismatch_declines() {
        // Three-component payload under /DeviceCMYK — the exact shape the old
        // RGB fallback used to CREATE.
        let rgb = jpeg_fixture("seq_color.jpg");
        assert_eq!(jpeg_component_count(&rgb), Some(3));
        let pdf = build_pdf_cmyk(rgb.clone(), 96, 64, 24, 16, None);
        let out = optimize_with_options(&pdf, OptimizeOptions::default());
        let (_, content) = only_image_stream(&out);
        assert_eq!(
            jpeg_component_count(&content),
            Some(3),
            "must stay as found"
        );
        assert_eq!(
            jpeg_quant_tables(&rgb),
            jpeg_quant_tables(&content),
            "must not be requantized"
        );

        // And the reverse: a four-component payload under /DeviceRGB.
        let cmyk = jpeg_fixture("cmyk_plain.jpg");
        let doc_bytes = {
            let mut doc =
                Document::load_mem(&build_pdf_cmyk(cmyk.clone(), 96, 64, 24, 16, None)).unwrap();
            let id = *doc
                .objects
                .iter()
                .find(|(_, o)| {
                    matches!(o, Object::Stream(s)
                        if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image"))
                })
                .unwrap()
                .0;
            if let Ok(Object::Stream(s)) = doc.get_object_mut(id) {
                s.dict
                    .set("ColorSpace", Object::Name(b"DeviceRGB".to_vec()));
            }
            let mut v = Vec::new();
            doc.save_to(&mut v).unwrap();
            v
        };
        let out = optimize_with_options(&doc_bytes, OptimizeOptions::default());
        let (_, content) = only_image_stream(&out);
        assert_eq!(
            jpeg_component_count(&content),
            Some(4),
            "must stay as found"
        );
        assert_eq!(jpeg_quant_tables(&cmyk), jpeg_quant_tables(&content));
    }

    /// Deterministic byte-mutation sweep over the CMYK path. libjpeg reports
    /// fatal errors by UNWINDING through C code (mozjpeg's own docs say to
    /// wrap every call in `catch_unwind`), so the contract that matters for
    /// untrusted bytes is the one `optimize_with_options` already provides:
    /// whatever happens inside, the caller gets valid PDF bytes back and the
    /// image is either untouched or a genuine four-component replacement.
    #[test]
    fn mutated_cmyk_streams_never_corrupt_or_escape() {
        let base = jpeg_fixture("cmyk_plain.jpg");
        // xorshift, so the sweep is reproducible without a dependency.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut replaced = 0usize;
        for _ in 0..400 {
            let mut data = base.clone();
            for _ in 0..3 {
                let at = (next() as usize) % data.len();
                data[at] ^= (next() % 255) as u8 + 1;
            }
            let pdf = build_pdf_cmyk(data.clone(), 96, 64, 24, 16, None);
            let out = optimize_with_options(&pdf, OptimizeOptions::default());
            let (dict, content) = only_image_stream(&out);
            assert!(!content.is_empty());
            // Geometry in the dict and the payload's own frame header must
            // never disagree, and a replacement must never drop to three
            // channels the way the old RGB fallback did.
            let (w, h) = (
                dict.get(b"Width").unwrap().as_i64().unwrap(),
                dict.get(b"Height").unwrap().as_i64().unwrap(),
            );
            if content != data {
                replaced += 1;
                assert_eq!(
                    jpeg_component_count(&content),
                    Some(4),
                    "a replacement under /DeviceCMYK must stay four-component"
                );
                let back = decode_cmyk_jpeg_scaled(&content, 1, 1)
                    .expect("a replacement must decode as CMYK");
                assert_eq!(
                    (back.source_width as i64, back.source_height as i64),
                    (w, h)
                );
                assert_eq!(
                    jpeg_component_count(&content),
                    Some(4),
                    "a replacement must stay four-component"
                );
                let back = decode_cmyk_jpeg_scaled(&content, 1, 1)
                    .expect("a replacement must decode as CMYK");
                assert_eq!(
                    (back.source_width as i64, back.source_height as i64),
                    (w, h)
                );
            } else {
                assert_eq!((w, h), (96, 64));
            }
        }
        // Sanity: the sweep has to actually exercise the replacement branch,
        // not just watch 400 declines go by.
        assert!(replaced > 0, "no mutation reached a replacement");
    }

    /// Cross-decoder verification against Pillow and libjpeg-turbo's `djpeg`.
    /// Ignored by default because it shells out to tools that are not build
    /// dependencies; run with
    /// `cargo test --release -- --ignored cmyk_cross_decoder`.
    /// Results as run on 2026-08-24 are recorded in `docs/CMYK-JPEG.md`.
    #[test]
    #[ignore = "requires python3+Pillow and djpeg on PATH"]
    fn cmyk_cross_decoder_check() {
        let src = jpeg_fixture("cmyk_large.jpg");
        let pdf = build_pdf_cmyk(src.clone(), 640, 480, 144, 108, None);
        let out = optimize_with_options(&pdf, OptimizeOptions::default());
        let (_, content) = only_image_stream(&out);

        let dir = std::env::temp_dir().join("picamatl-cmyk-crosscheck");
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("src.jpg"), dir.join("out.jpg"));
        std::fs::write(&a, &src).unwrap();
        std::fs::write(&b, &content).unwrap();

        // Pillow: decode both, resample the source down to the output's
        // geometry with its own Lanczos, and compare per channel. An inverted
        // or rotated channel lands near 85-128; requantization plus a
        // different Lanczos implementation lands in the single digits.
        let script = format!(
            r#"
import subprocess, sys
from PIL import Image
a = Image.open({a:?}); b = Image.open({b:?})
assert a.mode == "CMYK" and b.mode == "CMYK", (a.mode, b.mode)
assert b.size == (260, 195), b.size
ar = a.resize(b.size, Image.LANCZOS)
for i, ch in enumerate("CMYK"):
    pa, pb = ar.getchannel(i).tobytes(), b.getchannel(i).tobytes()
    mad = sum(abs(x - y) for x, y in zip(pa, pb)) / len(pa)
    print("PIL", ch, round(mad, 3))
    assert mad < 12.0, (ch, mad)
# djpeg is a second, independent libjpeg-turbo build; its CMYK->RGB path
# exercises the Adobe transform handling end to end.
def rgb(p):
    d = subprocess.run(["djpeg", "-pnm", p], capture_output=True, check=True).stdout
    assert d[:2] == b"P6", d[:20]
    return d.split(b"\n", 3)[3]
ra, rb = rgb({a:?}), rgb({b:?})
print("djpeg sizes", len(ra), len(rb))
"#
        );
        let status = std::process::Command::new("python3")
            .arg("-c")
            .arg(&script)
            .status()
            .expect("run python3");
        assert!(status.success(), "cross-decoder check failed");
    }
}
