//! Diagnostic: print each page's decoded content stream and resource dict.
use lopdf::{Document, Object};

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let doc = Document::load(&path).unwrap();
    for (num, page_id) in doc.get_pages() {
        println!("== page {num} {page_id:?}");
        if let Ok(page) = doc.get_object(page_id).and_then(Object::as_dict) {
            println!("page dict: {page:?}");
        }
        for sid in doc.get_page_contents(page_id) {
            if let Ok(s) = doc.get_object(sid).and_then(Object::as_stream) {
                println!(
                    "content stream {sid:?}: dict {:?} stored {} bytes",
                    s.dict,
                    s.content.len()
                );
            }
        }
        let c = doc.get_page_content(page_id);
        println!(
            "content ({} bytes):\n{}",
            c.len(),
            String::from_utf8_lossy(&c)
        );
    }
}
