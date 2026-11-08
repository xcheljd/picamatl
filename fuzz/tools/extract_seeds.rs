//! Throwaway: pull fuzz seeds out of real PDFs. Writes each distinct payload
//! (by FNV hash) under <out>/<kind>/, capped per kind and per size.
use lopdf::{Document, Object};
use std::collections::HashSet;
use std::path::Path;

const MAX_SEED: usize = 256 * 1024;

fn fnv(d: &[u8]) -> u64 {
    d.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100000001b3))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for path in args {
        let Ok(doc) = Document::load(&path) else { eprintln!("skip {path}"); continue };
        let mut save = |kind: &'static str, bytes: &[u8]| {
            if bytes.is_empty() || bytes.len() > MAX_SEED { return; }
            let h = fnv(bytes);
            if !seen.insert(h) { return; }
            let n = counts.entry(kind).or_default();
            if *n >= 400 { return; }
            *n += 1;
            let dir = Path::new(&out).join(kind);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{h:016x}")), bytes).unwrap();
        };
        let raw_len = |s: &lopdf::Stream| s.content.len();
        for obj in doc.objects.values() {
            let Object::Stream(s) = obj else { continue };
            let filter = s.dict.get(b"Filter").ok().and_then(|f| match f {
                Object::Name(n) => Some(n.clone()),
                Object::Array(a) if a.len() == 1 => a[0].as_name().ok().map(<[u8]>::to_vec),
                _ => None,
            });
            if filter.as_deref() == Some(b"FlateDecode") && raw_len(s) <= MAX_SEED {
                save("deflate", &s.content);
            }
            if filter.as_deref() == Some(b"DCTDecode") {
                save("jpeghuff", &s.content);
            }
            let decoded = s.decompressed_content().ok();
            let body = decoded.as_deref().unwrap_or(&s.content);
            if s.dict.has(b"Length1") || s.dict.has(b"Length2") {
                if s.dict.has(b"Length2") { save("type1", body) } else { save("truetype", body) }
            }
            match s.dict.get(b"Subtype").ok().and_then(|o| o.as_name().ok()) {
                Some(b"Type1C") | Some(b"CIDFontType0C") => save("cff_font", body),
                Some(b"Form") => save("content", body),
                _ => {}
            }
        }
        for (_, page) in doc.get_pages() {
            let c = doc.get_page_content(page);
            save("content", &c);
        }
    }
    println!("{counts:?}");
}
