//! Render fidelity across the `f32` real-literal restoration (`src/reals.rs`).
//!
//! lopdf holds every PDF real as an `f32`, so a literal like `/MediaBox [0 0
//! 595.91998 841.91998]` — LibreOffice's ordinary A4 — comes back out of a
//! save as `595.92 841.92`. That 2e-5 pt shift moves the page-to-device origin
//! and re-grid-fits every glyph on the page. `src/reals.rs` splices the input's
//! own literals back into the saved bytes; these tests are what proves it
//! reaches the pixels. (The literals themselves are checked in the unit tests
//! in `src/reals.rs`; here they are inside a deflated object stream.)
//!
//! Poppler, not Ghostscript: `gs` rounds the page box to whole device pixels
//! before fitting anything to the grid and is blind to this class of change at
//! any alpha setting. See the note on `render_pages` in `tests/forms_flatten.rs`
//! for the measurement.

use std::process::Command;

use amatl::{optimize_with_options, OptimizeOptions};

/// Every lossy pass off, so a surviving pixel difference can only be geometry.
fn lossless() -> OptimizeOptions {
    OptimizeOptions::default()
        .with_target_dpi(0.0)
        .with_downsample_flate_images(false)
        .with_subset_fonts(false)
}

fn pdftoppm() -> Option<String> {
    Command::new("pdftoppm")
        .arg("-v")
        .output()
        .ok()
        .map(|_| "pdftoppm".to_string())
}

fn render_pages(
    tool: &str,
    pdf: &std::path::Path,
    dir: &std::path::Path,
    dpi: u32,
) -> Vec<Vec<u8>> {
    let status = Command::new(tool)
        .args(["-r", &dpi.to_string(), "-gray"])
        .arg(pdf)
        .arg(dir.join("p"))
        .status()
        .expect("pdftoppm runs");
    assert!(status.success(), "pdftoppm failed on {}", pdf.display());

    let mut pages: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "pgm"))
        .collect();
    pages.sort();
    assert!(
        !pages.is_empty(),
        "pdftoppm wrote no pages for {}",
        pdf.display()
    );

    let rasters = pages.iter().map(|p| std::fs::read(p).unwrap()).collect();
    for page in pages {
        let _ = std::fs::remove_file(page);
    }
    rasters
}

/// Both DPIs: a fractional offset can land on the pixel grid at one scale and
/// off it at another.
fn assert_renders_identically(tool: &str, label: &str, input: &[u8], output: &[u8]) {
    let dir = std::env::temp_dir().join(format!("amatl-reals-{}-{label}", std::process::id()));
    let (before, after) = (dir.join("before"), dir.join("after"));
    std::fs::create_dir_all(&before).unwrap();
    std::fs::create_dir_all(&after).unwrap();
    let (a_pdf, b_pdf) = (dir.join("a.pdf"), dir.join("b.pdf"));
    std::fs::write(&a_pdf, input).unwrap();
    std::fs::write(&b_pdf, output).unwrap();

    for dpi in [100, 150] {
        let a = render_pages(tool, &a_pdf, &before, dpi);
        let b = render_pages(tool, &b_pdf, &after, dpi);
        assert_eq!(a.len(), b.len(), "{label}: page count changed");
        for (n, (x, y)) in a.iter().zip(&b).enumerate() {
            assert_eq!(
                x.len(),
                y.len(),
                "{label}: page {} geometry changed at {dpi} dpi",
                n + 1
            );
            let differing = x.iter().zip(y).filter(|(p, q)| p != q).count();
            assert_eq!(
                differing,
                0,
                "{label}: page {} differs at {dpi} dpi ({differing} of {} subpixels)",
                n + 1,
                x.len()
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The two corpus documents whose `/MediaBox` literals cannot survive an `f32`
/// round trip, and — before `src/reals.rs` — the only two that rendered
/// differently after a lossless run: 27 of 28 pages and 8 of 10.
fn assert_corpus_file_renders_identically(name: &str) {
    let Some(tool) = pdftoppm() else {
        eprintln!("skipping: `pdftoppm` not on PATH");
        return;
    };
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
    let Ok(input) = std::fs::read(&fixture) else {
        eprintln!("skipping: {} not present", fixture.display());
        return;
    };
    let out = optimize_with_options(&input, lossless());
    assert_ne!(out, input, "{name}: the fixture must actually be rewritten");
    assert_renders_identically(&tool, name, &input, &out);
}

/// LibreOffice A4, 28 pages, `/MediaBox [0 0 595.91998 841.91998]`.
#[test]
fn wiki_pdf_renders_identically_after_a_lossless_run() {
    assert_corpus_file_renders_identically("corpus-expanded/wiki-pdf.pdf");
}

/// Same producer, 10 pages, plus CMYK imagery.
#[test]
fn wiki_cmyk_topic_renders_identically_after_a_lossless_run() {
    assert_corpus_file_renders_identically("corpus-expanded/wiki-cmyk-topic.pdf");
}
