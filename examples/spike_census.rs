//! Phase 7 spike tooling (not shipped): stream-level census of a PDF's image
//! XObjects, and a two-file diff mode for idempotence diagnosis.
//!
//! Usage:
//!   cargo run --release --example spike_census -- census a.pdf [b.pdf ...]
//!   cargo run --release --example spike_census -- diff a.pdf b.pdf

use lopdf::{Document, Object};

fn short_colorspace(doc: &Document, obj: Option<&Object>) -> String {
    let resolved = obj.map(|o| match o {
        Object::Reference(id) => doc.get_object(*id).unwrap_or(o),
        other => other,
    });
    match resolved {
        Some(Object::Name(n)) => String::from_utf8_lossy(n).into_owned(),
        Some(Object::Array(items)) => match items.first() {
            Some(Object::Name(n)) => format!("[{}...]", String::from_utf8_lossy(n)),
            _ => "[?]".into(),
        },
        Some(_) => "?".into(),
        None => "-".into(),
    }
}

type ImageRow = (String, String, i64, i64, i64, bool, usize);

fn image_rows(doc: &Document) -> Vec<((u32, u16), ImageRow)> {
    let mut rows = Vec::new();
    for (id, obj) in &doc.objects {
        if let Object::Stream(s) = obj {
            if !matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
                continue;
            }
            let filter = match s.dict.get(b"Filter") {
                Ok(Object::Name(n)) => String::from_utf8_lossy(n).into_owned(),
                Ok(Object::Array(items)) => format!("{items:?}"),
                _ => "-".into(),
            };
            let cs = short_colorspace(doc, s.dict.get(b"ColorSpace").ok());
            let w = s
                .dict
                .get(b"Width")
                .ok()
                .and_then(|o| o.as_i64().ok())
                .unwrap_or(-1);
            let h = s
                .dict
                .get(b"Height")
                .ok()
                .and_then(|o| o.as_i64().ok())
                .unwrap_or(-1);
            let bpc = s
                .dict
                .get(b"BitsPerComponent")
                .ok()
                .and_then(|o| o.as_i64().ok())
                .unwrap_or(-1);
            let has_smask = s.dict.get(b"SMask").is_ok();
            rows.push((*id, (filter, cs, w, h, bpc, has_smask, s.content.len())));
        }
    }
    rows.sort_by_key(|(id, _)| *id);
    rows
}

fn census(path: &str) {
    let doc = Document::load(path).expect("load");
    let rows = image_rows(&doc);
    println!("== {path}");
    let mut by_class: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
    for (id, (filter, cs, w, h, bpc, smask, len)) in &rows {
        let e = by_class.entry(filter.clone()).or_default();
        e.0 += 1;
        e.1 += len;
        println!(
            "  obj {:>4} {:<12} {:<14} {:>5}x{:<5} bpc{} smask:{} {:>9} B",
            id.0,
            filter,
            cs,
            w,
            h,
            bpc,
            if *smask { "y" } else { "n" },
            len
        );
    }
    println!("  -- class totals:");
    for (class, (n, bytes)) in by_class {
        println!("     {class:<14} {n:>3} streams {bytes:>9} B");
    }
}

fn diff(a: &str, b: &str) {
    let da = Document::load(a).expect("load a");
    let db = Document::load(b).expect("load b");
    let ra: std::collections::BTreeMap<_, _> = image_rows(&da).into_iter().collect();
    let rb: std::collections::BTreeMap<_, _> = image_rows(&db).into_iter().collect();
    println!("== diff {a} -> {b} (streams whose size or filter changed)");
    for (id, row_a) in &ra {
        match rb.get(id) {
            Some(row_b) if row_a == row_b => {}
            Some(row_b) => println!("  obj {:>4} {:?} -> {:?}", id.0, row_a, row_b),
            None => println!("  obj {:>4} {:?} -> GONE", id.0, row_a),
        }
    }
    for (id, row_b) in &rb {
        if !ra.contains_key(id) {
            println!("  obj {:>4} NEW {:?}", id.0, row_b);
        }
    }
}

fn pages(path: &str) {
    let doc = Document::load(path).expect("load");
    println!("== pages of {path} (page -> image object ids)");
    for (page_no, page_id) in doc.get_pages() {
        let Ok(Object::Dictionary(page)) = doc.get_object(page_id) else {
            continue;
        };
        let resources = match page.get(b"Resources") {
            Ok(Object::Reference(id)) => match doc.get_object(*id) {
                Ok(Object::Dictionary(d)) => Some(d),
                _ => None,
            },
            Ok(Object::Dictionary(d)) => Some(d),
            _ => None,
        };
        let Some(resources) = resources else { continue };
        let xobjects = match resources.get(b"XObject") {
            Ok(Object::Reference(id)) => match doc.get_object(*id) {
                Ok(Object::Dictionary(d)) => Some(d),
                _ => None,
            },
            Ok(Object::Dictionary(d)) => Some(d),
            _ => None,
        };
        let Some(xobjects) = xobjects else { continue };
        let mut ids: Vec<u32> = xobjects
            .iter()
            .filter_map(|(_, v)| match v {
                Object::Reference(id) => Some(id.0),
                _ => None,
            })
            .collect();
        ids.sort_unstable();
        if !ids.is_empty() {
            println!("  page {page_no}: {ids:?}");
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("census") => args[1..].iter().for_each(|p| census(p)),
        Some("diff") => diff(&args[1], &args[2]),
        Some("pages") => args[1..].iter().for_each(|p| pages(p)),
        _ => eprintln!("usage: spike_census census|pages <pdf...> | diff <a.pdf> <b.pdf>"),
    }
}
