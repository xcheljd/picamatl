//! Phase 7 spike tooling (not shipped): assemble labeled side-by-side
//! composite PNGs from page renders, using the `image` crate (no external
//! image tools needed).
//!
//! Usage:
//!   cargo run --release --example spike_compose -- labels <out.pdf>
//!       # a one-page-per-label PDF; render with pdftoppm to get label strips
//!   cargo run --release --example spike_compose -- compose <out.png> \
//!       <label.png>:<in.png>:<WxH+X+Y>:<scale> [...more panels]

use image::{DynamicImage, GenericImage, GenericImageView, RgbImage};

const LABELS: &[&str] = &["ORIGINAL", "FLAG OFF", "FLAG ON (q78)", "FLAG ON (q85)"];

fn labels(out: &str) {
    use lopdf::content::{Content, Operation};
    use lopdf::{dictionary, Document, Object, Stream};
    let mut doc = Document::with_version("1.5");
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica-Bold",
    });
    let pages_id = doc.new_object_id();
    let mut kids: Vec<Object> = Vec::new();
    for text in LABELS {
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec![Object::Name(b"F1".to_vec()), 14.into()]),
                Operation::new("Td", vec![6.into(), 7.into()]),
                Operation::new(
                    "Tj",
                    vec![Object::String(
                        text.as_bytes().to_vec(),
                        lopdf::StringFormat::Literal,
                    )],
                ),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 160.into(), 26.into()],
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font_id } },
        });
        kids.push(page_id.into());
    }
    let count = kids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => count,
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    doc.save(out).unwrap();
    println!("wrote {out}; render with: pdftoppm -r 150 -png {out} <prefix>");
}

/// "WxH+X+Y" -> (w, h, x, y)
fn parse_crop(s: &str) -> (u32, u32, u32, u32) {
    let (wh, xy) = s.split_once('+').expect("crop format WxH+X+Y");
    let (w, h) = wh.split_once('x').expect("crop format WxH+X+Y");
    let (x, y) = xy.split_once('+').expect("crop format WxH+X+Y");
    (
        w.parse().unwrap(),
        h.parse().unwrap(),
        x.parse().unwrap(),
        y.parse().unwrap(),
    )
}

/// Nearest-neighbor integer upscale — no resample smoothing, so JPEG
/// artifacts in the source render stay honestly visible.
fn upscale(img: &RgbImage, factor: u32) -> RgbImage {
    let (w, h) = img.dimensions();
    RgbImage::from_fn(w * factor, h * factor, |x, y| {
        *img.get_pixel(x / factor, y / factor)
    })
}

const GUTTER: u32 = 6;
const LABEL_H: u32 = 55;

fn compose(out: &str, panels: &[String]) {
    struct Panel {
        label: RgbImage,
        body: RgbImage,
    }
    let panels: Vec<Panel> = panels
        .iter()
        .map(|spec| {
            let parts: Vec<&str> = spec.split(':').collect();
            let [label_path, in_path, crop, scale] = parts[..] else {
                panic!("panel format label.png:in.png:WxH+X+Y:scale, got {spec}");
            };
            let label = image::open(label_path).expect("label png").into_rgb8();
            let (w, h, x, y) = parse_crop(crop);
            let scale: u32 = scale.parse().unwrap();
            let src = image::open(in_path).expect("input png");
            let body = DynamicImage::ImageRgba8(src.view(x, y, w, h).to_image()).into_rgb8();
            Panel {
                label,
                body: upscale(&body, scale),
            }
        })
        .collect();

    let total_w: u32 =
        panels.iter().map(|p| p.body.width()).sum::<u32>() + GUTTER * (panels.len() as u32 + 1);
    let body_h = panels.iter().map(|p| p.body.height()).max().unwrap();
    let total_h = LABEL_H + body_h + GUTTER * 2;
    let mut canvas = RgbImage::from_pixel(total_w, total_h, image::Rgb([220, 220, 220]));

    let mut cx = GUTTER;
    for p in &panels {
        // Label strip: copied at native size, clipped to the panel width.
        let lw = p.label.width().min(p.body.width());
        let lh = p.label.height().min(LABEL_H);
        canvas
            .copy_from(&p.label.view(0, 0, lw, lh).to_image(), cx, GUTTER)
            .unwrap();
        canvas.copy_from(&p.body, cx, LABEL_H + GUTTER).unwrap();
        cx += p.body.width() + GUTTER;
    }
    canvas.save(out).unwrap();
    println!("wrote {out}");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("labels") => labels(&args[1]),
        Some("compose") => compose(&args[1], &args[2..]),
        _ => eprintln!("usage: spike_compose labels <out.pdf> | compose <out.png> <panel...>"),
    }
}
