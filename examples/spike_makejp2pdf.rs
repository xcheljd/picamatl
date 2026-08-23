//! Diagnostic: build a well-formed one-page PDF embedding a JP2/J2C file as a
//! /JPXDecode image XObject. Scratch tool for validating the JPX→JPEG path.
use lopdf::{dictionary, Document, Object, Stream};

fn main() {
    let jp2_path = std::env::args().nth(1).unwrap();
    let out_path = std::env::args().nth(2).unwrap();
    let w: i64 = std::env::args().nth(3).unwrap().parse().unwrap();
    let h: i64 = std::env::args().nth(4).unwrap().parse().unwrap();
    let data = std::fs::read(&jp2_path).unwrap();

    let mut doc = Document::with_version("1.6");
    let img = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => w, "Height" => h,
            "BitsPerComponent" => 8,
            "Filter" => "JPXDecode",
        },
        data,
    );
    let img_id = doc.add_object(img);
    let content = format!("q\n{w} 0 0 {h} 0 0 cm\n/Im0 Do\nQ");
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
    let pages_id = doc.new_object_id();
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), w.into(), h.into()],
        "Contents" => content_id,
        "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => img_id } },
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    doc.save(&out_path).unwrap();
    println!("wrote {out_path}");
}
