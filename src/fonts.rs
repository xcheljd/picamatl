//! Font subsetting (Phase 3, C-M1): Type0 / CIDFontType2 / Identity-H|V
//! fonts, plus nonsymbolic simple TrueType fonts (WinAnsi / MacRoman base
//! encodings, with `/Differences`), integrated via `subsetter` 0.2.6
//! (`default-features = false`).
//!
//! The Type0 design exploits the `/CIDToGIDMap` **stream** form so that
//! content-stream text bytes are **never rewritten**: the subset font gets new
//! (remapped) glyph IDs, and a freshly written old-CID -> new-GID map stream
//! absorbs the remapping. `/W`, `/DW`, and `/ToUnicode` are keyed by CID,
//! which never changes, so they stay untouched and text extraction is
//! bit-identical pre/post. The entire "rewrote the text wrong" bug class is
//! structurally impossible here.
//!
//! The simple-TrueType path keeps the same invariant a different way: codes,
//! `/Encoding`, `/Widths`, and `/ToUnicode` never change; the subset font
//! gets a freshly written `cmap` replicating the original's subtables
//! (restricted to retained glyphs, ids remapped), so every viewer lookup
//! path of ISO 32000-1 9.6.6.4 — (3,1) via glyph name -> Unicode, (1,0) via
//! glyph name -> Mac OS Roman code, plus any (3,0) the font carries —
//! resolves each used code to the same outline as before. Any used code the
//! cmap paths cannot resolve, an unknown glyph name, a symbolic flag, an
//! absent `/Encoding`/`/Widths`, or a cmap format outside {0, 4, 6, 12}
//! disqualifies that font (untouched, fail-safe).
//!
//! Fail-safe posture (eligibility, not effort — see docs/PHASE3-PLAN.md §C):
//!
//! - **Global rule:** if ANY content-bearing stream (page, form XObject,
//!   annotation appearance, tiling pattern, Type3 char proc) fails strict
//!   decompression or strict content parsing — or text is shown in a state we
//!   cannot attribute to a font — **no font in the document is touched**.
//! - **Per-font rule:** any doubt about one font (non-Identity encoding, no
//!   `/FontFile2`, unresolvable `/CIDToGIDMap`, shared descendant/descriptor/
//!   font file, out-of-range glyph IDs, `subsetter` error, not net-smaller)
//!   leaves *that* font untouched.
//! - PDF/A-declared documents (`pdfaid` in the XMP metadata) and encrypted
//!   documents are skipped entirely (C-M2 revisits PDF/A with `/CIDSet`
//!   regeneration).
//!
//! Discovery walks every stream class with its own resource context, with
//! bounded recursion and a path-based cycle guard. Fonts reachable from
//! contexts we cannot fully verify (AcroForm `/DR` — viewers may regenerate
//! appearance streams from `/DA` strings we do not parse; ExtGState `/Font`
//! entries) are disqualified rather than guessed at.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use lopdf::content::Content;
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};
use subsetter::GlyphRemapper;

use crate::{
    cffhint, cffmerge, deflate_level9, encodings, inflate_capped, resolve, truetype, type1,
    FilterClass,
};

/// Recursion bound for the form/pattern/Type3 walk (depth of nested streams).
const MAX_WALK_DEPTH: usize = 16;
/// Inflation cap for content/font/map streams walked by the subsetter path.
const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;

/// A planned, fully validated font subset, computed read-only against the
/// document.
pub(crate) enum FontPlan {
    Cid(CidFontPlan),
    Simple(SimpleFontPlan),
    Type1(Type1FontPlan),
}

impl FontPlan {
    /// The font dictionary object the plan anchors to (sort key).
    fn font_id(&self) -> ObjectId {
        match self {
            FontPlan::Cid(p) => p.type0_id,
            FontPlan::Simple(p) => p.font_id,
            FontPlan::Type1(p) => p.font_id,
        }
    }
}

/// A planned Type0/CIDFontType2 subset. Applying it replaces the
/// `/FontFile2` stream in place, points `/CIDToGIDMap` at a new map stream,
/// and re-tags the font names — nothing else changes.
pub(crate) struct CidFontPlan {
    type0_id: ObjectId,
    descendant_id: ObjectId,
    descriptor_id: ObjectId,
    font_file_id: ObjectId,
    /// Flate-compressed subset font program.
    deflated_font: Vec<u8>,
    /// Uncompressed length of the subset font (`/Length1`).
    font_len: i64,
    /// Flate-compressed old-CID -> new-GID map stream payload.
    deflated_map: Vec<u8>,
    /// `TAG+BaseFont` name applied to `/BaseFont` (both dicts) and `/FontName`.
    tagged_name: Vec<u8>,
}

/// A planned simple-TrueType subset. Applying it replaces the `/FontFile2`
/// stream in place and re-tags the font names — codes, `/Encoding`,
/// `/Widths`, and `/ToUnicode` never change.
pub(crate) struct SimpleFontPlan {
    font_id: ObjectId,
    descriptor_id: ObjectId,
    font_file_id: ObjectId,
    /// Flate-compressed subset font program (with rebuilt `cmap`).
    deflated_font: Vec<u8>,
    /// Uncompressed length of the subset font (`/Length1`).
    font_len: i64,
    /// `TAG+BaseFont` name applied to `/BaseFont` and `/FontName`.
    tagged_name: Vec<u8>,
}

/// A planned Type1 → Type1C (CFF) conversion (opt-in, `convert_type1`).
/// Applying it replaces the `/FontFile` stream object in place with a
/// `/FontFile3` (`/Subtype /Type1C`) subset, re-keys the descriptor entry,
/// and re-tags the font names — `/Encoding`, `/Widths`, and `/ToUnicode`
/// never change, so text extraction is bit-identical.
pub(crate) struct Type1FontPlan {
    font_id: ObjectId,
    descriptor_id: ObjectId,
    font_file_id: ObjectId,
    /// Flate-compressed CFF font program.
    deflated_cff: Vec<u8>,
    /// `TAG+BaseFont` name applied to `/BaseFont` and `/FontName`.
    tagged_name: Vec<u8>,
}

/// Plan every eligible font subset. Read-only; returns an empty vector (and
/// therefore changes nothing) on any global disqualifier. `subset_fonts`
/// gates the Type0/TrueType subsetting planners; `convert_type1` gates the
/// Type1 → Type1C conversion planner.
pub(crate) fn plan_font_subsets(
    doc: &Document,
    subset_fonts: bool,
    convert_type1: bool,
    strip_hinting: bool,
) -> Vec<FontPlan> {
    if doc.is_encrypted() || pdfa_blocked(doc) {
        return Vec::new();
    }

    let mut walker = Walker {
        doc,
        dr: None,
        used: HashMap::new(),
        ineligible: HashSet::new(),
        visited: HashSet::new(),
        aborted: false,
    };
    walker.walk_document();
    if walker.aborted || walker.used.is_empty() {
        return Vec::new();
    }

    // Reference counts over the whole live object graph: a descendant,
    // descriptor, or font file referenced from more than one place could be
    // shared with a font whose usage we did not attribute to it — mutating it
    // would be unsound, so such fonts are ineligible.
    let mut refcounts: HashMap<ObjectId, usize> = HashMap::new();
    for obj in doc.objects.values() {
        count_refs(obj, &mut refcounts);
    }
    for (_, val) in doc.trailer.iter() {
        count_refs(val, &mut refcounts);
    }

    let mut plans: Vec<FontPlan> = walker
        .used
        .iter()
        .filter(|(id, cids)| !walker.ineligible.contains(id) && !cids.is_empty())
        .filter_map(|(&id, cids)| {
            plan_one(
                doc,
                id,
                cids,
                &refcounts,
                subset_fonts,
                convert_type1,
                strip_hinting,
            )
        })
        .collect();
    // HashMap iteration order is arbitrary; sort so output is reproducible.
    plans.sort_by_key(FontPlan::font_id);
    plans
}

/// Dispatch a used font to the planner matching its subtype (each planner
/// gated by its own option).
#[allow(clippy::too_many_arguments)]
fn plan_one(
    doc: &Document,
    id: ObjectId,
    codes: &BTreeSet<u16>,
    refcounts: &HashMap<ObjectId, usize>,
    subset_fonts: bool,
    convert_type1: bool,
    strip_hinting: bool,
) -> Option<FontPlan> {
    let dict = doc.get_object(id).ok()?.as_dict().ok()?;
    match dict.get(b"Subtype").map(|s| resolve(doc, s)) {
        Ok(Object::Name(n)) if n == b"Type0" && subset_fonts => {
            plan_one_font(doc, id, codes, refcounts, strip_hinting).map(FontPlan::Cid)
        }
        Ok(Object::Name(n)) if n == b"TrueType" && subset_fonts => {
            plan_one_simple_font(doc, id, codes, refcounts, strip_hinting).map(FontPlan::Simple)
        }
        Ok(Object::Name(n)) if n == b"Type1" && convert_type1 => {
            plan_one_type1_font(doc, id, codes, refcounts).map(FontPlan::Type1)
        }
        _ => None,
    }
}

/// Apply planned subsets. Each plan is independent; the set may be empty.
pub(crate) fn apply_font_subsets(doc: &mut Document, plans: Vec<FontPlan>) {
    for plan in plans {
        match plan {
            FontPlan::Cid(plan) => apply_cid_plan(doc, plan),
            FontPlan::Simple(plan) => apply_simple_plan(doc, plan),
            FontPlan::Type1(plan) => apply_type1_plan(doc, plan),
        }
    }
}

/// One planned same-family Type1C union merge: every member's `FontFile3`
/// stream is replaced with the identical merged program (the document-wide
/// stream dedup then collapses them to one object), and its `/BaseFont` /
/// `/FontName` are retagged from the merged bytes.
pub(crate) struct T1cMergePlan {
    members: Vec<T1cMember>,
    deflated_font: Vec<u8>,
    tagged_name: Vec<u8>,
}

struct T1cMember {
    font_id: ObjectId,
    descriptor_id: ObjectId,
    font_file_id: ObjectId,
}

/// Plan union merges of same-family simple Type1C subset fragments
/// (lossless: see src/cffmerge.rs for the byte-conservative preconditions).
/// Read-only; any global disqualifier or per-family doubt yields no plan for
/// that family.
pub(crate) fn plan_type1c_merges(doc: &Document) -> Vec<T1cMergePlan> {
    if doc.is_encrypted() || pdfa_blocked(doc) {
        return Vec::new();
    }
    let mut refcounts: HashMap<ObjectId, usize> = HashMap::new();
    for obj in doc.objects.values() {
        count_refs(obj, &mut refcounts);
    }
    for (_, val) in doc.trailer.iter() {
        count_refs(val, &mut refcounts);
    }

    struct Candidate {
        font_id: ObjectId,
        descriptor_id: ObjectId,
        font_file_id: ObjectId,
        bytes: Vec<u8>,
        stored_len: usize,
        needs_empty_builtin: bool,
    }
    // Fragments grouped by base font name minus the subset tag; sorted maps
    // and sorted ids keep the plan order reproducible.
    let mut groups: BTreeMap<Vec<u8>, Vec<Candidate>> = BTreeMap::new();
    let mut ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
    ids.sort_unstable();
    for id in ids {
        let Ok(Object::Dictionary(font)) = doc.get_object(id) else {
            continue;
        };
        let is_type1 = matches!(
            font.get(b"Type").map(|o| resolve(doc, o)),
            Ok(Object::Name(n)) if n == b"Font"
        ) && matches!(
            font.get(b"Subtype").map(|o| resolve(doc, o)),
            Ok(Object::Name(n)) if n == b"Type1"
        );
        if !is_type1 {
            continue;
        }
        // The merged program's built-in encoding is explicitly empty, so the
        // PDF-side /Encoding must determine every code lookup without the
        // fragment's built-in: a named base encoding qualifies outright; an
        // /Encoding dictionary without /BaseEncoding falls back to the
        // built-in (Table 114) and qualifies only when that built-in is
        // itself empty (checked below, once the fragment bytes are read).
        let needs_empty_builtin = match font.get(b"Encoding").map(|o| resolve(doc, o)) {
            Ok(Object::Name(_)) => false,
            Ok(Object::Dictionary(d)) => !matches!(
                d.get(b"BaseEncoding").map(|o| resolve(doc, o)),
                Ok(Object::Name(_))
            ),
            _ => continue,
        };
        // /Widths must supply the advances (appended glyphs are re-based on
        // the base fragment's width parameters; see cffmerge).
        if font.get(b"Widths").is_err() {
            continue;
        }
        let Ok(Object::Name(base_name)) = font.get(b"BaseFont").map(|o| resolve(doc, o)) else {
            continue;
        };
        let base_name = base_name.clone();
        let Ok(desc_obj) = font.get(b"FontDescriptor") else {
            continue;
        };
        let (descriptor_id, descriptor) = resolve_ref(doc, desc_obj);
        let Some(descriptor_id) = descriptor_id else {
            continue;
        };
        let Ok(descriptor) = descriptor.as_dict() else {
            continue;
        };
        if descriptor.get(b"FontFile").is_ok() || descriptor.get(b"FontFile2").is_ok() {
            continue;
        }
        let Ok(ff_obj) = descriptor.get(b"FontFile3") else {
            continue;
        };
        let (font_file_id, font_file) = resolve_ref(doc, ff_obj);
        let Some(font_file_id) = font_file_id else {
            continue;
        };
        let Ok(font_file) = font_file.as_stream() else {
            continue;
        };
        if !matches!(
            font_file.dict.get(b"Subtype").map(|o| resolve(doc, o)),
            Ok(Object::Name(n)) if n == b"Type1C"
        ) {
            continue;
        }
        let Some(bytes) = strict_stream_bytes(doc, font_file) else {
            continue;
        };
        if needs_empty_builtin && !cffmerge::has_empty_builtin_encoding(&bytes) {
            continue;
        }
        groups
            .entry(strip_subset_tag(&base_name).to_vec())
            .or_default()
            .push(Candidate {
                font_id: id,
                descriptor_id,
                font_file_id,
                bytes,
                stored_len: font_file.content.len(),
                needs_empty_builtin,
            });
    }

    let mut plans = Vec::new();
    for (base_name, mut members) in groups {
        // Producers routinely share one descriptor/FontFile3 among several
        // same-family font dicts already. Sharing is safe exactly when every
        // reference to the shared object comes from inside this group: the
        // descriptor's refcount must equal the number of member font dicts
        // pointing at it, and the font file's refcount the number of member
        // descriptors pointing at it. Anything else could serve a font whose
        // usage was not attributed here — drop that member, fail-safe.
        let mut desc_refs: HashMap<ObjectId, usize> = HashMap::new();
        let mut file_refs: HashMap<ObjectId, HashSet<ObjectId>> = HashMap::new();
        for m in &members {
            *desc_refs.entry(m.descriptor_id).or_insert(0) += 1;
            file_refs
                .entry(m.font_file_id)
                .or_default()
                .insert(m.descriptor_id);
        }
        members.retain(|m| {
            refcounts.get(&m.descriptor_id) == Some(&desc_refs[&m.descriptor_id])
                && refcounts.get(&m.font_file_id) == Some(&file_refs[&m.font_file_id].len())
        });
        // Merge over the distinct programs (shared members repeat bytes).
        let mut seen_files: HashSet<ObjectId> = HashSet::new();
        let unique: Vec<&Candidate> = members
            .iter()
            .filter(|m| seen_files.insert(m.font_file_id))
            .collect();
        if unique.len() < 2 {
            continue;
        }
        let fragments: Vec<&[u8]> = unique.iter().map(|m| m.bytes.as_slice()).collect();
        let write_empty_encoding = members.iter().any(|m| m.needs_empty_builtin);
        let Some(merged) = cffmerge::merge_type1c(&fragments, write_empty_encoding) else {
            continue;
        };
        let Some(deflated) = deflate_level9(&merged) else {
            continue;
        };
        // Net-smaller guard on stored bytes: after the stream dedup collapses
        // the identical replacements, exactly one copy ships.
        let stored: usize = unique.iter().map(|m| m.stored_len).sum();
        if deflated.len() >= stored {
            continue;
        }
        let tag = subset_tag(&merged);
        let mut tagged_name = Vec::with_capacity(base_name.len() + 7);
        tagged_name.extend_from_slice(&tag);
        tagged_name.push(b'+');
        tagged_name.extend_from_slice(&base_name);
        plans.push(T1cMergePlan {
            members: members
                .into_iter()
                .map(|m| T1cMember {
                    font_id: m.font_id,
                    descriptor_id: m.descriptor_id,
                    font_file_id: m.font_file_id,
                })
                .collect(),
            deflated_font: deflated,
            tagged_name,
        });
    }
    plans
}

/// Strip Type2 hints from every `Type1C` font program in the document
/// (opt-in, under `--strip-hinting`).
///
/// Unlike the planners around it this runs *after* the font plans have been
/// applied, over the document's final `/FontFile3` streams, so it covers
/// programs this run produced — union merges and Type1 → Type1C conversions —
/// as well as ones no other pass touched.
///
/// It needs no plan and no reference-count analysis because a hint strip is
/// PDF-side inert: glyph names, glyph order, advance widths, `/Encoding`,
/// `/Widths` and `/ToUnicode` are all unchanged, so every font dictionary
/// pointing at a stream — however many there are — keeps resolving each code
/// to the same outline. What changes is only how a rasterizer grid-fits that
/// outline at small ppem, which is exactly the consent `--strip-hinting`
/// already carries.
///
/// Per-program fail-safe: [`cffhint::strip_hints`] verifies the rewritten
/// program traces glyph-for-glyph identically to the original and is strictly
/// smaller; anything else leaves that stream exactly as it was.
pub(crate) fn strip_type1c_hints(doc: &mut Document) {
    if doc.is_encrypted() || pdfa_blocked(doc) {
        return;
    }
    let mut ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
    ids.sort_unstable();
    let mut rewrites: Vec<(ObjectId, Vec<u8>)> = Vec::new();
    for id in ids {
        let Ok(Object::Stream(stream)) = doc.get_object(id) else {
            continue;
        };
        if !matches!(
            stream.dict.get(b"Subtype").map(|o| resolve(doc, o)),
            Ok(Object::Name(n)) if n == b"Type1C"
        ) {
            continue;
        }
        let Some(bytes) = strict_stream_bytes(doc, stream) else {
            continue;
        };
        // The Private DICT hinting keys (`BlueValues` and friends) describe
        // nothing once the charstring hints are gone, so they go too.
        let Some(stripped) = cffhint::strip_hints(&bytes, true) else {
            continue;
        };
        let Some(deflated) = deflate_level9(&stripped) else {
            continue;
        };
        if deflated.len() >= stream.content.len() {
            continue;
        }
        rewrites.push((id, deflated));
    }
    for (id, content) in rewrites {
        if let Ok(Object::Stream(stream)) = doc.get_object_mut(id) {
            stream
                .dict
                .set("Filter", Object::Name(b"FlateDecode".to_vec()));
            stream.dict.remove(b"DecodeParms");
            stream.set_content(content);
        }
    }
}

/// True when at least one `Type1C` program in the document has strippable
/// hints. Read-only; used only to answer "is there any work at all" for a
/// document nothing else touches. Stops at the first program that strips.
pub(crate) fn any_type1c_hint_work(doc: &Document) -> bool {
    if doc.is_encrypted() || pdfa_blocked(doc) {
        return false;
    }
    doc.objects.values().any(|obj| {
        let Object::Stream(stream) = obj else {
            return false;
        };
        matches!(
            stream.dict.get(b"Subtype").map(|o| resolve(doc, o)),
            Ok(Object::Name(n)) if n == b"Type1C"
        ) && strict_stream_bytes(doc, stream)
            .and_then(|b| cffhint::strip_hints(&b, true))
            .is_some()
    })
}

/// Apply planned Type1C family merges. Each plan is independent.
pub(crate) fn apply_type1c_merges(doc: &mut Document, plans: Vec<T1cMergePlan>) {
    for plan in plans {
        for member in &plan.members {
            let stream = Stream::new(
                dictionary! {
                    "Filter" => "FlateDecode",
                    "Subtype" => Object::Name(b"Type1C".to_vec()),
                },
                plan.deflated_font.clone(),
            )
            .with_compression(false);
            doc.objects
                .insert(member.font_file_id, Object::Stream(stream));
            if let Ok(Object::Dictionary(d)) = doc.get_object_mut(member.descriptor_id) {
                d.set("FontName", Object::Name(plan.tagged_name.clone()));
                // A stale /CharSet would misdescribe the union program; it is
                // optional metadata (PDF/A documents were skipped above).
                d.remove(b"CharSet");
            }
            if let Ok(Object::Dictionary(d)) = doc.get_object_mut(member.font_id) {
                d.set("BaseFont", Object::Name(plan.tagged_name.clone()));
            }
        }
    }
}

fn apply_cid_plan(doc: &mut Document, plan: CidFontPlan) {
    let map_stream = Stream::new(dictionary! { "Filter" => "FlateDecode" }, plan.deflated_map)
        .with_compression(false);
    let map_id = doc.add_object(map_stream);

    let font_stream = Stream::new(
        dictionary! {
            "Filter" => "FlateDecode",
            "Length1" => plan.font_len,
        },
        plan.deflated_font,
    )
    .with_compression(false);
    doc.objects
        .insert(plan.font_file_id, Object::Stream(font_stream));

    if let Ok(Object::Dictionary(d)) = doc.get_object_mut(plan.descendant_id) {
        d.set("CIDToGIDMap", Object::Reference(map_id));
        d.set("BaseFont", Object::Name(plan.tagged_name.clone()));
    }
    if let Ok(Object::Dictionary(d)) = doc.get_object_mut(plan.type0_id) {
        d.set("BaseFont", Object::Name(plan.tagged_name.clone()));
    }
    if let Ok(Object::Dictionary(d)) = doc.get_object_mut(plan.descriptor_id) {
        d.set("FontName", Object::Name(plan.tagged_name));
        // A stale /CIDSet would over-claim glyph coverage after the
        // subset; it is optional metadata outside PDF/A (and PDF/A
        // documents were skipped above), so drop it. C-M2 regenerates it.
        d.remove(b"CIDSet");
    }
}

fn apply_simple_plan(doc: &mut Document, plan: SimpleFontPlan) {
    let font_stream = Stream::new(
        dictionary! {
            "Filter" => "FlateDecode",
            "Length1" => plan.font_len,
        },
        plan.deflated_font,
    )
    .with_compression(false);
    doc.objects
        .insert(plan.font_file_id, Object::Stream(font_stream));

    if let Ok(Object::Dictionary(d)) = doc.get_object_mut(plan.font_id) {
        d.set("BaseFont", Object::Name(plan.tagged_name.clone()));
    }
    if let Ok(Object::Dictionary(d)) = doc.get_object_mut(plan.descriptor_id) {
        d.set("FontName", Object::Name(plan.tagged_name));
    }
}

fn apply_type1_plan(doc: &mut Document, plan: Type1FontPlan) {
    // The stream object keeps its id but changes role: `/FontFile3` streams
    // carry `/Subtype /Type1C` and none of the Type1 `/Length1..3` splits.
    let font_stream = Stream::new(
        dictionary! {
            "Filter" => "FlateDecode",
            "Subtype" => "Type1C",
        },
        plan.deflated_cff,
    )
    .with_compression(false);
    doc.objects
        .insert(plan.font_file_id, Object::Stream(font_stream));

    if let Ok(Object::Dictionary(d)) = doc.get_object_mut(plan.descriptor_id) {
        d.remove(b"FontFile");
        d.set("FontFile3", Object::Reference(plan.font_file_id));
        d.set("FontName", Object::Name(plan.tagged_name.clone()));
    }
    if let Ok(Object::Dictionary(d)) = doc.get_object_mut(plan.font_id) {
        d.set("BaseFont", Object::Name(plan.tagged_name));
    }
}

// ---------------------------------------------------------------------------
// Strict stream access
// ---------------------------------------------------------------------------

/// Strictly decode a stream's bytes: uncompressed or single-filter
/// FlateDecode without `/DecodeParms` only. Unlike lopdf's
/// `decompressed_content` (which returns *partial* data on corrupt zlib —
/// a silent under-read that would hide text-show operators from the walker),
/// corrupt input yields `None`.
fn strict_stream_bytes(doc: &Document, stream: &Stream) -> Option<Vec<u8>> {
    match stream.dict.get(b"Filter") {
        Err(_) => Some(stream.content.clone()),
        Ok(Object::Null) => Some(stream.content.clone()),
        Ok(filter) => match crate::classify_filter(doc, filter) {
            FilterClass::FlateOnly => {
                if !matches!(stream.dict.get(b"DecodeParms"), Err(_) | Ok(Object::Null)) {
                    return None;
                }
                inflate_capped(&stream.content, MAX_STREAM_BYTES)
            }
            _ => None,
        },
    }
}

/// Follow a reference chain, returning the id of the **final** reference (the
/// object that would be mutated) alongside the resolved object. `None` id
/// means the object was inline (not an indirect object).
fn resolve_ref<'a>(doc: &'a Document, mut obj: &'a Object) -> (Option<ObjectId>, &'a Object) {
    let mut last = None;
    for _ in 0..8 {
        match obj {
            Object::Reference(id) => match doc.get_object(*id) {
                Ok(next) => {
                    last = Some(*id);
                    obj = next;
                }
                Err(_) => break,
            },
            _ => break,
        }
    }
    (last, obj)
}

/// True when the document declares PDF/A conformance in its XMP metadata (or
/// when the metadata exists but cannot be read strictly — treated as "cannot
/// rule PDF/A out"). Subsetting would invalidate `/CIDSet`-style conformance
/// artifacts, so such documents are skipped wholesale in C-M1. Also consumed by
/// the final re-deflate pass in `lib.rs`, which declines conformance-claiming
/// documents for the same "do not touch a document that asserts its own byte
/// shape" reason.
pub(crate) fn pdfa_blocked(doc: &Document) -> bool {
    let Ok(catalog) = doc.catalog() else {
        return true;
    };
    match catalog.get(b"Metadata") {
        Err(_) => false,
        Ok(meta) => {
            let (_, meta) = resolve_ref(doc, meta);
            let Object::Stream(stream) = meta else {
                return true;
            };
            let Some(bytes) = strict_stream_bytes(doc, stream) else {
                return true;
            };
            // Match both the conventional prefix and the namespace URI tail,
            // since XMP prefixes are renameable.
            contains(&bytes, b"pdfaid:part") || contains(&bytes, b"pdfa/ns/id")
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn count_refs(obj: &Object, counts: &mut HashMap<ObjectId, usize>) {
    match obj {
        Object::Reference(id) => *counts.entry(*id).or_insert(0) += 1,
        Object::Array(items) => {
            for item in items {
                count_refs(item, counts);
            }
        }
        Object::Dictionary(dict) => {
            for (_, val) in dict.iter() {
                count_refs(val, counts);
            }
        }
        Object::Stream(stream) => {
            for (_, val) in stream.dict.iter() {
                count_refs(val, counts);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Glyph discovery walker
// ---------------------------------------------------------------------------

/// The font selected by the most recent `Tf` in the current stream.
enum CurrentFont {
    /// No `Tf` seen yet: a show operator here means text we cannot attribute,
    /// e.g. a form inheriting the invoker's text state — global abort.
    Unset,
    /// A font we will never touch (Type3, MMType1, non-Identity Type0,
    /// inline dictionaries): show strings are ignored.
    Other,
    /// A Type0 / Identity-H|V font: show strings are big-endian 2-byte CIDs.
    Candidate(ObjectId),
    /// A simple TrueType or Type1 font: show strings are single-byte codes.
    /// Eligibility (encoding, flags, cmap, font program) is decided at
    /// planning time, per subtype.
    Simple(ObjectId),
}

struct Walker<'a> {
    doc: &'a Document,
    /// AcroForm `/DR` resources: fallback context for appearance streams.
    dr: Option<&'a Dictionary>,
    /// Used CIDs per candidate Type0 font object.
    used: HashMap<ObjectId, BTreeSet<u16>>,
    /// Fonts disqualified during the walk (odd show strings, `/DR` exposure).
    ineligible: HashSet<ObjectId>,
    /// Self-contained streams already walked (safe to skip on re-encounter).
    visited: HashSet<ObjectId>,
    aborted: bool,
}

impl<'a> Walker<'a> {
    fn abort(&mut self) {
        self.aborted = true;
    }

    fn walk_document(&mut self) {
        self.collect_acroform();
        if self.aborted {
            return;
        }
        for (_, page_id) in self.doc.get_pages() {
            self.walk_page(page_id);
            if self.aborted {
                return;
            }
        }
    }

    /// Fonts reachable from AcroForm `/DR` are disqualified: viewers may
    /// regenerate field appearances from `/DA` strings (which we do not
    /// parse), using glyphs we never saw.
    fn collect_acroform(&mut self) {
        let Ok(catalog) = self.doc.catalog() else {
            self.abort();
            return;
        };
        let Ok(acroform) = catalog.get(b"AcroForm") else {
            return;
        };
        let Ok(acroform) = resolve(self.doc, acroform).as_dict() else {
            self.abort();
            return;
        };
        let Ok(dr) = acroform.get(b"DR") else {
            return;
        };
        let Ok(dr) = resolve(self.doc, dr).as_dict() else {
            self.abort();
            return;
        };
        self.dr = Some(dr);
        let Ok(fonts) = dr.get(b"Font") else {
            return;
        };
        let Ok(fonts) = resolve(self.doc, fonts).as_dict() else {
            self.abort();
            return;
        };
        for (_, val) in fonts.iter() {
            if let (Some(id), _) = resolve_ref(self.doc, val) {
                self.ineligible.insert(id);
            }
        }
    }

    fn walk_page(&mut self, page_id: ObjectId) {
        let mut path: Vec<ObjectId> = Vec::new();
        let resources = crate::page_resources(self.doc, page_id);

        // Concatenate the page's content streams (operators may span stream
        // boundaries, so they are parsed as one unit, per spec).
        let mut content = Vec::new();
        for stream_id in self.doc.get_page_contents(page_id) {
            let Ok(stream) = self.doc.get_object(stream_id).and_then(Object::as_stream) else {
                self.abort();
                return;
            };
            let Some(bytes) = strict_stream_bytes(self.doc, stream) else {
                self.abort();
                return;
            };
            content.extend_from_slice(&bytes);
            content.push(b'\n');
        }
        self.walk_context(&content, resources, &[], &mut path);
        if self.aborted {
            return;
        }
        self.walk_annotations(page_id, &mut path);
    }

    fn walk_annotations(&mut self, page_id: ObjectId, path: &mut Vec<ObjectId>) {
        let Ok(page) = self.doc.get_object(page_id).and_then(Object::as_dict) else {
            self.abort();
            return;
        };
        let Ok(annots) = page.get(b"Annots") else {
            return;
        };
        let Ok(annots) = resolve(self.doc, annots).as_array() else {
            self.abort();
            return;
        };
        for entry in annots {
            // A dangling annotation reference renders nothing; skip it.
            if let Object::Reference(id) = entry {
                if self.doc.get_object(*id).is_err() {
                    continue;
                }
            }
            let Ok(annot) = resolve(self.doc, entry).as_dict() else {
                self.abort();
                return;
            };
            let Ok(ap) = annot.get(b"AP") else {
                continue;
            };
            let Ok(ap) = resolve(self.doc, ap).as_dict() else {
                self.abort();
                return;
            };
            for (_, appearance) in ap.iter() {
                match resolve_ref(self.doc, appearance) {
                    (Some(id), Object::Stream(_)) => self.walk_appearance(id, path),
                    // Appearance sub-dictionary: one stream per state.
                    (_, Object::Dictionary(states)) => {
                        for (_, state) in states.iter() {
                            match resolve_ref(self.doc, state) {
                                (Some(id), Object::Stream(_)) => self.walk_appearance(id, path),
                                (_, Object::Reference(_)) => continue, // dangling
                                _ => self.abort(),
                            }
                            if self.aborted {
                                return;
                            }
                        }
                    }
                    (_, Object::Reference(_)) => continue, // dangling
                    _ => self.abort(),
                }
                if self.aborted {
                    return;
                }
            }
        }
    }

    /// Walk one annotation appearance stream. Its resource fallback is the
    /// AcroForm `/DR` (the context viewers evaluate `/DA` against), never the
    /// page resources.
    fn walk_appearance(&mut self, id: ObjectId, path: &mut Vec<ObjectId>) {
        let parent: Vec<&Dictionary> = self.dr.into_iter().collect();
        self.walk_stream_object(id, OwnResources::OfStream, &parent, path, true);
    }

    /// Walk a content-bearing stream object (form XObject, tiling pattern,
    /// appearance stream, or Type3 char proc) with cycle guard and depth
    /// bound. Returns true when any font lookup fell back past the stream's
    /// own resources (context-dependent: must be re-walked per context).
    fn walk_stream_object(
        &mut self,
        id: ObjectId,
        own: OwnResources<'a>,
        parent_chain: &[&'a Dictionary],
        path: &mut Vec<ObjectId>,
        cacheable: bool,
    ) -> bool {
        if path.contains(&id) {
            // A stream reachable from itself is malformed; walking it could
            // loop, so give up on the whole document.
            self.abort();
            return false;
        }
        if cacheable && self.visited.contains(&id) {
            return false;
        }
        if path.len() >= MAX_WALK_DEPTH {
            self.abort();
            return false;
        }
        let Ok(stream) = self.doc.get_object(id).and_then(Object::as_stream) else {
            self.abort();
            return false;
        };
        let own_res = match own {
            OwnResources::OfStream => match stream.dict.get(b"Resources") {
                Err(_) => None,
                Ok(res) => match resolve(self.doc, res).as_dict() {
                    Ok(dict) => Some(dict),
                    Err(_) => {
                        self.abort();
                        return false;
                    }
                },
            },
            OwnResources::Given(res) => res,
        };
        let Some(bytes) = strict_stream_bytes(self.doc, stream) else {
            self.abort();
            return false;
        };
        path.push(id);
        let fallback = self.walk_context(&bytes, own_res, parent_chain, path);
        path.pop();
        if cacheable && !fallback && !self.aborted {
            self.visited.insert(id);
        }
        fallback
    }

    /// Walk one content stream in its resource context: recurse into the
    /// resources' nested content (forms, patterns, Type3 char procs), then
    /// parse the operators tracking `Tf` and the four show operators.
    fn walk_context(
        &mut self,
        content: &[u8],
        own_res: Option<&'a Dictionary>,
        parent_chain: &[&'a Dictionary],
        path: &mut Vec<ObjectId>,
    ) -> bool {
        let mut chain: Vec<&'a Dictionary> = Vec::with_capacity(parent_chain.len() + 1);
        if let Some(res) = own_res {
            chain.push(res);
        }
        chain.extend_from_slice(parent_chain);
        let own_count = usize::from(own_res.is_some());

        let mut fallback = false;
        if let Some(res) = own_res {
            fallback |= self.walk_resources(res, &chain, path);
            if self.aborted {
                return fallback;
            }
        }

        // Strict parsing: the lenient `Content::decode` silently drops a
        // trailing unparseable region, which could hide show operators.
        let Some(parsed) = decode_content_strict(content) else {
            self.abort();
            return fallback;
        };

        let mut current = CurrentFont::Unset;
        for op in &parsed.operations {
            match op.operator.as_str() {
                "Tf" => {
                    let Some(Object::Name(name)) = op.operands.first() else {
                        self.abort();
                        return fallback;
                    };
                    let Some((font, from_fallback)) = self.lookup_font(&chain, own_count, name)
                    else {
                        self.abort();
                        return fallback;
                    };
                    fallback |= from_fallback;
                    current = self.classify_font(font);
                }
                "Tj" | "'" => match op.operands.first() {
                    Some(Object::String(s, _)) => self.record_show(&current, s),
                    _ => self.abort(),
                },
                "\"" => match op.operands.get(2) {
                    Some(Object::String(s, _)) => self.record_show(&current, s),
                    _ => self.abort(),
                },
                "TJ" => match op.operands.first() {
                    Some(Object::Array(items)) => {
                        for item in items {
                            match item {
                                Object::String(s, _) => self.record_show(&current, s),
                                Object::Integer(_) | Object::Real(_) => {}
                                _ => self.abort(),
                            }
                            if self.aborted {
                                return fallback;
                            }
                        }
                    }
                    _ => self.abort(),
                },
                // lopdf parses inline images into a "BI" operation carrying
                // the image as a stream operand; EMPTY operands mean it could
                // not parse the image and skipped bytes — parsing after that
                // point is untrustworthy.
                "BI" if op.operands.is_empty() => self.abort(),
                _ => {}
            }
            if self.aborted {
                return fallback;
            }
        }
        fallback
    }

    /// Look up a font name through the resource chain (innermost first).
    /// Returns the font object and whether the hit came from a fallback
    /// (inherited) context.
    fn lookup_font(
        &self,
        chain: &[&'a Dictionary],
        own_count: usize,
        name: &[u8],
    ) -> Option<(&'a Object, bool)> {
        for (idx, res) in chain.iter().enumerate() {
            let Ok(fonts) = res.get(b"Font") else {
                continue;
            };
            let Ok(fonts) = resolve(self.doc, fonts).as_dict() else {
                return None;
            };
            if let Ok(font) = fonts.get(name) {
                return Some((font, idx >= own_count));
            }
        }
        None
    }

    /// Classify the font selected by a `Tf`. Aborts (via the returned state
    /// being irrelevant after `self.aborted`) when the font cannot be
    /// understood well enough to be safe.
    fn classify_font(&mut self, font: &'a Object) -> CurrentFont {
        let (id, resolved) = resolve_ref(self.doc, font);
        let Ok(dict) = resolved.as_dict() else {
            self.abort();
            return CurrentFont::Other;
        };
        let subtype = match dict.get(b"Subtype").map(|s| resolve(self.doc, s)) {
            Ok(Object::Name(n)) => n.as_slice(),
            _ => return CurrentFont::Other,
        };
        if subtype == b"TrueType" || subtype == b"Type1" {
            return match id {
                Some(id) => CurrentFont::Simple(id),
                // An inline simple dict stays untouched (usage cannot be
                // keyed); sharing its descriptor or font file with a
                // candidate is caught by the refcount guard.
                None => CurrentFont::Other,
            };
        }
        if subtype != b"Type0" {
            return CurrentFont::Other;
        }
        let identity = matches!(
            dict.get(b"Encoding").map(|e| resolve(self.doc, e)),
            Ok(Object::Name(n)) if n == b"Identity-H" || n == b"Identity-V"
        );
        if !identity {
            // Predefined CJK CMaps, embedded CMap streams: never subset,
            // shows are safely ignored (the font stays untouched).
            return CurrentFont::Other;
        }
        match id {
            Some(id) => CurrentFont::Candidate(id),
            None => {
                // An inline Identity Type0 dict could share descendants with
                // a referenced font without us being able to attribute usage.
                self.abort();
                CurrentFont::Other
            }
        }
    }

    fn record_show(&mut self, current: &CurrentFont, bytes: &[u8]) {
        match current {
            CurrentFont::Unset => self.abort(),
            CurrentFont::Other => {}
            CurrentFont::Candidate(id) => {
                if !bytes.len().is_multiple_of(2) {
                    // Malformed for Identity-H/V; cannot trust this font's
                    // collected set.
                    self.ineligible.insert(*id);
                    return;
                }
                let set = self.used.entry(*id).or_default();
                for pair in bytes.as_chunks::<2>().0 {
                    set.insert(u16::from_be_bytes(*pair));
                }
            }
            CurrentFont::Simple(id) => {
                let set = self.used.entry(*id).or_default();
                for &byte in bytes {
                    set.insert(u16::from(byte));
                }
            }
        }
    }

    /// Recurse into the content-bearing streams reachable from one resource
    /// dictionary: form XObjects, tiling patterns, Type3 char procs. Also
    /// vets ExtGState entries (a `/Font` there selects a font without `Tf`,
    /// which the walker cannot attribute — global abort).
    fn walk_resources(
        &mut self,
        res: &'a Dictionary,
        chain: &[&'a Dictionary],
        path: &mut Vec<ObjectId>,
    ) -> bool {
        let mut fallback = false;

        if let Ok(states) = res.get(b"ExtGState") {
            let Ok(states) = resolve(self.doc, states).as_dict() else {
                self.abort();
                return fallback;
            };
            for (_, state) in states.iter() {
                let Ok(state) = resolve(self.doc, state).as_dict() else {
                    self.abort();
                    return fallback;
                };
                if state.get(b"Font").is_ok() {
                    self.abort();
                    return fallback;
                }
            }
        }

        if let Ok(xobjects) = res.get(b"XObject") {
            let Ok(xobjects) = resolve(self.doc, xobjects).as_dict() else {
                self.abort();
                return fallback;
            };
            for (_, val) in xobjects.iter() {
                let Object::Reference(id) = val else {
                    self.abort();
                    return fallback;
                };
                let Ok(obj) = self.doc.get_object(*id) else {
                    continue; // dangling: renders nothing
                };
                let Ok(stream) = obj.as_stream() else {
                    self.abort();
                    return fallback;
                };
                match stream.dict.get(b"Subtype").map(|s| resolve(self.doc, s)) {
                    Ok(Object::Name(n)) if n == b"Form" => {
                        fallback |=
                            self.walk_stream_object(*id, OwnResources::OfStream, chain, path, true);
                    }
                    Ok(Object::Name(n)) if n == b"Image" || n == b"PS" => {}
                    _ => {
                        self.abort();
                        return fallback;
                    }
                }
                if self.aborted {
                    return fallback;
                }
            }
        }

        if let Ok(patterns) = res.get(b"Pattern") {
            let Ok(patterns) = resolve(self.doc, patterns).as_dict() else {
                self.abort();
                return fallback;
            };
            for (_, val) in patterns.iter() {
                match resolve_ref(self.doc, val) {
                    (Some(id), Object::Stream(stream)) => {
                        match stream
                            .dict
                            .get(b"PatternType")
                            .map(|p| resolve(self.doc, p))
                            .and_then(Object::as_i64)
                        {
                            Ok(1) => {
                                fallback |= self.walk_stream_object(
                                    id,
                                    OwnResources::OfStream,
                                    chain,
                                    path,
                                    true,
                                );
                            }
                            Ok(2) => {}
                            _ => {
                                self.abort();
                                return fallback;
                            }
                        }
                    }
                    // Shading patterns may be plain dictionaries; no content.
                    (_, Object::Dictionary(dict))
                        if matches!(
                            dict.get(b"PatternType")
                                .map(|p| resolve(self.doc, p))
                                .and_then(Object::as_i64),
                            Ok(2)
                        ) => {}
                    (_, Object::Reference(_)) => continue, // dangling
                    _ => {
                        self.abort();
                        return fallback;
                    }
                }
                if self.aborted {
                    return fallback;
                }
            }
        }

        if let Ok(fonts) = res.get(b"Font") {
            let Ok(fonts) = resolve(self.doc, fonts).as_dict() else {
                self.abort();
                return fallback;
            };
            for (_, val) in fonts.iter() {
                let (_, resolved) = resolve_ref(self.doc, val);
                if let Object::Reference(id) = resolved {
                    if self.doc.get_object(*id).is_err() {
                        continue; // dangling
                    }
                }
                let Ok(dict) = resolved.as_dict() else {
                    self.abort();
                    return fallback;
                };
                let is_type3 = matches!(
                    dict.get(b"Subtype").map(|s| resolve(self.doc, s)),
                    Ok(Object::Name(n)) if n == b"Type3"
                );
                if is_type3 {
                    fallback |= self.walk_type3(dict, chain, path);
                    if self.aborted {
                        return fallback;
                    }
                }
            }
        }

        fallback
    }

    /// Walk a Type3 font's char-proc streams. Their names resolve in the
    /// font's own `/Resources` first, falling back to the invoking context
    /// (the deprecated-but-real inheritance path).
    fn walk_type3(
        &mut self,
        font: &'a Dictionary,
        chain: &[&'a Dictionary],
        path: &mut Vec<ObjectId>,
    ) -> bool {
        let mut fallback = false;
        let own_res = match font.get(b"Resources") {
            Err(_) => None,
            Ok(res) => match resolve(self.doc, res).as_dict() {
                Ok(dict) => Some(dict),
                Err(_) => {
                    self.abort();
                    return fallback;
                }
            },
        };
        let Ok(procs) = font.get(b"CharProcs") else {
            self.abort();
            return fallback;
        };
        let Ok(procs) = resolve(self.doc, procs).as_dict() else {
            self.abort();
            return fallback;
        };
        for (_, proc_ref) in procs.iter() {
            let Object::Reference(id) = proc_ref else {
                self.abort();
                return fallback;
            };
            if self.doc.get_object(*id).is_err() {
                continue; // dangling: glyph renders nothing
            }
            // Not cacheable: the same char-proc stream under a different
            // Type3 font would have a different own-resources context.
            fallback |=
                self.walk_stream_object(*id, OwnResources::Given(own_res), chain, path, false);
            if self.aborted {
                return fallback;
            }
        }
        fallback
    }
}

/// Where a walked stream's own resource dictionary comes from.
enum OwnResources<'a> {
    /// The stream's own `/Resources` entry (forms, patterns, appearances).
    OfStream,
    /// Supplied by the surrounding structure (Type3 `/Resources` for its
    /// char procs, which have no `/Resources` of their own).
    Given(Option<&'a Dictionary>),
}

// ---------------------------------------------------------------------------
// Per-font planning
// ---------------------------------------------------------------------------

/// The descendant's CID -> GID mapping on input.
enum CidToGid {
    Identity,
    Table(Vec<u8>),
}

struct CidMap {
    map: CidToGid,
    /// Stored (compressed) size of the old map stream, for the net-smaller
    /// comparison. Zero for `/Identity`.
    stored_len: usize,
}

impl CidMap {
    fn lookup(&self, cid: u16) -> u16 {
        match &self.map {
            CidToGid::Identity => cid,
            CidToGid::Table(table) => {
                let idx = usize::from(cid) * 2;
                match (table.get(idx), table.get(idx + 1)) {
                    (Some(&hi), Some(&lo)) => u16::from_be_bytes([hi, lo]),
                    // Beyond the table: .notdef, per spec.
                    _ => 0,
                }
            }
        }
    }
}

fn load_cid_map(doc: &Document, descendant: &Dictionary) -> Option<CidMap> {
    match descendant.get(b"CIDToGIDMap") {
        // Absent defaults to /Identity per spec.
        Err(_) => Some(CidMap {
            map: CidToGid::Identity,
            stored_len: 0,
        }),
        Ok(obj) => match resolve_ref(doc, obj).1 {
            Object::Name(n) if n == b"Identity" => Some(CidMap {
                map: CidToGid::Identity,
                stored_len: 0,
            }),
            Object::Stream(stream) => {
                let table = strict_stream_bytes(doc, stream)?;
                Some(CidMap {
                    map: CidToGid::Table(table),
                    stored_len: stream.content.len(),
                })
            }
            _ => None,
        },
    }
}

fn be16(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes([
        *data.get(offset)?,
        *data.get(offset.checked_add(1)?)?,
    ]))
}

/// `numGlyphs` from the font's `maxp` table (single fonts only; collections
/// return `None`, which disqualifies them — FontFile2 must not be a TTC).
fn num_glyphs(font: &[u8]) -> Option<u16> {
    let table_count = usize::from(be16(font, 4)?);
    for i in 0..table_count {
        let record = 12 + i * 16;
        if font.get(record..record + 4)? == b"maxp" {
            let offset =
                u32::from_be_bytes(font.get(record + 8..record + 12)?.try_into().ok()?) as usize;
            return be16(font, offset.checked_add(4)?);
        }
    }
    None
}

/// Deterministic 6-letter subset tag derived from the subset bytes (FNV-1a;
/// no randomness, so outputs are reproducible).
fn subset_tag(data: &[u8]) -> [u8; 6] {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut tag = [0u8; 6];
    for slot in &mut tag {
        *slot = b'A' + (hash % 26) as u8;
        hash /= 26;
    }
    tag
}

/// Strip an existing `ABCDEF+` subset tag so re-subsetting replaces it
/// instead of stacking a second one.
fn strip_subset_tag(name: &[u8]) -> &[u8] {
    if name.len() > 7 && name[6] == b'+' && name[..6].iter().all(u8::is_ascii_uppercase) {
        &name[7..]
    } else {
        name
    }
}

/// Validate one candidate Type0 font end to end and build its plan. Any
/// failure — wrong shapes, shared structure, subsetter error, not
/// net-smaller — returns `None` and the font ships untouched.
fn plan_one_font(
    doc: &Document,
    type0_id: ObjectId,
    cids: &BTreeSet<u16>,
    refcounts: &HashMap<ObjectId, usize>,
    strip_hinting: bool,
) -> Option<CidFontPlan> {
    let type0 = doc.get_object(type0_id).ok()?.as_dict().ok()?;

    let descendants = type0.get(b"DescendantFonts").ok()?;
    // If the array itself is indirect, it must not be shared between fonts.
    let (array_id, descendants) = resolve_ref(doc, descendants);
    if let Some(array_id) = array_id {
        if refcounts.get(&array_id) != Some(&1) {
            return None;
        }
    }
    let descendants = descendants.as_array().ok()?;
    if descendants.len() != 1 {
        return None;
    }
    let (descendant_id, descendant) = resolve_ref(doc, &descendants[0]);
    let descendant_id = descendant_id?;
    let descendant = descendant.as_dict().ok()?;
    if !matches!(
        descendant.get(b"Subtype").map(|s| resolve(doc, s)),
        Ok(Object::Name(n)) if n == b"CIDFontType2"
    ) {
        // CIDFontType0 (CFF) requires show-string rewriting — C-M2/M3.
        return None;
    }

    let (descriptor_id, descriptor) = resolve_ref(doc, descendant.get(b"FontDescriptor").ok()?);
    let descriptor_id = descriptor_id?;
    let descriptor = descriptor.as_dict().ok()?;
    // A descriptor carrying additional font programs is a shape we do not
    // understand well enough to mutate.
    if descriptor.get(b"FontFile").is_ok() || descriptor.get(b"FontFile3").is_ok() {
        return None;
    }
    let (font_file_id, font_file) = resolve_ref(doc, descriptor.get(b"FontFile2").ok()?);
    let font_file_id = font_file_id?;
    let font_file = font_file.as_stream().ok()?;

    // Shared descendant/descriptor/font-program structure could serve fonts
    // whose usage was not attributed here; mutating it would be unsound.
    if refcounts.get(&descendant_id) != Some(&1)
        || refcounts.get(&descriptor_id) != Some(&1)
        || refcounts.get(&font_file_id) != Some(&1)
    {
        return None;
    }

    let font_bytes = strict_stream_bytes(doc, font_file)?;
    let glyph_count = num_glyphs(&font_bytes)?;
    let cid_map = load_cid_map(doc, descendant)?;

    let mut gids: BTreeSet<u16> = BTreeSet::new();
    for &cid in cids {
        let gid = cid_map.lookup(cid);
        if gid != 0 && gid >= glyph_count {
            // The document references a glyph the font does not have; the
            // font is not in a state we can confidently transform.
            return None;
        }
        gids.insert(gid);
    }
    let gid_list: Vec<u16> = gids.iter().copied().collect();
    let remapper = GlyphRemapper::new_from_glyphs_sorted(&gid_list);
    let subset = subsetter::subset(&font_bytes, 0, &remapper).ok()?;
    let subset = if strip_hinting {
        truetype::strip_hinting(&subset).unwrap_or(subset)
    } else {
        subset
    };
    // Mask the producer's `name`-table subset tags so two identical subsets
    // of the same font become byte-equal and the stream dedup collapses them.
    let subset = truetype::mask_subset_tags(&subset).unwrap_or(subset);
    // Cheap structural sanity on the output before trusting it: it must have
    // at least as many glyphs as we remapped (composite closure adds more).
    if num_glyphs(&subset)? < remapper.num_gids() {
        return None;
    }

    // Old CID -> new GID, 2 bytes big-endian per CID up to the max used CID;
    // unused CIDs map to 0 (.notdef). Mostly zeros, so it deflates to nearly
    // nothing.
    let max_cid = *cids.iter().next_back()?;
    let mut map = vec![0u8; (usize::from(max_cid) + 1) * 2];
    for &cid in cids {
        let new_gid = remapper.get(cid_map.lookup(cid)).unwrap_or(0);
        let idx = usize::from(cid) * 2;
        map[idx..idx + 2].copy_from_slice(&new_gid.to_be_bytes());
    }

    let deflated_font = deflate_level9(&subset)?;
    let deflated_map = deflate_level9(&map)?;
    // Net-smaller guard on stored bytes: the new font program plus the new
    // map stream must undercut the old font program plus the old map stream.
    if deflated_font.len() + deflated_map.len() >= font_file.content.len() + cid_map.stored_len {
        return None;
    }

    let base_name = base_font_name(doc, type0, descendant, descriptor)?;
    let tag = subset_tag(&subset);
    let mut tagged_name = Vec::with_capacity(base_name.len() + 7);
    tagged_name.extend_from_slice(&tag);
    tagged_name.push(b'+');
    tagged_name.extend_from_slice(strip_subset_tag(&base_name));

    Some(CidFontPlan {
        type0_id,
        descendant_id,
        descriptor_id,
        font_file_id,
        deflated_font,
        font_len: subset.len() as i64,
        deflated_map,
        tagged_name,
    })
}

fn base_font_name(
    doc: &Document,
    type0: &Dictionary,
    descendant: &Dictionary,
    descriptor: &Dictionary,
) -> Option<Vec<u8>> {
    for (dict, key) in [
        (descendant, b"BaseFont".as_slice()),
        (type0, b"BaseFont".as_slice()),
        (descriptor, b"FontName".as_slice()),
    ] {
        if let Ok(obj) = dict.get(key) {
            if let Object::Name(name) = resolve(doc, obj) {
                return Some(name.clone());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Simple-TrueType planning
// ---------------------------------------------------------------------------

/// A base encoding's code -> glyph-name table plus `/Differences` overrides.
type SimpleEncoding = (&'static [&'static str; 256], HashMap<u8, Vec<u8>>);

/// The font's `/Encoding`, reduced to what the planner needs: the base
/// code -> glyph-name table and any `/Differences` overrides. `None` for any
/// shape outside "explicit WinAnsi/MacRoman base (+ well-formed
/// Differences)" — an absent `/Encoding` means the font's built-in encoding,
/// whose semantics we decline to guess.
fn parse_simple_encoding(doc: &Document, font: &Dictionary) -> Option<SimpleEncoding> {
    fn base_table(name: &[u8]) -> Option<&'static [&'static str; 256]> {
        match name {
            b"WinAnsiEncoding" => Some(&encodings::WIN_ANSI_NAMES),
            b"MacRomanEncoding" => Some(&encodings::MAC_ROMAN_NAMES),
            _ => None,
        }
    }
    match resolve(doc, font.get(b"Encoding").ok()?) {
        Object::Name(n) => base_table(n).map(|t| (t, HashMap::new())),
        Object::Dictionary(d) => {
            let base = match d.get(b"BaseEncoding").map(|b| resolve(doc, b)) {
                Ok(Object::Name(n)) => base_table(n)?,
                // No explicit base: per spec the *font's* encoding fills the
                // gaps, which we cannot replicate. Decline.
                _ => return None,
            };
            Some((base, parse_differences(doc, d)?))
        }
        _ => None,
    }
}

/// The `/Differences` overrides of an encoding dictionary (empty when the
/// entry is absent); `None` for any malformed shape.
fn parse_differences(doc: &Document, d: &Dictionary) -> Option<HashMap<u8, Vec<u8>>> {
    let mut diffs: HashMap<u8, Vec<u8>> = HashMap::new();
    if let Ok(arr) = d.get(b"Differences") {
        let arr = resolve(doc, arr).as_array().ok()?;
        let mut code: i64 = -1;
        for item in arr {
            match resolve(doc, item) {
                Object::Integer(i) => {
                    if !(0..=255).contains(i) {
                        return None;
                    }
                    code = *i;
                }
                Object::Name(n) => {
                    let slot = u8::try_from(code).ok()?;
                    diffs.insert(slot, n.clone());
                    code += 1;
                }
                _ => return None,
            }
        }
    }
    Some(diffs)
}

/// Validate one used simple TrueType font end to end and build its plan.
/// Any failure — symbolic flags, unknown encoding or glyph name, a used code
/// the cmap paths cannot resolve, shared structure, subsetter error, cmap
/// round-trip mismatch, not net-smaller — returns `None` and the font ships
/// untouched.
fn plan_one_simple_font(
    doc: &Document,
    font_id: ObjectId,
    codes: &BTreeSet<u16>,
    refcounts: &HashMap<ObjectId, usize>,
    strip_hinting: bool,
) -> Option<SimpleFontPlan> {
    let font = doc.get_object(font_id).ok()?.as_dict().ok()?;
    // With /Widths present every shown glyph's advance comes from the PDF,
    // not the font program; without it we would have to prove the subset's
    // metrics tables serve unused codes identically. Decline instead.
    font.get(b"Widths").ok()?;
    let (base_names, diffs) = parse_simple_encoding(doc, font)?;

    let (descriptor_id, descriptor) = resolve_ref(doc, font.get(b"FontDescriptor").ok()?);
    let descriptor_id = descriptor_id?;
    let descriptor = descriptor.as_dict().ok()?;
    let flags = resolve(doc, descriptor.get(b"Flags").ok()?).as_i64().ok()?;
    // Nonsymbolic set and Symbolic clear: exactly the fonts whose lookup the
    // nonsymbolic paths of 9.6.6.4 (replicated below) fully describe.
    if flags & 0x04 != 0 || flags & 0x20 == 0 {
        return None;
    }
    if descriptor.get(b"FontFile").is_ok() || descriptor.get(b"FontFile3").is_ok() {
        return None;
    }
    let (font_file_id, font_file) = resolve_ref(doc, descriptor.get(b"FontFile2").ok()?);
    let font_file_id = font_file_id?;
    let font_file = font_file.as_stream().ok()?;
    // Shared descriptor/font-program structure could serve fonts whose usage
    // was not attributed here; mutating it would be unsound.
    if refcounts.get(&descriptor_id) != Some(&1) || refcounts.get(&font_file_id) != Some(&1) {
        return None;
    }

    let font_bytes = strict_stream_bytes(doc, font_file)?;
    let glyph_count = num_glyphs(&font_bytes)?;
    let subtables = truetype::parse_cmap(&font_bytes)?;
    let sub = |p: u16, e: u16| {
        subtables
            .iter()
            .find(|s| s.platform == p && s.encoding == e)
    };
    let (c31, c10, c30) = (sub(3, 1), sub(1, 0), sub(3, 0));
    if c31.is_none() && c10.is_none() {
        // Neither nonsymbolic lookup path exists; a viewer would be
        // improvising and so would we.
        return None;
    }

    // Resolve every used code through the union of the 9.6.6.4 lookup paths;
    // every glyph any path could select is retained. A code no path resolves
    // disqualifies the font (the original might still render it via
    // `post`-table names, which the subset would lose).
    let mut gids: BTreeSet<u16> = BTreeSet::new();
    gids.insert(0);
    for &code in codes {
        let code = u8::try_from(code).ok()?;
        let name: &[u8] = match diffs.get(&code) {
            Some(n) => n,
            None => {
                let n = base_names[usize::from(code)];
                if n.is_empty() {
                    return None; // shown code with no name in the encoding
                }
                n.as_bytes()
            }
        };
        let mut candidates: Vec<u16> = Vec::new();
        if let Some(sub) = c31 {
            if let Some(u) = encodings::glyph_name_to_unicode(name) {
                candidates.extend(sub.map.get(&u));
            }
        }
        if let Some(sub) = c10 {
            if let Some(mac) = encodings::mac_roman_code_of(name) {
                candidates.extend(sub.map.get(&u32::from(mac)));
            }
        }
        if let Some(sub) = c30 {
            candidates.extend(sub.map.get(&(0xF000 | u32::from(code))));
            candidates.extend(sub.map.get(&u32::from(code)));
        }
        if candidates.is_empty() {
            return None;
        }
        for gid in candidates {
            if gid >= glyph_count {
                return None;
            }
            gids.insert(gid);
        }
    }

    let gid_list: Vec<u16> = gids.iter().copied().collect();
    let remapper = GlyphRemapper::new_from_glyphs_sorted(&gid_list);
    let subset = subsetter::subset(&font_bytes, 0, &remapper).ok()?;
    if num_glyphs(&subset)? < remapper.num_gids() {
        return None;
    }

    // Replicate every original cmap subtable restricted to retained glyphs,
    // ids remapped. PDF simple-font lookups are BMP-only, so supplementary
    // aliases are dropped; platform 1 codes are bytes by definition.
    let mut new_subtables: Vec<truetype::CmapSubtable> = Vec::with_capacity(subtables.len());
    for sub in &subtables {
        let limit = if sub.platform == 1 { 0xFF } else { 0xFFFE };
        let mut map = BTreeMap::new();
        for (&ch, &gid) in &sub.map {
            if ch > limit {
                continue;
            }
            if let Some(new_gid) = remapper.get(gid) {
                if new_gid != 0 {
                    map.insert(ch, new_gid);
                }
            }
        }
        new_subtables.push(truetype::CmapSubtable {
            platform: sub.platform,
            encoding: sub.encoding,
            map,
        });
    }
    let final_font = truetype::insert_cmap(&subset, &new_subtables)?;
    let final_font = if strip_hinting {
        truetype::strip_hinting(&final_font).unwrap_or(final_font)
    } else {
        final_font
    };
    // See the CID path: masking the `name`-table subset tags makes identical
    // subsets byte-equal so the stream dedup can share one program.
    let final_font = truetype::mask_subset_tags(&final_font).unwrap_or(final_font);

    // Round-trip check: what a reader parses from the rebuilt font must be
    // exactly the mappings we intended to write.
    let mut reread = truetype::parse_cmap(&final_font)?;
    reread.sort_by_key(|s| (s.platform, s.encoding));
    new_subtables.sort_by_key(|s| (s.platform, s.encoding));
    if reread.len() != new_subtables.len()
        || reread
            .iter()
            .zip(&new_subtables)
            .any(|(a, b)| (a.platform, a.encoding, &a.map) != (b.platform, b.encoding, &b.map))
    {
        return None;
    }

    let deflated_font = deflate_level9(&final_font)?;
    // Net-smaller guard on stored bytes.
    if deflated_font.len() >= font_file.content.len() {
        return None;
    }

    let base_name = match font.get(b"BaseFont").map(|o| resolve(doc, o)) {
        Ok(Object::Name(n)) => n.clone(),
        _ => match descriptor.get(b"FontName").map(|o| resolve(doc, o)) {
            Ok(Object::Name(n)) => n.clone(),
            _ => return None,
        },
    };
    let tag = subset_tag(&final_font);
    let mut tagged_name = Vec::with_capacity(base_name.len() + 7);
    tagged_name.extend_from_slice(&tag);
    tagged_name.push(b'+');
    tagged_name.extend_from_slice(strip_subset_tag(&base_name));

    Some(SimpleFontPlan {
        font_id,
        descriptor_id,
        font_file_id,
        deflated_font,
        font_len: final_font.len() as i64,
        tagged_name,
    })
}

// ---------------------------------------------------------------------------
// Type1 → Type1C planning
// ---------------------------------------------------------------------------

/// The base of a Type1 font's effective encoding: an explicit Annex D table
/// or the font program's built-in encoding.
enum Type1Base {
    Builtin,
    Table(&'static [&'static str; 256]),
}

/// The font's `/Encoding`, reduced to base + `/Differences`. Unlike the
/// TrueType path, an absent `/Encoding` (or a dictionary without
/// `/BaseEncoding`) is fully supported: the built-in encoding it defers to
/// is parsed out of the font program itself and replicated in the emitted
/// CFF, so every viewer lookup path resolves identically.
fn parse_type1_encoding(
    doc: &Document,
    font: &Dictionary,
) -> Option<(Type1Base, HashMap<u8, Vec<u8>>)> {
    fn base_table(name: &[u8]) -> Option<&'static [&'static str; 256]> {
        match name {
            b"WinAnsiEncoding" => Some(&encodings::WIN_ANSI_NAMES),
            b"MacRomanEncoding" => Some(&encodings::MAC_ROMAN_NAMES),
            _ => None,
        }
    }
    match font.get(b"Encoding") {
        Err(_) => Some((Type1Base::Builtin, HashMap::new())),
        Ok(obj) => match resolve(doc, obj) {
            Object::Name(n) => Some((Type1Base::Table(base_table(n)?), HashMap::new())),
            Object::Dictionary(d) => {
                let base = match d.get(b"BaseEncoding").map(|b| resolve(doc, b)) {
                    Ok(Object::Name(n)) => Type1Base::Table(base_table(n)?),
                    Err(_) => Type1Base::Builtin,
                    _ => return None,
                };
                Some((base, parse_differences(doc, d)?))
            }
            _ => None,
        },
    }
}

/// Validate one used Type1 font end to end and build its Type1C conversion
/// plan. Any failure — unparseable font program, unknown encoding shape,
/// charstring anomalies, shared structure, not strictly smaller — returns
/// `None` and the font ships untouched.
fn plan_one_type1_font(
    doc: &Document,
    font_id: ObjectId,
    codes: &BTreeSet<u16>,
    refcounts: &HashMap<ObjectId, usize>,
) -> Option<Type1FontPlan> {
    let font = doc.get_object(font_id).ok()?.as_dict().ok()?;
    let (descriptor_id, descriptor) = resolve_ref(doc, font.get(b"FontDescriptor").ok()?);
    let descriptor_id = descriptor_id?;
    let descriptor = descriptor.as_dict().ok()?;
    // Exactly one font program, of the Type1 kind: a descriptor already
    // carrying a `/FontFile3` (or a TrueType program) is not ours to touch.
    if descriptor.get(b"FontFile2").is_ok() || descriptor.get(b"FontFile3").is_ok() {
        return None;
    }
    let (font_file_id, font_file) = resolve_ref(doc, descriptor.get(b"FontFile").ok()?);
    let font_file_id = font_file_id?;
    let font_file = font_file.as_stream().ok()?;
    // Shared descriptor/font-program structure could serve fonts whose usage
    // was not attributed here; mutating it would be unsound.
    if refcounts.get(&descriptor_id) != Some(&1) || refcounts.get(&font_file_id) != Some(&1) {
        return None;
    }

    let font_bytes = strict_stream_bytes(doc, font_file)?;
    let t1 = type1::parse(&font_bytes)?;
    let (base, diffs) = parse_type1_encoding(doc, font)?;

    // Resolve every used code to a glyph name through the same encoding the
    // viewer applies (`/Differences`, then the base). A code that resolves
    // to no name, or to a glyph the font does not carry, renders `.notdef`
    // before AND after conversion (the encoding objects never change), so it
    // constrains nothing.
    let mut keep: BTreeSet<Vec<u8>> = BTreeSet::new();
    for &code in codes {
        let code = u8::try_from(code).ok()?;
        let name: Option<Vec<u8>> = match diffs.get(&code) {
            Some(n) => Some(n.clone()),
            None => match &base {
                Type1Base::Table(table) => {
                    let n = table[usize::from(code)];
                    (!n.is_empty()).then(|| n.as_bytes().to_vec())
                }
                Type1Base::Builtin => t1.builtin_name(code).map(<[u8]>::to_vec),
            },
        };
        if let Some(name) = name {
            if t1.has_glyph(&name) {
                keep.insert(name);
            }
        }
    }

    let cff = type1::convert_to_cff(&t1, &keep)?;
    let deflated_cff = deflate_level9(&cff)?;
    // Strict-smaller guard on stored bytes, per font: never regress one.
    if deflated_cff.len() >= font_file.content.len() {
        return None;
    }

    let base_name = match font.get(b"BaseFont").map(|o| resolve(doc, o)) {
        Ok(Object::Name(n)) => n.clone(),
        _ => match descriptor.get(b"FontName").map(|o| resolve(doc, o)) {
            Ok(Object::Name(n)) => n.clone(),
            _ => return None,
        },
    };
    let tag = subset_tag(&cff);
    let mut tagged_name = Vec::with_capacity(base_name.len() + 7);
    tagged_name.extend_from_slice(&tag);
    tagged_name.push(b'+');
    tagged_name.extend_from_slice(strip_subset_tag(&base_name));

    Some(Type1FontPlan {
        font_id,
        descriptor_id,
        font_file_id,
        deflated_cff,
        tagged_name,
    })
}

/// Strict content decode, with the one tolerance `lopdf`'s parser needs.
///
/// `lopdf` does not know `d0`/`d1` — the two glyph-metric operators that, per
/// PDF 32000-1 §9.6.5, open a Type3 `/CharProcs` stream. It tokenizes `d1` as
/// the operator `d` plus a stray number `1`, which then binds as the *first
/// operand of the next operator*; a char proc that ends right after `d1`
/// fails outright. Either way the walk used to abort, and one abort discards
/// every font plan in the document (measured on a LaTeX paper: 713 KB of font
/// programs left untouched because of one 22-byte char proc).
///
/// So the metrics prefix is split off before parsing, not after a failure:
/// a stream that "parses" with the stray number attached is misparsed, and
/// the walker's operand checks are what stands between that and a wrong
/// glyph attribution. The tolerance is deliberately narrow — only a
/// *leading* run of numeric tokens followed by `d0`/`d1` at the matching
/// arity is removed, and neither operator shows text or selects a font, so
/// the operation sequence the walker inspects is unchanged. Every other
/// parse failure still declines, exactly as before.
fn decode_content_strict(content: &[u8]) -> Option<Content> {
    let body = strip_type3_metrics(content).unwrap_or(content);
    Content::decode_strict(body).ok()
}

/// Split off a leading `wx wy d0` / `wx wy llx lly urx ury d1` prefix,
/// returning the rest of the stream. `None` unless the stream opens with
/// exactly that: only numeric tokens may precede the operator, the operand
/// count must match it, and any delimiter (`(`, `<`, `[`, `/`, `%`, ...)
/// before it means this is not a Type3 metrics prefix.
fn strip_type3_metrics(content: &[u8]) -> Option<&[u8]> {
    fn is_ws(b: u8) -> bool {
        matches!(b, b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
    }
    fn is_delim(b: u8) -> bool {
        matches!(
            b,
            b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
        )
    }
    let mut i = 0usize;
    let mut operands = 0usize;
    loop {
        while i < content.len() && is_ws(content[i]) {
            i += 1;
        }
        let start = i;
        while i < content.len() && !is_ws(content[i]) && !is_delim(content[i]) {
            i += 1;
        }
        if i == start {
            // A delimiter, or end of stream, before any `d0`/`d1`.
            return None;
        }
        let token = &content[start..i];
        match token {
            b"d0" => return (operands == 2).then(|| &content[i..]),
            b"d1" => return (operands == 6).then(|| &content[i..]),
            _ => {
                if !is_number(token) || operands >= 6 {
                    return None;
                }
                operands += 1;
            }
        }
    }
}

/// A PDF numeric object token: optional sign, digits and at most one point,
/// with at least one digit.
fn is_number(token: &[u8]) -> bool {
    let body = match token.first() {
        Some(b'+' | b'-') => &token[1..],
        _ => token,
    };
    body.iter().filter(|&&b| b == b'.').count() <= 1
        && body.iter().any(u8::is_ascii_digit)
        && body.iter().all(|&b| b.is_ascii_digit() || b == b'.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{optimize, optimize_with_options, OptimizeOptions};
    use lopdf::content::Operation;
    use lopdf::StringFormat;

    fn subset_opts() -> OptimizeOptions {
        OptimizeOptions::default().with_subset_fonts(true)
    }

    fn noto_bytes() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/fonts/NotoSans-Regular.ttf"
        ))
        .expect("fixtures/fonts/NotoSans-Regular.ttf missing")
    }

    /// GID of each char in the fixture font. Under `/CIDToGIDMap /Identity`,
    /// these double as the CIDs used in show strings.
    fn gids_for(text: &str) -> Vec<u16> {
        let data = noto_bytes();
        let face = ttf_parser::Face::parse(&data, 0).unwrap();
        text.chars()
            .map(|c| face.glyph_index(c).expect("fixture glyph missing").0)
            .collect()
    }

    /// A minimal ToUnicode CMap so text extraction has a CID -> Unicode
    /// oracle that must survive subsetting untouched.
    fn to_unicode_bytes(pairs: &[(u16, char)]) -> Vec<u8> {
        let mut body = String::new();
        for (cid, ch) in pairs {
            let mut units = [0u16; 2];
            let encoded = ch.encode_utf16(&mut units);
            let target: String = encoded.iter().map(|u| format!("{u:04X}")).collect();
            body.push_str(&format!("<{cid:04X}> <{target}>\u{a}"));
        }
        let mut cmap = String::new();
        for line in [
            "/CIDInit /ProcSet findresource begin",
            "12 dict begin",
            "begincmap",
            "/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def",
            "/CMapName /Adobe-Identity-UCS def",
            "/CMapType 2 def",
            "1 begincodespacerange",
            "<0000> <FFFF>",
            "endcodespacerange",
        ] {
            cmap.push_str(line);
            cmap.push('\u{a}');
        }
        cmap.push_str(&format!(
            "{} beginbfchar\u{a}{body}endbfchar\u{a}",
            pairs.len()
        ));
        for line in [
            "endcmap",
            "CMapName currentdict /CMap defineresource pop",
            "end",
            "end",
        ] {
            cmap.push_str(line);
            cmap.push('\u{a}');
        }
        cmap.into_bytes()
    }

    struct FontSpec {
        /// `None` => `/CIDToGIDMap /Identity`; `Some(table)` => a stream
        /// mapping CID i -> table[i].
        cid_table: Option<Vec<u16>>,
        base_font: &'static str,
        corrupt_font_file: bool,
        to_unicode: Vec<(u16, char)>,
    }

    impl FontSpec {
        fn identity(to_unicode: Vec<(u16, char)>) -> Self {
            FontSpec {
                cid_table: None,
                base_font: "NotoSans-Regular",
                corrupt_font_file: false,
                to_unicode,
            }
        }
    }

    /// Add a complete Type0/CIDFontType2/Identity-H font to `doc`, returning
    /// the Type0 font object id.
    fn add_type0_font(doc: &mut Document, spec: &FontSpec) -> ObjectId {
        let font_data = if spec.corrupt_font_file {
            b"this is not a truetype font at all".to_vec()
        } else {
            noto_bytes()
        };
        let font_len = font_data.len() as i64;
        let ff_id = doc.add_object(
            Stream::new(
                dictionary! { "Filter" => "FlateDecode", "Length1" => font_len },
                deflate_level9(&font_data).unwrap(),
            )
            .with_compression(false),
        );
        let descr_id = doc.add_object(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => Object::Name(spec.base_font.as_bytes().to_vec()),
            "Flags" => 32,
            "FontBBox" => vec![(-619).into(), (-293).into(), 1536.into(), 1069.into()],
            "ItalicAngle" => 0,
            "Ascent" => 1069,
            "Descent" => (-293),
            "CapHeight" => 714,
            "StemV" => 80,
            "FontFile2" => ff_id,
        });
        let cid_map_obj: Object = match &spec.cid_table {
            None => Object::Name(b"Identity".to_vec()),
            Some(table) => {
                let mut bytes = Vec::with_capacity(table.len() * 2);
                for gid in table {
                    bytes.extend_from_slice(&gid.to_be_bytes());
                }
                doc.add_object(Stream::new(dictionary! {}, bytes)).into()
            }
        };
        let desc_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "CIDFontType2",
            "BaseFont" => Object::Name(spec.base_font.as_bytes().to_vec()),
            "CIDSystemInfo" => dictionary! {
                "Registry" => Object::String(b"Adobe".to_vec(), StringFormat::Literal),
                "Ordering" => Object::String(b"Identity".to_vec(), StringFormat::Literal),
                "Supplement" => 0,
            },
            "FontDescriptor" => descr_id,
            "DW" => 600,
            "CIDToGIDMap" => cid_map_obj,
        });
        let tou_id = doc.add_object(Stream::new(
            dictionary! {},
            to_unicode_bytes(&spec.to_unicode),
        ));
        doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type0",
            "BaseFont" => Object::Name(spec.base_font.as_bytes().to_vec()),
            "Encoding" => "Identity-H",
            "DescendantFonts" => vec![desc_id.into()],
            "ToUnicode" => tou_id,
        })
    }

    fn show_text_ops(font: &str, cids: &[u16]) -> Vec<Operation> {
        let mut bytes = Vec::with_capacity(cids.len() * 2);
        for cid in cids {
            bytes.extend_from_slice(&cid.to_be_bytes());
        }
        vec![
            Operation::new("BT", vec![]),
            // 10pt: even the longest test string stays inside the MediaBox
            // (off-page text would be dropped by the pdftotext oracle).
            Operation::new(
                "Tf",
                vec![Object::Name(font.as_bytes().to_vec()), 10.into()],
            ),
            Operation::new("Td", vec![72.into(), 700.into()]),
            Operation::new("Tj", vec![Object::String(bytes, StringFormat::Hexadecimal)]),
            Operation::new("ET", vec![]),
        ]
    }

    fn finish_pdf(doc: &mut Document, pages_id: ObjectId, page_ids: Vec<ObjectId>) -> Vec<u8> {
        let kids: Vec<Object> = page_ids.iter().map(|&id| id.into()).collect();
        let count = page_ids.len() as i64;
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => count,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn add_text_page(
        doc: &mut Document,
        pages_id: ObjectId,
        font_id: ObjectId,
        content: Vec<Operation>,
    ) -> ObjectId {
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            Content {
                operations: content,
            }
            .encode()
            .unwrap(),
        ));
        doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! {
                "Font" => dictionary! { "F1" => font_id },
            },
        })
    }

    /// One-page-per-entry text PDF over a single shared font.
    fn build_text_pdf(spec: &FontSpec, pages: &[Vec<u16>]) -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let font_id = add_type0_font(&mut doc, spec);
        let page_ids: Vec<ObjectId> = pages
            .iter()
            .map(|cids| add_text_page(&mut doc, pages_id, font_id, show_text_ops("F1", cids)))
            .collect();
        finish_pdf(&mut doc, pages_id, page_ids)
    }

    // -- output inspection ---------------------------------------------------

    struct SubsetView {
        type0: Dictionary,
        descendant: Dictionary,
        descriptor: Dictionary,
        /// Decompressed subset font program.
        font: Vec<u8>,
        /// Decompressed CID -> GID map stream, when the map is a stream.
        cid_map: Option<Vec<u8>>,
        /// The re-loaded output document (for follow-up stream lookups).
        doc: Document,
    }

    fn subset_view(pdf: &[u8]) -> SubsetView {
        let doc = Document::load_mem(pdf).unwrap();
        let mut found = None;
        for obj in doc.objects.values() {
            let Object::Dictionary(type0) = obj else {
                continue;
            };
            if !matches!(type0.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Type0") {
                continue;
            }
            let descendants = resolve(&doc, type0.get(b"DescendantFonts").unwrap())
                .as_array()
                .unwrap()
                .clone();
            let descendant = resolve(&doc, &descendants[0]).as_dict().unwrap().clone();
            let descriptor = resolve(&doc, descendant.get(b"FontDescriptor").unwrap())
                .as_dict()
                .unwrap()
                .clone();
            let ff = resolve(&doc, descriptor.get(b"FontFile2").unwrap())
                .as_stream()
                .unwrap();
            let font = strict_stream_bytes(&doc, ff).expect("font stream must inflate");
            let cid_map = match descendant.get(b"CIDToGIDMap") {
                Ok(obj) => match resolve(&doc, obj) {
                    Object::Stream(s) => {
                        Some(strict_stream_bytes(&doc, s).expect("map must inflate"))
                    }
                    _ => None,
                },
                Err(_) => None,
            };
            found = Some((type0.clone(), descendant, descriptor, font, cid_map));
            break;
        }
        let (type0, descendant, descriptor, font, cid_map) =
            found.expect("no Type0 font in output");
        SubsetView {
            type0,
            descendant,
            descriptor,
            font,
            cid_map,
            doc,
        }
    }

    impl SubsetView {
        /// New GID for an old CID, through the rewritten map stream.
        fn new_gid(&self, cid: u16) -> u16 {
            let map = self.cid_map.as_ref().expect("CIDToGIDMap must be a stream");
            be16(map, usize::from(cid) * 2).unwrap_or(0)
        }
    }

    /// Record a glyph outline as a comparable op list.
    #[derive(Default)]
    struct Outline(Vec<String>);

    impl ttf_parser::OutlineBuilder for Outline {
        fn move_to(&mut self, x: f32, y: f32) {
            self.0.push(format!("M {x} {y}"));
        }
        fn line_to(&mut self, x: f32, y: f32) {
            self.0.push(format!("L {x} {y}"));
        }
        fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
            self.0.push(format!("Q {x1} {y1} {x} {y}"));
        }
        fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
            self.0.push(format!("C {x1} {y1} {x2} {y2} {x} {y}"));
        }
        fn close(&mut self) {
            self.0.push("Z".to_string());
        }
    }

    fn outline_and_advance(font: &[u8], gid: u16) -> (Vec<String>, Option<u16>) {
        let face = ttf_parser::Face::parse(font, 0).expect("font must parse");
        let mut rec = Outline::default();
        let id = ttf_parser::GlyphId(gid);
        face.outline_glyph(id, &mut rec);
        (rec.0, face.glyph_hor_advance(id))
    }

    /// Assert that the old GID (in the original font) and the subset's mapped
    /// GID draw the identical outline with the identical advance.
    fn assert_glyph_preserved(view: &SubsetView, old_font: &[u8], cid: u16, old_gid: u16) {
        let (old_outline, old_adv) = outline_and_advance(old_font, old_gid);
        let (new_outline, new_adv) = outline_and_advance(&view.font, view.new_gid(cid));
        // Both may legitimately be empty (e.g. the space glyph); what matters
        // is equality of outline and advance.
        assert_eq!(old_outline, new_outline, "outline mismatch for CID {cid}");
        assert_eq!(old_adv, new_adv, "advance mismatch for CID {cid}");
    }

    fn base_name_of(dict: &Dictionary, key: &[u8]) -> Vec<u8> {
        match dict.get(key) {
            Ok(Object::Name(n)) => n.clone(),
            other => panic!(
                "expected name at {}: {other:?}",
                String::from_utf8_lossy(key)
            ),
        }
    }

    // -- the battery ---------------------------------------------------------

    #[test]
    fn subset_shrinks_and_never_touches_text_or_cid_keyed_tables() {
        let cids = gids_for("Hello, World!");
        let pairs: Vec<(u16, char)> = cids.iter().copied().zip("Hello, World!".chars()).collect();
        let pdf = build_text_pdf(
            &FontSpec::identity(pairs.clone()),
            std::slice::from_ref(&cids),
        );
        let out = optimize_with_options(&pdf, subset_opts());

        assert!(out.len() < pdf.len(), "subsetting must shrink the file");

        // Content-stream bytes are the M1 superpower: byte-identical.
        let pre = Document::load_mem(&pdf).unwrap();
        let post = Document::load_mem(&out).unwrap();
        let pre_page = *pre.get_pages().get(&1).unwrap();
        let post_page = *post.get_pages().get(&1).unwrap();
        assert_eq!(
            pre.get_page_content(pre_page),
            post.get_page_content(post_page),
            "content stream must be byte-identical"
        );

        let view = subset_view(&out);
        // CID-keyed tables untouched.
        assert_eq!(
            view.descendant.get(b"DW").unwrap().as_i64().unwrap(),
            600,
            "/DW must be untouched"
        );
        // ToUnicode untouched (compare decompressed bytes: the save path may
        // Flate-wrap the previously raw stream).
        let tou = resolve(&view.doc, view.type0.get(b"ToUnicode").unwrap())
            .as_stream()
            .unwrap();
        let tou_bytes = tou
            .decompressed_content()
            .unwrap_or_else(|_| tou.content.clone());
        assert_eq!(
            tou_bytes,
            to_unicode_bytes(&pairs),
            "/ToUnicode must be untouched"
        );

        // Names re-tagged consistently, map now a stream, font smaller.
        let tagged = base_name_of(&view.type0, b"BaseFont");
        assert_eq!(tagged.len(), "NotoSans-Regular".len() + 7);
        assert_eq!(tagged[6], b'+');
        assert!(tagged[..6].iter().all(u8::is_ascii_uppercase));
        assert_eq!(&tagged[7..], b"NotoSans-Regular");
        assert_eq!(base_name_of(&view.descendant, b"BaseFont"), tagged);
        assert_eq!(base_name_of(&view.descriptor, b"FontName"), tagged);
        assert!(view.cid_map.is_some(), "CIDToGIDMap must now be a stream");
        let original = noto_bytes();
        assert!(
            view.font.len() < original.len() / 4,
            "subset must be much smaller than the full font"
        );

        // Every used glyph: outline + advance equality.
        for &cid in &cids {
            assert_glyph_preserved(&view, &original, cid, cid);
        }
    }

    #[test]
    fn text_extraction_is_identical_pre_and_post() {
        let text = "The quick brown fox";
        let cids = gids_for(text);
        let pairs: Vec<(u16, char)> = cids.iter().copied().zip(text.chars()).collect();
        let pdf = build_text_pdf(&FontSpec::identity(pairs), &[cids]);
        let out = optimize_with_options(&pdf, subset_opts());
        assert!(out.len() < pdf.len());

        let pre = Document::load_mem(&pdf).unwrap().extract_text(&[1]);
        let post = Document::load_mem(&out).unwrap().extract_text(&[1]);
        let pre = pre.expect("fixture text must extract");
        let post = post.expect("subset text must extract");
        assert_eq!(pre, post, "extracted text must be identical");
        assert!(
            pre.contains("The quick brown fox"),
            "oracle must see the text"
        );
    }

    #[test]
    fn composite_glyph_closure_is_preserved() {
        // "é" and "ü" are composite glyphs (base + accent) in Noto Sans; the
        // subsetter must pull their component glyphs into the subset or the
        // outline comparison fails.
        let text = "éü";
        let cids = gids_for(text);
        let pairs: Vec<(u16, char)> = cids.iter().copied().zip(text.chars()).collect();
        let pdf = build_text_pdf(&FontSpec::identity(pairs), std::slice::from_ref(&cids));
        let out = optimize_with_options(&pdf, subset_opts());
        assert!(out.len() < pdf.len());

        let view = subset_view(&out);
        let original = noto_bytes();
        for &cid in &cids {
            assert_glyph_preserved(&view, &original, cid, cid);
        }
        // Closure must have added component glyphs beyond .notdef + the two
        // composites we asked for.
        let face = ttf_parser::Face::parse(&view.font, 0).unwrap();
        assert!(
            face.number_of_glyphs() > 3,
            "composite components missing: only {} glyphs",
            face.number_of_glyphs()
        );
    }

    #[test]
    fn cid_to_gid_map_stream_input_is_composed() {
        // Non-identity input mapping: CID 1 -> 'a', CID 2 -> 'b', CID 3 -> 'c'.
        let abc = gids_for("abc");
        let spec = FontSpec {
            cid_table: Some(vec![0, abc[0], abc[1], abc[2]]),
            base_font: "NotoSans-Regular",
            corrupt_font_file: false,
            to_unicode: vec![(1, 'a'), (2, 'b'), (3, 'c')],
        };
        let pdf = build_text_pdf(&spec, &[vec![1, 2, 3]]);
        let out = optimize_with_options(&pdf, subset_opts());
        assert!(out.len() < pdf.len());

        let view = subset_view(&out);
        let original = noto_bytes();
        for (cid, &old_gid) in (1u16..=3).zip(abc.iter()) {
            assert_glyph_preserved(&view, &original, cid, old_gid);
        }
    }

    #[test]
    fn shared_font_accumulates_across_pages() {
        let page1 = gids_for("abc");
        let page2 = gids_for("xyz");
        let pairs: Vec<(u16, char)> = page1
            .iter()
            .chain(page2.iter())
            .copied()
            .zip("abcxyz".chars())
            .collect();
        let pdf = build_text_pdf(&FontSpec::identity(pairs), &[page1.clone(), page2.clone()]);
        let out = optimize_with_options(&pdf, subset_opts());
        assert!(out.len() < pdf.len());

        let view = subset_view(&out);
        let original = noto_bytes();
        for &cid in page1.iter().chain(page2.iter()) {
            assert_glyph_preserved(&view, &original, cid, cid);
        }
    }

    #[test]
    fn glyphs_in_forms_and_annotation_appearances_are_found() {
        let page_cids = gids_for("ab");
        let form_cids = gids_for("cd");
        let ap_cids = gids_for("ef");
        let pairs: Vec<(u16, char)> = page_cids
            .iter()
            .chain(form_cids.iter())
            .chain(ap_cids.iter())
            .copied()
            .zip("abcdef".chars())
            .collect();

        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let font_id = add_type0_font(&mut doc, &FontSpec::identity(pairs));

        let form_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font_id } },
            },
            Content {
                operations: show_text_ops("F1", &form_cids),
            }
            .encode()
            .unwrap(),
        ));
        let ap_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 200.into(), 50.into()],
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font_id } },
            },
            Content {
                operations: show_text_ops("F1", &ap_cids),
            }
            .encode()
            .unwrap(),
        ));
        let annot_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Square",
            "Rect" => vec![0.into(), 0.into(), 200.into(), 50.into()],
            "AP" => dictionary! { "N" => ap_id },
        });

        let mut ops = show_text_ops("F1", &page_cids);
        ops.push(Operation::new("Do", vec![Object::Name(b"Fm1".to_vec())]));
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            Content { operations: ops }.encode().unwrap(),
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! {
                "Font" => dictionary! { "F1" => font_id },
                "XObject" => dictionary! { "Fm1" => form_id },
            },
            "Annots" => vec![annot_id.into()],
        });
        let pdf = finish_pdf(&mut doc, pages_id, vec![page_id]);

        let out = optimize_with_options(&pdf, subset_opts());
        assert!(out.len() < pdf.len(), "subsetting must still shrink");

        let view = subset_view(&out);
        let original = noto_bytes();
        for &cid in page_cids
            .iter()
            .chain(form_cids.iter())
            .chain(ap_cids.iter())
        {
            assert_glyph_preserved(&view, &original, cid, cid);
        }
    }

    #[test]
    fn type3_metrics_prefix_is_split_off_only_when_well_formed() {
        // d1: six operands. d0: two.
        assert_eq!(
            strip_type3_metrics(b"0.27 0 0 0 0 0 d1\n1 0 0 1 0 0 cm"),
            Some(&b"\n1 0 0 1 0 0 cm"[..])
        );
        assert_eq!(strip_type3_metrics(b"12 0 d0 BT"), Some(&b" BT"[..]));
        // Wrong operand count for the operator: not a metrics prefix.
        assert_eq!(strip_type3_metrics(b"0 0 0 d1 BT"), None);
        assert_eq!(strip_type3_metrics(b"1 2 3 d0"), None);
        // A delimiter or a non-numeric token before the operator.
        assert_eq!(strip_type3_metrics(b"BT /F1 12 Tf"), None);
        assert_eq!(strip_type3_metrics(b"1 2 (s) Tj"), None);
        assert_eq!(strip_type3_metrics(b"0 0 0 0 0 0 0 0 d1"), None);
        // No operator at all.
        assert_eq!(strip_type3_metrics(b"1 2 3 4"), None);
        assert_eq!(strip_type3_metrics(b""), None);
    }

    #[test]
    fn type3_char_procs_parse_instead_of_aborting_the_walk() {
        // lopdf rejects `d1` outright, so the tolerance is what makes this
        // stream readable at all; the remaining operators must survive.
        let charproc = b"0.277832 0 0 0 0 0 d1\nBT /F1 12 Tf (Hi) Tj ET".to_vec();
        // lopdf reads `d1` as operator `d` plus a stray `1` that binds to the
        // next operator -- a misparse, not a parse.
        let lopdf_ops: Vec<(String, usize)> = Content::decode_strict(&charproc)
            .expect("lopdf accepts it, wrongly")
            .operations
            .iter()
            .map(|o| (o.operator.clone(), o.operands.len()))
            .collect();
        assert_eq!(lopdf_ops[0], ("d".to_string(), 6));
        assert_eq!(lopdf_ops[1], ("BT".to_string(), 1), "stray operand shifted");
        // A char proc that ends at `d1` does not parse at all.
        assert!(Content::decode_strict(b"0.277832 0 0 0 0 0 d1\n").is_err());

        let parsed = decode_content_strict(&charproc).expect("d1 prefix split off");
        let ops: Vec<&str> = parsed
            .operations
            .iter()
            .map(|o| o.operator.as_str())
            .collect();
        assert_eq!(ops, ["BT", "Tf", "Tj", "ET"]);
        // Still strict about everything else.
        assert!(decode_content_strict(b"(unterminated").is_none());
        assert!(decode_content_strict(b"0 0 0 0 0 0 d1 (unterminated").is_none());
    }

    #[test]
    fn a_type3_glyph_no_longer_disables_subsetting_document_wide() {
        let cids = gids_for("Hello");
        let pairs: Vec<(u16, char)> = cids.iter().copied().zip("Hello".chars()).collect();
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let font_id = add_type0_font(&mut doc, &FontSpec::identity(pairs));
        let text_page = add_text_page(&mut doc, pages_id, font_id, show_text_ops("F1", &cids));

        // A Type3 font whose one char proc opens with `d1`, drawn on its own
        // page. Nothing about it constrains the Type0 font on page 1.
        let proc_id = doc.add_object(Stream::new(
            dictionary! {},
            b"10 0 0 0 10 10 d1\n0 0 10 10 re f".to_vec(),
        ));
        let t3_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type3",
            "FontBBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            "FontMatrix" => vec![
                0.001.into(), 0.into(), 0.into(), 0.001.into(), 0.into(), 0.into(),
            ],
            "CharProcs" => dictionary! { "a" => proc_id },
            "Encoding" => dictionary! {
                "Type" => "Encoding",
                "Differences" => vec![97.into(), "a".into()],
            },
            "FirstChar" => 97,
            "LastChar" => 97,
            "Widths" => vec![10.into()],
        });
        let t3_content = doc.add_object(Stream::new(
            dictionary! {},
            b"BT /T3 12 Tf (a) Tj ET".to_vec(),
        ));
        let t3_page = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => t3_content,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! { "Font" => dictionary! { "T3" => t3_id } },
        });
        let pdf = finish_pdf(&mut doc, pages_id, vec![text_page, t3_page]);

        let out = optimize_with_options(&pdf, subset_opts());
        assert!(
            out.len() < pdf.len(),
            "the Type0 font must still be subsetted alongside a Type3 glyph"
        );
        let view = subset_view(&out);
        assert!(
            view.font.len() < noto_bytes().len(),
            "font program should have shrunk"
        );
    }

    #[test]
    fn unparseable_content_stream_disables_all_subsetting() {
        let cids = gids_for("Hello");
        let pairs: Vec<(u16, char)> = cids.iter().copied().zip("Hello".chars()).collect();
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let font_id = add_type0_font(&mut doc, &FontSpec::identity(pairs));
        let text_page = add_text_page(&mut doc, pages_id, font_id, show_text_ops("F1", &cids));
        // Page 2: an unterminated string literal that content parsing rejects.
        let bad_content = doc.add_object(Stream::new(
            dictionary! {},
            b"(this string never terminates".to_vec(),
        ));
        let bad_page = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => bad_content,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! {},
        });
        let pdf = finish_pdf(&mut doc, pages_id, vec![text_page, bad_page]);

        let out = optimize_with_options(&pdf, subset_opts());
        assert_eq!(
            out, pdf,
            "one unparseable stream must disable subsetting entirely"
        );
    }

    #[test]
    fn extgstate_font_entry_disables_all_subsetting() {
        // An ExtGState /Font selects a font without Tf; the walker cannot
        // attribute subsequent shows, so the document must be left alone.
        let cids = gids_for("Hello");
        let pairs: Vec<(u16, char)> = cids.iter().copied().zip("Hello".chars()).collect();
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let font_id = add_type0_font(&mut doc, &FontSpec::identity(pairs));
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            Content {
                operations: show_text_ops("F1", &cids),
            }
            .encode()
            .unwrap(),
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! {
                "Font" => dictionary! { "F1" => font_id },
                "ExtGState" => dictionary! {
                    "GS0" => dictionary! { "Font" => vec![font_id.into(), 12.into()] },
                },
            },
        });
        let pdf = finish_pdf(&mut doc, pages_id, vec![page_id]);

        let out = optimize_with_options(&pdf, subset_opts());
        assert_eq!(out, pdf, "ExtGState /Font must disable subsetting");
    }

    #[test]
    fn corrupt_font_file_leaves_font_untouched() {
        let cids = gids_for("Hi");
        let spec = FontSpec {
            corrupt_font_file: true,
            ..FontSpec::identity(cids.iter().copied().zip("Hi".chars()).collect())
        };
        let pdf = build_text_pdf(&spec, &[cids]);
        let out = optimize_with_options(&pdf, subset_opts());
        assert_eq!(out, pdf, "corrupt FontFile2 must return original bytes");
    }

    #[test]
    fn referenced_but_unused_font_is_untouched() {
        // A font in the resources that never shows text: no usage evidence,
        // so it ships untouched ("any uncertainty => leave it alone").
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let font_id = add_type0_font(&mut doc, &FontSpec::identity(vec![]));
        let page = add_text_page(&mut doc, pages_id, font_id, vec![]);
        let pdf = finish_pdf(&mut doc, pages_id, vec![page]);

        let out = optimize_with_options(&pdf, subset_opts());
        assert_eq!(out, pdf, "unused font must be untouched");
    }

    #[test]
    fn pdfa_declared_documents_are_skipped() {
        let cids = gids_for("Hello");
        let pairs: Vec<(u16, char)> = cids.iter().copied().zip("Hello".chars()).collect();
        let pdf = build_text_pdf(&FontSpec::identity(pairs), &[cids]);

        // Re-open and attach a PDF/A XMP metadata stream to the catalog.
        let mut doc = Document::load_mem(&pdf).unwrap();
        let xmp = concat!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">"#,
            r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">"#,
            r#"<rdf:Description rdf:about="" xmlns:pdfaid="http://www.aiim.org/pdfa/ns/id/">"#,
            r#"<pdfaid:part>2</pdfaid:part><pdfaid:conformance>B</pdfaid:conformance>"#,
            r#"</rdf:Description></rdf:RDF></x:xmpmeta>"#,
        );
        let meta_id = doc.add_object(Stream::new(
            dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
            xmp.as_bytes().to_vec(),
        ));
        if let Ok(catalog) = doc.catalog_mut() {
            catalog.set("Metadata", Object::Reference(meta_id));
        }
        let mut pdfa: Vec<u8> = Vec::new();
        doc.save_to(&mut pdfa).unwrap();

        let out = optimize_with_options(&pdfa, subset_opts());
        assert_eq!(out, pdfa, "PDF/A-declared document must be skipped");
    }

    #[test]
    fn subset_fonts_is_on_by_default_and_opt_out_restores_original() {
        let cids = gids_for("Hello");
        let pairs: Vec<(u16, char)> = cids.iter().copied().zip("Hello".chars()).collect();
        let pdf = build_text_pdf(&FontSpec::identity(pairs), &[cids]);
        let on = optimize(&pdf);
        assert_eq!(
            on,
            optimize_with_options(&pdf, subset_opts()),
            "default options must subset fonts"
        );
        assert!(on.len() < pdf.len());
        let off = optimize_with_options(&pdf, OptimizeOptions::default().with_subset_fonts(false));
        assert_eq!(off, pdf, "opt-out must not touch fonts");
    }

    #[test]
    fn existing_subset_tag_is_replaced_not_stacked() {
        let cids = gids_for("Hello");
        let spec = FontSpec {
            base_font: "ABCDEF+NotoSans-Regular",
            ..FontSpec::identity(cids.iter().copied().zip("Hello".chars()).collect())
        };
        let pdf = build_text_pdf(&spec, &[cids]);
        let out = optimize_with_options(&pdf, subset_opts());
        assert!(out.len() < pdf.len());

        let view = subset_view(&out);
        let tagged = base_name_of(&view.type0, b"BaseFont");
        assert_eq!(
            &tagged[7..],
            b"NotoSans-Regular",
            "old tag must be stripped"
        );
        assert_eq!(tagged.iter().filter(|&&b| b == b'+').count(), 1);
    }

    #[test]
    fn subsetting_is_idempotent() {
        let cids = gids_for("Hello, World!");
        let pairs: Vec<(u16, char)> = cids.iter().copied().zip("Hello, World!".chars()).collect();
        let pdf = build_text_pdf(&FontSpec::identity(pairs), &[cids]);
        let once = optimize_with_options(&pdf, subset_opts());
        assert!(once.len() < pdf.len(), "first pass must shrink");
        let twice = optimize_with_options(&once, subset_opts());
        assert_eq!(twice, once, "second pass must be byte-stable");
    }

    // -- simple TrueType -----------------------------------------------------

    struct SimpleSpec {
        base_font: &'static str,
        /// The `/Encoding` entry; `None` omits it (built-in encoding).
        encoding: Option<Object>,
        /// FontDescriptor `/Flags` (32 = Nonsymbolic).
        flags: i64,
        corrupt_font_file: bool,
    }

    impl SimpleSpec {
        fn winansi() -> Self {
            SimpleSpec {
                base_font: "NotoSans-Regular",
                encoding: Some(Object::Name(b"WinAnsiEncoding".to_vec())),
                flags: 32,
                corrupt_font_file: false,
            }
        }
    }

    /// Add a complete simple TrueType font to `doc`, returning the font
    /// object id.
    fn add_simple_tt_font(doc: &mut Document, spec: &SimpleSpec) -> ObjectId {
        let font_data = if spec.corrupt_font_file {
            b"this is not a truetype font at all".to_vec()
        } else {
            noto_bytes()
        };
        let font_len = font_data.len() as i64;
        let ff_id = doc.add_object(
            Stream::new(
                dictionary! { "Filter" => "FlateDecode", "Length1" => font_len },
                deflate_level9(&font_data).unwrap(),
            )
            .with_compression(false),
        );
        let descr_id = doc.add_object(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => Object::Name(spec.base_font.as_bytes().to_vec()),
            "Flags" => spec.flags,
            "FontBBox" => vec![(-619).into(), (-293).into(), 1536.into(), 1069.into()],
            "ItalicAngle" => 0,
            "Ascent" => 1069,
            "Descent" => (-293),
            "CapHeight" => 714,
            "StemV" => 80,
            "FontFile2" => ff_id,
        });
        let widths: Vec<Object> = (32..=255).map(|_| 500.into()).collect();
        let mut font = dictionary! {
            "Type" => "Font",
            "Subtype" => "TrueType",
            "BaseFont" => Object::Name(spec.base_font.as_bytes().to_vec()),
            "FirstChar" => 32,
            "LastChar" => 255,
            "Widths" => widths,
            "FontDescriptor" => descr_id,
        };
        if let Some(enc) = &spec.encoding {
            font.set("Encoding", enc.clone());
        }
        doc.add_object(font)
    }

    /// Show single-byte codes (simple-font semantics).
    fn show_byte_ops(font: &str, codes: &[u8]) -> Vec<Operation> {
        vec![
            Operation::new("BT", vec![]),
            Operation::new(
                "Tf",
                vec![Object::Name(font.as_bytes().to_vec()), 10.into()],
            ),
            Operation::new("Td", vec![72.into(), 700.into()]),
            Operation::new(
                "Tj",
                vec![Object::String(codes.to_vec(), StringFormat::Hexadecimal)],
            ),
            Operation::new("ET", vec![]),
        ]
    }

    fn build_simple_pdf(spec: &SimpleSpec, codes: &[u8]) -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let font_id = add_simple_tt_font(&mut doc, spec);
        let page = add_text_page(&mut doc, pages_id, font_id, show_byte_ops("F1", codes));
        finish_pdf(&mut doc, pages_id, vec![page])
    }

    /// The (first) simple TrueType font program in the output, decompressed,
    /// plus the font dictionary.
    fn simple_font_view(pdf: &[u8]) -> (Dictionary, Vec<u8>) {
        let doc = Document::load_mem(pdf).unwrap();
        for obj in doc.objects.values() {
            let Object::Dictionary(font) = obj else {
                continue;
            };
            if !matches!(font.get(b"Subtype"), Ok(Object::Name(n)) if n == b"TrueType") {
                continue;
            }
            let descriptor = resolve(&doc, font.get(b"FontDescriptor").unwrap())
                .as_dict()
                .unwrap();
            let ff = resolve(&doc, descriptor.get(b"FontFile2").unwrap())
                .as_stream()
                .unwrap();
            let bytes = strict_stream_bytes(&doc, ff).expect("font stream must inflate");
            return (font.clone(), bytes);
        }
        panic!("no simple TrueType font in output");
    }

    /// Assert the subset font resolves `ch` (through its own cmap, the
    /// viewer's lookup path) to the same outline and advance as the original.
    fn assert_char_preserved(subset: &[u8], original: &[u8], ch: char) {
        let orig_face = ttf_parser::Face::parse(original, 0).unwrap();
        let new_face = ttf_parser::Face::parse(subset, 0).unwrap();
        let old_gid = orig_face.glyph_index(ch).expect("original glyph missing");
        let new_gid = new_face
            .glyph_index(ch)
            .unwrap_or_else(|| panic!("subset cmap must map {ch:?}"));
        let (old_outline, old_adv) = outline_and_advance(original, old_gid.0);
        let (new_outline, new_adv) = outline_and_advance(subset, new_gid.0);
        assert_eq!(old_outline, new_outline, "outline mismatch for {ch:?}");
        assert_eq!(old_adv, new_adv, "advance mismatch for {ch:?}");
    }

    #[test]
    fn simple_winansi_font_is_subsetted_with_glyphs_preserved() {
        // 0xE9 = eacute, 0xFC = udieresis in WinAnsi; both are composite
        // glyphs in Noto Sans, so this also exercises glyph closure.
        let codes = b"Hello, World! \xE9\xFC";
        let text = "Hello, World! \u{e9}\u{fc}";
        let pdf = build_simple_pdf(&SimpleSpec::winansi(), codes);
        let out = optimize_with_options(&pdf, subset_opts());
        assert!(out.len() < pdf.len(), "subsetting must shrink the file");

        // Content stream untouched.
        let pre = Document::load_mem(&pdf).unwrap();
        let post = Document::load_mem(&out).unwrap();
        let pre_page = *pre.get_pages().get(&1).unwrap();
        let post_page = *post.get_pages().get(&1).unwrap();
        assert_eq!(
            pre.get_page_content(pre_page),
            post.get_page_content(post_page),
            "content stream must be byte-identical"
        );

        let (font, subset) = simple_font_view(&out);
        let original = noto_bytes();
        assert!(
            subset.len() < original.len() / 4,
            "subset must be much smaller than the full font"
        );
        for ch in text.chars() {
            assert_char_preserved(&subset, &original, ch);
        }
        // Codes, /Encoding, and /Widths untouched; name re-tagged.
        assert!(
            matches!(font.get(b"Encoding"), Ok(Object::Name(n)) if n == b"WinAnsiEncoding"),
            "/Encoding must be untouched"
        );
        let tagged = base_name_of(&font, b"BaseFont");
        assert_eq!(tagged[6], b'+');
        assert_eq!(&tagged[7..], b"NotoSans-Regular");
        // An unused glyph must actually be gone (it is a subset, not a copy).
        let new_face = ttf_parser::Face::parse(&subset, 0).unwrap();
        assert_eq!(
            new_face.glyph_index('A'),
            None,
            "unused 'A' must not survive"
        );
    }

    #[test]
    fn simple_font_differences_encoding_is_resolved() {
        // Code 65 (normally 'A') remapped to /eacute via /Differences.
        let spec = SimpleSpec {
            encoding: Some(Object::Dictionary(dictionary! {
                "Type" => "Encoding",
                "BaseEncoding" => "WinAnsiEncoding",
                "Differences" => vec![65.into(), Object::Name(b"eacute".to_vec())],
            })),
            ..SimpleSpec::winansi()
        };
        let pdf = build_simple_pdf(&spec, b"A");
        let out = optimize_with_options(&pdf, subset_opts());
        assert!(out.len() < pdf.len());
        let (_, subset) = simple_font_view(&out);
        assert_char_preserved(&subset, &noto_bytes(), '\u{e9}');
    }

    #[test]
    fn symbolic_simple_font_is_untouched() {
        let spec = SimpleSpec {
            flags: 32 | 4, // Symbolic set: outside the nonsymbolic lookup model
            ..SimpleSpec::winansi()
        };
        let pdf = build_simple_pdf(&spec, b"Hello");
        let out = optimize_with_options(&pdf, subset_opts());
        assert_eq!(out, pdf, "symbolic simple font must be untouched");
    }

    #[test]
    fn simple_font_without_encoding_is_untouched() {
        let spec = SimpleSpec {
            encoding: None, // built-in encoding: semantics we decline to guess
            ..SimpleSpec::winansi()
        };
        let pdf = build_simple_pdf(&spec, b"Hello");
        let out = optimize_with_options(&pdf, subset_opts());
        assert_eq!(out, pdf, "font without /Encoding must be untouched");
    }

    #[test]
    fn corrupt_simple_font_file_is_untouched() {
        let spec = SimpleSpec {
            corrupt_font_file: true,
            ..SimpleSpec::winansi()
        };
        let pdf = build_simple_pdf(&spec, b"Hello");
        let out = optimize_with_options(&pdf, subset_opts());
        assert_eq!(out, pdf, "corrupt FontFile2 must return original bytes");
    }

    #[test]
    fn simple_font_subsetting_is_idempotent() {
        let pdf = build_simple_pdf(&SimpleSpec::winansi(), b"Hello, World!");
        let once = optimize_with_options(&pdf, subset_opts());
        assert!(once.len() < pdf.len(), "first pass must shrink");
        let twice = optimize_with_options(&once, subset_opts());
        assert_eq!(twice, once, "second pass must be byte-stable");
    }

    /// Emit pre/post PDFs for the dev-only external verification harness
    /// (`scripts/verify-fonts.sh`: Ghostscript nullpage render + pdftotext
    /// diff). Not a CI gate; same posture as `bench-vs-gs.sh`.
    #[test]
    #[ignore = "writes target/font-verify/{pre,post}.pdf for scripts/verify-fonts.sh"]
    fn emit_font_verification_pdfs() {
        let text = "The quick brown fox jumps over the lazy dog: été, naïve, Zürich!";
        let cids = gids_for(text);
        let pairs: Vec<(u16, char)> = cids.iter().copied().zip(text.chars()).collect();
        let pdf = build_text_pdf(&FontSpec::identity(pairs), &[cids]);
        let out = optimize_with_options(&pdf, subset_opts());
        assert!(out.len() < pdf.len());

        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/target/font-verify");
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(format!("{dir}/pre.pdf"), &pdf).unwrap();
        std::fs::write(format!("{dir}/post.pdf"), &out).unwrap();
        println!(
            "wrote {dir}/pre.pdf ({} bytes) and post.pdf ({} bytes)",
            pdf.len(),
            out.len()
        );
    }
}
