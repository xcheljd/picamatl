//! Synthetic whole-PDF seeds for the `optimize_nopack` target: tiny one-page
//! files in every cross-reference layout the classic save path inherits —
//! a classic table, an xref stream, and a hybrid file (classic table plus an
//! `/XRefStm` stream listing objects kept in an object stream) — with stale
//! trailer keys (the `/Type/XRef/W/Index` set a Distiller classic trailer
//! carries, and the generic stream keys `/DL /F /FFilter /FDecodeParms`),
//! with and without a PDF/A-1B catalog XMP, under headers from 1.0 to 1.7.
//!
//! Written byte by byte so each trailer is exactly what is listed. Every
//! seed draws two byte-identical images (the merge makes the rewrite real
//! work) and carries an unfiltered, compressible content stream (deflated on
//! save: `/FlateDecode`, PDF 1.2, so the pre-1.2 headers must be raised).
use std::path::Path;

const XMP: &str = "<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
<rdf:Description rdf:about=\"\" xmlns:pdfaid=\"http://www.aiim.org/pdfa/ns/id/\">\
<pdfaid:part>1</pdfaid:part><pdfaid:conformance>B</pdfaid:conformance>\
</rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>";

/// The classic trailer `corpus/adobe-spec.pdf` (Distiller 8.1.0) ends in,
/// minus `/Encrypt /ID /Info`: an xref-stream key set on a classic table.
const ADOBE: &str =
    "/DecodeParms<</Columns 5/Predictor 12>>/Filter/FlateDecode/Length 137/Type/XRef/W[1 3 1]/Index[124332 848]";
const GENERIC: &str = "/DL 999999/F(external.dat)/FFilter/FlateDecode/FDecodeParms<<>>";

#[derive(Clone, Copy, PartialEq)]
enum Form {
    Table,
    Stream,
    Hybrid,
}

fn stream_obj(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut v = format!("<<{dict}/Length {}>>stream\n", data.len()).into_bytes();
    v.extend_from_slice(data);
    v.extend_from_slice(b"\nendstream");
    v
}

fn build(form: Form, extra: &str, pdfa: bool, version: &str) -> Vec<u8> {
    let mut x: u32 = 0x1234_567;
    let noise: Vec<u8> = (0..24 * 24)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 24) as u8
        })
        .collect();
    let mut content =
        b"q 100 0 0 100 50 50 cm /Im1 Do Q q 100 0 0 100 200 50 cm /Im2 Do Q\n".to_vec();
    content.extend_from_slice(&b"0 0 m 10 10 l S\n".repeat(40));
    let image = stream_obj(
        "/Type/XObject/Subtype/Image/Width 24/Height 24/ColorSpace/DeviceGray/BitsPerComponent 8",
        &noise,
    );
    let catalog = format!(
        "<</Type/Catalog/Pages 2 0 R{}>>",
        if pdfa { "/Metadata 7 0 R" } else { "" }
    );
    let pages = "<</Type/Pages/Kids[3 0 R]/Count 1>>";
    let page = "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R\
/Resources<</XObject<</Im1 5 0 R/Im2 6 0 R>>>>>>";

    // Top-level objects (id, body). In the hybrid layout the page tree
    // (objects 2 and 3) lives in object stream 8.
    let mut objs: Vec<(u32, Vec<u8>)> = vec![(1, catalog.into_bytes())];
    if form != Form::Hybrid {
        objs.push((2, pages.as_bytes().to_vec()));
        objs.push((3, page.as_bytes().to_vec()));
    }
    objs.push((4, stream_obj("", &content)));
    objs.push((5, image.clone()));
    objs.push((6, image));
    if pdfa {
        objs.push((7, stream_obj("/Type/Metadata/Subtype/XML", XMP.as_bytes())));
    }
    if form == Form::Hybrid {
        let head = format!("2 0 3 {} ", pages.len() + 1);
        let body = format!("{head}{pages} {page}");
        objs.push((
            8,
            stream_obj(
                &format!("/Type/ObjStm/N 2/First {}", head.len()),
                body.as_bytes(),
            ),
        ));
    }

    let mut out = format!("%PDF-{version}\n").into_bytes();
    out.extend_from_slice(b"%\xe2\xe3\xcf\xd3\n");
    let mut offsets = std::collections::BTreeMap::new();
    for (id, body) in &objs {
        offsets.insert(*id, out.len());
        out.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let size = 10;
    match form {
        Form::Table => {
            let start = out.len();
            out.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
            for id in 1..size {
                match offsets.get(&id) {
                    Some(off) => out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes()),
                    None => out.extend_from_slice(b"0000000000 00001 f \n"),
                }
            }
            out.extend_from_slice(
                format!("trailer\n<</Size {size}/Root 1 0 R{extra}>>\nstartxref\n{start}\n%%EOF\n")
                    .as_bytes(),
            );
        }
        Form::Stream => {
            let start = out.len();
            let mut rows = vec![0u8, 0, 0, 0, 0, 0xff, 0xff];
            for id in 1..size {
                let (t, off) = match (id, offsets.get(&id)) {
                    (9, _) => (1u8, start),
                    (_, Some(off)) => (1, *off),
                    (_, None) => (0, 0),
                };
                rows.push(t);
                rows.extend_from_slice(&(off as u32).to_be_bytes());
                rows.extend_from_slice(&[0, 0]);
            }
            let dict = format!("/Type/XRef/Size {size}/W[1 4 2]/Root 1 0 R{extra}");
            out.extend_from_slice(b"9 0 obj\n");
            out.extend_from_slice(&stream_obj(&dict, &rows));
            out.extend_from_slice(format!("\nendobj\nstartxref\n{start}\n%%EOF\n").as_bytes());
        }
        Form::Hybrid => {
            // Object 9: the /XRefStm stream for objects 2, 3 (in ObjStm 8)
            // and 8, 9 themselves; the classic table lists the rest.
            let xs = out.len();
            let mut rows = Vec::new();
            for (t, a, b) in [
                (2u8, 8u32, 0u16),
                (2, 8, 1),
                (1, offsets[&8] as u32, 0),
                (1, xs as u32, 0),
            ] {
                rows.push(t);
                rows.extend_from_slice(&a.to_be_bytes());
                rows.extend_from_slice(&b.to_be_bytes());
            }
            out.extend_from_slice(b"9 0 obj\n");
            out.extend_from_slice(&stream_obj(
                &format!("/Type/XRef/Size {size}/W[1 4 2]/Index[2 2 8 2]"),
                &rows,
            ));
            out.extend_from_slice(b"\nendobj\n");
            let start = out.len();
            out.extend_from_slice(b"xref\n0 2\n0000000000 65535 f \n");
            out.extend_from_slice(format!("{:010} 00000 n \n", offsets[&1]).as_bytes());
            let rest: Vec<u32> = (4..8).filter(|id| offsets.contains_key(id)).collect();
            out.extend_from_slice(format!("4 {}\n", rest.len()).as_bytes());
            for id in rest {
                out.extend_from_slice(format!("{:010} 00000 n \n", offsets[&id]).as_bytes());
            }
            out.extend_from_slice(
                format!(
                    "trailer\n<</Size {size}/Root 1 0 R/XRefStm {xs}{extra}>>\nstartxref\n{start}\n%%EOF\n"
                )
                .as_bytes(),
            );
        }
    }
    out
}

fn main() {
    let out = std::env::args().nth(1).expect("output dir");
    let dir = Path::new(&out);
    std::fs::create_dir_all(dir).unwrap();
    let versions = ["1.0", "1.1", "1.3", "1.4"];
    let mut n = 0;
    let mut write = |name: String, bytes: Vec<u8>| {
        std::fs::write(dir.join(name), bytes).unwrap();
        n += 1;
    };
    for (form, fname, extras) in [
        (
            Form::Table,
            "table",
            &[("plain", ""), ("adobe", ADOBE), ("generic", GENERIC)][..],
        ),
        (
            Form::Hybrid,
            "hybrid",
            &[("plain", ""), ("adobe", ADOBE), ("generic", GENERIC)][..],
        ),
        // An xref stream's own dictionary cannot also carry a second
        // /Type /W /Index set, so only the generic keys are stale there.
        (
            Form::Stream,
            "stream",
            &[("plain", ""), ("generic", GENERIC)][..],
        ),
    ] {
        for (i, (ename, extra)) in extras.iter().enumerate() {
            for pdfa in [false, true] {
                let v = versions[(i * 2 + usize::from(pdfa)) % versions.len()];
                let tag = if pdfa { "-pdfa1" } else { "" };
                write(
                    format!("xref-{fname}-{ename}{tag}-v{v}.pdf"),
                    build(form, extra, pdfa, v),
                );
            }
        }
    }
    // Every 1.x header on the plain classic layout.
    for v in ["1.0", "1.1", "1.2", "1.3", "1.4", "1.5", "1.6", "1.7"] {
        write(
            format!("xref-table-header-v{v}.pdf"),
            build(Form::Table, "", false, v),
        );
    }
    println!("wrote {n} seeds to {out}");
}
