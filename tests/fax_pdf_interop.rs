//! B-M1 pre-work probe C: prove a `fax`-encoded G4 stream survives a real PDF
//! embedding — written as a `/CCITTFaxDecode` image XObject via lopdf, read
//! back through lopdf (raw stream bytes; lopdf does not decode CCITT) and
//! decoded with `fax`, then rendered by Ghostscript (a foreign decoder) and
//! compared pixel-for-pixel against the source bitmap.
//!
//! The Ghostscript half is skipped with a message if `gs` is not installed —
//! the lopdf round-trip half always runs.

use std::path::PathBuf;
use std::process::Command;

use fax::decoder::{decode_g4, pels};
use fax::encoder::Encoder;
use fax::{Color, VecWriter};
use lopdf::{dictionary, Document, Object, Stream};

// Deliberately non-byte-aligned width and odd height: exercises the packed-row
// edges in the PBM comparison and proves G4's bit-continuous rows need no
// byte padding inside the PDF stream.
const W: usize = 203;
const H: usize = 131;

/// Deterministic bitonal test card: border frame, two rules, a diagonal, and
/// text-like block bands. `true` = black.
fn test_card() -> Vec<Vec<bool>> {
    let mut rows = vec![vec![false; W]; H];
    rows[0].fill(true);
    rows[H - 1].fill(true);
    for row in rows.iter_mut() {
        row[0] = true;
        row[W - 1] = true;
    }
    rows[H / 3][10..W - 10].fill(true);
    rows[2 * H / 3][10..W - 10].fill(true);
    for (i, row) in rows.iter_mut().enumerate().take(H.min(W)) {
        row[i] = true;
    }
    // Text-like bands: short black blocks with gaps.
    for band in 0..4 {
        let y0 = 8 + band * 30;
        for dy in 0..5 {
            let mut x = 6 + band * 3;
            while x + 4 < W - 6 {
                for dx in 0..3 {
                    if y0 + dy < H {
                        rows[y0 + dy][x + dx] = true;
                    }
                }
                x += 7 + (band + dy) % 5;
            }
        }
    }
    rows
}

fn g4_encode(rows: &[Vec<bool>]) -> Vec<u8> {
    let mut enc = Encoder::new(VecWriter::new());
    for row in rows {
        enc.encode_line(
            row.iter()
                .map(|&b| if b { Color::Black } else { Color::White }),
            W as u32,
        )
        .unwrap();
    }
    enc.finish().unwrap().finish()
}

/// Build a one-page PDF whose page is exactly W×H points, fully covered by the
/// G4-encoded image, so a 72-dpi render maps image pixels 1:1 to device pixels.
fn build_pdf(g4: Vec<u8>) -> Document {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    let image = Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => W as i64,
            "Height" => H as i64,
            "ColorSpace" => "DeviceGray",
            "BitsPerComponent" => 1,
            "Filter" => "CCITTFaxDecode",
            // K -1 = pure G4; BlackIs1 false (the default) = decoded black
            // pels become 0, which is black in DeviceGray.
            "DecodeParms" => dictionary! {
                "K" => -1,
                "Columns" => W as i64,
                "Rows" => H as i64,
                "BlackIs1" => false,
            },
        },
        g4,
    );
    let image_id = doc.add_object(image);

    let content = format!("q {W} 0 0 {H} 0 0 cm /Im0 Do Q");
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));

    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), (W as i64).into(), (H as i64).into()],
        "Resources" => dictionary! {
            "XObject" => dictionary! { "Im0" => image_id },
        },
        "Contents" => content_id,
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
    doc
}

/// Parse a raw (P4) PBM: returns rows of `true` = black (PBM 1 = black).
fn parse_pbm(data: &[u8]) -> Option<Vec<Vec<bool>>> {
    // Header: "P4" <ws> width <ws> height <single ws> packed rows.
    fn token(data: &[u8], mut i: usize) -> (usize, usize) {
        while i < data.len() && data[i].is_ascii_whitespace() {
            i += 1;
        }
        if data.get(i) == Some(&b'#') {
            while i < data.len() && data[i] != b'\n' {
                i += 1;
            }
            while i < data.len() && data[i].is_ascii_whitespace() {
                i += 1;
            }
        }
        let start = i;
        while i < data.len() && !data[i].is_ascii_whitespace() {
            i += 1;
        }
        (start, i)
    }
    let (s, e) = token(data, 0);
    if &data[s..e] != b"P4" {
        return None;
    }
    let (s, e) = token(data, e);
    let width: usize = std::str::from_utf8(&data[s..e]).ok()?.parse().ok()?;
    let (s, e) = token(data, e);
    let height: usize = std::str::from_utf8(&data[s..e]).ok()?.parse().ok()?;
    // Exactly one whitespace byte separates the header from the raster.
    let raster = &data[e + 1..];
    let stride = width.div_ceil(8);
    if raster.len() < stride * height {
        return None;
    }
    Some(
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| raster[y * stride + x / 8] >> (7 - x % 8) & 1 == 1)
                    .collect()
            })
            .collect(),
    )
}

#[test]
fn g4_pdf_embed_lopdf_round_trip_and_gs_render() {
    let pattern = test_card();
    let g4 = g4_encode(&pattern);
    let doc = build_pdf(g4.clone());

    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let pdf_path = tmp.join("fax_interop.pdf");
    let mut doc = doc;
    doc.save(&pdf_path).expect("save PDF");

    // --- Half 1: lopdf round-trip. lopdf must hand back the CCITT stream
    // bytes untouched, and fax must decode them to the original pixels.
    let reloaded = Document::load(&pdf_path).expect("reload PDF");
    let image = reloaded
        .objects
        .values()
        .find_map(|obj| {
            let stream = obj.as_stream().ok()?;
            let dict = &stream.dict;
            (dict.get(b"Subtype").ok()?.as_name().ok()? == b"Image").then(|| stream.clone())
        })
        .expect("image XObject present after reload");
    assert_eq!(
        image.dict.get(b"Filter").unwrap().as_name().unwrap(),
        b"CCITTFaxDecode"
    );
    assert_eq!(
        image.content, g4,
        "lopdf must pass CCITT stream bytes through untouched"
    );
    let mut rows: Vec<Vec<bool>> = Vec::new();
    decode_g4(
        image.content.iter().cloned(),
        W as u32,
        Some(H as u32),
        |transitions| {
            rows.push(
                pels(transitions, W as u32)
                    .map(|c| c == Color::Black)
                    .collect(),
            );
        },
    )
    .expect("fax decodes the PDF-embedded stream");
    assert_eq!(rows, pattern, "pixels differ after PDF embed round-trip");

    // --- Half 2: foreign-decoder render. Ghostscript rasterizes the page at
    // 72 dpi (1:1 pixel mapping given the W×H-point MediaBox) to bitonal PBM.
    let pbm_path = tmp.join("fax_interop.pbm");
    let gs = Command::new("gs")
        .args([
            "-sDEVICE=pbmraw",
            "-dNOPAUSE",
            "-dBATCH",
            "-dQUIET",
            "-r72",
            "-dGraphicsAlphaBits=1",
            "-dTextAlphaBits=1",
        ])
        .arg(format!("-sOutputFile={}", pbm_path.display()))
        .arg(&pdf_path)
        .output();
    let gs = match gs {
        Err(e) => {
            eprintln!("SKIPPED gs render half: ghostscript not runnable ({e})");
            return;
        }
        Ok(out) => out,
    };
    assert!(
        gs.status.success(),
        "gs failed: {}",
        String::from_utf8_lossy(&gs.stderr)
    );
    let pbm = std::fs::read(&pbm_path).expect("read gs output");
    let rendered = parse_pbm(&pbm).expect("parse P4 PBM");
    assert_eq!(rendered.len(), H, "rendered height");
    assert_eq!(rendered[0].len(), W, "rendered width");
    let mismatches: usize = rendered
        .iter()
        .zip(&pattern)
        .map(|(a, b)| a.iter().zip(b).filter(|(x, y)| x != y).count())
        .sum();
    assert_eq!(
        mismatches,
        0,
        "gs-rendered pixels differ from source bitmap ({mismatches} of {})",
        W * H
    );
}
