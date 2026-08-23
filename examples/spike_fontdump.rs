//! Scratch spike: dump every simple-font dictionary in a PDF — subtype,
//! encoding (incl. /Differences), descriptor /Flags, font-file presence/size.
//! Usage: cargo run --release --example spike_fontdump -- <file.pdf>

use lopdf::{Document, Object};

fn resolve<'a>(doc: &'a Document, obj: &'a Object) -> &'a Object {
    let mut cur = obj;
    for _ in 0..8 {
        match cur {
            Object::Reference(id) => match doc.get_object(*id) {
                Ok(next) => cur = next,
                Err(_) => break,
            },
            _ => break,
        }
    }
    cur
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: spike_fontdump <pdf>");
    let doc = Document::load(&path).expect("load");
    for (id, obj) in &doc.objects {
        let Object::Dictionary(d) = obj else { continue };
        let Ok(Object::Name(t)) = d.get(b"Type") else {
            continue;
        };
        if t != b"Font" {
            continue;
        }
        let subtype = match d.get(b"Subtype") {
            Ok(Object::Name(n)) => String::from_utf8_lossy(n).to_string(),
            _ => "?".into(),
        };
        if subtype == "Type0" || subtype == "CIDFontType2" || subtype == "CIDFontType0" {
            continue;
        }
        let base = match d.get(b"BaseFont") {
            Ok(Object::Name(n)) => String::from_utf8_lossy(n).to_string(),
            _ => "?".into(),
        };
        let enc = match d.get(b"Encoding") {
            Err(_) => "<absent>".into(),
            Ok(e) => match resolve(&doc, e) {
                Object::Name(n) => String::from_utf8_lossy(n).to_string(),
                Object::Dictionary(ed) => {
                    let be = match ed.get(b"BaseEncoding") {
                        Ok(Object::Name(n)) => String::from_utf8_lossy(n).to_string(),
                        _ => "<none>".into(),
                    };
                    let diffs = match ed.get(b"Differences") {
                        Ok(Object::Array(a)) => format!("{} entries: {:?}", a.len(), a),
                        _ => "<none>".into(),
                    };
                    format!("dict base={be} diffs={diffs}")
                }
                other => format!("{other:?}"),
            },
        };
        let (flags, ff) = match d.get(b"FontDescriptor").map(|o| resolve(&doc, o)) {
            Ok(Object::Dictionary(fd)) => {
                let flags = match fd.get(b"Flags").map(|o| resolve(&doc, o)) {
                    Ok(Object::Integer(i)) => *i,
                    _ => -1,
                };
                let ff = ["FontFile", "FontFile2", "FontFile3"]
                    .iter()
                    .filter_map(|k| {
                        fd.get(k.as_bytes()).ok().map(|o| match resolve(&doc, o) {
                            Object::Stream(s) => format!("{k}({}B)", s.content.len()),
                            _ => format!("{k}(?)"),
                        })
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                (flags, ff)
            }
            _ => (-1, "<no descriptor>".into()),
        };
        let widths = d.get(b"Widths").is_ok();
        let (fc, lc) = (
            d.get(b"FirstChar")
                .ok()
                .and_then(|o| resolve(&doc, o).as_i64().ok()),
            d.get(b"LastChar")
                .ok()
                .and_then(|o| resolve(&doc, o).as_i64().ok()),
        );
        println!(
            "{id:?} {subtype} {base} enc={enc} flags={flags} ff=[{ff}] widths={widths} fc={fc:?} lc={lc:?}"
        );
    }
}
