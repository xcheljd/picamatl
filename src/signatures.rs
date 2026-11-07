//! Signature handling: why this is detection and not a guard.
//!
//! A `/ByteRange` digest covers file offsets, so it cannot survive *any*
//! picamatl output: every run re-serializes the whole document from scratch,
//! moving every offset, and font subsetting, image downsampling, object-stream
//! packing and metadata stripping were never gated on signatures in the first
//! place. Gating only the three entropy-level passes (JPEG Huffman
//! re-optimization, whole-document re-deflate, content minification) would have
//! protected nothing while costing real bytes -- measured at 477 KB on a
//! Reader-extended IRS form whose 1.5 MB XFA attachment ships with a weak
//! deflate.
//!
//! The honest contract: optimizing a signed PDF invalidates its signature,
//! exactly as with every other PDF optimizer. `OptimizeOptions::on_signature`
//! controls whether `detect_signatures` runs (the default `Warn`) or the
//! pipeline proceeds without looking (`Silence`). The findings come back in
//! `OptimizeReport::signatures`, and only for a run that actually returned a
//! rewritten document -- the library itself prints nothing; the `picamatl`
//! binary turns the report into its one stderr warning. The fail-safe contract
//! covers rendered content, not offset-pinned digests; callers who must keep a
//! signature intact should not optimize the file at all.

use lopdf::{Document, Object, ObjectId};

use crate::resolve;

/// What kind of signature-bearing object [`detect_signatures`] found.
///
/// `#[non_exhaustive]`: shapes get added as documents in the wild demand
/// them. Use [`SignatureKind::label`] rather than matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SignatureKind {
    /// An `/AcroForm` field with `/FT /Sig` whose `/V` is the signature
    /// dictionary (ISO 32000-1 §12.7.4.5). Found anywhere in the field
    /// tree, including under `/Kids`, with `/FT` and `/V` inherited.
    Field,
    /// A merged field/widget node that *is* the signature dictionary — it
    /// carries `/ByteRange` itself rather than pointing at something that
    /// does.
    Dictionary,
    /// A `/UR3` (Reader usage-rights) or `/DocMDP` (certification) entry of
    /// the catalog's `/Perms` dictionary — ISO 32000-1 §12.8.4 Table 258,
    /// where those values *are* the signature dictionaries.
    Permissions,
}

impl SignatureKind {
    /// The human-readable label picamatl's own warning line uses.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            SignatureKind::Field => "signature field",
            SignatureKind::Dictionary => "signature dictionary",
            SignatureKind::Permissions => "permissions/usage-rights entry",
        }
    }
}

/// One digital signature in the *input* that the returned document's
/// re-serialization invalidates.
///
/// `Display` renders the form picamatl's warning uses, e.g.
/// `signature field (object 7 0)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct SignatureInfo {
    /// Object number and generation of the object carrying the `/ByteRange`
    /// (this is lopdf's `ObjectId`, a plain `(u32, u16)`). `None` when the
    /// signature dictionary is a *direct* dict with no object number of its
    /// own — the finding still stands, it just cannot be named.
    pub object: Option<(u32, u16)>,
    /// How the signature was reached.
    pub kind: SignatureKind,
}

impl SignatureInfo {
    /// Build one finding. The struct is `#[non_exhaustive]`, so this is how a
    /// downstream crate (picamatl's own binary included) constructs the
    /// values its warning-formatting tests need.
    #[must_use]
    pub fn new(object: Option<(u32, u16)>, kind: SignatureKind) -> Self {
        Self { object, kind }
    }
}

impl std::fmt::Display for SignatureInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.object {
            Some((number, generation)) => {
                write!(f, "{} (object {number} {generation})", self.kind.label())
            }
            None => write!(f, "{} (direct dict, no object number)", self.kind.label()),
        }
    }
}

/// Depth bound for the `/Kids` field-tree walk, mirroring `forms::MAX_DEPTH`.
/// A malformed document can point a `/Kids` entry back at its own ancestor;
/// detection must terminate on one regardless.
const SIG_MAX_DEPTH: usize = 32;

/// Detect digital signatures that optimization would invalidate.
///
/// Two reachability paths, both read-only:
///
/// 1. **The `/AcroForm` field tree.** Walked recursively through `/Kids` with
///    `/FT` and `/V` inheritance (ISO 32000-1 §12.7.3.1 makes the tree
///    hierarchical — only leaves must carry `/FT`, and a signature field with
///    several widget placements *has* to use `/Kids`). A node whose effective
///    `/FT` is `/Sig` and whose effective `/V` resolves to a dict carrying
///    `/ByteRange` is a signature; a node carrying `/ByteRange` itself is a
///    merged field/widget signature dictionary.
/// 2. **The catalog's `/Perms`.** Per §12.8.4 Table 258 the `/UR3` and
///    `/DocMDP` *values* **are** signature dictionaries — there is no
///    intermediate dictionary — so their presence alone is definitive and no
///    `/ByteRange` check is needed (unlike `/V`, which can legitimately point
///    at a non-signature). A `/Perms` that resolves straight to a dict with
///    `/ByteRange` is also taken at face value.
///
/// Every reference is resolved (`/AcroForm`, `/Fields`, `/FT`, `/V`, `/Kids`,
/// `/Perms`, and the `/Perms` entries), because any of them is legally
/// indirect.
///
/// Conservative by design: any dict carrying a `/ByteRange` key counts as a
/// signature even if malformed — a broken range is still a signature the
/// user placed. Uncertainty biases toward detection: a missed signature
/// means a silently invalidated one. The one deliberate exception is an
/// *empty* signature field (no `/V`, or a `/V` that is not a signature
/// dictionary): there is no digest to invalidate, Acrobat leaves those behind
/// routinely, and warning about them would train users to ignore the warning.
pub(crate) fn detect_signatures(doc: &Document) -> Vec<SignatureInfo> {
    let mut found: Vec<SignatureInfo> = Vec::new();

    // /AcroForm may be an indirect reference (the common case) or a direct
    // dictionary; so may /Fields, and so may every node below it.
    let acro = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"AcroForm").ok())
        .map(|a| resolve(doc, a))
        .and_then(|a| a.as_dict().ok());
    if let Some(acro) = acro {
        if let Ok(fields) = acro
            .get(b"Fields")
            .map(|f| resolve(doc, f))
            .and_then(Object::as_array)
        {
            for field in fields {
                walk_sig_field(doc, field, None, None, 0, &mut found);
            }
        }
    }

    // Catalog /Perms: Reader usage-rights (UR3) and MDP certification grants.
    if let Some(perms) = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"Perms").ok())
        .map(|p| resolve(doc, p))
        .and_then(|p| p.as_dict().ok())
    {
        // A /Perms that is itself the signature dictionary. Not the shape the
        // spec describes, but it is what picamatl's own signed-document test
        // fixture builds and it costs one `has` to cover.
        if perms.has(b"ByteRange") {
            found.push(SignatureInfo {
                object: doc
                    .catalog()
                    .ok()
                    .and_then(|c| c.get(b"Perms").ok())
                    .and_then(|p| p.as_reference().ok()),
                kind: SignatureKind::Permissions,
            });
        }
        // Fixed key order, not `perms.iter()`, so the output is deterministic
        // and no undefined /Perms entry can be mistaken for a signature.
        for key in [b"UR3".as_slice(), b"DocMDP".as_slice()] {
            let Ok(value) = perms.get(key) else {
                continue;
            };
            match value {
                // The value IS the signature dictionary (Table 258).
                Object::Reference(id) => found.push(SignatureInfo {
                    object: Some(*id),
                    kind: SignatureKind::Permissions,
                }),
                // A direct dict has no object number to name; the finding
                // still stands.
                Object::Dictionary(_) => found.push(SignatureInfo {
                    object: None,
                    kind: SignatureKind::Permissions,
                }),
                _ => {}
            }
        }
    }

    // One invalidated signature per object, however many paths reached it: a
    // Sig named by both an /AcroForm field and /Perms, or by two fields, is
    // still one digest. `sort_by_key` is the *stable* sort, so equal ids keep
    // insertion order and the surviving `kind` is deterministic (the
    // /AcroForm walk runs first, so a field beats a /Perms entry).
    //
    // Entries with no object number are deliberately NOT collapsed into each
    // other: two distinct direct signature dictionaries are two signatures,
    // and `None == None` would report one.
    found.sort_by_key(|s| s.object);
    found.dedup_by(|a, b| a.object.is_some() && a.object == b.object);
    found
}

/// One node of the `/AcroForm` field tree, with `/FT` and `/V` inherited from
/// its ancestors. Mirrors `forms::walk_field`, minus everything that module
/// needs for its data-preservation proof.
///
/// `inherited_v` carries the id of the node that *declared* the value along
/// with the value itself, so a direct signature dictionary inherited by three
/// widget kids is reported once, against the node that owns it, rather than
/// three times against the kids.
fn walk_sig_field<'a>(
    doc: &'a Document,
    node: &'a Object,
    inherited_ft: Option<&'a [u8]>,
    inherited_v: Option<(&'a Object, Option<ObjectId>)>,
    depth: usize,
    found: &mut Vec<SignatureInfo>,
) {
    if depth > SIG_MAX_DEPTH {
        return;
    }
    let node_id = match node {
        Object::Reference(id) => Some(*id),
        _ => None,
    };
    let Ok(dict) = resolve(doc, node).as_dict() else {
        // A field entry that is not a dictionary (junk in /Fields is common)
        // carries no signature and has no kids.
        return;
    };

    // /FT may be an indirect reference to the name. Some minimal writers also
    // store the leading slash inside the name bytes ("/Sig"); conforming ones
    // don't ("Sig"). Accept both — a missed signature is the expensive
    // mistake.
    let own_ft = dict
        .get(b"FT")
        .map(|f| resolve(doc, f))
        .ok()
        .and_then(|f| f.as_name().ok());
    let ft = own_ft.or(inherited_ft);

    let value = match dict.get(b"V") {
        Ok(v) => Some((v, node_id)),
        Err(_) => inherited_v,
    };

    if matches!(ft, Some(name) if name == b"Sig" || name == b"/Sig") {
        // The /V value may be an indirect reference (conforming writers) or a
        // direct signature dict (minimal writers). Both arms require
        // /ByteRange on the *resolved* target: an unsigned placeholder field
        // whose /V points at something else is not a signature, and saying it
        // is would put "the /ByteRange digest is invalidated" in the user's
        // face about an object that has no /ByteRange.
        if let Some((v, owner_id)) = value {
            if resolve(doc, v).as_dict().is_ok_and(|d| d.has(b"ByteRange")) {
                found.push(SignatureInfo {
                    // The reference names the signature dictionary itself;
                    // a direct dict can only be named by its owning node.
                    object: v.as_reference().ok().or(owner_id),
                    kind: SignatureKind::Field,
                });
            }
        }
    }

    // A merged field/widget node that IS the signature dictionary. A node
    // stored directly in its parent's /Fields or /Kids array has no object
    // number to name (`object: None`), but it is a signature all the same —
    // the module's contract is that any dict carrying /ByteRange counts, and
    // the final dedup deliberately keeps distinct `None` entries apart.
    if dict.has(b"ByteRange") {
        found.push(SignatureInfo {
            object: node_id,
            kind: SignatureKind::Dictionary,
        });
    }

    if let Ok(kids) = dict.get(b"Kids").map(|k| resolve(doc, k)) {
        if let Ok(kids) = kids.as_array() {
            for kid in kids {
                walk_sig_field(doc, kid, ft, value, depth + 1, found);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    use crate::tests::build_pdf;
    use crate::{optimize_with_options, optimize_with_report, OptimizeOptions, SignatureHandling};

    // ---- signature detection ----
    //
    // Every case below is a document *shape*, named after the shape and not
    // after the code path, because the code paths moved once already. The
    // shapes came out of a fixture matrix built with pikepdf; the ids in the
    // comments (A, B, K, …) are that matrix's.

    /// A signature dictionary as a real writer emits one. `/ByteRange` is the
    /// thing detection keys on; the rest is there so the fixture is honest.
    fn sig_dict() -> lopdf::Dictionary {
        dictionary! {
            "Type" => Object::Name(b"Sig".to_vec()),
            "Filter" => Object::Name(b"Adobe.PPKLite".to_vec()),
            "SubFilter" => Object::Name(b"adbe.pkcs7.detached".to_vec()),
            "ByteRange" => Object::Array(vec![
                Object::Integer(0), Object::Integer(10),
                Object::Integer(20), Object::Integer(10),
            ]),
            "Contents" => Object::String(b"fake".to_vec(), lopdf::StringFormat::Hexadecimal),
        }
    }

    /// Save and reparse, so detection sees the document exactly as the parser
    /// hands it over rather than as the test built it in memory.
    fn roundtrip(doc: &mut Document) -> Document {
        let mut bytes: Vec<u8> = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        Document::load_mem(&bytes).unwrap()
    }

    /// A minimal document with `/AcroForm` set to `fields`.
    fn doc_with_fields(fields: Object) -> Document {
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! { "Fields" => fields }),
        );
        doc
    }

    fn objects(sigs: &[SignatureInfo]) -> Vec<Option<(u32, u16)>> {
        sigs.iter().map(|s| s.object).collect()
    }

    #[test]
    fn a_conforming_writer_emits_the_ft_sig_name_without_a_slash() {
        // Pins the premise of the next test. lopdf's `Name(b"Sig")` — what
        // pikepdf and Acrobat write — serializes as `/Sig` and reparses as
        // the name whose BYTES are `Sig`. `Name(b"/Sig")` is a different
        // name: it serializes as `/#2FSig`. Both are accepted by detection,
        // but only the first is the real-world shape, and for a long while
        // only the second had a test.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let field_id = doc.add_object(dictionary! { "FT" => Object::Name(b"Sig".to_vec()) });
        let mut bytes: Vec<u8> = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        assert!(
            bytes.windows(4).any(|w| w == b"/Sig"),
            "a conforming /FT /Sig must appear literally in the file"
        );
        assert!(
            !bytes.windows(6).any(|w| w == b"#2FSig"),
            "no name-escape: this is /Sig, not the name whose bytes are \"/Sig\""
        );

        let reloaded = Document::load_mem(&bytes).unwrap();
        let field = reloaded.get_object(field_id).unwrap().as_dict().unwrap();
        assert_eq!(
            field.get(b"FT").unwrap().as_name().unwrap(),
            b"Sig",
            "the parsed name bytes carry no leading slash"
        );

        // The other half of the round-trip: `Name(b"/Sig")` is the escaped
        // name `/#2FSig`, which no writer emits. Detection accepts it anyway,
        // but a test that only used this form would be testing nothing real.
        let mut escaped = Document::load_mem(&build_pdf(400, 100)).unwrap();
        let escaped_id = escaped.add_object(dictionary! { "FT" => Object::Name(b"/Sig".to_vec()) });
        let mut escaped_bytes: Vec<u8> = Vec::new();
        escaped.save_to(&mut escaped_bytes).unwrap();
        assert!(
            escaped_bytes.windows(6).any(|w| w == b"#2FSig"),
            "Name(b\"/Sig\") must serialize escaped"
        );
        let escaped = Document::load_mem(&escaped_bytes).unwrap();
        let field = escaped.get_object(escaped_id).unwrap().as_dict().unwrap();
        assert_eq!(field.get(b"FT").unwrap().as_name().unwrap(), b"/Sig");
    }

    #[test]
    fn detect_signatures_finds_a_conforming_sig_field() {
        // Fixture A: the shape pikepdf writes — indirect field, /FT /Sig with
        // NO leading slash in the name bytes, /V an indirect signature dict.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let sig_id = doc.add_object(sig_dict());
        let field_id = doc.add_object(dictionary! {
            "FT" => Object::Name(b"Sig".to_vec()),
            "T" => Object::String(b"Signature1".to_vec(), lopdf::StringFormat::Literal),
            "V" => Object::Reference(sig_id),
        });
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! {
                "Fields" => Object::Array(vec![Object::Reference(field_id)]),
            }),
        );
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert_eq!(
            objects(&sigs),
            vec![Some(sig_id)],
            "conforming /FT /Sig field not detected exactly once; got {sigs:?}"
        );
        assert_eq!(sigs[0].kind, SignatureKind::Field);
    }

    #[test]
    fn detect_signatures_accepts_the_slash_prefixed_name_form() {
        // Some minimal writers put the slash inside the name bytes. Not a
        // conforming shape, but cheap to accept and a miss is the expensive
        // mistake, so the defensive arm stays pinned.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let sig_id = doc.add_object(sig_dict());
        let field_id = doc.add_object(dictionary! {
            "FT" => Object::Name(b"/Sig".to_vec()),
            "V" => Object::Reference(sig_id),
        });
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! {
                "Fields" => Object::Array(vec![Object::Reference(field_id)]),
            }),
        );
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert_eq!(objects(&sigs), vec![Some(sig_id)], "got {sigs:?}");
    }

    #[test]
    fn detect_signatures_finds_a_field_stored_directly_in_fields() {
        // The field itself is a direct dict inside /Fields (no indirection),
        // so only its /V has an object number to name.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let sig_id = doc.add_object(sig_dict());
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! {
                "Fields" => Object::Array(vec![
                    Object::Dictionary(dictionary! {
                        "FT" => Object::Name(b"Sig".to_vec()),
                        "V" => Object::Reference(sig_id),
                    }),
                ]),
            }),
        );
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert_eq!(objects(&sigs), vec![Some(sig_id)], "got {sigs:?}");
    }

    #[test]
    fn detect_signatures_walks_one_level_of_kids() {
        // Fixture B: the /Fields entry is a plain parent node; the /FT /Sig
        // and /V live on the kid. ISO 32000-1 12.7.3.1 only requires LEAF
        // nodes to carry /FT, so this is conforming and common.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let sig_id = doc.add_object(sig_dict());
        let kid_id = doc.add_object(dictionary! {
            "FT" => Object::Name(b"Sig".to_vec()),
            "V" => Object::Reference(sig_id),
            "Subtype" => Object::Name(b"Widget".to_vec()),
        });
        let parent_id = doc.add_object(dictionary! {
            "T" => Object::String(b"parent".to_vec(), lopdf::StringFormat::Literal),
            "Kids" => Object::Array(vec![Object::Reference(kid_id)]),
        });
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! {
                "Fields" => Object::Array(vec![Object::Reference(parent_id)]),
            }),
        );
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert_eq!(
            objects(&sigs),
            vec![Some(sig_id)],
            "one-level /Kids signature missed; got {sigs:?}"
        );
    }

    #[test]
    fn detect_signatures_walks_two_levels_of_kids() {
        // Fixture K: grandparent -> parent -> signature leaf.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let sig_id = doc.add_object(sig_dict());
        let leaf_id = doc.add_object(dictionary! {
            "FT" => Object::Name(b"Sig".to_vec()),
            "V" => Object::Reference(sig_id),
        });
        let mid_id = doc.add_object(dictionary! {
            "Kids" => Object::Array(vec![Object::Reference(leaf_id)]),
        });
        let top_id = doc.add_object(dictionary! {
            "Kids" => Object::Array(vec![Object::Reference(mid_id)]),
        });
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! {
                "Fields" => Object::Array(vec![Object::Reference(top_id)]),
            }),
        );
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert_eq!(
            objects(&sigs),
            vec![Some(sig_id)],
            "two-level /Kids signature missed; got {sigs:?}"
        );
    }

    #[test]
    fn detect_signatures_inherits_ft_and_v_down_to_kids_without_double_counting() {
        // Fixture L: /FT and /V on the parent, two widget kids that inherit
        // both. One signature, three nodes that claim it — exactly one
        // report.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let sig_id = doc.add_object(sig_dict());
        let kid_a = doc.add_object(dictionary! { "Subtype" => Object::Name(b"Widget".to_vec()) });
        let kid_b = doc.add_object(dictionary! { "Subtype" => Object::Name(b"Widget".to_vec()) });
        let parent_id = doc.add_object(dictionary! {
            "FT" => Object::Name(b"Sig".to_vec()),
            "V" => Object::Reference(sig_id),
            "Kids" => Object::Array(vec![Object::Reference(kid_a), Object::Reference(kid_b)]),
        });
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! {
                "Fields" => Object::Array(vec![Object::Reference(parent_id)]),
            }),
        );
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert_eq!(objects(&sigs), vec![Some(sig_id)], "got {sigs:?}");
    }

    #[test]
    fn detect_signatures_terminates_on_a_cyclic_kids_chain() {
        // A /Kids entry pointing back at its own ancestor. Detection must
        // return, not recurse forever — the depth guard's whole job.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let a_id = doc.new_object_id();
        let b_id = doc.add_object(dictionary! {
            "Kids" => Object::Array(vec![Object::Reference(a_id)]),
        });
        doc.set_object(
            a_id,
            Object::Dictionary(dictionary! {
                "Kids" => Object::Array(vec![Object::Reference(b_id)]),
            }),
        );
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! {
                "Fields" => Object::Array(vec![Object::Reference(a_id)]),
            }),
        );
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert!(sigs.is_empty(), "no signature in a cycle of empty nodes");
    }

    #[test]
    fn detect_signatures_finds_a_merged_field_widget_signature_dict() {
        // Fixture M: one object that is both the field and the signature
        // dictionary — /FT /Sig plus /ByteRange, no /V at all.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let mut merged = sig_dict();
        merged.set("FT", Object::Name(b"Sig".to_vec()));
        merged.set("Subtype", Object::Name(b"Widget".to_vec()));
        let merged_id = doc.add_object(merged);
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! {
                "Fields" => Object::Array(vec![Object::Reference(merged_id)]),
            }),
        );
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert_eq!(objects(&sigs), vec![Some(merged_id)], "got {sigs:?}");
        assert_eq!(sigs[0].kind, SignatureKind::Dictionary);
    }

    #[test]
    fn detect_signatures_finds_a_direct_merged_field_widget_signature_dict() {
        // The merged field/widget signature dictionary stored DIRECTLY in the
        // /Fields array (no object of its own). It carries /ByteRange, so it
        // is a signature; it simply has no object number to report.
        let mut merged = sig_dict();
        merged.set("FT", Object::Name(b"Sig".to_vec()));
        merged.set("Subtype", Object::Name(b"Widget".to_vec()));
        let mut doc = doc_with_fields(Object::Array(vec![Object::Dictionary(merged)]));
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert_eq!(objects(&sigs), vec![None], "got {sigs:?}");
        assert_eq!(sigs[0].kind, SignatureKind::Dictionary);
    }

    #[test]
    fn detect_signatures_resolves_an_indirect_fields_array() {
        // Fixture T: /Fields is a reference to the array. Any array may be
        // indirect.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let sig_id = doc.add_object(sig_dict());
        let field_id = doc.add_object(dictionary! {
            "FT" => Object::Name(b"Sig".to_vec()),
            "V" => Object::Reference(sig_id),
        });
        let fields_id = doc.add_object(Object::Array(vec![Object::Reference(field_id)]));
        let acro_id = doc.add_object(dictionary! { "Fields" => Object::Reference(fields_id) });
        doc.catalog_mut()
            .unwrap()
            .set("AcroForm", Object::Reference(acro_id));
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert_eq!(objects(&sigs), vec![Some(sig_id)], "got {sigs:?}");
    }

    #[test]
    fn detect_signatures_resolves_an_indirect_ft_name() {
        // Fixture U: /FT is a reference to the name object.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let sig_id = doc.add_object(sig_dict());
        let ft_id = doc.add_object(Object::Name(b"Sig".to_vec()));
        let field_id = doc.add_object(dictionary! {
            "FT" => Object::Reference(ft_id),
            "V" => Object::Reference(sig_id),
        });
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! {
                "Fields" => Object::Array(vec![Object::Reference(field_id)]),
            }),
        );
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert_eq!(objects(&sigs), vec![Some(sig_id)], "got {sigs:?}");
    }

    #[test]
    fn detect_signatures_ignores_an_unsigned_placeholder_field() {
        // Fixture E: /FT /Sig with no /V — the empty signature field Acrobat
        // leaves behind. There is no digest to invalidate, and warning about
        // these teaches users to ignore the warning.
        let mut doc = doc_with_fields(Object::Array(vec![Object::Dictionary(dictionary! {
            "FT" => Object::Name(b"Sig".to_vec()),
            "T" => Object::String(b"Signature1".to_vec(), lopdf::StringFormat::Literal),
        })]));
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert!(
            sigs.is_empty(),
            "unsigned placeholder warned about: {sigs:?}"
        );
    }

    #[test]
    fn detect_signatures_ignores_an_indirect_v_that_is_not_a_signature() {
        // Fixture E2: /V is a reference to a dict with NO /ByteRange. The
        // direct-dict arm always checked; the reference arm used to push
        // unconditionally, so picamatl announced that a /ByteRange digest was
        // invalidated for an object that has no /ByteRange.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let not_a_sig = doc.add_object(dictionary! { "Type" => Object::Name(b"Nothing".to_vec()) });
        let field_id = doc.add_object(dictionary! {
            "FT" => Object::Name(b"Sig".to_vec()),
            "V" => Object::Reference(not_a_sig),
        });
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! {
                "Fields" => Object::Array(vec![Object::Reference(field_id)]),
            }),
        );
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert!(
            sigs.is_empty(),
            "false positive on a non-signature /V: {sigs:?}"
        );
    }

    #[test]
    fn detect_signatures_survives_junk_in_the_fields_array() {
        // Fixture H: integers and strings mixed into /Fields. The real field
        // must still be found and nothing may panic.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let sig_id = doc.add_object(sig_dict());
        let field_id = doc.add_object(dictionary! {
            "FT" => Object::Name(b"Sig".to_vec()),
            "V" => Object::Reference(sig_id),
        });
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! {
                "Fields" => Object::Array(vec![
                    Object::Integer(42),
                    Object::String(b"junk".to_vec(), lopdf::StringFormat::Literal),
                    Object::Reference(field_id),
                    Object::Null,
                ]),
            }),
        );
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert_eq!(objects(&sigs), vec![Some(sig_id)], "got {sigs:?}");
    }

    #[test]
    fn detect_signatures_ignores_an_xfa_only_form_with_no_fields() {
        // Fixture G: /AcroForm present, /Fields empty. Nothing to find.
        let mut doc = doc_with_fields(Object::Array(vec![]));
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert!(sigs.is_empty(), "got {sigs:?}");
    }

    /// Fixtures C / D / N: `/Perms` direct or indirect, crossed with `/UR3`
    /// and `/DocMDP`, each value either a reference to a signature dictionary
    /// or a direct one. ISO 32000-1 Table 258: those values ARE the signature
    /// dictionaries — there is no intermediate dictionary, which is what the
    /// first implementation of this branch looked for.
    #[test]
    fn detect_signatures_finds_every_perms_shape() {
        for perms_is_indirect in [false, true] {
            for key in ["UR3", "DocMDP"] {
                for value_is_indirect in [false, true] {
                    let pdf = build_pdf(400, 100);
                    let mut doc = Document::load_mem(&pdf).unwrap();
                    let (value, expected) = if value_is_indirect {
                        let id = doc.add_object(sig_dict());
                        (Object::Reference(id), Some(id))
                    } else {
                        (Object::Dictionary(sig_dict()), None)
                    };
                    let perms = dictionary! { key => value };
                    let perms_obj = if perms_is_indirect {
                        Object::Reference(doc.add_object(perms))
                    } else {
                        Object::Dictionary(perms)
                    };
                    doc.catalog_mut().unwrap().set("Perms", perms_obj);

                    let sigs = detect_signatures(&roundtrip(&mut doc));
                    let case = format!(
                        "/Perms {} with /{key} {}",
                        if perms_is_indirect {
                            "indirect"
                        } else {
                            "direct"
                        },
                        if value_is_indirect {
                            "indirect"
                        } else {
                            "direct"
                        },
                    );
                    assert_eq!(objects(&sigs), vec![expected], "{case}: got {sigs:?}");
                    assert_eq!(sigs[0].kind, SignatureKind::Permissions, "{case}");
                }
            }
        }
    }

    #[test]
    fn detect_signatures_finds_a_perms_that_is_itself_a_signature_dict() {
        // Fixture V: `catalog.set("Perms", sig)` — what picamatl's own
        // signed-document test fixture builds. Not the spec's shape, but a
        // /Perms that resolves straight to a /ByteRange-carrying dict is
        // taken at face value.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let sig_id = doc.add_object(sig_dict());
        doc.catalog_mut()
            .unwrap()
            .set("Perms", Object::Reference(sig_id));
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! {
                "SigFlags" => Object::Integer(3),
                "Fields" => Object::Array(vec![]),
            }),
        );
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert_eq!(objects(&sigs), vec![Some(sig_id)], "got {sigs:?}");
        assert_eq!(sigs[0].kind, SignatureKind::Permissions);
    }

    #[test]
    fn detect_signatures_reports_one_signature_reached_by_two_paths() {
        // Fixture O: the same signature object named by an /AcroForm field
        // AND by /Perms /DocMDP. One digest, one report — the /AcroForm walk
        // runs first, so "signature field" is the surviving label.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let sig_id = doc.add_object(sig_dict());
        let field_id = doc.add_object(dictionary! {
            "FT" => Object::Name(b"Sig".to_vec()),
            "V" => Object::Reference(sig_id),
        });
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! {
                "Fields" => Object::Array(vec![Object::Reference(field_id)]),
            }),
        );
        doc.catalog_mut().unwrap().set(
            "Perms",
            Object::Dictionary(dictionary! { "DocMDP" => Object::Reference(sig_id) }),
        );
        let sigs = detect_signatures(&roundtrip(&mut doc));
        assert_eq!(
            objects(&sigs),
            vec![Some(sig_id)],
            "one object counted twice; got {sigs:?}"
        );
        assert_eq!(sigs[0].kind, SignatureKind::Field);
    }

    #[test]
    fn detect_signatures_dedups_non_adjacent_duplicates() {
        // Fixture P: three fields, the FIRST and THIRD sharing one signature.
        // `Vec::dedup_by` only removes CONSECUTIVE duplicates, so this shape
        // needs the sort that now precedes it — two signatures, not three.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let shared = doc.add_object(sig_dict());
        let other = doc.add_object(sig_dict());
        let field = |sig: ObjectId| {
            Object::Dictionary(dictionary! {
                "FT" => Object::Name(b"Sig".to_vec()),
                "V" => Object::Reference(sig),
            })
        };
        let fields = Object::Array(vec![field(shared), field(other), field(shared)]);
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! { "Fields" => fields }),
        );
        let sigs = detect_signatures(&roundtrip(&mut doc));
        let mut expected = vec![Some(shared), Some(other)];
        expected.sort();
        assert_eq!(objects(&sigs), expected, "got {sigs:?}");
    }

    #[test]
    fn detect_signatures_is_deterministic_across_runs() {
        // The warning line is user-facing text; its order must not depend on
        // hash iteration or sort instability.
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let a = doc.add_object(sig_dict());
        let b = doc.add_object(sig_dict());
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! {
                "Fields" => Object::Array(vec![
                    Object::Dictionary(dictionary! {
                        "FT" => Object::Name(b"Sig".to_vec()), "V" => Object::Reference(b),
                    }),
                    Object::Dictionary(dictionary! {
                        "FT" => Object::Name(b"Sig".to_vec()), "V" => Object::Reference(a),
                    }),
                ]),
            }),
        );
        let doc = roundtrip(&mut doc);
        let first = detect_signatures(&doc);
        assert_eq!(first, detect_signatures(&doc));
        assert_eq!(objects(&first), vec![Some(a.min(b)), Some(a.max(b))]);
    }

    /// An over-resolution (so: definitely shrinkable) document carrying one
    /// signature reachable from `/AcroForm`, plus the id of that signature.
    fn signed_shrinkable_pdf() -> (Vec<u8>, ObjectId) {
        let pdf = build_pdf(400, 100);
        let mut doc = Document::load_mem(&pdf).unwrap();
        let sig_id = doc.add_object(sig_dict());
        let field_id = doc.add_object(dictionary! {
            "FT" => Object::Name(b"Sig".to_vec()),
            "V" => Object::Reference(sig_id),
        });
        doc.catalog_mut().unwrap().set(
            "AcroForm",
            Object::Dictionary(dictionary! {
                "SigFlags" => Object::Integer(3),
                "Fields" => Object::Array(vec![Object::Reference(field_id)]),
            }),
        );
        let mut bytes: Vec<u8> = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        // Object ids survive the save; renumbering happens inside picamatl,
        // after detection has already read the input's ids.
        (bytes, sig_id)
    }

    #[test]
    fn optimize_with_report_names_the_signature_on_a_rewrite() {
        let (pdf, sig_id) = signed_shrinkable_pdf();
        let (out, report) = optimize_with_report(&pdf, OptimizeOptions::default());
        assert!(
            out.len() < pdf.len(),
            "fixture must shrink to exercise this"
        );
        assert!(report.rewritten);
        assert_eq!(objects(&report.signatures), vec![Some(sig_id)]);
    }

    #[test]
    fn silence_suppresses_detection_entirely() {
        let (pdf, _) = signed_shrinkable_pdf();
        let options = OptimizeOptions::default().with_on_signature(SignatureHandling::Silence);
        let (out, report) = optimize_with_report(&pdf, options);
        assert!(
            report.rewritten,
            "Silence must not change what is optimized"
        );
        assert!(
            report.signatures.is_empty(),
            "Silence must not even run detection; got {:?}",
            report.signatures
        );
        // And the bytes are identical to a Warn run: the knob is about who is
        // told, never about what is produced.
        let warned = optimize_with_options(&pdf, OptimizeOptions::default());
        assert_eq!(out, warned);
    }

    #[test]
    fn a_passthrough_reports_no_signature() {
        // Feeding picamatl its own output back in returns the input verbatim
        // (nothing left to win). Nothing was re-serialized, so no digest was
        // invalidated and the report must be empty — the warning used to fire
        // here, contradicting the CLI's own "wrote a byte-for-byte copy" note
        // on the same run.
        let (pdf, _) = signed_shrinkable_pdf();
        let once = optimize_with_options(&pdf, OptimizeOptions::default());
        let (twice, report) = optimize_with_report(&once, OptimizeOptions::default());
        assert_eq!(twice, once, "second pass must be a passthrough");
        assert!(!report.rewritten);
        assert!(
            report.signatures.is_empty(),
            "passthrough warned anyway; got {:?}",
            report.signatures
        );
    }

    #[test]
    fn a_declined_document_reports_no_signature() {
        // The page-/Contents fail-safe: a signed document whose /Contents is
        // not a stream is declined byte-identically, so there is nothing to
        // warn about either.
        let (pdf, _) = signed_shrinkable_pdf();
        let mut doc = Document::load_mem(&pdf).unwrap();
        let page_id = doc.get_pages().into_values().next().unwrap();
        let dangling = doc.add_object(Object::Integer(7));
        doc.get_object_mut(page_id)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Contents", Object::Reference(dangling));
        let mut broken: Vec<u8> = Vec::new();
        doc.save_to(&mut broken).unwrap();

        let (out, report) = optimize_with_report(&broken, OptimizeOptions::default());
        assert_eq!(out, broken, "the fail-safe must pass the bytes through");
        assert!(!report.rewritten);
        assert!(
            report.signatures.is_empty(),
            "fail-safe decline warned anyway; got {:?}",
            report.signatures
        );
    }

    #[test]
    fn optimize_with_options_matches_optimize_with_report_bytes() {
        // The report-returning entry point must be a pure superset: same
        // bytes, every time.
        let (pdf, _) = signed_shrinkable_pdf();
        for options in [
            OptimizeOptions::default(),
            OptimizeOptions::default().with_on_signature(SignatureHandling::Silence),
        ] {
            assert_eq!(
                optimize_with_options(&pdf, options),
                optimize_with_report(&pdf, options).0
            );
        }
        assert_eq!(
            optimize_with_options(b"not a pdf", OptimizeOptions::default()),
            b"not a pdf".to_vec()
        );
    }

    #[test]
    fn signature_info_displays_the_warning_form() {
        assert_eq!(
            SignatureInfo::new(Some((7, 0)), SignatureKind::Field).to_string(),
            "signature field (object 7 0)"
        );
        assert_eq!(
            SignatureInfo::new(None, SignatureKind::Permissions).to_string(),
            "permissions/usage-rights entry (direct dict, no object number)"
        );
        assert_eq!(
            SignatureInfo::new(Some((3, 1)), SignatureKind::Dictionary).to_string(),
            "signature dictionary (object 3 1)"
        );
    }
}
