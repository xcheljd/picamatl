//! Type1 → Type1C (CFF) font conversion for the opt-in `--convert-type1`
//! pass.
//!
//! Three self-contained stages, every one of which returns `None` on any
//! structural doubt (the caller then leaves the font untouched, fail-safe):
//!
//! 1. **Parse** ([`parse`]): PFB-segmented or raw `/FontFile` payloads,
//!    eexec decryption (binary or hex), cleartext header fields (FontName,
//!    FontMatrix, FontBBox, PaintType, the built-in `/Encoding`), and the
//!    private-dict tokenizer that extracts hint parameters, `/Subrs`, and
//!    `/CharStrings` (charstring-decrypted, `lenIV` honored).
//! 2. **Interpret** (`interpret_glyph`): a strict Type1 charstring
//!    interpreter that inlines subroutine calls and reduces each glyph to
//!    width + sidebearing + stem hints + relative path segments. The four
//!    standard OtherSubrs are evaluated structurally: flex (0–2) becomes the
//!    two curves it always rasterizes as, hint replacement (3) becomes a
//!    segment boundary carrying the replacement stem set, and `seac`
//!    composites are inlined from their component outlines (accent
//!    translated per the Type1 rendering rule `sbx + adx - asb`). Anything
//!    else — unknown operators, MM OtherSubrs, stack anomalies — fails the
//!    glyph and with it the font.
//! 3. **Emit** ([`convert_to_cff`]): Type2 charstrings (width encoded
//!    against `defaultWidthX`/`nominalWidthX`, `hstem(hm)`/`vstem(hm)` +
//!    `hintmask` reproducing the Type1 stem sets exactly) inside a minimal
//!    CFF: Name/Top DICT/String/CharStrings INDEXes, a format-0 charset, a
//!    custom encoding replicating the font's built-in encoding (restricted
//!    to retained glyphs, with supplements for multiply-encoded glyphs),
//!    and a Private DICT carrying every hint parameter the Type1 declared
//!    (BlueValues family, StdHW/VW, StemSnap, ForceBold, LanguageGroup).
//!
//! Coordinate fidelity: Type1 charstrings produce integers or `div`
//! rationals; both are carried as `f64` and re-emitted as Type2 integers or
//! 16.16 fixed-point, the same precision FreeType evaluates Type1 `div` at.
//! The left sidebearing (`hsbw`/`sbw`) is folded into each glyph's first
//! moveto, which is exactly how a Type2 consumer reconstructs the identical
//! outline; stem coordinates get the same sidebearing translation the Type1
//! rasterizer applies.

use std::collections::BTreeMap;

use crate::encodings;

/// Recursion bound for `callsubr` nesting (the Type1 spec guarantees 10).
const MAX_SUBR_DEPTH: usize = 32;
/// Executed-token bound per glyph; a charstring beyond this is pathological.
const MAX_GLYPH_TOKENS: usize = 1 << 20;
/// Operand-stack bound (the spec says 24; be tolerant, not unbounded).
const MAX_STACK: usize = 96;
/// Total stem-hint bound imposed by Type2 `hintmask`.
const MAX_STEMS: usize = 96;
/// Upper bound on parsed charstrings/subrs, against pathological inputs.
const MAX_GLYPHS: usize = 20_000;

// ---------------------------------------------------------------------------
// Parsed font model
// ---------------------------------------------------------------------------

/// Hint parameters lifted from the Type1 Private dict, re-emitted into the
/// CFF Private DICT. All optional; absent entries keep their CFF defaults.
#[derive(Default)]
pub(crate) struct PrivateHints {
    blue_values: Vec<f64>,
    other_blues: Vec<f64>,
    family_blues: Vec<f64>,
    family_other_blues: Vec<f64>,
    blue_scale: Option<f64>,
    blue_shift: Option<f64>,
    blue_fuzz: Option<f64>,
    std_hw: Option<f64>,
    std_vw: Option<f64>,
    stem_snap_h: Vec<f64>,
    stem_snap_v: Vec<f64>,
    force_bold: Option<bool>,
    language_group: Option<f64>,
}

/// A fully parsed Type1 font program: decrypted charstrings and subrs plus
/// the header fields the CFF re-emission needs.
pub(crate) struct Type1Font {
    pub(crate) font_name: Vec<u8>,
    font_matrix: [f64; 6],
    font_bbox: [f64; 4],
    paint_type: f64,
    /// Built-in encoding: code -> glyph name. `None` = `.notdef`.
    encoding: Vec<Option<Vec<u8>>>,
    /// Glyph name -> decrypted charstring (lenIV bytes already stripped).
    charstrings: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Decrypted subroutines (lenIV bytes already stripped).
    subrs: Vec<Vec<u8>>,
    private: PrivateHints,
}

impl Type1Font {
    /// The built-in encoding's glyph name at `code` (`None` = `.notdef`).
    pub(crate) fn builtin_name(&self, code: u8) -> Option<&[u8]> {
        match &self.encoding[usize::from(code)] {
            Some(name) if name != b".notdef" => Some(name),
            _ => None,
        }
    }

    /// Whether the font carries a charstring for `name`.
    pub(crate) fn has_glyph(&self, name: &[u8]) -> bool {
        self.charstrings.contains_key(name)
    }
}

// ---------------------------------------------------------------------------
// Decryption
// ---------------------------------------------------------------------------

const EEXEC_R: u16 = 55665;
const CHARSTRING_R: u16 = 4330;
const C1: u16 = 52845;
const C2: u16 = 22719;

/// Type1 decryption; `skip` leading plaintext bytes are discarded (4 for
/// eexec, `lenIV` for charstrings).
fn decrypt(data: &[u8], key: u16, skip: usize) -> Option<Vec<u8>> {
    if data.len() < skip {
        return None;
    }
    let mut r = key;
    let mut out = Vec::with_capacity(data.len() - skip);
    for (i, &c) in data.iter().enumerate() {
        let p = c ^ (r >> 8) as u8;
        r = (u16::from(c).wrapping_add(r))
            .wrapping_mul(C1)
            .wrapping_add(C2);
        if i >= skip {
            out.push(p);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Container handling (PFB / raw / hex eexec)
// ---------------------------------------------------------------------------

/// Split the font payload into (cleartext, eexec-decrypted plaintext).
fn split_and_decrypt(data: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    // PFB: 0x80-marked segments; concatenate their payloads in order (the
    // ascii/binary distinction is re-derived from the eexec split below).
    let linear = if data.first() == Some(&0x80) {
        let mut out = Vec::with_capacity(data.len());
        let mut pos = 0usize;
        while pos + 2 <= data.len() && data[pos] == 0x80 {
            match data[pos + 1] {
                3 => break,
                1 | 2 => {
                    let len = u32::from_le_bytes(data.get(pos + 2..pos + 6)?.try_into().ok()?);
                    let end = (pos + 6).checked_add(len as usize)?;
                    out.extend_from_slice(data.get(pos + 6..end)?);
                    pos = end;
                }
                _ => return None,
            }
        }
        out
    } else {
        data.to_vec()
    };

    // The cleartext ends at the `eexec` keyword; encrypted data begins after
    // the single following whitespace run.
    let eexec_at = find(&linear, b"eexec")?;
    let cleartext = linear[..eexec_at].to_vec();
    let mut enc_start = eexec_at + b"eexec".len();
    while linear
        .get(enc_start)
        .is_some_and(|b| b.is_ascii_whitespace())
    {
        enc_start += 1;
    }
    let encrypted = linear.get(enc_start..)?;
    if encrypted.len() < 8 {
        return None;
    }

    // Hex form: the spec guarantees a *binary* section never starts with
    // four ASCII-hex bytes.
    let is_hex = encrypted[..4].iter().all(u8::is_ascii_hexdigit);
    let binary: Vec<u8> = if is_hex {
        let mut bytes = Vec::with_capacity(encrypted.len() / 2);
        let mut nibble: Option<u8> = None;
        for &b in encrypted {
            let v = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ if b.is_ascii_whitespace() => continue,
                // First non-hex byte ends the section (the plaintext 512-zero
                // trailer region is tolerated: it decodes to junk the
                // private-dict parser never reaches).
                _ => break,
            };
            match nibble.take() {
                None => nibble = Some(v),
                Some(hi) => bytes.push((hi << 4) | v),
            }
        }
        bytes
    } else {
        encrypted.to_vec()
    };

    let plain = decrypt(&binary, EEXEC_R, 4)?;
    Some((cleartext, plain))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// PostScript-subset tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Token<'a> {
    /// `/name` literal.
    Name(&'a [u8]),
    Number(f64),
    /// Any executable token (`def`, `dup`, `RD`, `StandardEncoding`, ...).
    Word(&'a [u8]),
    /// One of `[ ] { }`.
    Delim(u8),
}

struct Lexer<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(data: &'a [u8]) -> Self {
        Lexer { data, pos: 0 }
    }

    fn skip_ws(&mut self) {
        while let Some(&b) = self.data.get(self.pos) {
            if b.is_ascii_whitespace() || b == 0 {
                self.pos += 1;
            } else if b == b'%' {
                while self.data.get(self.pos).is_some_and(|&c| c != b'\n') {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn next(&mut self) -> Option<Token<'a>> {
        self.skip_ws();
        let &b = self.data.get(self.pos)?;
        if matches!(b, b'[' | b']' | b'{' | b'}') {
            self.pos += 1;
            return Some(Token::Delim(b));
        }
        let start = self.pos;
        if b == b'/' {
            self.pos += 1;
            let name_start = self.pos;
            self.consume_regular();
            return Some(Token::Name(&self.data[name_start..self.pos]));
        }
        self.consume_regular();
        let word = &self.data[start..self.pos];
        if word.is_empty() {
            // A lone delimiter we do not model (e.g. `(`); skip the byte so
            // scanning always advances.
            self.pos += 1;
            return Some(Token::Word(&self.data[start..self.pos]));
        }
        match parse_number(word) {
            Some(v) => Some(Token::Number(v)),
            None => Some(Token::Word(word)),
        }
    }

    fn consume_regular(&mut self) {
        while let Some(&b) = self.data.get(self.pos) {
            if b.is_ascii_whitespace()
                || matches!(
                    b,
                    0 | b'/' | b'[' | b']' | b'{' | b'}' | b'(' | b')' | b'%' | b'<'
                )
            {
                break;
            }
            self.pos += 1;
        }
    }

    /// Consume the single whitespace byte separating an `RD`-style operator
    /// from its binary payload, then the payload itself.
    fn read_binary(&mut self, len: usize) -> Option<&'a [u8]> {
        let payload = self.pos.checked_add(1)?;
        let end = payload.checked_add(len)?;
        if !self.data.get(self.pos)?.is_ascii_whitespace() {
            return None;
        }
        let bytes = self.data.get(payload..end)?;
        self.pos = end;
        Some(bytes)
    }
}

fn parse_number(word: &[u8]) -> Option<f64> {
    let s = std::str::from_utf8(word).ok()?;
    // PostScript radix numbers (16#FF) and other exotica are not numbers we
    // model; plain decimal/real syntax only.
    if !s
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'))
    {
        return None;
    }
    s.parse::<f64>().ok().filter(|v| v.is_finite())
}

// ---------------------------------------------------------------------------
// Font-program parsing
// ---------------------------------------------------------------------------

/// Parse an embedded Type1 font program (raw `/FontFile` payload or PFB).
pub(crate) fn parse(data: &[u8]) -> Option<Type1Font> {
    let (cleartext, private) = split_and_decrypt(data)?;

    let mut font_name: Option<Vec<u8>> = None;
    let mut font_matrix: Option<[f64; 6]> = None;
    let mut font_bbox: Option<[f64; 4]> = None;
    let mut paint_type = 0.0f64;
    let mut encoding: Option<Vec<Option<Vec<u8>>>> = None;

    let mut lex = Lexer::new(&cleartext);
    while let Some(tok) = lex.next() {
        let Token::Name(key) = tok else { continue };
        match key {
            b"FontName" => {
                if let Some(Token::Name(n)) = lex.next() {
                    font_name = Some(n.to_vec());
                }
            }
            b"PaintType" => {
                if let Some(Token::Number(v)) = lex.next() {
                    paint_type = v;
                }
            }
            b"FontMatrix" => {
                let v = parse_array(&mut lex, 6)?;
                font_matrix = Some(v.try_into().ok()?);
            }
            b"FontBBox" => {
                let v = parse_array(&mut lex, 4)?;
                font_bbox = Some(v.try_into().ok()?);
            }
            b"Encoding" => {
                encoding = Some(parse_encoding(&mut lex)?);
            }
            _ => {}
        }
    }

    let mut font = Type1Font {
        font_name: font_name?,
        font_matrix: font_matrix?,
        font_bbox: font_bbox?,
        paint_type,
        encoding: encoding?,
        charstrings: BTreeMap::new(),
        subrs: Vec::new(),
        private: PrivateHints::default(),
    };
    parse_private(&private, &mut font)?;
    if font.charstrings.is_empty() {
        return None;
    }
    Some(font)
}

/// `[ n n n ... ]` or `{ n n n ... }` with exactly `count` numbers.
fn parse_array(lex: &mut Lexer, count: usize) -> Option<Vec<f64>> {
    let open = lex.next()?;
    let close = match open {
        Token::Delim(b'[') => b']',
        Token::Delim(b'{') => b'}',
        _ => return None,
    };
    let mut out = Vec::with_capacity(count);
    loop {
        match lex.next()? {
            Token::Number(v) => out.push(v),
            Token::Delim(d) if d == close => break,
            _ => return None,
        }
        if out.len() > count {
            return None;
        }
    }
    (out.len() == count).then_some(out)
}

/// The built-in `/Encoding`: either the `StandardEncoding` name or an array
/// populated by `dup <code> /<name> put` statements. Any construction we
/// cannot fully replicate (copying slices of another encoding vector) fails
/// the parse.
fn parse_encoding(lex: &mut Lexer) -> Option<Vec<Option<Vec<u8>>>> {
    let mut entries: Vec<Option<Vec<u8>>> = vec![None; 256];
    let mut first = true;
    loop {
        match lex.next()? {
            Token::Word(b"StandardEncoding") if first => {
                for (slot, name) in entries.iter_mut().zip(encodings::STANDARD_NAMES.iter()) {
                    if !name.is_empty() {
                        *slot = Some(name.as_bytes().to_vec());
                    }
                }
                return Some(entries);
            }
            Token::Word(b"def") | Token::Word(b"readonly") => {
                // `readonly def` terminates the array form; accepting bare
                // `readonly` here is safe because nothing else follows it in
                // the constructions we accept.
                return Some(entries);
            }
            Token::Word(b"dup") => {
                // `dup <code> /<name> put`; anything else after `dup` is a
                // construction we do not model.
                let Some(Token::Number(code)) = lex.next() else {
                    return None;
                };
                let Some(Token::Name(name)) = lex.next() else {
                    return None;
                };
                let Some(Token::Word(b"put")) = lex.next() else {
                    return None;
                };
                if !(0.0..=255.0).contains(&code) || code.fract() != 0.0 {
                    return None;
                }
                entries[code as usize] = Some(name.to_vec());
            }
            Token::Word(w)
                if w == b"getinterval" || w == b"putinterval" || w == b"StandardEncoding" =>
            {
                // Copying from another encoding: semantics we decline to
                // replicate.
                return None;
            }
            _ => {}
        }
        first = false;
    }
}

/// Sequentially parse the decrypted private section: `lenIV`, hint
/// parameters, `/Subrs`, `/CharStrings`. Sequential tokenization (with the
/// binary payloads consumed in place) guarantees we never scan for keywords
/// *inside* charstring bytes.
fn parse_private(data: &[u8], font: &mut Type1Font) -> Option<()> {
    let mut lex = Lexer::new(data);
    let mut len_iv = 4usize;

    while let Some(tok) = lex.next() {
        let Token::Name(key) = tok else { continue };
        match key {
            b"lenIV" => match lex.next()? {
                Token::Number(v) if v.fract() == 0.0 && (0.0..=16.0).contains(&v) => {
                    len_iv = v as usize;
                }
                // Negative lenIV (unencrypted charstrings) is a rarity we
                // decline rather than half-support.
                _ => return None,
            },
            b"BlueValues" => font.private.blue_values = parse_number_array(&mut lex, 14)?,
            b"OtherBlues" => font.private.other_blues = parse_number_array(&mut lex, 10)?,
            b"FamilyBlues" => font.private.family_blues = parse_number_array(&mut lex, 14)?,
            b"FamilyOtherBlues" => {
                font.private.family_other_blues = parse_number_array(&mut lex, 10)?;
            }
            b"BlueScale" => font.private.blue_scale = next_number(&mut lex),
            b"BlueShift" => font.private.blue_shift = next_number(&mut lex),
            b"BlueFuzz" => font.private.blue_fuzz = next_number(&mut lex),
            b"StdHW" => font.private.std_hw = parse_number_array(&mut lex, 1)?.first().copied(),
            b"StdVW" => font.private.std_vw = parse_number_array(&mut lex, 1)?.first().copied(),
            b"StemSnapH" => font.private.stem_snap_h = parse_number_array(&mut lex, 12)?,
            b"StemSnapV" => font.private.stem_snap_v = parse_number_array(&mut lex, 12)?,
            b"ForceBold" => match lex.next()? {
                Token::Word(b"true") => font.private.force_bold = Some(true),
                Token::Word(b"false") => font.private.force_bold = Some(false),
                _ => return None,
            },
            b"LanguageGroup" => font.private.language_group = next_number(&mut lex),
            b"Subrs" => parse_subrs(&mut lex, len_iv, font)?,
            b"CharStrings" => {
                parse_charstrings(&mut lex, len_iv, font)?;
                // Everything after `end` is trailer (`cleartomark`, the
                // zero region under hex containers) — do not scan it.
                return Some(());
            }
            _ => {}
        }
    }
    None
}

fn next_number(lex: &mut Lexer) -> Option<f64> {
    match lex.next()? {
        Token::Number(v) => Some(v),
        _ => None,
    }
}

/// `[ ... ]` / `{ ... }` of at most `max` numbers (spec-bounded arrays).
fn parse_number_array(lex: &mut Lexer, max: usize) -> Option<Vec<f64>> {
    let open = lex.next()?;
    let close = match open {
        Token::Delim(b'[') => b']',
        Token::Delim(b'{') => b'}',
        _ => return None,
    };
    let mut out = Vec::new();
    loop {
        match lex.next()? {
            Token::Number(v) => out.push(v),
            Token::Delim(d) if d == close => return Some(out),
            _ => return None,
        }
        if out.len() > max {
            return None;
        }
    }
}

/// `/Subrs <n> array` followed by `dup <index> <len> RD <bytes> NP` entries.
fn parse_subrs(lex: &mut Lexer, len_iv: usize, font: &mut Type1Font) -> Option<()> {
    let count = match lex.next()? {
        Token::Number(v) if v.fract() == 0.0 && (0.0..=MAX_GLYPHS as f64).contains(&v) => {
            v as usize
        }
        _ => return None,
    };
    font.subrs = vec![Vec::new(); count];
    let mut seen = 0usize;
    while seen < count {
        match lex.next()? {
            // `array`, `ND`-style epilogues, `noaccess put` interleavings.
            Token::Word(w) if w != b"dup" => {}
            Token::Word(_) => {
                let idx = match lex.next()? {
                    Token::Number(v) if v.fract() == 0.0 && v >= 0.0 => v as usize,
                    _ => return None,
                };
                let len = match lex.next()? {
                    Token::Number(v) if v.fract() == 0.0 && (0.0..=1e7).contains(&v) => v as usize,
                    _ => return None,
                };
                let Token::Word(_) = lex.next()? else {
                    return None;
                };
                let raw = lex.read_binary(len)?;
                let plain = decrypt(raw, CHARSTRING_R, len_iv)?;
                *font.subrs.get_mut(idx)? = plain;
                seen += 1;
            }
            _ => return None,
        }
    }
    Some(())
}

/// `/CharStrings <n> dict dup begin` followed by `/<name> <len> RD <bytes>
/// ND` entries, terminated by `end`.
fn parse_charstrings(lex: &mut Lexer, len_iv: usize, font: &mut Type1Font) -> Option<()> {
    // Skip forward to `begin`.
    loop {
        match lex.next()? {
            Token::Word(b"begin") => break,
            Token::Number(_) | Token::Word(_) => {}
            _ => return None,
        }
    }
    loop {
        match lex.next()? {
            Token::Word(b"end") => break,
            Token::Name(name) => {
                let len = match lex.next()? {
                    Token::Number(v) if v.fract() == 0.0 && (0.0..=1e7).contains(&v) => v as usize,
                    _ => return None,
                };
                let Token::Word(_) = lex.next()? else {
                    return None;
                };
                let raw = lex.read_binary(len)?;
                let plain = decrypt(raw, CHARSTRING_R, len_iv)?;
                if font.charstrings.len() >= MAX_GLYPHS {
                    return None;
                }
                font.charstrings.insert(name.to_vec(), plain);
            }
            // `ND`-style epilogue words between entries.
            Token::Word(_) => {}
            _ => return None,
        }
    }
    Some(())
}

// ---------------------------------------------------------------------------
// Type1 charstring interpretation
// ---------------------------------------------------------------------------

/// One relative path operation (Type2-shaped: closepath is implicit).
#[derive(Clone, Copy)]
enum PathOp {
    Move(f64, f64),
    Line(f64, f64),
    Curve(f64, f64, f64, f64, f64, f64),
}

impl PathOp {
    fn delta(&self) -> (f64, f64) {
        match *self {
            PathOp::Move(dx, dy) | PathOp::Line(dx, dy) => (dx, dy),
            PathOp::Curve(a, b, c, d, e, f) => (a + c + e, b + d + f),
        }
    }
}

/// A stem hint in absolute glyph space (sidebearing already applied).
#[derive(Clone, Copy, PartialEq)]
struct Stem {
    horiz: bool,
    lo: f64,
    ext: f64,
}

/// A run of path operations under one active stem set (hint-replacement
/// boundaries split runs).
struct Segment {
    /// Indices into `Glyph::stems`.
    active: Vec<usize>,
    ops: Vec<PathOp>,
}

/// `seac` composite parameters (Type1 operand order).
struct Seac {
    asb: f64,
    adx: f64,
    ady: f64,
    bchar: u8,
    achar: u8,
}

/// A fully interpreted glyph.
struct Glyph {
    width: f64,
    sb: (f64, f64),
    stems: Vec<Stem>,
    segments: Vec<Segment>,
    seac: Option<Seac>,
}

struct Interp<'a> {
    font: &'a Type1Font,
    stack: Vec<f64>,
    /// The `callothersubr`/`pop` result channel.
    ps_stack: Vec<f64>,
    width: Option<f64>,
    sb: (f64, f64),
    stems: Vec<Stem>,
    segments: Vec<Segment>,
    active: Vec<usize>,
    /// Active set changed since the last path op (next path op starts a new
    /// segment).
    dirty: bool,
    /// OtherSubr 3 seen: the next stem declaration replaces the active set.
    replace_pending: bool,
    in_flex: bool,
    flex_pts: Vec<(f64, f64)>,
    path_started: bool,
    /// Absolute current point (flex bookkeeping / final `setcurrentpoint`).
    cur: (f64, f64),
    tokens: usize,
    seac: Option<Seac>,
}

enum Flow {
    Continue,
    End,
}

impl<'a> Interp<'a> {
    fn new(font: &'a Type1Font) -> Self {
        Interp {
            font,
            stack: Vec::new(),
            ps_stack: Vec::new(),
            width: None,
            sb: (0.0, 0.0),
            stems: Vec::new(),
            segments: Vec::new(),
            active: Vec::new(),
            dirty: false,
            replace_pending: false,
            in_flex: false,
            flex_pts: Vec::new(),
            path_started: false,
            cur: (0.0, 0.0),
            tokens: 0,
            seac: None,
        }
    }

    fn push(&mut self, v: f64) -> Option<()> {
        if self.stack.len() >= MAX_STACK {
            return None;
        }
        self.stack.push(v);
        Some(())
    }

    fn pop(&mut self) -> Option<f64> {
        self.stack.pop()
    }

    /// Pop `n` values, returned in operand (push) order.
    fn take(&mut self, n: usize) -> Option<Vec<f64>> {
        if self.stack.len() < n {
            return None;
        }
        Some(self.stack.split_off(self.stack.len() - n))
    }

    fn add_stem(&mut self, horiz: bool, lo: f64, ext: f64) -> Option<()> {
        if self.replace_pending {
            self.active.clear();
            self.replace_pending = false;
            self.dirty = true;
        }
        let stem = Stem { horiz, lo, ext };
        let idx = match self.stems.iter().position(|s| *s == stem) {
            Some(idx) => idx,
            None => {
                if self.stems.len() >= MAX_STEMS {
                    return None;
                }
                self.stems.push(stem);
                self.stems.len() - 1
            }
        };
        if !self.active.contains(&idx) {
            self.active.push(idx);
            self.dirty = true;
        }
        Some(())
    }

    fn path_op(&mut self, op: PathOp) {
        let (dx, dy) = op.delta();
        self.cur.0 += dx;
        self.cur.1 += dy;
        if self.segments.is_empty() || self.dirty {
            let mut active = self.active.clone();
            active.sort_unstable();
            self.segments.push(Segment {
                active,
                ops: Vec::new(),
            });
            self.dirty = false;
        }
        self.segments.last_mut().expect("just ensured").ops.push(op);
    }

    fn moveto(&mut self, mut dx: f64, mut dy: f64) {
        if self.in_flex {
            self.flex_pts.push((dx, dy));
            self.cur.0 += dx;
            self.cur.1 += dy;
            return;
        }
        if !self.path_started {
            // Type1 starts the path at the sidebearing point; Type2 at the
            // origin. Fold the difference into the first moveto.
            dx += self.sb.0;
            dy += self.sb.1;
            self.path_started = true;
        }
        self.path_op(PathOp::Move(dx, dy));
    }

    fn exec(&mut self, code: &[u8], depth: usize) -> Option<Flow> {
        if depth > MAX_SUBR_DEPTH {
            return None;
        }
        let mut pos = 0usize;
        while pos < code.len() {
            self.tokens += 1;
            if self.tokens > MAX_GLYPH_TOKENS {
                return None;
            }
            let b0 = code[pos];
            pos += 1;
            match b0 {
                32..=246 => self.push(f64::from(i32::from(b0) - 139))?,
                247..=250 => {
                    let b1 = *code.get(pos)?;
                    pos += 1;
                    self.push(f64::from((i32::from(b0) - 247) * 256 + i32::from(b1) + 108))?;
                }
                251..=254 => {
                    let b1 = *code.get(pos)?;
                    pos += 1;
                    self.push(f64::from(
                        -(i32::from(b0) - 251) * 256 - i32::from(b1) - 108,
                    ))?;
                }
                255 => {
                    let raw = code.get(pos..pos + 4)?;
                    pos += 4;
                    self.push(f64::from(i32::from_be_bytes(raw.try_into().ok()?)))?;
                }
                1 => {
                    // hstem: y dy (y relative to the sidebearing point).
                    let a = self.take(2)?;
                    self.add_stem(true, a[0] + self.sb.1, a[1])?;
                }
                3 => {
                    // vstem: x dx.
                    let a = self.take(2)?;
                    self.add_stem(false, a[0] + self.sb.0, a[1])?;
                }
                4 => {
                    let a = self.take(1)?;
                    self.moveto(0.0, a[0]);
                }
                5 => {
                    let a = self.take(2)?;
                    self.path_op(PathOp::Line(a[0], a[1]));
                }
                6 => {
                    let a = self.take(1)?;
                    self.path_op(PathOp::Line(a[0], 0.0));
                }
                7 => {
                    let a = self.take(1)?;
                    self.path_op(PathOp::Line(0.0, a[0]));
                }
                8 => {
                    let a = self.take(6)?;
                    self.path_op(PathOp::Curve(a[0], a[1], a[2], a[3], a[4], a[5]));
                }
                9 => {
                    // closepath: implicit in Type2.
                    self.stack.clear();
                }
                10 => {
                    let idx = self.pop()?;
                    if idx.fract() != 0.0 || idx < 0.0 {
                        return None;
                    }
                    let subr = self.font.subrs.get(idx as usize)?.clone();
                    if let Flow::End = self.exec(&subr, depth + 1)? {
                        return Some(Flow::End);
                    }
                }
                11 => return Some(Flow::Continue),
                13 => {
                    // hsbw: sbx wx.
                    let a = self.take(2)?;
                    if self.path_started {
                        return None;
                    }
                    self.sb = (a[0], 0.0);
                    self.width = Some(a[1]);
                    self.stack.clear();
                }
                14 => return Some(Flow::End),
                21 => {
                    let a = self.take(2)?;
                    self.moveto(a[0], a[1]);
                }
                22 => {
                    let a = self.take(1)?;
                    self.moveto(a[0], 0.0);
                }
                30 => {
                    let a = self.take(4)?;
                    self.path_op(PathOp::Curve(0.0, a[0], a[1], a[2], a[3], 0.0));
                }
                31 => {
                    let a = self.take(4)?;
                    self.path_op(PathOp::Curve(a[0], 0.0, a[1], a[2], 0.0, a[3]));
                }
                12 => {
                    let b1 = *code.get(pos)?;
                    pos += 1;
                    match b1 {
                        0 => self.stack.clear(), // dotsection
                        1 => {
                            // vstem3: x0 dx0 x1 dx1 x2 dx2.
                            let a = self.take(6)?;
                            for pair in a.chunks_exact(2) {
                                self.add_stem(false, pair[0] + self.sb.0, pair[1])?;
                            }
                        }
                        2 => {
                            let a = self.take(6)?;
                            for pair in a.chunks_exact(2) {
                                self.add_stem(true, pair[0] + self.sb.1, pair[1])?;
                            }
                        }
                        6 => {
                            // seac: asb adx ady bchar achar.
                            let a = self.take(5)?;
                            let (bchar, achar) = (
                                u8::try_from(a[3] as i64).ok()?,
                                u8::try_from(a[4] as i64).ok()?,
                            );
                            if a[3].fract() != 0.0 || a[4].fract() != 0.0 {
                                return None;
                            }
                            return self.finish_seac(Seac {
                                asb: a[0],
                                adx: a[1],
                                ady: a[2],
                                bchar,
                                achar,
                            });
                        }
                        7 => {
                            // sbw: sbx sby wx wy.
                            let a = self.take(4)?;
                            if self.path_started || a[3] != 0.0 {
                                return None;
                            }
                            self.sb = (a[0], a[1]);
                            self.width = Some(a[2]);
                            self.stack.clear();
                        }
                        12 => {
                            let b = self.pop()?;
                            let a = self.pop()?;
                            if b == 0.0 {
                                return None;
                            }
                            self.push(a / b)?;
                        }
                        16 => self.callothersubr()?,
                        17 => {
                            let v = self.ps_stack.pop()?;
                            self.push(v)?;
                        }
                        33 => self.stack.clear(), // setcurrentpoint
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
        Some(Flow::Continue)
    }

    /// `seac` ends the charstring; record the composite parameters (path and
    /// stems, if any were emitted before it, are discarded — a conforming
    /// seac charstring is `sb w hsbw asb adx ady bchar achar seac`).
    fn finish_seac(&mut self, seac: Seac) -> Option<Flow> {
        if self.path_started {
            return None;
        }
        self.segments.clear();
        self.stems.clear();
        self.seac = Some(seac);
        Some(Flow::End)
    }

    fn callothersubr(&mut self) -> Option<()> {
        let othersubr = self.pop()?;
        let n = self.pop()?;
        if othersubr.fract() != 0.0 || n.fract() != 0.0 || n < 0.0 {
            return None;
        }
        let args = self.take(n as usize)?;
        match othersubr as i64 {
            0 => {
                // Flex end. The Adobe OtherSubrs[0] takes 17 args; the
                // reduced private protocol dvips-embedded fonts use takes 3
                // (flex height, end x, end y). Either way the outline comes
                // from the collected rmoveto points, which is what
                // rasterizers use too.
                if !matches!(args.len(), 3 | 17) || !self.in_flex || self.flex_pts.len() != 7 {
                    return None;
                }
                let p = std::mem::take(&mut self.flex_pts);
                self.in_flex = false;
                // p[0] is the flex reference point; fold its delta into the
                // first control point of the first curve.
                self.path_op(PathOp::Curve(
                    p[0].0 + p[1].0,
                    p[0].1 + p[1].1,
                    p[2].0,
                    p[2].1,
                    p[3].0,
                    p[3].1,
                ));
                self.path_op(PathOp::Curve(
                    p[4].0, p[4].1, p[5].0, p[5].1, p[6].0, p[6].1,
                ));
                // The charstring follows with `pop pop setcurrentpoint`.
                self.ps_stack.push(self.cur.1);
                self.ps_stack.push(self.cur.0);
            }
            1 => {
                if !args.is_empty() || self.in_flex || !self.path_started {
                    return None;
                }
                self.in_flex = true;
                self.flex_pts.clear();
            }
            2 => {
                if !args.is_empty() || !self.in_flex {
                    return None;
                }
            }
            3 => {
                // Hint replacement: the following `pop callsubr` executes a
                // subr whose stem declarations replace the active set.
                if args.len() != 1 {
                    return None;
                }
                self.replace_pending = true;
                self.ps_stack.push(args[0]);
            }
            _ => return None,
        }
        Some(())
    }
}

/// Interpret one named glyph. `allow_seac` guards against recursive
/// composites (a seac component containing another seac).
fn interpret_glyph(font: &Type1Font, name: &[u8], allow_seac: bool) -> Option<Glyph> {
    let charstring = font.charstrings.get(name)?;
    let mut interp = Interp::new(font);
    interp.exec(charstring, 0)?;
    if interp.in_flex {
        return None;
    }
    let mut glyph = Glyph {
        width: interp.width?,
        sb: interp.sb,
        stems: interp.stems,
        segments: interp.segments,
        seac: interp.seac,
    };
    if let Some(seac) = glyph.seac.take() {
        if !allow_seac {
            return None;
        }
        glyph = compose_seac(font, &glyph, &seac)?;
    }
    Some(glyph)
}

/// Inline a `seac` composite: base outline followed by the accent outline
/// translated by the Type1 rule `(sbx + adx - asb, ady)`. Stem hints are
/// dropped (the components' hints are valid only in their own coordinate
/// frames); unhinted rendering is unaffected and the outline is exact.
fn compose_seac(font: &Type1Font, composite: &Glyph, seac: &Seac) -> Option<Glyph> {
    let bname = std_name(seac.bchar)?;
    let aname = std_name(seac.achar)?;
    let base = interpret_glyph(font, bname, false)?;
    let accent = interpret_glyph(font, aname, false)?;

    let mut ops: Vec<PathOp> = Vec::new();
    let mut end = (0.0f64, 0.0f64);
    for seg in &base.segments {
        for op in &seg.ops {
            let (dx, dy) = op.delta();
            end.0 += dx;
            end.1 += dy;
            ops.push(*op);
        }
    }
    let translate = (
        composite.sb.0 + seac.adx - seac.asb,
        composite.sb.1 + seac.ady,
    );
    let mut first = true;
    for seg in &accent.segments {
        for op in &seg.ops {
            let mut op = *op;
            if first {
                // The accent's first moveto is relative to the composite
                // origin plus the accent displacement; re-base it against
                // where the base outline ended.
                let PathOp::Move(dx, dy) = op else {
                    return None;
                };
                op = PathOp::Move(dx + translate.0 - end.0, dy + translate.1 - end.1);
                first = false;
            }
            ops.push(op);
        }
    }
    if first {
        // An accent with no outline is not a composite we understand.
        return None;
    }
    Some(Glyph {
        width: composite.width,
        sb: composite.sb,
        stems: Vec::new(),
        segments: vec![Segment {
            active: Vec::new(),
            ops,
        }],
        seac: None,
    })
}

fn std_name(code: u8) -> Option<&'static [u8]> {
    let name = encodings::STANDARD_NAMES[usize::from(code)];
    (!name.is_empty()).then_some(name.as_bytes())
}

// ---------------------------------------------------------------------------
// Type2 charstring emission
// ---------------------------------------------------------------------------

fn t2_number(out: &mut Vec<u8>, v: f64) -> Option<()> {
    if v.fract() == 0.0 && (-32768.0..=32767.0).contains(&v) {
        let i = v as i32;
        match i {
            -107..=107 => out.push((i + 139) as u8),
            108..=1131 => {
                let d = i - 108;
                out.push((d / 256 + 247) as u8);
                out.push((d % 256) as u8);
            }
            -1131..=-108 => {
                let d = -i - 108;
                out.push((d / 256 + 251) as u8);
                out.push((d % 256) as u8);
            }
            _ => {
                out.push(28);
                out.extend_from_slice(&(i as i16).to_be_bytes());
            }
        }
    } else {
        let fixed = (v * 65536.0).round();
        if !(-32768.0 * 65536.0..=32767.0 * 65536.0 + 65535.0).contains(&fixed) {
            return None;
        }
        out.push(255);
        out.extend_from_slice(&(fixed as i32).to_be_bytes());
    }
    Some(())
}

fn t2_op(out: &mut Vec<u8>, op: u8) {
    out.push(op);
}

/// Sort stems into canonical order and return (hstems, vstems, index map
/// from original stem index to hintmask bit).
fn order_stems(stems: &[Stem]) -> (Vec<Stem>, Vec<Stem>, Vec<usize>) {
    let mut h: Vec<(usize, Stem)> = Vec::new();
    let mut v: Vec<(usize, Stem)> = Vec::new();
    for (i, s) in stems.iter().enumerate() {
        if s.horiz {
            h.push((i, *s));
        } else {
            v.push((i, *s));
        }
    }
    let key = |s: &Stem| (s.lo, s.lo + s.ext);
    h.sort_by(|a, b| {
        key(&a.1)
            .partial_cmp(&key(&b.1))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    v.sort_by(|a, b| {
        key(&a.1)
            .partial_cmp(&key(&b.1))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut bit_of = vec![0usize; stems.len()];
    for (bit, (orig, _)) in h.iter().chain(v.iter()).enumerate() {
        bit_of[*orig] = bit;
    }
    (
        h.into_iter().map(|(_, s)| s).collect(),
        v.into_iter().map(|(_, s)| s).collect(),
        bit_of,
    )
}

/// Emit one stem list as delta-encoded operands.
fn push_stem_args(out: &mut Vec<u8>, stems: &[Stem]) -> Option<()> {
    let mut prev = 0.0f64;
    for s in stems {
        t2_number(out, s.lo - prev)?;
        t2_number(out, s.ext)?;
        prev = s.lo + s.ext;
    }
    Some(())
}

fn emit_charstring(glyph: &Glyph, default_width: f64, nominal_width: f64) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut width_pending = if glyph.width == default_width {
        None
    } else {
        Some(glyph.width - nominal_width)
    };

    let (hstems, vstems, bit_of) = order_stems(&glyph.stems);
    let nstems = hstems.len() + vstems.len();
    if nstems > MAX_STEMS {
        return None;
    }
    // One stem op carries at most 24 pairs (48-operand stack, minus the
    // width slot); hstemhm/vstemhm cannot be split.
    if hstems.len() > 23 || vstems.len() > 23 {
        return None;
    }
    // Zero declared stems means a hintmask would carry zero mask bytes;
    // segments are then simply concatenated.
    let need_masks = glyph.segments.len() > 1 && nstems > 0;

    if !hstems.is_empty() {
        if let Some(w) = width_pending.take() {
            t2_number(&mut out, w)?;
        }
        push_stem_args(&mut out, &hstems)?;
        t2_op(&mut out, if need_masks { 18 } else { 1 }); // hstemhm / hstem
    }
    if !vstems.is_empty() {
        if let Some(w) = width_pending.take() {
            t2_number(&mut out, w)?;
        }
        push_stem_args(&mut out, &vstems)?;
        t2_op(&mut out, if need_masks { 23 } else { 3 }); // vstemhm / vstem
    }

    let mask_len = nstems.div_ceil(8);
    for seg in &glyph.segments {
        if need_masks {
            let mut mask = vec![0u8; mask_len];
            for &stem_idx in &seg.active {
                let bit = bit_of[stem_idx];
                mask[bit / 8] |= 0x80 >> (bit % 8);
            }
            if let Some(w) = width_pending.take() {
                t2_number(&mut out, w)?;
            }
            t2_op(&mut out, 19); // hintmask
            out.extend_from_slice(&mask);
        }
        for op in &seg.ops {
            if let Some(w) = width_pending.take() {
                t2_number(&mut out, w)?;
            }
            match *op {
                PathOp::Move(dx, dy) => {
                    if dx == 0.0 {
                        t2_number(&mut out, dy)?;
                        t2_op(&mut out, 4); // vmoveto
                    } else if dy == 0.0 {
                        t2_number(&mut out, dx)?;
                        t2_op(&mut out, 22); // hmoveto
                    } else {
                        t2_number(&mut out, dx)?;
                        t2_number(&mut out, dy)?;
                        t2_op(&mut out, 21); // rmoveto
                    }
                }
                PathOp::Line(dx, dy) => {
                    if dy == 0.0 {
                        t2_number(&mut out, dx)?;
                        t2_op(&mut out, 6); // hlineto
                    } else if dx == 0.0 {
                        t2_number(&mut out, dy)?;
                        t2_op(&mut out, 7); // vlineto
                    } else {
                        t2_number(&mut out, dx)?;
                        t2_number(&mut out, dy)?;
                        t2_op(&mut out, 5); // rlineto
                    }
                }
                PathOp::Curve(a, b, c, d, e, f) => {
                    for v in [a, b, c, d, e, f] {
                        t2_number(&mut out, v)?;
                    }
                    t2_op(&mut out, 8); // rrcurveto
                }
            }
        }
    }
    if let Some(w) = width_pending.take() {
        t2_number(&mut out, w)?;
    }
    t2_op(&mut out, 14); // endchar
    Some(out)
}

// ---------------------------------------------------------------------------
// CFF assembly
// ---------------------------------------------------------------------------

/// CFF INDEX with minimal offset size. An empty INDEX is the 2-byte zero
/// count.
fn cff_index(items: &[Vec<u8>]) -> Option<Vec<u8>> {
    if items.is_empty() {
        return Some(vec![0, 0]);
    }
    let count = u16::try_from(items.len()).ok()?;
    let total: usize = items.iter().map(Vec::len).sum();
    let last_offset = total.checked_add(1)?;
    let off_size: u8 = match last_offset {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFF_FFFF => 3,
        _ => 4,
    };
    let mut out = Vec::with_capacity(3 + (items.len() + 1) * usize::from(off_size) + total);
    out.extend_from_slice(&count.to_be_bytes());
    out.push(off_size);
    let mut offset = 1usize;
    for i in 0..=items.len() {
        let bytes = (offset as u32).to_be_bytes();
        out.extend_from_slice(&bytes[4 - usize::from(off_size)..]);
        if let Some(item) = items.get(i) {
            offset += item.len();
        }
    }
    for item in items {
        out.extend_from_slice(item);
    }
    Some(out)
}

/// DICT integer operand.
fn dict_int(out: &mut Vec<u8>, v: i32) {
    match v {
        -107..=107 => out.push((v + 139) as u8),
        108..=1131 => {
            let d = v - 108;
            out.push((d / 256 + 247) as u8);
            out.push((d % 256) as u8);
        }
        -1131..=-108 => {
            let d = -v - 108;
            out.push((d / 256 + 251) as u8);
            out.push((d % 256) as u8);
        }
        -32768..=32767 => {
            out.push(28);
            out.extend_from_slice(&(v as i16).to_be_bytes());
        }
        _ => {
            out.push(29);
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
}

/// DICT real operand (nibble-BCD, format 30).
fn dict_real(out: &mut Vec<u8>, v: f64) -> Option<()> {
    if !v.is_finite() {
        return None;
    }
    let text = format!("{v}");
    let mut nibbles: Vec<u8> = Vec::with_capacity(text.len() + 2);
    let mut chars = text.bytes().peekable();
    while let Some(c) = chars.next() {
        match c {
            b'0'..=b'9' => nibbles.push(c - b'0'),
            b'.' => nibbles.push(0xa),
            b'-' => nibbles.push(0xe),
            b'e' | b'E' => {
                if chars.peek() == Some(&b'-') {
                    chars.next();
                    nibbles.push(0xc);
                } else {
                    if chars.peek() == Some(&b'+') {
                        chars.next();
                    }
                    nibbles.push(0xb);
                }
            }
            _ => return None,
        }
    }
    nibbles.push(0xf);
    if nibbles.len() % 2 == 1 {
        nibbles.push(0xf);
    }
    out.push(30);
    for pair in nibbles.chunks_exact(2) {
        out.push((pair[0] << 4) | pair[1]);
    }
    Some(())
}

/// DICT numeric operand: integer form when exact, BCD real otherwise.
fn dict_number(out: &mut Vec<u8>, v: f64) -> Option<()> {
    if v.fract() == 0.0 && (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&v) {
        dict_int(out, v as i32);
        Some(())
    } else {
        dict_real(out, v)
    }
}

fn dict_op(out: &mut Vec<u8>, op: u16) {
    if op > 0xFF {
        out.push(12);
        out.push((op & 0xFF) as u8);
    } else {
        out.push(op as u8);
    }
}

/// Fixed-width (5-byte) DICT integer, for offsets resolved after layout.
fn dict_int32(out: &mut Vec<u8>, v: u32) {
    out.push(29);
    out.extend_from_slice(&(v as i32).to_be_bytes());
}

fn dict_delta(out: &mut Vec<u8>, values: &[f64]) -> Option<()> {
    let mut prev = 0.0f64;
    for &v in values {
        dict_number(out, v - prev)?;
        prev = v;
    }
    Some(())
}

/// Emit the CFF Private DICT from the Type1 hints plus width parameters.
fn private_dict(hints: &PrivateHints, default_width: f64, nominal_width: f64) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    if !hints.blue_values.is_empty() {
        dict_delta(&mut out, &hints.blue_values)?;
        dict_op(&mut out, 6);
    }
    if !hints.other_blues.is_empty() {
        dict_delta(&mut out, &hints.other_blues)?;
        dict_op(&mut out, 7);
    }
    if !hints.family_blues.is_empty() {
        dict_delta(&mut out, &hints.family_blues)?;
        dict_op(&mut out, 8);
    }
    if !hints.family_other_blues.is_empty() {
        dict_delta(&mut out, &hints.family_other_blues)?;
        dict_op(&mut out, 9);
    }
    if let Some(v) = hints.blue_scale {
        dict_number(&mut out, v)?;
        dict_op(&mut out, 0x0C09);
    }
    if let Some(v) = hints.blue_shift {
        dict_number(&mut out, v)?;
        dict_op(&mut out, 0x0C0A);
    }
    if let Some(v) = hints.blue_fuzz {
        dict_number(&mut out, v)?;
        dict_op(&mut out, 0x0C0B);
    }
    if let Some(v) = hints.std_hw {
        dict_number(&mut out, v)?;
        dict_op(&mut out, 10);
    }
    if let Some(v) = hints.std_vw {
        dict_number(&mut out, v)?;
        dict_op(&mut out, 11);
    }
    if !hints.stem_snap_h.is_empty() {
        dict_delta(&mut out, &hints.stem_snap_h)?;
        dict_op(&mut out, 0x0C0C);
    }
    if !hints.stem_snap_v.is_empty() {
        dict_delta(&mut out, &hints.stem_snap_v)?;
        dict_op(&mut out, 0x0C0D);
    }
    if let Some(v) = hints.force_bold {
        dict_int(&mut out, i32::from(v));
        dict_op(&mut out, 0x0C0E);
    }
    if let Some(v) = hints.language_group {
        dict_number(&mut out, v)?;
        dict_op(&mut out, 0x0C11);
    }
    dict_number(&mut out, default_width)?;
    dict_op(&mut out, 20);
    dict_number(&mut out, nominal_width)?;
    dict_op(&mut out, 21);
    Some(out)
}

/// Convert `font` to a CFF (Type1C) subset containing `.notdef` plus the
/// glyphs named in `keep` (names without a charstring must not be passed).
pub(crate) fn convert_to_cff(
    font: &Type1Font,
    keep: &std::collections::BTreeSet<Vec<u8>>,
) -> Option<Vec<u8>> {
    if font.paint_type != 0.0 {
        return None;
    }
    if !font.has_glyph(b".notdef") {
        return None;
    }

    // Glyph order: .notdef, then encoded glyphs (by lowest built-in code —
    // the CFF format-0 encoding requires encoded glyphs to be a prefix of
    // the glyph order), then unencoded glyphs by name.
    let mut codes_of: BTreeMap<&[u8], Vec<u8>> = BTreeMap::new();
    for (code, entry) in font.encoding.iter().enumerate() {
        if let Some(name) = entry {
            if name != b".notdef" && keep.contains(name.as_slice()) {
                codes_of
                    .entry(name.as_slice())
                    .or_default()
                    .push(code as u8);
            }
        }
    }
    let mut encoded: Vec<&[u8]> = codes_of.keys().copied().collect();
    encoded.sort_by_key(|name| codes_of[name][0]);
    let mut order: Vec<&[u8]> = vec![b".notdef"];
    order.extend(encoded.iter().copied());
    for name in keep {
        if name.as_slice() != b".notdef" && !codes_of.contains_key(name.as_slice()) {
            order.push(name.as_slice());
        }
    }
    if order.len() > usize::from(u16::MAX) {
        return None;
    }

    // Interpret every kept glyph.
    let mut glyphs: Vec<Glyph> = Vec::with_capacity(order.len());
    for name in &order {
        glyphs.push(interpret_glyph(font, name, true)?);
    }

    // Width parameters: defaultWidthX = most common width; nominalWidthX
    // equal to it keeps the deltas small.
    let mut freq: BTreeMap<u64, usize> = BTreeMap::new();
    for g in &glyphs {
        *freq.entry(g.width.to_bits()).or_insert(0) += 1;
    }
    let default_width = f64::from_bits(
        freq.iter()
            .max_by_key(|(_, &count)| count)
            .map(|(&bits, _)| bits)?,
    );
    let nominal_width = default_width;

    let charstrings: Vec<Vec<u8>> = glyphs
        .iter()
        .map(|g| emit_charstring(g, default_width, nominal_width))
        .collect::<Option<_>>()?;

    // Strings: every non-standard-need is custom (deliberately no standard-
    // strings table: custom SIDs are valid CFF and remove a large constant
    // table as a typo surface; Flate absorbs the byte cost).
    let mut strings: Vec<Vec<u8>> = Vec::new();
    let sid_of = |name: &[u8], strings: &mut Vec<Vec<u8>>| -> Option<u16> {
        if name == b".notdef" {
            return Some(0);
        }
        if let Some(pos) = strings.iter().position(|s| s == name) {
            return u16::try_from(391 + pos).ok();
        }
        strings.push(name.to_vec());
        u16::try_from(391 + strings.len() - 1).ok()
    };

    // Charset (format 0): SIDs for glyphs 1..n.
    let mut charset = vec![0u8];
    for name in &order[1..] {
        let sid = sid_of(name, &mut strings)?;
        charset.extend_from_slice(&sid.to_be_bytes());
    }

    // Encoding (format 0 + supplements): the built-in Type1 encoding
    // restricted to retained glyphs.
    let n_encoded = encoded.len();
    let mut enc_codes: Vec<u8> = Vec::with_capacity(n_encoded);
    let mut supplements: Vec<(u8, u16)> = Vec::new();
    for name in &encoded {
        let codes = &codes_of[name];
        enc_codes.push(codes[0]);
        for &extra in &codes[1..] {
            let sid = sid_of(name, &mut strings)?;
            supplements.push((extra, sid));
        }
    }
    let mut encoding = Vec::with_capacity(2 + n_encoded + 1 + supplements.len() * 3);
    encoding.push(if supplements.is_empty() { 0u8 } else { 0x80 });
    encoding.push(u8::try_from(n_encoded).ok()?);
    encoding.extend_from_slice(&enc_codes);
    if !supplements.is_empty() {
        encoding.push(u8::try_from(supplements.len()).ok()?);
        for (code, sid) in &supplements {
            encoding.push(*code);
            encoding.extend_from_slice(&sid.to_be_bytes());
        }
    }

    let charstrings_index = cff_index(&charstrings)?;
    let private = private_dict(&font.private, default_width, nominal_width)?;

    // Top DICT: fixed-width offset operands make the layout a single pass.
    let default_matrix = [0.001, 0.0, 0.0, 0.001, 0.0, 0.0];
    let mut top_prefix = Vec::new();
    if font.font_matrix != default_matrix {
        for v in font.font_matrix {
            dict_number(&mut top_prefix, v)?;
        }
        dict_op(&mut top_prefix, 0x0C07);
    }
    for v in font.font_bbox {
        dict_number(&mut top_prefix, v)?;
    }
    dict_op(&mut top_prefix, 5);

    // charset(15) + Encoding(16) + CharStrings(17): 5-byte operand + 1-byte
    // op each; Private(18): two 5-byte operands + 1-byte op.
    let top_dict_len = top_prefix.len() + (5 + 1) * 3 + (5 + 5 + 1);
    let header = [1u8, 0, 4, 4];
    let name_index = cff_index(std::slice::from_ref(&font.font_name))?;
    let top_index = cff_index(&[vec![0u8; top_dict_len]])?;
    let string_index = cff_index(&strings)?;
    let gsubr_index = cff_index(&[])?;

    let fixed =
        header.len() + name_index.len() + top_index.len() + string_index.len() + gsubr_index.len();
    let charset_at = fixed;
    let encoding_at = charset_at + charset.len();
    let charstrings_at = encoding_at + encoding.len();
    let private_at = charstrings_at + charstrings_index.len();

    let mut top_dict = top_prefix;
    dict_int32(&mut top_dict, u32::try_from(charset_at).ok()?);
    dict_op(&mut top_dict, 15);
    dict_int32(&mut top_dict, u32::try_from(encoding_at).ok()?);
    dict_op(&mut top_dict, 16);
    dict_int32(&mut top_dict, u32::try_from(charstrings_at).ok()?);
    dict_op(&mut top_dict, 17);
    dict_int32(&mut top_dict, u32::try_from(private.len()).ok()?);
    dict_int32(&mut top_dict, u32::try_from(private_at).ok()?);
    dict_op(&mut top_dict, 18);
    debug_assert_eq!(top_dict.len(), top_dict_len);
    let top_index = cff_index(&[top_dict])?;

    let mut out = Vec::with_capacity(private_at + private.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&name_index);
    out.extend_from_slice(&top_index);
    out.extend_from_slice(&string_index);
    out.extend_from_slice(&gsubr_index);
    out.extend_from_slice(&charset);
    out.extend_from_slice(&encoding);
    out.extend_from_slice(&charstrings_index);
    out.extend_from_slice(&private);
    Some(out)
}

#[cfg(test)]
mod probe {
    fn disasm(code: &[u8]) -> String {
        let mut out = String::new();
        let mut pos = 0;
        while pos < code.len() {
            let b0 = code[pos];
            pos += 1;
            let tok = match b0 {
                32..=246 => format!("{} ", i32::from(b0) - 139),
                247..=250 => {
                    let b1 = code[pos];
                    pos += 1;
                    format!("{} ", (i32::from(b0) - 247) * 256 + i32::from(b1) + 108)
                }
                251..=254 => {
                    let b1 = code[pos];
                    pos += 1;
                    format!("{} ", -(i32::from(b0) - 251) * 256 - i32::from(b1) - 108)
                }
                255 => {
                    let v = i32::from_be_bytes(code[pos..pos + 4].try_into().unwrap());
                    pos += 4;
                    format!("{v} ")
                }
                12 => {
                    let b1 = code[pos];
                    pos += 1;
                    format!("esc{b1} ")
                }
                op => format!("op{op} "),
            };
            out.push_str(&tok);
        }
        out
    }

    #[test]
    #[ignore]
    fn dump_one() {
        let data = std::fs::read("target/scratch/t1/fonts/5871.pfa").unwrap();
        let f = super::parse(&data).unwrap();
        println!("one: {}", disasm(&f.charstrings[b"one".as_slice()]));
        for (i, s) in f.subrs.iter().enumerate() {
            println!("subr{i}: {}", disasm(s));
        }
    }

    #[test]
    #[ignore]
    fn probe_corpus_fonts() {
        let dir = std::path::Path::new("target/scratch/t1/fonts");
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let data = std::fs::read(&path).unwrap();
            match super::parse(&data) {
                None => println!("{}: PARSE FAIL", path.display()),
                Some(f) => {
                    let keep: std::collections::BTreeSet<Vec<u8>> =
                        f.charstrings.keys().cloned().collect();
                    match super::convert_to_cff(&f, &keep) {
                        None => {
                            let bad: Vec<String> = keep
                                .iter()
                                .filter(|n| super::interpret_glyph(&f, n, true).is_none())
                                .map(|n| String::from_utf8_lossy(n).into_owned())
                                .collect();
                            println!(
                                "{}: CONVERT FAIL ({} glyphs) bad={:?}",
                                path.display(),
                                keep.len(),
                                bad
                            );
                        }
                        Some(cff) => println!(
                            "{}: ok t1={} cff={} glyphs={}",
                            path.display(),
                            data.len(),
                            cff.len(),
                            keep.len()
                        ),
                    }
                }
            }
        }
    }
}
