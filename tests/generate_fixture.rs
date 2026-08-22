//! Programmatic generator for `fixtures/sample.pdf`, the committed
//! redistributable input for the `real_file_shrinks_when_present` test and
//! `scripts/bench-vs-gs.sh`. Fully synthetic — no private content — so the
//! fixture can ship in the open-source repo and be regenerated at will:
//!
//! ```sh
//! cargo test --test generate_fixture -- --ignored
//! ```
//!
//! Shape (chosen to exercise both optimizer paths):
//! - Page 1 embeds an 800x800 deterministic-pattern JPEG drawn into a 144pt
//!   box → effective ~400 DPI, well over the 130 DPI x 1.15 margin, so amatl
//!   downsamples and re-encodes it.
//! - Page 2 embeds a 200x200 JPEG drawn into a 150pt box → effective ~96 DPI,
//!   under the target, so amatl must leave it untouched.

use image::codecs::jpeg::JpegEncoder;
use image::RgbImage;
use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, Stream};

/// Encode a deterministic sinusoidal RGB pattern as a baseline JPEG. The
/// pattern has enough spatial detail that the encoded stream carries real
/// image bytes (a flat fill would compress to almost nothing and make the
/// downsampling win unrepresentative).
fn pattern_jpeg(width: u32, height: u32, quality: u8) -> Vec<u8> {
    let img = RgbImage::from_fn(width, height, |x, y| {
        let (fx, fy) = (x as f32, y as f32);
        let r = 127.5 + 127.5 * (fx * 0.11).sin() * (fy * 0.07).cos();
        let g = 127.5 + 127.5 * ((fx + fy) * 0.05).sin();
        let b = 127.5 + 127.5 * (fx * 0.03 - fy * 0.09).cos();
        image::Rgb([r as u8, g as u8, b as u8])
    });
    let mut buf = Vec::new();
    img.write_with_encoder(JpegEncoder::new_with_quality(&mut buf, quality))
        .expect("JPEG encoding of the synthetic pattern failed");
    buf
}

/// Build one Letter-sized page drawing `img_name` into a `box_pt`-wide square
/// box, returning the page's object id.
fn add_page(
    doc: &mut Document,
    pages_id: lopdf::ObjectId,
    img_id: lopdf::ObjectId,
    img_name: &str,
    box_pt: i64,
) -> lopdf::ObjectId {
    let content = Content {
        operations: vec![
            Operation::new("q", vec![]),
            Operation::new(
                "cm",
                vec![
                    box_pt.into(),
                    0.into(),
                    0.into(),
                    box_pt.into(),
                    100.into(),
                    400.into(),
                ],
            ),
            Operation::new("Do", vec![Object::Name(img_name.into())]),
            Operation::new("Q", vec![]),
        ],
    };
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
    doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => content_id,
        "Resources" => dictionary! {
            "XObject" => dictionary! { img_name => img_id },
        },
    })
}

#[test]
#[ignore = "regenerates fixtures/sample.pdf; run explicitly with -- --ignored"]
fn generate_fixture() {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    let jpeg_over = pattern_jpeg(800, 800, 90);
    let img_over = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => 800_i64,
            "Height" => 800_i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
        },
        jpeg_over,
    ));

    let jpeg_under = pattern_jpeg(200, 200, 90);
    let img_under = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => 200_i64,
            "Height" => 200_i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
        },
        jpeg_under,
    ));

    // 800px into 144pt → ~400 DPI (downsampled); 200px into 150pt → ~96 DPI
    // (kept as-is).
    let page1 = add_page(&mut doc, pages_id, img_over, "Im1", 144);
    let page2 = add_page(&mut doc, pages_id, img_under, "Im2", 150);

    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page1.into(), page2.into()],
            "Count" => 2,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);

    let dest = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/sample.pdf");
    std::fs::create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures")).unwrap();
    doc.save(dest).expect("failed to write fixtures/sample.pdf");

    let written = std::fs::metadata(dest).unwrap().len();
    println!("wrote {dest}: {written} bytes");
    assert!(
        written < 1_000_000,
        "fixture must stay well under 1MB, got {written} bytes"
    );
}
