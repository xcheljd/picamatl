//! Diagnostic: print every image XObject dict of a PDF. Scratch tool.
use lopdf::{Document, Object};

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let doc = Document::load(&path).unwrap();
    for (id, obj) in doc.objects.iter() {
        if let Object::Stream(s) = obj {
            if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
                println!(
                    "{id:?}: {} content bytes\n{:?}\nfirst bytes: {:02x?}",
                    s.content.len(),
                    s.dict,
                    &s.content[..16.min(s.content.len())]
                );
            }
        }
    }
}
