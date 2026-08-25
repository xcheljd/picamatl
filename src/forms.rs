//! Opt-in interactive-form flattening (`OptimizeOptions::flatten_forms`).
//!
//! Turns an interactive AcroForm document into a static one: every widget
//! annotation that draws ink is painted into its page's content stream at the
//! position ISO 32000-1 12.5.5 says the viewer painted it, and then the whole
//! form layer — `/AcroForm`, the field tree, the XFA packet set, the widget
//! annotations — is removed. `prune_objects()` collects the remains.
//!
//! The contract this module exists to keep is **data preservation**: a field's
//! value survives either because its appearance stream (the thing that *shows*
//! the value) is now page content, or because the field has no value to lose.
//! When neither holds — a dynamic XFA form, a value with no appearance to
//! burn, a hidden field that carries data — the whole document is declined and
//! `try_optimize` proceeds exactly as if the flag were off. See
//! `docs/FORMS-PLAN.md` for the decline table (D1..D13) referenced by the
//! comments below.
//!
//! Everything here is planning against an immutable `&Document`; `apply` is a
//! separate, non-failing pass over the plan.

use std::collections::{HashMap, HashSet};

use lopdf::content::Content;
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};

use crate::{fonts, resolve};

/// Depth bound for the field-tree walk and the `/Parent` climb.
const MAX_DEPTH: usize = 32;

/// Geometric degeneracy threshold for `/BBox` and `/Rect` extents, in points.
/// Not a tolerance for "small": a box thinner than this maps through a
/// scale factor we refuse to compute (D12).
const MIN_EXTENT: f64 = 1e-6;

/// One appearance stream to paint into a page.
struct Burn {
    /// Resource name bound to `ap_id` in the page's `/XObject` dictionary.
    /// Globally unique across the document, so mutating a shared or inherited
    /// resource dictionary can never collide with an existing name.
    name: Vec<u8>,
    /// The appearance stream object, used unmodified.
    ap_id: ObjectId,
    /// Matrix **A** of ISO 32000-1 12.5.5 (`/BBox` bounds -> `/Rect`).
    matrix: [f64; 6],
}

struct PagePlan {
    page_id: ObjectId,
    burns: Vec<Burn>,
    /// Widget annotations to drop from this page's `/Annots`.
    drop_annots: HashSet<ObjectId>,
    /// The page has a `/Contents` entry, so the splice needs the `q` / `Q`
    /// pair that restores the initial CTM before the widget operators.
    has_contents: bool,
}

pub(crate) struct FlattenPlan {
    pages: Vec<PagePlan>,
    /// Every widget annotation removed anywhere, for the structure-tree
    /// `/OBJR` cleanup.
    removed_widgets: HashSet<ObjectId>,
    /// The catalog carries a `/Perms /UR3` usage-rights signature: the Reader
    /// grant for exactly the form filling being removed.
    drop_ur3: bool,
}

// -- planning ---------------------------------------------------------------

/// Plan the flattening, or return `None` to decline the document.
pub(crate) fn plan_flatten(doc: &Document) -> Option<FlattenPlan> {
    // D1 — same posture as every other structural pass.
    if doc.is_encrypted() || fonts::pdfa_blocked(doc) {
        return None;
    }
    let catalog = doc.catalog().ok()?;

    // D3 — ISO 32000-1 12.7.8: the marker for a dynamic XFA form, whose pages
    // are a placeholder the reader replaces by laying out the XFA template.
    // There is nothing static to flatten and amatl will never render XFA.
    if let Ok(needs) = catalog.get(b"NeedsRendering") {
        if matches!(resolve(doc, needs), Object::Boolean(true)) {
            return None;
        }
    }

    // D2 — no field tree, nothing to flatten. Widget annotations that are not
    // reachable from an `/AcroForm` are not guessed at either.
    let acroform = resolve(doc, catalog.get(b"AcroForm").ok()?)
        .as_dict()
        .ok()?;

    let mut scan = FieldScan::default();
    if let Ok(fields) = acroform.get(b"Fields") {
        let fields = resolve(doc, fields).as_array().ok()?;
        for field in fields {
            walk_field(doc, field, None, None, None, 0, &mut scan)?;
        }
    }

    // D5 — the reader was told to generate appearances from `/V`; the stored
    // ones may be stale or absent, and amatl has no text layout engine.
    if let Ok(need) = acroform.get(b"NeedAppearances") {
        if matches!(resolve(doc, need), Object::Boolean(true)) && scan.any_value {
            return None;
        }
    }

    // D4 — every piece of data in the XFA XML must be mirrored by an AcroForm
    // field value, or it lives only in the XML and flattening would drop it.
    if let Ok(xfa) = acroform.get(b"XFA") {
        check_xfa_mirrored(doc, resolve(doc, xfa), &scan)?;
    }

    let mut pages = Vec::new();
    // Widgets that end up painting their appearance into a page, and the
    // single-widget "fields" a page shows that `/Fields` never mentioned.
    let mut burned: HashSet<ObjectId> = HashSet::new();
    let mut orphan_valued: Vec<ObjectId> = Vec::new();
    let mut removed_widgets = HashSet::new();
    let mut next_name = 0usize;

    for (_, page_id) in doc.get_pages() {
        let page = doc.get_object(page_id).ok()?.as_dict().ok()?;
        let Ok(annots) = page.get(b"Annots") else {
            continue;
        };
        let annots = resolve(doc, annots).as_array().ok()?;

        let mut burns = Vec::new();
        let mut drop_annots = HashSet::new();
        for entry in annots {
            let object = resolve(doc, entry);
            let Ok(annot) = object.as_dict() else {
                continue;
            };
            if !matches!(annot.get(b"Subtype").map(|s| resolve(doc, s)), Ok(Object::Name(n)) if n == b"Widget")
            {
                // Links, markup, popups: not form machinery, not touched.
                continue;
            }
            // A widget we cannot name by object id is a widget we cannot
            // reliably drop from `/Annots` or clean out of the structure tree.
            let Object::Reference(annot_id) = entry else {
                return None;
            };
            let annot_id = *annot_id;

            // D7 — optional content makes visibility conditional; painting it
            // into the page would make it unconditional.
            if annot.has(b"OC") {
                return None;
            }

            // A widget the `/Fields` walk never reached is its own field: its
            // value can only be read off the annotation itself.
            if !scan.known_widgets.contains(&annot_id) {
                let value = annot.get(b"V").ok().map(|v| resolve(doc, v));
                if !value_is_empty(value) {
                    orphan_valued.push(annot_id);
                }
            }

            let appearance = match appearance_stream(doc, annot) {
                ApSel::Decline => return None,
                ApSel::None => None,
                ApSel::Stream(id) => {
                    if draws_ink(doc, id) {
                        Some(id)
                    } else {
                        None
                    }
                }
            };

            let flags = annot
                .get(b"F")
                .map(|f| resolve(doc, f))
                .and_then(|f| f.as_i64())
                .unwrap_or(0);
            let invisible = flags & 0b10 != 0 || flags & 0b10_0000 != 0; // Hidden | NoView

            if invisible {
                // D8 — a widget that is drawn on paper but not on screen (or
                // neither) cannot be expressed as unconditional page content.
                // Whether its field's value survives is settled below, by the
                // same rule as every other widget: some widget must burn it.
                if appearance.is_some() {
                    return None;
                }
            } else if let Some(ap_id) = appearance {
                // P1 — burn the appearance the viewer painted.
                let matrix = burn_matrix(doc, annot, ap_id)?;
                burns.push(Burn {
                    name: format!("AmXf{next_name}").into_bytes(),
                    ap_id,
                    matrix,
                });
                next_name += 1;
                burned.insert(annot_id);
            }
            // else: P2 — nothing drawn. Whether that is allowed is the
            // valued-field check below.

            drop_annots.insert(annot_id);
            removed_widgets.insert(annot_id);
        }

        if drop_annots.is_empty() {
            continue;
        }

        let has_contents = page.has(b"Contents");
        // D13 — the splice needs a page whose content is parseable and whose
        // graphics-state stack returns to its base level, so the `q` we
        // prepend survives to be popped by the `Q` we append.
        if !burns.is_empty() && has_contents && !content_is_spliceable(doc, page_id) {
            return None;
        }
        pages.push(PagePlan {
            page_id,
            burns,
            drop_annots,
            has_contents,
        });
    }

    // D9 / D11 — the data-preservation gate. Every field that holds a value
    // must have at least one widget whose appearance is now page content: a
    // radio group needs only its selected button, but a filled text field with
    // no appearance, a hidden one, or one whose widget is on no page at all
    // has nothing left showing its value, and the document declines.
    for widgets in scan.valued_fields.values() {
        if !widgets.iter().any(|widget| burned.contains(widget)) {
            return None;
        }
    }
    if orphan_valued.iter().any(|widget| !burned.contains(widget)) {
        return None;
    }

    let drop_ur3 = catalog
        .get(b"Perms")
        .map(|p| resolve(doc, p))
        .ok()
        .and_then(|p| p.as_dict().ok())
        .is_some_and(|perms| perms.has(b"UR3"));

    Some(FlattenPlan {
        pages,
        removed_widgets,
        drop_ur3,
    })
}

/// What the field-tree walk learned. Everything here is about *values*: the
/// module's whole job is to prove no value is silently dropped.
///
/// A value belongs to a **field**, not to a widget. A radio group is the case
/// that forces this: every button in the group inherits the group's `/V`, but
/// only the one whose `/AS` names a present state paints anything. Requiring
/// each *widget* to account for the value would decline every radio group ever
/// made; requiring each *field* to have at least one widget that burns is the
/// correct reading of "the value is still visible".
#[derive(Default)]
struct FieldScan {
    /// Field node that owns a non-empty `/V` -> the widget annotations under
    /// it. At least one of them must burn, or the document declines (D9/D11).
    valued_fields: HashMap<ObjectId, Vec<ObjectId>>,
    /// Every widget the field tree reaches, so a widget a page shows but
    /// `/Fields` never mentions can be recognized and handled on its own.
    known_widgets: HashSet<ObjectId>,
    /// Partial field name (any trailing `[n]` stripped) -> effective values.
    /// The XFA `datasets` mirror check reads this.
    values_by_name: HashMap<Vec<u8>, Vec<Option<Object>>>,
    /// Any field anywhere carries a non-empty value (feeds D5).
    any_value: bool,
}

/// Recursive `/Fields` walk with `/FT` and `/V` inheritance. Returns `None`
/// to decline the document.
fn walk_field(
    doc: &Document,
    node: &Object,
    inherited_ft: Option<&[u8]>,
    inherited_v: Option<&Object>,
    owner: Option<ObjectId>,
    depth: usize,
    scan: &mut FieldScan,
) -> Option<()> {
    if depth > MAX_DEPTH {
        return None;
    }
    let node_id = match node {
        Object::Reference(id) => Some(*id),
        _ => None,
    };
    let Ok(dict) = resolve(doc, node).as_dict() else {
        // A field entry that is not a dictionary tells us nothing about the
        // values under it.
        return None;
    };

    let own_ft = dict
        .get(b"FT")
        .map(|f| resolve(doc, f))
        .ok()
        .and_then(|f| f.as_name().ok())
        .map(<[u8]>::to_vec);
    let ft: Option<&[u8]> = own_ft.as_deref().or(inherited_ft);

    let own_v = dict.get(b"V").ok().map(|v| resolve(doc, v).clone());
    let value: Option<&Object> = own_v.as_ref().or(inherited_v);

    // D6 — a form field holding a real signature. Flattening would delete it.
    if ft == Some(b"Sig".as_slice()) && matches!(value, Some(Object::Dictionary(_))) {
        return None;
    }

    let has_value = !value_is_empty(value);
    if has_value {
        scan.any_value = true;
    }
    // Whichever node last declared a non-empty `/V` owns the value for this
    // subtree; a node that declares an empty one takes the value away again.
    let owner = match own_v {
        Some(_) if has_value => Some(node_id?),
        Some(_) => None,
        None => owner,
    };
    if let Some(owner) = owner {
        scan.valued_fields.entry(owner).or_default();
    }

    if let Ok(Object::String(name, _)) = dict.get(b"T").map(|t| resolve(doc, t)) {
        scan.values_by_name
            .entry(strip_index(&text_string(name)))
            .or_default()
            .push(value.cloned());
    }

    let is_widget = matches!(dict.get(b"Subtype").map(|s| resolve(doc, s)), Ok(Object::Name(n)) if n == b"Widget");
    if is_widget {
        // A merged field/widget node, or a widget kid. Attribute it to the
        // field whose value it may be showing.
        let id = node_id?;
        scan.known_widgets.insert(id);
        if let Some(owner) = owner {
            scan.valued_fields.entry(owner).or_default().push(id);
        }
    }

    if let Ok(kids) = dict.get(b"Kids") {
        for kid in resolve(doc, kids).as_array().ok()? {
            walk_field(doc, kid, ft, value, owner, depth + 1, scan)?;
        }
    }
    Some(())
}

/// A value that cannot be lost because there is nothing there: absent, null,
/// an all-whitespace string, an empty array, or a button's `/Off` state.
fn value_is_empty(value: Option<&Object>) -> bool {
    match value {
        None | Some(Object::Null) => true,
        Some(Object::String(s, _)) => text_string(s).iter().all(u8::is_ascii_whitespace),
        Some(Object::Name(n)) => n == b"Off",
        Some(Object::Array(a)) => a.is_empty(),
        _ => false,
    }
}

/// `f1_05[0]` -> `f1_05`. XFA data nodes carry the partial name without the
/// occurrence index PDF field names append.
fn strip_index(name: &[u8]) -> Vec<u8> {
    if name.last() == Some(&b']') {
        if let Some(open) = name.iter().rposition(|&b| b == b'[') {
            if name[open + 1..name.len() - 1]
                .iter()
                .all(u8::is_ascii_digit)
            {
                return name[..open].to_vec();
            }
        }
    }
    name.to_vec()
}

/// A PDF *text string* (ISO 32000-1 7.9.2.2) as UTF-8 bytes, so it can be
/// compared against the UTF-8 an XFA packet holds. Acrobat writes field names
/// and values as UTF-16BE with a byte-order mark; PDFDocEncoded strings are
/// returned as-is, which is exact for the ASCII range and, above it, produces
/// bytes that simply will not match the XFA text — declining, not guessing.
fn text_string(bytes: &[u8]) -> Vec<u8> {
    if let Some(body) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = body
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect();
        return match char::decode_utf16(units).collect::<Result<String, _>>() {
            Ok(text) => text.into_bytes(),
            // Unpaired surrogate: hand back the raw bytes rather than invent a
            // replacement character that could accidentally compare equal.
            Err(_) => bytes.to_vec(),
        };
    }
    match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(body) => body.to_vec(),
        None => bytes.to_vec(),
    }
}

// -- XFA --------------------------------------------------------------------

/// D4: every non-empty leaf of the XFA `datasets` (and `form`) packets must be
/// mirrored by an AcroForm field value. When it is, flattening the AcroForm
/// flattens the XFA data with it; when it is not, the data lives only in the
/// XML and the document is declined.
fn check_xfa_mirrored(doc: &Document, xfa: &Object, scan: &FieldScan) -> Option<()> {
    // Only the array (packet-list) form is handled: the single-stream XDP form
    // cannot be split into packets without an XML parser we are not adding, and
    // scanning the whole XDP would read `/config` values as if they were data.
    let packets = xfa.as_array().ok()?;
    for pair in packets.chunks(2) {
        let [name, stream] = pair else { return None };
        let Ok(name) = resolve(doc, name).as_str() else {
            continue;
        };
        if name != b"datasets" && name != b"form" {
            continue;
        }
        let bytes = resolve(doc, stream)
            .as_stream()
            .ok()?
            .decompressed_content()
            .ok()?;
        for (leaf, text) in datasets_leaves(&bytes)? {
            if !mirrored(&leaf, &text, scan) {
                return None;
            }
        }
    }
    Some(())
}

/// One XFA leaf value against the AcroForm field tree: at least one field with
/// that partial name, and *every* such field carrying an equivalent value.
fn mirrored(leaf: &[u8], text: &[u8], scan: &FieldScan) -> bool {
    let Some(values) = scan.values_by_name.get(leaf) else {
        return false;
    };
    !values.is_empty()
        && values.iter().all(|v| match v {
            Some(Object::String(s, _)) => text_string(s) == text,
            // XFA writes a checkbox's off-state as `0`; PDF writes it `/Off`.
            Some(Object::Name(n)) => n == text || (n == b"Off" && text == b"0"),
            _ => false,
        })
}

/// Element name + character data for every leaf element that has non-blank
/// text, from a well-formed XFA packet. `None` on anything the scanner cannot
/// account for — an unbalanced or truncated packet declines the document
/// rather than being read past.
///
/// Deliberately not a general XML parser: it resolves no namespaces (the
/// prefix is stripped), decodes no entities (an entity-bearing value simply
/// will not match a `/V` and declines), and needs neither, because all it has
/// to answer is "does this packet carry data the AcroForm does not mirror?".
fn datasets_leaves(xml: &[u8]) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
    struct Frame {
        name: Vec<u8>,
        text: Vec<u8>,
        had_child: bool,
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < xml.len() {
        if xml[i] != b'<' {
            let start = i;
            while i < xml.len() && xml[i] != b'<' {
                i += 1;
            }
            if let Some(top) = stack.last_mut() {
                top.text.extend_from_slice(&xml[start..i]);
            }
            continue;
        }
        if xml[i..].starts_with(b"<!--") {
            i = find(xml, b"-->", i)? + 3;
        } else if xml[i..].starts_with(b"<![CDATA[") {
            let end = find(xml, b"]]>", i)?;
            if let Some(top) = stack.last_mut() {
                top.text.extend_from_slice(&xml[i + 9..end]);
            }
            i = end + 3;
        } else if xml[i..].starts_with(b"<?") || xml[i..].starts_with(b"<!") {
            i = find(xml, b">", i)? + 1;
        } else if xml[i..].starts_with(b"</") {
            let end = find(xml, b">", i)?;
            let frame = stack.pop()?;
            if !frame.had_child && !frame.text.iter().all(u8::is_ascii_whitespace) {
                out.push((frame.name, trim(&frame.text).to_vec()));
            }
            if let Some(parent) = stack.last_mut() {
                parent.had_child = true;
            }
            i = end + 1;
        } else {
            let end = tag_end(xml, i)?;
            let self_closing = xml[end - 1] == b'/';
            let name = local_name(&xml[i + 1..if self_closing { end - 1 } else { end }]);
            if self_closing {
                if let Some(parent) = stack.last_mut() {
                    parent.had_child = true;
                }
            } else {
                stack.push(Frame {
                    name,
                    text: Vec::new(),
                    had_child: false,
                });
            }
            i = end + 1;
        }
    }
    stack.is_empty().then_some(out)
}

/// End index of a start tag's `>`, skipping `>` inside quoted attribute values.
fn tag_end(xml: &[u8], from: usize) -> Option<usize> {
    let mut quote = 0u8;
    for (offset, &b) in xml[from..].iter().enumerate() {
        match (quote, b) {
            (0, b'"' | b'\'') => quote = b,
            (q, c) if q != 0 && q == c => quote = 0,
            (0, b'>') => return Some(from + offset),
            _ => {}
        }
    }
    None
}

/// `xfa:data foo="1"` -> `data`.
fn local_name(tag: &[u8]) -> Vec<u8> {
    let end = tag
        .iter()
        .position(|b| b.is_ascii_whitespace())
        .unwrap_or(tag.len());
    let name = &tag[..end];
    match name.iter().position(|&b| b == b':') {
        Some(colon) => name[colon + 1..].to_vec(),
        None => name.to_vec(),
    }
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn trim(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |p| p + 1);
    &bytes[start..end]
}

// -- appearances ------------------------------------------------------------

enum ApSel {
    /// No appearance is selected — the widget paints nothing.
    None,
    /// The selected normal-appearance form XObject.
    Stream(ObjectId),
    /// Ambiguous or malformed; decline the document.
    Decline,
}

/// Resolve `/AP /N`, honouring `/AS` when it is a state subdictionary.
fn appearance_stream(doc: &Document, annot: &Dictionary) -> ApSel {
    let Ok(ap) = annot.get(b"AP").map(|a| resolve(doc, a)) else {
        return ApSel::None;
    };
    let Ok(ap) = ap.as_dict() else {
        return ApSel::Decline;
    };
    let Ok(normal) = ap.get(b"N") else {
        return ApSel::None;
    };
    match normal {
        Object::Reference(id) => match doc.get_object(*id) {
            Ok(Object::Stream(_)) => ApSel::Stream(*id),
            // A reference to a state subdictionary: pick with `/AS`.
            Ok(Object::Dictionary(states)) => select_state(doc, annot, states),
            _ => ApSel::Decline,
        },
        Object::Dictionary(states) => select_state(doc, annot, states),
        // A directly-embedded stream has no object id to reference from the
        // page's resources; real producers never emit one.
        _ => ApSel::Decline,
    }
}

fn select_state(doc: &Document, annot: &Dictionary, states: &Dictionary) -> ApSel {
    // D10 — ISO 32000-1 12.5.5 requires `/AS` when `/N` is a subdictionary.
    // Guessing which state was showing is guessing at data.
    let Ok(Object::Name(state)) = annot.get(b"AS").map(|s| resolve(doc, s)) else {
        return ApSel::Decline;
    };
    match states.get(state) {
        // A state that is not in the dictionary paints nothing — the shape
        // every IRS XFA-foreground checkbox is in (`/AS /Off`, only `/1`
        // present).
        Err(_) => ApSel::None,
        Ok(Object::Reference(id)) => match doc.get_object(*id) {
            Ok(Object::Stream(_)) => ApSel::Stream(*id),
            _ => ApSel::Decline,
        },
        Ok(_) => ApSel::Decline,
    }
}

/// Whether an appearance stream contains any operator at all. An appearance
/// whose content stream is empty is what a producer leaves behind when a
/// widget exists only to carry a name; painting it would be a no-op and
/// dropping it changes nothing on the page.
fn draws_ink(doc: &Document, ap_id: ObjectId) -> bool {
    let Ok(Object::Stream(stream)) = doc.get_object(ap_id) else {
        return true;
    };
    let Ok(content) = stream.decompressed_content() else {
        return true;
    };
    match Content::decode(&content) {
        Ok(parsed) => !parsed.operations.is_empty(),
        Err(_) => true,
    }
}

/// Matrix **A** of ISO 32000-1 12.5.5: the four `/BBox` corners are mapped
/// through the form's `/Matrix`, the axis-aligned bounds of the result are
/// taken, and those bounds are scaled and translated onto the widget's
/// (normalized) `/Rect`. The `Do` operator concatenates `/Matrix` itself, so
/// emitting A as the `cm` gives the spec's `AA = Matrix x A`.
fn burn_matrix(doc: &Document, annot: &Dictionary, ap_id: ObjectId) -> Option<[f64; 6]> {
    let Ok(Object::Stream(stream)) = doc.get_object(ap_id) else {
        return None;
    };
    // An appearance must be a form XObject; an image would need its own
    // placement conventions we are not inventing.
    if !matches!(stream.dict.get(b"Subtype").map(|s| resolve(doc, s)), Ok(Object::Name(n)) if n == b"Form")
    {
        return None;
    }
    let bbox = numbers(doc, stream.dict.get(b"BBox").ok()?, 4)?;
    let matrix = match stream.dict.get(b"Matrix") {
        Ok(m) => numbers(doc, m, 6)?,
        Err(_) => vec![1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
    };
    let rect = numbers(doc, annot.get(b"Rect").ok()?, 4)?;

    let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
    let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for (x, y) in [
        (bbox[0], bbox[1]),
        (bbox[2], bbox[1]),
        (bbox[2], bbox[3]),
        (bbox[0], bbox[3]),
    ] {
        let tx = matrix[0] * x + matrix[2] * y + matrix[4];
        let ty = matrix[1] * x + matrix[3] * y + matrix[5];
        min_x = min_x.min(tx);
        min_y = min_y.min(ty);
        max_x = max_x.max(tx);
        max_y = max_y.max(ty);
    }
    let (bw, bh) = (max_x - min_x, max_y - min_y);
    let (rx0, rx1) = (rect[0].min(rect[2]), rect[0].max(rect[2]));
    let (ry0, ry1) = (rect[1].min(rect[3]), rect[1].max(rect[3]));
    let (rw, rh) = (rx1 - rx0, ry1 - ry0);

    // D12 — a degenerate box has no mapping onto the rectangle.
    if !(bw > MIN_EXTENT && bh > MIN_EXTENT && rw > MIN_EXTENT && rh > MIN_EXTENT) {
        return None;
    }
    let (sx, sy) = (rw / bw, rh / bh);
    if ![sx, sy].iter().all(|v| v.is_finite()) {
        return None;
    }
    Some([sx, 0.0, 0.0, sy, rx0 - min_x * sx, ry0 - min_y * sy])
}

/// Exactly `count` numeric entries from an array object.
fn numbers(doc: &Document, object: &Object, count: usize) -> Option<Vec<f64>> {
    let array = resolve(doc, object).as_array().ok()?;
    if array.len() != count {
        return None;
    }
    array
        .iter()
        .map(|v| match resolve(doc, v) {
            Object::Integer(i) => Some(*i as f64),
            Object::Real(r) => Some(f64::from(*r)),
            _ => None,
        })
        .collect()
}

// -- content splicing -------------------------------------------------------

/// D13: the page's content must parse, must not contain an inline image (a
/// naive `q`/`Q` scan would miscount the binary payload, and `Content::decode`
/// hands `BI` back as an operator whose operands we do not model), and must
/// leave the graphics-state stack exactly as it found it — otherwise the `q`
/// prepended before it is not the one the appended `Q` pops.
fn content_is_spliceable(doc: &Document, page_id: ObjectId) -> bool {
    let bytes = doc.get_page_content(page_id);
    let Ok(parsed) = Content::decode_strict(&bytes) else {
        return false;
    };
    let mut depth = 0i64;
    for op in &parsed.operations {
        match op.operator.as_str() {
            "BI" => return false,
            "q" => depth += 1,
            "Q" => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// `q <A> cm /Name Do Q` for each burn, prefixed by the `Q` that closes the
/// `q` prepended before the page's own content.
fn burn_operators(plan: &PagePlan) -> Vec<u8> {
    let mut out = Vec::new();
    if plan.has_contents {
        out.extend_from_slice(b"Q\n");
    }
    for burn in &plan.burns {
        out.extend_from_slice(b"q ");
        for value in burn.matrix {
            out.extend_from_slice(format_number(value).as_bytes());
            out.push(b' ');
        }
        out.extend_from_slice(b"cm /");
        out.extend_from_slice(&burn.name);
        out.extend_from_slice(b" Do Q\n");
    }
    out
}

/// Rust's `{}` for `f64` is the shortest representation that round-trips, but
/// it can print an exponent, which PDF numbers must not have.
fn format_number(value: f64) -> String {
    let text = format!("{value}");
    if text.contains(['e', 'E']) {
        format!("{value:.6}")
    } else {
        text
    }
}

// -- applying ---------------------------------------------------------------

/// Apply a plan. Infallible by construction: every decision was made during
/// planning, and a step that cannot find what it planned for simply does
/// nothing (the object graph only ever loses form machinery).
pub(crate) fn apply_flatten(doc: &mut Document, plan: FlattenPlan) {
    // One shared `q` stream for every spliced page; `dedup_streams` would
    // merge per-page copies anyway, so make one and reference it.
    let mut save_state_id: Option<ObjectId> = None;

    for page in &plan.pages {
        if !page.burns.is_empty() {
            let bindings: Vec<(Vec<u8>, ObjectId)> = page
                .burns
                .iter()
                .map(|b| (b.name.clone(), b.ap_id))
                .collect();
            bind_xobjects(doc, page.page_id, &bindings);

            let ops_id = doc.add_object(Stream::new(dictionary! {}, burn_operators(page)));
            let prefix = if page.has_contents {
                Some(*save_state_id.get_or_insert_with(|| {
                    doc.add_object(Stream::new(dictionary! {}, b"q\n".to_vec()))
                }))
            } else {
                None
            };
            splice_contents(doc, page.page_id, prefix, ops_id);
        }
        drop_widget_annots(doc, page.page_id, &page.drop_annots);
    }

    // The structure tree keeps object references to annotations; a removed
    // widget must not leave a dangling `/OBJR` behind.
    drop_objr_references(doc, &plan.removed_widgets);

    if let Ok(catalog) = doc.catalog_mut() {
        catalog.remove(b"AcroForm");
        catalog.remove(b"NeedsRendering");
        if plan.drop_ur3 {
            // The Reader usage-rights signature grants exactly the local form
            // filling and saving this pass removes (and any amatl rewrite has
            // already invalidated it). `/DocMDP` is left alone.
            let perms_id = match catalog.get(b"Perms") {
                Ok(Object::Reference(id)) => Some(*id),
                _ => None,
            };
            match perms_id {
                Some(id) => {
                    if let Ok(Object::Dictionary(perms)) = doc.get_object_mut(id) {
                        perms.remove(b"UR3");
                        if perms.is_empty() {
                            if let Ok(catalog) = doc.catalog_mut() {
                                catalog.remove(b"Perms");
                            }
                        }
                    }
                }
                None => {
                    if let Ok(Object::Dictionary(perms)) = catalog.get_mut(b"Perms") {
                        perms.remove(b"UR3");
                        if perms.is_empty() {
                            catalog.remove(b"Perms");
                        }
                    }
                }
            }
        }
    }
}

/// Bind appearance streams into the page's `/XObject` resources. The resource
/// dictionary is mutated wherever it lives — on the page, inherited from a
/// `/Pages` node, or shared by reference between pages — which is safe only
/// because the names are unique across the whole document, so a page that
/// gains a name it never draws renders identically.
fn bind_xobjects(doc: &mut Document, page_id: ObjectId, bindings: &[(Vec<u8>, ObjectId)]) {
    enum Home {
        /// `/Resources` is an indirect object.
        Indirect(ObjectId),
        /// `/Resources` is inline in this object's dictionary.
        Inline(ObjectId),
    }

    // Phase 1, immutable: find the resource dictionary and its `/XObject`.
    let mut home = None;
    let mut current = page_id;
    for _ in 0..MAX_DEPTH {
        let Ok(dict) = doc.get_object(current).and_then(|o| o.as_dict()) else {
            break;
        };
        match dict.get(b"Resources") {
            Ok(Object::Reference(id)) => {
                home = Some(Home::Indirect(*id));
                break;
            }
            Ok(Object::Dictionary(_)) => {
                home = Some(Home::Inline(current));
                break;
            }
            _ => match dict.get(b"Parent") {
                Ok(Object::Reference(parent)) => current = *parent,
                _ => break,
            },
        }
    }
    let home = match home {
        Some(home) => home,
        None => {
            // No resources anywhere in the chain: give the page its own.
            let Ok(Object::Dictionary(page)) = doc.get_object_mut(page_id) else {
                return;
            };
            page.set("Resources", Object::Dictionary(Dictionary::new()));
            Home::Inline(page_id)
        }
    };
    let resources_owner = match home {
        Home::Indirect(id) => id,
        Home::Inline(id) => id,
    };
    let inline_resources = matches!(home, Home::Inline(_));

    let xobject_ref = {
        let Some(resources) = resource_dict(doc, resources_owner, inline_resources) else {
            return;
        };
        match resources.get(b"XObject") {
            Ok(Object::Reference(id)) => Some(*id),
            _ => None,
        }
    };

    // Phase 2, mutable.
    if let Some(id) = xobject_ref {
        if let Ok(Object::Dictionary(xobjects)) = doc.get_object_mut(id) {
            for (name, ap_id) in bindings {
                xobjects.set(name.clone(), Object::Reference(*ap_id));
            }
        }
        return;
    }
    let Some(resources) = resource_dict_mut(doc, resources_owner, inline_resources) else {
        return;
    };
    if !matches!(resources.get(b"XObject"), Ok(Object::Dictionary(_))) {
        resources.set("XObject", Object::Dictionary(Dictionary::new()));
    }
    let Ok(Object::Dictionary(xobjects)) = resources.get_mut(b"XObject") else {
        return;
    };
    for (name, ap_id) in bindings {
        xobjects.set(name.clone(), Object::Reference(*ap_id));
    }
}

fn resource_dict(doc: &Document, owner: ObjectId, inline: bool) -> Option<&Dictionary> {
    let object = doc.get_object(owner).ok()?;
    if inline {
        object
            .as_dict()
            .ok()?
            .get(b"Resources")
            .ok()?
            .as_dict()
            .ok()
    } else {
        object.as_dict().ok()
    }
}

fn resource_dict_mut(doc: &mut Document, owner: ObjectId, inline: bool) -> Option<&mut Dictionary> {
    let object = doc.get_object_mut(owner).ok()?;
    if inline {
        object
            .as_dict_mut()
            .ok()?
            .get_mut(b"Resources")
            .ok()?
            .as_dict_mut()
            .ok()
    } else {
        object.as_dict_mut().ok()
    }
}

/// Rewrite `/Contents` as `[prefix?, ...original..., burn_ops]`.
fn splice_contents(
    doc: &mut Document,
    page_id: ObjectId,
    prefix: Option<ObjectId>,
    ops_id: ObjectId,
) {
    let Ok(Object::Dictionary(page)) = doc.get_object_mut(page_id) else {
        return;
    };
    let mut parts: Vec<Object> = prefix.into_iter().map(Object::Reference).collect();
    match page.get(b"Contents") {
        Ok(Object::Array(existing)) => parts.extend(existing.iter().cloned()),
        Ok(other) => parts.push(other.clone()),
        Err(_) => {}
    }
    parts.push(Object::Reference(ops_id));
    page.set("Contents", Object::Array(parts));
}

/// Drop the planned widget annotations from a page, and the `/Annots` key
/// itself once nothing is left in it.
fn drop_widget_annots(doc: &mut Document, page_id: ObjectId, drop: &HashSet<ObjectId>) {
    // `/Annots` may be an indirect array shared with nothing else; handle both
    // shapes without cloning the page's other entries.
    let annots_ref = match doc.get_object(page_id).and_then(|o| o.as_dict()) {
        Ok(dict) => match dict.get(b"Annots") {
            Ok(Object::Reference(id)) => Some(*id),
            _ => None,
        },
        Err(_) => return,
    };
    let keep = |array: &mut Vec<Object>| {
        array.retain(|entry| !matches!(entry, Object::Reference(id) if drop.contains(id)));
        array.is_empty()
    };
    let emptied = match annots_ref {
        Some(id) => match doc.get_object_mut(id) {
            Ok(Object::Array(array)) => keep(array),
            _ => false,
        },
        None => match doc.get_object_mut(page_id) {
            Ok(Object::Dictionary(page)) => match page.get_mut(b"Annots") {
                Ok(Object::Array(array)) => keep(array),
                _ => false,
            },
            _ => false,
        },
    };
    if emptied {
        if let Ok(Object::Dictionary(page)) = doc.get_object_mut(page_id) {
            page.remove(b"Annots");
        }
    }
}

/// Remove every `/Type /OBJR` structure-tree entry whose `/Obj` pointed at a
/// widget this pass deleted, so the tagged tree keeps no reference to an
/// object that no longer exists.
fn drop_objr_references(doc: &mut Document, removed: &HashSet<ObjectId>) {
    if removed.is_empty() {
        return;
    }
    let dead: HashSet<ObjectId> = doc
        .objects
        .iter()
        .filter(|(_, object)| is_dead_objr(object, removed))
        .map(|(id, _)| *id)
        .collect();

    for object in doc.objects.values_mut() {
        prune_objr(object, removed, &dead);
    }
}

fn is_dead_objr(object: &Object, removed: &HashSet<ObjectId>) -> bool {
    let Object::Dictionary(dict) = object else {
        return false;
    };
    matches!(dict.get(b"Type"), Ok(Object::Name(t)) if t == b"OBJR")
        && matches!(dict.get(b"Obj"), Ok(Object::Reference(id)) if removed.contains(id))
}

fn prune_objr(object: &mut Object, removed: &HashSet<ObjectId>, dead: &HashSet<ObjectId>) {
    let is_dead = |entry: &Object| match entry {
        Object::Reference(id) => dead.contains(id),
        other => is_dead_objr(other, removed),
    };
    match object {
        Object::Array(array) => {
            array.retain(|entry| !is_dead(entry));
            for entry in array.iter_mut() {
                prune_objr(entry, removed, dead);
            }
        }
        Object::Dictionary(dict) => {
            for (_, value) in dict.iter_mut() {
                if is_dead(value) {
                    *value = Object::Null;
                } else {
                    prune_objr(value, removed, dead);
                }
            }
        }
        Object::Stream(stream) => {
            for (_, value) in stream.dict.iter_mut() {
                if is_dead(value) {
                    *value = Object::Null;
                } else {
                    prune_objr(value, removed, dead);
                }
            }
        }
        _ => {}
    }
}
