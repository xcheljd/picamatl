//! `--flatten-forms` — end-to-end behaviour on synthetic filled AcroForms.
//!
//! The point of every test here is the same one: **a filled-in value must
//! survive flattening or the document must decline.** The fixtures are built
//! programmatically so the exact appearance-stream bytes, `/Rect`, `/BBox` and
//! `/Matrix` are known, which is what lets the assertions be about ink rather
//! than about file size.
//!
//! `flatten_render_is_pixel_identical` additionally renders the original (with
//! its annotations, i.e. what a viewer shows) against the flattened output
//! through Ghostscript and compares every page pixel for pixel. It is skipped
//! with a message when `gs` is not installed.

use std::collections::HashSet;
use std::process::Command;

use picamatl::{optimize_with_options, OptimizeOptions};
use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream, StringFormat};

// -- options ----------------------------------------------------------------

/// Defaults plus the flag. Nothing else is turned on: the tests are about
/// flattening, not about the image or font paths.
fn flatten() -> OptimizeOptions {
    OptimizeOptions::default().with_flatten_forms(true)
}

fn plain() -> OptimizeOptions {
    OptimizeOptions::default()
}

// -- fixture construction ---------------------------------------------------

/// The text a filled text field shows, and the bytes its appearance stream
/// paints. Deliberately distinctive so it can be searched for in the output.
const FILLED_TEXT: &str = "Ada Lovelace";

fn text(value: &str) -> Object {
    Object::String(value.as_bytes().to_vec(), StringFormat::Literal)
}

/// A UTF-16BE PDF text string with a byte-order mark — how Acrobat actually
/// writes field names and values, and the encoding the XFA mirror check has to
/// decode before it can compare against the packet's UTF-8.
fn utf16(value: &str) -> Object {
    let mut bytes = vec![0xFE, 0xFF];
    for unit in value.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    Object::String(bytes, StringFormat::Literal)
}

/// A form XObject that paints `label` at the given size, plus a visible
/// border, inside a `w` x `h` box.
fn appearance(doc: &mut Document, font_id: ObjectId, w: f64, h: f64, label: &str) -> ObjectId {
    appearance_with(doc, Some(font_id), w, h, label)
}

/// `font` is `None` to build an appearance with **no** `/Resources` at all,
/// the shape that was leaning on `/AcroForm /DR`.
fn appearance_with(
    doc: &mut Document,
    font_id: Option<ObjectId>,
    w: f64,
    h: f64,
    label: &str,
) -> ObjectId {
    let ops = vec![
        Operation::new("q", vec![]),
        Operation::new("g", vec![Object::Real(0.0)]),
        Operation::new("BT", vec![]),
        Operation::new(
            "Tf",
            vec![Object::Name(b"Helv".to_vec()), Object::Real(9.0)],
        ),
        Operation::new("Td", vec![Object::Real(1.0), Object::Real(2.0)]),
        Operation::new(
            "Tj",
            vec![Object::String(
                label.as_bytes().to_vec(),
                StringFormat::Literal,
            )],
        ),
        Operation::new("ET", vec![]),
        Operation::new("Q", vec![]),
    ];
    let mut dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Form",
        "BBox" => vec![0.into(), 0.into(), Object::Real(w as f32), Object::Real(h as f32)],
    };
    if let Some(font_id) = font_id {
        dict.set(
            "Resources",
            dictionary! { "Font" => dictionary! { "Helv" => font_id } },
        );
    }
    doc.add_object(Stream::new(
        dict,
        Content { operations: ops }.encode().unwrap(),
    ))
}

/// Knobs the decline tests turn, one at a time, off an otherwise-flattenable
/// document.
#[derive(Default, Clone)]
struct Fixture {
    /// Give the filled text field no `/AP` at all (D9).
    text_field_without_appearance: bool,
    /// Mark the filled text field's widget `Hidden` (D9, hidden branch).
    hide_text_field: bool,
    /// `/AcroForm /NeedAppearances true` (D5).
    need_appearances: bool,
    /// Catalog `/NeedsRendering true` — a dynamic XFA form (D3).
    needs_rendering: bool,
    /// Attach an XFA packet set whose `datasets` says what the argument says.
    xfa_datasets: Option<String>,
    /// Add a `/FT /Sig` field carrying a signature dictionary (D6).
    signed_field: bool,
    /// Put an `/OC` entry on the checkbox widget (D7).
    optional_content: bool,
    /// Leave the checkbox's `/AS` off while `/AP /N` is a state dict (D10).
    drop_appearance_state: bool,
    /// Emit page content with one more `Q` than `q` (D13).
    unbalanced_content: bool,
    /// Strip `/Resources` off the text field's appearance stream, leaving its
    /// `Tf` to resolve against `/AcroForm /DR` (D15).
    appearance_without_dr: bool,
    /// Put `/Contents` behind an indirect reference to an *array* of streams,
    /// the shape the splice has to unwrap rather than nest.
    indirect_contents_array: bool,
}

/// A one-page filled AcroForm: a text field with a value and an appearance, a
/// checked checkbox with an on/off appearance dictionary, a two-button radio
/// group with one button selected, an unfilled empty field, and a `/Link`
/// annotation that must survive untouched.
fn build(fixture: &Fixture) -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });

    // -- text field, filled -------------------------------------------------
    let text_ap = appearance_with(
        &mut doc,
        (!fixture.appearance_without_dr).then_some(font_id),
        160.0,
        14.0,
        FILLED_TEXT,
    );
    let mut text_widget = dictionary! {
        "Type" => "Annot",
        "Subtype" => "Widget",
        "FT" => "Tx",
        "T" => utf16("name"),
        "V" => utf16(FILLED_TEXT),
        "Rect" => vec![72.into(), 700.into(), 232.into(), 714.into()],
        "P" => pages_id,
    };
    if !fixture.text_field_without_appearance {
        text_widget.set("AP", dictionary! { "N" => text_ap });
    }
    // Annotation flags: `Print` (bit 3) is what real form widgets carry, and
    // what makes a rendering device draw them at all.
    text_widget.set(
        "F",
        Object::Integer(if fixture.hide_text_field { 2 } else { 4 }),
    );
    let text_id = doc.add_object(text_widget);

    // -- checkbox, checked --------------------------------------------------
    let on_ap = appearance(&mut doc, font_id, 12.0, 12.0, "X");
    let off_ap = appearance(&mut doc, font_id, 12.0, 12.0, "");
    let mut check_widget = dictionary! {
        "Type" => "Annot",
        "Subtype" => "Widget",
        "FT" => "Btn",
        "T" => utf16("agree"),
        "V" => Object::Name(b"Yes".to_vec()),
        "Rect" => vec![72.into(), 660.into(), 84.into(), 672.into()],
        "F" => 4,
        "AP" => dictionary! { "N" => dictionary! { "Yes" => on_ap, "Off" => off_ap } },
        "P" => pages_id,
    };
    if !fixture.drop_appearance_state {
        check_widget.set("AS", Object::Name(b"Yes".to_vec()));
    }
    if fixture.optional_content {
        let ocg = doc.add_object(dictionary! { "Type" => "OCG", "Name" => text("layer") });
        check_widget.set("OC", ocg);
    }
    let check_id = doc.add_object(check_widget);

    // -- radio group, second button selected --------------------------------
    let radio_a = appearance(&mut doc, font_id, 12.0, 12.0, "");
    let radio_b = appearance(&mut doc, font_id, 12.0, 12.0, "o");
    let radio_group = doc.new_object_id();
    let radio_a_id = doc.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Widget",
        "Parent" => radio_group,
        "Rect" => vec![72.into(), 630.into(), 84.into(), 642.into()],
        "F" => 4,
        "AS" => Object::Name(b"Off".to_vec()),
        "AP" => dictionary! { "N" => dictionary! { "A" => radio_a } },
        "P" => pages_id,
    });
    let radio_b_id = doc.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Widget",
        "Parent" => radio_group,
        "Rect" => vec![120.into(), 630.into(), 132.into(), 642.into()],
        "F" => 4,
        "AS" => Object::Name(b"B".to_vec()),
        "AP" => dictionary! { "N" => dictionary! { "B" => radio_b } },
        "P" => pages_id,
    });
    doc.set_object(
        radio_group,
        dictionary! {
            "FT" => "Btn",
            "Ff" => Object::Integer(1 << 15), // Radio
            "T" => utf16("choice"),
            "V" => Object::Name(b"B".to_vec()),
            "Kids" => vec![radio_a_id.into(), radio_b_id.into()],
        },
    );

    // -- an empty field: nothing to preserve, nothing drawn -----------------
    let empty_id = doc.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Widget",
        "FT" => "Tx",
        "T" => utf16("comment"),
        "V" => text(""),
        "Rect" => vec![300.into(), 700.into(), 460.into(), 714.into()],
        "F" => 4,
        "P" => pages_id,
    });

    // -- a link annotation, which is not form machinery ---------------------
    let link_id = doc.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Link",
        "Rect" => vec![72.into(), 600.into(), 200.into(), 612.into()],
        "A" => dictionary! { "S" => "URI", "URI" => text("https://example.invalid/") },
    });

    let mut annots = vec![
        Object::Reference(text_id),
        Object::Reference(check_id),
        Object::Reference(radio_a_id),
        Object::Reference(radio_b_id),
        Object::Reference(empty_id),
        Object::Reference(link_id),
    ];
    let mut fields = vec![
        Object::Reference(text_id),
        Object::Reference(check_id),
        Object::Reference(radio_group),
        Object::Reference(empty_id),
    ];

    if fixture.signed_field {
        let sig = doc.add_object(dictionary! {
            "Type" => "Sig",
            "Filter" => "Adobe.PPKLite",
            "ByteRange" => vec![0.into(), 100.into(), 200.into(), 100.into()],
            "Contents" => Object::String(vec![0u8; 32], StringFormat::Hexadecimal),
        });
        let sig_field = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Sig",
            "T" => utf16("signature"),
            "V" => sig,
            "Rect" => vec![300.into(), 630.into(), 400.into(), 660.into()],
            "F" => 4,
            "P" => pages_id,
        });
        annots.push(Object::Reference(sig_field));
        fields.push(Object::Reference(sig_field));
    }

    // -- page ---------------------------------------------------------------
    // A top-level `cm` outside any q/Q: the CTM this content leaves behind is
    // NOT the identity, which is exactly why the splice wraps it in q/Q.
    let mut page_ops =
        String::from("1 0 0 1 20 20 cm\nq\n0 g\nBT /Helv 12 Tf 40 740 Td (Form) Tj ET\nQ\n");
    if fixture.unbalanced_content {
        page_ops.push_str("Q\n");
    }
    let content_id = doc.add_object(Stream::new(dictionary! {}, page_ops.into_bytes()));
    let contents: Object = if fixture.indirect_contents_array {
        Object::Reference(doc.add_object(Object::Array(vec![Object::Reference(content_id)])))
    } else {
        Object::Reference(content_id)
    };
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => contents,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Resources" => dictionary! { "Font" => dictionary! { "Helv" => font_id } },
        "Annots" => annots,
    });

    let mut acroform = dictionary! {
        "Fields" => fields,
        "DA" => text("/Helv 0 Tf 0 g"),
        "DR" => dictionary! { "Font" => dictionary! { "Helv" => font_id } },
    };
    if fixture.need_appearances {
        acroform.set("NeedAppearances", Object::Boolean(true));
    }
    if let Some(datasets) = &fixture.xfa_datasets {
        let template = doc.add_object(Stream::new(
            dictionary! {},
            // Padded so the packet is worth removing, like a real template.
            format!("<template>{}</template>", " ".repeat(4096)).into_bytes(),
        ));
        let datasets_id =
            doc.add_object(Stream::new(dictionary! {}, datasets.clone().into_bytes()));
        acroform.set(
            "XFA",
            Object::Array(vec![
                text("template"),
                Object::Reference(template),
                text("datasets"),
                Object::Reference(datasets_id),
            ]),
        );
    }

    doc.set_object(
        pages_id,
        dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        },
    );
    let mut catalog = dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
        "AcroForm" => acroform,
    };
    if fixture.needs_rendering {
        catalog.set("NeedsRendering", Object::Boolean(true));
    }
    let catalog_id = doc.add_object(catalog);
    doc.trailer.set("Root", catalog_id);
    doc.compress();

    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

/// The happy path: everything preservable, nothing to decline.
fn flattenable() -> Vec<u8> {
    build(&Fixture::default())
}

// -- inspection helpers -----------------------------------------------------

struct View(Document);

impl View {
    fn of(bytes: &[u8]) -> Self {
        View(Document::load_mem(bytes).expect("output must be a loadable PDF"))
    }

    fn catalog(&self) -> &Dictionary {
        self.0.catalog().unwrap()
    }

    fn page_id(&self) -> ObjectId {
        *self.0.get_pages().values().next().unwrap()
    }

    /// The page's content, concatenated and decompressed the way a viewer
    /// would execute it.
    fn page_content(&self) -> Vec<u8> {
        self.0.get_page_content(self.page_id())
    }

    fn annot_subtypes(&self) -> Vec<String> {
        let page = self
            .0
            .get_object(self.page_id())
            .unwrap()
            .as_dict()
            .unwrap();
        let Ok(annots) = page.get(b"Annots") else {
            return Vec::new();
        };
        let annots = match annots {
            Object::Reference(id) => self.0.get_object(*id).unwrap(),
            other => other,
        };
        annots
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                let dict = match entry {
                    Object::Reference(id) => self.0.get_object(*id).unwrap().as_dict().unwrap(),
                    other => other.as_dict().unwrap(),
                };
                String::from_utf8_lossy(dict.get(b"Subtype").unwrap().as_name().unwrap())
                    .into_owned()
            })
            .collect()
    }

    /// Object ids the page's `/XObject` resources bind, by resource name.
    fn page_xobjects(&self) -> Vec<(String, ObjectId)> {
        let page = self
            .0
            .get_object(self.page_id())
            .unwrap()
            .as_dict()
            .unwrap();
        let Ok(resources) = page.get(b"Resources") else {
            return Vec::new();
        };
        let resources = match resources {
            Object::Reference(id) => self.0.get_object(*id).unwrap(),
            other => other,
        };
        let Ok(xobjects) = resources.as_dict().unwrap().get(b"XObject") else {
            return Vec::new();
        };
        let xobjects = match xobjects {
            Object::Reference(id) => self.0.get_object(*id).unwrap(),
            other => other,
        };
        xobjects
            .as_dict()
            .unwrap()
            .iter()
            .filter_map(|(name, value)| match value {
                Object::Reference(id) => Some((String::from_utf8_lossy(name).into_owned(), *id)),
                _ => None,
            })
            .collect()
    }

    /// Decompressed bytes of every stream in the document, for "did this
    /// content survive anywhere" questions.
    fn all_stream_bytes(&self) -> Vec<Vec<u8>> {
        self.0
            .objects
            .values()
            .filter_map(|object| match object {
                Object::Stream(stream) => stream.decompressed_content().ok(),
                _ => None,
            })
            .collect()
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// -- the happy path ---------------------------------------------------------

#[test]
fn flag_off_leaves_the_form_completely_untouched() {
    let input = flattenable();
    let out = optimize_with_options(&input, plain());
    let view = View::of(&out);
    assert!(
        view.catalog().has(b"AcroForm"),
        "without the flag the form must survive verbatim"
    );
    assert_eq!(
        view.annot_subtypes()
            .iter()
            .filter(|s| *s == "Widget")
            .count(),
        5,
        "every widget annotation must still be on the page"
    );
}

#[test]
fn flatten_removes_the_form_layer() {
    let input = flattenable();
    let out = optimize_with_options(&input, flatten());
    let view = View::of(&out);
    assert!(!view.catalog().has(b"AcroForm"), "/AcroForm must be gone");
    assert!(!view.catalog().has(b"NeedsRendering"));
    assert_eq!(
        view.annot_subtypes(),
        vec!["Link".to_string()],
        "widgets go, the link stays"
    );
    assert!(out.len() < input.len(), "flattening must not grow the file");
}

/// P1, the core data-preservation claim: the appearance stream that showed the
/// value is now painted by the page, unmodified, at the widget's `/Rect`.
#[test]
fn filled_value_is_burned_into_the_page_content() {
    let input = flattenable();
    let before = View::of(&input);

    // The exact bytes the text field's appearance painted, read off the input.
    let original_ap = before
        .all_stream_bytes()
        .into_iter()
        .find(|bytes| contains(bytes, FILLED_TEXT.as_bytes()))
        .expect("the fixture's appearance stream paints the value");

    let out = optimize_with_options(&input, flatten());
    let view = View::of(&out);
    let content = view.page_content();

    // Every burn is a `Do` of an XObject the page's resources bind...
    let bound: Vec<(String, ObjectId)> = view.page_xobjects();
    assert_eq!(
        bound.len(),
        3,
        "text field, checked checkbox and the selected radio button burn; \
         the empty field and the unselected radio paint nothing: {bound:?}"
    );
    let mut painted = HashSet::new();
    for op in Content::decode(&content).unwrap().operations {
        if op.operator == "Do" {
            painted.insert(String::from_utf8_lossy(op.operands[0].as_name().unwrap()).into_owned());
        }
    }
    for (name, _) in &bound {
        assert!(painted.contains(name), "{name} is bound but never painted");
    }

    // ...and the object it binds is the ORIGINAL appearance stream: the value
    // is preserved because the bytes that drew it were not regenerated.
    let burned: Vec<Vec<u8>> = bound
        .iter()
        .map(|(_, id)| match view.0.get_object(*id).unwrap() {
            Object::Stream(stream) => stream.decompressed_content().unwrap(),
            other => panic!("a burned appearance must be a stream, got {other:?}"),
        })
        .collect();
    assert!(
        burned.contains(&original_ap),
        "the text field's appearance stream must survive byte-identical"
    );
    assert!(
        burned
            .iter()
            .any(|bytes| contains(bytes, b"(X) Tj") || contains(bytes, b"(X)Tj")),
        "the checked checkbox's on-appearance must be among the burns"
    );
    assert!(
        burned.iter().any(|bytes| contains(bytes, b"(o)")),
        "the selected radio button's appearance must be among the burns"
    );
}

/// The value is now reachable by ordinary content-stream text extraction,
/// which never looked inside annotation appearances.
#[test]
fn burned_value_is_reachable_from_the_page_content() {
    let out = optimize_with_options(&flattenable(), flatten());
    let view = View::of(&out);
    let content = view.page_content();

    let mut reachable = false;
    for op in Content::decode(&content).unwrap().operations {
        if op.operator != "Do" {
            continue;
        }
        let name = op.operands[0].as_name().unwrap().to_vec();
        for (bound, id) in view.page_xobjects() {
            if bound.as_bytes() != name {
                continue;
            }
            if let Ok(Object::Stream(stream)) = view.0.get_object(id) {
                if contains(
                    &stream.decompressed_content().unwrap(),
                    FILLED_TEXT.as_bytes(),
                ) {
                    reachable = true;
                }
            }
        }
    }
    assert!(
        reachable,
        "'{FILLED_TEXT}' must be painted by an XObject the page content invokes"
    );
}

/// The `cm` before each `Do` must be matrix **A** of ISO 32000-1 12.5.5. The
/// text field's `/BBox` is 160x14 at the origin with no `/Matrix`, and its
/// `/Rect` is (72, 700)-(232, 714) — the same size — so A is a pure
/// translation to the rectangle's lower-left corner.
#[test]
fn burn_matrix_places_the_appearance_at_the_widget_rect() {
    let out = optimize_with_options(&flattenable(), flatten());
    let view = View::of(&out);

    let text_name = view
        .page_xobjects()
        .into_iter()
        .find(|(_, id)| match view.0.get_object(*id) {
            Ok(Object::Stream(stream)) => contains(
                &stream.decompressed_content().unwrap(),
                FILLED_TEXT.as_bytes(),
            ),
            _ => false,
        })
        .expect("the text field's appearance is bound")
        .0;

    let ops = Content::decode(&view.page_content()).unwrap().operations;
    let index = ops
        .iter()
        .position(|op| {
            op.operator == "Do" && op.operands[0].as_name().ok() == Some(text_name.as_bytes())
        })
        .expect("the text field is painted");
    let cm = &ops[index - 1];
    assert_eq!(cm.operator, "cm");
    let values: Vec<f32> = cm
        .operands
        .iter()
        .map(|o| match o {
            Object::Integer(i) => *i as f32,
            Object::Real(r) => *r,
            other => panic!("non-numeric cm operand {other:?}"),
        })
        .collect();
    assert_eq!(values, vec![1.0, 0.0, 0.0, 1.0, 72.0, 700.0]);
    assert_eq!(ops[index - 2].operator, "q");
    assert_eq!(ops[index + 1].operator, "Q");
}

/// The page's own content leaves a translated CTM behind (`1 0 0 1 20 20 cm`
/// outside any q/Q). Without the prepended `q` / appended `Q` the burns would
/// land 20 points off in both axes; this pins that they do not.
#[test]
fn splice_restores_the_initial_ctm_before_painting() {
    let out = optimize_with_options(&flattenable(), flatten());
    let ops = Content::decode(&View::of(&out).page_content())
        .unwrap()
        .operations;
    let first_do = ops.iter().position(|op| op.operator == "Do").unwrap();
    let mut depth = 0i64;
    let mut saw_reset = false;
    for op in &ops[..first_do] {
        match op.operator.as_str() {
            "q" => depth += 1,
            "Q" => {
                depth -= 1;
                if depth == 0 {
                    saw_reset = true;
                }
            }
            _ => {}
        }
    }
    assert!(
        saw_reset,
        "the page's own content must be closed back to the base graphics state \
         before the first burn"
    );
    assert_eq!(depth, 1, "the burn runs inside its own q");
}

/// `/Contents` behind an indirect reference to an array: the splice must
/// unwrap it, not nest a reference-to-an-array inside the new array, which
/// would silently drop the page's own drawing.
#[test]
fn splices_into_an_indirect_contents_array() {
    let input = build(&Fixture {
        indirect_contents_array: true,
        ..Fixture::default()
    });
    let out = optimize_with_options(&input, flatten());
    let view = View::of(&out);
    let content = view.page_content();
    assert!(
        contains(&content, b"(Form) Tj"),
        "the page's own drawing must survive the splice"
    );
    assert_eq!(
        Content::decode(&content)
            .unwrap()
            .operations
            .iter()
            .filter(|op| op.operator == "Do")
            .count(),
        3,
        "and all three burns must be there"
    );
}

#[test]
fn flatten_is_idempotent() {
    let once = optimize_with_options(&flattenable(), flatten());
    let twice = optimize_with_options(&once, flatten());
    assert_eq!(once, twice, "a second flatten must be a no-op");
}

/// The 1.58 MB case: a static XFA whose data the AcroForm mirrors.
#[test]
fn static_xfa_with_mirrored_data_flattens_and_drops_the_packets() {
    let input = build(&Fixture {
        xfa_datasets: Some(
            "<xfa:datasets xmlns:xfa=\"http://www.xfa.org/schema/xfa-data/1.0/\">\
             <xfa:data><form1><name>Ada Lovelace</name><agree>Yes</agree>\
             <comment/></form1></xfa:data></xfa:datasets>"
                .to_string(),
        ),
        ..Fixture::default()
    });
    let out = optimize_with_options(&input, flatten());
    let view = View::of(&out);
    assert!(!view.catalog().has(b"AcroForm"));
    assert!(
        !view
            .all_stream_bytes()
            .iter()
            .any(|bytes| contains(bytes, b"xfa:datasets")),
        "the XFA packets must be gone"
    );
    assert!(
        view.page_xobjects().iter().any(
            |(_, id)| matches!(view.0.get_object(*id), Ok(Object::Stream(s))
                if contains(&s.decompressed_content().unwrap(), FILLED_TEXT.as_bytes()))
        ),
        "and the value it mirrored must still be painted"
    );
}

// -- declines ---------------------------------------------------------------

/// Every decline must produce exactly the bytes the flag-off run produces:
/// the document is optimized as if `--flatten-forms` had never been passed.
#[track_caller]
fn assert_declines(fixture: &Fixture, why: &str) {
    let input = build(fixture);
    let flattened = optimize_with_options(&input, flatten());
    let untouched = optimize_with_options(&input, plain());
    assert_eq!(flattened, untouched, "must decline: {why}");
    assert!(
        View::of(&flattened).catalog().has(b"AcroForm"),
        "a declined document keeps its form: {why}"
    );
}

#[test]
fn declines_a_value_with_no_appearance_to_burn() {
    // D9 — the gate. The field says "Ada Lovelace" and nothing on the page
    // ever drew it, so there is no way to keep it.
    assert_declines(
        &Fixture {
            text_field_without_appearance: true,
            ..Fixture::default()
        },
        "filled field with no /AP",
    );
}

#[test]
fn declines_a_hidden_field_that_holds_a_value() {
    // D9, hidden branch — the value is data even though no ink shows it.
    assert_declines(
        &Fixture {
            hide_text_field: true,
            ..Fixture::default()
        },
        "Hidden widget over a filled field",
    );
}

#[test]
fn declines_need_appearances_over_a_filled_form() {
    // D5 — the reader was told to regenerate appearances; the stored ones may
    // not be what it would have shown.
    assert_declines(
        &Fixture {
            need_appearances: true,
            ..Fixture::default()
        },
        "/NeedAppearances true with values present",
    );
}

#[test]
fn declines_a_dynamic_xfa_form() {
    // D3 — the pages are a placeholder; picamatl does not render XFA.
    assert_declines(
        &Fixture {
            needs_rendering: true,
            xfa_datasets: Some("<xfa:datasets><data/></xfa:datasets>".to_string()),
            ..Fixture::default()
        },
        "/NeedsRendering true",
    );
}

#[test]
fn declines_xfa_data_the_acroform_does_not_mirror() {
    // D4 — `nickname` exists only in the XML. Dropping the packets would drop
    // it, and no appearance anywhere shows it.
    assert_declines(
        &Fixture {
            xfa_datasets: Some(
                "<xfa:datasets><xfa:data><form1><name>Ada Lovelace</name>\
                 <nickname>Countess</nickname></form1></xfa:data></xfa:datasets>"
                    .to_string(),
            ),
            ..Fixture::default()
        },
        "an XFA leaf with no matching AcroForm value",
    );
}

#[test]
fn declines_xfa_data_that_disagrees_with_the_acroform() {
    // D4 — same field name, different text: the XML is authoritative for a
    // reader that renders XFA, so the two are not interchangeable.
    assert_declines(
        &Fixture {
            xfa_datasets: Some(
                "<xfa:datasets><xfa:data><form1><name>Grace Hopper</name>\
                 </form1></xfa:data></xfa:datasets>"
                    .to_string(),
            ),
            ..Fixture::default()
        },
        "an XFA leaf whose text differs from the mirrored /V",
    );
}

#[test]
fn declines_a_signature_field_holding_a_signature() {
    // D6.
    assert_declines(
        &Fixture {
            signed_field: true,
            ..Fixture::default()
        },
        "/FT /Sig with a signature dictionary",
    );
}

#[test]
fn declines_an_optional_content_widget() {
    // D7 — burning it would make a conditional thing unconditional.
    assert_declines(
        &Fixture {
            optional_content: true,
            ..Fixture::default()
        },
        "/OC on a widget",
    );
}

#[test]
fn declines_a_state_appearance_with_no_as() {
    // D10 — which state was showing is not something to guess.
    assert_declines(
        &Fixture {
            drop_appearance_state: true,
            ..Fixture::default()
        },
        "/AP /N subdictionary with no /AS",
    );
}

#[test]
fn declines_an_appearance_that_leans_on_the_acroform_resources() {
    // D15 — the appearance says `/Helv 9 Tf` and carries no `/Resources`, so
    // it was resolving `/Helv` against `/AcroForm /DR`. Burning it after the
    // AcroForm is gone would paint nothing where the value used to be, and a
    // `Do` on a name with no font is skipped silently.
    assert_declines(
        &Fixture {
            appearance_without_dr: true,
            ..Fixture::default()
        },
        "appearance with no /Resources of its own",
    );
}

#[test]
fn declines_unbalanced_page_content() {
    // D13 — one extra `Q` would pop past the `q` the splice prepends.
    assert_declines(
        &Fixture {
            unbalanced_content: true,
            ..Fixture::default()
        },
        "page content that pops past its base graphics state",
    );
}

#[test]
fn a_document_with_no_form_is_byte_identical_with_and_without_the_flag() {
    // D2 — the flag must be inert on the 99% of PDFs that are not forms.
    let sample =
        std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample.pdf"))
            .expect("fixtures/sample.pdf");
    assert_eq!(
        optimize_with_options(&sample, flatten()),
        optimize_with_options(&sample, plain()),
    );
}

// -- render fidelity --------------------------------------------------------

/// Renders the original (annotations and all — what a viewer shows) and the
/// flattened output, and requires every page to be pixel-identical.
#[test]
fn flatten_render_is_pixel_identical() {
    let Some(gs) = ghostscript() else {
        eprintln!("skipping: `gs` not on PATH");
        return;
    };
    let dir = std::env::temp_dir().join(format!("picamatl-forms-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let input = flattenable();
    let out = optimize_with_options(&input, flatten());
    let original = dir.join("original.pdf");
    let flattened = dir.join("flattened.pdf");
    std::fs::write(&original, &input).unwrap();
    std::fs::write(&flattened, &out).unwrap();

    let a = render(&gs, &original, &dir.join("a.pgm"));
    let b = render(&gs, &flattened, &dir.join("b.pgm"));
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(a.len(), b.len(), "renders must have the same geometry");
    let differing = a
        .iter()
        .zip(&b)
        .filter(|(x, y)| x.abs_diff(**y) > 0)
        .count();
    assert_eq!(
        differing,
        0,
        "flattening must not move a single pixel ({differing} of {} differ)",
        a.len()
    );
}

fn ghostscript() -> Option<String> {
    for name in ["gs", "gswin64c"] {
        if Command::new(name).arg("--version").output().is_ok() {
            return Some(name.to_string());
        }
    }
    None
}

fn render(gs: &str, pdf: &std::path::Path, out: &std::path::Path) -> Vec<u8> {
    let status = Command::new(gs)
        .args([
            "-q",
            "-dNOPAUSE",
            "-dBATCH",
            "-sDEVICE=pgmraw",
            "-r150",
            "-dTextAlphaBits=1",
            "-dGraphicsAlphaBits=1",
        ])
        .arg(format!("-sOutputFile={}", out.display()))
        .arg(pdf)
        .status()
        .expect("gs runs");
    assert!(status.success(), "gs failed on {}", pdf.display());
    std::fs::read(out).expect("gs wrote a raster")
}

// -- multi-page and inherited resources ------------------------------------

/// Two pages, a filled widget on each, and **no** `/Resources` on either page
/// — they inherit one dictionary from the `/Pages` node. Exercises the shared
/// `q` stream, globally-unique burn names in a shared resource dictionary, and
/// per-page splicing.
fn build_two_pages() -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });

    let mut page_ids = Vec::new();
    let mut fields = Vec::new();
    for (index, label) in ["Ada Lovelace", "Grace Hopper"].iter().enumerate() {
        let ap = appearance(&mut doc, font_id, 160.0, 14.0, label);
        let widget = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Tx",
            "T" => utf16(&format!("name{index}")),
            "V" => utf16(label),
            "F" => 4,
            "Rect" => vec![72.into(), 700.into(), 232.into(), 714.into()],
            "AP" => dictionary! { "N" => ap },
        });
        fields.push(Object::Reference(widget));
        let content = doc.add_object(Stream::new(
            dictionary! {},
            format!("q 0 g BT /Helv 12 Tf 40 740 Td (Page {index}) Tj ET Q\n").into_bytes(),
        ));
        page_ids.push(Object::Reference(doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content,
            "Annots" => vec![Object::Reference(widget)],
        })));
    }

    doc.set_object(
        pages_id,
        dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids,
            "Count" => 2,
            // Inherited by both pages, and shared: the burn names must not
            // collide across pages.
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! { "Font" => dictionary! { "Helv" => font_id } },
        },
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
        "AcroForm" => dictionary! { "Fields" => fields },
    });
    doc.trailer.set("Root", catalog_id);
    doc.compress();

    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

#[test]
fn burns_on_several_pages_sharing_one_inherited_resource_dictionary() {
    let out = optimize_with_options(&build_two_pages(), flatten());
    let view = View::of(&out);
    assert!(!view.catalog().has(b"AcroForm"));

    let pages: Vec<ObjectId> = view.0.get_pages().values().copied().collect();
    assert_eq!(pages.len(), 2);
    let mut painted_labels = Vec::new();
    for (index, &page_id) in pages.iter().enumerate() {
        let content = view.0.get_page_content(page_id);
        assert!(
            contains(&content, format!("(Page {index}) Tj").as_bytes()),
            "page {index} keeps its own drawing"
        );
        let names: Vec<Vec<u8>> = Content::decode(&content)
            .unwrap()
            .operations
            .iter()
            .filter(|op| op.operator == "Do")
            .map(|op| op.operands[0].as_name().unwrap().to_vec())
            .collect();
        assert_eq!(names.len(), 1, "one burn per page");

        // The name resolves through the INHERITED resource dictionary.
        let page = view.0.get_object(page_id).unwrap().as_dict().unwrap();
        assert!(
            !page.has(b"Resources"),
            "the page must still inherit its resources, not gain a private copy"
        );
        let parent = match page.get(b"Parent").unwrap() {
            Object::Reference(id) => view.0.get_object(*id).unwrap().as_dict().unwrap(),
            other => other.as_dict().unwrap(),
        };
        let resources = match parent.get(b"Resources").unwrap() {
            Object::Reference(id) => view.0.get_object(*id).unwrap(),
            other => other,
        };
        let xobjects = match resources.as_dict().unwrap().get(b"XObject").unwrap() {
            Object::Reference(id) => view.0.get_object(*id).unwrap(),
            other => other,
        };
        let target = match xobjects.as_dict().unwrap().get(&names[0]).unwrap() {
            Object::Reference(id) => view.0.get_object(*id).unwrap(),
            other => other,
        };
        let Object::Stream(stream) = target else {
            panic!("a burn must resolve to a stream");
        };
        painted_labels.push(stream.decompressed_content().unwrap());
    }
    assert!(
        painted_labels[0] != painted_labels[1],
        "distinct burn targets"
    );
    assert!(contains(&painted_labels[0], b"Ada Lovelace"));
    assert!(contains(&painted_labels[1], b"Grace Hopper"));
}

// -- committed fixtures -----------------------------------------------------

/// Writes the redistributable fixtures used by `scripts/forms-verify.py` and
/// the branch report. Fully synthetic, so they can ship in the repo:
///
/// ```sh
/// cargo test --test forms_flatten -- --ignored
/// ```
#[test]
#[ignore = "regenerates committed fixtures"]
fn generate_fixtures() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/forms");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, fixture) in [
        ("filled-acroform.pdf", Fixture::default()),
        (
            "filled-acroform-static-xfa.pdf",
            Fixture {
                xfa_datasets: Some(
                    "<xfa:datasets xmlns:xfa=\"http://www.xfa.org/schema/xfa-data/1.0/\">\
                     <xfa:data><form1><name>Ada Lovelace</name><agree>Yes</agree>\
                     <comment/></form1></xfa:data></xfa:datasets>"
                        .to_string(),
                ),
                ..Fixture::default()
            },
        ),
        (
            "dynamic-xfa.pdf",
            Fixture {
                needs_rendering: true,
                xfa_datasets: Some(
                    "<xfa:datasets><xfa:data><form1><name>Ada Lovelace</name>\
                     </form1></xfa:data></xfa:datasets>"
                        .to_string(),
                ),
                ..Fixture::default()
            },
        ),
        (
            "unappearanced-value.pdf",
            Fixture {
                text_field_without_appearance: true,
                ..Fixture::default()
            },
        ),
    ] {
        std::fs::write(dir.join(name), build(&fixture)).unwrap();
    }
}

// -- antialiased render fidelity --------------------------------------------

/// Flatten only: every other pass off, so a render difference here can only
/// have come from the flatten path.
fn flatten_only() -> OptimizeOptions {
    OptimizeOptions::default()
        .with_flatten_forms(true)
        .with_target_dpi(0.0)
        .with_downsample_flate_images(false)
        .with_subset_fonts(false)
        .with_pack_object_streams(false)
}

fn pdftoppm() -> Option<String> {
    Command::new("pdftoppm")
        .arg("-v")
        .output()
        .ok()
        .map(|_| "pdftoppm".to_string())
}

/// Render every page to a greyscale raster, antialiasing on.
///
/// Poppler on purpose, rather than the `gs` this file already shells out to,
/// for two measured reasons.
///
/// `render` above pins `-dTextAlphaBits=1 -dGraphicsAlphaBits=1`, quantising
/// every edge to hard black or white. That only sees a geometry change once it
/// pushes an edge clean across a pixel centre; a smaller shift moves edge
/// *coverage* and nothing else. Offsetting the burn `cm` in `burn_operators`
/// and rerunning both tests: at 0.02 pt both fail, at 0.005 pt only this one
/// does, at 0.001 pt neither. Roughly four times the resolution on the burn
/// geometry, for the same renders.
///
/// Ghostscript is also blind to page-box drift specifically, at any alpha
/// setting, because it rounds the box to whole device pixels before fitting
/// anything to the grid — it even reports a different raster width for the two
/// files. Poppler grid-fits against the unrounded box. Measured on
/// `wiki-pdf.pdf` with only `/MediaBox 841.91998` nudged to `841.92`: poppler
/// 16901 differing subpixels and 3562 outright black/white flips,
/// `gs -dTextAlphaBits=4` exactly 0.
fn render_pages(
    tool: &str,
    pdf: &std::path::Path,
    dir: &std::path::Path,
    dpi: u32,
) -> Vec<Vec<u8>> {
    let status = Command::new(tool)
        .args(["-r", &dpi.to_string(), "-gray"])
        .arg(pdf)
        .arg(dir.join("p"))
        .status()
        .expect("pdftoppm runs");
    assert!(status.success(), "pdftoppm failed on {}", pdf.display());

    let mut pages: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "pgm"))
        .collect();
    pages.sort();
    assert!(
        !pages.is_empty(),
        "pdftoppm wrote no pages for {}",
        pdf.display()
    );

    let rasters = pages.iter().map(|p| std::fs::read(p).unwrap()).collect();
    for page in pages {
        let _ = std::fs::remove_file(page);
    }
    rasters
}

/// Both DPIs matter: a fractional offset can land on the pixel grid at one
/// scale and off it at another, so a single-resolution check can pass straight
/// over a real geometry change.
fn assert_flatten_moves_no_subpixel(tool: &str, label: &str, input: &[u8], flattened: &[u8]) {
    let dir = std::env::temp_dir().join(format!("picamatl-render-{}-{label}", std::process::id()));
    let (before, after) = (dir.join("before"), dir.join("after"));
    std::fs::create_dir_all(&before).unwrap();
    std::fs::create_dir_all(&after).unwrap();
    let (a_pdf, b_pdf) = (dir.join("a.pdf"), dir.join("b.pdf"));
    std::fs::write(&a_pdf, input).unwrap();
    std::fs::write(&b_pdf, flattened).unwrap();

    for dpi in [100, 150] {
        let a = render_pages(tool, &a_pdf, &before, dpi);
        let b = render_pages(tool, &b_pdf, &after, dpi);
        assert_eq!(
            a.len(),
            b.len(),
            "{label}: page count must survive flattening"
        );

        for (n, (x, y)) in a.iter().zip(&b).enumerate() {
            assert_eq!(
                x.len(),
                y.len(),
                "{label}: page {} geometry changed at {dpi} dpi",
                n + 1
            );
            let differing = x.iter().zip(y).filter(|(p, q)| p != q).count();
            assert_eq!(
                differing,
                0,
                "{label}: page {} differs at {dpi} dpi ({differing} of {} subpixels)",
                n + 1,
                x.len()
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The synthetic fixtures, whose widgets paint real ink at a known `/Rect`.
/// This is the test that pins the burn *geometry*: matrix **A**, the `q`/`Q`
/// nesting around the splice, and the stacking of each burn over the page's
/// own content. Shifting the emitted `cm` by 0.02 pt — a fifth of a pixel at
/// 100 dpi, far below anything a human would call a difference — fails it.
#[test]
fn flatten_render_moves_no_subpixel_on_painted_widgets() {
    let Some(tool) = pdftoppm() else {
        eprintln!("skipping: `pdftoppm` not on PATH");
        return;
    };
    for (label, input) in [("one-page", flattenable()), ("two-page", build_two_pages())] {
        let out = optimize_with_options(&input, flatten());
        assert_ne!(out, input, "{label}: the fixture must exercise flattening");
        assert_flatten_moves_no_subpixel(&tool, label, &input, &out);
    }
}

/// `corpus-expanded/irs-w2.pdf` — 11 pages, a static XFA packet over a filled
/// AcroForm, and a decorative flag raster in the page-1 header whose diagonal
/// stripe boundaries are the most antialiasing-sensitive ink in the corpus.
///
/// Complementary to the fixture test above rather than a stronger version of
/// it. This form's widgets paint nothing, so the burns cannot move a pixel by
/// construction and the burn geometry is *not* what is under test here. What
/// is, is everything else flattening does to a real document — the spliced
/// page content, the rebound resource dictionaries, the dropped `/AcroForm`
/// and annotation arrays, the page tree it leaves behind. The header raster is
/// the sensitive witness: it is the ink most likely to shift if any of that
/// quietly changed the page's geometry.
#[test]
fn flatten_render_moves_no_subpixel_on_irs_w2() {
    let Some(tool) = pdftoppm() else {
        eprintln!("skipping: `pdftoppm` not on PATH");
        return;
    };
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus-expanded/irs-w2.pdf");
    let Ok(input) = std::fs::read(&fixture) else {
        eprintln!("skipping: {} not present", fixture.display());
        return;
    };
    let out = optimize_with_options(&input, flatten_only());
    assert_ne!(out, input, "the fixture must actually exercise flattening");
    assert_flatten_moves_no_subpixel(&tool, "irs-w2", &input, &out);
}
