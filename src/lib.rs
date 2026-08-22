//! **amatl** — pure-Rust PDF size optimization.
//!
//! Named for the Nahuatl word for the fig-bark paper used in pre-Columbian
//! Mesoamerican codices. Amatl shrinks PDFs by downsampling over-resolution
//! embedded images to the resolution they are actually rendered at.
//!
//! For the dominant input shape this targets — business documents (flyers,
//! catalogs, decks, reports) exported from office suites, where embedded JPEG
//! product photos are ~80% of file bytes — a measured sweet spot of ~130 DPI /
//! JPEG quality 78 yields ~40-60% smaller files with no perceptible quality
//! loss at the displayed size. Image bytes are matched within ~0.01% of
//! Ghostscript's output via mozjpeg (optimized Huffman + trellis quantization).
//!
//! Strategy (lossy only on over-resolution images, never on text/vectors):
//!   1. Walk each page's content stream, tracking the CTM, to compute the
//!      on-page rendered size (in points) of every painted image XObject.
//!   2. For DCTDecode (JPEG) images whose *effective* DPI exceeds the target,
//!      decode, resize to the target pixel dimensions, and re-encode as JPEG.
//!      For FlateDecode raster images (screenshots, exported bitmaps), decode,
//!      resize, and re-deflate — same format, `/ColorSpace` untouched, so no
//!      JPEG artifacts are ever introduced on that content class.
//!   3. Replace the stream only when the result is actually smaller.
//!
//! Hard safety guarantees:
//!   - Images we can't measure a placement for are left untouched.
//!   - Images already at/below the target DPI are left untouched (no upscaling).
//!   - A re-encode that isn't smaller is discarded.
//!   - Any failure (parse, decode, save) falls back to the original bytes.

use std::collections::HashMap;

use image::{DynamicImage, ImageFormat};
use rayon::prelude::*;
use lopdf::content::Content;
use lopdf::{dictionary, Document, Object, ObjectId};

/// Target resolution for downsampled images, in dots per inch.
const TARGET_DPI: f32 = 130.0;
/// JPEG quality (0-100) for re-encoded images.
const JPEG_QUALITY: u8 = 78;
/// Only downsample when the effective DPI exceeds the target by this factor,
/// so we don't churn images that are already close to ideal.
const DPI_MARGIN: f32 = 1.15;

/// Options for [`optimize_with_options`]. Defaults preserve the input's
/// accessibility data and use the simpler (non-packed) save path.
///
/// As a library, amatl is accessibility-preserving by default. Callers who
/// know their audience (e.g. sighted-only retail promotions) can opt in to
/// `strip_accessibility` for ~18 percentage points of additional reduction.
/// Stripping removes the PDF structure tree (`/StructTreeRoot`, `/MarkInfo`,
/// `/Lang`), which screen readers use to navigate the document semantically.
/// Visually, the output is identical.
///
/// `pack_object_streams` controls whether eligible non-stream objects are
/// packed into PDF 1.5 `ObjStm` streams with a binary xref stream. Default
/// `false`. On image-dominated documents with few non-stream objects (e.g.
/// after a strip), this buys only a couple of percentage points because there
/// are few objects left to pack; for larger/denser documents it can buy
/// substantially more. Implemented in pure Rust (no native deps) to avoid
/// bundling an external tool such as qpdf.
///
/// # Example
///
/// ```
/// use amatl::OptimizeOptions;
/// let opts = OptimizeOptions::default()
///     .with_strip_accessibility(true)
///     .with_target_dpi(110.0);
/// ```
///
/// `#[non_exhaustive]`: construct via [`OptimizeOptions::default()`] plus the
/// `with_*` setters, never a struct literal. This lets future options ship in a
/// *minor* release instead of a breaking one once amatl is published.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct OptimizeOptions {
    /// Target resolution for downsampled images, in dots per inch. Images
    /// whose effective on-page DPI exceeds this (by `dpi_margin`) are
    /// downsampled to it. Values <= 0 disable downsampling entirely.
    /// Default: 130.0 (a measured visual-lossless sweet spot for business
    /// documents).
    pub target_dpi: f32,

    /// JPEG quality (1-100) for re-encoded images. Clamped to [1, 100] at
    /// use. Default: 78 (matches Ghostscript's image payload within ~0.01%).
    pub jpeg_quality: u8,

    /// Only downsample when the effective DPI exceeds `target_dpi` by this
    /// factor, so images already near the target are not churned. Clamped to
    /// a minimum of 1.0 at use. Default: 1.15.
    pub dpi_margin: f32,

    /// If true, remove the PDF's structure tree (accessibility metadata) for
    /// additional size reduction. Visually lossless; accessibility-lossy.
    /// Default: `false`.
    pub strip_accessibility: bool,

    /// If true, pack eligible non-stream objects into PDF 1.5 `ObjStm` streams
    /// with a binary cross-reference stream (additional structural
    /// compression). Default: `false`. See struct doc for the cost/benefit
    /// trade-off on different input shapes.
    pub pack_object_streams: bool,

    /// Downsample over-resolution FlateDecode raster images in place (format
    /// preserved: the result is still FlateDecode with the same `/ColorSpace`,
    /// so no JPEG artifacts are introduced). Applies the same effective-DPI
    /// threshold as the JPEG path. Default: `true`.
    pub downsample_flate_images: bool,
}

/// Written by hand, NOT derived: a derived `Default` would zero the numeric
/// fields, making `target_dpi` 0.0 and collapsing every image toward 1px. The
/// module consts are the single source of truth for the measured sweet spot.
/// `default_options_match_documented_sweet_spot` pins these values.
impl Default for OptimizeOptions {
    fn default() -> Self {
        Self {
            target_dpi: TARGET_DPI,
            jpeg_quality: JPEG_QUALITY,
            dpi_margin: DPI_MARGIN,
            strip_accessibility: false,
            pack_object_streams: false,
            downsample_flate_images: true,
        }
    }
}

/// Chainable setters. Because the struct is `#[non_exhaustive]`, these are the
/// only way an external crate can configure it — which is what keeps adding a
/// future option a non-breaking (minor) release. `Copy`, so taking `self` by
/// value is cheap.
impl OptimizeOptions {
    /// Set the target downsampling resolution in DPI. See the field docs.
    #[must_use]
    pub fn with_target_dpi(mut self, dpi: f32) -> Self {
        self.target_dpi = dpi;
        self
    }

    /// Set the JPEG quality (1-100) for re-encoded images.
    #[must_use]
    pub fn with_jpeg_quality(mut self, quality: u8) -> Self {
        self.jpeg_quality = quality;
        self
    }

    /// Set the over-resolution margin factor (minimum 1.0 at use).
    #[must_use]
    pub fn with_dpi_margin(mut self, margin: f32) -> Self {
        self.dpi_margin = margin;
        self
    }

    /// Enable/disable stripping the PDF structure tree (accessibility metadata).
    #[must_use]
    pub fn with_strip_accessibility(mut self, strip: bool) -> Self {
        self.strip_accessibility = strip;
        self
    }

    /// Enable/disable PDF 1.5 object-stream packing.
    #[must_use]
    pub fn with_pack_object_streams(mut self, pack: bool) -> Self {
        self.pack_object_streams = pack;
        self
    }

    /// Enable/disable in-place downsampling of over-resolution FlateDecode
    /// raster images (on by default).
    #[must_use]
    pub fn with_downsample_flate_images(mut self, downsample: bool) -> Self {
        self.downsample_flate_images = downsample;
        self
    }
}

/// Optimize a PDF with default options (accessibility data preserved), returning
/// smaller bytes when possible. On any failure or if the result is not smaller,
/// the original bytes are returned unchanged. Equivalent to
/// [`optimize_with_options`] with [`OptimizeOptions::default()`].
pub fn optimize(input: &[u8]) -> Vec<u8> {
    optimize_with_options(input, OptimizeOptions::default())
}

/// Optimize a PDF with the given options, returning smaller bytes when possible.
/// On any failure or if the result is not smaller, the original bytes are
/// returned unchanged.
///
/// # Fail-safe contract (invariant)
///
/// For any `input: &[u8]` — including malformed PDFs, truncated streams,
/// crafted attacker input, and empty slices — this function returns without
/// panicking. On any error, panic, or non-shrinking result, the returned
/// bytes equal `input`. Callers can treat the output as always valid and
/// always at most as large as the input.
///
/// This is enforced by a [`std::panic::catch_unwind`] boundary that turns any
/// panic in the JPEG decoder, mozjpeg encoder, or lopdf into the same graceful
/// fallback as a `Result::Err`. The regression tests
/// `crafted_pdf_panic_is_caught_not_unwound`, `degenerate_inputs_do_not_panic`,
/// and `invalid_pdf_falls_back_to_original` pin the three failure shapes
/// (panic, degenerate input, parse error); do not remove them.
pub fn optimize_with_options(input: &[u8], options: OptimizeOptions) -> Vec<u8> {
    // amatl optimizes arbitrary user-supplied PDFs, and its contract is to
    // return the original bytes on ANY failure. try_optimize handles the
    // expected error paths (Result::Err), but a crafted PDF could still trigger
    // a panic deep in the JPEG decoder, the mozjpeg encoder, or lopdf. Catch it
    // here so a panic becomes the same graceful fallback as any other failure.
    let result = std::panic::catch_unwind(|| try_optimize(input, options));
    match result {
        // A rewritten document that actually got smaller. Everything else —
        // Ok(None) (nothing to do), a lopdf error, or a caught panic — falls
        // through to returning the input unchanged.
        Ok(Ok(Some(out))) if out.len() < input.len() => out,
        _ => input.to_vec(),
    }
}

/// A 2x3 affine transform `[a b c d e f]`, the PDF current transformation
/// matrix. Maps (x, y) -> (a*x + c*y + e, b*x + d*y + f).
#[derive(Clone, Copy)]
struct Mat {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Mat {
    const IDENTITY: Mat = Mat { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    /// Concatenate: `self` applied first, then `then` (PDF `cm` semantics with
    /// `then` being the pre-existing CTM).
    fn concat(self, then: Mat) -> Mat {
        Mat {
            a: self.a * then.a + self.b * then.c,
            b: self.a * then.b + self.b * then.d,
            c: self.c * then.a + self.d * then.c,
            d: self.c * then.b + self.d * then.d,
            e: self.e * then.a + self.f * then.c + then.e,
            f: self.e * then.b + self.f * then.d + then.f,
        }
    }

    /// Length in points of the transformed unit-square width edge (1,0).
    fn rendered_width(&self) -> f32 {
        (self.a * self.a + self.b * self.b).sqrt()
    }

    /// Length in points of the transformed unit-square height edge (0,1).
    fn rendered_height(&self) -> f32 {
        (self.c * self.c + self.d * self.d).sqrt()
    }
}

fn num(obj: &Object) -> f32 {
    match obj {
        Object::Integer(i) => *i as f32,
        Object::Real(r) => *r,
        _ => 0.0,
    }
}

/// Follow reference chains to the concrete object (bounded to avoid cycles).
fn resolve<'a>(doc: &'a Document, mut obj: &'a Object) -> &'a Object {
    for _ in 0..8 {
        match obj {
            Object::Reference(id) => match doc.get_object(*id) {
                Ok(next) => obj = next,
                Err(_) => break,
            },
            _ => break,
        }
    }
    obj
}

/// Resolve a page's `Resources` dict, climbing the `Parent` chain since
/// resources can be inherited from the page tree.
fn page_resources(doc: &Document, page_id: ObjectId) -> Option<&lopdf::Dictionary> {
    let mut current = page_id;
    for _ in 0..32 {
        let dict = doc.get_object(current).ok()?.as_dict().ok()?;
        if let Ok(res) = dict.get(b"Resources") {
            return resolve(doc, res).as_dict().ok();
        }
        match dict.get(b"Parent") {
            Ok(Object::Reference(parent)) => current = *parent,
            _ => break,
        }
    }
    None
}

/// Map of image-XObject resource names to their object id for one page.
fn page_image_names(doc: &Document, page_id: ObjectId) -> HashMap<Vec<u8>, ObjectId> {
    let mut map = HashMap::new();
    let Some(resources) = page_resources(doc, page_id) else {
        return map;
    };
    let Ok(xobjects) = resources.get(b"XObject").map(|x| resolve(doc, x)) else {
        return map;
    };
    let Ok(xobjects) = xobjects.as_dict() else {
        return map;
    };
    for (name, value) in xobjects.iter() {
        if let Object::Reference(id) = value {
            map.insert(name.clone(), *id);
        }
    }
    map
}

/// Largest on-page rendered size (in points) for each image object id, across
/// every placement on every page. We size to the largest use so a shared image
/// is never under-resolved.
fn collect_placements(doc: &Document) -> HashMap<ObjectId, (f32, f32)> {
    let mut sizes: HashMap<ObjectId, (f32, f32)> = HashMap::new();

    for (_, page_id) in doc.get_pages() {
        let names = page_image_names(doc, page_id);
        if names.is_empty() {
            continue;
        }
        let Ok(content_bytes) = doc.get_page_content(page_id) else {
            continue;
        };
        let Ok(content) = Content::decode(&content_bytes) else {
            continue;
        };

        let mut ctm = Mat::IDENTITY;
        let mut stack: Vec<Mat> = Vec::new();

        for op in content.operations {
            match op.operator.as_str() {
                "q" => stack.push(ctm),
                "Q" => {
                    if let Some(prev) = stack.pop() {
                        ctm = prev;
                    }
                }
                "cm" if op.operands.len() == 6 => {
                    let m = Mat {
                        a: num(&op.operands[0]),
                        b: num(&op.operands[1]),
                        c: num(&op.operands[2]),
                        d: num(&op.operands[3]),
                        e: num(&op.operands[4]),
                        f: num(&op.operands[5]),
                    };
                    ctm = m.concat(ctm);
                }
                "Do" => {
                    if let Some(Object::Name(name)) = op.operands.first() {
                        if let Some(id) = names.get(name) {
                            let (w, h) = (ctm.rendered_width(), ctm.rendered_height());
                            let entry = sizes.entry(*id).or_insert((0.0, 0.0));
                            entry.0 = entry.0.max(w);
                            entry.1 = entry.1.max(h);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    sizes
}

/// Dictionary edits that accompany a replacement's new stream bytes, beyond
/// the always-updated `/Width`/`/Height`.
enum DictUpdate {
    /// JPEG path: the payload is still raw DCTDecode, nothing else changes.
    Dct,
    /// Flate path: normalize `/Filter` to the scalar name and set the new
    /// `/DecodeParms` (Up-predictor variant) or remove it (plain deflate).
    Flate { decode_parms: Option<lopdf::Dictionary> },
}

/// A planned image replacement, computed read-only before mutating the doc.
struct Replacement {
    id: ObjectId,
    content: Vec<u8>,
    width: i64,
    height: i64,
    dict_update: DictUpdate,
}

/// The single-filter classes `plan_replacement` knows how to re-encode.
enum FilterClass {
    DctOnly,
    FlateOnly,
    Other,
}

/// Classify a stream's `/Filter`: exactly DCTDecode (raw JPEG payload),
/// exactly FlateDecode (deflated raster data), or anything else (untouched).
/// Both the scalar-name and one-element-array forms are recognized.
fn classify_filter(doc: &Document, filter: &Object) -> FilterClass {
    let name = match resolve(doc, filter) {
        Object::Name(n) => n.as_slice(),
        Object::Array(items) if items.len() == 1 => match resolve(doc, &items[0]) {
            Object::Name(n) => n.as_slice(),
            _ => return FilterClass::Other,
        },
        _ => return FilterClass::Other,
    };
    match name {
        b"DCTDecode" => FilterClass::DctOnly,
        b"FlateDecode" => FilterClass::FlateOnly,
        _ => FilterClass::Other,
    }
}

/// Decode, resize, and re-encode one image if it's an over-resolution JPEG or
/// Flate raster. Returns `None` to leave the image untouched.
fn plan_replacement(
    doc: &Document,
    id: ObjectId,
    rendered: (f32, f32),
    options: OptimizeOptions,
) -> Option<Replacement> {
    let (rendered_w_pts, rendered_h_pts) = rendered;
    if rendered_w_pts <= 0.0 || rendered_h_pts <= 0.0 {
        return None;
    }

    // Defensive: a non-positive target DPI means "do not downsample".
    // Without this guard, target_w/target_h below would collapse toward 1px.
    let target_dpi = options.target_dpi;
    if target_dpi <= 0.0 {
        return None;
    }
    let dpi_margin = options.dpi_margin.max(1.0);

    let stream = doc.get_object(id).ok()?.as_stream().ok()?;
    let dict = &stream.dict;

    // Must be an image with no soft mask (we don't touch transparency).
    if !matches!(dict.get(b"Subtype").map(|s| resolve(doc, s)), Ok(Object::Name(n)) if n == b"Image")
    {
        return None;
    }
    if dict.get(b"SMask").is_ok() || dict.get(b"Mask").is_ok() {
        return None;
    }
    let filter = dict.get(b"Filter").ok()?;
    let class = classify_filter(doc, filter);
    if matches!(class, FilterClass::Other) {
        return None;
    }

    let px_w = dict.get(b"Width").ok().and_then(|o| o.as_i64().ok())? as u32;
    let px_h = dict.get(b"Height").ok().and_then(|o| o.as_i64().ok())? as u32;
    if px_w == 0 || px_h == 0 {
        return None;
    }

    // Effective DPI = pixels / inches displayed, evaluated on BOTH axes. A
    // non-uniformly scaled image (say 1000x1000 px drawn into 500x100 pt) can
    // sit under the threshold horizontally while being ~6x over-resolved
    // vertically; testing width alone skipped the whole image. Consider it if
    // *either* axis is over-resolved — the `target_* >= px_*` guard below still
    // prevents any upscaling. `non_uniform_placement_is_downsampled` pins this.
    let eff_dpi_w = px_w as f32 / (rendered_w_pts / 72.0);
    let eff_dpi_h = px_h as f32 / (rendered_h_pts / 72.0);
    if eff_dpi_w.max(eff_dpi_h) <= target_dpi * dpi_margin {
        return None;
    }

    let target_w = ((rendered_w_pts / 72.0) * target_dpi).round().max(1.0) as u32;
    let target_h = ((rendered_h_pts / 72.0) * target_dpi).round().max(1.0) as u32;
    if target_w >= px_w || target_h >= px_h {
        return None;
    }

    let (out, dict_update) = match class {
        FilterClass::DctOnly => {
            let out = plan_dct(stream, options, target_w, target_h)?;
            (out, DictUpdate::Dct)
        }
        FilterClass::FlateOnly => {
            if !options.downsample_flate_images {
                return None;
            }
            let (out, decode_parms) =
                plan_flate(doc, stream, px_w, px_h, target_w, target_h)?;
            (out, DictUpdate::Flate { decode_parms })
        }
        FilterClass::Other => return None,
    };

    if out.len() >= stream.content.len() {
        return None;
    }

    Some(Replacement {
        id,
        content: out,
        width: target_w as i64,
        height: target_h as i64,
        dict_update,
    })
}

/// The JPEG re-encode path: decode (scaled when possible), resize, re-encode
/// via mozjpeg. Channel count is preserved to match the unchanged /ColorSpace.
fn plan_dct(
    stream: &lopdf::Stream,
    options: OptimizeOptions,
    target_w: u32,
    target_h: u32,
) -> Option<Vec<u8>> {
    let quality = options.jpeg_quality.clamp(1, 100);

    // Prefer scaled decoding; fall back to a full decode for color spaces the
    // scaled path declines (CMYK/YCCK) or if libjpeg refuses the stream.
    let (decoded, is_gray) = decode_jpeg_scaled(&stream.content, target_w, target_h)
        .or_else(|| {
            let decoded =
                image::load_from_memory_with_format(&stream.content, ImageFormat::Jpeg).ok()?;
            let is_gray = matches!(
                decoded,
                DynamicImage::ImageLuma8(_) | DynamicImage::ImageLuma16(_)
            );
            Some((decoded, is_gray))
        })?;
    let resized = decoded.resize_exact(target_w, target_h, image::imageops::FilterType::Lanczos3);

    // Preserve the original component count so the PDF /ColorSpace (which we
    // leave unchanged) still matches: gray -> 1 channel, else RGB -> 3.
    encode_jpeg(resized, is_gray, quality)
}

/// Raw pixel budget above which a Flate image is never decoded: 256 MiB
/// (~9.5k x 9.5k RGB), far above anything legitimately placed on a page.
/// Guards against decompression bombs before any allocation happens.
const MAX_FLATE_PIXEL_BYTES: u64 = 256 * 1024 * 1024;

/// The Flate same-format path: inflate (hard-capped), un-apply any PNG
/// predictor, resize, and re-deflate at level 9 — trying both a PNG
/// Up-predictor variant and a plain no-predictor variant, keeping the smaller.
/// `/ColorSpace` is never touched, so no artifact-class change is possible.
///
/// Every eligibility gate returns `None` (image untouched) on doubt. Decoding
/// is done with our own capped inflate + spec-correct predictor inversion
/// instead of lopdf's `Stream::decompressed_content`, for three verified
/// lopdf 0.42 reasons: its `decode_row` mis-computes the PNG Avg filter
/// (`left + up/2` instead of `(left + up)/2` — silently wrong pixels with the
/// right length), it swallows corrupt-zlib errors and returns partial data as
/// `Ok`, and it has no decompressed-size cap. The predictor-VALUE gates still
/// exist because lopdf also ignores TIFF Predictor 2 and the array form of
/// `/DecodeParms`; we keep both out of M1 scope entirely.
fn plan_flate(
    doc: &Document,
    stream: &lopdf::Stream,
    px_w: u32,
    px_h: u32,
    target_w: u32,
    target_h: u32,
) -> Option<(Vec<u8>, Option<lopdf::Dictionary>)> {
    let dict = &stream.dict;

    // M1 scope: 8-bit only (Indexed/1/2/4/16-bit are skipped, never touched).
    let bpc = dict
        .get(b"BitsPerComponent")
        .ok()
        .map(|o| resolve(doc, o))
        .and_then(|o| o.as_i64().ok())?;
    if bpc != 8 {
        return None;
    }

    // A /Decode array remaps sample values; resizing under a remap is not
    // sample-preserving, so skip.
    if dict.get(b"Decode").is_ok() {
        return None;
    }

    let channels = flate_channels(doc, dict)?;
    let encoding = flate_encoding(dict, channels, px_w)?;

    // Decompression-bomb guard before decoding, then an EXACT length check
    // after: any mismatch (truncated stream, lying dimensions) means we cannot
    // trust the pixel data — skip.
    let expected = u64::from(px_w) * u64::from(px_h) * channels as u64;
    if expected > MAX_FLATE_PIXEL_BYTES {
        return None;
    }
    let bytes_per_row = px_w as usize * channels;
    let decoded = match encoding {
        FlateEncoding::Plain => inflate_capped(&stream.content, expected as usize)?,
        FlateEncoding::PngPredictor => {
            // Filtered layout: one leading tag byte per row.
            let filtered_len = (bytes_per_row + 1) * px_h as usize;
            let inflated = inflate_capped(&stream.content, filtered_len)?;
            if inflated.len() != filtered_len {
                return None;
            }
            png_defilter(&inflated, channels, bytes_per_row)?
        }
    };
    if decoded.len() as u64 != expected {
        return None;
    }

    let img = if channels == 3 {
        DynamicImage::ImageRgb8(image::RgbImage::from_raw(px_w, px_h, decoded)?)
    } else {
        DynamicImage::ImageLuma8(image::GrayImage::from_raw(px_w, px_h, decoded)?)
    };
    let resized = img.resize_exact(target_w, target_h, image::imageops::FilterType::Lanczos3);
    let raw = if channels == 3 {
        resized.into_rgb8().into_raw()
    } else {
        resized.into_luma8().into_raw()
    };

    // Re-encode both variants and keep the smaller. The Up-predictor variant
    // must also pay for the /DecodeParms dict it forces into the image
    // dictionary (~70 serialized bytes), so the comparison includes that.
    let plain = deflate_level9(&raw)?;
    let up = deflate_level9(&png_up_filter(&raw, target_w, channels))?;
    const PARMS_OVERHEAD: usize = 70;
    if up.len() + PARMS_OVERHEAD < plain.len() {
        let parms = dictionary! {
            "Predictor" => 15_i64,
            "Colors" => channels as i64,
            "BitsPerComponent" => 8_i64,
            "Columns" => target_w as i64,
        };
        Some((up, Some(parms)))
    } else {
        Some((plain, None))
    }
}

/// Resolve `/ColorSpace` to a component count the M1 Flate path handles:
/// DeviceRGB / ICCBased N=3 -> 3, DeviceGray / ICCBased N=1 -> 1. Anything
/// else (Indexed, Separation, Lab, CalRGB, ...) -> `None` (image untouched).
fn flate_channels(doc: &Document, dict: &lopdf::Dictionary) -> Option<usize> {
    match resolve(doc, dict.get(b"ColorSpace").ok()?) {
        Object::Name(n) if n == b"DeviceRGB" => Some(3),
        Object::Name(n) if n == b"DeviceGray" => Some(1),
        Object::Array(items) if items.len() == 2 => {
            if !matches!(resolve(doc, &items[0]), Object::Name(n) if n == b"ICCBased") {
                return None;
            }
            let icc = resolve(doc, &items[1]).as_stream().ok()?;
            match resolve(doc, icc.dict.get(b"N").ok()?).as_i64().ok()? {
                1 => Some(1),
                3 => Some(3),
                _ => None,
            }
        }
        _ => None,
    }
}

/// How a Flate image's pixel data is laid out inside the deflate stream.
enum FlateEncoding {
    /// Raw pixel rows, no predictor.
    Plain,
    /// PNG-filtered rows (Predictor 10-15): a leading filter-type byte per row.
    PngPredictor,
}

/// Classify the stream's `/DecodeParms` (if any) into a layout the M1 path
/// decodes, or `None` (skip). Only the direct-dictionary form is accepted
/// (arrays and indirect references are out of scope — lopdf ignores them too,
/// so such streams have never been reliably decodable here), the predictor
/// must be 1 (none) or PNG 10-15 (TIFF Predictor 2 is out of scope), and for
/// PNG predictors the declared Colors/Columns/BitsPerComponent must match the
/// image dictionary or the row stride would be wrong. Defaults mirror the PDF
/// spec (Predictor 1, Colors 1, Columns 1, BitsPerComponent 8).
fn flate_encoding(
    dict: &lopdf::Dictionary,
    channels: usize,
    px_w: u32,
) -> Option<FlateEncoding> {
    let parms = match dict.get(b"DecodeParms") {
        Err(_) => return Some(FlateEncoding::Plain), // no parms: plain deflate
        Ok(Object::Dictionary(d)) => d,
        Ok(_) => return None, // array / reference form: out of M1 scope
    };
    let predictor = parms.get(b"Predictor").and_then(Object::as_i64).unwrap_or(1);
    match predictor {
        1 => Some(FlateEncoding::Plain),
        10..=15 => {
            let colors = parms.get(b"Colors").and_then(Object::as_i64).unwrap_or(1);
            let columns = parms.get(b"Columns").and_then(Object::as_i64).unwrap_or(1);
            let parms_bpc = parms
                .get(b"BitsPerComponent")
                .and_then(Object::as_i64)
                .unwrap_or(8);
            let matches_image = colors == channels as i64
                && columns == i64::from(px_w)
                && parms_bpc == 8;
            matches_image.then_some(FlateEncoding::PngPredictor)
        }
        _ => None,
    }
}

/// Inflate a zlib stream with a hard output cap. Returns `None` for corrupt
/// or truncated input (unlike lopdf, which logs and returns partial data as
/// `Ok`) and for streams that would produce more than `limit` bytes — the
/// actual decompression-bomb guard, enforced *during* inflation.
fn inflate_capped(data: &[u8], limit: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    // Read one byte past the cap so an over-limit stream is distinguishable
    // from one that is exactly at it.
    let mut dec = flate2::read::ZlibDecoder::new(data).take(limit as u64 + 1);
    dec.read_to_end(&mut out).ok()?;
    if out.len() > limit {
        return None;
    }
    Some(out)
}

/// PNG Paeth predictor (PNG spec §9.4).
fn paeth_predict(left: u8, up: u8, upper_left: u8) -> u8 {
    let (a, b, c) = (i16::from(left), i16::from(up), i16::from(upper_left));
    let p = a + b - c;
    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
    if pa <= pb && pa <= pc {
        left
    } else if pb <= pc {
        up
    } else {
        upper_left
    }
}

/// Spec-correct inversion of PNG row filters over a whole frame of
/// `[tag][filtered row]` records. Hand-rolled rather than lopdf's
/// `filters::png::decode_frame` because lopdf 0.42 mis-computes the Avg
/// filter (`left + up/2` instead of `(left + up)/2`, verified by round-trip
/// test) — silently wrong pixels with the correct length, which the outer
/// length check cannot catch. Returns `None` on any malformed shape.
fn png_defilter(data: &[u8], bytes_per_pixel: usize, bytes_per_row: usize) -> Option<Vec<u8>> {
    let stride = bytes_per_row.checked_add(1)?;
    if bytes_per_row == 0 || !data.len().is_multiple_of(stride) {
        return None;
    }
    let rows = data.len() / stride;
    let mut out = vec![0u8; rows * bytes_per_row];
    for r in 0..rows {
        let src = &data[r * stride..(r + 1) * stride];
        let tag = src[0];
        let (done, rest) = out.split_at_mut(r * bytes_per_row);
        let prev = &done[done.len().saturating_sub(bytes_per_row)..];
        let cur = &mut rest[..bytes_per_row];
        cur.copy_from_slice(&src[1..]);
        let bpp = bytes_per_pixel;
        let up_at = |i: usize| if r == 0 { 0 } else { prev[i] };
        match tag {
            0 => {}
            1 => {
                for i in bpp..bytes_per_row {
                    cur[i] = cur[i].wrapping_add(cur[i - bpp]);
                }
            }
            2 => {
                for (i, c) in cur.iter_mut().enumerate() {
                    *c = c.wrapping_add(up_at(i));
                }
            }
            3 => {
                for i in 0..bytes_per_row {
                    let left = if i >= bpp { u16::from(cur[i - bpp]) } else { 0 };
                    let up = u16::from(up_at(i));
                    cur[i] = cur[i].wrapping_add(((left + up) / 2) as u8);
                }
            }
            4 => {
                for i in 0..bytes_per_row {
                    let left = if i >= bpp { cur[i - bpp] } else { 0 };
                    let up = up_at(i);
                    let upper_left = if i >= bpp { up_at(i - bpp) } else { 0 };
                    cur[i] = cur[i].wrapping_add(paeth_predict(left, up, upper_left));
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Apply the PNG Up filter to every row, producing the predictor-encoded byte
/// stream FlateDecode + `/Predictor >= 10` expects: each row is a leading
/// filter-type byte (2 = Up) followed by the filtered row bytes.
fn png_up_filter(raw: &[u8], width: u32, channels: usize) -> Vec<u8> {
    use lopdf::filters::png;

    let bytes_per_row = width as usize * channels;
    let rows = raw.len() / bytes_per_row.max(1);
    let mut out = Vec::with_capacity(raw.len() + rows);
    let mut previous = vec![0u8; bytes_per_row];
    for row in raw.chunks_exact(bytes_per_row) {
        let mut current = row.to_vec();
        // `previous` must be the UNFILTERED prior row (encode_row subtracts it).
        png::encode_row(png::FilterType::Up, channels, &previous, &mut current);
        out.push(2); // PNG filter-type tag: Up
        out.extend_from_slice(&current);
        previous.copy_from_slice(row);
    }
    out
}

/// Deflate (zlib container, as FlateDecode requires) at maximum compression.
fn deflate_level9(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(9));
    enc.write_all(data).ok()?;
    enc.finish().ok()
}

/// Encode an image as JPEG using mozjpeg (optimized Huffman + trellis), which
/// produces substantially smaller files than the basic encoder at equal
/// quality. Channel count is preserved to match the unchanged PDF /ColorSpace.
/// Decode a JPEG that we are about to shrink, using libjpeg's DCT-domain
/// scaled decoding so the full-resolution image is never materialized.
///
/// Decoding is done at the smallest `n/8` scale whose output still covers the
/// target in both axes, so the caller's Lanczos3 step always downsamples and
/// quality is preserved. A 4000x4000 source targeting 180px decodes at 1/8
/// (500x500, ~750 KB) instead of full size (~48 MB) — most of the IDCT work and
/// nearly all of the intermediate allocation disappear.
///
/// Returns `(image, is_grayscale)`, or `None` for anything that is not plain
/// RGB or grayscale (e.g. CMYK/YCCK) so the caller can fall back to the
/// general-purpose decoder rather than risk mis-handling color.
fn decode_jpeg_scaled(
    data: &[u8],
    target_w: u32,
    target_h: u32,
) -> Option<(DynamicImage, bool)> {
    let mut dec = mozjpeg::Decompress::new_mem(data).ok()?;
    let (full_w, full_h) = (dec.width(), dec.height());
    if full_w == 0 || full_h == 0 {
        return None;
    }

    // Smallest n/8 that still covers the target in BOTH axes (never upscale).
    let mut numerator = 8u8;
    for n in 1..=8u8 {
        let scaled_w = (full_w * n as usize).div_ceil(8);
        let scaled_h = (full_h * n as usize).div_ceil(8);
        if scaled_w >= target_w as usize && scaled_h >= target_h as usize {
            numerator = n;
            break;
        }
    }
    // Decide the channel count from the JPEG's OWN colorspace and then request
    // that output explicitly. Do NOT rely on `image()`/`out_color_space`: for a
    // grayscale JPEG libjpeg's default can still hand back RGB, which would
    // write 3-channel data into a stream whose PDF /ColorSpace is DeviceGray —
    // a corrupt image. `grayscale_stays_grayscale` pins this.
    use mozjpeg::ColorSpace;
    let is_gray = dec.color_space() == ColorSpace::JCS_GRAYSCALE;
    // CMYK/YCCK need a color conversion we don't want to hand-roll.
    if matches!(dec.color_space(), ColorSpace::JCS_CMYK | ColorSpace::JCS_YCCK) {
        return None;
    }

    dec.scale(numerator);

    let mut started = if is_gray {
        dec.grayscale().ok()?
    } else {
        dec.rgb().ok()?
    };
    let (w, h) = (started.width(), started.height());
    let buf: Vec<u8> = started.read_scanlines::<u8>().ok()?;
    started.finish().ok()?;

    let img = if is_gray {
        DynamicImage::ImageLuma8(image::GrayImage::from_raw(w as u32, h as u32, buf)?)
    } else {
        DynamicImage::ImageRgb8(image::RgbImage::from_raw(w as u32, h as u32, buf)?)
    };
    Some((img, is_gray))
}

/// Takes `img` **by value** so the pixel buffer can be moved out with `into_*`
/// instead of copied: `to_rgb8(&self)` always allocates a fresh buffer, while
/// `into_rgb8(self)` returns the existing one when the variant already matches
/// (which it does — `resize_exact` preserves the type).
fn encode_jpeg(img: DynamicImage, is_gray: bool, quality: u8) -> Option<Vec<u8>> {
    use mozjpeg::{ColorSpace, Compress};

    // Capture dimensions before the buffer is moved out.
    let (width, height) = (img.width() as usize, img.height() as usize);
    let (color_space, data) = if is_gray {
        (ColorSpace::JCS_GRAYSCALE, img.into_luma8().into_raw())
    } else {
        (ColorSpace::JCS_RGB, img.into_rgb8().into_raw())
    };

    let mut comp = Compress::new(color_space);
    comp.set_size(width, height);
    comp.set_quality(quality as f32);

    let mut started = comp.start_compress(Vec::new()).ok()?;
    started.write_scanlines(&data).ok()?;
    started.finish().ok()
}

/// Serialize a non-stream object to bytes for hashing. Returns `None` if
/// serialization fails (the object will not be deduplicated in that case).
fn serialize_object(obj: &Object) -> Option<Vec<u8>> {
    if matches!(obj, Object::Stream(_)) {
        return None;
    }
    // lopdf's Writer is private, but Object has a deterministic Debug impl:
    // structurally identical objects produce identical output, which is exactly
    // the equivalence we need for dedup. (Conservative: differing key order or
    // formatting simply means two objects aren't merged — never a false merge.)
    Some(format!("{obj:?}").into_bytes())
}

/// Recursively replace all `ObjectId` references in `obj` according to the
/// `remap` table. Streams are traversed (dict only; content bytes unchanged).
fn remap_references(obj: &mut Object, remap: &HashMap<ObjectId, ObjectId>) {
    match obj {
        Object::Reference(id) => {
            if let Some(&canonical) = remap.get(id) {
                *id = canonical;
            }
        }
        Object::Array(arr) => {
            for item in arr.iter_mut() {
                remap_references(item, remap);
            }
        }
        Object::Dictionary(dict) => {
            for (_, val) in dict.iter_mut() {
                remap_references(val, remap);
            }
        }
        Object::Stream(stream) => {
            for (_, val) in stream.dict.iter_mut() {
                remap_references(val, remap);
            }
        }
        _ => {}
    }
}

/// Merge true duplicate non-stream objects. Two objects are duplicates when
/// their serialized bytes are identical. For each duplicate group the lowest
/// `ObjectId` is kept as canonical; all references to the others are
/// redirected, and the duplicates are removed from the document.
///
/// This is always safe (identical objects produce identical results in all
/// contexts) and reduces the object count before packing. On sparse documents
/// (a few dozen duplicates among a few hundred objects) the gain is small; on
/// denser documents it can be more significant.
fn dedup_objects(doc: &mut Document) {
    // Group non-stream objects by their exact serialized bytes. Keying the map
    // on the bytes themselves (not a 64-bit hash of them) means only genuinely
    // identical objects ever share a bucket, so a hash collision can never
    // cause two different objects to be merged.
    let mut by_bytes: HashMap<Vec<u8>, Vec<ObjectId>> = HashMap::new();
    for (&id, obj) in doc.objects.iter() {
        if let Some(bytes) = serialize_object(obj) {
            by_bytes.entry(bytes).or_default().push(id);
        }
    }

    // Build a remap table: non-canonical id -> canonical id.
    // Use the smallest id in each group as canonical (stable, deterministic).
    let mut remap: HashMap<ObjectId, ObjectId> = HashMap::new();
    for (_, mut ids) in by_bytes {
        if ids.len() < 2 {
            continue;
        }
        ids.sort_unstable();
        let canonical = ids[0];
        for duplicate in &ids[1..] {
            remap.insert(*duplicate, canonical);
        }
    }

    if remap.is_empty() {
        return;
    }

    // Rewrite all references throughout the document.
    for obj in doc.objects.values_mut() {
        remap_references(obj, &remap);
    }

    // Also fix any references in the trailer dict.
    for (_, val) in doc.trailer.iter_mut() {
        remap_references(val, &remap);
    }

    // Remove the now-redundant duplicate objects. prune_objects() would also
    // clean them up, but removing them explicitly here keeps the object table
    // consistent before renumber_objects().
    for id in remap.keys() {
        doc.objects.remove(id);
    }
}

/// Merge byte-identical **stream** objects — in practice repeated images: a logo
/// or product shot re-embedded once per page. Returns true if anything merged.
///
/// Run *before* image planning, so a repeated image is decoded, resized and
/// re-encoded exactly **once** instead of once per copy, and stored once in the
/// output. [`dedup_objects`] deliberately skips streams (Debug-formatting
/// multi-megabyte content into a map key would be enormous), so this is the
/// stream-shaped counterpart.
///
/// Safety: buckets are keyed on a cheap `(dict, len, content-hash)` triple, then
/// full byte equality is verified before merging — so a hash collision can never
/// cause a false merge, matching `dedup_objects`' conservative stance.
fn dedup_streams(doc: &mut Document) -> bool {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut buckets: HashMap<(Vec<u8>, usize, u64), Vec<ObjectId>> = HashMap::new();
    for (&id, obj) in doc.objects.iter() {
        if let Object::Stream(s) = obj {
            let mut hasher = DefaultHasher::new();
            s.content.hash(&mut hasher);
            let key = (
                format!("{:?}", s.dict).into_bytes(),
                s.content.len(),
                hasher.finish(),
            );
            buckets.entry(key).or_default().push(id);
        }
    }

    // Build the remap under immutable borrows only, verifying real equality.
    let mut remap: HashMap<ObjectId, ObjectId> = HashMap::new();
    for ids in buckets.values() {
        if ids.len() < 2 {
            continue;
        }
        let mut ids = ids.clone();
        ids.sort_unstable();
        let canonical = ids[0];
        let Some(Object::Stream(canon)) = doc.objects.get(&canonical) else {
            continue;
        };
        for dup in &ids[1..] {
            if let Some(Object::Stream(other)) = doc.objects.get(dup) {
                // Same bucket already implies an equal dict; confirm the bytes.
                if other.content == canon.content {
                    remap.insert(*dup, canonical);
                }
            }
        }
    }

    if remap.is_empty() {
        return false;
    }

    for obj in doc.objects.values_mut() {
        remap_references(obj, &remap);
    }
    for (_, val) in doc.trailer.iter_mut() {
        remap_references(val, &remap);
    }
    for id in remap.keys() {
        doc.objects.remove(id);
    }
    true
}

/// Returns `Ok(None)` when there was genuinely nothing to do, so the caller can
/// hand back the original bytes without anyone allocating a throwaway copy of
/// them. `Ok(Some(bytes))` is a real, rewritten document.
fn try_optimize(
    input: &[u8],
    options: OptimizeOptions,
) -> Result<Option<Vec<u8>>, lopdf::Error> {
    let mut doc = Document::load_mem(input)?;

    // Collapse repeated images first: every downstream step (placement
    // collection, decode/resize/re-encode, and the final write) then sees one
    // object instead of N identical ones.
    let merged_streams = dedup_streams(&mut doc);

    // Plan every image in parallel: each is an independent decode -> resize ->
    // re-encode against an immutable &Document, so there is no shared mutable
    // state. Rayon propagates a worker panic to this thread, so the
    // `catch_unwind` boundary in `optimize_with_options` still holds
    // (`crafted_pdf_panic_is_caught_not_unwound` pins that).
    let placements = collect_placements(&doc);
    let replacements: Vec<Replacement> = placements
        .par_iter()
        .filter_map(|(&id, &rendered)| plan_replacement(&doc, id, rendered, options))
        .collect();

    // If we have no work to do at all, hand back the original bytes.
    // Note: pack_object_streams alone is not sufficient reason to write a new
    // file — packing only helps when there are objects to pack, and the
    // dispatcher handles it cheaply inside the save step regardless.
    // `merged_streams` counts as work: dedup_streams may have collapsed repeated
    // images even when nothing needed downsampling, and discarding that would
    // throw away a real size win.
    if replacements.is_empty() && !options.strip_accessibility && !merged_streams {
        return Ok(None);
    }

    for r in replacements {
        if let Ok(Object::Stream(stream)) = doc.get_object_mut(r.id) {
            stream.set_content(r.content);
            stream.dict.set("Width", Object::Integer(r.width));
            stream.dict.set("Height", Object::Integer(r.height));
            if let DictUpdate::Flate { decode_parms } = r.dict_update {
                // Normalize /Filter to the scalar name (the array form was
                // accepted on input) and write parms matching the new payload.
                stream
                    .dict
                    .set("Filter", Object::Name(b"FlateDecode".to_vec()));
                match decode_parms {
                    Some(parms) => stream.dict.set("DecodeParms", Object::Dictionary(parms)),
                    None => {
                        stream.dict.remove(b"DecodeParms");
                    }
                }
            }
        }
    }

    // Optionally strip the PDF's structure tree (accessibility metadata). This
    // is what Ghostscript's /ebook and /screen presets do silently: removes
    // /StructTreeRoot (the tree of StructElem objects screen readers navigate),
    // /MarkInfo, and /Lang from the catalog. Visually lossless; the resulting
    // PDF degrades from "tagged" to "untagged" and is no longer PDF/UA.
    // `prune_objects()` below drops the now-orphaned StructElem subtree.
    if options.strip_accessibility {
        if let Ok(catalog) = doc.catalog_mut() {
            catalog.remove(b"StructTreeRoot");
            catalog.remove(b"MarkInfo");
            catalog.remove(b"Lang");
        }
    }

    // Merge true duplicate non-stream objects (identical serialized bytes ->
    // same canonical id, references redirected, duplicates removed). Runs
    // before prune so the orphan cleanup sees an already-compacted object set.
    dedup_objects(&mut doc);

    // Drop orphaned objects, then Flate-compress any uncompressed content
    // streams (DCTDecode images are skipped — Stream::compress only touches
    // streams without a /Filter).
    doc.prune_objects();
    doc.compress();

    // Renumber to a contiguous id space so the saved trailer /Size matches the
    // highest object number. Without this, lopdf 0.41's classic save emits a
    // /Size that's slightly too high, which `qpdf --check` flags (benign, but
    // we want strictly clean output for email recipients / strict readers).
    doc.renumber_objects();

    save_document(&mut doc, options).map(Some)
}

/// Serialize the document, optionally using PDF 1.5 object-stream packing when
/// `options.pack_object_streams` is true. The packed path produces smaller
/// output for object-heavy documents but is more complex; the classic path is
/// the always-available fallback and matches what lopdf ships.
fn save_document(
    doc: &mut Document,
    options: OptimizeOptions,
) -> Result<Vec<u8>, lopdf::Error> {
    if options.pack_object_streams {
        pack_and_save(doc)
    } else {
        let mut out: Vec<u8> = Vec::new();
        doc.save_to(&mut out)?;
        Ok(out)
    }
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
        .max_objects_per_stream(100_000_000)
        .compression_level(9)
        .build();
    let mut out: Vec<u8> = Vec::new();
    doc.save_with_options(&mut out, options)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::content::Operation;
    use lopdf::{dictionary, Stream};

    /// Build a one-page PDF embedding a `px`×`px` RGB JPEG drawn into a
    /// `draw_pts`×`draw_pts` box, i.e. at an effective DPI of px/(draw_pts/72).
    fn build_pdf(px: u32, draw_pts: i64) -> Vec<u8> {
        build_pdf_placed(px, draw_pts, draw_pts)
    }

    /// Same, but with an independent width/height placement box so a
    /// NON-UNIFORMLY scaled image can be exercised.
    fn build_pdf_placed(px: u32, draw_w_pts: i64, draw_h_pts: i64) -> Vec<u8> {
        // A non-flat gradient so JPEG has real content to compress.
        let mut img = image::RgbImage::new(px, px);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            *pixel = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let mut jpeg: Vec<u8> = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 92)
            .encode_image(&img)
            .unwrap();

        let mut doc = Document::with_version("1.5");
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => px as i64,
                "Height" => px as i64,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
                "Filter" => "DCTDecode",
            },
            jpeg,
        ));

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
        let content_id =
            doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));

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

    /// One page holding `copies` SEPARATE image objects that all contain the
    /// same JPEG bytes — what you get when a logo or product shot is
    /// re-embedded per page. Pins the `dedup_streams` behavior.
    fn build_pdf_duplicate_images(copies: usize, px: u32) -> Vec<u8> {
        let mut img = image::RgbImage::new(px, px);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            *pixel = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let mut jpeg: Vec<u8> = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 92)
            .encode_image(&img)
            .unwrap();

        let mut doc = Document::with_version("1.5");
        let mut ops = vec![];
        let mut xobjs = lopdf::Dictionary::new();
        for i in 0..copies {
            let id = doc.add_object(Stream::new(
                dictionary! {
                    "Type" => "XObject", "Subtype" => "Image",
                    "Width" => px as i64, "Height" => px as i64,
                    "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8,
                    "Filter" => "DCTDecode",
                },
                jpeg.clone(),
            ));
            let name = format!("Im{i}");
            xobjs.set(name.as_bytes().to_vec(), id);
            ops.push(Operation::new("q", vec![]));
            ops.push(Operation::new(
                "cm",
                vec![100.into(), 0.into(), 0.into(), 100.into(), 0.into(), 0.into()],
            ));
            ops.push(Operation::new("Do", vec![Object::Name(name.into_bytes())]));
            ops.push(Operation::new("Q", vec![]));
        }
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            Content { operations: ops }.encode().unwrap(),
        ));
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! { "XObject" => xobjs },
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut out: Vec<u8> = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn count_image_streams(pdf: &[u8]) -> usize {
        let doc = Document::load_mem(pdf).unwrap();
        doc.objects
            .values()
            .filter(|o| {
                matches!(o, Object::Stream(s)
                    if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image"))
            })
            .count()
    }

    fn image_dims(pdf: &[u8]) -> (i64, i64) {
        let doc = Document::load_mem(pdf).unwrap();
        for obj in doc.objects.values() {
            if let Object::Stream(s) = obj {
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
                    let w = s.dict.get(b"Width").unwrap().as_i64().unwrap();
                    let h = s.dict.get(b"Height").unwrap().as_i64().unwrap();
                    return (w, h);
                }
            }
        }
        panic!("no image found");
    }

    // ---- Flate-path fixtures ----------------------------------------------

    /// Deterministic xorshift noise. Noise is essentially incompressible, so a
    /// downsampled re-encode is reliably smaller than the original — unlike a
    /// regular gradient, whose predictor-filtered rows deflate to almost
    /// nothing and (correctly) trip the never-larger guard.
    fn flate_pixels(px_w: u32, px_h: u32, channels: usize) -> Vec<u8> {
        let mut state = 0x2545_F491_u32;
        (0..px_w as usize * px_h as usize * channels)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state >> 24) as u8
            })
            .collect()
    }

    /// Apply one PNG row filter to every row, producing the tagged row stream
    /// (`[tag][filtered row]...`) that FlateDecode + PNG predictors expect.
    /// Spec-correct forward filtering (PNG spec §9), hand-rolled: lopdf 0.42's
    /// `encode_row` mis-computes Avg (`(left wrapping+ up)/2` overflows), so
    /// fixtures built with it would not exercise the real-world byte shape.
    fn png_filter_rows(raw: &[u8], px_w: u32, channels: usize, tag: u8) -> Vec<u8> {
        let bpr = px_w as usize * channels;
        let bpp = channels;
        let mut out = Vec::with_capacity(raw.len() + raw.len() / bpr);
        let mut previous = vec![0u8; bpr];
        for row in raw.chunks_exact(bpr) {
            out.push(tag);
            for i in 0..bpr {
                let x = row[i];
                let left = if i >= bpp { row[i - bpp] } else { 0 };
                let up = previous[i];
                let upper_left = if i >= bpp { previous[i - bpp] } else { 0 };
                let filtered = match tag {
                    0 => x,
                    1 => x.wrapping_sub(left),
                    2 => x.wrapping_sub(up),
                    3 => x.wrapping_sub(((u16::from(left) + u16::from(up)) / 2) as u8),
                    4 => x.wrapping_sub(paeth_predict(left, up, upper_left)),
                    _ => unreachable!("bad filter tag"),
                };
                out.push(filtered);
            }
            previous.copy_from_slice(row);
        }
        out
    }

    /// Wrap a prepared image XObject stream in a one-page document drawing it
    /// into a `draw_pts` square box.
    fn wrap_image_pdf(doc: &mut Document, img_id: ObjectId, draw_pts: i64) -> Vec<u8> {
        let content = Content {
            operations: vec![
                Operation::new("q", vec![]),
                Operation::new(
                    "cm",
                    vec![
                        draw_pts.into(),
                        0.into(),
                        0.into(),
                        draw_pts.into(),
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
            "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => img_id } },
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut out: Vec<u8> = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    /// Build a one-page PDF embedding a `px`x`px` FlateDecode image drawn into
    /// a `draw_pts` square. `predictor: Some(f)` stores PNG-filtered rows with
    /// a `/DecodeParms << /Predictor 15 ... >>`; `None` stores plain deflate
    /// with no DecodeParms. `filter_as_array`/`parms_as_array` exercise the
    /// array dictionary forms.
    fn build_pdf_flate_ext(
        px: u32,
        draw_pts: i64,
        channels: usize,
        predictor: Option<u8>,
        filter_as_array: bool,
        parms_as_array: bool,
    ) -> Vec<u8> {
        let raw = flate_pixels(px, px, channels);
        let (payload, parms) = match predictor {
            Some(tag) => {
                let filtered = png_filter_rows(&raw, px, channels, tag);
                let parms = dictionary! {
                    "Predictor" => 15_i64,
                    "Colors" => channels as i64,
                    "BitsPerComponent" => 8_i64,
                    "Columns" => px as i64,
                };
                (deflate_level9(&filtered).unwrap(), Some(parms))
            }
            None => (deflate_level9(&raw).unwrap(), None),
        };

        let mut dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => px as i64,
            "Height" => px as i64,
            "ColorSpace" => if channels == 1 { "DeviceGray" } else { "DeviceRGB" },
            "BitsPerComponent" => 8_i64,
        };
        if filter_as_array {
            dict.set("Filter", vec![Object::Name(b"FlateDecode".to_vec())]);
        } else {
            dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
        }
        if let Some(parms) = parms {
            if parms_as_array {
                dict.set("DecodeParms", vec![Object::Dictionary(parms)]);
            } else {
                dict.set("DecodeParms", Object::Dictionary(parms));
            }
        }

        let mut doc = Document::with_version("1.5");
        let img_id = doc.add_object(Stream::new(dict, payload));
        wrap_image_pdf(&mut doc, img_id, draw_pts)
    }

    fn build_pdf_flate(px: u32, draw_pts: i64, predictor: Option<u8>) -> Vec<u8> {
        build_pdf_flate_ext(px, draw_pts, 3, predictor, false, false)
    }

    /// The single image stream's decompressed pixel bytes.
    fn flate_image_pixels(pdf: &[u8]) -> Vec<u8> {
        let doc = Document::load_mem(pdf).unwrap();
        for obj in doc.objects.values() {
            if let Object::Stream(s) = obj {
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
                    return s.decompressed_content().unwrap();
                }
            }
        }
        panic!("no image stream");
    }

    // ---- Flate-path tests --------------------------------------------------

    #[test]
    fn flate_predictor_variants_are_downsampled() {
        // 400px drawn into 100pt => ~288 DPI, over target. Every PNG row
        // filter plus the no-DecodeParms form must decode, downsample to
        // ~181px, and still yield consistent pixel data.
        let variants: [(Option<u8>, &str); 6] = [
            (None, "no DecodeParms"),
            (Some(0), "Predictor 15 / rows None"),
            (Some(1), "Predictor 15 / rows Sub"),
            (Some(2), "Predictor 15 / rows Up"),
            (Some(3), "Predictor 15 / rows Avg"),
            (Some(4), "Predictor 15 / rows Paeth"),
        ];
        for (predictor, label) in variants {
            let pdf = build_pdf_flate(400, 100, predictor);
            let out = optimize(&pdf);
            let (w, h) = image_dims(&out);
            assert!(
                (150..=210).contains(&w) && w == h,
                "{label}: unexpected dims {w}x{h}"
            );
            assert!(out.len() < pdf.len(), "{label}: output must be smaller");
            // The rewritten stream, decoded through lopdf's own filter path,
            // must equal a direct Lanczos3 resize of the source pixels EXACTLY
            // (both sides run the same deterministic resize) — this pins the
            // whole predictor decode chain, not just the byte count.
            let pixels = flate_image_pixels(&out);
            let reference = DynamicImage::ImageRgb8(
                image::RgbImage::from_raw(400, 400, flate_pixels(400, 400, 3)).unwrap(),
            )
            .resize_exact(w as u32, h as u32, image::imageops::FilterType::Lanczos3)
            .into_rgb8()
            .into_raw();
            assert_eq!(
                pixels, reference,
                "{label}: decoded pixels must match a direct resize of the source"
            );
        }
    }

    #[test]
    fn flate_filter_array_form_is_downsampled() {
        // Day-1 probe, kept as a regression test: /Filter [/FlateDecode] with
        // a SCALAR /DecodeParms dict decodes fine and must be handled like
        // the scalar-name form.
        let pdf = build_pdf_flate_ext(400, 100, 3, Some(2), true, false);
        let out = optimize(&pdf);
        let (w, h) = image_dims(&out);
        assert!((150..=210).contains(&w) && w == h, "unexpected dims {w}x{h}");
        assert!(out.len() < pdf.len());
        assert!(Document::load_mem(&out).is_ok());
    }

    #[test]
    fn flate_decode_parms_array_form_is_skipped() {
        // Day-1 probe result: lopdf 0.42 only reads the direct-dict form of
        // /DecodeParms — the array form is silently NOT applied, which would
        // hand us predictor-filtered rows as if they were pixels. The gate
        // must skip such images entirely (original bytes returned).
        let pdf = build_pdf_flate_ext(400, 100, 3, Some(2), false, true);
        let out = optimize(&pdf);
        assert_eq!(out, pdf, "array-form DecodeParms must leave the file untouched");
    }

    #[test]
    fn flate_tiff_predictor_2_is_skipped() {
        // Day-1 probe result: lopdf silently IGNORES TIFF Predictor 2 — the
        // decoded bytes are wrong but the length is right, so the length check
        // alone cannot catch it. The explicit predictor-value gate must skip.
        let px = 400u32;
        let mut raw = flate_pixels(px, px, 3);
        let bpr = px as usize * 3;
        for row in raw.chunks_exact_mut(bpr) {
            for i in (3..bpr).rev() {
                row[i] = row[i].wrapping_sub(row[i - 3]);
            }
        }
        let payload = deflate_level9(&raw).unwrap();
        let mut doc = Document::with_version("1.5");
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => px as i64,
                "Height" => px as i64,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8_i64,
                "Filter" => "FlateDecode",
                "DecodeParms" => dictionary! {
                    "Predictor" => 2_i64,
                    "Colors" => 3_i64,
                    "BitsPerComponent" => 8_i64,
                    "Columns" => px as i64,
                },
            },
            payload,
        ));
        let pdf = wrap_image_pdf(&mut doc, img_id, 100);

        let out = optimize(&pdf);
        assert_eq!(out, pdf, "TIFF Predictor 2 must leave the file untouched");
    }

    #[test]
    fn flate_grayscale_stays_single_channel() {
        let pdf = build_pdf_flate_ext(400, 100, 1, Some(2), false, false);
        let out = optimize(&pdf);
        let (w, h) = image_dims(&out);
        assert!((150..=210).contains(&w) && w == h, "unexpected dims {w}x{h}");
        assert!(out.len() < pdf.len());
        // 1 channel in, 1 channel out — the DeviceGray /ColorSpace is unchanged
        // so 3-channel data here would be a corrupt image.
        let pixels = flate_image_pixels(&out);
        assert_eq!(pixels.len(), (w * h) as usize, "grayscale must stay 1-channel");
    }

    #[test]
    fn flate_iccbased_n3_is_accepted() {
        // ICCBased with /N 3 is component-count-equivalent to DeviceRGB, which
        // is all the same-format path needs (the color space is never touched).
        let px = 400u32;
        let raw = flate_pixels(px, px, 3);
        let payload = deflate_level9(&raw).unwrap();
        let mut doc = Document::with_version("1.5");
        // A stand-in ICC profile stream: /N is what matters to the gate.
        let icc_id = doc.add_object(Stream::new(
            dictionary! { "N" => 3_i64 },
            vec![0u8; 128],
        ));
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => px as i64,
                "Height" => px as i64,
                "ColorSpace" => vec![Object::Name(b"ICCBased".to_vec()), icc_id.into()],
                "BitsPerComponent" => 8_i64,
                "Filter" => "FlateDecode",
            },
            payload,
        ));
        let pdf = wrap_image_pdf(&mut doc, img_id, 100);

        let out = optimize(&pdf);
        let (w, h) = image_dims(&out);
        assert!((150..=210).contains(&w) && w == h, "unexpected dims {w}x{h}");
        assert!(out.len() < pdf.len());
        assert!(Document::load_mem(&out).is_ok());
    }

    /// Build an over-resolution Flate image PDF whose dict is customized by
    /// `mutate` before saving — for the "provably untouched" gate tests.
    fn build_flate_pdf_with_dict(
        px: u32,
        channels: usize,
        mutate: impl FnOnce(&mut Document, &mut lopdf::Dictionary),
    ) -> Vec<u8> {
        let raw = flate_pixels(px, px, channels);
        let payload = deflate_level9(&raw).unwrap();
        let mut doc = Document::with_version("1.5");
        let mut dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => px as i64,
            "Height" => px as i64,
            "ColorSpace" => if channels == 1 { "DeviceGray" } else { "DeviceRGB" },
            "BitsPerComponent" => 8_i64,
            "Filter" => "FlateDecode",
        };
        mutate(&mut doc, &mut dict);
        let img_id = doc.add_object(Stream::new(dict, payload));
        wrap_image_pdf(&mut doc, img_id, 100)
    }

    #[test]
    fn flate_ineligible_images_are_untouched() {
        // Each ineligible shape must return the ORIGINAL bytes — dims and
        // stream bytes provably unchanged (whole-file equality implies both).
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "Indexed color space",
                build_flate_pdf_with_dict(400, 3, |_, d| {
                    d.set(
                        "ColorSpace",
                        vec![
                            Object::Name(b"Indexed".to_vec()),
                            Object::Name(b"DeviceRGB".to_vec()),
                            Object::Integer(255),
                            Object::String(vec![0u8; 768], lopdf::StringFormat::Hexadecimal),
                        ],
                    );
                }),
            ),
            (
                "1-bit",
                build_flate_pdf_with_dict(400, 1, |_, d| {
                    d.set("BitsPerComponent", 1_i64);
                }),
            ),
            (
                "16-bit",
                build_flate_pdf_with_dict(400, 3, |_, d| {
                    d.set("BitsPerComponent", 16_i64);
                }),
            ),
            (
                "SMask present",
                build_flate_pdf_with_dict(400, 3, |doc, d| {
                    let mask = doc.add_object(Stream::new(
                        dictionary! {
                            "Type" => "XObject",
                            "Subtype" => "Image",
                            "Width" => 400_i64,
                            "Height" => 400_i64,
                            "ColorSpace" => "DeviceGray",
                            "BitsPerComponent" => 8_i64,
                            "Filter" => "FlateDecode",
                        },
                        deflate_level9(&flate_pixels(400, 400, 1)).unwrap(),
                    ));
                    d.set("SMask", mask);
                }),
            ),
            (
                "/Decode array present",
                build_flate_pdf_with_dict(400, 3, |_, d| {
                    d.set(
                        "Decode",
                        vec![
                            1.into(), 0.into(), 1.into(), 0.into(), 1.into(), 0.into(),
                        ],
                    );
                }),
            ),
        ];
        for (label, pdf) in cases {
            let out = optimize(&pdf);
            assert_eq!(out, pdf, "{label}: must be byte-identical to the input");
        }
    }

    #[test]
    fn flate_corrupt_streams_return_original_bytes() {
        // Degradation contract: corruption must yield the EXACT original
        // bytes, never a partially rewritten document.
        // (a) Truncated zlib body — lopdf returns partial data as Ok, so the
        //     exact-length check is what catches this.
        let pdf = build_pdf_flate(400, 100, None);
        let mut doc = Document::load_mem(&pdf).unwrap();
        for obj in doc.objects.values_mut() {
            if let Object::Stream(s) = obj {
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
                    let half = s.content.len() / 2;
                    let truncated = s.content[..half].to_vec();
                    s.set_content(truncated);
                }
            }
        }
        let mut truncated_pdf: Vec<u8> = Vec::new();
        doc.save_to(&mut truncated_pdf).unwrap();
        let out = optimize(&truncated_pdf);
        assert_eq!(out, truncated_pdf, "truncated zlib must return original bytes");

        // (b) Decoded-length mismatch: dict claims 400x400 but the stream
        //     holds 200x200 worth of pixels.
        let small = deflate_level9(&flate_pixels(200, 200, 3)).unwrap();
        let mut doc = Document::with_version("1.5");
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 400_i64,
                "Height" => 400_i64,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8_i64,
                "Filter" => "FlateDecode",
            },
            small,
        ));
        let mismatched = wrap_image_pdf(&mut doc, img_id, 100);
        let out = optimize(&mismatched);
        assert_eq!(out, mismatched, "length mismatch must return original bytes");

        // (c) A predictor value lopdf rejects/ignores (99).
        let bad_pred = build_flate_pdf_with_dict(400, 3, |_, d| {
            d.set(
                "DecodeParms",
                dictionary! { "Predictor" => 99_i64, "Colors" => 3_i64, "Columns" => 400_i64 },
            );
        });
        let out = optimize(&bad_pred);
        assert_eq!(out, bad_pred, "unknown predictor must return original bytes");
    }

    #[test]
    fn flate_optimize_is_idempotent() {
        // Characterization: a second pass over already-optimized output must
        // be byte-stable — the downsampled image sits at the target DPI
        // (inside the margin), so no further work is planned and the fail-safe
        // path hands back the input unchanged.
        let pdf = build_pdf_flate(400, 100, Some(2));
        let once = optimize(&pdf);
        assert!(once.len() < pdf.len(), "first pass must shrink");
        let twice = optimize(&once);
        assert_eq!(twice, once, "second pass must be byte-stable");
    }

    #[test]
    fn flate_under_resolution_is_untouched() {
        // 120px drawn into 100pt => ~86 DPI, below target: exact original bytes.
        let pdf = build_pdf_flate(120, 100, None);
        let out = optimize(&pdf);
        assert_eq!(out, pdf, "under-resolution Flate image must be untouched");
    }

    #[test]
    fn downsample_flate_images_off_leaves_flate_untouched() {
        let pdf = build_pdf_flate(400, 100, Some(2));
        let opts = OptimizeOptions::default().with_downsample_flate_images(false);
        let out = optimize_with_options(&pdf, opts);
        assert_eq!(out, pdf, "flag off must leave Flate images untouched");
    }

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

    #[test]
    fn downsamples_over_resolution_image() {
        // 400px drawn into 100pt box => ~288 DPI, well above the 130 target.
        let pdf = build_pdf(400, 100);
        let out = optimize(&pdf);

        assert!(out.len() < pdf.len(), "expected smaller output");
        let (w, h) = image_dims(&out);
        // Target ≈ 100/72 * 130 ≈ 180px.
        assert!(w < 400 && w > 120, "unexpected downsampled width: {w}");
        assert_eq!(w, h, "aspect ratio should be preserved");
        // Output must still be a loadable PDF.
        assert!(Document::load_mem(&out).is_ok());
    }

    /// Pull the (single) image stream's JPEG bytes out of a PDF.
    fn image_stream_bytes(pdf: &[u8]) -> Vec<u8> {
        let doc = Document::load_mem(pdf).unwrap();
        for obj in doc.objects.values() {
            if let Object::Stream(s) = obj {
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
                    return s.content.clone();
                }
            }
        }
        panic!("no image stream");
    }

    #[test]
    fn non_uniform_placement_is_downsampled() {
        // 1000x1000 px drawn into a 500x100 pt box:
        //   horizontal effective DPI = 1000 / (500/72) = 144  (under 130*1.15)
        //   vertical   effective DPI = 1000 / (100/72) = 720  (~6x over target)
        // Testing width alone skipped this image entirely. Both axes must be
        // considered, so the vertically over-resolved image gets downsampled.
        let pdf = build_pdf_placed(1000, 500, 100);
        let out = optimize(&pdf);

        let (w, h) = image_dims(&out);
        assert!(
            (w, h) != (1000, 1000),
            "vertically over-resolved image must not be skipped"
        );
        // Target is sized per axis: 500pt -> ~903px wide, 100pt -> ~181px tall.
        assert!((850..=950).contains(&w), "unexpected width: {w}");
        assert!((150..=210).contains(&h), "unexpected height: {h}");
        assert!(out.len() < pdf.len(), "output must be smaller");
        assert!(Document::load_mem(&out).is_ok(), "output must still load");
    }

    #[test]
    fn uniformly_low_resolution_image_still_skipped() {
        // Guard the other direction: considering both axes must not cause
        // already-adequate images to be churned.
        let pdf = build_pdf_placed(120, 100, 100);
        let out = optimize(&pdf);
        assert_eq!(image_dims(&out), (120, 120), "must be left untouched");
    }

    #[test]
    fn scaled_decode_matches_full_decode_pixels() {
        // The scaled-decode fast path must be visually equivalent to the old
        // full-decode-then-resize path, not merely the right dimensions.
        let pdf = build_pdf(800, 100);
        let out = optimize(&pdf);
        let produced = image::load_from_memory(&image_stream_bytes(&out))
            .unwrap()
            .to_rgb8();

        let src = image::load_from_memory(&image_stream_bytes(&pdf)).unwrap();
        let reference = src
            .resize_exact(
                produced.width(),
                produced.height(),
                image::imageops::FilterType::Lanczos3,
            )
            .to_rgb8();

        assert_eq!(produced.dimensions(), reference.dimensions());
        let sad: f64 = produced
            .as_raw()
            .iter()
            .zip(reference.as_raw().iter())
            .map(|(a, b)| (*a as f64 - *b as f64).abs())
            .sum();
        let mad = sad / produced.as_raw().len() as f64;
        assert!(mad < 12.0, "scaled decode diverges from full decode: MAD={mad}");
    }

    #[test]
    fn scaled_decode_never_undershoots_target() {
        // Decoding must always cover the target so the final Lanczos3 step
        // downsamples; undershooting would silently upscale and blur.
        let mut img = image::RgbImage::new(4000, 4000);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([(x % 256) as u8, (y % 256) as u8, 0]);
        }
        let mut jpeg: Vec<u8> = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85)
            .encode_image(&DynamicImage::ImageRgb8(img))
            .unwrap();
        for target in [180u32, 500, 1000, 2500, 3999] {
            let (d, _) = decode_jpeg_scaled(&jpeg, target, target).unwrap();
            assert!(
                d.width() >= target && d.height() >= target,
                "target {target}: decoded {}x{} undershoots",
                d.width(),
                d.height()
            );
        }
    }

    #[test]
    fn grayscale_jpeg_round_trips_as_grayscale() {
        // A true JCS_GRAYSCALE JPEG must decode back as 1-channel, so the
        // re-encode matches a DeviceGray /ColorSpace. Encoding 3-channel data
        // into a DeviceGray stream would corrupt the image.
        //
        // NOTE: build the fixture with amatl's own encoder. image's JpegEncoder
        // writes a Luma8 buffer as a 3-component YCbCr JPEG, which is NOT a
        // grayscale JPEG and would not exercise this path.
        let mut gray = image::GrayImage::new(600, 600);
        for (x, y, p) in gray.enumerate_pixels_mut() {
            *p = image::Luma([((x + y) % 256) as u8]);
        }
        let jpeg = encode_jpeg(DynamicImage::ImageLuma8(gray), true, 90).unwrap();

        let (decoded, is_gray) = decode_jpeg_scaled(&jpeg, 100, 100).expect("should decode");
        assert!(is_gray, "true grayscale JPEG must report is_gray");
        assert!(
            matches!(decoded, DynamicImage::ImageLuma8(_)),
            "must decode as single-channel Luma8"
        );
        assert!(decoded.width() >= 100 && decoded.height() >= 100);
    }

    #[test]
    fn duplicate_image_streams_are_merged() {
        // Eight byte-identical images must collapse to ONE stream: decoded and
        // re-encoded once instead of eight times, and stored once. Before
        // dedup_streams this produced 8 separate (identical) image streams.
        let pdf = build_pdf_duplicate_images(8, 400);
        assert_eq!(count_image_streams(&pdf), 8, "fixture should start with 8");

        let out = optimize(&pdf);
        assert_eq!(
            count_image_streams(&out),
            1,
            "identical images must be merged into a single stream"
        );
        assert!(out.len() < pdf.len(), "output must be smaller");
        assert!(Document::load_mem(&out).is_ok(), "output must still load");
    }

    #[test]
    fn distinct_image_streams_are_not_merged() {
        // Guard against over-merging: differing bytes must never collapse.
        let mut doc = Document::load_mem(&build_pdf_duplicate_images(2, 400)).unwrap();
        let ids: Vec<ObjectId> = doc
            .objects
            .iter()
            .filter(|(_, o)| {
                matches!(o, Object::Stream(s)
                    if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image"))
            })
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(ids.len(), 2);
        if let Ok(Object::Stream(s)) = doc.get_object_mut(ids[1]) {
            s.content.push(0x00);
        }
        assert!(!dedup_streams(&mut doc), "differing bytes must not merge");
    }

    #[test]
    fn default_options_match_documented_sweet_spot() {
        // Pins the manual Default impl: adding numeric fields must NOT regress
        // the measured 130 DPI / Q78 / 1.15 sweet spot downstream consumers
        // depend on. A derived Default would zero these and collapse to ~1px.
        let d = OptimizeOptions::default();
        assert_eq!(d.target_dpi, 130.0);
        assert_eq!(d.jpeg_quality, 78);
        assert_eq!(d.dpi_margin, 1.15);
        assert!(!d.strip_accessibility);
        assert!(!d.pack_object_streams);
        assert!(d.downsample_flate_images, "Flate downsampling is default-ON (0.2.0)");
    }

    #[test]
    fn builder_methods_set_each_field() {
        let o = OptimizeOptions::default()
            .with_target_dpi(96.0)
            .with_jpeg_quality(60)
            .with_dpi_margin(1.5)
            .with_strip_accessibility(true)
            .with_pack_object_streams(true)
            .with_downsample_flate_images(false);
        assert_eq!(o.target_dpi, 96.0);
        assert_eq!(o.jpeg_quality, 60);
        assert_eq!(o.dpi_margin, 1.5);
        assert!(o.strip_accessibility);
        assert!(o.pack_object_streams);
        assert!(!o.downsample_flate_images);
    }

    #[test]
    fn custom_target_dpi_downsamples_more_aggressively() {
        // Same input, lower target DPI => smaller downsampled pixel dimensions.
        // 400px drawn into a 100pt box is ~288 DPI, above both targets.
        let pdf = build_pdf(400, 100);

        let at_130 = optimize_with_options(&pdf, OptimizeOptions::default());
        let opts_72 = OptimizeOptions::default().with_target_dpi(72.0);
        let at_72 = optimize_with_options(&pdf, opts_72);

        let (w130, _) = image_dims(&at_130);
        let (w72, _) = image_dims(&at_72);
        assert!(w72 < w130, "lower target DPI must yield fewer pixels: {w72} !< {w130}");
        // 100pt / 72 * 72 DPI = 100px target.
        assert!((90..=110).contains(&w72), "unexpected 72-DPI width: {w72}");
    }

    #[test]
    fn zero_target_dpi_leaves_images_untouched() {
        // Defensive-clamp regression: target_dpi <= 0 must mean "no
        // downsampling", NOT "downsample to ~1px".
        let pdf = build_pdf(400, 100);
        let opts = OptimizeOptions::default().with_target_dpi(0.0);
        let out = optimize_with_options(&pdf, opts);

        // No image work and no strip => fail-safe path returns the original bytes.
        let (w, h) = image_dims(&out);
        assert_eq!((w, h), (400, 400), "zero target DPI must not resize the image");
    }

    #[test]
    fn leaves_low_resolution_image_untouched() {
        // 120px drawn into 100pt box => ~86 DPI, below target: no change.
        let pdf = build_pdf(120, 100);
        let out = optimize(&pdf);

        let (w, h) = image_dims(&out);
        assert_eq!((w, h), (120, 120), "low-res image must not be resized");
    }

    #[test]
    fn invalid_pdf_falls_back_to_original() {
        let garbage = b"this is not a pdf at all";
        let out = optimize(garbage);
        assert_eq!(out, garbage, "must return original bytes on failure");
    }

    /// Fail-safe contract regression: a crafted PDF that parses but contains
    /// a malformed JPEG stream must not abort the process. Before the
    /// `catch_unwind` wrapper in `optimize_with_options`, this could panic in
    /// the image decoder; the wrapper turns any panic into the same graceful
    /// fallback as a parse error. This pins that regression so the panic
    /// boundary can't silently disappear.
    #[test]
    fn crafted_pdf_panic_is_caught_not_unwound() {
        let pdf = build_pdf(400, 100);
        // Reload, swap the image stream for bytes that will decode-fail in a
        // way that historically panicked past the `?` operators in
        // plan_replacement. The fail-safe contract is byte-equality with input.
        let mut doc = Document::load_mem(&pdf).unwrap();
        for obj in doc.objects.values_mut() {
            if let Object::Stream(s) = obj {
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
                    // Valid DCTDecode header bytes but truncated body: the JPEG
                    // decoder will error, not panic. The point is that even if
                    // it DID panic (as mozjpeg has done on some crafted input),
                    // the wrapper would catch it and return the original bytes.
                    s.set_content(b"\xff\xd8\xff\xe0".to_vec());
                }
            }
        }
        let mut input: Vec<u8> = Vec::new();
        doc.save_to(&mut input).unwrap();

        let out = optimize(&input);
        assert_eq!(
            out, input,
            "panic-causing input must return original bytes, not abort"
        );
    }

    /// Fail-safe contract regression: empty and near-empty inputs must not
    /// panic on index/slice operations. The library must be safe to call with
    /// any `&[u8]`, including the degenerate cases a fuzzer would find first.
    #[test]
    fn degenerate_inputs_do_not_panic() {
        for input in [&b""[..], &[0u8], b"%", b"%P", b"%PDF"] {
            let out = optimize(input);
            assert_eq!(
                out, input,
                "degenerate input ({:?}) must pass through unchanged",
                input
            );
        }
    }

    #[test]
    fn default_options_preserve_accessibility() {
        // Default options must NOT strip the structure tree even when present.
        let pdf = build_pdf(400, 100); // has no StructTreeRoot, but verify options work
        let out = optimize_with_options(&pdf, OptimizeOptions::default());
        assert!(out.len() < pdf.len(), "expected shrink from downsampling");
        assert!(Document::load_mem(&out).is_ok());
    }

    #[test]
    fn strip_accessibility_runs_even_without_image_work() {
        // A doc with no over-resolution images would normally be a no-op, but
        // strip_accessibility should still produce (smaller) output. Build a
        // tiny PDF with a low-res image and an explicit StructTreeRoot entry.
        let pdf = build_pdf(80, 100); // 80px @ 100pt ≈ 58 DPI, won't downsample
        // Inject a fake structure tree so stripping has something to remove.
        // We reload, add the entries, re-save, then run the optimizer.
        let mut doc = Document::load_mem(&pdf).unwrap();
        let struct_id = doc.add_object(dictionary! {
            "Type" => "StructTreeRoot",
            "RoleMap" => dictionary!{},
        });
        if let Ok(catalog) = doc.catalog_mut() {
            catalog.set("StructTreeRoot", Object::Reference(struct_id));
            catalog.set("MarkInfo", dictionary! { "Marked" => true });
        }
        let mut reencoded: Vec<u8> = Vec::new();
        doc.save_to(&mut reencoded).unwrap();

        let opts = OptimizeOptions::default().with_strip_accessibility(true);
        let out = optimize_with_options(&reencoded, opts);
        assert!(
            out.len() < reencoded.len(),
            "strip path must produce smaller output even with no image work"
        );
        let out_doc = Document::load_mem(&out).expect("stripped output must load");
        let catalog = out_doc.catalog().expect("catalog present");
        assert!(
            catalog.get(b"StructTreeRoot").is_err(),
            "StructTreeRoot must be removed"
        );
        assert!(
            catalog.get(b"MarkInfo").is_err(),
            "MarkInfo must be removed"
        );
    }

    #[test]
    fn dedup_merges_identical_objects() {
        // Two structurally identical dictionaries plus an object referencing both.
        // dedup must collapse them to one and redirect both references to it.
        let mut doc = Document::with_version("1.5");
        let a = doc.add_object(dictionary! { "Type" => "ExtGState", "ca" => 1 });
        let b = doc.add_object(dictionary! { "Type" => "ExtGState", "ca" => 1 });
        let holder = doc.add_object(dictionary! { "First" => a, "Second" => b });

        let before = doc.objects.len();
        dedup_objects(&mut doc);

        assert_eq!(
            doc.objects.len(),
            before - 1,
            "exactly one duplicate object should be removed"
        );
        let dict = doc.get_object(holder).unwrap().as_dict().unwrap();
        let first = dict.get(b"First").unwrap();
        let second = dict.get(b"Second").unwrap();
        assert_eq!(
            first, second,
            "both references must point at the single surviving object"
        );
    }

    #[test]
    fn dedup_keeps_distinct_objects() {
        // Same shape, but the two dictionaries differ by one value. They must NOT
        // be merged: distinct content stays distinct.
        let mut doc = Document::with_version("1.5");
        let a = doc.add_object(dictionary! { "Type" => "ExtGState", "ca" => 1 });
        let b = doc.add_object(dictionary! { "Type" => "ExtGState", "ca" => 2 });
        let holder = doc.add_object(dictionary! { "First" => a, "Second" => b });

        let before = doc.objects.len();
        dedup_objects(&mut doc);

        assert_eq!(doc.objects.len(), before, "no object should be removed");
        let dict = doc.get_object(holder).unwrap().as_dict().unwrap();
        let first = dict.get(b"First").unwrap();
        let second = dict.get(b"Second").unwrap();
        assert_ne!(first, second, "distinct objects must keep distinct references");
    }

    /// Real-file check. Defaults to the committed fixture
    /// (`fixtures/sample.pdf`, regenerable via `tests/generate_fixture.rs`);
    /// set AMATL_TEST_PDF to run against another PDF instead. Uses a typical
    /// size-focused configuration (strip the accessibility tree). Asserts the
    /// output is smaller and remains a valid, loadable PDF.
    #[test]
    fn real_file_shrinks_when_present() {
        let path = std::env::var("AMATL_TEST_PDF").unwrap_or_else(|_| {
            concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/sample.pdf").to_string()
        });
        let input = std::fs::read(&path).expect("failed to read real-file test input");
        // Opt in to object-stream packing for this run via AMATL_TEST_PACK=1.
        let opts = OptimizeOptions::default()
            .with_strip_accessibility(true)
            .with_pack_object_streams(std::env::var("AMATL_TEST_PACK").is_ok());
        let out = optimize_with_options(&input, opts);
        println!(
            "{path}: {} -> {} bytes ({}%)",
            input.len(),
            out.len(),
            out.len() * 100 / input.len()
        );
        assert!(out.len() < input.len(), "expected real file to shrink");
        assert!(Document::load_mem(&out).is_ok(), "output must be a valid PDF");
        if let Ok(dest) = std::env::var("AMATL_TEST_OUT") {
            std::fs::write(&dest, &out).unwrap();
        }
    }

    #[test]
    fn corrupt_jpeg_stream_falls_back_without_crashing() {
        // Structurally valid PDF, but the image's "JPEG" bytes are garbage. The
        // effective DPI (400px drawn into a 100pt box ≈ 288 DPI) is above target,
        // so plan_replacement attempts to decode — and must fail gracefully,
        // leaving the document untouched and returning the original bytes.
        let mut doc = Document::with_version("1.5");
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 400_i64,
                "Height" => 400_i64,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
                "Filter" => "DCTDecode",
            },
            b"\xff\xd8\xff not a real jpeg payload".to_vec(),
        ));
        let content = Content {
            operations: vec![
                Operation::new("q", vec![]),
                Operation::new(
                    "cm",
                    vec![100.into(), 0.into(), 0.into(), 100.into(), 0.into(), 0.into()],
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
            "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => img_id } },
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut input: Vec<u8> = Vec::new();
        doc.save_to(&mut input).unwrap();

        let out = optimize(&input);
        assert_eq!(out, input, "corrupt image must leave the document unchanged");
        assert!(Document::load_mem(&out).is_ok(), "output must remain a valid PDF");
    }
}





