#![no_main]
//! Defaults with object-stream packing off (`--no-pack-object-streams`), the
//! only way a non-PDF/A file reaches the classic save path: lopdf's plain
//! writer keeps the input's xref form (classic table or stream), so the
//! classic-trailer key strip and the header version rules run on every
//! input here, not just on PDF/A-1-labelled ones.
//!
//! Same contract as `optimize` (never larger, any rewrite loads), plus two
//! checks read back from the rewritten file itself:
//! - no trailer key that describes the input's cross-reference section
//!   survives (`/Prev /XRefStm /F /FFilter /FDecodeParms /DL` never; on a
//!   classic table also not `/Type /XRef`, `/W`, `/Index`, `/Filter`,
//!   `/DecodeParms`, `/Length`);
//! - the `%PDF-x.y` header covers the xref form and the stream filters the
//!   file uses (xref stream 1.5, `/JPXDecode` 1.5, `/JBIG2Decode` 1.4,
//!   `/FlateDecode` 1.2; ISO 32000-1 7.5.8 and Table 6). Headers that are
//!   not `<digits>.<digits>` are skipped: there is no number to compare.

use libfuzzer_sys::fuzz_target;
use lopdf::xref::XrefType;
use lopdf::{Document, Object};
use picamatl::{optimize_with_options, OptimizeOptions};

fn version(v: &str) -> Option<(u32, u32)> {
    let (a, b) = v.split_once('.')?;
    let num = |s: &str| {
        (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
            .then(|| s.parse::<u32>().ok())
            .flatten()
    };
    Some((num(a)?, num(b)?))
}

fn filter_floor(name: &[u8]) -> (u32, u32) {
    match name {
        b"FlateDecode" => (1, 2),
        b"JBIG2Decode" => (1, 4),
        b"JPXDecode" => (1, 5),
        _ => (1, 0),
    }
}

fuzz_target!(|data: &[u8]| {
    let opts = OptimizeOptions::default().with_pack_object_streams(false);
    let out = optimize_with_options(data, opts);
    assert!(out.len() <= data.len(), "output larger than input");
    if out == data {
        return;
    }
    let doc = Document::load_mem(&out).expect("rewritten output must load");

    let classic = matches!(
        doc.reference_table.cross_reference_type,
        XrefType::CrossReferenceTable
    );
    let mut stale: Vec<&[u8]> = vec![
        b"Prev",
        b"XRefStm",
        b"F",
        b"FFilter",
        b"FDecodeParms",
        b"DL",
    ];
    if classic {
        stale.extend_from_slice(&[b"W", b"Index", b"Filter", b"DecodeParms", b"Length"]);
        if matches!(doc.trailer.get(b"Type"), Ok(Object::Name(n)) if n == b"XRef") {
            panic!("classic trailer carries /Type /XRef");
        }
    }
    for key in stale {
        assert!(
            doc.trailer.get(key).is_err(),
            "stale trailer key /{} (classic: {classic})",
            String::from_utf8_lossy(key)
        );
    }

    let Some(declared) = version(&doc.version) else {
        return;
    };
    let mut floor = if classic { (1, 0) } else { (1, 5) };
    for object in doc.objects.values() {
        let Object::Stream(stream) = object else {
            continue;
        };
        match stream.dict.get(b"Filter") {
            Ok(Object::Name(n)) => floor = floor.max(filter_floor(n)),
            Ok(Object::Array(names)) => {
                for n in names {
                    if let Object::Name(n) = n {
                        floor = floor.max(filter_floor(n));
                    }
                }
            }
            _ => {}
        }
    }
    assert!(
        declared >= floor,
        "header {} under-declares: the output needs PDF {}.{}",
        doc.version,
        floor.0,
        floor.1
    );
});
