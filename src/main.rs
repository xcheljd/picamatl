//! The `amatl` CLI: a thin binary over [`amatl::optimize_with_options`].
//!
//! Every optimization default is taken from [`OptimizeOptions::default()`] at
//! runtime — the CLI never hardcodes a tunable value that could drift from
//! the library. Flags map 1:1 to the `with_*` builder setters; boolean
//! options come as `--<flag>` / `--no-<flag>` pairs so either direction of a
//! future library-default flip stays expressible.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use amatl::{DeflateBackend, OptimizeOptions};
use clap::Parser;

/// How far into the file to look for `%PDF-`. The PDF spec's implementation
/// notes allow junk before the header; 1024 bytes is the tolerance most
/// readers apply.
const HEADER_SCAN_LIMIT: usize = 1024;

/// Rendered under `--help` so the boolean defaults are always the library's
/// current ones, never a doc string that could go stale.
fn defaults_blurb() -> String {
    fn on_off(v: bool) -> &'static str {
        if v {
            "on"
        } else {
            "off"
        }
    }
    let d = OptimizeOptions::default();
    format!(
        "Boolean defaults (from OptimizeOptions::default()):\n  \
         strip-accessibility {}, strip-metadata {},\n  \
         strip-private-data {},\n  \
         pack-object-streams {},\n  \
         downsample-flate-images {}, subset-fonts {},\n  \
         convert-type1 {}, strip-hinting {}, recompress-bitonal-images {},\n  \
         allow-lossy {}, collapse-gray-images {},\n  \
         flatten-forms {}",
        on_off(d.strip_accessibility),
        on_off(d.strip_metadata),
        on_off(d.strip_private_data),
        on_off(d.pack_object_streams),
        on_off(d.downsample_flate_images),
        on_off(d.subset_fonts),
        on_off(d.convert_type1),
        on_off(d.strip_hinting),
        on_off(d.recompress_bitonal_images),
        on_off(d.allow_lossy_reencode),
        on_off(d.collapse_gray_images),
        on_off(d.flatten_forms),
    )
}

#[derive(Parser)]
#[command(
    name = "amatl",
    version,
    about = "Pure-Rust PDF size optimizer: CTM-aware effective-DPI downsampling \
             with a hard fail-safe contract (never larger, never corrupt)",
    after_help = defaults_blurb(),
)]
struct Cli {
    /// Input PDF file
    input: PathBuf,

    /// Output path [default: <input>.optimized.pdf]
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,

    /// With no --output, overwrite the input file in place
    #[arg(long)]
    force: bool,

    /// Target resolution (DPI) for over-resolution images; <= 0 disables
    /// downsampling
    #[arg(long, value_name = "DPI",
          default_value_t = OptimizeOptions::default().target_dpi)]
    target_dpi: f32,

    /// JPEG quality (1-100) for re-encoded images
    #[arg(long, value_name = "Q",
          default_value_t = OptimizeOptions::default().jpeg_quality,
          value_parser = clap::value_parser!(u8).range(1..=100))]
    jpeg_quality: u8,

    /// Only downsample when effective DPI exceeds target-dpi by this factor
    /// (minimum 1.0)
    #[arg(long, value_name = "FACTOR",
          default_value_t = OptimizeOptions::default().dpi_margin)]
    dpi_margin: f32,

    /// Higher target DPI for chart/diagram images with rendered text; requires
    /// a positive --target-dpi and only acts above it; <= 0 disables
    #[arg(long, value_name = "DPI", default_value_t = 0.0)]
    figure_dpi: f32,

    /// Strip the accessibility structure tree (accessibility-lossy)
    #[arg(long, overrides_with = "no_strip_accessibility")]
    strip_accessibility: bool,
    /// Keep the accessibility structure tree
    #[arg(long)]
    no_strip_accessibility: bool,

    /// Strip every /Metadata (XMP) packet; breaks PDF/A and PDF/UA
    /// identification
    #[arg(long, overrides_with = "no_strip_metadata")]
    strip_metadata: bool,
    /// Keep /Metadata (XMP) packets
    #[arg(long)]
    no_strip_metadata: bool,

    /// Strip every /PieceInfo (private authoring-application data); costs
    /// round-trip editability in the producing application
    #[arg(long, overrides_with = "no_strip_private_data")]
    strip_private_data: bool,
    /// Keep /PieceInfo private application data
    #[arg(long)]
    no_strip_private_data: bool,

    /// Pack eligible objects into PDF 1.5 object streams
    #[arg(long, overrides_with = "no_pack_object_streams")]
    pack_object_streams: bool,
    /// Do not pack objects into PDF 1.5 object streams
    #[arg(long)]
    no_pack_object_streams: bool,

    /// Downsample over-resolution FlateDecode raster images in place
    #[arg(long, overrides_with = "no_downsample_flate_images")]
    downsample_flate_images: bool,
    /// Leave FlateDecode raster images untouched
    #[arg(long)]
    no_downsample_flate_images: bool,

    /// Subset embedded fonts (Type0/CIDFontType2 Identity-H/V and simple
    /// TrueType)
    #[arg(long, overrides_with = "no_subset_fonts")]
    subset_fonts: bool,
    /// Do not subset embedded fonts
    #[arg(long)]
    no_subset_fonts: bool,

    /// Convert embedded Type1 fonts to subsetted Type1C (CFF), swapping
    /// each font only when strictly smaller
    #[arg(long, overrides_with = "no_convert_type1")]
    convert_type1: bool,
    /// Leave embedded Type1 fonts untouched
    #[arg(long)]
    no_convert_type1: bool,

    /// Strip hinting: TrueType instructions from subsetted fonts, and Type2
    /// hints from every Type1C (CFF) program (rasterization-lossy at small
    /// sizes; strictly opt-in)
    #[arg(long, overrides_with = "no_strip_hinting")]
    strip_hinting: bool,
    /// Keep TrueType and Type1C hinting
    #[arg(long)]
    no_strip_hinting: bool,

    /// Losslessly recompress bitonal (1-bit) images to CCITT G4
    #[arg(long, overrides_with = "no_recompress_bitonal_images")]
    recompress_bitonal_images: bool,
    /// Do not recompress bitonal images
    #[arg(long)]
    no_recompress_bitonal_images: bool,

    /// Allow LOSSY re-encoding of lossless FlateDecode images to JPEG
    /// (encoding-class change; strictly opt-in)
    #[arg(long = "allow-lossy", overrides_with = "no_allow_lossy")]
    allow_lossy: bool,
    /// Never re-encode lossless images to JPEG
    #[arg(long = "no-allow-lossy")]
    no_allow_lossy: bool,

    /// Losslessly collapse channel-identical DeviceRGB Flate images to
    /// DeviceGray (rewrites /ColorSpace; strictly opt-in)
    #[arg(long, overrides_with = "no_collapse_gray_images")]
    collapse_gray_images: bool,
    /// Do not collapse channel-identical RGB images to grayscale
    #[arg(long)]
    no_collapse_gray_images: bool,

    /// Flatten interactive forms: paint widget appearances into the page and
    /// remove /AcroForm, the field tree, XFA and every widget annotation.
    /// Declines any document where a field value could not be preserved
    /// (semantic change; strictly opt-in)
    #[arg(long, overrides_with = "no_flatten_forms")]
    flatten_forms: bool,
    /// Leave interactive forms untouched
    #[arg(long)]
    no_flatten_forms: bool,

    /// Deflate backend for the final re-deflate and xref-stream passes:
    /// zopfli is ~30x the CPU for a few percent smaller output
    #[arg(long, value_enum, value_name = "BACKEND")]
    deflate_backend: Option<DeflateBackendArg>,
}

/// CLI mirror of [`amatl::DeflateBackend`] so the library type stays free of
/// clap derives. `None` (flag absent) keeps the library default.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum DeflateBackendArg {
    Zlib,
    Zopfli,
}

impl From<DeflateBackendArg> for DeflateBackend {
    fn from(arg: DeflateBackendArg) -> Self {
        match arg {
            DeflateBackendArg::Zlib => DeflateBackend::Zlib,
            DeflateBackendArg::Zopfli => DeflateBackend::Zopfli,
        }
    }
}

/// Fold a `--<flag>` / `--no-<flag>` pair down to one value. The pair
/// mutually overrides in clap (last occurrence wins), so at most one of
/// `yes` / `no` is set; when neither is, the library default stands.
fn resolve(yes: bool, no: bool, library_default: bool) -> bool {
    match (yes, no) {
        (true, _) => true,
        (_, true) => false,
        (false, false) => library_default,
    }
}

fn options_from(cli: &Cli) -> OptimizeOptions {
    let d = OptimizeOptions::default();
    d.with_target_dpi(cli.target_dpi)
        .with_jpeg_quality(cli.jpeg_quality)
        .with_dpi_margin(cli.dpi_margin)
        .with_figure_dpi(cli.figure_dpi)
        .with_strip_accessibility(resolve(
            cli.strip_accessibility,
            cli.no_strip_accessibility,
            d.strip_accessibility,
        ))
        .with_strip_metadata(resolve(
            cli.strip_metadata,
            cli.no_strip_metadata,
            d.strip_metadata,
        ))
        .with_strip_private_data(resolve(
            cli.strip_private_data,
            cli.no_strip_private_data,
            d.strip_private_data,
        ))
        .with_pack_object_streams(resolve(
            cli.pack_object_streams,
            cli.no_pack_object_streams,
            d.pack_object_streams,
        ))
        .with_downsample_flate_images(resolve(
            cli.downsample_flate_images,
            cli.no_downsample_flate_images,
            d.downsample_flate_images,
        ))
        .with_subset_fonts(resolve(
            cli.subset_fonts,
            cli.no_subset_fonts,
            d.subset_fonts,
        ))
        .with_convert_type1(resolve(
            cli.convert_type1,
            cli.no_convert_type1,
            d.convert_type1,
        ))
        .with_strip_hinting(resolve(
            cli.strip_hinting,
            cli.no_strip_hinting,
            d.strip_hinting,
        ))
        .with_recompress_bitonal_images(resolve(
            cli.recompress_bitonal_images,
            cli.no_recompress_bitonal_images,
            d.recompress_bitonal_images,
        ))
        .with_allow_lossy_reencode(resolve(
            cli.allow_lossy,
            cli.no_allow_lossy,
            d.allow_lossy_reencode,
        ))
        .with_collapse_gray_images(resolve(
            cli.collapse_gray_images,
            cli.no_collapse_gray_images,
            d.collapse_gray_images,
        ))
        .with_flatten_forms(resolve(
            cli.flatten_forms,
            cli.no_flatten_forms,
            d.flatten_forms,
        ))
        .with_deflate_backend(
            cli.deflate_backend
                .map(DeflateBackend::from)
                .unwrap_or(d.deflate_backend),
        )
}

fn run(cli: &Cli) -> Result<(), String> {
    let shown_in = cli.input.display();
    let input = std::fs::read(&cli.input).map_err(|e| format!("cannot read {shown_in}: {e}"))?;

    let head = &input[..input.len().min(HEADER_SCAN_LIMIT)];
    if !head.windows(b"%PDF-".len()).any(|w| w == b"%PDF-") {
        return Err(format!(
            "{shown_in} is not a PDF (no %PDF- header in the first 1 KiB)"
        ));
    }

    let (output_path, defaulted) = match (&cli.output, cli.force) {
        (Some(path), _) => (path.clone(), false),
        (None, true) => (cli.input.clone(), false),
        (None, false) => (cli.input.with_extension("optimized.pdf"), true),
    };
    if output_path == cli.input && !cli.force {
        return Err(format!("refusing to overwrite {shown_in} without --force"));
    }
    let shown_out = output_path.display();
    if defaulted {
        eprintln!(
            "no --output given: writing to {shown_out} \
             (use -o to choose a path, or --force to overwrite the input)"
        );
    }

    // The library is infallible by contract: on any internal error or panic it
    // returns the input bytes unchanged, so the only failures the CLI can see
    // are I/O.
    let started = Instant::now();
    let optimized = amatl::optimize_with_options(&input, options_from(cli));
    let secs = started.elapsed().as_secs_f64();

    std::fs::write(&output_path, &optimized)
        .map_err(|e| format!("cannot write {shown_out}: {e}"))?;

    let (in_len, out_len) = (input.len(), optimized.len());
    let saved = 100.0 * (1.0 - out_len as f64 / in_len as f64);
    eprintln!(
        "{shown_in}: {in_len} bytes -> {out_len} bytes \
         ({saved:.1}% saved) in {secs:.2}s -> {shown_out}"
    );
    if out_len == in_len {
        eprintln!("note: no size reduction possible; wrote a byte-for-byte copy");
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}
