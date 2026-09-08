//! Image content classification: line art, and the `--figure-dpi` figure gate.
//!
//! Both classifiers are heuristics over one histogram pass ([`LineArtMetrics`])
//! computed on DECODED SOURCE pixels. They only ever answer "leave today's
//! behavior in place" or "do the thing the option asked for"; every decode
//! failure fails open. The tuning constants and their calibration corpora are
//! documented on the constants themselves.

use lopdf::Document;

use crate::cmyk::decode_cmyk_jpeg_scaled;
use crate::{decode_flate_image, decode_jpeg, jpeg_route, FilterClass, JpegRoute};

/// Metrics behind the line-art guard, computed in a single pass over decoded
/// 8-bit samples. See [`looks_like_line_art`] for the thresholds and the
/// measured per-class values.
pub(crate) struct LineArtMetrics {
    /// Fraction of pixels sharing the single most common quantized color.
    pub(crate) background: f64,
    /// Fraction of pixels covered by the 8 most common quantized colors.
    pub(crate) palette: f64,
    /// Fraction of pixels whose right or lower neighbor differs by more than
    /// `EDGE_STEP` in any channel.
    pub(crate) edges: f64,
}

/// Channel step that counts as a sharp edge between neighboring samples.
const EDGE_STEP: u8 = 48;

/// Widest buffer [`line_art_metrics`] will histogram. The quantized key packs
/// 5 bits per channel, so four channels — CMYK, the widest thing any decode
/// route in this crate produces — is a 20-bit key, i.e. a 4 MiB `Vec<u32>`
/// that `calloc` hands back as zero pages. Nothing here decodes wider.
const METRICS_MAX_CHANNELS: usize = 4;

/// Compute the [`LineArtMetrics`] of an interleaved 8-bit buffer (`channels`
/// samples per pixel, `w * h * channels` bytes). Colors are quantized to 5 bits
/// per channel before histogramming, so anti-aliasing fringes and mild noise do
/// not shatter a flat region into thousands of distinct "colors".
pub(crate) fn line_art_metrics(pixels: &[u8], channels: usize, w: u32, h: u32) -> LineArtMetrics {
    // A channel count outside the key width has no metrics we can compute, so
    // report the all-zero shape — which fails every gate built on top of this
    // ([`looks_like_line_art`] needs a HIGH background, [`image_is_figure_like`]
    // a mid-band one). Unreachable today (callers pass 1, 3, or 4); it is the
    // "any doubt declines" default the rest of the pipeline uses.
    if channels == 0 || channels > METRICS_MAX_CHANNELS {
        return LineArtMetrics {
            background: 0.0,
            palette: 0.0,
            edges: 0.0,
        };
    }
    let total = (w as usize) * (h as usize);
    // Direct-indexed array rather than a `HashMap`: the quantized key is dense
    // and bounded (5 bits per channel), so the counts are byte-identical at a
    // fraction of the cost. Worth doing because this runs over every decoded
    // pixel of every candidate on the `--allow-lossy` and `--figure-dpi` paths.
    let mut histogram = vec![0u32; 1usize << (5 * channels)];
    let quantized = |i: usize| -> u32 {
        let px = &pixels[i * channels..i * channels + channels];
        px.iter()
            .fold(0u32, |acc, s| (acc << 5) | u32::from(s >> 3))
    };
    let mut edges = 0usize;
    for y in 0..h as usize {
        for x in 0..w as usize {
            let i = y * w as usize + x;
            histogram[quantized(i) as usize] += 1;
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
    // Only the top-1 and top-8 sums are ever needed, so keep a descending
    // 8-slot ladder in one linear scan instead of sorting a table that is
    // mostly zeros (and, for CMYK, a million entries long). Equal counts may
    // land in either order; the SUM of the top n does not care.
    let mut top = [0u32; 8];
    for &count in &histogram {
        if count <= top[7] {
            continue;
        }
        top[7] = count;
        let mut k = 7;
        while k > 0 && top[k] > top[k - 1] {
            top.swap(k, k - 1);
            k -= 1;
        }
    }
    let sum_top = |n: usize| -> u64 { top.iter().take(n).map(|c| u64::from(*c)).sum() };
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
pub(crate) fn looks_like_line_art(pixels: &[u8], channels: usize, w: u32, h: u32) -> bool {
    let m = line_art_metrics(pixels, channels, w, h);
    m.background >= LINE_ART_MIN_BACKGROUND
        && m.palette >= LINE_ART_MIN_PALETTE
        && m.edges <= LINE_ART_MAX_EDGES
}

const LINE_ART_MIN_BACKGROUND: f64 = 0.75;
const LINE_ART_MIN_PALETTE: f64 = 0.90;
const LINE_ART_MAX_EDGES: f64 = 0.08;

/// Lower background bound for the `--figure-dpi` promotion. Below this the
/// image's color mass is spread across gradients — a photograph, a rendered
/// 3D surface, a filled velocity field — and there is no flat paper for
/// rendered text to sit on. Measured on the NASA reference (2026-08-25):
/// photographic objs 100/133/47 land at background ≤ 0.077.
pub(crate) const FIGURE_MIN_BACKGROUND: f64 = 0.25;

/// Upper background bound, deliberately equal to [`LINE_ART_MIN_BACKGROUND`]:
/// at/above 0.75 the image is [`looks_like_line_art`] territory — sparse ink
/// on flat paper, which the resampler already handles well at the ordinary
/// target — so the promotion defers rather than overlapping.
pub(crate) const FIGURE_MAX_BACKGROUND: f64 = 0.75;

/// Minimum fraction of pixels sitting on a sharp transition. Rendered text and
/// plot rules are exactly this: many short high-contrast runs. NASA's largest
/// worst offenders (objs 353/336/343 — charts whose axis labels turned to mush
/// at 130 DPI) sit at background 0.49-0.67 with edges above this line.
pub(crate) const FIGURE_MIN_EDGES: f64 = 0.10;

/// Decode a candidate's SOURCE pixels for [`image_is_figure_like`], at FULL
/// resolution. Returns `(pixels, channels, w, h)` for the interleaved 8-bit
/// buffer, or `None` when the stream cannot be decoded with confidence.
///
/// Full resolution is not negotiable here. A DCT-scaled decode at `n/8` divides
/// the pixel count by `(8/n)²` while preserving nearly every edge, which
/// inflates `LineArtMetrics::edges` several-fold and blows straight through the
/// calibrated [`FIGURE_MIN_EDGES`] gate. Hence `u32::MAX` as the requested
/// geometry: it pins [`crate::dct_scale_numerator`] at 8/8 no matter what the dict
/// claims, so a dict/frame-header disagreement cannot quietly shrink the
/// decode either. The decompression-bomb budgets inside each decoder still
/// apply — they are what bounds this.
///
/// Three routes, because the JPEG decoders are not interchangeable:
/// [`crate::decode_jpeg_scaled`] hard-declines CMYK/YCCK, so four-component payloads
/// go to [`decode_cmyk_jpeg_scaled`] and come back as an interleaved 4-channel
/// buffer that [`line_art_metrics`] handles via its `channels` parameter.
fn figure_metrics_source(
    doc: &Document,
    stream: &lopdf::Stream,
    class: &FilterClass,
    px_w: u32,
    px_h: u32,
) -> Option<(Vec<u8>, usize, u32, u32)> {
    match class {
        FilterClass::DctOnly => match jpeg_route(&stream.content) {
            JpegRoute::Cmyk => {
                let img = decode_cmyk_jpeg_scaled(&stream.content, u32::MAX, u32::MAX)?;
                Some((img.data, 4, img.width, img.height))
            }
            JpegRoute::GrayOrRgb => {
                let (img, is_gray) = decode_jpeg(&stream.content, u32::MAX, u32::MAX)?;
                let (w, h) = (img.width(), img.height());
                let (pixels, channels) = if is_gray {
                    (img.to_luma8().into_raw(), 1)
                } else {
                    (img.to_rgb8().into_raw(), 3)
                };
                Some((pixels, channels, w, h))
            }
            JpegRoute::Decline => None,
        },
        // `decode_flate_image` already guarantees the buffer is exactly
        // `px_w * px_h * channels` bytes, so the dict geometry is the truth here.
        FilterClass::FlateOnly => {
            let (img, channels) = decode_flate_image(doc, stream, px_w, px_h)?;
            let pixels = if channels == 1 {
                img.to_luma8().into_raw()
            } else {
                img.to_rgb8().into_raw()
            };
            Some((pixels, channels, px_w, px_h))
        }
        _ => None,
    }
}

/// The `--figure-dpi` classifier: does this image look like a chart, graph, or
/// diagram with rendered text in it, rather than a photograph?
///
/// Heuristic only — no OCR, no new dependencies, and it reuses the
/// [`line_art_metrics`] pass that already exists for the line-art guard. The
/// signature is "meaningful flat background, but not ONLY background, with a
/// dense scattering of sharp transitions on top": rendered axis labels, tick
/// marks, and plot rules produce many short high-contrast runs, while a
/// photograph's tonal gradients produce almost no flat background at all.
///
/// Calibrated 2026-08-25 against 85 hand-labelled images from the NASA
/// reference and cross-checked on `arxiv-diffusion` / `arxiv-gpt4`. The
/// operating point below is FALSE-POSITIVE-FREE on that labelled set — no
/// photograph is promoted — which is the property that matters: a false
/// positive spends bytes on content that gains nothing, a false negative just
/// leaves today's behavior in place.
///
/// KNOWN LIMITATION, accepted for v1: a chart drawn over a busy colored
/// background (background < [`FIGURE_MIN_BACKGROUND`]) is not promoted. Closing
/// that band needs word boxes, i.e. OCR, which is deliberately out of scope.
///
/// FAIL-OPEN: every decode failure, unsupported filter class, and unreadable
/// payload returns `false`, which means "use the ordinary `--target-dpi`" —
/// exactly today's behavior. Classification never produces an error, and it
/// adds no new exposure to hostile payloads: it decodes the SAME bytes the
/// over-resolution branch below was already going to hand the same decoders,
/// under the same [`crate::optimize_with_options`] `catch_unwind` boundary.
pub(crate) fn image_is_figure_like(
    doc: &Document,
    stream: &lopdf::Stream,
    class: &FilterClass,
    px_w: u32,
    px_h: u32,
) -> bool {
    let Some((pixels, channels, w, h)) = figure_metrics_source(doc, stream, class, px_w, px_h)
    else {
        return false;
    };
    // `line_art_metrics` indexes `w * h * channels` bytes unconditionally; a
    // decoder that returned a shorter buffer than its own reported geometry
    // would panic there. Decline instead.
    if (w as usize)
        .checked_mul(h as usize)
        .and_then(|px| px.checked_mul(channels))
        .is_none_or(|need| pixels.len() < need)
    {
        return false;
    }
    let m = line_art_metrics(&pixels, channels, w, h);
    m.background >= FIGURE_MIN_BACKGROUND
        && m.background < FIGURE_MAX_BACKGROUND
        && m.edges >= FIGURE_MIN_EDGES
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lopdf::{Object, ObjectId, Stream};

    use crate::cmyk::jpeg_frame_info;
    use crate::cmyk::tests::{build_pdf_cmyk, jpeg_fixture};
    use crate::tests::{build_pdf_flate_raw, line_art_pixels, photo_pixels};

    /// Chart pixels: a flat plot panel on flat paper, gridlines, and dense
    /// rendered-text-like ink — the `--figure-dpi` class. Two large flat
    /// fields rather than one keep the dominant-color share in the mid band
    /// (no single color owns ≥ 75% of the image, so this is NOT line art),
    /// while the text rows push the sharp-edge fraction well past
    /// `FIGURE_MIN_EDGES`.
    pub(crate) fn chart_pixels(w: u32, h: u32) -> Vec<u8> {
        let mut buf = vec![255u8; (w * h * 3) as usize];
        let idx = |x: u32, y: u32| ((y * w + x) * 3) as usize;
        // Plot panel over the lower ~45%: a second flat field, far enough from
        // white to land in its own 5-bit quantization bucket. Every flat value
        // here is ≡ 7 (mod 8), i.e. at the TOP of its bucket, so the paper
        // grain applied at the end (≤ 6 counts down) cannot split a field
        // across two buckets and move the background metric.
        let panel_top = h * 55 / 100;
        for y in panel_top..h {
            for x in 0..w {
                buf[idx(x, y)..idx(x, y) + 3].copy_from_slice(&[231, 231, 239]);
            }
        }
        // Gridlines inside the panel.
        for y in panel_top..h {
            for x in 0..w {
                if x % 17 == 0 || y % 13 == 0 {
                    buf[idx(x, y)..idx(x, y) + 3].copy_from_slice(&[151, 151, 159]);
                }
            }
        }
        // Rendered text: bands of 1-2 px high-contrast strokes at irregular
        // spacing, the way axis labels, tick numbers, and a legend read to a
        // per-pixel edge count.
        let mut state = 0x1357_9BDF_u32;
        for band in 0..h / 5 {
            let y0 = band * 5 + 1;
            if y0 + 2 >= h {
                break;
            }
            for x in 0..w {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                if state % 5 < 2 {
                    continue; // inter-glyph and inter-word gaps
                }
                for y in y0..y0 + 2 {
                    buf[idx(x, y)..idx(x, y) + 3].copy_from_slice(&[15, 15, 15]);
                }
            }
        }
        // Faint deterministic paper grain, the same trick `line_art_pixels`
        // uses: ≤ 6 counts below each flat value, so it is invisible to all
        // three metrics (same quantization bucket, far below `EDGE_STEP`) but
        // deflate-hostile. Without it the full-size stream compresses so well
        // that the never-larger guard declines the downsample outright and the
        // fixture proves nothing about targets.
        for (i, byte) in buf.iter_mut().enumerate() {
            let mut x = i as u32 ^ 0x9E37_79B9;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *byte -= (x % 7) as u8;
        }
        buf
    }

    /// The classifier's operating point, stated as metrics rather than as a
    /// bare boolean: charts sit in the mid background band with dense edges,
    /// photographs have no flat background at all, and line art is above the
    /// upper bound where `looks_like_line_art` already owns it.
    #[test]
    fn figure_metrics_separate_charts_from_photos_and_line_art() {
        let chart = line_art_metrics(&chart_pixels(400, 400), 3, 400, 400);
        assert!(
            chart.background >= FIGURE_MIN_BACKGROUND
                && chart.background < FIGURE_MAX_BACKGROUND
                && chart.edges >= FIGURE_MIN_EDGES,
            "chart fixture must sit in the figure band (bg {:.3} edge {:.4})",
            chart.background,
            chart.edges
        );

        let photo = line_art_metrics(&photo_pixels(400, 400, 3), 3, 400, 400);
        assert!(
            photo.background < FIGURE_MIN_BACKGROUND,
            "photographic content must fail the background floor (bg {:.3})",
            photo.background
        );

        // Line art is vetoed by the upper bound, not by the edge floor — the
        // two gates deliberately meet at 0.75 with no overlap and no gap.
        let art = line_art_metrics(&line_art_pixels(400, 400), 3, 400, 400);
        assert!(
            art.background >= FIGURE_MAX_BACKGROUND,
            "line art must be above the figure band, not inside it (bg {:.3})",
            art.background
        );
    }

    /// The array histogram must reproduce the `HashMap` + sort it replaced.
    /// Recomputed here the slow, obvious way on a buffer with a known answer.
    #[test]
    fn line_art_metrics_histogram_matches_the_reference_computation() {
        for (pixels, channels, w, h) in [
            (chart_pixels(64, 64), 3, 64u32, 64u32),
            (photo_pixels(64, 64, 3), 3, 64, 64),
            (photo_pixels(64, 64, 1), 1, 64, 64),
        ] {
            let mut reference: std::collections::HashMap<u32, u32> =
                std::collections::HashMap::new();
            for i in 0..(w as usize * h as usize) {
                let key = pixels[i * channels..i * channels + channels]
                    .iter()
                    .fold(0u32, |acc, s| (acc << 5) | u32::from(s >> 3));
                *reference.entry(key).or_insert(0) += 1;
            }
            let mut counts: Vec<u32> = reference.into_values().collect();
            counts.sort_unstable_by(|a, b| b.cmp(a));
            let total = (w as usize * h as usize) as f64;
            let sum: u64 = counts.iter().take(8).map(|c| u64::from(*c)).sum();

            let m = line_art_metrics(&pixels, channels, w, h);
            assert_eq!(m.background, u64::from(counts[0]) as f64 / total);
            assert_eq!(m.palette, sum as f64 / total);
        }
    }

    /// A four-channel buffer is a real input now (CMYK JPEGs route through
    /// `decode_cmyk_jpeg_scaled`), so the 20-bit key width has to work; and a
    /// width the histogram cannot represent must decline rather than panic.
    #[test]
    fn line_art_metrics_handles_cmyk_width_and_declines_beyond_it() {
        let flat = vec![7u8; 32 * 32 * 4];
        let m = line_art_metrics(&flat, 4, 32, 32);
        assert_eq!(m.background, 1.0, "a flat CMYK field is all background");
        assert_eq!(m.edges, 0.0);

        for channels in [0usize, METRICS_MAX_CHANNELS + 1] {
            let m = line_art_metrics(&flat, channels, 32, 32);
            assert_eq!((m.background, m.palette, m.edges), (0.0, 0.0, 0.0));
            assert!(!looks_like_line_art(&flat, channels, 32, 32));
        }
    }

    /// The single image stream of a single-image PDF, as a `(dict, content)`
    /// pair, for the classifier tests below.
    fn only_image_of(pdf: &[u8]) -> (Document, ObjectId) {
        let doc = Document::load_mem(pdf).unwrap();
        let id = doc
            .objects
            .iter()
            .find(|(_, o)| {
                matches!(o, Object::Stream(s)
                    if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image"))
            })
            .map(|(id, _)| *id)
            .expect("an image stream");
        (doc, id)
    }

    #[test]
    fn figure_dpi_classifies_cmyk_jpegs_through_the_four_channel_route() {
        // `decode_jpeg_scaled` hard-declines CMYK/YCCK, so the classifier has
        // its own route to `decode_cmyk_jpeg_scaled`. Pin that it produces a
        // FULL-RESOLUTION four-channel buffer — the DCT-scaled shortcut would
        // quarter the pixel count while preserving the edges, which is exactly
        // what breaks the calibrated edge gate.
        for name in ["cmyk_plain.jpg", "cmyk_ycck.jpg", "cmyk_large.jpg"] {
            let src = jpeg_fixture(name);
            let (w, h) = {
                let f = jpeg_frame_info(&src).unwrap();
                (u32::from(f.width), u32::from(f.height))
            };
            let pdf = build_pdf_cmyk(src, w, h, 144, 108, None);
            let (doc, id) = only_image_of(&pdf);
            let stream = doc.get_object(id).unwrap().as_stream().unwrap();
            let (pixels, channels, dw, dh) =
                figure_metrics_source(&doc, stream, &FilterClass::DctOnly, w, h)
                    .unwrap_or_else(|| panic!("{name} must decode for classification"));
            assert_eq!(channels, 4, "{name} keeps its four components");
            assert_eq!((dw, dh), (w, h), "{name} must decode at full resolution");
            assert_eq!(pixels.len(), (w as usize) * (h as usize) * 4, "{name}");
            // The gate itself must run without panicking on a 20-bit key.
            let _ = image_is_figure_like(&doc, stream, &FilterClass::DctOnly, w, h);
        }
    }

    #[test]
    fn figure_dpi_classification_is_fail_open_on_undecodable_payloads() {
        // Every decode failure means "not figure-like", i.e. the ordinary
        // target — never an error, never a panic.
        let pdf = build_pdf_flate_raw(&chart_pixels(64, 64), 64, 100, 3);
        let (doc, id) = only_image_of(&pdf);
        let stream = doc.get_object(id).unwrap().as_stream().unwrap();
        // Right stream, wrong class: JPX/JBIG2/CCITT/Other have no route.
        for class in [
            FilterClass::CcittOnly,
            FilterClass::JpxOnly,
            FilterClass::Jbig2Only,
            FilterClass::Other,
        ] {
            assert!(!image_is_figure_like(&doc, stream, &class, 64, 64));
        }
        // Right class, undecodable payload — on both routes.
        let junk = Stream::new(
            stream.dict.clone(),
            b"\xff\xd8\xff not a real jpeg payload".to_vec(),
        );
        assert!(!image_is_figure_like(
            &doc,
            &junk,
            &FilterClass::FlateOnly,
            64,
            64
        ));
        // (The DCT route's malformed-payload behavior is covered end-to-end by
        // `figure_dpi_returns_the_original_bytes_on_a_corrupt_over_resolution_jpeg`
        // instead: mozjpeg PANICS on a malformed stream rather than returning
        // an error, so `decode_jpeg` is only safe to exercise from inside the
        // `optimize_with_options` `catch_unwind` boundary — which is equally
        // true of the `plan_dct` call this classifier sits next to.)
        //
        // Lying geometry: the dict claims a size the payload cannot fill.
        assert!(!image_is_figure_like(
            &doc,
            stream,
            &FilterClass::FlateOnly,
            4096,
            4096
        ));
    }
}
