//! **picamatl** — pure-Rust PDF size optimization.
//!
//! Named for the Nahuatl word for the fig-bark paper used in pre-Columbian
//! Mesoamerican codices. Picamatl shrinks PDFs by downsampling over-resolution
//! embedded images to the resolution they are actually rendered at.
//!
//! ```
//! use picamatl::{optimize_with_options, OptimizeOptions};
//!
//! // Any &[u8]; this is the crate's own four-page test fixture.
//! let input: &[u8] = include_bytes!("../fixtures/sample.pdf");
//!
//! let opts = OptimizeOptions::default().with_target_dpi(130.0);
//! let output = optimize_with_options(input, opts);
//!
//! // The contract, in one line: never larger than what you handed in. Any
//! // change that cannot prove it shrinks is declined, and a file picamatl
//! // cannot make smaller comes back byte for byte.
//! assert!(output.len() <= input.len());
//! ```
//!
//! [`optimize`] is the same call with default options; [`optimize_with_report`]
//! additionally returns what the run found — notably any digital signatures it
//! would invalidate. None of them return an error, and none of them panic: see
//! *Hard safety guarantees* below.
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
//! Soft-masked images (`/SMask`) are handled too, under rules narrow enough
//! that alignment cannot break; they are documented on [`OptimizeOptions`].
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
mod cffhint;
mod cffmerge;
mod classify;
mod cmyk;
mod deflate;
mod encodings;
mod fonts;
mod forms;
mod jpeghuff;
mod reals;
mod serialize;
mod signatures;
mod truetype;
mod type1;

use std::collections::HashMap;

use image::{DynamicImage, ImageFormat};
use lopdf::content::Content;
use lopdf::{dictionary, Document, Object, ObjectId};
use rayon::prelude::*;

use classify::{image_is_figure_like, looks_like_line_art};
use cmyk::{jpeg_component_count, jpeg_frame_info, plan_dct_cmyk};
use deflate::{deflate_backend, deflate_level9, inflate_capped, png_defilter, png_up_filter};
use serialize::save_document;
use signatures::detect_signatures;

pub use signatures::{SignatureInfo, SignatureKind};

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
/// As a library, picamatl is accessibility-preserving by default. Callers who
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
/// # Soft-masked images
///
/// No option turns this on or off; it is how `target_dpi`, `jpeg_quality` and
/// `allow_lossy_reencode` behave when an image carries an `/SMask`. The rules
/// are named D-M1/D-M2/D-M3 in the source and in `docs/ARCHITECTURE.md`.
///
/// Soft-masked DCTDecode bases: over-resolution pairs are **downsampled as a
/// unit** (D-M2) — the base JPEG and its `/SMask` stream are resampled to the
/// SAME target geometry and replaced together or not at all — while pairs at
/// or below the target resolution take the dimension-preserving D-M1
/// requantization (the base is re-encoded at `jpeg_quality` at its OWN
/// dimensions, never resized, and the `/SMask` stream is never modified).
/// Both apply only when the mask is a plain 8-bit DeviceGray image stream with
/// no `/Matte`. `/Mask` (stencil/color-key) images and any ineligible `/SMask`
/// stay untouched.
///
/// Soft-masked FlateDecode bases (D-M3) take the coupled downsample when
/// over-resolution; otherwise they are untouched by default, and with
/// `allow_lossy_reencode` they take the dimension-preserving Flate→JPEG
/// conversion. That conversion rewrites the base stream only — the `/SMask`
/// object keeps its bytes and its dimensions, so alignment is preserved by
/// construction (and shared masks stay safe for the same reason the D-M1
/// requant is safe for them). Under the same flag the over-resolution coupled
/// downsample also runs a JPEG competitor at its TARGET geometry, against the
/// losslessly resampled mask, so a masked pair reaches its final encoding in
/// one pass instead of two.
///
/// A mask shared by more than one base is never resized (P-M1), so no other
/// consumer of it can be misaligned by a decision made for one base.
///
/// # Example
///
/// ```
/// use picamatl::OptimizeOptions;
/// let opts = OptimizeOptions::default()
///     .with_strip_accessibility(true)
///     .with_target_dpi(110.0);
/// ```
///
/// `#[non_exhaustive]`: construct via [`OptimizeOptions::default()`] plus the
/// `with_*` setters, never a struct literal. This lets future options ship in a
/// *minor* release instead of a breaking one once picamatl is published.
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

    /// Optional higher target resolution, in DPI, for images that a cheap
    /// pixel-statistic heuristic classifies as *figure-like*: charts, graphs,
    /// and diagrams carrying rendered text, where `target_dpi` turns axis
    /// labels to mush while a photograph at the same DPI looks fine. See
    /// [`image_is_figure_like`] for the classifier and its calibration.
    ///
    /// This is a QUALITY knob, not a consent tier: it only ever raises the
    /// target geometry of an image the pipeline was already going to
    /// downsample, so output is more faithful and (at most) larger than with
    /// it unset — never lossier. The never-larger-than-the-original contract
    /// is untouched: a candidate at the higher target that fails the
    /// strictly-smaller guard is declined and the image is left alone.
    ///
    /// Only values ABOVE `target_dpi` do anything; at or below it the knob is
    /// inert, which is what keeps the "never lossier" promise true. It also
    /// requires a positive `target_dpi` — `target_dpi <= 0` means "do not
    /// downsample at all" and returns before any of this is consulted.
    ///
    /// Costs one full-resolution decode per over-resolution image to classify,
    /// so it is off by default. Default: `None`.
    pub figure_dpi: Option<f32>,

    /// If true, remove the PDF's structure tree (accessibility metadata) for
    /// additional size reduction. Visually lossless; accessibility-lossy.
    /// Default: `false`.
    pub strip_accessibility: bool,

    /// If true, remove every `/Metadata` entry (XMP packets on the catalog,
    /// pages, and XObjects). Visually lossless — no viewer consults XMP to
    /// render — but it discards document provenance and breaks PDF/A and
    /// PDF/UA identification, so it is strictly opt-in. Producers that write a
    /// full XMP packet per page/XObject can spend a double-digit percentage of
    /// the file on it (adobe-spec: 860 KB, 12%). Default: `false`.
    pub strip_metadata: bool,

    /// If true, remove every `/PieceInfo` entry — the "page-piece"
    /// dictionaries of ISO 32000-1 14.5, where an authoring application
    /// stashes its own private data (Illustrator's `AIPrivateData`,
    /// InDesign's, Photoshop's). Conforming readers ignore them entirely, so
    /// this is visually lossless and changes no page content; what it costs
    /// is round-trip editability in the producing application, which is why
    /// it is opt-in like `strip_metadata`. Illustrator-authored figures carry
    /// the whole editable artwork alongside the flattened page: measured at
    /// 295 KB of a 374 KB file, and 307 KB inside a 2.2 MB paper.
    /// Default: `false`.
    pub strip_private_data: bool,

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

    /// Strip font hinting.
    ///
    /// For TrueType (`/FontFile2`) subsets: the `fpgm`, `prep`, and `cvt `
    /// tables plus every per-glyph instruction block. Only takes effect where
    /// `subset_fonts` already rewrites the program.
    ///
    /// For Type1C / CFF (`/FontFile3`) programs: the Type2 hint operators
    /// (`hstem`, `vstem`, `hstemhm`, `vstemhm`, `hintmask`, `cntrmask`) and
    /// their operands, plus the Private DICT hinting parameters
    /// (`BlueValues`, `StemSnap*`, …) that describe nothing once they are
    /// gone. This one applies to *every* Type1C program in the document,
    /// including the ones this run produced by union-merging fragments or
    /// converting Type1 — a hint strip changes no glyph name, glyph order, or
    /// advance width, so it is inert to every font dictionary pointing at the
    /// program. Coordinates are never re-encoded: charstrings are rewritten
    /// token by token and each glyph's outline and advance are re-verified
    /// from the emitted bytes.
    ///
    /// In both cases outlines, metrics, and character mappings are untouched,
    /// but hinted rasterization at small sizes can change (modern viewers
    /// largely ignore hinting; classic Windows GDI paths do not), so this is
    /// NOT covered by the pixel-identity contract and is strictly opt-in.
    /// Default: `false`.
    pub strip_hinting: bool,

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

    /// Flatten interactive forms: paint every widget annotation's appearance
    /// stream into its page's content stream at the position ISO 32000-1
    /// 12.5.5 places it, then remove the whole form layer — `/AcroForm`, the
    /// field tree, the XFA packet set (template, datasets, and the rest), and
    /// every `/Widget` annotation. Non-widget annotations (`/Link`, markup,
    /// popups) are not form machinery and are never touched.
    ///
    /// This is a **semantic** change, which is why it is opt-in: the output is
    /// no longer a form. It cannot be filled in, exported, or signed. What it
    /// is not, and will never be, is a silent loss of entered data. A field's
    /// value survives only if the appearance stream that *shows* it becomes
    /// page content, or if the field has no value to lose (`/V` absent, empty,
    /// or a button's `/Off`); anything else declines the entire document and
    /// the run proceeds byte-for-byte as if the flag were off. Dynamic XFA
    /// forms (`/NeedsRendering true`) always decline — their pages are a
    /// placeholder the reader builds from the XFA template, and picamatl does not
    /// render XFA. XFA data that the AcroForm field tree does not mirror
    /// declines. So does a value with no appearance to burn, a hidden field
    /// carrying data, `/NeedAppearances true` over a filled form, a signature
    /// field holding a signature, and an optional-content widget. The full
    /// decline table is `docs/FORMS-PLAN.md`.
    ///
    /// The win is the removed machinery: `corpus-expanded/irs-w2.pdf` spends
    /// 1.58 MB of its 2.15 MB on an XFA packet set whose AcroForm layer draws
    /// no ink at all. Default: `false`.
    pub flatten_forms: bool,

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

    /// Whether to detect digital signatures in the input. Every run
    /// re-serializes the whole document, so a `/ByteRange` digest — which
    /// pins file offsets — is invalid in the output. Detection covers the
    /// `/AcroForm` field tree (walked through `/Kids`, with `/FT` and `/V`
    /// inherited), merged field/widget signature dictionaries, and the
    /// catalog's `/Perms` `/UR3` usage-rights and `/DocMDP` certification
    /// entries.
    ///
    /// [`SignatureHandling::Warn`] (the default) runs the walk and returns
    /// the findings in [`OptimizeReport::signatures`] — which the `picamatl`
    /// binary prints as one stderr line, and only when the run actually
    /// returned a rewritten document. [`SignatureHandling::Silence`] skips
    /// detection entirely.
    pub on_signature: SignatureHandling,
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

/// Whether to look for digital signatures in the input document.
/// See [`OptimizeOptions::on_signature`].
///
/// The library never prints: this only decides whether the (read-only)
/// detection walk runs at all, and therefore whether
/// [`optimize_with_report`] can return any [`SignatureInfo`]. The picamatl
/// binary turns a non-empty report into its one stderr line.
///
/// `#[non_exhaustive]`: a future `Decline` variant (refuse to optimize a
/// signed document) would otherwise break every downstream `match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SignatureHandling {
    /// Detect signatures and report them. The default — the CLI turns the
    /// report into one stderr warning; a library caller reads
    /// [`OptimizeReport::signatures`].
    #[default]
    Warn,
    /// Skip detection entirely, for callers that already know (batch
    /// pipelines that pre-filter signed files, etc.). Costs nothing: the
    /// walk does not run and [`OptimizeReport::signatures`] is empty.
    Silence,
}

/// What [`optimize_with_report`] observed, alongside the bytes it returned.
///
/// `#[non_exhaustive]`: future findings ship in a minor release.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct OptimizeReport {
    /// Digital signatures the input carried, deduplicated by object id and
    /// ordered deterministically.
    ///
    /// **Non-empty only when `rewritten` is true.** A passthrough returns the
    /// input bytes verbatim, so nothing was invalidated and there is nothing
    /// to report — which is exactly what keeps the CLI's warning honest.
    /// Also always empty under [`SignatureHandling::Silence`], which skips
    /// detection.
    pub signatures: Vec<SignatureInfo>,
    /// True when the returned bytes are a rewritten, strictly smaller
    /// document; false when they are the input, byte for byte (nothing to
    /// do, a parse error, a caught panic, or a non-shrinking result).
    pub rewritten: bool,
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
            figure_dpi: None,
            strip_accessibility: false,
            strip_metadata: false,
            strip_private_data: false,
            pack_object_streams: true,
            downsample_flate_images: true,
            subset_fonts: true,
            convert_type1: false,
            strip_hinting: false,
            recompress_bitonal_images: false,
            allow_lossy_reencode: false,
            collapse_gray_images: false,
            flatten_forms: false,
            deflate_backend: DeflateBackend::Zlib,
            on_signature: SignatureHandling::Warn,
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

    /// Set the figure-detection target resolution in DPI. See the
    /// [`figure_dpi`](OptimizeOptions::figure_dpi) field docs.
    ///
    /// Validated at the setter rather than at use, so the stored `Option` is
    /// always a usable DPI: a non-finite or non-positive `dpi` means "off" and
    /// stores `None`. That makes `with_figure_dpi(0.0)` the natural CLI
    /// default for an absent flag.
    #[must_use]
    pub fn with_figure_dpi(mut self, dpi: f32) -> Self {
        self.figure_dpi = (dpi.is_finite() && dpi > 0.0).then_some(dpi);
        self
    }

    /// Enable/disable stripping the PDF structure tree (accessibility metadata).
    #[must_use]
    pub fn with_strip_accessibility(mut self, strip: bool) -> Self {
        self.strip_accessibility = strip;
        self
    }

    /// Enable/disable stripping every `/Metadata` (XMP) entry.
    #[must_use]
    pub fn with_strip_metadata(mut self, strip: bool) -> Self {
        self.strip_metadata = strip;
        self
    }

    /// Enable/disable stripping every `/PieceInfo` (private application data)
    /// entry. See [`OptimizeOptions::strip_private_data`].
    #[must_use]
    pub fn with_strip_private_data(mut self, strip: bool) -> Self {
        self.strip_private_data = strip;
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

    /// Set whether subsetted TrueType programs also lose their hinting
    /// (off by default; not pixel-identical). See
    /// [`OptimizeOptions::strip_hinting`].
    #[must_use]
    pub fn with_strip_hinting(mut self, strip: bool) -> Self {
        self.strip_hinting = strip;
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

    /// Enable/disable interactive-form flattening (off by default). See
    /// [`OptimizeOptions::flatten_forms`].
    #[must_use]
    pub fn with_flatten_forms(mut self, flatten: bool) -> Self {
        self.flatten_forms = flatten;
        self
    }

    /// Choose the deflate backend for the final re-deflate and xref-stream
    /// passes (zlib by default). See [`OptimizeOptions::deflate_backend`].
    #[must_use]
    pub fn with_deflate_backend(mut self, backend: DeflateBackend) -> Self {
        self.deflate_backend = backend;
        self
    }

    /// Set what happens when the input carries a digital signature. See
    /// [`OptimizeOptions::on_signature`].
    #[must_use]
    pub fn with_on_signature(mut self, on_signature: SignatureHandling) -> Self {
        self.on_signature = on_signature;
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
    optimize_with_report(input, options).0
}

/// [`optimize_with_options`] plus what the run observed: the same bytes, and an
/// [`OptimizeReport`] naming the digital signatures the input carried.
///
/// The bytes are byte-identical to what [`optimize_with_options`] returns for
/// the same arguments, and the fail-safe contract is the same one — this
/// function only adds the findings the optimizer already had to compute.
///
/// The library performs no I/O of its own: a caller that wants a user-visible
/// warning formats [`OptimizeReport::signatures`] itself (each
/// [`SignatureInfo`] `Display`s as `signature field (object 7 0)`), exactly as
/// the picamatl binary does. Detection only runs under
/// [`SignatureHandling::Warn`].
///
/// ```
/// use picamatl::{optimize_with_report, OptimizeOptions};
/// let (bytes, report) = optimize_with_report(b"not a pdf", OptimizeOptions::default());
/// assert_eq!(bytes, b"not a pdf");
/// assert!(!report.rewritten);
/// assert!(report.signatures.is_empty());
/// ```
pub fn optimize_with_report(input: &[u8], options: OptimizeOptions) -> (Vec<u8>, OptimizeReport) {
    // picamatl optimizes arbitrary user-supplied PDFs, and its contract is to
    // return the original bytes on ANY failure. try_optimize handles the
    // expected error paths (Result::Err), but a crafted PDF could still trigger
    // a panic deep in the JPEG decoder, the mozjpeg encoder, or lopdf. Catch it
    // here so a panic becomes the same graceful fallback as any other failure.
    let result = std::panic::catch_unwind(|| try_optimize(input, options));
    match result {
        // A rewritten document that actually got smaller. Everything else —
        // Ok(None) (nothing to do), a lopdf error, or a caught panic — falls
        // through to returning the input unchanged.
        //
        // The signatures ride along ONLY on this arm, and that is the whole
        // point: every other arm hands back `input` verbatim, so no
        // /ByteRange digest moved and there is nothing to warn about. A
        // report built here cannot describe an invalidation that did not
        // happen.
        Ok(Ok((Some(out), signatures))) if out.len() < input.len() => (
            out,
            OptimizeReport {
                signatures,
                rewritten: true,
            },
        ),
        _ => (input.to_vec(), OptimizeReport::default()),
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

/// A content-stream operand as a number, with 0.0 standing in for anything
/// that is not one.
///
/// The zero is a deliberate conservative guess, not an oversight: these
/// operands feed `cm`/`Tm` matrices whose only use is estimating how large an
/// image is *rendered*. A malformed operand collapses that estimate toward a
/// smaller rendered size, which raises the effective DPI and — at worst —
/// makes an image look under-resolved, i.e. leaves it untouched. Guessing
/// large would do the opposite and downsample something that is drawn big.
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

/// Depth bound for nested Form XObjects during placement collection.
const MAX_FORM_DEPTH: usize = 12;

/// The `/XObject` sub-dictionary of a resource dictionary, resolved.
fn xobject_map(
    doc: &Document,
    resources: Option<&lopdf::Dictionary>,
) -> HashMap<Vec<u8>, ObjectId> {
    let mut map = HashMap::new();
    let Some(resources) = resources else {
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
///
/// Placements inside Form XObjects count: a producer that wraps its page in a
/// form (Illustrator, LaTeX `\includegraphics`, IRS's XFA-rendered forms) used
/// to hide every image it draws from this scan, and an image with no recorded
/// placement is never downsampled. On `corpus/irs-1040gi.pdf` that hid a
/// 5120x3413 scan drawn at 405 effective DPI — 1.45 MB, half the optimized
/// file — behind one form.
///
/// Failing to see a placement is safe (the image is left alone); *under*-
/// stating one is not, so anything unparseable simply contributes no
/// placement, and a form's `/BBox` clip is deliberately ignored — clipping can
/// only ever make the visible area smaller than what we record.
fn collect_placements(doc: &Document) -> HashMap<ObjectId, (f32, f32)> {
    let mut sizes: HashMap<ObjectId, (f32, f32)> = HashMap::new();

    for (_, page_id) in doc.get_pages() {
        let resources = page_resources(doc, page_id);
        // Bounded inflate: a page whose /Contents is a small zlib stream that
        // expands without limit must not be able to exhaust memory here (an
        // OOM abort is the one failure `catch_unwind` cannot turn into a
        // pass-through). A page over the budget contributes no placements,
        // which is the safe direction — its images are simply left alone.
        let Ok(content_bytes) = doc.get_page_content_with_limit(page_id, MAX_REDEFLATE_BYTES)
        else {
            continue;
        };
        let mut path: Vec<ObjectId> = Vec::new();
        walk_placements(
            doc,
            &content_bytes,
            resources,
            Mat::IDENTITY,
            &mut sizes,
            &mut path,
        );
    }

    sizes
}

/// Walk one content stream's operators in its resource context, recording
/// image placements and recursing into the Form XObjects it draws.
fn walk_placements(
    doc: &Document,
    content_bytes: &[u8],
    resources: Option<&lopdf::Dictionary>,
    initial_ctm: Mat,
    sizes: &mut HashMap<ObjectId, (f32, f32)>,
    path: &mut Vec<ObjectId>,
) {
    let names = xobject_map(doc, resources);
    if names.is_empty() {
        return;
    }
    let Ok(content) = Content::decode(content_bytes) else {
        return;
    };

    let mut ctm = initial_ctm;
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
                let Some(Object::Name(name)) = op.operands.first() else {
                    continue;
                };
                let Some(&id) = names.get(name) else { continue };
                let Ok(Object::Stream(stream)) = doc.get_object(id) else {
                    continue;
                };
                match stream.dict.get(b"Subtype").map(|s| resolve(doc, s)) {
                    Ok(Object::Name(n)) if n == b"Image" => {
                        let (w, h) = (ctm.rendered_width(), ctm.rendered_height());
                        let entry = sizes.entry(id).or_insert((0.0, 0.0));
                        entry.0 = entry.0.max(w);
                        entry.1 = entry.1.max(h);
                    }
                    Ok(Object::Name(n)) if n == b"Form" => {
                        // Cycle guard and depth bound: a malformed form
                        // reachable from itself must not loop.
                        if path.contains(&id) || path.len() >= MAX_FORM_DEPTH {
                            continue;
                        }
                        let form_ctm = match stream.dict.get(b"Matrix").map(|m| resolve(doc, m)) {
                            Ok(Object::Array(items)) if items.len() == 6 => Mat {
                                a: num(&items[0]),
                                b: num(&items[1]),
                                c: num(&items[2]),
                                d: num(&items[3]),
                                e: num(&items[4]),
                                f: num(&items[5]),
                            }
                            .concat(ctm),
                            // No /Matrix means the identity; anything else is
                            // unreadable, and guessing a scale here could
                            // UNDER-state the placement. Skip the form.
                            Err(_) | Ok(Object::Null) => ctm,
                            _ => continue,
                        };
                        // A form without its own /Resources inherits the
                        // context it is drawn in (ISO 32000-1 8.10.2).
                        let own = match stream.dict.get(b"Resources").map(|r| resolve(doc, r)) {
                            // Bind what the arm already matched instead of
                            // repeating the lookup and unwrapping it: this walk
                            // runs over untrusted input, and the crate's
                            // never-panics contract is not worth spending on a
                            // borrow workaround.
                            Ok(own @ Object::Dictionary(_)) => own.as_dict().ok(),
                            _ => resources,
                        };
                        let Some(bytes) = content_stream_plain(doc, stream) else {
                            continue;
                        };
                        path.push(id);
                        walk_placements(doc, &bytes, own, form_ctm, sizes, path);
                        path.pop();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
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

/// The number of colour components an image dict DECLARES, or `None` when the
/// colour space is one we cannot count with certainty (an unresolvable
/// reference, a pattern, a name we do not know).
///
/// Used only as a consistency cross-check against the payload's own frame
/// header — never to decide how to decode anything.
fn colorspace_component_count(doc: &Document, dict: &lopdf::Dictionary) -> Option<usize> {
    let cs = resolve(doc, dict.get(b"ColorSpace").ok()?);
    match cs {
        Object::Name(n) => match n.as_slice() {
            b"DeviceGray" | b"CalGray" | b"G" => Some(1),
            b"DeviceRGB" | b"CalRGB" | b"RGB" => Some(3),
            b"DeviceCMYK" | b"CMYK" => Some(4),
            _ => None,
        },
        Object::Array(items) => {
            let Object::Name(family) = resolve(doc, items.first()?) else {
                return None;
            };
            match family.as_slice() {
                b"CalGray" => Some(1),
                b"CalRGB" | b"Lab" => Some(3),
                // The ICC profile's own /N is authoritative for the stream.
                b"ICCBased" => {
                    let Object::Stream(profile) = resolve(doc, items.get(1)?) else {
                        return None;
                    };
                    usize::try_from(profile.dict.get(b"N").ok()?.as_i64().ok()?).ok()
                }
                // Indexed and Separation are both one sample per pixel.
                b"Indexed" | b"I" | b"Separation" => Some(1),
                b"DeviceN" => match resolve(doc, items.get(1)?) {
                    Object::Array(names) => Some(names.len()),
                    _ => None,
                },
                _ => None,
            }
        }
        _ => None,
    }
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
    // The finiteness half matters as much as the sign: a content stream whose
    // matrix arithmetic overflowed to ±inf, or whose operands multiplied out
    // to NaN, must decline here rather than reach the target geometry below
    // (NaN survives `<= 0.0`, and `f32::max` would then quietly turn it into a
    // 1px target).
    if !rendered_w_pts.is_finite()
        || !rendered_h_pts.is_finite()
        || rendered_w_pts <= 0.0
        || rendered_h_pts <= 0.0
    {
        return None;
    }

    // Defensive: a non-positive target DPI means "do not downsample".
    // Without this guard, target_w/target_h below would collapse toward 1px.
    let target_dpi = options.target_dpi;
    if !target_dpi.is_finite() || target_dpi <= 0.0 {
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

    // Dict/payload consistency for four-component JPEGs. The dict says how
    // many samples a pixel has; the frame header says how many the payload
    // carries. When those disagree the image is already broken, and any
    // re-encode picks a side — which is exactly how the pre-0.3.2 CMYK bug
    // wrote three RGB channels back under an unchanged /DeviceCMYK.
    //
    // `jpeg_route` handles the common case, but it reads the frame header,
    // and a payload damaged badly enough to defeat the structural walk while
    // a lenient decoder keeps going would slip past it (the mutation sweep in
    // `mutated_cmyk_streams_never_corrupt_or_escape` finds exactly that).
    // Hence `actual != Some(4)` rather than `actual == Some(n)`: an
    // unreadable header under a four-component colour space is a decline.
    //
    // Deliberately narrowed to mismatches INVOLVING four components. A
    // grayscale JPEG under /DeviceRGB is a long-standing, benign shape this
    // pipeline already handles (`build_pdf_gray_in_rgb`), and widening the
    // check would change behaviour on files that were never broken.
    if matches!(class, FilterClass::DctOnly) {
        let declared = colorspace_component_count(doc, dict);
        let actual = jpeg_component_count(&stream.content);
        if declared == Some(4) && actual != Some(4) {
            return None;
        }
        if actual == Some(4) && declared.is_some_and(|d| d != 4) {
            return None;
        }
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
    // Target geometry and the over-resolution verdict for a given DPI. Both
    // inputs are finite and positive by the guards above, so the products can
    // only leave the finite range by overflowing to +inf — which `as u32`
    // would saturate to u32::MAX, an "upscale to 4 billion pixels" target.
    // Decline instead of clamping: a placement that large is not a real page.
    let geometry_at = |dpi: f32| -> Option<(u32, u32, bool)> {
        let target_w_f = (rendered_w_pts / 72.0) * dpi;
        let target_h_f = (rendered_h_pts / 72.0) * dpi;
        if !target_w_f.is_finite() || !target_h_f.is_finite() {
            return None;
        }
        let target_w = target_w_f.round().max(1.0) as u32;
        let target_h = target_h_f.round().max(1.0) as u32;
        let over =
            eff_dpi_w.max(eff_dpi_h) > dpi * dpi_margin && target_w < px_w && target_h < px_h;
        Some((target_w, target_h, over))
    };
    let (mut target_w, mut target_h, mut over_resolution) = geometry_at(target_dpi)?;

    // `--figure-dpi`: charts/diagrams with rendered text get a HIGHER target so
    // their axis labels stay legible, while photographic content keeps
    // compressing at the ordinary one. The ONLY semantic change is which DPI
    // feeds the three values above.
    //
    // Two gates before the classifier runs, and both are free:
    //   - `fig_dpi > target_dpi`. At or below the ordinary target the knob is
    //     documented as inert, which is what makes "only ever more faithful"
    //     true rather than aspirational.
    //   - `over_resolution` at the ORDINARY target. Being over-resolution at a
    //     higher DPI strictly implies being over-resolution at a lower one, so
    //     an image that already fails this gate cannot pass it at `fig_dpi`;
    //     and `target_w`/`target_h` are read only on the over-resolution
    //     branches below. Skipping here is therefore not an approximation —
    //     it is the same answer without the full-resolution decode.
    let promote = options
        .figure_dpi
        .is_some_and(|fig_dpi| fig_dpi > target_dpi)
        && over_resolution
        && image_is_figure_like(doc, stream, &class, px_w, px_h);
    if promote {
        // `promote` is only true when `figure_dpi` is `Some`; `??` declines
        // rather than panicking if that ever stops holding, matching the
        // "any doubt leaves the image untouched" contract.
        let (w, h, over) = geometry_at(options.figure_dpi?)?;
        target_w = w;
        target_h = h;
        over_resolution = over;
    }

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
        // `smask_present && smask_id.is_none()` already returned above, so this
        // is `Some` — but the invariant lives 60 lines away, and a panic here
        // would be a page-wide DoS on untrusted input. `?` declines instead:
        // the outer contract is "any doubt leaves the image untouched".
        let mask_id = smask_id?;
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
                doc,
                id,
                mask_id,
                stream,
                PairGeometry {
                    px_w,
                    px_h,
                    target_w,
                    target_h,
                },
                options,
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
                doc,
                id,
                mask_id,
                stream,
                PairGeometry {
                    px_w,
                    px_h,
                    target_w,
                    target_h,
                },
                options,
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
            let out = plan_dct_resize_verified(stream, options, target_w, target_h)?;
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

/// Which pipeline a DCTDecode payload belongs to, decided from its frame
/// header's component count alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum JpegRoute {
    /// 1 or 3 components — the historical gray/RGB pipeline, byte-for-byte
    /// unchanged. An unparseable header also lands here so that streams the
    /// real decoders have always accepted keep being accepted.
    GrayOrRgb,
    /// 4 components — CMYK or YCCK, handled by [`plan_dct_cmyk`].
    Cmyk,
    /// Anything else (2 components, or a frame header claiming 0). Nothing
    /// here knows what those channels mean, so the stream is left untouched
    /// rather than passed to a decoder that would guess.
    Decline,
}

fn jpeg_route(data: &[u8]) -> JpegRoute {
    match jpeg_component_count(data) {
        Some(4) => JpegRoute::Cmyk,
        Some(1) | Some(3) | None => JpegRoute::GrayOrRgb,
        Some(_) => JpegRoute::Decline,
    }
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
///
/// This is the ONLY place the full-decode fallback lives, so the
/// decompression-bomb budget below is applied once for every caller. The
/// fallback is the dangerous half: unlike the scaled path it is bounded by
/// nothing but the frame header, so a tiny crafted stream that libjpeg-turbo
/// refuses and `jpeg-decoder` accepts would allocate gigabytes — an OOM abort,
/// which is a process kill that `catch_unwind` cannot contain.
fn decode_jpeg(data: &[u8], target_w: u32, target_h: u32) -> Option<(DynamicImage, bool)> {
    if let Some(decoded) = decode_jpeg_scaled(data, target_w, target_h) {
        return Some(decoded);
    }
    // libjpeg refused the stream. Before handing the same bytes to a more
    // permissive decoder, price the allocation it would make from the frame
    // header alone. An unreadable header declines: without dimensions there is
    // no ceiling to check, and "we cannot tell" is the decline case
    // everywhere else in this crate.
    let frame = jpeg_frame_info(data)?;
    if !jpeg_decode_within_budget(frame.width, frame.height, frame.components) {
        return None;
    }
    let decoded = image::load_from_memory_with_format(data, ImageFormat::Jpeg).ok()?;
    let is_gray = matches!(
        decoded,
        DynamicImage::ImageLuma8(_) | DynamicImage::ImageLuma16(_)
    );
    Some((decoded, is_gray))
}

/// True when a raster of `width * height * channels` bytes is small enough to
/// materialize. Mirrors [`MAX_FLATE_PIXEL_BYTES`] — the same 256 MiB ceiling
/// the Flate path applies before inflating, for the same reason, and shared by
/// every decoder that allocates from numbers a file told it (JPEG frame
/// headers, JPEG2000 codestream headers).
///
/// Overflowing the multiplication is itself a decline: a raster whose size
/// does not fit in a u64 is not one we are going to decode.
fn raster_within_budget(width: u64, height: u64, channels: u64) -> bool {
    width
        .checked_mul(height)
        .and_then(|px| px.checked_mul(channels))
        .is_some_and(|bytes| bytes <= MAX_FLATE_PIXEL_BYTES)
}

/// [`raster_within_budget`] for a JPEG frame header.
///
/// A zero component count is priced as 4 (the widest this crate ever decodes)
/// rather than as free: a frame header claiming zero components is already
/// nonsense, and pricing it at zero would wave it straight through.
fn jpeg_decode_within_budget(width: u16, height: u16, components: u8) -> bool {
    let channels = if components == 0 {
        4u64
    } else {
        u64::from(components)
    };
    raster_within_budget(u64::from(width), u64::from(height), channels)
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

    match jpeg_route(&stream.content) {
        // CMYK/YCCK: the same dimension-preserving contract, enforced inside
        // `plan_dct_cmyk` by its exact-source-geometry mode. It carries the
        // decode-back verification and the quantization-table idempotence
        // guard too, so this branch is complete as written.
        JpegRoute::Cmyk => return plan_dct_cmyk(stream, quality, px_w, px_h, true),
        JpegRoute::Decline => return None,
        JpegRoute::GrayOrRgb => {}
    }

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
    match jpeg_route(&stream.content) {
        // A CMYK base under an /SMask: the mask half is an independent
        // single-channel stream, so the pair only needs the base to land on
        // the same target geometry — which `plan_dct_cmyk` guarantees, along
        // with the decode-back verification this function is named for.
        JpegRoute::Cmyk => return plan_dct_cmyk(stream, quality, target_w, target_h, false),
        JpegRoute::Decline => return None,
        JpegRoute::GrayOrRgb => {}
    }
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
    geom: PairGeometry,
) -> Option<Vec<u8>> {
    let PairGeometry {
        px_w,
        px_h,
        target_w,
        target_h,
    } = geom;
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

/// The four numbers every coupled base+`/SMask` plan carries: the pair's
/// stored pixel dimensions and the geometry both halves are resampled to.
/// They only ever travel together — the whole point of D-M2/D-M3 is that base
/// and mask land on identical pixel grids — so they move as one value rather
/// than as four positional `u32`s that a call site could transpose.
#[derive(Debug, Clone, Copy)]
struct PairGeometry {
    px_w: u32,
    px_h: u32,
    target_w: u32,
    target_h: u32,
}

/// The combined never-larger / 5%-minimum-savings guard shared by D-M2 and
/// D-M3, evaluated over the pair rather than either stream alone.
///
/// The 5% floor is the D-M1 philosophy ported to the pair: a genuine first
/// pass saves far more, while a second pass over an already-optimized pair
/// lands under 5% and is declined (downsampling to a fixed DPI is naturally
/// idempotent anyway — the `target_* >= px_*` half of `over_resolution`
/// declines before either planner is reached). It also enforces strict-shrink
/// atomicity: the candidate must beat the pair's original size by the full 5%
/// before both streams are replaced.
///
/// `true` means "declined", so the two call sites read the same way as the
/// no-compounding-losses predicate D-M3 builds on top of it.
fn pair_savings_declined(
    base_original: usize,
    mask_original: usize,
    base_candidate: usize,
    mask_candidate: usize,
) -> bool {
    let combined_original = base_original + mask_original;
    let combined_candidate = base_candidate + mask_candidate + MASK_DICT_OVERHEAD;
    combined_candidate * 100 >= combined_original * 95
}

/// Phase 5 D-M2: coupled downsampling of an over-resolution JPEG base and its
/// eligible `/SMask`. Both streams are planned together — and carried by the
/// single returned `Replacement` so the apply pass replaces them together.
/// Any doubt on either side, or a COMBINED size that fails the never-larger /
/// 5% idempotence guard, returns `None` and the whole pair stays untouched.
fn plan_smask_pair_downsample(
    doc: &Document,
    id: ObjectId,
    mask_id: ObjectId,
    base_stream: &lopdf::Stream,
    geom: PairGeometry,
    options: OptimizeOptions,
) -> Option<Replacement> {
    let PairGeometry {
        target_w, target_h, ..
    } = geom;

    // Base: JPEG at the target geometry with the existing MAD decode-back.
    let base_out = plan_dct_resize_verified(base_stream, options, target_w, target_h)?;

    // Mask: decode (JPEG or Flate) + resample to the SAME geometry + Flate
    // re-encode, verified by its own decode-back.
    let mask_stream = doc.get_object(mask_id).ok()?.as_stream().ok()?;
    let mask_out = plan_mask_resample(doc, mask_stream, geom)?;

    // ATOMICITY + idempotence, evaluated over the COMBINED pair.
    if pair_savings_declined(
        base_stream.content.len(),
        mask_stream.content.len(),
        base_out.len(),
        mask_out.len(),
    ) {
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
fn plan_flate_smask_pair_downsample(
    doc: &Document,
    id: ObjectId,
    mask_id: ObjectId,
    base_stream: &lopdf::Stream,
    geom: PairGeometry,
    options: OptimizeOptions,
) -> Option<Replacement> {
    let PairGeometry {
        px_w,
        px_h,
        target_w,
        target_h,
    } = geom;

    // Base: the exact eligibility gates and re-encode variants of the unmasked
    // Flate route (8bpc only, no /Decode, capped inflate + exact length check,
    // plain-vs-Up-predictor output selection). Flate is lossless, so the
    // deflate round trip needs no MAD-style decode-back of its own.
    let (base_out, decode_parms) = plan_flate(doc, base_stream, px_w, px_h, target_w, target_h)?;

    // Mask: decode (JPEG or Flate) + resample to the SAME geometry + Flate
    // re-encode, verified by its own exact decode-back.
    let mask_stream = doc.get_object(mask_id).ok()?.as_stream().ok()?;
    let mask_out = plan_mask_resample(doc, mask_stream, geom)?;

    // NO COMPOUNDING LOSSES, the pair's form of the unmasked rule: the geometry
    // change is the LOSSLESS path's decision. If the fully lossless pair
    // candidate would itself be declined by the combined guard below — the
    // resample did not pay for itself in the format that preserves every sample
    // — then a JPEG base must not resurrect it by hiding the resolution loss
    // behind a DCT win. The predicate is the exact guard the pair is judged by,
    // so "declined" means the same thing on both sides.
    let lossless_declined = pair_savings_declined(
        base_stream.content.len(),
        mask_stream.content.len(),
        base_out.len(),
        mask_out.len(),
    );
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

    // ATOMICITY + idempotence over the COMBINED pair — the same guard, re-run
    // against whichever base candidate won.
    if pair_savings_declined(
        base_stream.content.len(),
        mask_stream.content.len(),
        content.len(),
        mask_out.len(),
    ) {
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

/// The smallest `n/8` DCT scale whose output still covers the target in BOTH
/// axes (never upscale), so the caller's resampling step always downsamples.
/// Falls back to 8/8 — an unscaled decode — when no smaller scale covers the
/// target. Shared by both scaled decoders so the two stay in lockstep.
fn dct_scale_numerator(full_w: usize, full_h: usize, target_w: u32, target_h: u32) -> u8 {
    for n in 1..=8u8 {
        let scaled_w = (full_w * n as usize).div_ceil(8);
        let scaled_h = (full_h * n as usize).div_ceil(8);
        if scaled_w >= target_w as usize && scaled_h >= target_h as usize {
            return n;
        }
    }
    8
}

/// True when the raster libjpeg would hand back at scale `numerator/8` fits
/// inside the [`MAX_FLATE_PIXEL_BYTES`] budget. The scaled path is bounded by
/// the target geometry in the ordinary case, but its floor is 1/8 of the
/// source, and 1/8 of a bomb is still a bomb.
fn scaled_decode_within_budget(full_w: usize, full_h: usize, numerator: u8, channels: u64) -> bool {
    let scaled_w = (full_w as u64 * numerator as u64).div_ceil(8);
    let scaled_h = (full_h as u64 * numerator as u64).div_ceil(8);
    raster_within_budget(scaled_w, scaled_h, channels)
}

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

    let numerator = dct_scale_numerator(full_w, full_h, target_w, target_h);
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

    // Even the scaled path allocates: 1/8 of an absurd source is still absurd.
    if !scaled_decode_within_budget(full_w, full_h, numerator, if is_gray { 1 } else { 3 }) {
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

/// Encode an image as JPEG using mozjpeg (optimized Huffman + trellis), which
/// produces substantially smaller files than the basic encoder at equal
/// quality. Channel count is preserved to match the unchanged PDF /ColorSpace.
///
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
        let channels = if is_gray { 1usize } else { 3 };
        // Price the raster BEFORE decoding. `decode()` allocates whatever the
        // codestream header declares, so a JPEG2000 decompression bomb — a few
        // KB claiming 60000x60000 — is an OOM abort, and an abort is a process
        // kill no `catch_unwind` can contain. Same 256 MiB ceiling the Flate
        // and DCT paths apply, for the same reason. The length check below
        // stays: it catches a codestream that lied the other way.
        if !raster_within_budget(u64::from(w), u64::from(h), channels as u64) {
            continue;
        }
        let Ok(pixels) = jpx.decode() else {
            continue;
        };
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
/// Why this is lossless: for two streams with the same decoded payload AND the
/// same dictionary, the only observable effect is that decoded payload — they
/// render identically and extract identically, so merging merely changes which
/// object a reference points at.
///
/// The dictionary is part of the key, minus `/Length` (which is a property of
/// the *stored* bytes, and differing `/Length` is the whole point of this
/// pass). Decoded bytes alone are NOT a render equivalence: the same payload
/// under a different dictionary is a different image. On the 756-page Adobe
/// spec, keying on the payload alone merged five image pairs that must stay
/// distinct — two with transposed dimensions (65x66 vs 66x65, identical index
/// bytes), and three sharing index bytes under *different* `/Indexed` palette
/// objects, which silently recolored the overprint figure on page 752.
///
/// Fail-safe: only scalar `FlateDecode` with no `/DecodeParms` is considered
/// (the shape whose decode is a pure byte-for-byte inflate, so decoded
/// equality is exactly the equivalence a viewer renders); any inflate error,
/// oversize output, or unsupported shape is skipped. The merge is keyed on the
/// full decoded bytes (not a hash), so a collision cannot cause a false merge.
/// Dictionaries are compared by their `Debug` rendering, matching
/// [`dedup_objects`]' conservative stance: differing key order or differing
/// spelling of an equivalent value simply declines the merge.
fn dedup_decoded_streams(doc: &mut Document) -> bool {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // (decoded length, decoded hash, dict minus /Length) -> object ids.
    // The decoded bytes themselves are NOT kept: the per-stream inflate cap
    // is 128 MiB, so holding every payload at once would let a document of
    // many small bomb streams allocate N x 128 MiB before any bucket is
    // examined (an OOM abort escapes `catch_unwind`). Candidates in a bucket
    // of two or more are re-inflated and compared byte-for-byte below, the
    // same verify-before-merge shape `dedup_streams` uses.
    let mut buckets: HashMap<(usize, u64, String), Vec<ObjectId>> = HashMap::new();
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
                let mut dict = s.dict.clone();
                dict.remove(b"Length");
                let mut hasher = DefaultHasher::new();
                decoded.hash(&mut hasher);
                buckets
                    .entry((decoded.len(), hasher.finish(), format!("{dict:?}")))
                    .or_default()
                    .push(id);
            }
        }
    }

    // Lowest object id wins as canonical; everything else redirects to it —
    // after its decoded bytes are confirmed equal (a hash is a hint, not a
    // proof). One inflate of the canonical per bucket, one per candidate.
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
        let Some(canon_decoded) = inflate_capped(&canon.content, MAX_REDEFLATE_BYTES) else {
            continue;
        };
        for dup in &ids[1..] {
            let Some(Object::Stream(other)) = doc.objects.get(dup) else {
                continue;
            };
            let same = inflate_capped(&other.content, MAX_REDEFLATE_BYTES)
                .is_some_and(|decoded| decoded == canon_decoded);
            if same {
                remap.insert(*dup, canonical);
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

/// Dict-text cost charged to a candidate that adds `/Filter/FlateDecode` to a
/// previously unfiltered stream.
const FILTER_DICT_COST: usize = b"/Filter/FlateDecode".len();

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
    if doc.is_encrypted() || fonts::pdfa_blocked(doc) {
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

/// The outcome of one optimization attempt: the rewrite (if any) plus the
/// signatures detected in the input.
type OptimizeOutcome = (Option<Vec<u8>>, Vec<SignatureInfo>);

/// Returns `Ok((None, _))` when there was genuinely nothing to do, so the
/// caller can hand back the original bytes without anyone allocating a
/// throwaway copy of them. `Ok((Some(bytes), _))` is a real, rewritten
/// document.
///
/// The second element is the signatures detected in the *input*. It travels
/// out rather than being reported here so that the caller — which is the only
/// place that knows whether the rewrite was actually kept — decides whether
/// anyone hears about it.
fn try_optimize(input: &[u8], options: OptimizeOptions) -> Result<OptimizeOutcome, lopdf::Error> {
    // Read the input's own real literals BEFORE lopdf parses them into f32s
    // that cannot hold their digits. Empty for the overwhelming majority of
    // documents, in which case the restoration pass at save time is a no-op
    // and the output is byte-identical to what it would be without it. See
    // src/reals.rs.
    let literals = reals::capture(input);
    let mut doc = Document::load_mem(input)?;

    // Signature detection. Every save path re-serializes from scratch, so a
    // /ByteRange digest is invalidated whether or not any pass touches the
    // signature objects themselves. Detection happens HERE, before the dedup
    // passes, so the findings name the input's signature objects rather than
    // whatever survives dedup — but they are only *returned*, never
    // announced: the decision to keep the rewrite is made in
    // `optimize_with_report`, and on every other path the caller gets the
    // input back and nothing was invalidated at all.
    //
    // Gated so `Silence` pays literally nothing — the walk does not run.
    let signatures = if options.on_signature == SignatureHandling::Warn {
        detect_signatures(&doc)
    } else {
        Vec::new()
    };

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
            return Ok((None, signatures));
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

    // Opt-in form flattening, BEFORE every other planner: the appearance
    // streams it moves into page content must then be minified, re-deflated
    // and (if they carry images) planned like any other page content, and the
    // XFA / field-tree objects it orphans must already be unreferenced when
    // `prune_objects()` runs. A `None` plan is a decline — the document is
    // optimized exactly as it would have been with the flag off. Counts as
    // work: dropping a megabyte of XFA is the whole point.
    let flattened = options.flatten_forms
        && match forms::plan_flatten(&doc) {
            Some(plan) => {
                forms::apply_flatten(&mut doc, plan);
                true
            }
            None => false,
        };

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
        fonts::plan_font_subsets(
            &doc,
            options.subset_fonts,
            options.convert_type1,
            options.strip_hinting,
        )
    } else {
        Vec::new()
    };

    // Plan lossless same-family Type1C union merges (read-only; empty unless
    // at least one family clears every byte-conservative precondition).
    let t1c_merge_plans = if options.subset_fonts {
        fonts::plan_type1c_merges(&doc)
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
    let nothing_else_qualified = replacements.is_empty()
        && font_plans.is_empty()
        && bitonal_plans.is_empty()
        && jpx_plans.is_empty()
        && !options.strip_accessibility
        && !merged_streams
        && !merged_decoded
        && !minified
        && !flattened
        && !gray_work;

    // The JPEG Huffman re-optimizer normally runs at the end of the pipeline,
    // after every stream this pass replaces is in place. But for a document
    // nothing else qualified to touch there is nothing to wait for, and the
    // question "is there any work at all" can only be answered by doing the
    // entropy analysis anyway — so run the real pass here and keep its answer,
    // instead of probing with a throwaway analysis and then redoing it below.
    // It has to be asked: a PDF whose only win is pass-through JPEGs with
    // unoptimized Huffman tables is a real win, not a serialization detail,
    // and must not be declined.
    let jpeg_huffman_done = nothing_else_qualified;
    let jpeg_huffman_work = jpeg_huffman_done && reoptimize_jpeg_streams(&mut doc);

    if nothing_else_qualified
        && !jpeg_huffman_work
        // Same story for the opt-in Type1C hint strip: it is real work, and
        // it is the only thing that happens in a `--strip-hinting` run over a
        // document whose fonts nothing else qualifies to touch.
        && !(options.strip_hinting && fonts::any_type1c_hint_work(&doc))
    {
        return Ok((None, signatures));
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
    fonts::apply_type1c_merges(&mut doc, t1c_merge_plans);

    // Type1C (CFF) hint stripping, under the same `--strip-hinting` consent as
    // the TrueType path. Runs after the font plans are applied so it also
    // covers the programs this run just produced — union merges and
    // Type1 → Type1C conversions — not only the ones nothing else touched.
    if options.strip_hinting {
        fonts::strip_type1c_hints(&mut doc);
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

    // Optionally drop every XMP packet. Producers that stamp a full packet on
    // each page and XObject can spend a double-digit percentage of the file on
    // metadata no viewer reads to render (adobe-spec: 134 packets, 860 KB,
    // 12% of the optimized output). The reference is what costs bytes; the
    // orphaned streams themselves go in `prune_objects()` below.
    if options.strip_metadata {
        for object in doc.objects.values_mut() {
            match object {
                Object::Dictionary(dict) => dict.remove(b"Metadata"),
                Object::Stream(stream) => stream.dict.remove(b"Metadata"),
                _ => None,
            };
        }
    }

    // Optionally drop every page-piece dictionary (ISO 32000-1 14.5): private
    // data an authoring application keeps beside the page it rendered.
    // Illustrator's is the extreme case — the entire editable artwork,
    // PostScript-encoded, next to the flattened page that draws it (295 KB of
    // a 374 KB corpus file). No conforming reader consults it to render;
    // dropping it costs round-trip editability in the producing application
    // and nothing else, which is why it is opt-in. As with `/Metadata`, only
    // the reference is removed here — `prune_objects()` below collects the
    // orphaned streams.
    if options.strip_private_data {
        for object in doc.objects.values_mut() {
            match object {
                Object::Dictionary(dict) => dict.remove(b"PieceInfo"),
                Object::Stream(stream) => stream.dict.remove(b"PieceInfo"),
                _ => None,
            };
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

    // Rebuild the Huffman tables of every JPEG payload from its own symbol
    // statistics. Strictly lossless (the DCT coefficients never move; see
    // src/jpeghuff.rs), so it needs no consent flag and runs unconditionally.
    // Placed here, after every image decision, for the same reason as the
    // re-deflate below: it is a pure re-serialization of bytes already chosen.
    // Skipped when the "is there any work at all" check above already ran it —
    // which it only does for a document where nothing else qualified, so no
    // image decision came after it there either.
    if !jpeg_huffman_done {
        reoptimize_jpeg_streams(&mut doc);
    }

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

    save_document(&mut doc, options, &literals).map(|out| (Some(out), signatures))
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
/// picamatl's output that predictor declaration sat on top of the unpredicted
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

/// Re-optimize the Huffman tables of every raw `/DCTDecode` payload in the
/// document. Returns true when at least one stream was replaced — the answer
/// `try_optimize` needs for a document nothing else qualified to touch, where
/// this pass is the only thing standing between "hand back the input bytes"
/// and a real win.
///
/// This is the JPEG analogue of [`redeflate_flate_streams`]: an entropy-coding
/// improvement over bytes whose *content* is already decided. It is strictly
/// lossless — [`jpeghuff::optimize`] never touches a DCT coefficient and
/// verifies its own output decodes back to the identical token sequence — so
/// unlike the image paths it needs no pixel contract and no opt-in.
///
/// Streams picamatl re-encoded itself carry mozjpeg's already-optimal tables and
/// simply decline (nothing is smaller). The headroom is in the JPEGs the image
/// path passes through untouched: CMYK/Separation payloads, and any image
/// whose effective DPI is already at target.
fn reoptimize_jpeg_streams(doc: &mut Document) -> bool {
    if doc.is_encrypted() || fonts::pdfa_blocked(doc) {
        return false;
    }

    // Collect first (classification resolves references against the immutable
    // document), then do the entropy work off-document, in parallel.
    let candidates: Vec<(ObjectId, Vec<u8>)> = doc
        .objects
        .iter()
        .filter_map(|(&id, obj)| {
            let Object::Stream(stream) = obj else {
                return None;
            };
            let filter = stream.dict.get(b"Filter").ok()?;
            matches!(classify_filter(doc, filter), FilterClass::DctOnly)
                .then(|| (id, stream.content.clone()))
        })
        .collect();

    let shrunk: Vec<(ObjectId, Vec<u8>)> = candidates
        .into_par_iter()
        .filter_map(|(id, content)| jpeghuff::optimize(&content).map(|out| (id, out)))
        .collect();

    let mut changed = false;
    for (id, content) in shrunk {
        if let Ok(Object::Stream(stream)) = doc.get_object_mut(id) {
            stream.set_content(content); // keeps /Length in sync
                                         // Counted only when a stream really was rewritten: an id that no
                                         // longer resolves is not work done.
            changed = true;
        }
    }
    changed
}

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
/// Declined wholesale for encrypted documents (stream bytes are ciphertext)
/// and PDF/A-declared documents. Signed documents are NOT declined: see the
/// module header of `src/signatures.rs` for why a signature guard here
/// protects nothing.
fn redeflate_flate_streams(doc: &mut Document, backend: DeflateBackend) {
    if doc.is_encrypted() || fonts::pdfa_blocked(doc) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::content::Operation;
    use lopdf::{dictionary, Stream};

    use crate::classify::tests::chart_pixels;
    use crate::classify::{
        line_art_metrics, FIGURE_MAX_BACKGROUND, FIGURE_MIN_BACKGROUND, FIGURE_MIN_EDGES,
    };
    use crate::cmyk::tests::jpeg_fixture;
    use crate::deflate::paeth_predict;
    use crate::serialize::find_sub;

    /// Build a one-page PDF embedding a `px`×`px` RGB JPEG drawn into a
    /// `draw_pts`×`draw_pts` box, i.e. at an effective DPI of px/(draw_pts/72).
    pub(crate) fn build_pdf(px: u32, draw_pts: i64) -> Vec<u8> {
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
    pub(crate) fn build_pdf_duplicate_images(copies: usize, px: u32) -> Vec<u8> {
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
    pub(crate) fn photo_pixels(px_w: u32, px_h: u32, channels: usize) -> Vec<u8> {
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
    pub(crate) fn build_pdf_flate_raw(
        raw: &[u8],
        px: u32,
        draw_pts: i64,
        channels: usize,
    ) -> Vec<u8> {
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

    /// Mean absolute difference between a reference RGB buffer and a JPEG's
    /// decode-back, in 0..=255 units — the quantity `decode_back_matches`
    /// thresholds against `DECODE_BACK_MAX_MAD`.
    fn jpeg_mad(jpeg: &[u8], reference: &[u8], w: u32, h: u32) -> f64 {
        let (decoded, _) = decode_jpeg(jpeg, w, h).expect("candidate must decode");
        let actual = decoded.to_rgb8().into_raw();
        assert_eq!(actual.len(), reference.len());
        let sad: u64 = reference
            .iter()
            .zip(&actual)
            .map(|(a, b)| u64::from(a.abs_diff(*b)))
            .sum();
        sad as f64 / reference.len() as f64
    }

    /// HUNT4 item 11 — is the "retry at a higher quality after a MAD decline"
    /// ladder reachable at all?
    ///
    /// Feeds the encoder the worst case a JPEG can be handed: per-channel
    /// independent uniform noise, which has no spatial correlation for the DCT
    /// to exploit. Two facts get pinned down:
    ///
    /// 1. even here the q78 MAD lands *far* below `DECODE_BACK_MAX_MAD`, so no
    ///    real image can trip the guard — the ladder has no reachable trigger;
    /// 2. the ladder's *premise* is still sound: at a threshold between the two
    ///    measured MADs, q78 declines where q90 passes. The mechanism works;
    ///    it simply never fires at the shipped ceiling.
    #[test]
    fn lossy_quality_ladder_has_no_reachable_trigger() {
        let (w, h) = (128u32, 128u32);
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut pixels = Vec::with_capacity((w * h * 3) as usize);
        for _ in 0..w * h * 3 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            pixels.push((state >> 33) as u8);
        }
        let img = |p: &Vec<u8>| {
            DynamicImage::ImageRgb8(image::RgbImage::from_raw(w, h, p.clone()).unwrap())
        };
        let q78 = encode_jpeg(img(&pixels), false, 78).expect("q78 must encode");
        let q90 = encode_jpeg(img(&pixels), false, 90).expect("q90 must encode");

        let mad78 = jpeg_mad(&q78, &pixels, w, h);
        let mad90 = jpeg_mad(&q90, &pixels, w, h);

        eprintln!("HUNT4 item 11: noise q78 MAD {mad78:.2}, q90 MAD {mad90:.2}, ceiling {DECODE_BACK_MAX_MAD}");
        // (1) The shipped ceiling is unreachable: pure noise still passes.
        assert!(
            mad78 < DECODE_BACK_MAX_MAD,
            "adversarial noise must still pass the shipped MAD ceiling \
             (q78 MAD {mad78:.1} vs ceiling {DECODE_BACK_MAX_MAD}) — if this \
             ever fails, the ladder has become reachable and is worth building"
        );
        assert!(
            decode_back_matches(&q78, &pixels, false, w, h, DECODE_BACK_MAX_MAD),
            "the guard itself must agree"
        );

        // (2) The ladder would work if a decline existed: pick a threshold
        // between the two MADs and watch q78 fail where q90 passes.
        assert!(
            mad90 < mad78,
            "higher quality must reduce MAD (q90 {mad90:.1} vs q78 {mad78:.1})"
        );
        let between = f64::midpoint(mad90, mad78);
        assert!(
            !decode_back_matches(&q78, &pixels, false, w, h, between),
            "forced-decline scenario: q78 must fail at MAD {between:.1}"
        );
        assert!(
            decode_back_matches(&q90, &pixels, false, w, h, between),
            "forced-decline scenario: q90 must pass at MAD {between:.1}"
        );
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
    pub(crate) fn line_art_pixels(w: u32, h: u32) -> Vec<u8> {
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
    fn figure_dpi_defaults_off_and_validates_at_the_setter() {
        assert_eq!(OptimizeOptions::default().figure_dpi, None);
        // <= 0 and non-finite mean "off", so the CLI's absent-flag sentinel
        // (0.0) and a fat-fingered value both land on today's behavior.
        for bad in [0.0f32, -1.0, -195.0, f32::NAN, f32::INFINITY] {
            assert_eq!(
                OptimizeOptions::default().with_figure_dpi(bad).figure_dpi,
                None,
                "{bad} must disable the knob"
            );
        }
        assert_eq!(
            OptimizeOptions::default().with_figure_dpi(195.0).figure_dpi,
            Some(195.0)
        );
    }

    #[test]
    fn figure_dpi_promotes_a_chart_to_the_higher_target() {
        // 400 px into 100 pt ⇒ 288 effective DPI, over-resolution at both the
        // 130 default (181 px) and a 195 figure target (271 px).
        let pdf = build_pdf_flate_raw(&chart_pixels(400, 400), 400, 100, 3);
        assert_eq!(
            image_dims(&optimize(&pdf)),
            (181, 181),
            "the flag-off shape"
        );

        let opts = OptimizeOptions::default().with_figure_dpi(195.0);
        assert_eq!(
            image_dims(&optimize_with_options(&pdf, opts)),
            (271, 271),
            "a chart must land at the figure target, not the ordinary one"
        );
    }

    #[test]
    fn figure_dpi_leaves_photographic_content_at_the_ordinary_target() {
        // The false-positive case, which is the one that matters: promoting a
        // photograph spends bytes on content that gains nothing from them.
        let pdf = build_pdf_flate_raw(&photo_pixels(400, 400, 3), 400, 100, 3);
        let opts = OptimizeOptions::default().with_figure_dpi(195.0);
        assert_eq!(
            optimize_with_options(&pdf, opts),
            optimize(&pdf),
            "a photograph must get byte-identical output with the flag on"
        );
    }

    #[test]
    fn figure_dpi_at_or_below_the_ordinary_target_is_inert() {
        // Documented contract: the knob only ever RAISES a target. At or below
        // `target_dpi` it must not fire at all — not even to re-derive the same
        // geometry — so the output stays byte-identical AND the classifier's
        // full-resolution decode is never paid for.
        let pdf = build_pdf_flate_raw(&chart_pixels(400, 400), 400, 100, 3);
        let baseline = optimize(&pdf);
        for dpi in [1.0f32, 72.0, 129.9, 130.0] {
            let opts = OptimizeOptions::default().with_figure_dpi(dpi);
            assert_eq!(
                optimize_with_options(&pdf, opts),
                baseline,
                "--figure-dpi {dpi} is at or below the 130 target and must be inert"
            );
        }
    }

    #[test]
    fn figure_dpi_needs_a_positive_target_dpi() {
        // `target_dpi <= 0` means "do not downsample", and it returns before
        // the promotion is consulted. A figure target cannot revive it.
        let pdf = build_pdf_flate_raw(&chart_pixels(400, 400), 400, 100, 3);
        let opts = OptimizeOptions::default()
            .with_target_dpi(0.0)
            .with_figure_dpi(195.0);
        assert_eq!(
            optimize_with_options(&pdf, opts),
            optimize_with_options(&pdf, OptimizeOptions::default().with_target_dpi(0.0)),
            "--figure-dpi must not downsample when --target-dpi is disabled"
        );
    }

    #[test]
    fn figure_dpi_promotes_a_masked_chart_pair_to_one_geometry() {
        // D-M3 interaction: classification reads the BASE image only, and the
        // /SMask follows the base's target exactly as it does today — both
        // streams land on the SAME promoted geometry, atomically.
        let base = chart_pixels(400, 400);
        let mask = flate_pixels(400, 400, 1);
        let pdf = build_pdf_smask_flate_ext(400, 100, &base, &mask, |_| {}, |_| {});

        let (_, dw, dh, dmask) = smask_base_info(&optimize(&pdf));
        assert_eq!((dw, dh), (181, 181), "the flag-off pair shape");
        assert_eq!(mask_shape(&optimize(&pdf), dmask).1, 181);

        let opts = OptimizeOptions::default().with_figure_dpi(195.0);
        let out = optimize_with_options(&pdf, opts);
        let (_, w, h, mask_id) = smask_base_info(&out);
        assert_eq!((w, h), (271, 271), "the base takes the figure target");
        let (_, mw, mh) = mask_shape(&out, mask_id);
        assert_eq!((mw, mh), (271, 271), "the mask follows its base, as always");
    }

    #[test]
    fn figure_dpi_never_grows_a_stream() {
        // The never-larger contract is unchanged by construction: the higher
        // target only changes the geometry fed to the existing candidates, and
        // the per-stream strictly-smaller guard still decides. A checkerboard
        // is figure-like by the metrics (mid background, dense edges) but
        // deflates to almost nothing at full size and far worse resampled, so
        // BOTH targets are declined and the image is left untouched.
        let raw = checkerboard_pixels(400, 4, 3);
        let m = line_art_metrics(&raw, 3, 400, 400);
        assert!(
            m.background >= FIGURE_MIN_BACKGROUND
                && m.background < FIGURE_MAX_BACKGROUND
                && m.edges >= FIGURE_MIN_EDGES,
            "fixture must be classified as figure-like (bg {:.3} edge {:.4})",
            m.background,
            m.edges
        );
        let pdf = build_pdf_flate_raw(&raw, 400, 100, 3);
        let opts = OptimizeOptions::default().with_figure_dpi(195.0);
        let out = optimize_with_options(&pdf, opts);
        assert_eq!(
            image_dims(&out),
            (400, 400),
            "declined ⇒ untouched geometry"
        );
        assert!(
            out.len() <= pdf.len(),
            "output must never exceed the input ({} -> {})",
            pdf.len(),
            out.len()
        );
    }

    #[test]
    fn figure_dpi_returns_the_original_bytes_on_a_corrupt_over_resolution_jpeg() {
        // The classifier decodes the same bytes the JPEG planner was already going to
        // decode, so it adds no new exposure — but pin the end-to-end contract
        // anyway: with the flag on, a corrupt over-resolution DCT stream still
        // yields the exact input bytes.
        let mut doc = Document::load_mem(&build_pdf(400, 100)).unwrap();
        for obj in doc.objects.values_mut() {
            if let Object::Stream(s) = obj {
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
                    s.set_content(b"\xff\xd8\xff not a real jpeg payload".to_vec());
                }
            }
        }
        let mut input: Vec<u8> = Vec::new();
        doc.save_to(&mut input).unwrap();

        let opts = OptimizeOptions::default().with_figure_dpi(195.0);
        assert_eq!(optimize_with_options(&input, opts), input);
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

    /// The bytes of the cross-reference object a saved file's `startxref`
    /// points at, up to (and excluding) the payload. Panics if the file does
    /// not end in the shape lopdf's writer produces.
    pub(crate) fn xref_object_header(pdf: &[u8]) -> Vec<u8> {
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

    /// A producer's trailer can carry cross-reference-stream keys that describe
    /// the *input's* table (`corpus/adobe-spec.pdf` ends in a classic `trailer`
    /// holding `/DecodeParms<</Columns 5/Predictor 12>>`). lopdf copies the
    /// trailer into the xref stream it synthesizes, so those keys would sit on
    /// top of picamatl's freshly deflated, unpredicted payload and make every
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

    /// Stored length of the (single) image stream in a loaded document.
    fn image_stream_len_of(doc: &Document) -> usize {
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
            .unwrap()
    }

    /// A PDF/A conformance claim disables the pass wholesale (same posture as
    /// font subsetting), so a weakly-deflated stream ships untouched.
    #[test]
    fn redeflate_declines_pdfa_documents() {
        let pdf = build_pdf_weakly_deflated(120, 120);
        let before = image_stream_len(&pdf);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let meta = doc.add_object(Stream::new(
            dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
            b"<x:xmpmeta><pdfaid:part>2</pdfaid:part></x:xmpmeta>".to_vec(),
        ));
        doc.catalog_mut().unwrap().set("Metadata", meta);
        let mut marked: Vec<u8> = Vec::new();
        doc.save_to(&mut marked).unwrap();

        let mut reloaded = Document::load_mem(&marked).unwrap();
        redeflate_flate_streams(&mut reloaded, DeflateBackend::Zlib);
        assert_eq!(
            image_stream_len_of(&reloaded),
            before,
            "PDF/A: stream must be untouched"
        );
    }

    /// A signature is NOT a reason to decline: picamatl re-serializes the whole
    /// document either way, so the `/ByteRange` digest is already broken by
    /// the passes that were never gated. Declining here only cost bytes.
    #[test]
    fn redeflate_runs_on_signed_documents_and_keeps_the_signature_object() {
        let pdf = build_pdf_weakly_deflated(120, 120);
        let before = image_stream_len(&pdf);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let sig = doc.add_object(dictionary! {
            "Type" => "Sig",
            "ByteRange" => vec![0.into(), 0.into(), 0.into(), 0.into()],
        });
        doc.catalog_mut().unwrap().set("Perms", sig);
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            dictionary! { "SigFlags" => 3, "Fields" => Object::Array(vec![]) },
        );
        let mut marked: Vec<u8> = Vec::new();
        doc.save_to(&mut marked).unwrap();

        let mut reloaded = Document::load_mem(&marked).unwrap();
        redeflate_flate_streams(&mut reloaded, DeflateBackend::Zlib);
        assert!(
            image_stream_len_of(&reloaded) < before,
            "signed: the weakly-deflated stream must still be re-deflated"
        );
        // The signature dictionary itself is data like any other: carried
        // through untouched, never rewritten or dropped.
        assert!(
            reloaded.objects.values().any(|o| matches!(
                o,
                Object::Dictionary(d) if matches!(d.get(b"Type"), Ok(Object::Name(n)) if n == b"Sig")
            )),
            "the /Sig object must survive"
        );
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
        // NOTE: build the fixture with picamatl's own encoder. image's JpegEncoder
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

    /// The decoded-payload dedup must key on the dictionary too. Identical
    /// index/sample bytes under a different `/Width`-`/Height` (or a different
    /// `/Indexed` palette) are a DIFFERENT image; merging them silently
    /// transposed and recolored figures in the Adobe PDF spec (page 752's
    /// overprint illustration among them).
    #[test]
    fn decoded_dedup_respects_the_dictionary() {
        // A 6x4 gray raster: the same 24 sample bytes also describe a 4x6 one.
        let plain: Vec<u8> = (0..24u8).collect();
        // Independently deflated bytes for the two copies — the case this pass
        // exists for: same payload, same dict, different /Length and content.
        let deflate_at = |level: u32| {
            use std::io::Write;
            let mut enc =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(level));
            enc.write_all(&plain).unwrap();
            enc.finish().unwrap()
        };
        let mut doc = Document::with_version("1.5");
        let ids: Vec<ObjectId> = [9, 1]
            .into_iter()
            .map(|level| {
                let content = deflate_at(level);
                doc.add_object(Stream::new(
                    dictionary! {
                        "Type" => "XObject", "Subtype" => "Image",
                        "Width" => 6i64, "Height" => 4i64,
                        "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8,
                        "Filter" => "FlateDecode",
                    },
                    content,
                ))
            })
            .collect();
        assert_ne!(
            match doc.get_object(ids[0]) {
                Ok(Object::Stream(s)) => s.content.clone(),
                _ => unreachable!(),
            },
            match doc.get_object(ids[1]) {
                Ok(Object::Stream(s)) => s.content.clone(),
                _ => unreachable!(),
            },
            "the fixture must differ in its STORED bytes, else dedup_streams would do"
        );
        let mut same = doc.clone();
        assert!(
            dedup_decoded_streams(&mut same),
            "same payload + same dict must merge even when the stored bytes differ"
        );

        // Now transpose the second image's dimensions. The decoded bytes are
        // untouched, so a payload-only key would still merge — it must not.
        if let Ok(Object::Stream(s)) = doc.get_object_mut(ids[1]) {
            let w = s.dict.get(b"Width").unwrap().as_i64().unwrap();
            let h = s.dict.get(b"Height").unwrap().as_i64().unwrap();
            s.dict.set("Width", h + 1);
            s.dict.set("Height", w);
        }
        assert!(
            !dedup_decoded_streams(&mut doc),
            "identical payload under a different dictionary must NOT merge"
        );
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
        assert!(
            !d.flatten_forms,
            "form flattening is a semantic change (the output is no longer a \
             form) and must stay opt-in"
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
            .with_allow_lossy_reencode(true)
            .with_flatten_forms(true);
        assert_eq!(o.target_dpi, 96.0);
        assert_eq!(o.jpeg_quality, 60);
        assert_eq!(o.dpi_margin, 1.5);
        assert!(o.strip_accessibility);
        assert!(!o.pack_object_streams);
        assert!(!o.downsample_flate_images);
        assert!(o.subset_fonts);
        assert!(o.recompress_bitonal_images);
        assert!(o.allow_lossy_reencode);
        assert!(o.flatten_forms);
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
        // Default options must NOT strip the structure tree. A fixture WITH a
        // StructTreeRoot is mandatory here: the survival claim ("accessibility
        // tags survive optimization") is only evidenced if the tree existed on
        // input and is verifiably present on output. Mirrors the injection
        // setup of strip_accessibility_runs_even_without_image_work.
        let pdf = build_pdf(400, 100);
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

        let out = optimize_with_options(&reencoded, OptimizeOptions::default());
        assert!(
            out.len() < reencoded.len(),
            "expected shrink from downsampling"
        );
        let out_doc = Document::load_mem(&out).expect("output must load");
        let catalog = out_doc.catalog().expect("catalog present");
        assert!(
            catalog.get(b"StructTreeRoot").is_ok(),
            "defaults must preserve /StructTreeRoot"
        );
        assert!(
            catalog.get(b"MarkInfo").is_ok(),
            "defaults must preserve /MarkInfo"
        );
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

    /// Page-piece dictionaries hold private authoring data no reader renders.
    /// Opt-in, and the flag must take the referenced private stream with it.
    #[test]
    fn strip_private_data_drops_piece_info_and_is_opt_in() {
        let pdf = build_pdf(80, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let page_id = doc.get_pages().values().copied().next().unwrap();
        // Incompressible payload, so its removal cannot be confused with a
        // deflate improvement elsewhere.
        let private: Vec<u8> = (0..8192u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
            .collect();
        let blob = doc.add_object(Stream::new(dictionary! {}, private));
        let piece = dictionary! {
            "Illustrator" => dictionary! {
                "LastModified" => Object::string_literal("D:20260101000000Z"),
                "Private" => Object::Reference(blob),
            },
        };
        if let Ok(Object::Dictionary(page)) = doc.get_object_mut(page_id) {
            page.set("PieceInfo", piece.clone());
        }
        doc.catalog_mut().unwrap().set("PieceInfo", piece);
        let mut reencoded: Vec<u8> = Vec::new();
        doc.save_to(&mut reencoded).unwrap();

        let kept = optimize_with_options(&reencoded, OptimizeOptions::default());
        let kept_doc = Document::load_mem(&kept).expect("default output must load");
        assert!(
            kept_doc.catalog().unwrap().get(b"PieceInfo").is_ok(),
            "default must keep /PieceInfo"
        );

        let opts = OptimizeOptions::default().with_strip_private_data(true);
        let out = optimize_with_options(&reencoded, opts);
        let out_doc = Document::load_mem(&out).expect("stripped output must load");
        for object in out_doc.objects.values() {
            let dict = match object {
                Object::Dictionary(d) => d,
                Object::Stream(s) => &s.dict,
                _ => continue,
            };
            assert!(dict.get(b"PieceInfo").is_err(), "no /PieceInfo may survive");
        }
        // The private payload itself must be gone, not merely unreferenced.
        assert!(
            !out_doc
                .objects
                .values()
                .any(|o| matches!(o, Object::Stream(s) if s.content.len() > 4096)),
            "the orphaned private stream must be pruned"
        );
        assert!(out.len() < kept.len(), "stripping must shrink the output");

        // Page content is untouched: same page count, same content bytes.
        assert_eq!(kept_doc.get_pages().len(), out_doc.get_pages().len());
    }

    #[test]
    fn strip_metadata_drops_xmp_packets_and_is_opt_in() {
        // A catalog-level and a page-level XMP packet, both large enough that
        // removing them must shrink the file.
        let pdf = build_pdf(80, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let packet = vec![b'x'; 4096];
        let page_id = doc.get_pages().values().copied().next().unwrap();
        let meta_a = doc.add_object(Stream::new(
            dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
            packet.clone(),
        ));
        let meta_b = doc.add_object(Stream::new(
            dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
            packet,
        ));
        doc.catalog_mut()
            .unwrap()
            .set("Metadata", Object::Reference(meta_a));
        if let Ok(Object::Dictionary(page)) = doc.get_object_mut(page_id) {
            page.set("Metadata", Object::Reference(meta_b));
        }
        let mut reencoded: Vec<u8> = Vec::new();
        doc.save_to(&mut reencoded).unwrap();

        let kept = optimize_with_options(&reencoded, OptimizeOptions::default());
        let kept_doc = Document::load_mem(&kept).expect("default output must load");
        assert!(
            kept_doc.catalog().unwrap().get(b"Metadata").is_ok(),
            "default must keep XMP"
        );

        let opts = OptimizeOptions::default().with_strip_metadata(true);
        let out = optimize_with_options(&reencoded, opts);
        let out_doc = Document::load_mem(&out).expect("stripped output must load");
        assert!(
            out_doc.catalog().unwrap().get(b"Metadata").is_err(),
            "catalog /Metadata must be removed"
        );
        for object in out_doc.objects.values() {
            if let Object::Dictionary(d) = object {
                assert!(d.get(b"Metadata").is_err(), "no /Metadata may survive");
            }
        }
        assert!(out.len() < kept.len(), "stripping must shrink the output");
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

    /// A pass-through JPEG — one the image path never re-encodes — must still
    /// come out with rebuilt Huffman tables: strictly smaller bytes that decode
    /// to exactly the same pixels. The `image` crate's encoder emits the fixed
    /// T.81 Annex K tables, which is the real-world shape this pass targets.
    #[test]
    fn pass_through_jpeg_gets_optimized_huffman_tables() {
        // Big enough that the entropy saving clears the cost of carrying the
        // rebuilt tables; a smooth ramp keeps the coefficient alphabet small,
        // so the flat Annex K tables are a long way from this image's own
        // statistics.
        let mut img = image::RgbImage::new(256, 256);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            *pixel = image::Rgb([x as u8, y as u8, 128]);
        }
        let mut jpeg: Vec<u8> = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode_image(&img)
            .unwrap();

        let mut doc = Document::with_version("1.5");
        // Never painted by the content stream, so no planner touches it; the
        // only pass that can change these bytes is the Huffman re-optimization.
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 256_i64,
                "Height" => 256_i64,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8_i64,
                "Filter" => "DCTDecode",
            },
            jpeg.clone(),
        ));
        let content = Content {
            operations: vec![Operation::new("q", vec![]), Operation::new("Q", vec![])],
        };
        let c1 = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => c1,
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
        let mut pdf: Vec<u8> = Vec::new();
        doc.save_to(&mut pdf).unwrap();

        let out = optimize(&pdf);
        let doc = Document::load_mem(&out).unwrap();
        let payload = doc
            .objects
            .values()
            .find_map(|o| match o {
                Object::Stream(s)
                    if matches!(s.dict.get(b"Filter"), Ok(Object::Name(n)) if n == b"DCTDecode") =>
                {
                    Some(s.content.clone())
                }
                _ => None,
            })
            .expect("the JPEG must still be there");
        assert!(
            payload.len() < jpeg.len(),
            "Huffman re-optimization must shrink the pass-through JPEG: {} vs {}",
            payload.len(),
            jpeg.len()
        );
        let before = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).unwrap();
        let after =
            image::load_from_memory_with_format(&payload, image::ImageFormat::Jpeg).unwrap();
        assert_eq!(
            before.to_rgb8().into_raw(),
            after.to_rgb8().into_raw(),
            "the rebuild must be pixel-identical"
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
    /// set PICAMATL_TEST_PDF to run against another PDF instead. Uses a typical
    /// size-focused configuration (strip the accessibility tree). Asserts the
    /// output is smaller and remains a valid, loadable PDF.
    #[test]
    fn real_file_shrinks_when_present() {
        let path = std::env::var("PICAMATL_TEST_PDF").unwrap_or_else(|_| {
            concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/sample.pdf").to_string()
        });
        let input = std::fs::read(&path).expect("failed to read real-file test input");
        // Opt in to object-stream packing for this run via PICAMATL_TEST_PACK=1.
        let opts = OptimizeOptions::default()
            .with_strip_accessibility(true)
            .with_pack_object_streams(std::env::var("PICAMATL_TEST_PACK").is_ok());
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
        if let Ok(dest) = std::env::var("PICAMATL_TEST_OUT") {
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
        // The `image` crate ships the fixed T.81 Annex K Huffman tables, which
        // the (lossless, unconditional) re-optimization pass would rebuild.
        // Fixture JPEGs come in with optimal tables already so the skip tests
        // below can still assert exact byte-for-byte passthrough.
        let jpeg = jpeghuff::optimize(&jpeg).unwrap_or(jpeg);
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

    /// Routing is decided from the frame header alone, and it must not have
    /// moved for the gray/RGB fixtures the untouched pipeline owns.
    #[test]
    fn jpeg_route_reads_the_frame_header() {
        assert_eq!(jpeg_route(&jpeg_fixture("cmyk_plain.jpg")), JpegRoute::Cmyk);
        assert_eq!(jpeg_route(&jpeg_fixture("cmyk_ycck.jpg")), JpegRoute::Cmyk);
        assert_eq!(jpeg_route(&jpeg_fixture("cmyk_large.jpg")), JpegRoute::Cmyk);
        for rgb in ["seq_color.jpg", "prog_color.jpg", "prog_color444.jpg"] {
            assert_eq!(jpeg_component_count(&jpeg_fixture(rgb)), Some(3), "{rgb}");
            assert_eq!(
                jpeg_route(&jpeg_fixture(rgb)),
                JpegRoute::GrayOrRgb,
                "{rgb}"
            );
        }
        assert_eq!(
            jpeg_component_count(&jpeg_fixture("prog_gray.jpg")),
            Some(1)
        );
        assert_eq!(
            jpeg_route(&jpeg_fixture("prog_gray.jpg")),
            JpegRoute::GrayOrRgb
        );
        // Garbage never routes to CMYK, and an unparseable header keeps the
        // historical gray/RGB behaviour rather than declining outright.
        assert_eq!(jpeg_component_count(b"not a jpeg"), None);
        assert_eq!(jpeg_route(b"not a jpeg"), JpegRoute::GrayOrRgb);
    }

    /// The declared-component-count reader, over the colour-space shapes a
    /// four-component JPEG realistically appears under.
    #[test]
    fn colorspace_component_counts() {
        let doc = Document::with_version("1.5");
        let count = |cs: Object| {
            let mut d = lopdf::Dictionary::new();
            d.set("ColorSpace", cs);
            colorspace_component_count(&doc, &d)
        };
        assert_eq!(count(Object::Name(b"DeviceCMYK".to_vec())), Some(4));
        assert_eq!(count(Object::Name(b"DeviceRGB".to_vec())), Some(3));
        assert_eq!(count(Object::Name(b"DeviceGray".to_vec())), Some(1));
        assert_eq!(count(Object::Name(b"Pattern".to_vec())), None);
        assert_eq!(
            count(Object::Array(vec![
                Object::Name(b"DeviceN".to_vec()),
                Object::Array(vec![
                    Object::Name(b"C".to_vec()),
                    Object::Name(b"M".to_vec()),
                    Object::Name(b"Y".to_vec()),
                    Object::Name(b"K".to_vec()),
                ]),
            ])),
            Some(4)
        );
        assert_eq!(
            count(Object::Array(vec![
                Object::Name(b"Separation".to_vec()),
                Object::Name(b"Spot".to_vec()),
            ])),
            Some(1)
        );
        // An ICC profile stream's own /N is authoritative.
        let mut doc2 = Document::with_version("1.5");
        let icc = doc2.add_object(Stream::new(dictionary! { "N" => 4_i64 }, vec![0u8; 4]));
        let mut d = lopdf::Dictionary::new();
        d.set(
            "ColorSpace",
            Object::Array(vec![
                Object::Name(b"ICCBased".to_vec()),
                Object::Reference(icc),
            ]),
        );
        assert_eq!(colorspace_component_count(&doc2, &d), Some(4));
        // No /ColorSpace at all: no conclusion either way.
        assert_eq!(
            colorspace_component_count(&doc2, &lopdf::Dictionary::new()),
            None
        );
    }

    // -----------------------------------------------------------------------
    // Decompression-bomb guards.
    //
    // Every decoder this crate reaches for allocates `w * h * channels` from
    // numbers the FILE supplied. A few dozen crafted bytes can therefore ask
    // for gigabytes, and an allocation failure in Rust is an abort — a process
    // kill, not a panic, so the `catch_unwind` that contains every other kind
    // of decoder misbehaviour cannot contain it. These pin the ceilings.
    // -----------------------------------------------------------------------

    /// A JPEG that is a few dozen bytes and declares a `w` x `h` frame: SOI,
    /// an optional Adobe APP14 (which makes libjpeg read four components as
    /// CMYK), SOF0, SOS, a scrap of entropy data, EOI. Structurally valid
    /// enough for a decoder to believe the geometry; nowhere near enough data
    /// to fill it.
    fn bomb_jpeg(w: u16, h: u16, comps: u8, adobe: bool) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        if adobe {
            v.extend_from_slice(&[0xFF, 0xEE, 0x00, 0x0E]);
            v.extend_from_slice(b"Adobe");
            // version(2) flags0(2) flags1(2) transform(1) = the 12-byte
            // payload the declared length above promises.
            v.extend_from_slice(&[0, 100, 0, 0, 0, 0, 0]);
        }
        let ncomp = comps as usize;
        v.extend_from_slice(&[0xFF, 0xC0]);
        v.extend_from_slice(&((8 + 3 * ncomp) as u16).to_be_bytes());
        v.push(8); // sample precision
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.push(comps);
        for c in 0..comps {
            v.extend_from_slice(&[c + 1, 0x11, 0]);
        }
        v.extend_from_slice(&[0xFF, 0xDA]);
        v.extend_from_slice(&((6 + 2 * ncomp) as u16).to_be_bytes());
        v.push(comps);
        for c in 0..comps {
            v.extend_from_slice(&[c + 1, 0x00]);
        }
        v.extend_from_slice(&[0x00, 0x3F, 0x00]);
        v.extend_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    #[test]
    fn jpeg_frame_info_reads_the_declared_geometry() {
        let f = jpeg_frame_info(&bomb_jpeg(60000, 40000, 3, false)).expect("frame parses");
        assert_eq!((f.width, f.height, f.components), (60000, 40000, 3));
        // The component-count wrapper still answers what it always did.
        assert_eq!(jpeg_component_count(&bomb_jpeg(8, 8, 4, true)), Some(4));
        assert_eq!(jpeg_component_count(b"not a jpeg"), None);
    }

    #[test]
    fn the_raster_budget_mirrors_the_flate_ceiling() {
        // Exactly at the ceiling is fine; one byte over is not.
        assert!(raster_within_budget(MAX_FLATE_PIXEL_BYTES, 1, 1));
        assert!(!raster_within_budget(MAX_FLATE_PIXEL_BYTES + 1, 1, 1));
        // An ordinary page image is nowhere near it.
        assert!(raster_within_budget(2000, 2000, 3));
        // 60000 x 60000 RGB is 10 GB.
        assert!(!raster_within_budget(60000, 60000, 3));
        // Overflow declines rather than wrapping into a small number.
        assert!(!raster_within_budget(u64::MAX, u64::MAX, 4));
        // A frame header claiming zero components is priced as four, not free.
        assert!(!jpeg_decode_within_budget(u16::MAX, u16::MAX, 0));
        assert!(jpeg_decode_within_budget(1000, 1000, 3));
    }

    /// The window the full-decode fallback opens: `decode_jpeg_scaled`
    /// declines CMYK cleanly (it will not hand-roll the colour conversion), so
    /// the bytes go on to the general-purpose decoder, which would otherwise
    /// allocate straight from the frame header. This is the path a `/SMask`
    /// stream takes, and the one a crafted PDF would aim at.
    ///
    /// Honest about what this pins: the `image` version in use carries its own
    /// (larger, ~512 MiB) default allocation limit, so today it would refuse
    /// the first case on its own. Ours is the ceiling that does not move when
    /// a dependency's default does — the second case sits between the two, and
    /// only our budget rules it out.
    #[test]
    fn decode_jpeg_declines_a_bomb_before_the_fallback_allocates() {
        for (w, h) in [(60000, 60000), (10000, 9000)] {
            let bomb = bomb_jpeg(w, h, 4, true);
            assert!(
                decode_jpeg_scaled(&bomb, 100, 100).is_none(),
                "{w}x{h}: the CMYK scaled path declines, opening the fallback"
            );
            assert!(
                !jpeg_decode_within_budget(w, h, 4),
                "{w}x{h}: must be priced out of the fallback"
            );
            assert!(
                decode_jpeg(&bomb, 100, 100).is_none(),
                "{w}x{h}: the fallback must decline, not try to hold the frame"
            );
        }
        // The control: the same shape at a size a real page produces is NOT
        // priced out — the guard rules out bombs, not CMYK.
        assert!(jpeg_decode_within_budget(2000, 1500, 4));
    }

    /// End to end: a page whose image is a decompression bomb comes back as
    /// the input bytes. What is really being pinned is that the process is
    /// still alive to return them.
    #[test]
    fn a_decompression_bomb_jpeg_returns_the_input_bytes() {
        for (label, comps, cs) in [("rgb", 3u8, "DeviceRGB"), ("cmyk", 4, "DeviceCMYK")] {
            let mut doc = Document::with_version("1.5");
            let img_id = doc.add_object(Stream::new(
                dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => 60000_i64,
                    "Height" => 60000_i64,
                    "ColorSpace" => cs,
                    "BitsPerComponent" => 8_i64,
                    "Filter" => "DCTDecode",
                },
                bomb_jpeg(60000, 60000, comps, comps == 4),
            ));
            let pdf = wrap_image_pdf(&mut doc, img_id, 72);
            let out = optimize_with_options(&pdf, OptimizeOptions::default());
            assert_eq!(out, pdf, "{label} bomb must leave the file untouched");
        }
    }

    /// A page whose content is split across two streams: the real drawing
    /// (an over-resolution image placement) and a small Flate stream that
    /// inflates past the 128 MiB budget. The placement walk must not inflate
    /// the page without a bound — it skips the page instead, so the image it
    /// draws is never sized off that page and is left alone (the safe
    /// direction), and the run still returns a valid document.
    #[test]
    fn a_page_content_bomb_is_bounded_and_its_images_are_left_alone() {
        use std::io::Write;
        let mut doc = Document::load_mem(&build_pdf(400, 100)).unwrap();
        let page_id = *doc.get_pages().get(&1).unwrap();
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        let chunk = vec![b' '; 1 << 20];
        for _ in 0..(MAX_REDEFLATE_BYTES / chunk.len() + 1) {
            z.write_all(&chunk).unwrap();
        }
        let bomb = z.finish().unwrap();
        assert!(bomb.len() < 1 << 20, "the bomb must be small on disk");
        let bomb_id = doc.add_object(
            Stream::new(dictionary! { "Filter" => "FlateDecode" }, bomb).with_compression(false),
        );
        let page = doc.get_object_mut(page_id).unwrap().as_dict_mut().unwrap();
        let first = page.get(b"Contents").unwrap().clone();
        page.set("Contents", vec![first, Object::Reference(bomb_id)]);
        let mut pdf = Vec::new();
        doc.save_to(&mut pdf).unwrap();

        // Control: without the bomb this exact image IS downsampled, so an
        // untouched image below is the bound at work, not an inert fixture.
        let control = build_pdf(400, 100);
        let control_out = optimize_with_options(&control, OptimizeOptions::default());
        assert!(control_out.len() < control.len(), "control must downsample");

        let out = optimize_with_options(&pdf, OptimizeOptions::default());
        let image_dims = |bytes: &[u8]| -> Vec<(i64, i64)> {
            let d = Document::load_mem(bytes).unwrap();
            d.objects
                .values()
                .filter_map(|o| match o {
                    Object::Stream(s)
                        if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") =>
                    {
                        Some((
                            s.dict.get(b"Width").unwrap().as_i64().unwrap(),
                            s.dict.get(b"Height").unwrap().as_i64().unwrap(),
                        ))
                    }
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            image_dims(&out),
            vec![(400, 400)],
            "the bombed page's image must keep its size"
        );
    }

    /// A bare JPEG2000 codestream (SOC/SIZ/COD/QCD/SOT/SOD/EOC) declaring
    /// `w` x `h` in `comps` 8-bit components. Header-valid, content-free.
    fn bomb_jpx(w: u32, h: u32, comps: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0x4F]; // SOC
        v.extend_from_slice(&[0xFF, 0x51]); // SIZ
        v.extend_from_slice(&(38u16 + 3 * comps).to_be_bytes());
        v.extend_from_slice(&0u16.to_be_bytes()); // Rsiz
        v.extend_from_slice(&w.to_be_bytes()); // Xsiz
        v.extend_from_slice(&h.to_be_bytes()); // Ysiz
        v.extend_from_slice(&0u32.to_be_bytes()); // XOsiz
        v.extend_from_slice(&0u32.to_be_bytes()); // YOsiz
        v.extend_from_slice(&w.to_be_bytes()); // XTsiz (one tile)
        v.extend_from_slice(&h.to_be_bytes()); // YTsiz
        v.extend_from_slice(&0u32.to_be_bytes()); // XTOsiz
        v.extend_from_slice(&0u32.to_be_bytes()); // YTOsiz
        v.extend_from_slice(&comps.to_be_bytes()); // Csiz
        for _ in 0..comps {
            v.extend_from_slice(&[7, 1, 1]); // 8-bit unsigned, no subsampling
        }
        // COD: no precincts, LRCP, 1 layer, 5 decomposition levels, 5/3.
        v.extend_from_slice(&[
            0xFF, 0x52, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x01, 0x00, 0x05, 0x04, 0x04, 0x00, 0x01,
        ]);
        // QCD: 2 guard bits, no quantization, 3 * 5 + 1 subbands.
        v.extend_from_slice(&[0xFF, 0x5C, 0x00, 0x13, 0x40]);
        v.extend_from_slice(&[0x40u8; 16]);
        // SOT / SOD / EOC.
        v.extend_from_slice(&[
            0xFF, 0x90, 0x00, 0x0A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0E, 0x00, 0x01,
        ]);
        v.extend_from_slice(&[0xFF, 0x93, 0x00, 0x00]);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    /// The JPX→JPEG conversion is opt-in, but consent to a lossy re-encode is
    /// not consent to allocate 10 GB from a header. The codestream below is
    /// header-valid — `JpxImage::new` accepts it and reports its geometry —
    /// so nothing before the size check would have turned it away.
    #[test]
    fn an_oversized_jpx_declines_under_allow_lossy() {
        let mut doc = Document::with_version("1.6");
        let payload = bomb_jpx(60000, 60000, 3);
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 60000_i64,
                "Height" => 60000_i64,
                "BitsPerComponent" => 8_i64,
                "Filter" => "JPXDecode",
            },
            payload.clone(),
        ));
        let pdf = wrap_image_pdf(&mut doc, img_id, 72);

        let options = OptimizeOptions {
            allow_lossy_reencode: true,
            ..OptimizeOptions::default()
        };
        let doc = Document::load_mem(&pdf).unwrap();
        assert!(
            plan_jpx_conversions(&doc, options).is_empty(),
            "a 10 GB raster must be priced out before decode()"
        );

        // And end to end: the payload survives the run byte for byte.
        let out = optimize_with_options(&pdf, options);
        let survived = Document::load_mem(&out).unwrap().objects.values().any(|o| {
            matches!(o, Object::Stream(s)
                if matches!(s.dict.get(b"Filter"), Ok(Object::Name(n)) if n == b"JPXDecode")
                    && s.content == payload)
        });
        assert!(survived, "the JPX stream must come through untouched");
    }

    /// The `/SMask` invariant `plan_replacement` relies on: a mask that fails
    /// eligibility takes the WHOLE pair out of consideration, so the code
    /// downstream never sees `smask_present` without an id. Pinned here
    /// because the two live sixty lines apart.
    #[test]
    fn an_ineligible_smask_takes_the_whole_pair_out() {
        type Mutate<'a> = &'a dyn Fn(&mut lopdf::Dictionary);
        let mutations: [(&str, Mutate); 4] = [
            // A stencil mask, not a soft mask.
            ("ImageMask", &|d| d.set("ImageMask", Object::Boolean(true))),
            // Not 8-bit gray samples.
            ("BitsPerComponent 1", &|d| {
                d.set("BitsPerComponent", Object::Integer(1))
            }),
            ("ColorSpace DeviceRGB", &|d| {
                d.set("ColorSpace", Object::Name(b"DeviceRGB".to_vec()))
            }),
            // A /Decode array remaps the mask's samples.
            ("Decode array", &|d| {
                d.set("Decode", vec![Object::Integer(1), Object::Integer(0)])
            }),
        ];
        for (label, mutate) in mutations {
            let pdf = build_pdf_smask_ext(600, 72, 92, |_| {}, mutate);
            let doc = Document::load_mem(&pdf).unwrap();
            for (&id, obj) in doc.objects.iter() {
                let Object::Stream(s) = obj else { continue };
                if s.dict.get(b"SMask").is_err() {
                    continue;
                }
                assert!(
                    plan_replacement(&doc, id, (72.0, 72.0), OptimizeOptions::default()).is_none(),
                    "{label}: an ineligible mask must decline the pair"
                );
            }
            let out = optimize_with_options(&pdf, OptimizeOptions::default());
            assert_eq!(out, pdf, "{label}: the file must be untouched");
        }
    }

    /// Rogue placement geometry declines instead of collapsing to a 1px
    /// target (NaN) or saturating to a 4-billion-pixel one (+inf).
    #[test]
    fn non_finite_placement_geometry_declines() {
        let pdf = build_pdf(600, 72);
        let doc = Document::load_mem(&pdf).unwrap();
        let id = doc
            .objects
            .iter()
            .find(|(_, o)| {
                matches!(o, Object::Stream(s)
                    if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image"))
            })
            .map(|(&id, _)| id)
            .expect("the fixture has one image");

        // The control: an ordinary placement of this fixture is planned.
        assert!(
            plan_replacement(&doc, id, (72.0, 72.0), OptimizeOptions::default()).is_some(),
            "control: a sane placement is still planned"
        );
        for (label, rendered) in [
            ("NaN", (f32::NAN, 72.0)),
            ("+inf", (f32::INFINITY, 72.0)),
            ("-inf", (f32::NEG_INFINITY, 72.0)),
            ("NaN height", (72.0, f32::NAN)),
        ] {
            assert!(
                plan_replacement(&doc, id, rendered, OptimizeOptions::default()).is_none(),
                "{label} placement must decline"
            );
        }
        // A non-finite target DPI is the same story from the other side.
        for dpi in [f32::NAN, f32::INFINITY] {
            let options = OptimizeOptions {
                target_dpi: dpi,
                ..OptimizeOptions::default()
            };
            assert!(plan_replacement(&doc, id, (72.0, 72.0), options).is_none());
        }
    }
}
