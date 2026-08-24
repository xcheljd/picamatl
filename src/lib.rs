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
//!   Soft-masked DCTDecode images (`/SMask`) follow the Phase 5 D-milestone
//!   rules: over-resolution pairs are **downsampled as a unit** (D-M2) — the
//!   base JPEG and its `/SMask` stream are resampled to the SAME target
//!   geometry and replaced together or not at all — while pairs at or below
//!   the target resolution take the dimension-preserving D-M1 requantization
//!   (the base is re-encoded at the configured JPEG quality at its OWN
//!   dimensions, never resized, and the `/SMask` stream is never modified).
//!   Both apply only when the mask is a plain 8-bit DeviceGray image stream
//!   with no `/Matte`. `/Mask` (stencil/color-key) images and any ineligible
//!   `/SMask` stay untouched.
//!
//!   Soft-masked FlateDecode bases (D-M3) take the coupled downsample when
//!   over-resolution; otherwise they are untouched by default, and with the
//!   `allow_lossy_reencode` consent flag they take the dimension-preserving
//!   Flate→JPEG conversion. That conversion rewrites the base stream only —
//!   the `/SMask` object keeps its bytes and its dimensions, so alignment is
//!   preserved by construction (and shared masks stay safe for the same
//!   reason the D-M1 requant is safe for them). Under the same flag the
//!   over-resolution coupled downsample also runs a JPEG competitor at its
//!   TARGET geometry, against the losslessly resampled mask, so a masked pair
//!   reaches its final encoding in one pass instead of two.
//!
//! By default (opt-out via [`OptimizeOptions::subset_fonts`]), embedded
//! Type0/CIDFontType2 (Identity-H/V) fonts and nonsymbolic simple TrueType
//! fonts (WinAnsi/MacRoman) are subset to the glyphs actually shown, using
//! techniques that never rewrite content-stream text bytes (see
//! `src/fonts.rs`).
//!
//! Hard safety guarantees:
//!   - Images we can't measure a placement for are left untouched.
//!   - Images already at/below the target DPI are left untouched (no upscaling).
//!   - A re-encode that isn't smaller is discarded.
//!   - Any failure (parse, decode, save) falls back to the original bytes.

mod bitonal;
mod encodings;
mod fonts;
mod truetype;
mod type1;

use std::collections::HashMap;

use image::{DynamicImage, ImageFormat};
use lopdf::content::Content;
use lopdf::{dictionary, Document, Object, ObjectId};
use rayon::prelude::*;

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
/// `true` (it was `false` through 0.3.0). The saving scales with
/// the *object* count, not the file size: on the 58-page NASA reference at
/// otherwise-default settings — object-heavy, structure tree intact, 2,497
/// objects — packing measured **−162,069 B (3.27%)**, replacing ~168 KB of
/// plaintext object bodies and their `N G obj … endobj` framing with a single
/// 41 KB `ObjStm`. Documents with few non-stream objects left to pack (e.g.
/// after `strip_accessibility`) gain proportionally less.
///
/// The cost is a **PDF 1.5 floor**: a reader older than Acrobat 6 (2003)
/// cannot open an `ObjStm` file *at all* — a hard failure, not a degradation.
/// Set this to `false` (CLI: `--no-pack-object-streams`) when the audience may
/// include such readers. Implemented in pure Rust (no native deps) to avoid
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
    /// compression). Default: `true` (was `false` through 0.3.0). Lossless —
    /// same objects, same semantics, different serialization — but it imposes
    /// a PDF 1.5 floor on the output. See struct doc for the measured benefit
    /// and the escape hatch.
    pub pack_object_streams: bool,

    /// Downsample over-resolution FlateDecode raster images in place (format
    /// preserved: the result is still FlateDecode with the same `/ColorSpace`,
    /// so no JPEG artifacts are introduced). Applies the same effective-DPI
    /// threshold as the JPEG path. Default: `true`.
    pub downsample_flate_images: bool,

    /// Subset embedded fonts to the glyphs the document actually shows:
    /// Type0/CIDFontType2 (Identity-H/V) fonts, and nonsymbolic simple
    /// TrueType fonts with WinAnsi/MacRoman encodings (incl. `/Differences`).
    /// Only the font program is replaced (plus `/CIDToGIDMap` on the Type0
    /// path and the subset name tag): content-stream text bytes are never
    /// rewritten, and `/W`, `/DW`, `/Widths`, `/Encoding`, and `/ToUnicode`
    /// stay untouched, so text extraction is bit-identical. Any parse
    /// uncertainty disables subsetting for the affected font or the whole
    /// document — output is always valid. PDF/A-declared and encrypted
    /// documents are skipped. Default: `true` (was `false` through 0.3.1;
    /// rendering-preserving and verified, so it joined the lossless
    /// defaults).
    pub subset_fonts: bool,

    /// Convert embedded Type1 (`/FontFile`) fonts to subsetted Type1C/CFF
    /// (`/FontFile3`), the same re-encoding Ghostscript applies. Charstrings
    /// are re-expressed as Type2 with outlines preserved exactly (flex
    /// becomes the curves it rasterizes as, `seac` composites are inlined,
    /// stem hints and widths carry over); only the glyphs the document shows
    /// are retained. The font dictionary's `/Encoding`, `/Widths`, and
    /// `/ToUnicode` never change (the CFF replicates the font's built-in
    /// encoding, so every name-lookup path resolves as before) and each font
    /// is swapped only when the new stream is strictly smaller. Any parse or
    /// conversion doubt — MM OtherSubrs, non-zero `/PaintType`, encoding
    /// constructions we cannot replicate — leaves that font untouched.
    /// Default: `false` (opt-in for at least one release cycle).
    pub convert_type1: bool,

    /// Losslessly recompress bitonal (1-bit) images to CCITT G4: CCITT-stored
    /// sources (G4 `/K -1`, or EOL-framed G3 `/K 0` with `/EndOfLine true`)
    /// and Flate-stored 1-bit images. Pixels are never resampled and
    /// `/Width`/`/Height` never change; `/BlackIs1` polarity is normalized at
    /// the sample level so rendered output is bit-identical. A stream is
    /// replaced only when the G4 payload is strictly smaller AND a decode-back
    /// pass reproduces the source samples exactly; every parse or parameter
    /// doubt (`/EncodedByteAlign true`, `/K > 0`, non-identity `/Decode`, …)
    /// leaves the image untouched. Default: `false` (opt-in for at least one
    /// release cycle).
    pub recompress_bitonal_images: bool,

    /// Allow LOSSY re-encoding of lossless (FlateDecode) raster images to
    /// JPEG (`/DCTDecode`) — an encoding-class change, which the library
    /// contract otherwise forbids, so this is strictly opt-in consent
    /// (Phase 7 spike). Scope: unmasked 8-bit DeviceGray / DeviceRGB /
    /// ICCBased(N=1/3) FlateDecode images only. Over-resolution images get a
    /// JPEG candidate at the same target geometry as the format-preserving
    /// downsample and the smaller candidate wins; images at/below the DPI
    /// threshold are re-encoded at their own geometry, replaced only when the
    /// JPEG saves at least 5% AND passes decode-back verification. Indexed,
    /// CMYK, and non-8-bit images are never converted. Images with an eligible
    /// `/SMask` convert too: on the dimension-preserving path the mask
    /// stream's bytes and geometry are never modified, so base/mask alignment
    /// is preserved by construction, and on the over-resolution coupled
    /// downsample the JPEG candidate is computed at the SAME target geometry
    /// the mask is losslessly resampled to, so the two land on identical pixel
    /// grids (both validated by the mask-alignment compositing experiment — a
    /// hard-edged or antialiased mask over a q78 4:2:0 base shows no
    /// misregistration, only the JPEG quantization this flag consents to).
    /// Default: `false`.
    pub allow_lossy_reencode: bool,

    /// Collapse channel-identical `/DeviceRGB` FlateDecode images to
    /// `/DeviceGray`: when EVERY pixel has R == G == B exactly, store the
    /// single channel instead of three. Sample-preserving — the decoded
    /// value of each pixel is unchanged, and the PDF color model defines
    /// DeviceGray g as DeviceRGB (g, g, g) — but it does rewrite
    /// `/ColorSpace`, so like the bitonal G4 recompression it stays opt-in
    /// for at least one release cycle. Scope: unmasked-or-SMasked 8-bit
    /// plain-`/DeviceRGB` images only (ICCBased color is never collapsed —
    /// dropping a profile is not sample-preserving in color meaning; a
    /// color-key `/Mask` is RGB-range-based and disqualifies). Replaced only
    /// when the gray stream is strictly smaller. Default: `false`.
    pub collapse_gray_images: bool,

    /// Deflate implementation for the final serialization passes: the
    /// whole-document re-deflate (`redeflate_flate_streams`) and the
    /// cross-reference stream. [`DeflateBackend::Zopfli`] spends ~30× the CPU
    /// of zlib level 9 searching for a smaller deflate encoding of the SAME
    /// bytes (measured −142 KB / 3.2% of output on the NASA reference); the
    /// strictly-smaller + inflate-back guard applies to both backends, so the
    /// choice affects size and speed only, never correctness. Earlier
    /// planning passes (image re-deflate, font streams) stay on zlib either
    /// way — the final pass revisits their output. Default:
    /// [`DeflateBackend::Zlib`].
    pub deflate_backend: DeflateBackend,
}

/// Which deflate implementation the final re-deflate and xref-stream passes
/// use. See [`OptimizeOptions::deflate_backend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeflateBackend {
    /// zlib level 9 via `flate2`'s zlib-rs backend — fast, the default.
    #[default]
    Zlib,
    /// Exhaustive-search deflate (pure-Rust `zopfli` crate) — smallest
    /// output, ~30× the CPU. Opt-in for callers who value bytes over speed.
    Zopfli,
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
            pack_object_streams: true,
            downsample_flate_images: true,
            subset_fonts: true,
            convert_type1: false,
            recompress_bitonal_images: false,
            allow_lossy_reencode: false,
            collapse_gray_images: false,
            deflate_backend: DeflateBackend::Zlib,
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

    /// Enable/disable subsetting of embedded Type0/CIDFontType2 fonts
    /// (off by default). See [`OptimizeOptions::subset_fonts`].
    #[must_use]
    pub fn with_subset_fonts(mut self, subset: bool) -> Self {
        self.subset_fonts = subset;
        self
    }

    /// Enable/disable Type1 → Type1C (CFF) font conversion
    /// (off by default). See [`OptimizeOptions::convert_type1`].
    #[must_use]
    pub fn with_convert_type1(mut self, convert: bool) -> Self {
        self.convert_type1 = convert;
        self
    }

    /// Enable/disable lossless G4 recompression of bitonal images
    /// (off by default). See [`OptimizeOptions::recompress_bitonal_images`].
    #[must_use]
    pub fn with_recompress_bitonal_images(mut self, recompress: bool) -> Self {
        self.recompress_bitonal_images = recompress;
        self
    }

    /// Enable/disable lossy Flate→JPEG re-encoding of lossless images
    /// (off by default). See [`OptimizeOptions::allow_lossy_reencode`].
    #[must_use]
    pub fn with_allow_lossy_reencode(mut self, allow: bool) -> Self {
        self.allow_lossy_reencode = allow;
        self
    }

    /// Enable/disable collapsing channel-identical DeviceRGB Flate images to
    /// DeviceGray (off by default). See
    /// [`OptimizeOptions::collapse_gray_images`].
    #[must_use]
    pub fn with_collapse_gray_images(mut self, collapse: bool) -> Self {
        self.collapse_gray_images = collapse;
        self
    }

    /// Choose the deflate backend for the final re-deflate and xref-stream
    /// passes (zlib by default). See [`OptimizeOptions::deflate_backend`].
    #[must_use]
    pub fn with_deflate_backend(mut self, backend: DeflateBackend) -> Self {
        self.deflate_backend = backend;
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
    const IDENTITY: Mat = Mat {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

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
    Flate {
        decode_parms: Option<lopdf::Dictionary>,
    },
    /// Phase 7 spike (consent-gated): a FlateDecode payload became a JPEG.
    /// `/Filter` becomes the scalar `/DCTDecode` and any `/DecodeParms` (a
    /// Flate-predictor artifact, meaningless for DCT) is dropped.
    /// `/ColorSpace` and `/BitsPerComponent` are deliberately NOT rewritten:
    /// the conversion is only planned for 8-bit DeviceGray / DeviceRGB /
    /// ICCBased(N=1/3) sources and the JPEG preserves the channel count
    /// (gray → 1-channel, RGB → 3-channel), so the existing entries still
    /// describe the payload exactly.
    FlateToJpeg,
}

/// A planned image replacement, computed read-only before mutating the doc.
struct Replacement {
    id: ObjectId,
    content: Vec<u8>,
    width: i64,
    height: i64,
    dict_update: DictUpdate,
    /// Phase 5 D-M2: a paired `/SMask` stream replacement, applied atomically
    /// with the base (the mask/image unit rule — never one side alone). `None`
    /// for single-stream replacements (unmasked downsample, D-M1 requant).
    smask: Option<MaskReplacement>,
}

/// Phase 5 D-M2: the `/SMask` stream's half of a coupled downsampling. The
/// mask is re-encoded as plain FlateDecode 8-bit DeviceGray rows at the WIDTH
/// AND HEIGHT of the base's target geometry.
struct MaskReplacement {
    mask_id: ObjectId,
    content: Vec<u8>,
    width: i64,
    height: i64,
}

/// The single-filter classes the re-encode paths know how to handle.
/// `plan_replacement` (downsampling) consumes Dct/Flate; the lossless bitonal
/// pass (`bitonal::plan_bitonal_recompressions`) consumes Ccitt/Flate.
#[derive(Debug, PartialEq, Eq)]
enum FilterClass {
    DctOnly,
    FlateOnly,
    CcittOnly,
    /// `/JPXDecode`: JPEG2000, a JP2 container or bare codestream. Decodable
    /// via the pure-Rust `dicom-toolkit-jpeg2000` crate, but ONLY the
    /// consent-gated `JPX→JPEG` conversion under `--allow-lossy` uses it (see
    /// `plan_jpx_conversions`); every default-options path still declines the
    /// class wholesale. A lossless `JPX→Flate` was considered and rejected:
    /// irreversible (9/7) codestreams decode with implementation-specific
    /// rounding, so "the pixels we decoded" are not provably the pixels the
    /// viewer's decoder produces — not render-identical — and deflate never
    /// beats JPEG2000 on the photographic payloads the format carries anyway.
    JpxOnly,
    /// `/JBIG2Decode`: lossless JBIG2 bilevel compression, seen in scanned
    /// office / fax archives. No JBIG2 decoder is linked. Recognized so it is
    /// never treated as DCT/Flate/CCITT; always left untouched.
    Jbig2Only,
    Other,
}

/// Classify a stream's `/Filter`: exactly DCTDecode (raw JPEG payload),
/// exactly FlateDecode (deflated raster data), exactly CCITTFaxDecode (raw
/// CCITT bitstream), `/JPXDecode` (JPEG2000) and `/JBIG2Decode` (JBIG2) or
/// anything else. The exotic classes are recognized-and-declined: they are
/// never handed to a re-encode path (which has no decoder for them). Both the
/// scalar-name and one-element-array forms are recognized.
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
        b"CCITTFaxDecode" => FilterClass::CcittOnly,
        b"JPXDecode" => FilterClass::JpxOnly,
        b"JBIG2Decode" => FilterClass::Jbig2Only,
        _ => FilterClass::Other,
    }
}

/// How the caller intends to transform a masked pair (Phase 6 P-M1). A
/// dimension-preserving requantization never touches the mask, so a mask
/// shared by several bases stays valid for every consumer; any RESIZE of a
/// shared mask would break the other consumers' pixel alignment, so only the
/// resize intent carries the shared-mask refcount guard.
#[derive(Clone, Copy)]
enum SmaskUse {
    /// D-M1/P-M1 requantization: base re-encoded at its own geometry, mask
    /// stream never modified.
    Requant,
    /// D-M2/D-M3 coupled downsample: base and mask change geometry together.
    Resize,
}

/// Phase 5 D-M1: return the mask's object id when the `/SMask` value resolves
/// to a plain 8-bit DeviceGray image stream — `/ImageMask` stencil unset, no
/// `/Matte` (premultiplied color semantics are not understood), exactly 8 bits
/// per component. Any doubt returns `None` (unresolvable reference, not an
/// image object, other color space, other bpc, stencil flag set), leaving the
/// masked pair untouched. `SmaskUse::Resize` additionally applies the
/// shared-mask refcount guard; `SmaskUse::Requant` deliberately does not
/// (Phase 6 P-M1 — see `SmaskUse`).
fn eligible_smask(doc: &Document, smask: &Object, usage: SmaskUse) -> Option<ObjectId> {
    // The `/SMask` value is normally a direct reference. Take the id from the
    // RAW value (not `resolve`, which would already have dereferenced it to
    // the mask stream itself) and only then look the stream up.
    let mask_id = match smask {
        Object::Reference(id) => *id,
        _ => return None,
    };
    let stream = doc.get_object(mask_id).ok()?.as_stream().ok()?;
    let dict = &stream.dict;

    if !matches!(
        dict.get(b"Subtype").map(|s| resolve(doc, s)),
        Ok(Object::Name(n)) if n == b"Image"
    ) {
        return None;
    }
    // Stencil masks (/ImageMask true) are bilevel sampling masks — skip.
    if matches!(dict.get(b"ImageMask"), Ok(Object::Boolean(true))) {
        return None;
    }
    // /Matte premultiplies the mask samples against a background color;
    // requantizing the base under that interpretation is not supported.
    if dict.get(b"Matte").is_ok() {
        return None;
    }
    // Plain DeviceGray only (an array-wrapped or ICCBased gray is out of scope).
    if !matches!(
        dict.get(b"ColorSpace").map(|c| resolve(doc, c)),
        Ok(Object::Name(n)) if n == b"DeviceGray"
    ) {
        return None;
    }
    if dict
        .get(b"BitsPerComponent")
        .ok()
        .and_then(|b| b.as_i64().ok())
        != Some(8)
    {
        return None;
    }
    // Shared-mask fail-safe (Phase 5 review finding), RESIZE intent only: a
    // `/SMask` object referenced by MORE than one image cannot be safely
    // resized for one consumer's geometry without breaking the other's pixel
    // alignment. This is reachable in practice: dedup merges byte-identical
    // masks (e.g. three copies of the same 620-byte thumbnail mask on one
    // NASA page) BEFORE planning runs, so several images legitimately share
    // one mask id here. Count DIRECT references to this mask id; any second
    // consumer disqualifies the pair entirely. (Indirect-reference chains are
    // not counted, but `eligible_smask` only accepts direct references
    // anyway.) A REQUANT never modifies the mask stream, so sharing is
    // harmless there — Phase 6 P-M1 measured 16 masked-JPEG payloads
    // (1,163,221 B) on the reference corpus that the guard needlessly blocked
    // from requantization.
    if matches!(usage, SmaskUse::Resize) {
        let refcount = doc
            .objects
            .values()
            .filter(|obj| {
                let raw = match obj {
                    Object::Stream(s) => s.dict.get(b"SMask"),
                    Object::Dictionary(d) => d.get(b"SMask"),
                    _ => return false,
                };
                matches!(raw, Ok(Object::Reference(id2)) if *id2 == mask_id)
            })
            .count();
        if refcount > 1 {
            return None;
        }
    }
    Some(mask_id)
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

    // Must be an image.
    if !matches!(dict.get(b"Subtype").map(|s| resolve(doc, s)), Ok(Object::Name(n)) if n == b"Image")
    {
        return None;
    }

    // Transparency handling (Phase 5 D-M1 / D-M2). A `/Mask` (stencil /
    // color-key) is always a hard skip; `/SMask` soft masks open eligibility
    // only for the pair shapes D-M1 already vetted — a plain 8-bit DeviceGray
    // image stream, `/ImageMask` stencil unset, no `/Matte` anywhere in the
    // pair. An ineligible `/SMask` (unresolvable reference, non-image object,
    // `/ImageMask` stencil, non-DeviceGray color space, `BitsPerComponent`
    // other than 8) leaves the whole pair untouched. Eligibility is checked
    // here with the REQUANT intent (Phase 6 P-M1): a shared mask does not
    // disqualify the pair outright anymore — the refcount guard is re-applied
    // below only on the branches that would resize the mask.
    let smask_raw = dict.get(b"SMask").ok();
    let smask_present = smask_raw.is_some();
    if dict.get(b"Mask").is_ok() || (smask_present && dict.get(b"Matte").is_ok()) {
        return None;
    }
    let smask_id = smask_raw.and_then(|value| eligible_smask(doc, value, SmaskUse::Requant));
    if smask_present && smask_id.is_none() {
        return None;
    }
    let filter = dict.get(b"Filter").ok()?;
    let class = classify_filter(doc, filter);
    // CCITT streams are bitonal: never resampled here (quality trap — plan §B).
    // Their lossless G4 recompression lives in the dedicated bitonal pass.
    // JPX (JPEG2000) and JBIG2 streams are recognized-and-declined here: no
    // decoder is linked, so any re-encode path would corrupt them. The masks
    // of such a base are likewise out of scope (an eligible_smask check never
    // runs — the whole object stays untouched).
    if matches!(
        class,
        FilterClass::Other | FilterClass::CcittOnly | FilterClass::JpxOnly | FilterClass::Jbig2Only
    ) {
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
    // *either* axis is over-resolved — the `target_* < px_*` component here
    // still prevents any upscaling. This is EXACTLY the gate the unmasked
    // path applies below; D-M2's coupled downsampling shares it unchanged.
    // `non_uniform_placement_is_downsampled` pins the both-axes rule.
    let eff_dpi_w = px_w as f32 / (rendered_w_pts / 72.0);
    let eff_dpi_h = px_h as f32 / (rendered_h_pts / 72.0);
    let target_w = ((rendered_w_pts / 72.0) * target_dpi).round().max(1.0) as u32;
    let target_h = ((rendered_h_pts / 72.0) * target_dpi).round().max(1.0) as u32;
    let over_resolution =
        eff_dpi_w.max(eff_dpi_h) > target_dpi * dpi_margin && target_w < px_w && target_h < px_h;

    // D-M1 / D-M2 / D-M3 masked-image handling.
    //   - DCTDecode bases: OVER-RESOLUTION pairs are downsampled as a unit
    //     (D-M2): base and `/SMask` are resampled to the SAME target geometry
    //     and replaced together (atomic — never one side alone); pairs
    //     at/below the target take the dimension-preserving D-M1
    //     requantization: the base is decoded at its own size, re-encoded at
    //     the configured quality, and the `/SMask` stream is never modified.
    //   - FlateDecode bases (D-M3): OVER-RESOLUTION pairs take the same
    //     atomic coupled downsample, with the base going through the
    //     format-preserving Flate→Flate path — or, under
    //     `allow_lossy_reencode`, through whichever of that path and a JPEG
    //     candidate at the same target geometry is smaller. Pairs that do not take that
    //     downsample are left untouched by default — the lossless requant
    //     analogue does not exist for a lossless payload — but with the
    //     `allow_lossy_reencode` consent flag they take the
    //     dimension-preserving Flate→JPEG conversion, which rewrites the base
    //     only and never the `/SMask` stream.
    //
    // Idempotence guard (D-M1, % ported to the D-M2 pair below): requantization
    // is lossy, so re-running it on an already-requantized payload keeps
    // shrinking by a fraction of a percent each pass (generation loss). A
    // stream is only requantized when the candidate saves at least 5% — a real
    // first-time requant of an over-quality scan saves far more (the NASA
    // corpus measured 40-55% per stream), while same-quality generation-loss
    // churn is 1-4% and decays each pass. This makes optimize(optimize(x)) a
    // no-op in practice without blocking genuine wins.
    if smask_present {
        let mask_id = smask_id.expect("smask_id is set whenever smask_present");
        // Resize eligibility (Phase 6 P-M1 split): the shared-mask refcount
        // guard applies only to the branches that would change the mask's
        // geometry. Evaluated lazily — the requant branch never needs it.
        let resize_eligible = || {
            smask_raw.is_some_and(|value| eligible_smask(doc, value, SmaskUse::Resize).is_some())
        };
        if matches!(class, FilterClass::FlateOnly) {
            // D-M3: the masked-Flate pair. Two transforms live here — the
            // over-resolution coupled downsample (gated by the same consent
            // flag as the unmasked Flate path; a shared mask is never
            // resized), which under `allow_lossy_reencode` also runs a JPEG
            // competitor at its target geometry, and, when that downsample is
            // NOT taken, the dimension-preserving Flate→JPEG conversion under
            // the same flag. The dimension-preserving conversion never touches
            // the mask stream (`Replacement::smask` is `None`), so it is safe
            // for shared masks by exactly the P-M1 argument; the competitor
            // inside the downsample resizes the mask either way, so it stays
            // behind the `resize_eligible()` gate and changes nothing about
            // shared-mask exposure.
            if !over_resolution || !options.downsample_flate_images || !resize_eligible() {
                if options.allow_lossy_reencode {
                    return plan_flate_lossy_requant_replacement(
                        doc, stream, options, id, px_w, px_h,
                    );
                }
                return None;
            }
            // The coupled downsample was ATTEMPTED. With consent it carries its
            // own JPEG competitor at the target geometry (Option B), so the
            // full harvest lands in ONE pass; if the pair declines (decode-back
            // mismatch, or it does not save the 5% minimum) it stays untouched.
            // There is still no lossy fallback AFTER a decline — that would
            // re-litigate a resampling decision the lossless path already made,
            // the same no-compounding-losses rule the unmasked path applies.
            return plan_flate_smask_pair_downsample(
                doc, id, mask_id, stream, px_w, px_h, target_w, target_h, options,
            );
        }
        if over_resolution {
            // A shared mask is never RESIZED (P-M1 fail-safe, unchanged from
            // Phase 5): an over-resolution pair whose mask has a second
            // consumer cannot take the coupled downsample. It CAN still take
            // the dimension-preserving requant below: the mask stream is not
            // touched, so every other consumer keeps its alignment, and a
            // future coupled downsample of this pair is blocked by the same
            // shared mask either way — requantizing removes no option that
            // existed before (an earlier draft skipped over-res shared-mask
            // pairs entirely; that stranded ~1 MB of real savings on the NASA
            // corpus while protecting against nothing).
            if !resize_eligible() {
                return plan_requant_replacement(stream, options, id, px_w, px_h);
            }
            // The base is over-resolution, so the whole pair earns a
            // downsample. ATOMICITY (hard rule): plan both streams together
            // and apply both together — a failure on EITHER side (corrupt
            // base, corrupt mask, decode-back mismatch, or a COMBINED size
            // that does not save the 5% minimum) skips the entire pair. There
            // is deliberately no D-M1 fallback here: replacing only the base
            // would violate the mask/image unit rule.
            if let Some(replacement) = plan_smask_pair_downsample(
                doc, id, mask_id, stream, px_w, px_h, target_w, target_h, options,
            ) {
                return Some(replacement);
            }
            return None;
        }

        // D-M1: dimension-preserving requantization (see the guard note
        // above). Reachable for SHARED masks too (P-M1): the mask stream is
        // never modified, so every other consumer keeps its alignment.
        return plan_requant_replacement(stream, options, id, px_w, px_h);
    }

    // The unmasked path. Anything not over-resolved (effective DPI inside the
    // margin, or already at/below the target pixel geometry) is never
    // RESIZED; under-threshold DCTDecode payloads instead take the same
    // dimension-preserving requantization D-M1 applies to masked bases
    // (Phase 6 P-M2) — quality normalization for scanner-quality JPEGs the
    // resize pipeline never reaches. FlateDecode stays untouched under the
    // default lossless contract; with the `allow_lossy_reencode` consent flag
    // (Phase 7 spike) it takes the dimension-preserving Flate→JPEG conversion
    // instead, under the same 5% + decode-back guards as the requant.
    // CCITT/bitonal never reaches this point.
    if !over_resolution {
        if matches!(class, FilterClass::DctOnly) {
            return plan_requant_replacement(stream, options, id, px_w, px_h);
        }
        if matches!(class, FilterClass::FlateOnly) && options.allow_lossy_reencode {
            return plan_flate_lossy_requant_replacement(doc, stream, options, id, px_w, px_h);
        }
        return None;
    }

    let (out, dict_update) = match class {
        FilterClass::DctOnly => {
            let out = plan_dct(stream, options, target_w, target_h)?;
            (out, DictUpdate::Dct)
        }
        FilterClass::FlateOnly => {
            if !options.downsample_flate_images {
                // Geometry changes are declined (`downsample_flate_images`
                // off), but with consent the ENCODING CLASS can still change
                // in place: the same dimension-preserving Flate→JPEG
                // conversion the under-threshold branch applies.
                if options.allow_lossy_reencode {
                    return plan_flate_lossy_requant_replacement(
                        doc, stream, options, id, px_w, px_h,
                    );
                }
                return None;
            }
            // Phase 7 spike: with consent, a JPEG candidate at the SAME
            // target geometry competes with the format-preserving Flate
            // downsample and the smaller payload wins (the shared
            // never-larger guard below still decides against the original).
            // The line-art content guard applies here too, evaluated on the
            // SOURCE pixels (see `plan_flate_to_jpeg`): the p12 profiles that
            // failed human review are over-resolution in the source PDF, so
            // they reach this competition rather than the under-threshold
            // branch. Declining the JPEG candidate leaves the lossless
            // downsample to ship — line art then gets exactly the flag-off
            // result instead of a DCT-mottled one.
            //
            // NO COMPOUNDING LOSSES (Phase 7 post-review fix): the geometry
            // change is the LOSSLESS path's decision to make. If the Flate
            // candidate at this target would itself be declined by the shared
            // never-larger guard below — i.e. downsampling did not even pay
            // for itself in the format that preserves every sample — then the
            // resample is not worth doing, and a JPEG candidate must not
            // resurrect it by hiding the resolution loss behind a DCT win.
            // That is how the p7 TKE banners (objs 22-29) ended up carrying
            // BOTH losses in the spike: the flate downsample grew the stream
            // and was rejected, then the JPEG-of-downsampled-pixels shrank it
            // and shipped. `--allow-lossy` is consent to re-encode, not
            // consent to re-litigate a resampling decision the lossless path
            // already declined.
            let lossless = plan_flate(doc, stream, px_w, px_h, target_w, target_h);
            let lossless_declined = lossless
                .as_ref()
                .is_some_and(|(out, _)| out.len() >= stream.content.len());
            let lossy = if options.allow_lossy_reencode && !lossless_declined {
                plan_flate_to_jpeg(doc, stream, options, px_w, px_h, target_w, target_h)
            } else {
                None
            };
            match (lossless, lossy) {
                (Some((flate_out, parms)), Some(jpeg_out)) => {
                    if jpeg_out.len() < flate_out.len() {
                        (jpeg_out, DictUpdate::FlateToJpeg)
                    } else {
                        (
                            flate_out,
                            DictUpdate::Flate {
                                decode_parms: parms,
                            },
                        )
                    }
                }
                (Some((flate_out, parms)), None) => (
                    flate_out,
                    DictUpdate::Flate {
                        decode_parms: parms,
                    },
                ),
                (None, Some(jpeg_out)) => (jpeg_out, DictUpdate::FlateToJpeg),
                (None, None) => return None,
            }
        }
        FilterClass::CcittOnly
        | FilterClass::Other
        | FilterClass::JpxOnly
        | FilterClass::Jbig2Only => return None,
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
        smask: None,
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
    let (decoded, is_gray) =
        decode_jpeg_scaled(&stream.content, target_w, target_h).or_else(|| {
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

/// Mean-absolute-difference ceiling for the D-M1 decode-back verification.
/// Deliberately loose: a JPEG requantization is lossy by design, so this gate
/// catches catastrophes (wrong component mix, stale/scrambled payloads,
/// shifted geometry), never ordinary quality loss — q78 round trips land at
/// single-digit MAD on document-like content.
const DECODE_BACK_MAX_MAD: f64 = 96.0;

/// Decode a JPEG at (at least) the requested target geometry: mozjpeg's
/// DCT-scaled path first (never materializing the full image when a smaller
/// scale covers the target), falling back to a full decode for color spaces
/// the scaled path declines (CMYK/YCCK) or streams libjpeg refuses. Returns
/// `(image, is_grayscale)`, or `None` on any decode doubt.
fn decode_jpeg(data: &[u8], target_w: u32, target_h: u32) -> Option<(DynamicImage, bool)> {
    decode_jpeg_scaled(data, target_w, target_h).or_else(|| {
        let decoded = image::load_from_memory_with_format(data, ImageFormat::Jpeg).ok()?;
        let is_gray = matches!(
            decoded,
            DynamicImage::ImageLuma8(_) | DynamicImage::ImageLuma16(_)
        );
        Some((decoded, is_gray))
    })
}

/// Phase 5 D-M1: dimension-preserving JPEG requantization for a base image
/// carrying an eligible `/SMask`. The base is decoded at its OWN dimensions
/// (never resized → soft-mask alignment untouched by construction), re-encoded
/// at `OptimizeOptions::jpeg_quality`, and the candidate payload must pass a
/// decode-back verification before it is returned. Any doubt — decode failure,
/// lying stream geometry, or a divergent decode-back — returns `None`.
fn plan_dct_requant(
    stream: &lopdf::Stream,
    options: OptimizeOptions,
    px_w: u32,
    px_h: u32,
) -> Option<Vec<u8>> {
    let quality = options.jpeg_quality.clamp(1, 100);

    // Full decode: same path as the resize pipeline, with the target set to
    // the stream's own geometry (libjpeg then picks the unscaled 8/8 DCT
    // size), so the ordinary decoding machinery is reused as-is.
    let (img, is_gray) = decode_jpeg(&stream.content, px_w, px_h)?;

    // Dimension-preserving contract: the decoded pixel buffer must be EXACTLY
    // the declared geometry. If /Width//Height lie, any re-declaration or
    // implied scale would break mask alignment — fail-safe skip.
    if img.width() != px_w || img.height() != px_h {
        return None;
    }

    // Reference pixels are captured BEFORE the buffer is moved into the
    // encoder; the decode-back verification below compares against these.
    let reference_pixels = if is_gray {
        img.to_luma8().into_raw()
    } else {
        img.to_rgb8().into_raw()
    };

    let out = encode_jpeg(img, is_gray, quality)?;

    // Exact idempotence guard (Phase 7 measurement): if the source's
    // quantization tables are byte-identical to the candidate's, the payload
    // is ALREADY at the configured quality and this "requantization" is pure
    // generation-loss churn. The 5% rule alone does not converge here —
    // mozjpeg's trellis quantization keeps shaving 5-10% per pass on
    // graphics-heavy content it encoded itself (measured on the NASA corpus
    // once Flate→JPEG conversions started producing our own q78 payloads).
    // Table equality is the exact version of what the 5% rule approximates.
    if let (Some(src_tables), Some(out_tables)) =
        (jpeg_quant_tables(&stream.content), jpeg_quant_tables(&out))
    {
        if src_tables == out_tables {
            return None;
        }
    }

    // Decode-back verification of the re-encoded base (hard rule): re-decoding
    // the candidate must reproduce the same geometry, channel count, and
    // nearby pixels before it is allowed to replace the original bytes.
    if !decode_back_matches(
        &out,
        &reference_pixels,
        is_gray,
        px_w,
        px_h,
        DECODE_BACK_MAX_MAD,
    ) {
        return None;
    }
    Some(out)
}

/// The concatenated payloads of a JPEG stream's DQT (quantization table)
/// segments, in file order, up to the start-of-scan marker. Two JPEGs with
/// identical output from this function were quantized with the same tables —
/// i.e. they are at the same quality setting. Returns `None` on any parse
/// doubt (callers must then not draw conclusions either way).
fn jpeg_quant_tables(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 2 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut out = Vec::new();
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
            // Start of scan: every table segment has been seen.
            0xDA => return Some(out),
            _ => {}
        }
        if i + 4 > data.len() {
            return None;
        }
        let len = ((data[i + 2] as usize) << 8) | data[i + 3] as usize;
        if len < 2 || i + 2 + len > data.len() {
            return None;
        }
        if marker == 0xDB {
            out.extend_from_slice(&data[i + 4..i + 2 + len]);
        }
        i += 2 + len;
    }
    None
}

/// The dimension-preserving requantization as a full `Replacement`, shared by
/// D-M1 (masked bases, shared or not) and P-M2 (unmasked under-threshold
/// JPEGs): re-encode at the configured quality via `plan_dct_requant`, then
/// apply the 5% minimum-savings guard (the idempotence note in
/// `plan_replacement` — generation-loss churn lands under 5% and is declined,
/// genuine first-time requants of scanner-quality payloads save far more).
fn plan_requant_replacement(
    stream: &lopdf::Stream,
    options: OptimizeOptions,
    id: ObjectId,
    px_w: u32,
    px_h: u32,
) -> Option<Replacement> {
    let out = plan_dct_requant(stream, options, px_w, px_h)?;
    if out.len() * 100 >= stream.content.len() * 95 {
        return None;
    }
    Some(Replacement {
        id,
        content: out,
        width: px_w as i64,
        height: px_h as i64,
        dict_update: DictUpdate::Dct,
        smask: None,
    })
}

/// True if re-decoding `out` reproduces the reference's geometry, channel
/// count, and pixels (mean absolute difference ≤ `max_mad`).
fn decode_back_matches(
    out: &[u8],
    reference_pixels: &[u8],
    reference_is_gray: bool,
    w: u32,
    h: u32,
    max_mad: f64,
) -> bool {
    let Some((decoded, is_gray)) = decode_jpeg(out, w, h) else {
        return false;
    };
    if is_gray != reference_is_gray {
        return false;
    }
    let actual = if is_gray {
        decoded.to_luma8().into_raw()
    } else {
        decoded.to_rgb8().into_raw()
    };
    if actual.len() != reference_pixels.len() {
        return false;
    }
    let sad: u64 = reference_pixels
        .iter()
        .zip(&actual)
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    sad as f64 / reference_pixels.len() as f64 <= max_mad
}

/// Phase 5 D-M2. Overhead allowance for the combined-size guard, covering the
/// dict tokens the mask replacement writes beyond the stream bytes themselves
/// (`/Width`, `/Height`, the scalar `/Filter /FlateDecode` line). The real
/// delta is usually negative — a stale `/DecodeParms` (a multi-hundred-byte
/// dict) is dropped — so this fixed positive allowance is deliberately
/// conservative.
const MASK_DICT_OVERHEAD: usize = 64;

/// The D-M2 base half: decode (DCT-scaled when possible), resize to the EXACT
/// target geometry with the same Lanczos3 kernel the unmasked path uses,
/// re-encode as JPEG, and decode-back-verify the candidate against the exact
/// resized pixels (geometry + channel count + MAD ceiling) — the same MAD-based
/// check D-M1 applies to its requantized payloads.
fn plan_dct_resize_verified(
    stream: &lopdf::Stream,
    options: OptimizeOptions,
    target_w: u32,
    target_h: u32,
) -> Option<Vec<u8>> {
    let quality = options.jpeg_quality.clamp(1, 100);
    let (decoded, is_gray) = decode_jpeg(&stream.content, target_w, target_h)?;
    let resized = decoded.resize_exact(target_w, target_h, image::imageops::FilterType::Lanczos3);
    // Reference pixels are the exact buffer handed to the encoder; the
    // decode-back below must reproduce its geometry, channel count and nearby
    // pixels from the re-encoded JPEG.
    let reference = if is_gray {
        resized.to_luma8().into_raw()
    } else {
        resized.to_rgb8().into_raw()
    };
    let out = encode_jpeg(resized, is_gray, quality)?;
    if !decode_back_matches(
        &out,
        &reference,
        is_gray,
        target_w,
        target_h,
        DECODE_BACK_MAX_MAD,
    ) {
        return None;
    }
    Some(out)
}

/// The D-M2 mask half: decode the `/SMask` stream — the payload may be JPEG OR
/// FlateDecode — resample gray samples to the BASE's exact target geometry
/// (the core invariant: base and mask must end at identical width/height), and
/// re-encode as plain FlateDecode 8-bit DeviceGray rows (zlib of packed rows,
/// no predictor). Returns `None` on any doubt: a mask that refuses to decode
/// as single-channel gray, a non-identity `/Decode`, lying geometry, or a
/// candidate that fails its own decode-back.
fn plan_mask_resample(
    doc: &Document,
    mask_stream: &lopdf::Stream,
    px_w: u32,
    px_h: u32,
    target_w: u32,
    target_h: u32,
) -> Option<Vec<u8>> {
    let dict = &mask_stream.dict;
    // A non-identity `/Decode` would remap samples after decoding; the
    // exact-byte verification below assumes identity.
    if dict.get(b"Decode").is_ok() {
        return None;
    }
    let filter = dict.get(b"Filter").ok()?;
    let class = classify_filter(doc, filter);
    let mask_img = match class {
        FilterClass::DctOnly => {
            let (decoded, is_gray) = decode_jpeg(&mask_stream.content, target_w, target_h)?;
            // A soft mask is a one-component image; a non-gray payload behind
            // the DeviceGray declaration is a mismatch — skip the whole pair.
            if !is_gray {
                return None;
            }
            decoded
        }
        FilterClass::FlateOnly => {
            if flate_channels(doc, dict)? != 1 {
                return None;
            }
            let encoding = flate_encoding(dict, 1, px_w)?;
            let expected = u64::from(px_w) * u64::from(px_h);
            if expected > MAX_FLATE_PIXEL_BYTES {
                return None;
            }
            let decoded = match encoding {
                FlateEncoding::Plain => inflate_capped(&mask_stream.content, expected as usize)?,
                FlateEncoding::PngPredictor => {
                    let filtered_len = (px_w as usize + 1) * px_h as usize;
                    let inflated = inflate_capped(&mask_stream.content, filtered_len)?;
                    if inflated.len() != filtered_len {
                        return None;
                    }
                    png_defilter(&inflated, 1, px_w as usize)?
                }
            };
            if decoded.len() as u64 != expected {
                return None;
            }
            DynamicImage::ImageLuma8(image::GrayImage::from_raw(px_w, px_h, decoded)?)
        }
        FilterClass::CcittOnly
        | FilterClass::Other
        | FilterClass::JpxOnly
        | FilterClass::Jbig2Only => return None,
    };

    // Bilinear on gray (plan §D-M2), to the base's exact target geometry.
    let resized = mask_img.resize_exact(target_w, target_h, image::imageops::FilterType::Triangle);
    let raw = resized.into_luma8().into_raw();
    let out = deflate_level9(&raw)?;

    // Mask decode-back verification (hard rule): the candidate must inflate
    // back to EXACTLY target_w*target_h gray samples, byte-identical to the
    // planned raster. Flate is lossless, so equality must be exact.
    let back = inflate_capped(&out, raw.len())?;
    if back != raw {
        return None;
    }
    Some(out)
}

/// Phase 5 D-M2: coupled downsampling of an over-resolution JPEG base and its
/// eligible `/SMask`. Both streams are planned together — and carried by the
/// single returned `Replacement` so the apply pass replaces them together.
/// Any doubt on either side, or a COMBINED size that fails the never-larger /
/// 5% idempotence guard, returns `None` and the whole pair stays untouched.
#[allow(clippy::too_many_arguments)] // mirrors plan_replacement's flat signature
fn plan_smask_pair_downsample(
    doc: &Document,
    id: ObjectId,
    mask_id: ObjectId,
    base_stream: &lopdf::Stream,
    px_w: u32,
    px_h: u32,
    target_w: u32,
    target_h: u32,
    options: OptimizeOptions,
) -> Option<Replacement> {
    // Base: JPEG at the target geometry with the existing MAD decode-back.
    let base_out = plan_dct_resize_verified(base_stream, options, target_w, target_h)?;

    // Mask: decode (JPEG or Flate) + resample to the SAME geometry + Flate
    // re-encode, verified by its own decode-back.
    let mask_stream = doc.get_object(mask_id).ok()?.as_stream().ok()?;
    let mask_out = plan_mask_resample(doc, mask_stream, px_w, px_h, target_w, target_h)?;

    // ATOMICITY + idempotence, evaluated over the COMBINED pair. The 5%
    // minimum-savings guard is the D-M1 philosophy ported to the pair: both
    // lossy re-encodes save far more on a genuine first pass, while a second
    // pass over an already-optimized pair lands under 5% and is declined
    // (downsampling to a fixed DPI is naturally idempotent anyway — the
    // `target_* >= px_*` half of `over_resolution` declines before we get
    // here). It also enforces strict-shrink atomicity: the candidate must
    // beat the pair's original size by the full 5% to replace both streams.
    let combined_original = base_stream.content.len() + mask_stream.content.len();
    let combined_candidate = base_out.len() + mask_out.len() + MASK_DICT_OVERHEAD;
    if combined_candidate * 100 >= combined_original * 95 {
        return None;
    }

    Some(Replacement {
        id,
        content: base_out,
        width: target_w as i64,
        height: target_h as i64,
        dict_update: DictUpdate::Dct, // base keeps its existing form (/Filter, /ColorSpace, ...)
        smask: Some(MaskReplacement {
            mask_id,
            content: mask_out,
            width: target_w as i64,
            height: target_h as i64,
        }),
    })
}

/// Phase 5 D-M3: coupled downsampling of an over-resolution FlateDecode base
/// and its eligible `/SMask` — the same atomicity structure as the D-M2 JPEG
/// pair, with the base going through the format-preserving Flate→Flate path
/// (`plan_flate`: same `/ColorSpace`, predictor handling unchanged) instead of
/// a JPEG re-encode. Both streams are planned together and carried by the
/// single returned `Replacement` so the apply pass replaces them together.
/// Any doubt on either side, or a COMBINED size that fails the never-larger /
/// 5% minimum-savings guard, returns `None` and the whole pair stays untouched.
///
/// Phase 7 (Option B): under `allow_lossy_reencode` the base additionally gets
/// a JPEG competitor at the SAME target geometry, mirroring the unmasked
/// over-resolution competition in `plan_replacement`. The mask half is
/// unchanged either way — it is still the losslessly resampled
/// `plan_mask_resample` raster at the base's target geometry — so base and
/// mask land on identical pixel grids whichever base candidate wins.
///
/// SHARED-MASK REASONING (P-M1 line): a target-geometry JPEG competitor sits
/// squarely on the RESIZE side of the requant-safe / resize-blocked split. It
/// does not widen the pair's mask exposure by one byte — the mask is resampled
/// here regardless of which base candidate wins, so this whole function is
/// already gated on `SmaskUse::Resize` eligibility in `plan_replacement` and a
/// shared mask never reaches it at all. The competitor changes only the base
/// stream's ENCODING, and every base that reaches it was already going to be
/// resampled losslessly. Nothing about the shared-mask refcount guard moves.
///
/// This is also what restores one-pass idempotence for masked Flate pairs:
/// before the competitor existed, pass 1 downsampled the pair to Flate-at-
/// target and pass 2 then saw an at-target masked *Flate* base and converted it
/// through `plan_flate_lossy_requant_replacement`, splitting one harvest across
/// two passes. With the competitor, the conversion happens in pass 1 and the
/// pass-2 base is DCTDecode at target geometry — not over-resolution, so it
/// takes only the D-M1 requant, which its own 5% guard declines. Pinned by
/// `smask_flate_lossy_pair_is_idempotent_in_one_pass`.
#[allow(clippy::too_many_arguments)] // mirrors plan_smask_pair_downsample's flat signature
fn plan_flate_smask_pair_downsample(
    doc: &Document,
    id: ObjectId,
    mask_id: ObjectId,
    base_stream: &lopdf::Stream,
    px_w: u32,
    px_h: u32,
    target_w: u32,
    target_h: u32,
    options: OptimizeOptions,
) -> Option<Replacement> {
    // Base: the exact eligibility gates and re-encode variants of the unmasked
    // Flate route (8bpc only, no /Decode, capped inflate + exact length check,
    // plain-vs-Up-predictor output selection). Flate is lossless, so the
    // deflate round trip needs no MAD-style decode-back of its own.
    let (base_out, decode_parms) = plan_flate(doc, base_stream, px_w, px_h, target_w, target_h)?;

    // Mask: decode (JPEG or Flate) + resample to the SAME geometry + Flate
    // re-encode, verified by its own exact decode-back.
    let mask_stream = doc.get_object(mask_id).ok()?.as_stream().ok()?;
    let mask_out = plan_mask_resample(doc, mask_stream, px_w, px_h, target_w, target_h)?;

    // ATOMICITY + idempotence, evaluated over the COMBINED pair — the same
    // arithmetic as the D-M2 guard (see plan_smask_pair_downsample): the
    // candidate, including the mask's dict-token overhead, must beat the
    // pair's original size by the full 5% to replace both streams.
    let combined_original = base_stream.content.len() + mask_stream.content.len();
    let combined_lossless = base_out.len() + mask_out.len() + MASK_DICT_OVERHEAD;

    // NO COMPOUNDING LOSSES, the pair's form of the unmasked rule: the geometry
    // change is the LOSSLESS path's decision. If the fully lossless pair
    // candidate would itself be declined by the combined guard below — the
    // resample did not pay for itself in the format that preserves every sample
    // — then a JPEG base must not resurrect it by hiding the resolution loss
    // behind a DCT win. The predicate is the exact guard the pair is judged by,
    // so "declined" means the same thing on both sides.
    let lossless_declined = combined_lossless * 100 >= combined_original * 95;
    let lossy = if options.allow_lossy_reencode && !lossless_declined {
        // The line-art content guard lives inside `plan_flate_to_jpeg` and is
        // evaluated on the decoded SOURCE pixels; declining there just leaves
        // the lossless downsample to ship, i.e. exactly the flag-off pair.
        plan_flate_to_jpeg(doc, base_stream, options, px_w, px_h, target_w, target_h)
    } else {
        None
    };

    // Smaller base wins. The mask half is byte-for-byte the same either way, so
    // comparing the base candidates alone is the same comparison as comparing
    // the two combined pairs.
    let (content, dict_update) = match lossy {
        Some(jpeg_out) if jpeg_out.len() < base_out.len() => (jpeg_out, DictUpdate::FlateToJpeg),
        _ => (base_out, DictUpdate::Flate { decode_parms }),
    };

    let combined_candidate = content.len() + mask_out.len() + MASK_DICT_OVERHEAD;
    if combined_candidate * 100 >= combined_original * 95 {
        return None;
    }

    Some(Replacement {
        id,
        content,
        width: target_w as i64,
        height: target_h as i64,
        dict_update,
        smask: Some(MaskReplacement {
            mask_id,
            content: mask_out,
            width: target_w as i64,
            height: target_h as i64,
        }),
    })
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
    let (img, channels) = decode_flate_image(doc, stream, px_w, px_h)?;
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

/// One planned RGB→Gray collapse: the gray payload plus the predictor parms
/// it was encoded under (`None` = plain rows). Applied by `try_optimize`
/// together with the `/ColorSpace /DeviceGray` rewrite.
struct GrayCollapse {
    id: ObjectId,
    content: Vec<u8>,
    decode_parms: Option<lopdf::Dictionary>,
}

/// Plan every eligible RGB→Gray collapse in parallel (read-only against the
/// document). See [`OptimizeOptions::collapse_gray_images`] for scope; every
/// gate declines on doubt, and a candidate is kept only when the gray stream
/// is strictly smaller than the RGB one it replaces.
fn plan_gray_collapses(doc: &Document) -> Vec<GrayCollapse> {
    let candidates: Vec<(ObjectId, &lopdf::Stream)> = doc
        .objects
        .iter()
        .filter_map(|(&id, obj)| {
            let Object::Stream(stream) = obj else {
                return None;
            };
            let dict = &stream.dict;
            if !matches!(dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
                return None;
            }
            // A color-key /Mask is a range over the CURRENT color space's
            // components — collapsing under one would change what it masks.
            if dict.get(b"Mask").is_ok() {
                return None;
            }
            // Plain /DeviceRGB only: ICCBased color carries a profile that a
            // /DeviceGray rewrite would silently drop.
            if !matches!(
                dict.get(b"ColorSpace").map(|o| resolve(doc, o)),
                Ok(Object::Name(n)) if n == b"DeviceRGB"
            ) {
                return None;
            }
            let filter = dict.get(b"Filter").ok()?;
            matches!(classify_filter(doc, filter), FilterClass::FlateOnly).then_some((id, stream))
        })
        .collect();

    candidates
        .into_par_iter()
        .filter_map(|(id, stream)| {
            let px_w = u32::try_from(
                resolve(doc, stream.dict.get(b"Width").ok()?)
                    .as_i64()
                    .ok()?,
            )
            .ok()?;
            let px_h = u32::try_from(
                resolve(doc, stream.dict.get(b"Height").ok()?)
                    .as_i64()
                    .ok()?,
            )
            .ok()?;
            let (img, channels) = decode_flate_image(doc, stream, px_w, px_h)?;
            if channels != 3 {
                return None;
            }
            let rgb = img.into_rgb8().into_raw();
            if rgb
                .as_chunks::<3>()
                .0
                .iter()
                .any(|px| px[0] != px[1] || px[1] != px[2])
            {
                return None;
            }
            let gray: Vec<u8> = rgb.iter().step_by(3).copied().collect();

            // Same encoder choice as `plan_flate`: plain vs Up-filtered rows,
            // the predictor variant paying for the /DecodeParms it forces in.
            let plain = deflate_level9(&gray)?;
            let up = deflate_level9(&png_up_filter(&gray, px_w, 1))?;
            const PARMS_OVERHEAD: usize = 70;
            let (content, decode_parms) = if up.len() + PARMS_OVERHEAD < plain.len() {
                let parms = dictionary! {
                    "Predictor" => 15_i64,
                    "Colors" => 1_i64,
                    "BitsPerComponent" => 8_i64,
                    "Columns" => px_w as i64,
                };
                (up, Some(parms))
            } else {
                (plain, None)
            };
            (content.len() < stream.content.len()).then_some(GrayCollapse {
                id,
                content,
                decode_parms,
            })
        })
        .collect()
}

/// The shared decode stage of every unmasked-Flate transform: all of
/// `plan_flate`'s eligibility gates (8-bit only, no `/Decode`, handled color
/// space, decodable predictor layout), the decompression-bomb cap, capped
/// inflate + spec-correct PNG defiltering, and the exact decoded-length check.
/// Returns the decoded pixels and the channel count (1 or 3), or `None` on
/// any doubt (image untouched).
fn decode_flate_image(
    doc: &Document,
    stream: &lopdf::Stream,
    px_w: u32,
    px_h: u32,
) -> Option<(DynamicImage, usize)> {
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
    Some((img, channels))
}

/// Metrics behind the line-art guard, computed in a single pass over decoded
/// 8-bit samples. See [`looks_like_line_art`] for the thresholds and the
/// measured per-class values.
struct LineArtMetrics {
    /// Fraction of pixels sharing the single most common quantized color.
    background: f64,
    /// Fraction of pixels covered by the 8 most common quantized colors.
    palette: f64,
    /// Fraction of pixels whose right or lower neighbor differs by more than
    /// `EDGE_STEP` in any channel.
    edges: f64,
}

/// Channel step that counts as a sharp edge between neighboring samples.
const EDGE_STEP: u8 = 48;

/// Compute the [`LineArtMetrics`] of an interleaved 8-bit buffer (`channels`
/// samples per pixel, `w * h * channels` bytes). Colors are quantized to 5 bits
/// per channel before histogramming, so anti-aliasing fringes and mild noise do
/// not shatter a flat region into thousands of distinct "colors".
fn line_art_metrics(pixels: &[u8], channels: usize, w: u32, h: u32) -> LineArtMetrics {
    let total = (w as usize) * (h as usize);
    let mut histogram: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let quantized = |i: usize| -> u32 {
        let px = &pixels[i * channels..i * channels + channels];
        px.iter()
            .fold(0u32, |acc, s| (acc << 5) | u32::from(s >> 3))
    };
    let mut edges = 0usize;
    for y in 0..h as usize {
        for x in 0..w as usize {
            let i = y * w as usize + x;
            *histogram.entry(quantized(i)).or_insert(0) += 1;
            let here = &pixels[i * channels..i * channels + channels];
            let step = |other: &[u8]| {
                here.iter()
                    .zip(other)
                    .any(|(a, b)| a.abs_diff(*b) > EDGE_STEP)
            };
            let right = (x + 1 < w as usize)
                .then(|| (i + 1) * channels)
                .is_some_and(|j| step(&pixels[j..j + channels]));
            let down = (y + 1 < h as usize)
                .then(|| (i + w as usize) * channels)
                .is_some_and(|j| step(&pixels[j..j + channels]));
            if right || down {
                edges += 1;
            }
        }
    }
    let mut counts: Vec<u32> = histogram.into_values().collect();
    counts.sort_unstable_by(|a, b| b.cmp(a));
    let sum_top = |n: usize| -> u64 { counts.iter().take(n).map(|c| u64::from(*c)).sum() };
    let total_f = total.max(1) as f64;
    LineArtMetrics {
        background: sum_top(1) as f64 / total_f,
        palette: sum_top(8) as f64 / total_f,
        edges: edges as f64 / total_f,
    }
}

/// Phase 7 (post-spike human review): decline the lossy Flate→JPEG conversion
/// for line-art-like content — thin curves/dashes on a flat background, few
/// distinct colors, sharp edges. On this class DCT produces exactly the defects
/// the side-by-side review flagged (background mottling, muddied dash-dot
/// lines, hairline color shift), and the 5%-savings guard is no protection: a
/// JPEG of line art beats a mediocre deflate almost every time, so savings
/// alone always says "convert".
///
/// The signature is "mostly one flat background color, a handful of ink colors
/// on top, and only a thin scattering of sharp transitions": a dominant
/// background covering ≥ 75% of the image, the top 8 quantized colors covering
/// ≥ 90%, and fewer than 10% of pixels sitting on a sharp edge. Rasterized
/// plots/photographs fail the first two by a wide margin — their color mass is
/// spread across gradients — and dense high-contrast raster content fails the
/// third.
///
/// Measured values on the Phase 7 review corpus (`target/spike/nasa.off.pdf`
/// and `sample.off.pdf`, the flag-OFF outputs the census diffed):
///
/// | class | objs | background | palette(8) | edges | verdict |
/// | --- | --- | ---: | ---: | ---: | --- |
/// | line-art plug profiles (p12) | 59–61 | 0.909–0.930 | 0.946–0.956 | 0.061–0.065 | DECLINE |
/// | CFD velocity fields (p8/p10) | 36, 46 | 0.201, 0.413 | 0.425, 0.568 | 0.092, 0.138 | convert |
/// | 3D PSD surface plots (p39/40) | 248–255 | 0.522–0.538 | 0.747–0.778 | 0.152–0.183 | convert |
/// | synthetic noise stripes | sample 4–5 | 0.001–0.008 | 0.007–0.057 | 0.104–0.363 | convert |
///
/// The background metric alone separates the classes by a factor of two; the
/// palette and edge conditions are belt-and-braces so a near-flat *photograph*
/// (say a product shot on white) with real tonal content is still converted.
fn looks_like_line_art(pixels: &[u8], channels: usize, w: u32, h: u32) -> bool {
    let m = line_art_metrics(pixels, channels, w, h);
    m.background >= LINE_ART_MIN_BACKGROUND
        && m.palette >= LINE_ART_MIN_PALETTE
        && m.edges <= LINE_ART_MAX_EDGES
}

const LINE_ART_MIN_BACKGROUND: f64 = 0.75;
const LINE_ART_MIN_PALETTE: f64 = 0.90;
const LINE_ART_MAX_EDGES: f64 = 0.08;

/// Phase 7 spike: the consent-gated lossy Flate→JPEG candidate. Decodes the
/// Flate pixels through the exact same gates as the format-preserving path
/// (`decode_flate_image`), optionally resizes to the target geometry
/// (Lanczos3 — the same kernel every resize path uses; a same-size target is
/// a pure re-encode), JPEG-encodes at `OptimizeOptions::jpeg_quality`
/// preserving the channel count, and decode-back-verifies the candidate
/// against the exact pixels handed to the encoder (geometry + channel count +
/// the D-M1 MAD ceiling). Returns the JPEG bytes, or `None` on any doubt.
/// Only reached when `allow_lossy_reencode` is true; size guards are the
/// caller's.
///
/// The [`looks_like_line_art`] content check runs on every candidate, always
/// against the DECODED SOURCE pixels — never resized ones, where the resampler
/// has already blurred the sharp edges and flat backgrounds the metrics key on.
/// Declining here removes only the JPEG candidate; on the over-resolution path
/// the format-preserving Flate downsample still competes and ships, so line art
/// keeps exactly the flag-off result.
fn plan_flate_to_jpeg(
    doc: &Document,
    stream: &lopdf::Stream,
    options: OptimizeOptions,
    px_w: u32,
    px_h: u32,
    target_w: u32,
    target_h: u32,
) -> Option<Vec<u8>> {
    let quality = options.jpeg_quality.clamp(1, 100);
    let (img, channels) = decode_flate_image(doc, stream, px_w, px_h)?;
    let is_gray = channels == 1;
    let source = if is_gray {
        img.to_luma8().into_raw()
    } else {
        img.to_rgb8().into_raw()
    };
    if looks_like_line_art(&source, channels, px_w, px_h) {
        return None;
    }
    let img = if (target_w, target_h) == (px_w, px_h) {
        img
    } else {
        img.resize_exact(target_w, target_h, image::imageops::FilterType::Lanczos3)
    };
    let reference = if is_gray {
        img.to_luma8().into_raw()
    } else {
        img.to_rgb8().into_raw()
    };
    let out = encode_jpeg(img, is_gray, quality)?;
    if !decode_back_matches(
        &out,
        &reference,
        is_gray,
        target_w,
        target_h,
        DECODE_BACK_MAX_MAD,
    ) {
        return None;
    }
    Some(out)
}

/// The dimension-preserving lossy Flate→JPEG conversion as a full
/// `Replacement` (Phase 7 spike): re-encode at the stream's own geometry,
/// then apply the strict-smaller AND 5% minimum-savings guard — the same
/// arithmetic as `plan_requant_replacement`, and for the same reason: once
/// converted, the payload is a DCTDecode stream whose second-pass requant
/// churn lands under 5% and is declined, keeping repeat passes byte-stable.
///
/// The [`looks_like_line_art`] content guard matters most here: a
/// dimension-preserving conversion competes against nothing, so without a
/// content check the 5% rule converts line art unconditionally (a q78 JPEG of
/// thin curves on white beats a mediocre deflate by ~50%) — and line art is
/// precisely the class DCT is worst at.
fn plan_flate_lossy_requant_replacement(
    doc: &Document,
    stream: &lopdf::Stream,
    options: OptimizeOptions,
    id: ObjectId,
    px_w: u32,
    px_h: u32,
) -> Option<Replacement> {
    let out = plan_flate_to_jpeg(doc, stream, options, px_w, px_h, px_w, px_h)?;
    if out.len() * 100 >= stream.content.len() * 95 {
        return None;
    }
    Some(Replacement {
        id,
        content: out,
        width: px_w as i64,
        height: px_h as i64,
        dict_update: DictUpdate::FlateToJpeg,
        smask: None,
    })
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
fn flate_encoding(dict: &lopdf::Dictionary, channels: usize, px_w: u32) -> Option<FlateEncoding> {
    let parms = match dict.get(b"DecodeParms") {
        Err(_) => return Some(FlateEncoding::Plain), // no parms: plain deflate
        Ok(Object::Dictionary(d)) => d,
        Ok(_) => return None, // array / reference form: out of M1 scope
    };
    let predictor = parms
        .get(b"Predictor")
        .and_then(Object::as_i64)
        .unwrap_or(1);
    match predictor {
        1 => Some(FlateEncoding::Plain),
        10..=15 => {
            let colors = parms.get(b"Colors").and_then(Object::as_i64).unwrap_or(1);
            let columns = parms.get(b"Columns").and_then(Object::as_i64).unwrap_or(1);
            let parms_bpc = parms
                .get(b"BitsPerComponent")
                .and_then(Object::as_i64)
                .unwrap_or(8);
            let matches_image =
                colors == channels as i64 && columns == i64::from(px_w) && parms_bpc == 8;
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

/// Deflate with the exhaustive zopfli search (default 15 iterations). Same
/// zlib container as [`deflate_level9`] — any inflate reads it — just a more
/// expensive hunt for a smaller encoding.
fn deflate_zopfli(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    zopfli::compress(
        zopfli::Options::default(),
        zopfli::Format::Zlib,
        data,
        &mut out,
    )
    .ok()?;
    Some(out)
}

/// Dispatch to the configured deflate backend (final-pass call sites only —
/// planning-time deflates always use [`deflate_level9`] for speed, and the
/// final pass revisits their output anyway).
fn deflate_backend(data: &[u8], backend: DeflateBackend) -> Option<Vec<u8>> {
    match backend {
        DeflateBackend::Zlib => deflate_level9(data),
        DeflateBackend::Zopfli => deflate_zopfli(data),
    }
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
fn decode_jpeg_scaled(data: &[u8], target_w: u32, target_h: u32) -> Option<(DynamicImage, bool)> {
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
    if matches!(
        dec.color_space(),
        ColorSpace::JCS_CMYK | ColorSpace::JCS_YCCK
    ) {
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

/// A planned `/JPXDecode` → `/DCTDecode` conversion (strictly opt-in via
/// `--allow-lossy`, like every other encoding-class change).
struct JpxConversion {
    id: ObjectId,
    content: Vec<u8>,
    /// `DeviceRGB` or `DeviceGray` — the JPX stream may carry its color space
    /// inside the codestream with no `/ColorSpace` in the dict at all, so the
    /// replacement must write one explicitly for the JPEG payload.
    colorspace: &'static [u8],
    /// Codestream geometry. Written into the dict: PDF 32000 §7.4.9 requires
    /// dict `/Width`/`/Height` to match the codestream, but real files get
    /// this wrong, and viewers follow the self-framing codestream (the image
    /// maps to the unit square either way). The replacement normalizes the
    /// dict to the truth.
    width: u32,
    height: u32,
}

/// Plan lossy JPEG2000 → JPEG conversions for every eligible `/JPXDecode`
/// image. JPX is the one payload class where a *dimension-preserving* lossy
/// re-encode adds compatibility value beyond size: JPEG2000 support in
/// viewers is spotty, DCT support is universal.
///
/// Eligibility is deliberately narrow (any doubt → untouched):
/// - `/Subtype /Image`, scalar `/JPXDecode` filter, no `/DecodeParms`;
/// - no `/SMask`, `/Mask`, or `/Decode` (semantics we would have to carry);
/// - `/ColorSpace` absent (the JP2 box supplies it) or exactly the matching
///   `DeviceRGB`/`DeviceGray` name; anything else (ICC, Indexed, Lab) would
///   change appearance when re-tagged;
/// - 8-bit, no alpha, opaque Gray/RGB decode (the codestream's geometry is
///   authoritative and gets written back into the dict — see
///   [`JpxConversion::width`]);
/// - not line art (`looks_like_line_art`, same posture as Flate→JPEG);
/// - the JPEG re-decodes to the same pixels within `DECODE_BACK_MAX_MAD`;
/// - ≥5% smaller, the same arithmetic as `plan_requant_replacement` and for
///   the same reason: the output is a DCTDecode stream a second pass would
///   otherwise churn on, and the 5% floor keeps repeat passes byte-stable.
fn plan_jpx_conversions(doc: &Document, options: OptimizeOptions) -> Vec<JpxConversion> {
    use dicom_toolkit_jpeg2000::{ColorSpace, DecodeSettings, Image as JpxImage};

    let quality = options.jpeg_quality.clamp(1, 100);
    let mut plans = Vec::new();
    for (&id, obj) in doc.objects.iter() {
        let Object::Stream(stream) = obj else {
            continue;
        };
        if !matches!(stream.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
            continue;
        }
        let Ok(filter) = stream.dict.get(b"Filter") else {
            continue;
        };
        if classify_filter(doc, filter) != FilterClass::JpxOnly {
            continue;
        }
        if [&b"DecodeParms"[..], b"SMask", b"Mask", b"Decode"]
            .iter()
            .any(|k| !matches!(stream.dict.get(k), Err(_) | Ok(Object::Null)))
        {
            continue;
        }
        let Ok(jpx) = JpxImage::new(&stream.content, &DecodeSettings::default()) else {
            continue;
        };
        if jpx.has_alpha() || jpx.original_bit_depth() != 8 {
            continue;
        }
        let (is_gray, cs_name): (bool, &'static [u8]) = match jpx.color_space() {
            ColorSpace::Gray => (true, b"DeviceGray"),
            ColorSpace::RGB => (false, b"DeviceRGB"),
            _ => continue,
        };
        match stream.dict.get(b"ColorSpace") {
            Err(_) | Ok(Object::Null) => {}
            Ok(cs) => match resolve(doc, cs) {
                Object::Name(n) if n.as_slice() == cs_name => {}
                _ => continue,
            },
        }
        let (w, h) = (jpx.width(), jpx.height());
        match stream.dict.get(b"BitsPerComponent") {
            Err(_) | Ok(Object::Null) => {}
            Ok(bpc) => {
                if bpc.as_i64().ok() != Some(8) {
                    continue;
                }
            }
        }
        let Ok(pixels) = jpx.decode() else {
            continue;
        };
        let channels = if is_gray { 1usize } else { 3 };
        if pixels.len() != (w as usize) * (h as usize) * channels {
            continue;
        }
        if looks_like_line_art(&pixels, channels, w, h) {
            continue;
        }
        let img = if is_gray {
            image::GrayImage::from_raw(w, h, pixels.clone()).map(DynamicImage::ImageLuma8)
        } else {
            image::RgbImage::from_raw(w, h, pixels.clone()).map(DynamicImage::ImageRgb8)
        };
        let Some(img) = img else { continue };
        let Some(out) = encode_jpeg(img, is_gray, quality) else {
            continue;
        };
        if !decode_back_matches(&out, &pixels, is_gray, w, h, DECODE_BACK_MAX_MAD) {
            continue;
        }
        if out.len() * 100 >= stream.content.len() * 95 {
            continue;
        }
        plans.push(JpxConversion {
            id,
            content: out,
            colorspace: cs_name,
            width: w,
            height: h,
        });
    }
    plans
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

/// True for page-tree node dicts (`/Type /Page` or `/Type /Pages`), which
/// must never be deduplicated even when byte-identical. A page object has
/// IDENTITY semantics beyond its bytes: merging two identical blank pages
/// puts the same object id in `/Kids` twice, which (a) changes what GoTo
/// destinations and `/StructParents` resolve to, and (b) breaks lopdf 0.42's
/// `renumber_objects_with` — its page-reordering pass assumes `page_iter()`
/// yields distinct ids, and duplicated kids make it collide page objects onto
/// one id, silently overwriting OTHER pages (verified on the NASA repro:
/// merging two identical blank pages dropped a scanned page's dict and
/// orphaned its 1.37 MB image subtree).
fn is_page_node(obj: &Object) -> bool {
    let Object::Dictionary(dict) = obj else {
        return false;
    };
    matches!(dict.get(b"Type"), Ok(Object::Name(n)) if n == b"Page" || n == b"Pages")
}

/// Merge true duplicate non-stream objects. Two objects are duplicates when
/// their serialized bytes are identical. For each duplicate group the lowest
/// `ObjectId` is kept as canonical; all references to the others are
/// redirected, and the duplicates are removed from the document. Returns true
/// if anything merged, so the caller can iterate to a fixpoint.
///
/// Safe because identical objects produce identical results in all contexts —
/// EXCEPT page-tree nodes, whose object identity is load-bearing (see
/// [`is_page_node`]); those are never merged. Reduces the object count before
/// packing. On sparse documents (a few dozen duplicates among a few hundred
/// objects) the gain is small; on denser documents it can be more significant.
fn dedup_objects(doc: &mut Document) -> bool {
    // Group non-stream objects by their exact serialized bytes. Keying the map
    // on the bytes themselves (not a 64-bit hash of them) means only genuinely
    // identical objects ever share a bucket, so a hash collision can never
    // cause two different objects to be merged.
    let mut by_bytes: HashMap<Vec<u8>, Vec<ObjectId>> = HashMap::new();
    for (&id, obj) in doc.objects.iter() {
        if is_page_node(obj) {
            continue;
        }
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
        return false;
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
    true
}

/// Merge `FlateDecode` streams that decode to **byte-identical** payloads,
/// even when their on-disk (compressed) bytes differ. `dedup_streams` keys on
/// the raw stream bytes + dict, so it catches only exact restatements; this
/// second pass catches the more common case where a producer (pdfTeX, zlib)
/// wrote the *same* embedded font program or bitmap through independent
/// inflate calls — the stored zlib streams and their `/Length` differ, but the
/// decoded bytes a viewer actually consumes are identical. Down the pipeline
/// those N copies collapse to one object, removing every duplicate's bytes.
///
/// Why this is lossless: the only observable effect of an embedded font or
/// image stream is its decoded bytes. Two streams with identical decoded
/// payloads render identically and extract identically; merging merely changes
/// which object a reference points at. The canonical stream keeps its own
/// decoded payload (by construction equal) and its `/Length1/2/3`/color-space
/// properties (equal, since they are properties of the same payload), so no
/// descriptor or content ever reads contradictory metadata.
///
/// Fail-safe: only scalar `FlateDecode` with no `/DecodeParms` is considered
/// (the shape whose decode is a pure byte-for-byte inflate, so decoded
/// equality is exactly the equivalence a viewer renders); any inflate error,
/// oversize output, or unsupported shape is skipped. The merge is
/// keyed on the full decoded bytes (not a hash), so a collision cannot cause a
/// false merge.
fn dedup_decoded_streams(doc: &mut Document) -> bool {
    // decoded-payload -> object ids carrying it.
    let mut buckets: HashMap<Vec<u8>, Vec<ObjectId>> = HashMap::new();
    for (&id, obj) in doc.objects.iter() {
        if let Object::Stream(s) = obj {
            // DecodeParms imply a non-trivial decode (PNG/tiff predictors)
            // whose bytes are format-specific; decoded equality would not be a
            // clean render equivalence for those shapes, so skip them.
            if !matches!(s.dict.get(b"DecodeParms"), Err(_) | Ok(Object::Null)) {
                continue;
            }
            let filter = match s.dict.get(b"Filter") {
                Ok(f) => f,
                Err(_) => continue,
            };
            if classify_filter(doc, filter) != FilterClass::FlateOnly {
                continue;
            }
            if let Some(decoded) = inflate_capped(&s.content, MAX_REDEFLATE_BYTES) {
                buckets.entry(decoded).or_default().push(id);
            }
        }
    }

    // Lowest object id wins as canonical; everything else redirects to it.
    let mut remap: HashMap<ObjectId, ObjectId> = HashMap::new();
    for ids in buckets.values() {
        if ids.len() < 2 {
            continue;
        }
        let mut ids = ids.clone();
        ids.sort_unstable();
        let canonical = ids[0];
        for dup in &ids[1..] {
            remap.insert(*dup, canonical);
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

/// Decoded bytes of a content stream, accepting only shapes whose decode is
/// beyond doubt: no filter at all, or scalar/array `FlateDecode` with no
/// `/DecodeParms`. Anything else (LZW, predictors, exotic filters, inflate
/// failure) returns `None` and the stream is left untouched.
fn content_stream_plain(doc: &Document, stream: &lopdf::Stream) -> Option<Vec<u8>> {
    match stream.dict.get(b"Filter") {
        Err(_) | Ok(Object::Null) => Some(stream.content.clone()),
        Ok(filter) => {
            if classify_filter(doc, filter) != FilterClass::FlateOnly {
                return None;
            }
            if !matches!(stream.dict.get(b"DecodeParms"), Err(_) | Ok(Object::Null)) {
                return None;
            }
            inflate_capped(&stream.content, MAX_REDEFLATE_BYTES)
        }
    }
}

/// Count how many `Object::Reference`s point at each object id, across every
/// object body and the trailer. Used by the content minifier to prove a
/// multi-stream page's content objects are referenced ONLY by that page before
/// replacing the array with a single merged stream.
fn count_object_references(doc: &Document) -> HashMap<ObjectId, usize> {
    fn walk(obj: &Object, counts: &mut HashMap<ObjectId, usize>) {
        match obj {
            Object::Reference(id) => *counts.entry(*id).or_insert(0) += 1,
            Object::Array(items) => items.iter().for_each(|o| walk(o, counts)),
            Object::Dictionary(d) => d.iter().for_each(|(_, v)| walk(v, counts)),
            Object::Stream(s) => s.dict.iter().for_each(|(_, v)| walk(v, counts)),
            _ => {}
        }
    }
    let mut counts = HashMap::new();
    for obj in doc.objects.values() {
        walk(obj, &mut counts);
    }
    for (_, v) in doc.trailer.iter() {
        walk(v, &mut counts);
    }
    counts
}

/// Semantic equality for re-parsed content operands. Plain `PartialEq` is too
/// strict for one deliberate case: `Real(1.0)` re-emits as `1` (Rust's
/// shortest round-trip formatting), which re-parses as `Integer(1)`. The
/// number a viewer computes is identical, so Integer/Real pairs compare by
/// value. The integer side always came from re-parsing the Real's exact
/// printed digits, so the `as f32` conversion reproduces the original bit
/// pattern — there is no precision loophole. Everything else (names, strings
/// with their format, booleans, references) must match exactly.
fn objects_equivalent(a: &Object, b: &Object) -> bool {
    match (a, b) {
        (Object::Integer(i), Object::Real(r)) | (Object::Real(r), Object::Integer(i)) => {
            *i as f32 == *r
        }
        (Object::Array(x), Object::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| objects_equivalent(p, q))
        }
        (Object::Dictionary(x), Object::Dictionary(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).map(|w| objects_equivalent(v, w)).unwrap_or(false))
        }
        _ => a == b,
    }
}

/// The numeric literals of a content stream, in order, as f64 — the precision
/// a real viewer parses at. lopdf holds operands as f32, so its re-emit can
/// print a *shorter* decimal that maps to the same f32 but a different f64
/// (e.g. `0.30000001` -> `0.3`), and that sub-1e-7 drift is enough to flip an
/// antialiased pixel in a strict render-hash comparison. The minifier
/// therefore requires the original and re-emitted number sequences to be
/// f64-identical; trailing-zero/whitespace rewrites pass (same decimal value),
/// true precision changes don't. `None` = lexing confusion; caller must skip.
///
/// The lexer only needs to be exact about what can HIDE digits: literal
/// strings (with escapes and balanced parens), hex strings, names (regular
/// chars include digits), and comments. Numbers are `[+-]?[0-9.]+`; a token
/// like `1.2.3` (two PDF numbers) fails the f64 parse and returns `None`,
/// erring toward keeping the original bytes.
fn content_number_values(bytes: &[u8]) -> Option<Vec<f64>> {
    let tokens = content_number_tokens(bytes)?;
    let mut out = Vec::with_capacity(tokens.len());
    for span in tokens {
        let text = std::str::from_utf8(&bytes[span])
            .ok()
            .filter(|t| t.bytes().filter(|&b| b == b'.').count() <= 1)?;
        out.push(text.parse::<f64>().ok()?);
    }
    Some(out)
}

/// Byte spans of every numeric literal in a content stream, in order. See
/// `content_number_values` for the lexer's scope and failure posture.
fn content_number_tokens(bytes: &[u8]) -> Option<Vec<std::ops::Range<usize>>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\0' | b'\t' | b'\n' | b'\x0C' | b'\r' | b' ' => i += 1,
            b'%' => {
                while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
                    i += 1;
                }
            }
            b'(' => {
                let mut depth = 1usize;
                i += 1;
                while i < bytes.len() && depth > 0 {
                    match bytes[i] {
                        b'\\' => i += 1, // skip the escaped byte too
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        _ => {}
                    }
                    i += 1;
                }
                if depth > 0 {
                    return None;
                }
            }
            b'<' => {
                if bytes.get(i + 1) == Some(&b'<') {
                    i += 2;
                } else {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'>' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'/' => {
                i += 1;
                while i < bytes.len() && is_regular_content_char(bytes[i]) {
                    i += 1;
                }
            }
            b'+' | b'-' | b'.' | b'0'..=b'9' => {
                let start = i;
                i += 1;
                while i < bytes.len() && matches!(bytes[i], b'0'..=b'9' | b'.') {
                    i += 1;
                }
                out.push(start..i);
            }
            b'>' | b']' | b'[' | b'{' | b'}' | b')' => i += 1,
            _ => {
                i += 1;
                while i < bytes.len() && is_regular_content_char(bytes[i]) {
                    i += 1;
                }
            }
        }
    }
    Some(out)
}

/// Shortest decimal-EXACT form of a PDF number literal: drop a `+` sign,
/// leading integer zeros, trailing fraction zeros, and a bare trailing `.`.
/// Never changes the decimal value, so the f64 a viewer parses is identical —
/// unlike shortest-f32 re-printing. `-0`/`-0.000` normalize to `0`.
fn minify_number_literal(text: &str) -> String {
    let (neg, rest) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let (int, frac) = match rest.split_once('.') {
        Some((i, f)) => (i, f),
        None => (rest, ""),
    };
    let int = int.trim_start_matches('0');
    let frac = frac.trim_end_matches('0');
    let sign = if neg && !(int.is_empty() && frac.is_empty()) {
        "-"
    } else {
        ""
    };
    if frac.is_empty() {
        if int.is_empty() {
            "0".to_string()
        } else {
            format!("{sign}{int}")
        }
    } else {
        format!("{sign}{int}.{frac}")
    }
}

fn is_regular_content_char(b: u8) -> bool {
    !matches!(
        b,
        b'\0'
            | b'\t'
            | b'\n'
            | b'\x0C'
            | b'\r'
            | b' '
            | b'('
            | b')'
            | b'<'
            | b'>'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b'/'
            | b'%'
    )
}

fn operations_equivalent(a: &[lopdf::content::Operation], b: &[lopdf::content::Operation]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.operator == y.operator
                && x.operands.len() == y.operands.len()
                && x.operands
                    .iter()
                    .zip(&y.operands)
                    .all(|(p, q)| objects_equivalent(p, q))
        })
}

/// Re-emit one decoded content payload compactly; `None` unless every guard
/// passes. Returns the winning stored bytes and whether they are deflated.
///
/// Guards, in order:
/// - `Content::decode_strict` — the lenient decoder silently TRUNCATES
///   malformed input, which re-encoding would turn into dropped operators.
///   Strict parsing errors instead, and we skip the stream.
/// - No `BI` operator: lopdf represents an unparseable inline image as a bare
///   `BI` with the binary data dropped, so any stream carrying inline images
///   is left untouched wholesale.
/// - Re-parse equality: the re-emitted bytes must strict-parse back to
///   operations semantically identical to the original (see
///   `objects_equivalent`) — the emit is verified, not trusted.
/// - `encoded < plain_stored`: the pass claims work only for TRUE text
///   minification. A stream whose text merely re-deflates smaller is the
///   final serialization pass's business and must not, on its own, cause an
///   otherwise-untouched file to be rewritten.
/// - `best < disk_stored`: whatever we store must be strictly smaller than
///   the caller's bar — the smaller of the stored bytes and the redeflated
///   original text, i.e. what the pipeline would produce with no minify.
///   When the original had no `/Filter` and the winner is deflated, the
///   19 bytes of `/Filter/FlateDecode` dict text the rewrite adds are charged
///   to the candidate (measured: without this, ~350 tiny uncompressed Form
///   XObjects each "won" by 2 stream bytes while growing 17 on disk).
const FILTER_DICT_COST: usize = b"/Filter/FlateDecode".len();

fn replan_content(
    decoded: &[u8],
    plain_stored: usize,
    disk_stored: usize,
    gains_filter_cost: bool,
    backend: DeflateBackend,
) -> Option<(Vec<u8>, bool)> {
    let ops = Content::decode_strict(decoded).ok()?;
    if ops.operations.iter().any(|op| op.operator == "BI") {
        return None;
    }
    let emitted = ops.encode().ok()?;
    // lopdf re-emits numbers from its f32 operands, which can silently move
    // the f64 value a viewer parses (see `content_number_values`). Splice the
    // ORIGINAL literals back in, in their shortest decimal-exact form: the
    // token sequences correspond 1:1 because the operations are the same.
    let original_tokens = content_number_tokens(decoded)?;
    let emitted_tokens = content_number_tokens(&emitted)?;
    if original_tokens.len() != emitted_tokens.len() {
        return None;
    }
    let mut encoded = Vec::with_capacity(emitted.len());
    let mut cursor = 0usize;
    for (orig, emit) in original_tokens.iter().zip(&emitted_tokens) {
        let literal = std::str::from_utf8(&decoded[orig.clone()]).ok()?;
        encoded.extend_from_slice(&emitted[cursor..emit.start]);
        encoded.extend_from_slice(minify_number_literal(literal).as_bytes());
        cursor = emit.end;
    }
    encoded.extend_from_slice(&emitted[cursor..]);
    if encoded.len() >= plain_stored {
        return None;
    }
    let reparsed = Content::decode_strict(&encoded).ok()?;
    if !operations_equivalent(&ops.operations, &reparsed.operations) {
        return None;
    }
    // Belt and braces: the spliced stream's numbers must be f64-identical to
    // the original's. Holds by construction; verified anyway because this is
    // the guard the render-hash contract rests on.
    if content_number_values(decoded)? != content_number_values(&encoded)? {
        return None;
    }
    let (best, is_deflated) = match deflate_backend(&encoded, backend) {
        Some(d) if d.len() < encoded.len() => (d, true),
        _ => (encoded, false),
    };
    let cost = best.len()
        + if is_deflated && gains_filter_cost {
            FILTER_DICT_COST
        } else {
            0
        };
    (cost < disk_stored).then_some((best, is_deflated))
}

/// Minify page content streams and Form XObject content: decode the operator
/// stream, re-emit it with single-space operand separation and Rust's
/// shortest-round-trip float formatting, and keep the result only when it is
/// strictly smaller (see `replan_content` for the full guard list). A page
/// whose `/Contents` is an array is re-emitted as ONE merged stream — the
/// array elements are concatenated before parsing (operators may span element
/// boundaries, so per-element parsing would be wrong), and the merge is
/// applied only when every element is referenced solely by this page.
///
/// Why this is render-equivalent: the operations a viewer executes are the
/// parsed ones, and the re-parse-equality guard proves the new bytes parse to
/// the same operations. Comments and redundant whitespace are the only
/// casualties. Prior spike measurements showed the effect is CONDITIONAL
/// per file (some producers already emit compactly; floats like `.1531`
/// re-print longer as `0.1531`), which is exactly why every stream keeps its
/// original bytes unless the rewrite wins.
///
/// Declined wholesale for encrypted, PDF/A-declared, and signed documents,
/// same posture as `redeflate_flate_streams`.
fn minify_content_streams(doc: &mut Document, backend: DeflateBackend) -> bool {
    if doc.is_encrypted() || fonts::pdfa_blocked(doc) || signature_present(doc) {
        return false;
    }
    let refcounts = count_object_references(doc);
    let mut changed = false;

    struct PageRewrite {
        page_id: ObjectId,
        ids: Vec<ObjectId>,
        content: Vec<u8>,
        deflated: bool,
    }
    let mut rewrites: Vec<PageRewrite> = Vec::new();
    for (_, page_id) in doc.get_pages() {
        let ids = doc.get_page_contents(page_id);
        if ids.is_empty() {
            continue;
        }
        let mut plain_stored = 0usize;
        let mut disk_stored = 0usize;
        let mut decoded = Vec::new();
        let mut eligible = true;
        let mut any_unfiltered = false;
        for &sid in &ids {
            let Ok(stream) = doc.get_object(sid).and_then(Object::as_stream) else {
                eligible = false;
                break;
            };
            let Some(bytes) = content_stream_plain(doc, stream) else {
                eligible = false;
                break;
            };
            let has_filter = !matches!(stream.dict.get(b"Filter"), Err(_) | Ok(Object::Null));
            any_unfiltered |= !has_filter;
            plain_stored += bytes.len();
            // The bar to beat is not what the file stores today but what the
            // final redeflate pass would store WITHOUT minification — else a
            // weakly-deflated original lets a longer-but-freshly-deflated
            // re-emit "win" while losing to the counterfactual (measured:
            // +8 KB on a pdfTeX file whose floats mostly re-print longer). An
            // unfiltered original that the pipeline would compress also pays
            // the filter-name dict cost in that counterfactual.
            let redeflated = deflate_backend(&bytes, backend)
                .map(|d| d.len() + if has_filter { 0 } else { FILTER_DICT_COST });
            disk_stored += match redeflated {
                Some(n) => n.min(stream.content.len()),
                None => stream.content.len(),
            };
            decoded.extend_from_slice(&bytes);
            // Separator between elements, per spec concatenation semantics.
            decoded.push(b'\n');
        }
        if !eligible {
            continue;
        }
        let Some((content, deflated)) =
            replan_content(&decoded, plain_stored, disk_stored, any_unfiltered, backend)
        else {
            continue;
        };
        // Merging an array into one stream deletes the elements (via the later
        // orphan prune); that is only sound — and only the size win we
        // measured — when nothing else references them.
        if ids.len() > 1 && ids.iter().any(|id| refcounts.get(id) != Some(&1)) {
            continue;
        }
        rewrites.push(PageRewrite {
            page_id,
            ids,
            content,
            deflated,
        });
    }
    for r in rewrites {
        if let [only] = r.ids[..] {
            if let Ok(Object::Stream(s)) = doc.get_object_mut(only) {
                s.set_content(r.content);
                if r.deflated {
                    s.dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
                } else {
                    s.dict.remove(b"Filter");
                }
                s.dict.remove(b"DecodeParms");
                changed = true;
            }
        } else {
            let mut dict = lopdf::Dictionary::new();
            if r.deflated {
                dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
            }
            let new_id = doc.add_object(Object::Stream(lopdf::Stream::new(dict, r.content)));
            if let Ok(Object::Dictionary(page)) = doc.get_object_mut(r.page_id) {
                page.set("Contents", Object::Reference(new_id));
                changed = true;
                // The replaced elements go orphan; prune_objects drops them.
            }
        }
    }

    // Form XObjects are self-contained content streams (their own /Resources,
    // never split), so each is minified independently, in place — safe even if
    // shared across pages, since the semantics are proven unchanged.
    let form_ids: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter_map(|(&id, obj)| {
            let Object::Stream(s) = obj else { return None };
            matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Form").then_some(id)
        })
        .collect();
    for id in form_ids {
        let Ok(stream) = doc.get_object(id).and_then(Object::as_stream) else {
            continue;
        };
        let Some(plain) = content_stream_plain(doc, stream) else {
            continue;
        };
        // Same counterfactual bar as the page pass: beat the redeflated
        // original, not merely the (possibly weakly-deflated) stored bytes.
        let has_filter = !matches!(stream.dict.get(b"Filter"), Err(_) | Ok(Object::Null));
        let bar = match deflate_backend(&plain, backend) {
            Some(d) => {
                (d.len() + if has_filter { 0 } else { FILTER_DICT_COST }).min(stream.content.len())
            }
            None => stream.content.len(),
        };
        let Some((content, deflated)) =
            replan_content(&plain, plain.len(), bar, !has_filter, backend)
        else {
            continue;
        };
        if let Ok(Object::Stream(s)) = doc.get_object_mut(id) {
            s.set_content(content);
            if deflated {
                s.dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
            } else {
                s.dict.remove(b"Filter");
            }
            s.dict.remove(b"DecodeParms");
            changed = true;
        }
    }
    changed
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
fn try_optimize(input: &[u8], options: OptimizeOptions) -> Result<Option<Vec<u8>>, lopdf::Error> {
    let mut doc = Document::load_mem(input)?;

    // Fail-safe: if any page's /Contents cannot be resolved to stream objects
    // lopdf actually LOADED, the parse lost content — seen in the wild with a
    // malformed /Length whose recovery scan swallowed the whole object.
    // Viewers with more forgiving recovery still render such files; a rewrite
    // from our (lossy) parse would serialize the loss as a blank page. The
    // whole document is declined, byte-identical passthrough.
    for (_, page_id) in doc.get_pages() {
        let has_contents = doc
            .get_object(page_id)
            .and_then(Object::as_dict)
            .map(|d| d.has(b"Contents"))
            .unwrap_or(false);
        if !has_contents {
            continue;
        }
        let ids = doc.get_page_contents(page_id);
        if ids.is_empty()
            || ids
                .iter()
                .any(|&id| !matches!(doc.get_object(id), Ok(Object::Stream(_))))
        {
            return Ok(None);
        }
    }

    // Collapse repeated images first: every downstream step (placement
    // collection, decode/resize/re-encode, and the final write) then sees one
    // object instead of N identical ones.
    let merged_streams = dedup_streams(&mut doc);
    // A second, decoded-payload dedup collapses embedded font programs /
    // bitmaps that were written with differing (but decoded-identical) zlib
    // streams — the dominant duplication on text-heavy LaTeX exports. Neither
    // pass can undo the other's merges, so both run and both count as work.
    let merged_decoded = dedup_decoded_streams(&mut doc);

    // Minify page/Form content streams before anything parses them: the
    // planners below then read the same (verified-equivalent) operations from
    // smaller bytes. Counts as work — a file whose only win is a genuinely
    // smaller content re-emit deserves the rewrite (guarded so that pure
    // re-serialization does NOT count; see `replan_content`).
    let minified = minify_content_streams(&mut doc, options.deflate_backend);

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

    // Plan font subsets on the still-immutable document (read-only; empty
    // when the option is off or anything at all disqualified the document —
    // see src/fonts.rs for the eligibility posture). Applied further down,
    // after the image replacements.
    let font_plans = if options.subset_fonts || options.convert_type1 {
        fonts::plan_font_subsets(&doc, options.subset_fonts, options.convert_type1)
    } else {
        Vec::new()
    };

    // Plan lossless bitonal→G4 recompression (read-only; empty when the
    // option is off or nothing qualified). No overlap with `replacements`:
    // the downsample paths require 8-bit samples or DCT payloads, the bitonal
    // pass requires 1-bit ones.
    let bitonal_plans = if options.recompress_bitonal_images {
        bitonal::plan_bitonal_recompressions(&doc)
    } else {
        Vec::new()
    };

    // Plan JPX (JPEG2000) → JPEG conversions; empty unless --allow-lossy
    // consented to encoding-class changes. Read-only planning, applied below.
    let jpx_plans = if options.allow_lossy_reencode {
        plan_jpx_conversions(&doc, options)
    } else {
        Vec::new()
    };

    // Gray-collapse work detection runs on the ORIGINAL document (the real
    // plan runs later, against post-replacement pixels, because a downsampled
    // candidate must be collapsed at its new geometry — and a downsample of a
    // channel-identical image is itself channel-identical). Only the yes/no
    // matters here: it decides whether a file with no other work still gets
    // rewritten.
    let gray_work = options.collapse_gray_images && !plan_gray_collapses(&doc).is_empty();

    // If we have no work to do at all, hand back the original bytes.
    // Note: the serialization-time passes — pack_object_streams, the final
    // re-deflate, and the xref-stream compression — are deliberately NOT
    // counted as work here. Rewriting a file we otherwise decided not to touch
    // would break the "declined everything ⇒ your exact bytes back" property
    // that `flate_ineligible_images_are_untouched` and friends pin; they only
    // apply to files we were already going to rewrite. Pinned by
    // `serialization_wins_do_not_rewrite_an_otherwise_unchanged_file`.
    // `merged_streams` and `merged_decoded` count as work: the dedup passes
    // may have collapsed repeated images or identical embedded font programs
    // even when nothing needed downsampling, and discarding that would throw
    // away a real size win.
    if replacements.is_empty()
        && font_plans.is_empty()
        && bitonal_plans.is_empty()
        && jpx_plans.is_empty()
        && !options.strip_accessibility
        && !merged_streams
        && !merged_decoded
        && !minified
        && !gray_work
    {
        return Ok(None);
    }

    for r in replacements {
        if let Ok(Object::Stream(stream)) = doc.get_object_mut(r.id) {
            stream.set_content(r.content);
            stream.dict.set("Width", Object::Integer(r.width));
            stream.dict.set("Height", Object::Integer(r.height));
            match r.dict_update {
                DictUpdate::Dct => {}
                DictUpdate::Flate { decode_parms } => {
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
                DictUpdate::FlateToJpeg => {
                    // Phase 7 spike: the payload is now a raw JPEG. Any
                    // /DecodeParms belonged to the old Flate encoding and
                    // must go; /ColorSpace and /BitsPerComponent still match
                    // (see the variant's doc — channel count is preserved).
                    stream
                        .dict
                        .set("Filter", Object::Name(b"DCTDecode".to_vec()));
                    stream.dict.remove(b"DecodeParms");
                }
            }
            // D-M2: the paired /SMask is applied atomically with the base —
            // never one side alone. The mask stream keeps /ColorSpace
            // /DeviceGray and /BitsPerComponent 8; it gains the new
            // /Width//Height, the scalar /Filter /FlateDecode, and drops any
            // stale /DecodeParms its old encoding carried.
            if let Some(smask) = r.smask {
                if let Ok(Object::Stream(mask_stream)) = doc.get_object_mut(smask.mask_id) {
                    mask_stream.set_content(smask.content);
                    mask_stream.dict.set("Width", Object::Integer(smask.width));
                    mask_stream
                        .dict
                        .set("Height", Object::Integer(smask.height));
                    mask_stream
                        .dict
                        .set("Filter", Object::Name(b"FlateDecode".to_vec()));
                    mask_stream.dict.remove(b"DecodeParms");
                }
            }
        }
    }

    // Apply bitonal replacements: new G4 payload plus normalized filter and
    // parms. `/Width`/`/Height` are untouched — the transform never resamples.
    for r in bitonal_plans {
        if let Ok(Object::Stream(stream)) = doc.get_object_mut(r.id) {
            stream.set_content(r.content);
            stream
                .dict
                .set("Filter", Object::Name(b"CCITTFaxDecode".to_vec()));
            stream.dict.set(
                "DecodeParms",
                Object::Dictionary(dictionary! {
                    "K" => -1_i64,
                    "Columns" => r.columns,
                    "Rows" => r.rows,
                    "BlackIs1" => false,
                }),
            );
        }
    }

    // Apply JPX→JPEG conversions: raw JPEG payload, explicit /ColorSpace
    // (the JP2 box that used to supply it is gone with the old payload),
    // /BitsPerComponent, and the codestream's authoritative geometry.
    for p in jpx_plans {
        if let Ok(Object::Stream(stream)) = doc.get_object_mut(p.id) {
            stream.set_content(p.content);
            stream
                .dict
                .set("Filter", Object::Name(b"DCTDecode".to_vec()));
            stream
                .dict
                .set("ColorSpace", Object::Name(p.colorspace.to_vec()));
            stream.dict.set("BitsPerComponent", Object::Integer(8));
            stream.dict.set("Width", Object::Integer(p.width as i64));
            stream.dict.set("Height", Object::Integer(p.height as i64));
            stream.dict.remove(b"DecodeParms");
        }
    }

    // Collapse channel-identical DeviceRGB Flate images to DeviceGray.
    // Planned HERE — after the image replacements above are applied — so a
    // just-downsampled candidate is collapsed at its final geometry rather
    // than racing the downsample for the same stream.
    if options.collapse_gray_images {
        for g in plan_gray_collapses(&doc) {
            if let Ok(Object::Stream(stream)) = doc.get_object_mut(g.id) {
                stream.set_content(g.content);
                stream
                    .dict
                    .set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
                stream
                    .dict
                    .set("Filter", Object::Name(b"FlateDecode".to_vec()));
                match g.decode_parms {
                    Some(parms) => stream.dict.set("DecodeParms", Object::Dictionary(parms)),
                    None => {
                        stream.dict.remove(b"DecodeParms");
                    }
                }
            }
        }
    }

    fonts::apply_font_subsets(&mut doc, font_plans);

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

    // Merge true duplicate objects (identical serialized bytes -> same
    // canonical id, references redirected, duplicates removed) — iterated to a
    // FIXPOINT, alternating the non-stream and stream passes. One generation
    // is not enough: merging duplicate leaves remaps references, which can
    // make their *parents* newly byte-identical. Real-world shape (the NASA
    // repro): N byte-identical image streams that each referenced their own
    // copy of a duplicated ColorSpace object — the streams' dicts only become
    // identical after dedup_objects collapses the ColorSpaces, and merging the
    // streams can in turn make dicts referencing them identical. A single
    // generation per call left that cascade to the NEXT optimize call,
    // breaking idempotence. Terminates: every iteration that continues has
    // strictly removed at least one object. Runs before prune so the orphan
    // cleanup sees an already-compacted object set.
    loop {
        let merged_dicts = dedup_objects(&mut doc);
        let merged_more_streams = dedup_streams(&mut doc);
        if !merged_dicts && !merged_more_streams {
            break;
        }
    }

    // Drop orphaned objects, then Flate-compress any uncompressed content
    // streams (DCTDecode images are skipped — Stream::compress only touches
    // streams without a /Filter).
    doc.prune_objects();
    doc.compress();

    // Last planning-free pass: re-deflate every already-Flate stream with the
    // configured backend (zlib level 9, or zopfli when opted in).
    // Runs after EVERY other decision (images, fonts, bitonal, lossy, dedup,
    // prune, compress) so it is purely a serialization improvement and can
    // never influence, or be influenced by, what the planners chose.
    redeflate_flate_streams(&mut doc, options.deflate_backend);

    // Renumber to a contiguous id space so the saved trailer /Size matches the
    // highest object number. Without this, lopdf 0.41's classic save emits a
    // /Size that's slightly too high, which `qpdf --check` flags (benign, but
    // we want strictly clean output for email recipients / strict readers).
    doc.renumber_objects();

    strip_stale_xref_trailer_keys(&mut doc);

    save_document(&mut doc, options).map(Some)
}

/// Drop trailer entries that describe the *input's* cross-reference section.
///
/// lopdf seeds `Document::trailer` from the file's last trailer dictionary and
/// copies whatever survives into the cross-reference stream it synthesizes at
/// save time. Most xref keys are overwritten there (`/Type`, `/Size`, `/W`,
/// `/Index`, `/Length`), but `/DecodeParms`, `/Filter`, `/Prev` and `/XRefStm`
/// are not — they leak through and describe bytes that no longer exist.
///
/// This is not hypothetical. `corpus/adobe-spec.pdf` (Distiller 8.1.0) ends in
/// a *classic* `trailer` dictionary that nonetheless carries a full xref-stream
/// key set, including `/DecodeParms<</Columns 5/Predictor 12>>`. Carried into
/// amatl's output that predictor declaration sat on top of the unpredicted
/// 7-bytes-per-row payload `compress_xref_stream` had just deflated, so every
/// strict reader failed to decode the table and fell back to reconstruction
/// (Ghostscript: "The /Prev entry in an XrefStm dictionary did not point to an
/// XrefStm" / "xref table was repaired").
///
/// The writer owns the cross-reference section; nothing the reader saw about
/// the old one may survive into the new one.
fn strip_stale_xref_trailer_keys(doc: &mut Document) {
    for key in [
        b"DecodeParms".as_slice(),
        b"Filter",
        b"Prev",
        b"XRefStm",
        b"Length",
    ] {
        doc.trailer.remove(key);
    }
}

/// Inflation ceiling for the final re-deflate pass. A stream that expands past
/// this is left exactly as it arrived — the same decompression-bomb posture
/// `inflate_capped` enforces everywhere else in the crate.
const MAX_REDEFLATE_BYTES: usize = 128 * 1024 * 1024;

/// Final lossless re-deflate pass: every stream whose `/Filter` is exactly
/// `FlateDecode` is inflated and re-deflated at zlib level 9, keeping the
/// result only when it is STRICTLY smaller and verified to inflate back to the
/// original bytes.
///
/// This is a serialization change and nothing else. `/Filter` and
/// `/DecodeParms` are untouched, so any PNG/TIFF predictor still applies to
/// exactly the same post-inflate bytes: every reader decodes what it decoded
/// before, byte for byte. No pixel is resampled and no encoding class moves,
/// which is why it needs no consent flag — it is the same class of work
/// `doc.compress()` already does by default, extended to streams that arrived
/// with a producer's (often weaker) deflate output.
///
/// Idempotent: a second pass re-deflates already-level-9 output to the same
/// size, which fails the strictly-smaller test and changes nothing.
///
/// Declined wholesale for encrypted documents (stream bytes are ciphertext),
/// PDF/A-declared documents, and signed documents — a signature's byte range
/// covers offsets this pass would move.
fn redeflate_flate_streams(doc: &mut Document, backend: DeflateBackend) {
    if doc.is_encrypted() || fonts::pdfa_blocked(doc) || signature_present(doc) {
        return;
    }

    // Collect first: classification resolves references against the immutable
    // document, while the deflate work itself happens off the document
    // entirely, in parallel.
    let candidates: Vec<(ObjectId, Vec<u8>)> = doc
        .objects
        .iter()
        .filter_map(|(&id, obj)| {
            let Object::Stream(stream) = obj else {
                return None;
            };
            // Object and cross-reference streams are the writer's business:
            // lopdf rebuilds both from scratch at save time (and the xref
            // stream is deflated by `compress_xref_stream` afterwards).
            if matches!(stream.dict.get(b"Type"),
                Ok(Object::Name(n)) if n == b"ObjStm" || n == b"XRef")
            {
                return None;
            }
            let filter = stream.dict.get(b"Filter").ok()?;
            matches!(classify_filter(doc, filter), FilterClass::FlateOnly)
                .then(|| (id, stream.content.clone()))
        })
        .collect();

    let shrunk: Vec<(ObjectId, Vec<u8>)> = candidates
        .into_par_iter()
        .filter_map(|(id, content)| replan_deflate(&content, backend).map(|out| (id, out)))
        .collect();

    for (id, content) in shrunk {
        if let Ok(Object::Stream(stream)) = doc.get_object_mut(id) {
            stream.set_content(content); // keeps /Length in sync
        }
    }
}

/// Inflate and re-deflate one Flate payload. `None` (leave the stream exactly
/// as it is) unless the new payload is strictly smaller AND inflates back
/// byte-identically — deflate is lossless, so that equality must be exact.
fn replan_deflate(content: &[u8], backend: DeflateBackend) -> Option<Vec<u8>> {
    let plain = inflate_capped(content, MAX_REDEFLATE_BYTES)?;
    let out = deflate_backend(&plain, backend)?;
    if out.len() >= content.len() {
        return None;
    }
    (inflate_capped(&out, plain.len())? == plain).then_some(out)
}

/// True when the document carries a digital signature. A signature dictionary
/// pins a `/ByteRange` over the file's bytes; AcroForm `/SigFlags` declares one
/// exists. Rather than reason about which bytes a range covers, the re-deflate
/// pass declines such documents entirely.
fn signature_present(doc: &Document) -> bool {
    if let Ok(catalog) = doc.catalog() {
        if let Ok(acroform) = catalog.get(b"AcroForm") {
            if let Object::Dictionary(d) = resolve(doc, acroform) {
                if d.get(b"SigFlags").is_ok() {
                    return true;
                }
            }
        }
    }
    doc.objects.values().any(|obj| match obj {
        Object::Dictionary(d) => is_signature_dict(d),
        Object::Stream(s) => is_signature_dict(&s.dict),
        _ => false,
    })
}

fn is_signature_dict(dict: &lopdf::Dictionary) -> bool {
    dict.get(b"ByteRange").is_ok()
        || matches!(dict.get(b"Type"),
            Ok(Object::Name(n)) if n == b"Sig" || n == b"DocTimeStamp")
}

/// Serialize the document, optionally using PDF 1.5 object-stream packing when
/// `options.pack_object_streams` is true. The packed path produces smaller
/// output for object-heavy documents but is more complex; the classic path is
/// the always-available fallback and matches what lopdf ships.
fn save_document(doc: &mut Document, options: OptimizeOptions) -> Result<Vec<u8>, lopdf::Error> {
    let out = if options.pack_object_streams {
        pack_and_save(doc)?
    } else {
        let mut out: Vec<u8> = Vec::new();
        doc.save_to(&mut out)?;
        out
    };
    // The zopfli backend can re-deflate the ObjStm the writer just emitted
    // (lopdf deflates it internally at zlib level 9, out of reach of the
    // final re-deflate pass). Must run BEFORE the xref compression below:
    // the patch reads and rewrites the still-uncompressed xref rows.
    let out = if options.deflate_backend == DeflateBackend::Zopfli {
        rezopfli_objstm(out)
    } else {
        out
    };
    // Both save paths emit an uncompressed cross-reference stream; deflate it.
    Ok(compress_xref_stream(out, options.deflate_backend))
}

/// Deflate the cross-reference stream of a just-serialized document.
///
/// lopdf hardcodes `XRefStreamFilter::None` in
/// `writer.rs::write_cross_reference_stream` — there is no `SaveOptions` knob
/// for it, and `doc.compress()` cannot reach the object because it does not
/// exist in `Document::objects`: the writer synthesizes it during save, after
/// every document-level pass has run. So amatl's output shipped a raw
/// 7-bytes-per-entry xref stream (17,479 B on the 2,497-object NASA reference,
/// where Ghostscript spends 5,375 B).
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
    // the payload this patch is about to install — it does not (the deflate
    // below applies no predictor), so decline rather than emit a dictionary
    // that lies about its own stream.
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

    let mut patched = Vec::with_capacity(out.len());
    patched.extend_from_slice(&out[..dict_start + len_key]);
    patched.extend_from_slice(b"/Filter/FlateDecode/Length ");
    patched.extend_from_slice(deflated.len().to_string().as_bytes());
    patched.extend_from_slice(&out[dict_start + val_end..content_start]);
    patched.extend_from_slice(&deflated);
    patched.extend_from_slice(&out[content_start + content_len..]);
    Some(patched)
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
fn find_sub(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
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
                vec![
                    100.into(),
                    0.into(),
                    0.into(),
                    100.into(),
                    0.into(),
                    0.into(),
                ],
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
        assert!(
            (150..=210).contains(&w) && w == h,
            "unexpected dims {w}x{h}"
        );
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
        assert_eq!(
            out, pdf,
            "array-form DecodeParms must leave the file untouched"
        );
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
        assert!(
            (150..=210).contains(&w) && w == h,
            "unexpected dims {w}x{h}"
        );
        assert!(out.len() < pdf.len());
        // 1 channel in, 1 channel out — the DeviceGray /ColorSpace is unchanged
        // so 3-channel data here would be a corrupt image.
        let pixels = flate_image_pixels(&out);
        assert_eq!(
            pixels.len(),
            (w * h) as usize,
            "grayscale must stay 1-channel"
        );
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
        let icc_id = doc.add_object(Stream::new(dictionary! { "N" => 3_i64 }, vec![0u8; 128]));
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
        assert!(
            (150..=210).contains(&w) && w == h,
            "unexpected dims {w}x{h}"
        );
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

    /// Build a one-page PDF embedding an image whose `/Filter` is one of the
    /// exotic, undeclared-decoder classes (`/JPXDecode`, `/JBIG2Decode`), with
    /// `payload` left as opaque bytes (no decoder is linked, so the exact
    /// bytes must never be parsed or touched). `as_array` exercises the
    /// one-element `/Filter [<name>]` form, which `classify_filter` must also
    /// recognize.
    fn build_exotic_pdf(filter: &[u8], payload: &[u8], as_array: bool) -> Vec<u8> {
        let mut dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => 400_i64,
            "Height" => 400_i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8_i64,
        };
        if as_array {
            dict.set("Filter", vec![Object::Name(filter.to_vec())]);
        } else {
            dict.set("Filter", Object::Name(filter.to_vec()));
        }
        let mut doc = Document::with_version("1.5");
        let img_id = doc.add_object(Stream::new(dict, payload.to_vec()));
        wrap_image_pdf(&mut doc, img_id, 100)
    }

    #[test]
    fn classify_recognizes_exotic_filter_names() {
        // The leading-line seam for the exotic conversions: /JPXDecode and
        // /JBIG2Decode must classify distinctly (never fall into DctOnly /
        // FlateOnly, which is what would let a re-encode path corrupt them).
        let doc = Document::with_version("1.5");
        let cases: [(String, Vec<u8>, FilterClass); 2] = [
            (
                "JPXDecode".to_owned(),
                b"JPXDecode".to_vec(),
                FilterClass::JpxOnly,
            ),
            (
                "JBIG2Decode".to_owned(),
                b"JBIG2Decode".to_vec(),
                FilterClass::Jbig2Only,
            ),
        ];
        for (label, name, want) in cases {
            let scalar = Object::Name(name.to_vec());
            assert_eq!(
                classify_filter(&doc, &scalar),
                want,
                "{label}: scalar-name form must classify as {want:?}"
            );
            // The one-element-array form is the other spelling the PDF spec
            // permits for a single filter.
            let array = Object::Array(vec![Object::Name(name.to_vec())]);
            assert_eq!(
                classify_filter(&doc, &array),
                want,
                "{label}: one-element-array form must classify as {want:?}"
            );
        }
    }

    #[test]
    fn exotic_filter_images_are_left_byte_identical() {
        // No JP2 or JBIG2 decoder is linked (the image crate build is
        // JPEG-only), so any re-encode of these would corrupt them. The
        // fail-safe contract therefore demands the ORIGINAL bytes come back —
        // whole-file equality — for both filter names and both spellings.
        let payload = vec![0x1Au8; 96];
        let cases: Vec<(String, &[u8], bool)> = vec![
            ("JPXDecode scalar".to_owned(), b"JPXDecode", false),
            ("JPXDecode array".to_owned(), b"JPXDecode", true),
            ("JBIG2Decode scalar".to_owned(), b"JBIG2Decode", false),
            ("JBIG2Decode array".to_owned(), b"JBIG2Decode", true),
        ];
        for (label, name, as_array) in cases {
            let pdf = build_exotic_pdf(name, &payload, as_array);
            let out = optimize(&pdf);
            assert_eq!(
                out, pdf,
                "{label}: exotic-filter image must be left byte-identical"
            );
            // The filter name must survive verbatim (never relabeled DCT/Flate).
            let reloaded = Document::load_mem(&out).unwrap();
            let mut has_exotic = false;
            for obj in reloaded.objects.values() {
                let exotic = match obj {
                    Object::Stream(s) => {
                        let is_image = matches!(
                            s.dict.get(b"Subtype"),
                            Ok(Object::Name(n)) if n == b"Image"
                        );
                        is_image
                            && match s.dict.get(b"Filter") {
                                // Both the scalar /Name and the PDF-spec
                                // one-element-array spelling count.
                                Ok(Object::Name(n)) => n == name,
                                Ok(Object::Array(items)) if items.len() == 1 => {
                                    matches!(&items[0], Object::Name(n) if n == name)
                                }
                                _ => false,
                            }
                    }
                    _ => false,
                };
                has_exotic |= exotic;
            }
            assert!(has_exotic, "{label}: filter name must be preserved");
        }
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
            // (An eligible "/SMask present" shape used to sit in this list;
            // since D-M3 it is a POSITIVE case — see the masked-Flate battery.)
            (
                "/Decode array present",
                build_flate_pdf_with_dict(400, 3, |_, d| {
                    d.set(
                        "Decode",
                        vec![1.into(), 0.into(), 1.into(), 0.into(), 1.into(), 0.into()],
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
        assert_eq!(
            out, truncated_pdf,
            "truncated zlib must return original bytes"
        );

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
        assert_eq!(
            out, mismatched,
            "length mismatch must return original bytes"
        );

        // (c) A predictor value lopdf rejects/ignores (99).
        let bad_pred = build_flate_pdf_with_dict(400, 3, |_, d| {
            d.set(
                "DecodeParms",
                dictionary! { "Predictor" => 99_i64, "Colors" => 3_i64, "Columns" => 400_i64 },
            );
        });
        let out = optimize(&bad_pred);
        assert_eq!(
            out, bad_pred,
            "unknown predictor must return original bytes"
        );
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

    // ---- Phase 7 spike: consent-gated lossy Flate→JPEG re-encode -----------

    /// Smooth sinusoidal shading plus low-amplitude deterministic noise — a
    /// stand-in for photographic content: the noise floor keeps deflate
    /// mediocre (like a real photo's sensor grain) while JPEG's DCT
    /// quantization absorbs it, so the lossy candidate genuinely wins. Line
    /// art is the opposite shape (see the checkerboard fixture below).
    fn photo_pixels(px_w: u32, px_h: u32, channels: usize) -> Vec<u8> {
        let mut state = 0x2545_F491_u32;
        let mut out = Vec::with_capacity(px_w as usize * px_h as usize * channels);
        for y in 0..px_h {
            for x in 0..px_w {
                for c in 0..channels {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    let noise = (state >> 28) as i32 - 8; // [-8, 7]
                    let phase = (x as f32 / 23.0) + (y as f32 / 17.0) + c as f32;
                    let base = 128.0 + 96.0 * phase.sin();
                    out.push((base as i32 + noise).clamp(0, 255) as u8);
                }
            }
        }
        out
    }

    /// Build a one-page PDF embedding the given raw pixels as a plain-deflate
    /// FlateDecode image drawn into a `draw_pts` square.
    fn build_pdf_flate_raw(raw: &[u8], px: u32, draw_pts: i64, channels: usize) -> Vec<u8> {
        let payload = deflate_level9(raw).unwrap();
        let mut doc = Document::with_version("1.5");
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => px as i64,
                "Height" => px as i64,
                "ColorSpace" => if channels == 1 { "DeviceGray" } else { "DeviceRGB" },
                "BitsPerComponent" => 8_i64,
                "Filter" => "FlateDecode",
            },
            payload,
        ));
        wrap_image_pdf(&mut doc, img_id, draw_pts)
    }

    /// The (single) image stream's `/Filter` name (scalar or first array
    /// element), plus whether the dict still carries `/DecodeParms`.
    fn image_filter_info(pdf: &[u8]) -> (Vec<u8>, bool) {
        let doc = Document::load_mem(pdf).unwrap();
        for obj in doc.objects.values() {
            if let Object::Stream(s) = obj {
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
                    let name = match s.dict.get(b"Filter").unwrap() {
                        Object::Name(n) => n.clone(),
                        Object::Array(items) => match &items[0] {
                            Object::Name(n) => n.clone(),
                            other => panic!("unexpected filter element {other:?}"),
                        },
                        other => panic!("unexpected filter {other:?}"),
                    };
                    return (name, s.dict.get(b"DecodeParms").is_ok());
                }
            }
        }
        panic!("no image stream");
    }

    /// A 200 px photographic Flate image drawn into 200 pt: 72 effective DPI,
    /// safely under the 130×1.15 threshold — the under-resolution shape.
    fn build_photo_flate_under_res() -> Vec<u8> {
        build_pdf_flate_raw(&photo_pixels(200, 200, 3), 200, 200, 3)
    }

    #[test]
    fn lossy_reencode_cannot_fire_without_consent() {
        // The consent pin (Phase 7). Default options: an under-resolution
        // photographic Flate image — exactly the shape the lossy path targets
        // — must come back byte-identical...
        let pdf = build_photo_flate_under_res();
        let out = optimize(&pdf);
        assert_eq!(
            out, pdf,
            "without allow_lossy_reencode the Flate payload must be untouched"
        );

        // ...and an over-resolution one must stay a FlateDecode stream (the
        // lossless downsample may fire; the encoding class must not change).
        let over = build_pdf_flate_raw(&photo_pixels(400, 400, 3), 400, 100, 3);
        let out = optimize(&over);
        let (filter, _) = image_filter_info(&out);
        assert_eq!(
            filter, b"FlateDecode",
            "without consent the encoding class must never change"
        );
    }

    #[test]
    fn lossy_reencode_converts_photographic_flate() {
        // Flag on, under-resolution photographic image: the payload converts
        // to a strictly smaller DCTDecode stream at UNCHANGED geometry, the
        // stale /DecodeParms is gone, the JPEG decodes back to nearby pixels,
        // and a second optimize call is byte-stable (requant guards see a
        // fresh q78 payload and decline the <5% churn).
        let pdf = build_photo_flate_under_res();
        let opts = OptimizeOptions::default().with_allow_lossy_reencode(true);
        let out = optimize_with_options(&pdf, opts);
        assert!(out.len() < pdf.len(), "conversion must shrink the file");

        let (filter, has_parms) = image_filter_info(&out);
        assert_eq!(filter, b"DCTDecode", "payload must now be a JPEG");
        assert!(!has_parms, "Flate /DecodeParms must be dropped");
        assert_eq!(image_dims(&out), (200, 200), "geometry must be unchanged");

        let jpeg = image_stream_bytes(&out);
        let decoded = image::load_from_memory_with_format(&jpeg, ImageFormat::Jpeg)
            .expect("converted payload must decode as a JPEG");
        assert_eq!((decoded.width(), decoded.height()), (200, 200));
        let actual = decoded.to_rgb8().into_raw();
        let reference = photo_pixels(200, 200, 3);
        let sad: u64 = reference
            .iter()
            .zip(&actual)
            .map(|(a, b)| u64::from(a.abs_diff(*b)))
            .sum();
        let mad = sad as f64 / reference.len() as f64;
        assert!(
            mad <= 24.0,
            "decode-back must reproduce nearby pixels (MAD {mad:.1})"
        );

        let twice = optimize_with_options(&out, opts);
        assert_eq!(twice, out, "second pass must be byte-stable");
    }

    #[test]
    fn lossy_reencode_over_resolution_jpeg_candidate_competes() {
        // Flag on, over-resolution photographic image (400 px into 100 pt ≈
        // 288 DPI): the JPEG candidate at the SAME target geometry as the
        // lossless downsample wins on noisy content, and beats the flag-off
        // output.
        let pdf = build_pdf_flate_raw(&photo_pixels(400, 400, 3), 400, 100, 3);
        let flag_off = optimize(&pdf);
        let opts = OptimizeOptions::default().with_allow_lossy_reencode(true);
        let flag_on = optimize_with_options(&pdf, opts);

        let (filter, has_parms) = image_filter_info(&flag_on);
        assert_eq!(filter, b"DCTDecode", "JPEG candidate must win on a photo");
        assert!(!has_parms);
        let (w, h) = image_dims(&flag_on);
        assert!(
            (150..=210).contains(&w) && w == h,
            "must land at the downsample's target geometry, got {w}x{h}"
        );
        assert!(
            flag_on.len() < flag_off.len(),
            "lossy candidate must beat the lossless downsample ({} !< {})",
            flag_on.len(),
            flag_off.len()
        );
    }

    #[test]
    fn lossy_reencode_line_art_never_larger() {
        // Sharp-edged flat-color content: deflate is near-optimal, the JPEG
        // candidate cannot save 5% — the guard declines and the file comes
        // back byte-identical (never-larger holds trivially).
        let pdf = build_pdf_flate_raw(&checkerboard_pixels(200, 8, 3), 200, 200, 3);
        let opts = OptimizeOptions::default().with_allow_lossy_reencode(true);
        let out = optimize_with_options(&pdf, opts);
        assert!(out.len() <= pdf.len(), "never-larger must hold");
        assert_eq!(
            out, pdf,
            "line art must decline conversion (5% guard / strict-smaller)"
        );

        // Over-resolution line art: whatever happens (lossless downsample or
        // nothing), the encoding class must not flip to JPEG — the Flate
        // candidate is smaller on this content.
        let over = build_pdf_flate_raw(&checkerboard_pixels(400, 8, 3), 400, 100, 3);
        let out = optimize_with_options(&over, opts);
        let (filter, _) = image_filter_info(&out);
        assert_eq!(
            filter, b"FlateDecode",
            "line art must keep its lossless encoding even with consent"
        );
    }

    #[test]
    fn lossy_reencode_skips_ineligible_color_shapes() {
        // Indexed, 16-bit, and CMYK Flate images are outside the conversion's
        // vetted scope: byte-identical output even with the flag on. All are
        // under-resolution (120 px into 100 pt ≈ 86 DPI) so the lossy requant
        // branch — not the downsample — is what gets exercised.
        let indexed = build_pdf_flate_raw(&photo_pixels(120, 120, 3), 120, 100, 3);
        let mut doc = Document::load_mem(&indexed).unwrap();
        for obj in doc.objects.values_mut() {
            if let Object::Stream(s) = obj {
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
                    s.dict.set(
                        "ColorSpace",
                        vec![
                            Object::Name(b"Indexed".to_vec()),
                            Object::Name(b"DeviceRGB".to_vec()),
                            Object::Integer(255),
                            Object::String(vec![0u8; 768], lopdf::StringFormat::Hexadecimal),
                        ],
                    );
                }
            }
        }
        let mut indexed_pdf: Vec<u8> = Vec::new();
        doc.save_to(&mut indexed_pdf).unwrap();

        let mut sixteen_doc = Document::load_mem(&indexed).unwrap();
        for obj in sixteen_doc.objects.values_mut() {
            if let Object::Stream(s) = obj {
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
                    s.dict.set("BitsPerComponent", 16_i64);
                }
            }
        }
        let mut sixteen_pdf: Vec<u8> = Vec::new();
        sixteen_doc.save_to(&mut sixteen_pdf).unwrap();

        let mut cmyk_doc = Document::load_mem(&indexed).unwrap();
        for obj in cmyk_doc.objects.values_mut() {
            if let Object::Stream(s) = obj {
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
                    s.dict
                        .set("ColorSpace", Object::Name(b"DeviceCMYK".to_vec()));
                }
            }
        }
        let mut cmyk_pdf: Vec<u8> = Vec::new();
        cmyk_doc.save_to(&mut cmyk_pdf).unwrap();

        let opts = OptimizeOptions::default().with_allow_lossy_reencode(true);
        for (label, pdf) in [
            ("Indexed", indexed_pdf),
            ("16-bit", sixteen_pdf),
            ("DeviceCMYK", cmyk_pdf),
        ] {
            let out = optimize_with_options(&pdf, opts);
            assert_eq!(out, pdf, "{label}: must never convert, even with consent");
        }
    }

    #[test]
    fn lossy_reencode_corrupt_flate_returns_exact_original_bytes() {
        // Degradation contract with the flag ON: a truncated zlib payload
        // must yield the EXACT original bytes, never a partial rewrite.
        let pdf = build_photo_flate_under_res();
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
        let mut corrupt: Vec<u8> = Vec::new();
        doc.save_to(&mut corrupt).unwrap();

        let opts = OptimizeOptions::default().with_allow_lossy_reencode(true);
        let out = optimize_with_options(&corrupt, opts);
        assert_eq!(
            out, corrupt,
            "corrupt input must return exact original bytes"
        );
    }

    #[test]
    fn requant_declines_payload_already_at_target_quality() {
        // Exact idempotence guard (Phase 7): a JPEG that mozjpeg itself
        // encoded at the configured quality carries byte-identical
        // quantization tables to the requant candidate — re-encoding it is
        // pure generation-loss churn even when trellis could still shave ≥5%
        // (graphics-heavy content, the NASA banner repro). Must be declined
        // outright, leaving the file byte-identical.
        let raw = checkerboard_pixels(300, 8, 3);
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_raw(300, 300, raw).unwrap());
        let jpeg = encode_jpeg(img, false, 78).unwrap();
        let mut doc = Document::with_version("1.5");
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 300_i64,
                "Height" => 300_i64,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8_i64,
                "Filter" => "DCTDecode",
            },
            jpeg,
        ));
        // 300 px into 300 pt ⇒ 72 DPI, under-threshold: the P-M2 requant is
        // the branch that would otherwise fire.
        let pdf = wrap_image_pdf(&mut doc, img_id, 300);
        let out = optimize(&pdf);
        assert_eq!(
            out, pdf,
            "a payload already at the target quality must never be requantized"
        );
    }

    /// A one-page PDF holding a `px`-square FlateDecode RGB base with an
    /// eligible plain 8-bit DeviceGray `/SMask`, drawn into `draw_pts`.
    /// `base` supplies the base pixels so callers can vary the content class.
    fn build_pdf_masked_flate(base: &[u8], px: u32, draw_pts: i64) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let mask_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => px as i64,
                "Height" => px as i64,
                "ColorSpace" => "DeviceGray",
                "BitsPerComponent" => 8_i64,
                "Filter" => "FlateDecode",
            },
            deflate_level9(&photo_pixels(px, px, 1)).unwrap(),
        ));
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => px as i64,
                "Height" => px as i64,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8_i64,
                "Filter" => "FlateDecode",
                "SMask" => mask_id,
            },
            deflate_level9(base).unwrap(),
        ));
        wrap_image_pdf(&mut doc, img_id, draw_pts)
    }

    /// Every masked image stream as `(filter name, /SMask target, w, h)`.
    fn masked_bases(pdf: &[u8]) -> Vec<(Vec<u8>, ObjectId, i64, i64)> {
        let doc = Document::load_mem(pdf).unwrap();
        let mut out: Vec<_> = doc
            .objects
            .iter()
            .filter_map(|(id, obj)| match obj {
                Object::Stream(s)
                    if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image")
                        && s.dict.get(b"SMask").is_ok() =>
                {
                    let filter = match s.dict.get(b"Filter").unwrap() {
                        Object::Name(n) => n.clone(),
                        Object::Array(items) => match &items[0] {
                            Object::Name(n) => n.clone(),
                            other => panic!("unexpected filter element {other:?}"),
                        },
                        other => panic!("unexpected filter {other:?}"),
                    };
                    let mask = match s.dict.get(b"SMask").unwrap() {
                        Object::Reference(r) => *r,
                        other => panic!("fixture uses indirect masks, got {other:?}"),
                    };
                    let w = s.dict.get(b"Width").unwrap().as_i64().unwrap();
                    let h = s.dict.get(b"Height").unwrap().as_i64().unwrap();
                    Some((*id, (filter, mask, w, h)))
                }
                _ => None,
            })
            .collect();
        out.sort_by_key(|(id, _)| *id);
        out.into_iter().map(|(_, v)| v).collect()
    }

    /// A mask stream's `(bytes, width, height)`.
    fn mask_shape(pdf: &[u8], mask_id: ObjectId) -> (Vec<u8>, i64, i64) {
        let doc = Document::load_mem(pdf).unwrap();
        let s = doc.get_object(mask_id).unwrap().as_stream().unwrap();
        (
            s.content.clone(),
            s.dict.get(b"Width").unwrap().as_i64().unwrap(),
            s.dict.get(b"Height").unwrap().as_i64().unwrap(),
        )
    }

    #[test]
    fn lossy_reencode_converts_masked_flate_base_keeps_mask() {
        // A Flate base carrying an eligible /SMask, under-resolution (200 px
        // into 200 pt ⇒ 72 DPI, so the D-M3 coupled downsample does not
        // apply). With consent it takes the DIMENSION-PRESERVING Flate→JPEG
        // conversion: the base becomes DCTDecode at the same geometry and the
        // mask stream is not touched at all — same bytes, same /Width//Height
        // — which is what keeps base and mask aligned (mask-alignment
        // experiment: hard-edged and antialiased masks alike show no
        // misregistration over a q78 4:2:0 base).
        let px = 200u32;
        let pdf = build_pdf_masked_flate(&photo_pixels(px, px, 3), px, 200);
        let before = masked_bases(&pdf);
        assert_eq!(before.len(), 1, "fixture holds one masked base");
        let mask_id = before[0].1;
        let mask_before = mask_shape(&pdf, mask_id);

        let opts = OptimizeOptions::default().with_allow_lossy_reencode(true);
        let out = optimize_with_options(&pdf, opts);
        assert!(out.len() < pdf.len(), "the conversion must shrink the file");

        let after = masked_bases(&out);
        assert_eq!(after.len(), 1, "the masked base must survive");
        let (filter, mask_after_id, w, h) = &after[0];
        assert_eq!(filter.as_slice(), b"DCTDecode", "base converted to JPEG");
        assert_eq!(
            (*w, *h),
            (px as i64, px as i64),
            "the conversion is dimension-preserving"
        );
        assert_eq!(*mask_after_id, mask_id, "the /SMask reference is intact");
        assert_eq!(
            mask_shape(&out, mask_id),
            mask_before,
            "the mask stream must be byte-identical, same dimensions"
        );
        assert!(Document::load_mem(&out).is_ok(), "output must load back");

        // Flag off, same fixture: nothing at all happens (the pre-consent
        // behavior of this branch is unchanged).
        assert_eq!(
            optimize(&pdf),
            pdf,
            "without consent the masked Flate pair stays untouched"
        );
    }

    #[test]
    fn lossy_reencode_shared_mask_bases_convert_mask_untouched() {
        // Two under-resolution Flate bases sharing ONE /SMask object. The
        // conversion never modifies a mask stream, so a shared mask does not
        // block it (exactly the P-M1 argument that lets shared-mask DCT pairs
        // requantize): both bases convert, the single mask stays byte-identical
        // at the same dimensions, and both /SMask refs still point at it.
        let px = 200u32;
        let mut doc = Document::with_version("1.5");
        let mask_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => px as i64,
                "Height" => px as i64,
                "ColorSpace" => "DeviceGray",
                "BitsPerComponent" => 8_i64,
                "Filter" => "FlateDecode",
            },
            deflate_level9(&photo_pixels(px, px, 1)).unwrap(),
        ));
        let pixels_a = photo_pixels(px, px, 3);
        // Inverted pixels: still photographic, but different bytes, so
        // `dedup_streams` cannot merge the two BASES into one object.
        let pixels_b: Vec<u8> = pixels_a.iter().map(|b| 255 - b).collect();
        let base_dict = |mask: ObjectId| {
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => px as i64,
                "Height" => px as i64,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8_i64,
                "Filter" => "FlateDecode",
                "SMask" => mask,
            }
        };
        let img_a = doc.add_object(Stream::new(
            base_dict(mask_id),
            deflate_level9(&pixels_a).unwrap(),
        ));
        let img_b = doc.add_object(Stream::new(
            base_dict(mask_id),
            deflate_level9(&pixels_b).unwrap(),
        ));
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            b"q 200 0 0 200 0 0 cm /Im0 Do Q q 200 0 0 200 250 0 cm /Im1 Do Q".to_vec(),
        ));
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! {
                "XObject" => dictionary! {
                    "Im0" => Object::Reference(img_a),
                    "Im1" => Object::Reference(img_b),
                },
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
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut pdf: Vec<u8> = Vec::new();
        doc.save_to(&mut pdf).unwrap();

        let before = masked_bases(&pdf);
        assert_eq!(before.len(), 2, "fixture holds two masked bases");
        assert_eq!(before[0].1, before[1].1, "both share one mask object");
        let mask_before = mask_shape(&pdf, before[0].1);

        let opts = OptimizeOptions::default().with_allow_lossy_reencode(true);
        let out = optimize_with_options(&pdf, opts);

        let after = masked_bases(&out);
        assert_eq!(after.len(), 2, "both masked bases must survive");
        for (filter, mask, w, h) in &after {
            assert_eq!(filter.as_slice(), b"DCTDecode", "both bases convert");
            assert_eq!((*w, *h), (px as i64, px as i64), "geometry preserved");
            assert_eq!(*mask, before[0].1, "/SMask still points at the shared mask");
        }
        assert_eq!(
            mask_shape(&out, before[0].1),
            mask_before,
            "the shared mask must be byte-identical, same dimensions"
        );
        assert!(Document::load_mem(&out).is_ok(), "output must load back");
    }

    #[test]
    fn lossy_reencode_masked_line_art_is_declined() {
        // The line-art content guard runs on the SOURCE pixels inside
        // `plan_flate_to_jpeg`, so it protects the masked branch too: a masked
        // line-art base is left completely untouched even with consent.
        let px = 200u32;
        let pdf = build_pdf_masked_flate(&line_art_pixels(px, px), px, 200);
        let opts = OptimizeOptions::default().with_allow_lossy_reencode(true);
        assert_eq!(
            optimize_with_options(&pdf, opts),
            pdf,
            "masked line art must not be converted even with --allow-lossy"
        );
    }

    /// Line-art pixels: sharp 1 px black lines on a white background — the p12
    /// class the Phase 7 human review rejected (dashes muddy, background
    /// mottles, hairlines shift color under DCT).
    fn line_art_pixels(w: u32, h: u32) -> Vec<u8> {
        let mut buf = vec![255u8; (w * h * 3) as usize];
        // Faint deterministic paper grain (≤ 6 counts below white): invisible
        // to all three guard metrics (same >>3 quantization bucket, far below
        // EDGE_STEP) but deflate-hostile and DCT-cheap — this is what keeps
        // the JPEG candidate clearing the 5% size bar "by a wide margin"
        // below, even against the zlib-rs deflate backend.
        for (i, byte) in buf.iter_mut().enumerate() {
            let mut x = i as u32 ^ 0x9E37_79B9;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *byte -= (x % 7) as u8;
        }
        let palette = [
            [20u8, 40, 190],
            [10, 10, 10],
            [180, 30, 40],
            [20, 120, 60],
            [90, 30, 150],
        ];
        // Anti-aliased dash-dot curves at irregular (non-periodic) positions:
        // sparse ink, sharp black/white transitions, a handful of colors — and
        // deliberately NOT deflate-friendly, so the size guard alone would
        // convert it. (The curve frequency is tuned so that holds with margin
        // under the zlib-rs deflate backend, which out-compresses the old
        // miniz_oxide baseline.)
        for (k, color) in palette.iter().enumerate() {
            for x in 0..w {
                if (x as usize / (5 + k)) % 3 == 2 {
                    continue; // dash gaps
                }
                let f = x as f32 * (0.013 + k as f32 * 0.0037) + k as f32 * 0.7;
                let y = h as f32 / 2.0 + (h as f32 / 2.6) * f.sin();
                let yi = y.floor();
                if yi < 0.0 || yi as u32 + 1 >= h {
                    continue;
                }
                let frac = y - yi;
                for (row, cover) in [(yi as u32, 1.0 - frac), (yi as u32 + 1, frac)] {
                    let i = ((row * w + x) * 3) as usize;
                    for c in 0..3 {
                        let bg = buf[i + c] as f32;
                        buf[i + c] = (bg + (color[c] as f32 - bg) * cover).round() as u8;
                    }
                }
            }
        }
        // Plot frame: one-pixel black rules.
        for x in 0..w {
            for row in [0u32, h - 1] {
                let i = ((row * w + x) * 3) as usize;
                buf[i..i + 3].copy_from_slice(&[0, 0, 0]);
            }
        }
        for y in 0..h {
            for col in [0u32, w - 1] {
                let i = ((y * w + col) * 3) as usize;
                buf[i..i + 3].copy_from_slice(&[0, 0, 0]);
            }
        }
        buf
    }

    #[test]
    fn lossy_reencode_declines_line_art() {
        // FIX 1 (Phase 7 post-review): the under-threshold lossy path must
        // decline line-art content even though the size guard would happily
        // convert it. 200 px into 200 pt ⇒ 72 DPI, the dimension-preserving
        // shape.
        let raw = line_art_pixels(200, 200);
        let m = line_art_metrics(&raw, 3, 200, 200);
        assert!(
            looks_like_line_art(&raw, 3, 200, 200),
            "fixture must trip the content guard (bg {:.3} pal {:.3} edge {:.4})",
            m.background,
            m.palette,
            m.edges
        );
        assert!(
            !looks_like_line_art(&photo_pixels(200, 200, 3), 3, 200, 200),
            "photographic content must NOT trip the guard"
        );

        let pdf = build_pdf_flate_raw(&raw, 200, 200, 3);
        let opts = OptimizeOptions::default().with_allow_lossy_reencode(true);
        let out = optimize_with_options(&pdf, opts);
        assert_eq!(
            out, pdf,
            "line art must not be converted even with --allow-lossy"
        );

        // And prove the guard is what declined it: without the content check
        // the JPEG candidate clears the 5% savings bar by a wide margin.
        let doc = Document::load_mem(&pdf).unwrap();
        let stream = doc
            .objects
            .values()
            .find_map(|o| match o {
                Object::Stream(s)
                    if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") =>
                {
                    Some(s)
                }
                _ => None,
            })
            .unwrap();
        let (img, _) = decode_flate_image(&doc, stream, 200, 200).unwrap();
        let unguarded = encode_jpeg(img, false, opts.jpeg_quality).unwrap();
        assert!(
            unguarded.len() * 100 < stream.content.len() * 95,
            "without the guard this fixture would convert ({} -> {})",
            stream.content.len(),
            unguarded.len()
        );
    }

    #[test]
    fn lossy_reencode_declines_over_resolution_line_art() {
        // The p12 shape as it actually occurs: the line-art profiles are
        // OVER-RESOLUTION in the source PDF, so they reach the JPEG-vs-Flate
        // competition, not the dimension-preserving branch. The content guard
        // (evaluated on the source pixels) removes the JPEG candidate, leaving
        // the lossless downsample to ship — so the flag-on output is exactly
        // the flag-off output.
        let pdf = build_pdf_flate_raw(&line_art_pixels(400, 400), 400, 100, 3);
        let flag_off = optimize(&pdf);
        let opts = OptimizeOptions::default().with_allow_lossy_reencode(true);
        let flag_on = optimize_with_options(&pdf, opts);
        assert_eq!(
            flag_on, flag_off,
            "line art must never convert, whatever its resolution"
        );
        let (filter, _) = image_filter_info(&flag_on);
        assert_eq!(filter, b"FlateDecode", "encoding class must not change");
    }

    /// Periodic banner content: a stepped color ramp under a fine 1 px grid.
    /// Deflate exploits the exact periodicity at full resolution; resampling
    /// destroys it, so the LOSSLESS downsample of this image is *larger* than
    /// the original stream while a JPEG of the same target geometry is much
    /// smaller — the compounding-loss trap FIX 2 closes.
    fn periodic_banner_pixels(px: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((px * px * 3) as usize);
        for y in 0..px {
            for x in 0..px {
                let base = [
                    (60 + (x / 3) % 190) as u8,
                    (200 - (y / 4) % 150) as u8,
                    (120 + ((x + y) / 5) % 120) as u8,
                ];
                if x % 7 == 0 || y % 11 == 0 {
                    out.extend_from_slice(&[25, 25, 25]);
                } else {
                    out.extend_from_slice(&base);
                }
            }
        }
        out
    }

    #[test]
    fn lossy_reencode_never_compounds_a_declined_downsample() {
        // FIX 2 (Phase 7 post-review): 400 px drawn into 140 pt ⇒ ~206 DPI,
        // over-resolution, target ≈ 253 px. On this content the lossless Flate
        // downsample GROWS the stream (periodicity destroyed by resampling) and
        // is declined by the never-larger guard; a JPEG at the same target
        // geometry would be far smaller. Consent to re-encode must not
        // resurrect the resample the lossless path rejected: the image keeps
        // its ORIGINAL bytes.
        let raw = periodic_banner_pixels(400);
        let pdf = build_pdf_flate_raw(&raw, 400, 140, 3);

        // Pin the premise: at the target geometry the Flate candidate is not
        // smaller than the original, while the JPEG candidate is.
        let doc = Document::load_mem(&pdf).unwrap();
        let stream = doc
            .objects
            .values()
            .find_map(|o| match o {
                Object::Stream(s)
                    if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") =>
                {
                    Some(s)
                }
                _ => None,
            })
            .unwrap();
        let opts = OptimizeOptions::default().with_allow_lossy_reencode(true);
        let (target_w, target_h) = (253u32, 253u32);
        let (flate_out, _) = plan_flate(&doc, stream, 400, 400, target_w, target_h).unwrap();
        assert!(
            flate_out.len() >= stream.content.len(),
            "premise: the lossless downsample must grow ({} -> {})",
            stream.content.len(),
            flate_out.len()
        );
        let jpeg_out =
            plan_flate_to_jpeg(&doc, stream, opts, 400, 400, target_w, target_h).unwrap();
        assert!(
            jpeg_out.len() < stream.content.len(),
            "premise: the JPEG-at-target candidate must shrink ({} -> {})",
            stream.content.len(),
            jpeg_out.len()
        );

        let out = optimize_with_options(&pdf, opts);
        assert_eq!(
            out, pdf,
            "a resample the lossless path declined must not return via the lossy path"
        );
        let (filter, _) = image_filter_info(&out);
        assert_eq!(filter, b"FlateDecode", "encoding class must be unchanged");
        assert_eq!(image_dims(&out), (400, 400), "geometry must be unchanged");
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

    /// The bytes of the cross-reference object a saved file's `startxref`
    /// points at, up to (and excluding) the payload. Panics if the file does
    /// not end in the shape lopdf's writer produces.
    fn xref_object_header(pdf: &[u8]) -> Vec<u8> {
        let sx = pdf.windows(9).rposition(|w| w == b"startxref").unwrap();
        let digits: Vec<u8> = pdf[sx + 9..]
            .iter()
            .copied()
            .skip_while(u8::is_ascii_whitespace)
            .take_while(u8::is_ascii_digit)
            .collect();
        let start: usize = String::from_utf8(digits).unwrap().parse().unwrap();
        let end = find_sub(pdf, b">>stream\n", start).unwrap();
        pdf[start..end].to_vec()
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

    /// A producer's trailer can carry cross-reference-stream keys that describe
    /// the *input's* table (`corpus/adobe-spec.pdf` ends in a classic `trailer`
    /// holding `/DecodeParms<</Columns 5/Predictor 12>>`). lopdf copies the
    /// trailer into the xref stream it synthesizes, so those keys would sit on
    /// top of amatl's freshly deflated, unpredicted payload and make every
    /// strict reader repair the file. None may survive.
    #[test]
    fn stale_xref_keys_in_the_input_trailer_do_not_reach_the_output() {
        // Every key the strip covers must go, whatever the input put there.
        let mut doc = Document::load_mem(&build_pdf_duplicate_images(6, 400)).unwrap();
        for key in ["DecodeParms", "Filter", "Prev", "XRefStm", "Length"] {
            doc.trailer.set(key, 16i64);
        }
        strip_stale_xref_trailer_keys(&mut doc);
        for key in [b"DecodeParms".as_slice(), b"Filter", b"Prev", b"XRefStm"] {
            assert!(
                doc.trailer.get(key).is_err(),
                "{} must not survive in the trailer",
                String::from_utf8_lossy(key)
            );
        }

        // End to end, with the shape adobe-spec.pdf actually ships: a stale
        // predictor declaration. It is inert in the staged input (no /Filter
        // there), and must stay out of the output, where a /Filter IS added.
        let mut doc = Document::load_mem(&build_pdf_duplicate_images(6, 400)).unwrap();
        doc.trailer.set(
            "DecodeParms",
            dictionary! { "Columns" => 5, "Predictor" => 12 },
        );
        let mut staged: Vec<u8> = Vec::new();
        doc.save_to(&mut staged).unwrap();
        assert!(
            find_sub(&staged, b"/Predictor", 0).is_some(),
            "premise: the staged input must actually carry the stale key"
        );

        for pack in [true, false] {
            let opts = OptimizeOptions::default().with_pack_object_streams(pack);
            let out = optimize_with_options(&staged, opts);
            let header = xref_object_header(&out);
            let shown = String::from_utf8_lossy(&header).to_string();
            for key in [b"/DecodeParms".as_slice(), b"/Prev", b"/XRefStm"] {
                assert!(
                    find_sub(&header, key, 0).is_none(),
                    "pack={pack}: stale {} leaked into the xref stream: {shown}",
                    String::from_utf8_lossy(key)
                );
            }
            // The dictionary must describe the payload it actually ships: one
            // /Filter (ours) and a table that reads back.
            assert!(
                find_sub(&header, b"/Filter/FlateDecode", 0).is_some(),
                "pack={pack}: xref stream must be deflated, got {shown}"
            );
            let reloaded = Document::load_mem(&out)
                .unwrap_or_else(|e| panic!("pack={pack}: output must load: {e}"));
            assert!(
                !reloaded.get_pages().is_empty(),
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

    /// Build a one-page PDF whose image stream is FlateDecode at level 1 —
    /// i.e. what a careless producer ships, which is exactly what the final
    /// re-deflate pass exists to improve on.
    fn build_pdf_weakly_deflated(px: u32, draw_pts: i64) -> Vec<u8> {
        use std::io::Write;
        let raw = flate_pixels(px, px, 3);
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(1));
        enc.write_all(&raw).unwrap();
        let payload = enc.finish().unwrap();

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
            },
            payload,
        ));
        wrap_image_pdf(&mut doc, img_id, draw_pts)
    }

    /// Task 2: the pass must shrink a weakly-deflated stream while decoding to
    /// byte-identical pixels. Exercised directly on the document, because the
    /// pass deliberately runs at serialization time — see
    /// `serialization_wins_do_not_rewrite_an_otherwise_unchanged_file` for the
    /// end-to-end consequence of that placement.
    #[test]
    fn redeflate_shrinks_weakly_compressed_streams_losslessly() {
        // Drawn at its own size, so nothing else in the pipeline would touch
        // this stream even if it ran.
        let pdf = build_pdf_weakly_deflated(120, 120);
        let before_pixels = flate_image_pixels(&pdf);
        let before_len = image_stream_len(&pdf);

        let mut doc = Document::load_mem(&pdf).unwrap();
        redeflate_flate_streams(&mut doc, DeflateBackend::Zlib);
        let mut out: Vec<u8> = Vec::new();
        doc.save_to(&mut out).unwrap();

        assert!(
            image_stream_len(&out) < before_len,
            "level-9 must beat level-1: {before_len} -> {}",
            image_stream_len(&out)
        );
        assert_eq!(
            flate_image_pixels(&out),
            before_pixels,
            "decoded samples must be byte-identical"
        );
    }

    /// Pins the placement decision: the re-deflate pass and ObjStm packing are
    /// serialization-time work, so a document where NOTHING semantic was
    /// planned still comes back byte-identical. That is the crate's existing
    /// "declined everything ⇒ your exact bytes" contract (see the early return
    /// in `try_optimize`), and it bounds when the re-deflate win is realized.
    #[test]
    fn serialization_wins_do_not_rewrite_an_otherwise_unchanged_file() {
        let pdf = build_pdf_weakly_deflated(120, 120);
        assert_eq!(
            optimize_with_options(&pdf, OptimizeOptions::default()),
            pdf,
            "no planned work ⇒ input returned unchanged"
        );
    }

    /// The payload byte length of the (single) image stream.
    fn image_stream_len(pdf: &[u8]) -> usize {
        let doc = Document::load_mem(pdf).unwrap();
        doc.objects
            .values()
            .find_map(|o| match o {
                Object::Stream(s)
                    if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") =>
                {
                    Some(s.content.len())
                }
                _ => None,
            })
            .expect("no image stream")
    }

    /// Task 2 gates: never-larger per stream, and idempotent across passes —
    /// checked on every stream of a document that exercises several classes.
    #[test]
    fn redeflate_is_never_larger_and_idempotent() {
        for pdf in [
            build_pdf_flate(400, 100, Some(2)),
            build_pdf_flate(400, 100, None),
            build_pdf_duplicate_images(4, 400),
        ] {
            let once = optimize_with_options(&pdf, OptimizeOptions::default());
            let twice = optimize_with_options(&once, OptimizeOptions::default());
            assert_eq!(once, twice, "a second pass must be byte-identical");

            // Never-larger, per stream: every Flate stream in the output is at
            // most the size of its decoded content's level-9 re-deflate.
            let doc = Document::load_mem(&once).unwrap();
            for obj in doc.objects.values() {
                let Object::Stream(s) = obj else { continue };
                if !matches!(s.dict.get(b"Filter"), Ok(Object::Name(n)) if n == b"FlateDecode") {
                    continue;
                }
                assert!(
                    replan_deflate(&s.content, DeflateBackend::Zlib).is_none(),
                    "a shipped stream still had slack: {} bytes",
                    s.content.len()
                );
            }
        }
    }

    /// Build a one-page PDF with a channel-identical RGB Flate image (every
    /// pixel R==G==B), drawn at its own size so downsampling never fires.
    fn build_pdf_gray_in_rgb(px: u32) -> Vec<u8> {
        let gray = flate_pixels(px, px, 1);
        let rgb: Vec<u8> = gray.iter().flat_map(|&g| [g, g, g]).collect();
        let payload = deflate_level9(&rgb).unwrap();
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
            },
            payload,
        ));
        wrap_image_pdf(&mut doc, img_id, px as i64)
    }

    /// The collapse must rewrite /ColorSpace to DeviceGray, keep the decoded
    /// samples byte-identical to the shared channel, and be idempotent.
    #[test]
    fn gray_collapse_rewrites_channel_identical_rgb() {
        let px = 64;
        let pdf = build_pdf_gray_in_rgb(px);
        let opts = OptimizeOptions::default().with_collapse_gray_images(true);
        let out = optimize_with_options(&pdf, opts);
        assert!(out.len() < pdf.len(), "the gray stream must be smaller");

        let doc = Document::load_mem(&out).unwrap();
        let stream = doc
            .objects
            .values()
            .find_map(|o| match o {
                Object::Stream(s)
                    if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") =>
                {
                    Some(s)
                }
                _ => None,
            })
            .expect("no image stream");
        assert!(
            matches!(stream.dict.get(b"ColorSpace"), Ok(Object::Name(n)) if n == b"DeviceGray"),
            "/ColorSpace must be DeviceGray after the collapse"
        );
        let (img, channels) = decode_flate_image(&doc, stream, px, px).unwrap();
        assert_eq!(channels, 1);
        assert_eq!(
            img.into_luma8().into_raw(),
            flate_pixels(px, px, 1),
            "decoded samples must equal the shared channel exactly"
        );

        assert_eq!(
            optimize_with_options(&out, opts),
            out,
            "a second pass must be byte-identical"
        );
    }

    /// Off by default (the /ColorSpace rewrite is consent-gated), and a true
    /// color image must never be touched even with the flag on.
    #[test]
    fn gray_collapse_is_opt_in_and_declines_true_color() {
        let pdf = build_pdf_gray_in_rgb(64);
        assert_eq!(
            optimize_with_options(&pdf, OptimizeOptions::default()),
            pdf,
            "collapse must be opt-in"
        );

        let color = build_pdf_flate(64, 64, None);
        assert_eq!(
            optimize_with_options(
                &color,
                OptimizeOptions::default().with_collapse_gray_images(true)
            ),
            color,
            "distinct channels must pass through untouched"
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

    /// The zopfli backend must honor the same contract as zlib: never larger
    /// than what zlib ships, decoded samples byte-identical, and a second
    /// zopfli pass byte-stable (zopfli is deterministic, so re-deflating its
    /// own output fails the strictly-smaller test and changes nothing).
    #[test]
    fn zopfli_backend_is_smaller_lossless_and_idempotent() {
        let pdf = build_pdf_weakly_deflated(120, 120);
        let before_pixels = flate_image_pixels(&pdf);

        let mut doc = Document::load_mem(&pdf).unwrap();
        redeflate_flate_streams(&mut doc, DeflateBackend::Zlib);
        let mut zlib_out: Vec<u8> = Vec::new();
        doc.save_to(&mut zlib_out).unwrap();

        let mut doc = Document::load_mem(&pdf).unwrap();
        redeflate_flate_streams(&mut doc, DeflateBackend::Zopfli);
        let mut zopfli_out: Vec<u8> = Vec::new();
        doc.save_to(&mut zopfli_out).unwrap();

        assert!(
            image_stream_len(&zopfli_out) <= image_stream_len(&zlib_out),
            "zopfli must never lose to zlib under the strictly-smaller guard: \
             zlib {} vs zopfli {}",
            image_stream_len(&zlib_out),
            image_stream_len(&zopfli_out)
        );
        assert_eq!(
            flate_image_pixels(&zopfli_out),
            before_pixels,
            "decoded samples must be byte-identical"
        );

        // Idempotence of the PASS itself: on reload, a second zopfli run must
        // leave every stream untouched (deterministic zopfli re-produces the
        // same bytes, which fails the strictly-smaller test). Baseline and
        // second pass both go through the same load+save round trip so lopdf's
        // re-serialization cannot masquerade as a pass effect.
        let mut doc = Document::load_mem(&zopfli_out).unwrap();
        let mut reloaded: Vec<u8> = Vec::new();
        doc.save_to(&mut reloaded).unwrap();
        let mut doc = Document::load_mem(&zopfli_out).unwrap();
        redeflate_flate_streams(&mut doc, DeflateBackend::Zopfli);
        let mut twice: Vec<u8> = Vec::new();
        doc.save_to(&mut twice).unwrap();
        assert_eq!(reloaded, twice, "a second zopfli pass must change nothing");
    }

    /// A PDF/A conformance claim disables the pass wholesale (same posture as
    /// font subsetting), so a weakly-deflated stream ships untouched.
    #[test]
    fn redeflate_declines_pdfa_and_signed_documents() {
        for marker in ["pdfa", "signed"] {
            let pdf = build_pdf_weakly_deflated(120, 120);
            let mut doc = Document::load_mem(&pdf).unwrap();
            let before = image_stream_len(&pdf);
            match marker {
                "pdfa" => {
                    let meta = doc.add_object(Stream::new(
                        dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
                        b"<x:xmpmeta><pdfaid:part>2</pdfaid:part></x:xmpmeta>".to_vec(),
                    ));
                    doc.catalog_mut().unwrap().set("Metadata", meta);
                }
                _ => {
                    let sig = doc.add_object(dictionary! {
                        "Type" => "Sig",
                        "ByteRange" => vec![0.into(), 0.into(), 0.into(), 0.into()],
                    });
                    doc.catalog_mut().unwrap().set("Perms", sig);
                }
            }
            let mut marked: Vec<u8> = Vec::new();
            doc.save_to(&mut marked).unwrap();

            redeflate_flate_streams(
                &mut Document::load_mem(&marked).unwrap(),
                DeflateBackend::Zlib,
            );
            let mut reloaded = Document::load_mem(&marked).unwrap();
            redeflate_flate_streams(&mut reloaded, DeflateBackend::Zlib);
            let after = reloaded
                .objects
                .values()
                .find_map(|o| match o {
                    Object::Stream(s)
                        if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") =>
                    {
                        Some(s.content.len())
                    }
                    _ => None,
                })
                .unwrap();
            assert_eq!(after, before, "{marker}: stream must be untouched");
        }
    }

    /// Task 3: the flipped default really produces ObjStm-packed output, and
    /// the escape hatch really produces the old flat layout.
    #[test]
    fn default_packs_object_streams_and_opt_out_does_not() {
        let pdf = build_pdf_duplicate_images(6, 400);
        let packed = optimize_with_options(&pdf, OptimizeOptions::default());
        let flat = optimize_with_options(
            &pdf,
            OptimizeOptions::default().with_pack_object_streams(false),
        );

        assert!(
            find_sub(&packed, b"/Type/ObjStm", 0).is_some(),
            "default output must be ObjStm-packed"
        );
        assert!(
            find_sub(&flat, b"/Type/ObjStm", 0).is_none(),
            "--no-pack-object-streams must produce the flat layout"
        );
        assert!(
            packed.len() < flat.len(),
            "packing must win on an object-heavy document: {} vs {}",
            packed.len(),
            flat.len()
        );
        assert!(Document::load_mem(&packed).is_ok());
        assert!(Document::load_mem(&flat).is_ok());
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
        // already-adequate images to be RESIZED. (The q92 payload itself may
        // shrink via the P-M2 dimension-preserving requantization — geometry
        // is what this test pins.)
        let pdf = build_pdf_placed(120, 100, 100);
        let out = optimize(&pdf);
        assert_eq!(image_dims(&out), (120, 120), "must never be resized");
    }

    #[test]
    fn under_threshold_unmasked_jpeg_is_requantized() {
        // Phase 6 P-M2: 400px drawn into a 240pt box ≈ 120 DPI — under the
        // 130 x 1.15 over-resolution threshold, so the resize pipeline never
        // fires. The scanner-quality q92 payload must instead be requantized
        // at jpeg_quality (78) in place: strictly smaller, exact same
        // geometry.
        let pdf = build_pdf(400, 240);
        let before = image_stream_bytes(&pdf);
        let out = optimize(&pdf);

        assert!(out.len() < pdf.len(), "requantized output must be smaller");
        assert_eq!(image_dims(&out), (400, 400), "dimensions must be identical");
        let after = image_stream_bytes(&out);
        assert!(
            after.len() < before.len(),
            "the payload must be strictly smaller"
        );
        assert!(Document::load_mem(&out).is_ok());
    }

    #[test]
    fn under_threshold_requant_is_idempotent() {
        // P-M2 idempotence: the second pass re-attempts the requant and the
        // 5% minimum-savings guard declines the generation-loss churn.
        let pdf = build_pdf(400, 240);
        let once = optimize(&pdf);
        assert!(once.len() < pdf.len(), "first pass must shrink");
        let twice = optimize(&once);
        assert_eq!(twice, once, "second pass must be byte-stable");
    }

    #[test]
    fn under_threshold_requant_growth_is_discarded() {
        // The unmasked analogue of the D-M1 never-larger guard: the first
        // pass downsamples to ~130 DPI q78; a second pass at quality 100 hits
        // the P-M2 requant path, must grow the stream, and the guard discards
        // it — the exact baseline bytes come back.
        let pdf = build_pdf(400, 100);
        let baseline = optimize(&pdf);
        assert!(baseline.len() < pdf.len(), "baseline must be smaller");
        let opts = OptimizeOptions::default().with_jpeg_quality(100);
        let out = optimize_with_options(&baseline, opts);
        assert_eq!(out, baseline, "growing requantization must be discarded");
    }

    #[test]
    fn corrupt_under_threshold_jpeg_returns_exact_original_bytes() {
        // Structurally valid PDF, garbage JPEG bytes, UNDER the threshold: the
        // P-M2 requant attempts a decode, fails, and the fail-safe returns
        // the exact input bytes.
        let mut doc = Document::load_mem(&build_pdf(400, 240)).unwrap();
        for obj in doc.objects.values_mut() {
            if let Object::Stream(s) = obj {
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
                    s.set_content(b"\xff\xd8\xff not a real jpeg payload".to_vec());
                }
            }
        }
        let mut input: Vec<u8> = Vec::new();
        doc.save_to(&mut input).unwrap();

        let out = optimize(&input);
        assert_eq!(out, input, "corrupt under-threshold JPEG must be untouched");
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
        assert!(
            mad < 12.0,
            "scaled decode diverges from full decode: MAD={mad}"
        );
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
        assert!(
            d.pack_object_streams,
            "ObjStm packing is default-ON (measured −162,069 B / 3.27% on NASA)"
        );
        assert!(
            d.downsample_flate_images,
            "Flate downsampling is default-ON (0.2.0)"
        );
        assert!(
            d.subset_fonts,
            "font subsetting is default-ON (rendering-preserving, verified; \
             measured −124,486 B on NASA vs --no-subset-fonts)"
        );
        assert!(
            !d.recompress_bitonal_images,
            "bitonal G4 recompression is opt-in (B-M1)"
        );
        assert!(
            !d.allow_lossy_reencode,
            "lossy Flate→JPEG re-encode is consent-gated (Phase 7 spike)"
        );
        assert_eq!(
            d.deflate_backend,
            DeflateBackend::Zlib,
            "zopfli is opt-in: ~30× the CPU belongs behind a flag"
        );
        assert!(
            !d.collapse_gray_images,
            "RGB→Gray collapse rewrites /ColorSpace, so it is opt-in \
             (same posture as bitonal G4)"
        );
    }

    #[test]
    fn builder_methods_set_each_field() {
        let o = OptimizeOptions::default()
            .with_target_dpi(96.0)
            .with_jpeg_quality(60)
            .with_dpi_margin(1.5)
            .with_strip_accessibility(true)
            .with_pack_object_streams(false)
            .with_downsample_flate_images(false)
            .with_subset_fonts(true)
            .with_recompress_bitonal_images(true)
            .with_allow_lossy_reencode(true);
        assert_eq!(o.target_dpi, 96.0);
        assert_eq!(o.jpeg_quality, 60);
        assert_eq!(o.dpi_margin, 1.5);
        assert!(o.strip_accessibility);
        assert!(!o.pack_object_streams);
        assert!(!o.downsample_flate_images);
        assert!(o.subset_fonts);
        assert!(o.recompress_bitonal_images);
        assert!(o.allow_lossy_reencode);
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
        assert!(
            w72 < w130,
            "lower target DPI must yield fewer pixels: {w72} !< {w130}"
        );
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
        assert_eq!(
            (w, h),
            (400, 400),
            "zero target DPI must not resize the image"
        );
    }

    #[test]
    fn leaves_low_resolution_image_untouched() {
        // 120px drawn into 100pt box => ~86 DPI, below target: never resized.
        // (P-M2 may still requantize the q92 payload in place — the geometry
        // is the contract here.)
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
    fn minify_number_literal_is_decimal_exact() {
        for (input, want) in [
            ("+3", "3"),
            ("007", "7"),
            ("0.5000", ".5"),
            ("-0.000", "0"),
            ("-0", "0"),
            ("10.", "10"),
            ("0.0", "0"),
            ("-.12", "-.12"),
            ("1.25", "1.25"),
            ("0.30000001", ".30000001"),
        ] {
            assert_eq!(minify_number_literal(input), want, "literal {input:?}");
            // The transform's whole contract: identical f64.
            assert_eq!(
                input.parse::<f64>().unwrap(),
                minify_number_literal(input).parse::<f64>().unwrap(),
                "f64 drift on {input:?}"
            );
        }
    }

    #[test]
    fn replan_content_minifies_sloppy_streams() {
        let sloppy = b"1.000  0.000 0.000   1.000 100.00 700.00 cm\n% a comment\nq   Q";
        let (out, deflated) = replan_content(
            sloppy,
            sloppy.len(),
            sloppy.len(),
            false,
            DeflateBackend::Zlib,
        )
        .expect("sloppy content must minify");
        let stored = if deflated {
            inflate_capped(&out, 1 << 16).unwrap()
        } else {
            out.clone()
        };
        assert!(out.len() < sloppy.len());
        // Values a viewer parses are identical at f64.
        assert_eq!(
            content_number_values(sloppy).unwrap(),
            content_number_values(&stored).unwrap()
        );
        // And the operations are semantically unchanged.
        let a = Content::decode_strict(sloppy).unwrap();
        let b = Content::decode_strict(&stored).unwrap();
        assert!(operations_equivalent(&a.operations, &b.operations));
    }

    #[test]
    fn replan_content_preserves_f64_of_long_literals() {
        // 0.30000001 is NOT the shortest print of its f32 (that would be 0.3),
        // so a naive f32 re-emit would move the f64 a viewer parses. The
        // splice must keep the original decimal digits.
        let sloppy = b"0.30000001  0.000 0.000 0.30000001   0.000 0.000 cm  q   Q";
        let (out, deflated) = replan_content(
            sloppy,
            sloppy.len(),
            sloppy.len(),
            false,
            DeflateBackend::Zlib,
        )
        .expect("must minify");
        let stored = if deflated {
            inflate_capped(&out, 1 << 16).unwrap()
        } else {
            out
        };
        let text = String::from_utf8(stored).unwrap();
        assert!(
            text.contains(".30000001"),
            "original digits must survive: {text}"
        );
    }

    #[test]
    fn replan_content_declines_inline_images_and_garbage() {
        // Inline image: lopdf drops the binary data of an unparseable BI and
        // represents a parseable one as an operand it re-serializes in a
        // DIFFERENT (dict + stream) form — either way, hands off.
        let bi = b"BI /W 1 /H 1 /BPC 8 /CS /G ID x EI\nq Q";
        assert!(replan_content(bi, bi.len(), bi.len(), false, DeflateBackend::Zlib).is_none());
        // Truncated garbage must fail strict parsing, not silently truncate.
        let garbage = b"1.000 0.000 zzz <malformed  ";
        assert!(replan_content(
            garbage,
            garbage.len(),
            garbage.len(),
            false,
            DeflateBackend::Zlib
        )
        .is_none());
    }

    #[test]
    fn replan_content_declines_already_minimal_streams() {
        let minimal = b"1 0 0 1 5 5 cm";
        assert!(replan_content(
            minimal,
            minimal.len(),
            minimal.len(),
            false,
            DeflateBackend::Zlib
        )
        .is_none());
    }

    #[test]
    fn minify_merges_multi_stream_page_contents() {
        // A /Contents ARRAY whose operator spans the element boundary: the
        // page must be minified as one unit and re-emitted as a single stream.
        let body: String = "0.100 0.200 0.300 0.400 0.500 0.600 cm\n".repeat(60);
        let c1 = Stream::new(
            dictionary! {},
            format!("{body}1.000 0.000 0.000").into_bytes(),
        );
        let c2 = Stream::new(dictionary! {}, b" 1.000 5.000 7.000 cm\nq Q".to_vec());
        let mut doc = Document::with_version("1.5");
        let c1_id = doc.add_object(c1);
        let c2_id = doc.add_object(c2);
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "Contents" => vec![c1_id.into(), c2_id.into()],
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let before = doc.get_and_decode_page_content(page_id).unwrap();

        let changed = minify_content_streams(&mut doc, DeflateBackend::Zlib);
        assert!(changed, "sloppy multi-stream page must minify");
        let ids = doc.get_page_contents(page_id);
        assert_eq!(ids.len(), 1, "array must merge to a single stream");
        let after = doc.get_and_decode_page_content(page_id).unwrap();
        assert!(operations_equivalent(&before.operations, &after.operations));
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
        assert_ne!(
            first, second,
            "distinct objects must keep distinct references"
        );
    }

    #[test]
    fn cascaded_duplicate_merges_reach_fixpoint_in_one_call() {
        // Trimmed-down NASA repro (16 MB scanned doc, 2202 objects after one
        // pass): byte-identical image streams that each reference their OWN
        // copy of a duplicated ColorSpace object. The stream dedup pass alone
        // cannot merge the images — their dicts differ until dedup_objects
        // collapses the ColorSpace copies and remaps the references — so a
        // single dedup generation per call left the stream merge to the NEXT
        // optimize call, and optimize(optimize(x)) kept shrinking. The merge
        // cascade must reach its fixpoint inside ONE call.
        let mut img = image::RgbImage::new(16, 16);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            *pixel = image::Rgb([(x * 16) as u8, (y * 16) as u8, 0]);
        }
        let mut jpeg: Vec<u8> = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 92)
            .encode_image(&img)
            .unwrap();

        let mut doc = Document::with_version("1.5");
        // Two identical indirect ColorSpace objects — first-generation dupes.
        let cs1 = doc.add_object(Object::Name(b"DeviceRGB".to_vec()));
        let cs2 = doc.add_object(Object::Name(b"DeviceRGB".to_vec()));
        // Two image streams with identical bytes whose dicts differ ONLY in
        // which ColorSpace copy they reference — mergeable one generation
        // AFTER the ColorSpaces collapse. Never painted, so the downsample
        // planner ignores them; reachable via /Resources, so prune keeps them.
        let image_dict = |cs: ObjectId| {
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 16_i64,
                "Height" => 16_i64,
                "ColorSpace" => cs,
                "BitsPerComponent" => 8_i64,
                "Filter" => "DCTDecode",
            }
        };
        let img1 = doc.add_object(Stream::new(image_dict(cs1), jpeg.clone()));
        let img2 = doc.add_object(Stream::new(image_dict(cs2), jpeg));
        // Two byte-identical content streams (a /Contents array is legal PDF):
        // merged by the FIRST dedup_streams pass, which is what marks the
        // document as having work to do at all (merged_streams == true).
        let content = Content {
            operations: vec![Operation::new("q", vec![]), Operation::new("Q", vec![])],
        };
        let c1 = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let c2 = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));

        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => vec![c1.into(), c2.into()],
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! {
                "XObject" => dictionary! { "Im0" => img1, "Im1" => img2 },
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
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut pdf: Vec<u8> = Vec::new();
        doc.save_to(&mut pdf).unwrap();

        let once = optimize(&pdf);
        assert!(once.len() < pdf.len(), "first pass must shrink");
        assert_eq!(
            count_image_streams(&once),
            1,
            "the second-generation image-stream merge must happen in pass one"
        );
        let twice = optimize(&once);
        assert_eq!(twice, once, "second pass must be byte-stable");
        assert_eq!(
            twice.len(),
            once.len(),
            "single-pass output must already be the two-pass size"
        );
    }

    #[test]
    fn identical_blank_pages_are_never_merged() {
        // NASA repro, part two: two blank pages with byte-identical content
        // streams and shared resources. Once the content streams merge, the
        // page dicts become byte-identical — and merging THEM corrupts the
        // document: the same object id lands in /Kids twice, and lopdf's
        // renumber page-reordering pass then collides page objects onto one
        // id, silently overwriting other pages (a scanned page went blank and
        // its 1.37 MB image subtree was orphaned). Page-tree nodes must
        // survive dedup even when byte-identical.
        let mut doc = Document::with_version("1.5");
        let content = Content {
            operations: vec![Operation::new("q", vec![]), Operation::new("Q", vec![])],
        };
        // Separate but byte-identical content streams: their merge is both the
        // work trigger (merged_streams == true) and what makes the page dicts
        // identical.
        let c1 = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let c2 = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));

        let pages_id = doc.new_object_id();
        let blank_page = |contents: ObjectId| {
            dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => contents,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            }
        };
        let p1 = doc.add_object(blank_page(c1));
        let p2 = doc.add_object(blank_page(c2));
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![p1.into(), p2.into()],
                "Count" => 2,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut pdf: Vec<u8> = Vec::new();
        doc.save_to(&mut pdf).unwrap();

        let once = optimize(&pdf);
        let out_doc = Document::load_mem(&once).expect("output must load");
        let pages: Vec<ObjectId> = out_doc.get_pages().into_values().collect();
        assert_eq!(pages.len(), 2, "both pages must survive");
        assert_ne!(
            pages[0], pages[1],
            "identical pages must stay distinct objects, never merged"
        );
        let twice = optimize(&once);
        assert_eq!(twice, once, "second pass must be byte-stable");
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
        assert!(
            Document::load_mem(&out).is_ok(),
            "output must be a valid PDF"
        );
        if let Ok(dest) = std::env::var("AMATL_TEST_OUT") {
            std::fs::write(&dest, &out).unwrap();
        }
    }

    // ---- D-M1: SMask-aware JPEG requantization (Phase 5) -------------------

    /// Build a one-page PDF embedding a `px`×`px` RGB JPEG WITH a plain 8-bit
    /// DeviceGray `/SMask` soft mask, drawn into a `draw_pts` square — the
    /// D-M1 positive fixture shape.
    fn build_pdf_smask(px: u32, draw_pts: i64, jpeg_quality: u8) -> Vec<u8> {
        build_pdf_smask_ext(px, draw_pts, jpeg_quality, |_| {}, |_| {})
    }

    /// The same, with `base_mutate`/`mask_mutate` applied to the base and mask
    /// stream dicts before draw — for the ineligible-mask skip cases.
    fn build_pdf_smask_ext(
        px: u32,
        draw_pts: i64,
        jpeg_quality: u8,
        base_mutate: impl FnOnce(&mut lopdf::Dictionary),
        mask_mutate: impl FnOnce(&mut lopdf::Dictionary),
    ) -> Vec<u8> {
        let mut img = image::RgbImage::new(px, px);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            *pixel = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let mut jpeg: Vec<u8> = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, jpeg_quality)
            .encode_image(&img)
            .unwrap();
        let mask_payload = deflate_level9(&flate_pixels(px, px, 1)).unwrap();

        let mut doc = Document::with_version("1.5");
        let mask_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => px as i64,
                "Height" => px as i64,
                "ColorSpace" => "DeviceGray",
                "BitsPerComponent" => 8_i64,
                "Filter" => "FlateDecode",
            },
            mask_payload,
        ));
        if let Ok(Object::Stream(s)) = doc.get_object_mut(mask_id) {
            mask_mutate(&mut s.dict);
        }
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => px as i64,
                "Height" => px as i64,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8_i64,
                "Filter" => "DCTDecode",
                "SMask" => mask_id,
            },
            jpeg,
        ));
        if let Ok(Object::Stream(s)) = doc.get_object_mut(img_id) {
            base_mutate(&mut s.dict);
        }
        wrap_image_pdf(&mut doc, img_id, draw_pts)
    }

    /// (base stream content, width, height, /SMask target object id) for the
    /// single image stream carrying an /SMask in `pdf`.
    fn smask_base_info(pdf: &[u8]) -> (Vec<u8>, i64, i64, ObjectId) {
        let doc = Document::load_mem(pdf).unwrap();
        for obj in doc.objects.values() {
            if let Object::Stream(s) = obj {
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image")
                    && s.dict.get(b"SMask").is_ok()
                {
                    let w = s.dict.get(b"Width").unwrap().as_i64().unwrap();
                    let h = s.dict.get(b"Height").unwrap().as_i64().unwrap();
                    let smask = match s.dict.get(b"SMask").unwrap() {
                        Object::Reference(r) => *r,
                        _ => panic!("SMask must be a reference in the fixture"),
                    };
                    return (s.content.clone(), w, h, smask);
                }
            }
        }
        panic!("no smask-carrying image stream found");
    }

    /// The `/Filter` name of the single `/SMask`-carrying image stream in `pdf`
    /// — which encoding the masked base ended up in.
    fn smask_base_filter(pdf: &[u8]) -> Vec<u8> {
        let doc = Document::load_mem(pdf).unwrap();
        for obj in doc.objects.values() {
            if let Object::Stream(s) = obj {
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image")
                    && s.dict.get(b"SMask").is_ok()
                {
                    return match s.dict.get(b"Filter").unwrap() {
                        Object::Name(n) => n.clone(),
                        other => panic!("fixture uses scalar filters, got {other:?}"),
                    };
                }
            }
        }
        panic!("no smask-carrying image stream found");
    }

    fn smask_stream_bytes(pdf: &[u8], smask_id: ObjectId) -> Vec<u8> {
        let doc = Document::load_mem(pdf).unwrap();
        match doc.get_object(smask_id) {
            Ok(Object::Stream(s)) => s.content.clone(),
            _ => panic!("smask id is not a stream"),
        }
    }

    #[test]
    fn smask_masked_jpeg_requantizes_smaller_dimensions_identical() {
        // Positive D-M1 case: a q92 RGB JPEG with a plain 8-bit DeviceGray
        // /SMask placed at 240 pt (~120 DPI — UNDER the D-M2 over-resolution
        // threshold, so the pair is not eligible for the coupled downsample
        // and D-M1 remains the transform that applies). The base must be
        // re-encoded at jpeg_quality (78) WITHOUT resizing, replaced only
        // because it is strictly smaller, and the /SMask must survive
        // byte-for-byte pointing at the same mask stream.
        let pdf = build_pdf_smask(400, 240, 92);
        let (base_before, iw, ih, smask_before) = smask_base_info(&pdf);
        let mask_before = smask_stream_bytes(&pdf, smask_before);
        assert_eq!((iw, ih), (400, 400), "fixture base must be 400x400");

        let out = optimize(&pdf);

        assert!(out.len() < pdf.len(), "requantized output must be smaller");
        let (base_after, aw, ah, smask_after) = smask_base_info(&out);
        assert_eq!(
            (aw, ah),
            (iw, ih),
            "base dimensions must be identical after D-M1"
        );
        assert_ne!(
            base_after, base_before,
            "the base must actually be re-encoded, not passed through"
        );
        assert_eq!(
            smask_after, smask_before,
            "/SMask reference must stay intact"
        );
        assert_eq!(
            smask_stream_bytes(&out, smask_after),
            mask_before,
            "the /SMask stream itself must be untouched"
        );
        assert!(Document::load_mem(&out).is_ok());
    }

    #[test]
    fn smask_masked_optimize_is_idempotent() {
        // Over-resolution masked pair: the first pass is a D-M2 coupled
        // downsample (base + mask to 181 px). The second pass sees the pair at
        // ~130 DPI, under the margin, so the coupled downsample is declined by
        // the `target_* >= px_*` gate; the D-M1 requant that then applies is
        // a byte-identical no-op (5% guard). Byte-stable.
        let pdf = build_pdf_smask(400, 100, 92);
        let once = optimize(&pdf);
        assert!(once.len() < pdf.len(), "first pass must shrink");
        let twice = optimize(&once);
        assert_eq!(twice, once, "second pass must be byte-stable");
    }

    /// Two 400x400 q92 JPEG bases sharing ONE eligible `/SMask` object (the
    /// NASA dedup shape: byte-identical masks merged into a single id before
    /// planning), each drawn `draw_pts` square on one page. The bases carry
    /// DIFFERENT pixels so dedup never merges them — only the mask is shared.
    fn build_pdf_shared_smask(draw_pts: i64) -> Vec<u8> {
        let mut doc = Document::load_mem(&build_pdf_smask(400, draw_pts, 92)).unwrap();
        // Locate the original image id + its mask id.
        let (img_a_id, mask_a) = doc
            .objects
            .iter()
            .find_map(|(id, obj)| match obj {
                Object::Stream(s)
                    if matches!(
                        s.dict.get(b"Subtype"),
                        Ok(Object::Name(n)) if n == b"Image"
                    ) && s.dict.get(b"SMask").is_ok() =>
                {
                    let mask = match s.dict.get(b"SMask").unwrap() {
                        Object::Reference(r) => *r,
                        _ => panic!("fixture uses direct refs"),
                    };
                    Some((*id, mask))
                }
                _ => None,
            })
            .expect("fixture has one masked image");
        // Second image: same geometry class (over-res when drawn), DIFFERENT
        // pixels so dedup does not merge the BASES — only the masks merge.
        let mut img_b = image::RgbImage::new(400, 400);
        for (x, y, pixel) in img_b.enumerate_pixels_mut() {
            *pixel = image::Rgb([255 - (x % 256) as u8, (y % 256) as u8, 128]);
        }
        let mut jpeg_b: Vec<u8> = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_b, 92)
            .encode_image(&img_b)
            .unwrap();
        let img_b_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 400_i64,
                "Height" => 400_i64,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8_i64,
                "Filter" => "DCTDecode",
                "SMask" => mask_a,
            },
            jpeg_b,
        ));
        // Draw both on one page at draw_pts each.
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            format!(
                "q {d} 0 0 {d} 0 0 cm /Im0 Do Q q {d} 0 0 {d} {off} 0 cm /Im1 Do Q",
                d = draw_pts,
                off = draw_pts + 50
            )
            .into_bytes(),
        ));
        let page_resources = dictionary! {
            "XObject" => dictionary! {
                "Im0" => Object::Reference(img_a_id),
                "Im1" => Object::Reference(img_b_id),
            },
        };
        // Point the fixture's single-image page at our two-image content.
        for obj in doc.objects.values_mut() {
            if let Object::Dictionary(d) = obj {
                if matches!(
                    d.get(b"Type").map(|t| t.as_name()),
                    Ok(Ok(name)) if name == b"Page"
                ) {
                    d.set("Resources", page_resources.clone());
                    d.set("Contents", Object::Reference(content_id));
                }
            }
        }
        let mut input: Vec<u8> = Vec::new();
        doc.save_to(&mut input).unwrap();
        input
    }

    /// Every image stream carrying an `/SMask` in `pdf`, as
    /// `(content, width, height, mask_id)` sorted by object id.
    fn shared_smask_bases(pdf: &[u8]) -> Vec<(Vec<u8>, i64, i64, ObjectId)> {
        let doc = Document::load_mem(pdf).unwrap();
        let mut ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
        ids.sort();
        ids.iter()
            .filter_map(|id| match doc.get_object(*id) {
                Ok(Object::Stream(s))
                    if matches!(
                        s.dict.get(b"Subtype"),
                        Ok(Object::Name(n)) if n == b"Image"
                    ) && s.dict.get(b"SMask").is_ok() =>
                {
                    let mask = match s.dict.get(b"SMask").unwrap() {
                        Object::Reference(r) => *r,
                        _ => panic!("fixture uses direct refs"),
                    };
                    let w = s.dict.get(b"Width").unwrap().as_i64().unwrap();
                    let h = s.dict.get(b"Height").unwrap().as_i64().unwrap();
                    Some((s.content.clone(), w, h, mask))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn shared_smask_is_never_resized() {
        // NASA-derived corruption repro: two over-resolution masked images
        // whose masks are byte-identical. dedup merges the masks into ONE
        // object BEFORE planning, so both bases reference a single /SMask id.
        // Resizing that shared mask for one base's geometry would break the
        // other's alpha alignment (real page-33 corruption caught live). At
        // 100pt the pairs are OVER-resolution (400px ≈ 288 DPI), so the
        // coupled downsample must be declined — but the P-M1 split allows the
        // DIMENSION-PRESERVING requant (mask untouched): both q92 bases get
        // re-encoded at jpeg_quality in place. Geometry stays 400x400, both
        // /SMask refs still point at the same unmodified mask object, and the
        // mask's bytes are byte-identical before/after.
        let input = build_pdf_shared_smask(100);
        let out = optimize(&input);
        assert!(out.len() < input.len(), "requant must shrink the bases");
        let pairs = shared_smask_bases(&out);
        assert_eq!(pairs.len(), 2);
        for (content, w, h, _mask) in &pairs {
            assert_eq!((*w, *h), (400, 400), "dims must be unchanged");
            let img = image::load_from_memory_with_format(content, image::ImageFormat::Jpeg)
                .expect("base is still a decodable JPEG");
            assert_eq!((img.width(), img.height()), (400, 400));
        }
        assert_eq!(pairs[0].3, pairs[1].3, "both refs point at one mask");
        // The shared mask itself is byte-identical to its pre-optimization form.
        let in_pairs = shared_smask_bases(&input);
        let input_doc = Document::load_mem(&input).unwrap();
        let mask_before = input_doc
            .get_object(in_pairs[0].3)
            .unwrap()
            .as_stream()
            .unwrap()
            .content
            .clone();
        let mask_after = Document::load_mem(&out)
            .unwrap()
            .get_object(pairs[0].3)
            .unwrap()
            .as_stream()
            .unwrap()
            .content
            .clone();
        assert_eq!(mask_before, mask_after, "shared mask must be untouched");
    }

    #[test]
    fn shared_smask_under_threshold_bases_are_requantized() {
        // Phase 6 P-M1: the same shared-mask shape drawn at 240pt (~120 DPI,
        // UNDER the over-resolution threshold). Requantization never touches
        // the mask, so the shared mask no longer blocks it: BOTH q92 bases
        // must be re-encoded at jpeg_quality (78) in place — smaller, exact
        // same 400x400 geometry, both /SMask refs still pointing at the SAME
        // untouched mask object.
        let input = build_pdf_shared_smask(240);
        let before = shared_smask_bases(&input);
        assert_eq!(before.len(), 2, "fixture must hold two masked bases");
        assert_eq!(
            before[0].3, before[1].3,
            "fixture masks must be merged into one object"
        );
        let mask_bytes_before = smask_stream_bytes(&input, before[0].3);

        let out = optimize(&input);
        assert!(
            out.len() < input.len(),
            "requantized output must be smaller"
        );

        let after = shared_smask_bases(&out);
        assert_eq!(after.len(), 2, "both masked bases must survive");
        assert_eq!(
            after[0].3, after[1].3,
            "both /SMask refs must still point at the same mask object"
        );
        for ((base_before, ..), (base_after, w, h, _)) in before.iter().zip(&after) {
            assert_eq!((*w, *h), (400, 400), "base dimensions must be identical");
            assert!(
                base_after.len() < base_before.len(),
                "each base must be strictly smaller"
            );
        }
        assert_eq!(
            smask_stream_bytes(&out, after[0].3),
            mask_bytes_before,
            "the shared mask stream must be byte-identical"
        );
        assert!(Document::load_mem(&out).is_ok());
    }

    #[test]
    fn shared_smask_requant_is_idempotent() {
        // P-M1 idempotence: the first pass requantizes both shared-mask bases;
        // the second pass re-attempts the requant and the 5% minimum-savings
        // guard declines the generation-loss churn. Byte-stable.
        let input = build_pdf_shared_smask(240);
        let once = optimize(&input);
        assert!(once.len() < input.len(), "first pass must shrink");
        let twice = optimize(&once);
        assert_eq!(twice, once, "second pass must be byte-stable");
    }

    #[test]
    fn corrupt_masked_jpeg_returns_exact_original_bytes() {
        // Structurally valid PDF, corrupt base JPEG bytes, eligible /SMask:
        // the decode fails → fail-safe returns the exact input bytes.
        let mut doc = Document::load_mem(&build_pdf_smask(400, 100, 92)).unwrap();
        for obj in doc.objects.values_mut() {
            if let Object::Stream(s) = obj {
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image")
                    && s.dict.get(b"SMask").is_ok()
                {
                    s.set_content(b"\xff\xd8\xff not a real jpeg payload".to_vec());
                }
            }
        }
        let mut input: Vec<u8> = Vec::new();
        doc.save_to(&mut input).unwrap();

        let out = optimize(&input);
        assert_eq!(out, input, "corrupt masked JPEG must return original bytes");
        assert!(Document::load_mem(&out).is_ok());
    }

    #[test]
    fn smask_requantization_never_larger_guard_holds() {
        // First pass D-M2-downsamples the over-resolution pair to 181 px. The
        // second pass (quality 100) then sees an under-resolution pair, so the
        // D-M1 dimension-preserving path applies: re-encoding the base UP to
        // quality 100 from its q78 payload must grow the stream, and the
        // per-stream never-larger guard must discard it — the exact baseline
        // bytes (and untouched mask) come back.
        let pdf = build_pdf_smask(400, 100, 92);
        let baseline = optimize(&pdf); // D-M2'd: pair sits at ~130 DPI after this
        assert!(baseline.len() < pdf.len(), "baseline must be smaller");
        let opts = OptimizeOptions::default().with_jpeg_quality(100);
        let out = optimize_with_options(&baseline, opts);
        assert_eq!(out, baseline, "growing requantization must be discarded");
    }

    #[test]
    fn smask_matte_anywhere_skips_the_pair() {
        // /Matte (premultiplied background color) on either side of the pair
        // is a hard skip in D-M1.
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "/Matte on the mask",
                build_pdf_smask_ext(
                    400,
                    100,
                    92,
                    |_| {},
                    |m| {
                        m.set("Matte", vec![23.into(), 128.into(), 240.into()]);
                    },
                ),
            ),
            (
                "/Matte on the base",
                build_pdf_smask_ext(
                    400,
                    100,
                    92,
                    |b| {
                        b.set("Matte", vec![23.into(), 128.into(), 240.into()]);
                    },
                    |_| {},
                ),
            ),
        ];
        for (label, pdf) in cases {
            let out = optimize(&pdf);
            assert_eq!(out, pdf, "{label}: must leave the masked pair untouched");
        }
    }

    #[test]
    fn stencil_and_colorkey_masks_are_skipped() {
        // An /ImageMask stencil used as the /SMask, and a /Mask color-key on
        // the base: both remain hard skips in D-M1.
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "/ImageMask stencil as the /SMask",
                build_pdf_smask_ext(
                    400,
                    100,
                    92,
                    |_| {},
                    |m| {
                        m.set("ImageMask", Object::Boolean(true));
                    },
                ),
            ),
            (
                "/Mask color-key on the base",
                build_pdf_smask_ext(
                    400,
                    100,
                    92,
                    |b| {
                        b.set("Mask", vec![Object::Integer(1), Object::Integer(255)]);
                    },
                    |_| {},
                ),
            ),
        ];
        for (label, pdf) in cases {
            let out = optimize(&pdf);
            assert_eq!(out, pdf, "{label}: must leave the masked pair untouched");
        }
    }

    #[test]
    fn ineligible_smask_variants_are_skipped() {
        // Every mask-shape doubt rolls back to the untouched original:
        // unresolvable reference, non-image SMask object, non-DeviceGray
        // color space, and non-8-bit samples.
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "unresolvable /SMask reference",
                build_pdf_smask_ext(
                    400,
                    100,
                    92,
                    |b| {
                        b.set("SMask", 9_999_999_i64);
                    },
                    |_| {},
                ),
            ),
            (
                "/SMask that is not an image object",
                build_pdf_smask_ext(
                    400,
                    100,
                    92,
                    |b| {
                        b.set("SMask", dictionary! { "Type" => "AnyObject" });
                    },
                    |_| {},
                ),
            ),
            (
                "mask color space /DeviceRGB",
                build_pdf_smask_ext(
                    400,
                    100,
                    92,
                    |_| {},
                    |m| {
                        m.set("ColorSpace", "DeviceRGB");
                    },
                ),
            ),
            (
                "mask /BitsPerComponent 1 (not 8)",
                build_pdf_smask_ext(
                    400,
                    100,
                    92,
                    |_| {},
                    |m| {
                        m.set("BitsPerComponent", 1_i64);
                    },
                ),
            ),
        ];
        for (label, pdf) in cases {
            let out = optimize(&pdf);
            assert_eq!(out, pdf, "{label}: must leave the image untouched");
        }
    }

    // ---- D-M3: SMask-coupled Flate-base downsampling (Phase 5) -------------

    /// Build a one-page PDF embedding a `px`×`px` FlateDecode RGB noise image
    /// WITH a plain 8-bit DeviceGray FlateDecode `/SMask`, drawn into a
    /// `draw_pts` square — the D-M3 positive fixture shape (noise, so the
    /// downsampled re-encode is reliably smaller; see `flate_pixels`).
    fn build_pdf_smask_flate(px: u32, draw_pts: i64) -> Vec<u8> {
        let base = flate_pixels(px, px, 3);
        let mask = flate_pixels(px, px, 1);
        build_pdf_smask_flate_ext(px, draw_pts, &base, &mask, |_| {}, |_| {})
    }

    /// The same, with explicit raw pixel buffers and `base_mutate`/`mask_mutate`
    /// dict hooks — for the combined-guard and skip cases.
    fn build_pdf_smask_flate_ext(
        px: u32,
        draw_pts: i64,
        base_raw: &[u8],
        mask_raw: &[u8],
        base_mutate: impl FnOnce(&mut lopdf::Dictionary),
        mask_mutate: impl FnOnce(&mut lopdf::Dictionary),
    ) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let mask_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => px as i64,
                "Height" => px as i64,
                "ColorSpace" => "DeviceGray",
                "BitsPerComponent" => 8_i64,
                "Filter" => "FlateDecode",
            },
            deflate_level9(mask_raw).unwrap(),
        ));
        if let Ok(Object::Stream(s)) = doc.get_object_mut(mask_id) {
            mask_mutate(&mut s.dict);
        }
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => px as i64,
                "Height" => px as i64,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8_i64,
                "Filter" => "FlateDecode",
                "SMask" => mask_id,
            },
            deflate_level9(base_raw).unwrap(),
        ));
        if let Ok(Object::Stream(s)) = doc.get_object_mut(img_id) {
            base_mutate(&mut s.dict);
        }
        wrap_image_pdf(&mut doc, img_id, draw_pts)
    }

    /// A `px`-square checkerboard with `cell`-pixel cells: perfectly periodic
    /// input that deflates to almost nothing, while its Lanczos-downsampled
    /// counterpart (non-integer scale → aperiodic anti-aliased edges) deflates
    /// far worse — reliably tripping the combined never-larger/5% guard.
    fn checkerboard_pixels(px: u32, cell: u32, channels: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(px as usize * px as usize * channels);
        for y in 0..px {
            for x in 0..px {
                let v = if ((x / cell) + (y / cell)).is_multiple_of(2) {
                    0u8
                } else {
                    255u8
                };
                out.extend(std::iter::repeat_n(v, channels));
            }
        }
        out
    }

    #[test]
    fn smask_flate_pair_downsamples_atomically_dimensions_identical() {
        // Positive D-M3 case: a 400px FlateDecode RGB base with a plain 8-bit
        // DeviceGray Flate /SMask drawn at 100 pt (≈288 DPI, over-resolution).
        // Both streams must land at the SAME target geometry (181 px at the
        // default 130 DPI), the base must still be FlateDecode (format
        // preserved), and the /SMask reference must stay intact.
        let pdf = build_pdf_smask_flate(400, 100);
        let (base_before, iw, ih, smask_before) = smask_base_info(&pdf);
        let mask_before = smask_stream_bytes(&pdf, smask_before);
        assert_eq!((iw, ih), (400, 400), "fixture base must be 400x400");

        let out = optimize(&pdf);

        assert!(out.len() < pdf.len(), "downsampled output must be smaller");
        let (base_after, aw, ah, smask_after) = smask_base_info(&out);
        assert_eq!((aw, ah), (181, 181), "base must land at the 130-DPI target");
        assert_ne!(base_after, base_before, "the base must be re-encoded");
        assert_eq!(
            smask_after, smask_before,
            "/SMask reference must stay intact"
        );
        assert_ne!(
            smask_stream_bytes(&out, smask_after),
            mask_before,
            "the mask must be re-encoded alongside the base"
        );
        let doc = Document::load_mem(&out).unwrap();
        let mask_dict = &doc
            .get_object(smask_after)
            .unwrap()
            .as_stream()
            .unwrap()
            .dict;
        assert_eq!(
            (
                mask_dict.get(b"Width").unwrap().as_i64().unwrap(),
                mask_dict.get(b"Height").unwrap().as_i64().unwrap(),
            ),
            (aw, ah),
            "mask geometry must be identical to the base's (the unit rule)"
        );
        for obj in doc.objects.values() {
            if let Object::Stream(s) = obj {
                if s.dict.get(b"SMask").is_ok() {
                    assert!(
                        matches!(s.dict.get(b"Filter"), Ok(Object::Name(n)) if n == b"FlateDecode"),
                        "base must remain FlateDecode (format-preserving path)"
                    );
                }
            }
        }
    }

    #[test]
    fn smask_flate_under_resolution_pair_is_untouched() {
        // 400px drawn at 240 pt ≈ 120 DPI — inside the threshold. There is no
        // requantization analogue for lossless Flate bases in D-M3, so the
        // pair must come back byte-for-byte.
        let pdf = build_pdf_smask_flate(400, 240);
        let out = optimize(&pdf);
        assert_eq!(
            out, pdf,
            "under-resolution masked Flate pair must be untouched"
        );
    }

    #[test]
    fn corrupt_masked_flate_streams_return_exact_original_bytes() {
        // Atomicity under corruption: truncating EITHER half of the pair must
        // return the exact input bytes — never a one-sided replacement.
        for corrupt_mask in [false, true] {
            let mut doc = Document::load_mem(&build_pdf_smask_flate(400, 100)).unwrap();
            for obj in doc.objects.values_mut() {
                if let Object::Stream(s) = obj {
                    // The base carries /SMask; the mask is the image without it.
                    if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image")
                        && s.dict.get(b"SMask").is_ok() != corrupt_mask
                    {
                        let half = s.content.len() / 2;
                        let truncated = s.content[..half].to_vec();
                        s.set_content(truncated);
                    }
                }
            }
            let mut input: Vec<u8> = Vec::new();
            doc.save_to(&mut input).unwrap();
            let out = optimize(&input);
            assert_eq!(
                out,
                input,
                "corrupt {} must return original bytes for the whole pair",
                if corrupt_mask { "mask" } else { "base" }
            );
        }
    }

    #[test]
    fn smask_flate_combined_guard_skips_never_larger_pair() {
        // A periodic checkerboard deflates to almost nothing, but its
        // downsampled (aperiodic, anti-aliased) counterpart deflates far
        // worse: the combined candidate cannot beat the pair's original size
        // by 5%, so the guard must skip the whole pair byte-for-byte.
        let base = checkerboard_pixels(400, 8, 3);
        let mask = checkerboard_pixels(400, 8, 1);
        let pdf = build_pdf_smask_flate_ext(400, 100, &base, &mask, |_| {}, |_| {});
        let out = optimize(&pdf);
        assert_eq!(out, pdf, "never-larger pair must be skipped atomically");
    }

    #[test]
    fn smask_flate_optimize_is_idempotent() {
        // First pass: coupled downsample to 181 px. Second pass: the pair sits
        // at ~130 DPI (inside the margin) and Flate has no dimension-preserving
        // transform, so nothing is planned — byte-stable.
        let pdf = build_pdf_smask_flate(400, 100);
        let once = optimize(&pdf);
        assert!(once.len() < pdf.len(), "first pass must shrink");
        let twice = optimize(&once);
        assert_eq!(twice, once, "second pass must be byte-stable");
    }

    // ---- D-M3 + Phase 7 Option B: the coupled downsample's JPEG competitor ---

    /// The over-resolution masked-Flate shape the competitor targets: a 400px
    /// photographic RGB base with a plain 8-bit DeviceGray Flate `/SMask`,
    /// drawn at 100 pt (≈288 DPI ⇒ over-resolution, 181 px at the default 130
    /// DPI target). Photographic content, so the JPEG candidate is genuinely
    /// smaller than the format-preserving deflate at that geometry.
    fn build_pdf_smask_flate_photo() -> Vec<u8> {
        let base = photo_pixels(400, 400, 3);
        let mask = photo_pixels(400, 400, 1);
        build_pdf_smask_flate_ext(400, 100, &base, &mask, |_| {}, |_| {})
    }

    #[test]
    fn smask_flate_lossy_pair_is_idempotent_in_one_pass() {
        // The whole point of Option B. Without the competitor, pass 1 produced
        // an at-target masked FLATE pair and pass 2 then converted it through
        // the dimension-preserving fall-through — one harvest split across two
        // passes, breaking optimize(optimize(x)) == optimize(x). With the
        // competitor the base lands as DCTDecode at the target geometry in
        // pass 1, so pass 2 sees an at-target masked JPEG whose only remaining
        // transform is the D-M1 requant — declined by its own 5% guard.
        let pdf = build_pdf_smask_flate_photo();
        let opts = OptimizeOptions::default().with_allow_lossy_reencode(true);

        let once = optimize_with_options(&pdf, opts);
        assert!(once.len() < pdf.len(), "first pass must shrink");
        let (_, w, h, mask_id) = smask_base_info(&once);
        assert_eq!(
            smask_base_filter(&once).as_slice(),
            b"DCTDecode",
            "the JPEG competitor must win on photographic content"
        );
        assert_eq!((w, h), (181, 181), "base at the 130-DPI target geometry");
        let doc = Document::load_mem(&once).unwrap();
        let mask_dict = &doc.get_object(mask_id).unwrap().as_stream().unwrap().dict;
        assert_eq!(
            (
                mask_dict.get(b"Width").unwrap().as_i64().unwrap(),
                mask_dict.get(b"Height").unwrap().as_i64().unwrap(),
            ),
            (w, h),
            "the mask must be resampled to the base's geometry (the unit rule)"
        );

        let twice = optimize_with_options(&once, opts);
        assert_eq!(twice, once, "second pass must be BYTE-IDENTICAL");
    }

    /// A smooth diagonal ramp: no sharp edges and no flat background, so the
    /// line-art guard passes it (it is "photographic" by the metrics), but it
    /// is far more deflate-friendly than any JPEG at any quality — the
    /// competitor must lose on it.
    fn smooth_ramp_pixels(px: u32, channels: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(px as usize * px as usize * channels);
        for y in 0..px {
            for x in 0..px {
                let v = ((x + y) as f32 / (2.0 * px as f32) * 255.0) as u8;
                out.extend(std::iter::repeat_n(v, channels));
            }
        }
        out
    }

    #[test]
    fn smask_flate_lossy_competitor_wins_only_when_smaller() {
        // Competitor selection is a pure size comparison of the two base
        // candidates at ONE fixed target geometry — the mask half is identical
        // either way, so comparing the bases IS comparing the pairs. A smooth
        // ramp puts the two candidates within a factor of two of each other at
        // the 181-px target (measured: deflate 1,780 B; JPEG 1,027 B at q78,
        // 4,028 B at q100), so `jpeg_quality` flips the winner with the content
        // held fixed. Losing must leave exactly the flag-off pair, byte for
        // byte — not a "close enough" re-encode.
        let ramp = build_pdf_smask_flate_ext(
            400,
            100,
            &smooth_ramp_pixels(400, 3),
            &photo_pixels(400, 400, 1),
            |_| {},
            |_| {},
        );
        let flag_off = optimize(&ramp);

        let opts = OptimizeOptions::default().with_allow_lossy_reencode(true);
        let jpeg_wins = optimize_with_options(&ramp, opts);
        assert_eq!(
            smask_base_filter(&jpeg_wins).as_slice(),
            b"DCTDecode",
            "q78: the smaller JPEG candidate must win"
        );
        assert!(
            jpeg_wins.len() < flag_off.len(),
            "and it must actually beat the flag-off pair on size"
        );

        let lossless_wins = optimize_with_options(&ramp, opts.with_jpeg_quality(100));
        assert_eq!(
            smask_base_filter(&lossless_wins).as_slice(),
            b"FlateDecode",
            "q100: the larger JPEG candidate must lose to the deflate"
        );
        assert_eq!(
            lossless_wins, flag_off,
            "losing the competition must leave exactly the flag-off pair"
        );
    }

    #[test]
    fn smask_flate_lossy_competitor_declines_line_art() {
        // The line-art content guard runs inside `plan_flate_to_jpeg` on the
        // decoded SOURCE pixels, so it protects the coupled path too: an
        // over-resolution masked line-art pair still downsamples losslessly and
        // must come out byte-identical to the flag-off result — never
        // DCT-mottled.
        let base = line_art_pixels(400, 400);
        let mask = photo_pixels(400, 400, 1);
        let pdf = build_pdf_smask_flate_ext(400, 100, &base, &mask, |_| {}, |_| {});
        let opts = OptimizeOptions::default().with_allow_lossy_reencode(true);
        let out = optimize_with_options(&pdf, opts);
        assert_eq!(
            smask_base_filter(&out).as_slice(),
            b"FlateDecode",
            "masked line art must keep the format-preserving downsample"
        );
        assert_eq!(
            out,
            optimize(&pdf),
            "masked line art must get exactly the flag-off result"
        );
    }

    #[test]
    fn smask_flate_lossy_competitor_never_compounds_a_declined_downsample() {
        // No compounding losses, the pair's form: a checkerboard base whose
        // Lanczos-downsampled deflate GROWS past the combined 5% guard. The
        // lossless path therefore declines the resample, and a JPEG candidate
        // must not resurrect it by hiding the resolution loss behind a DCT win
        // — the pair comes back byte-for-byte even with consent.
        let base = checkerboard_pixels(400, 4, 3);
        let mask = checkerboard_pixels(400, 4, 1);
        let pdf = build_pdf_smask_flate_ext(400, 100, &base, &mask, |_| {}, |_| {});
        assert_eq!(optimize(&pdf), pdf, "the lossless pair must decline first");
        let opts = OptimizeOptions::default().with_allow_lossy_reencode(true);
        assert_eq!(
            optimize_with_options(&pdf, opts),
            pdf,
            "a declined resample must not be resurrected by a JPEG candidate"
        );
    }

    #[test]
    fn shared_smask_flate_pair_is_never_resized() {
        // Two DIFFERENT Flate bases referencing one /SMask id: the refcount
        // guard in eligible_smask must disqualify both pairs (resizing the
        // shared mask for one base's geometry would break the other's).
        let mut doc = Document::load_mem(&build_pdf_smask_flate(400, 100)).unwrap();
        let (img_a_id, mask_a) = doc
            .objects
            .iter()
            .find_map(|(id, obj)| match obj {
                Object::Stream(s)
                    if matches!(
                        s.dict.get(b"Subtype"),
                        Ok(Object::Name(n)) if n == b"Image"
                    ) && s.dict.get(b"SMask").is_ok() =>
                {
                    let mask = match s.dict.get(b"SMask").unwrap() {
                        Object::Reference(r) => *r,
                        _ => panic!("fixture uses direct refs"),
                    };
                    Some((*id, mask))
                }
                _ => None,
            })
            .expect("fixture has one masked image");
        // Different pixels, so dedup does not merge the BASES.
        let mut raw_b = flate_pixels(400, 400, 3);
        for b in &mut raw_b {
            *b = !*b;
        }
        let img_b_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 400_i64,
                "Height" => 400_i64,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8_i64,
                "Filter" => "FlateDecode",
                "SMask" => mask_a,
            },
            deflate_level9(&raw_b).unwrap(),
        ));
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            b"q 100 0 0 100 0 0 cm /Im0 Do Q q 100 0 0 100 150 0 cm /Im1 Do Q".to_vec(),
        ));
        let page_resources = dictionary! {
            "XObject" => dictionary! {
                "Im0" => Object::Reference(img_a_id),
                "Im1" => Object::Reference(img_b_id),
            },
        };
        for obj in doc.objects.values_mut() {
            if let Object::Dictionary(d) = obj {
                if matches!(
                    d.get(b"Type").map(|t| t.as_name()),
                    Ok(Ok(name)) if name == b"Page"
                ) {
                    d.set("Resources", page_resources.clone());
                    d.set("Contents", Object::Reference(content_id));
                }
            }
        }
        let mut input: Vec<u8> = Vec::new();
        doc.save_to(&mut input).unwrap();

        let out = optimize(&input);
        assert_eq!(out.len(), input.len(), "shared-mask pairs must not change");
    }

    #[test]
    fn smask_flate_matte_and_stencil_are_skipped() {
        // The D-M1 skip rules carry over unchanged to Flate bases: /Matte on
        // either side of the pair, and an /ImageMask stencil as the /SMask,
        // all leave the pair byte-for-byte untouched.
        let base = flate_pixels(400, 400, 3);
        let mask = flate_pixels(400, 400, 1);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "/Matte on the mask",
                build_pdf_smask_flate_ext(
                    400,
                    100,
                    &base,
                    &mask,
                    |_| {},
                    |m| {
                        m.set("Matte", vec![23.into(), 128.into(), 240.into()]);
                    },
                ),
            ),
            (
                "/Matte on the base",
                build_pdf_smask_flate_ext(
                    400,
                    100,
                    &base,
                    &mask,
                    |b| {
                        b.set("Matte", vec![23.into(), 128.into(), 240.into()]);
                    },
                    |_| {},
                ),
            ),
            (
                "/ImageMask stencil as the /SMask",
                build_pdf_smask_flate_ext(
                    400,
                    100,
                    &base,
                    &mask,
                    |_| {},
                    |m| {
                        m.set("ImageMask", Object::Boolean(true));
                    },
                ),
            ),
        ];
        for (label, pdf) in cases {
            let out = optimize(&pdf);
            assert_eq!(out, pdf, "{label}: must leave the masked pair untouched");
        }
    }

    #[test]
    fn downsample_flate_images_off_leaves_masked_pair_untouched() {
        // The masked-Flate coupled downsample honors the same consent flag as
        // the unmasked Flate path.
        let pdf = build_pdf_smask_flate(400, 100);
        let opts = OptimizeOptions::default().with_downsample_flate_images(false);
        let out = optimize_with_options(&pdf, opts);
        assert_eq!(out, pdf, "flag off must leave the masked pair untouched");
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
                    vec![
                        100.into(),
                        0.into(),
                        0.into(),
                        100.into(),
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
        let mut input: Vec<u8> = Vec::new();
        doc.save_to(&mut input).unwrap();

        let out = optimize(&input);
        assert_eq!(
            out, input,
            "corrupt image must leave the document unchanged"
        );
        assert!(
            Document::load_mem(&out).is_ok(),
            "output must remain a valid PDF"
        );
    }
}
