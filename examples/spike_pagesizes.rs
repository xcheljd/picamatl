//! Diagnostic: per-page stored content-stream bytes, totals by object kind,
//! and raw-byte scans for ObjStm/XRef stream lengths, for PDFs side by side.
//! Scratch tool for the minify pass.
use lopdf::{Document, Object};

fn find_lengths(raw: &[u8], marker: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + marker.len() <= raw.len() {
        if &raw[i..i + marker.len()] == marker {
            // scan forward for "/Length " within 200 bytes
            let window = &raw[i..(i + 200).min(raw.len())];
            if let Some(p) = window.windows(7).position(|w| w == b"/Length") {
                let mut q = i + p + 7;
                while raw.get(q).is_some_and(|b| b.is_ascii_whitespace()) {
                    q += 1;
                }
                let start = q;
                while raw.get(q).is_some_and(|b| b.is_ascii_digit()) {
                    q += 1;
                }
                if let Ok(n) = std::str::from_utf8(&raw[start..q]).unwrap_or("x").parse() {
                    out.push(n);
                }
            }
            i += marker.len();
        } else {
            i += 1;
        }
    }
    out
}

fn stats(path: &str) {
    let raw = std::fs::read(path).unwrap();
    let objstm = find_lengths(&raw, b"/ObjStm");
    let xref = find_lengths(&raw, b"/XRef");
    let doc = Document::load(path).unwrap();
    let mut content_total = 0usize;
    for (_, page_id) in doc.get_pages() {
        for sid in doc.get_page_contents(page_id) {
            if let Ok(s) = doc.get_object(sid).and_then(Object::as_stream) {
                content_total += s.content.len();
            }
        }
    }
    let mut stream_total = 0usize;
    let mut nstreams = 0usize;
    for obj in doc.objects.values() {
        if let Object::Stream(s) = obj {
            nstreams += 1;
            stream_total += s.content.len();
        }
    }
    println!(
        "{path}: file={} objs={} streams={nstreams} stream_bytes={stream_total} page_content={content_total} objstm={objstm:?} xref={xref:?}",
        raw.len(),
        doc.objects.len(),
    );
}

fn main() {
    for a in std::env::args().skip(1) {
        stats(&a);
    }
}
