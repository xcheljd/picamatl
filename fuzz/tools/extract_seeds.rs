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
            // Legacy filters, by decoder: the raw payload of the FIRST
            // filter in the chain (what that decoder reads). seed.sh adds
            // the selector byte the lzw target expects.
            let first = match s.dict.get(b"Filter") {
                Ok(Object::Name(n)) => Some(n.clone()),
                Ok(Object::Array(a)) => a.first().and_then(|o| o.as_name().ok()).map(<[u8]>::to_vec),
                _ => None,
            };
            match first.as_deref() {
                Some(b"LZWDecode") => save("lzw_lzw", &s.content),
                Some(b"ASCII85Decode") => save("lzw_a85", &s.content),
                Some(b"ASCIIHexDecode") => save("lzw_ahx", &s.content),
                Some(b"RunLengthDecode") => save("lzw_rl", &s.content),
                _ => {}
            }
            if first.as_deref() == Some(b"CCITTFaxDecode") {
                ccitt_seeds(&doc, s, &mut save);
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

/// Seeds for the ccitt target from one K < 0 CCITT image (both dimensions
/// at most 4096, the target's range): the stored stream itself (mode 0 =
/// continuous, 1 = /EncodedByteAlign), plus, when it decodes, 24-row bands
/// from the top, middle and bottom re-encoded byte-aligned (mode 1) and as
/// round-trip bitmaps (mode 2). The bands keep small real byte-aligned
/// inputs in the corpus.
fn ccitt_seeds(doc: &Document, s: &lopdf::Stream, save: &mut impl FnMut(&'static str, &[u8])) {
    let int = |o: Option<&Object>| {
        o.map(|o| match o {
            Object::Reference(r) => doc.get_object(*r).ok().cloned().unwrap_or(Object::Null),
            o => o.clone(),
        })
        .and_then(|o| o.as_i64().ok())
    };
    let parms = match s.dict.get(b"DecodeParms") {
        Ok(Object::Dictionary(d)) => d.clone(),
        Ok(Object::Array(a)) if a.len() == 1 => match a[0].as_dict() {
            Ok(d) => d.clone(),
            Err(_) => return,
        },
        _ => return,
    };
    if int(parms.get(b"K").ok()).unwrap_or(0) >= 0 {
        return;
    }
    let aligned = matches!(parms.get(b"EncodedByteAlign"), Ok(Object::Boolean(true)));
    let (Some(w), Some(h)) = (int(s.dict.get(b"Width").ok()), int(s.dict.get(b"Height").ok()))
    else {
        return;
    };
    if !(1..=4096).contains(&w) || !(1..=4096).contains(&h) {
        return;
    }
    let header = |mode: u8, w: i64, h: i64| {
        let (w, h) = ((w - 1) as u16, (h - 1) as u16);
        let mut v = vec![mode];
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v
    };
    let mut raw = header(u8::from(aligned), w, h);
    raw.extend_from_slice(&s.content);
    save(if aligned { "ccitt_eba" } else { "ccitt_g4" }, &raw);
    let (wu, hu) = (w as u32, h as usize);
    let samples = if aligned {
        picamatl::fuzz_api::ccitt_g4_decode(&s.content, wu, hu, true)
    } else {
        picamatl::fuzz_api::fax_g4_decode(&s.content, wu, hu)
    };
    let Some(samples) = samples else { return };
    let stride = (w as usize).div_ceil(8);
    let band = 24.min(hu);
    for start in [0, (hu - band) / 2, hu - band] {
        let rows = &samples[start * stride..(start + band) * stride];
        let enc = picamatl::fuzz_api::ccitt_g4_encode_aligned(rows, wu, band);
        let mut seed = header(1, w, band as i64);
        seed.extend_from_slice(&enc);
        save("ccitt_band", &seed);
        let mut rt = header(2, w, 1);
        rt.extend_from_slice(rows);
        save("ccitt_rt", &rt);
    }
}
