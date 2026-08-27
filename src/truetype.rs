//! Minimal TrueType `cmap` machinery for the simple-font subsetting path.
//!
//! `subsetter` (the CID path's table writer) deliberately drops the `cmap`
//! table — CID-keyed PDF fonts never consult it. Simple TrueType fonts do:
//! the viewer resolves each character code through the font's `cmap`
//! (ISO 32000-1 9.6.6.4), so the subset must carry one. This module parses
//! the original font's `cmap` (formats 0, 4, 6, 12 — anything else fails the
//! whole font, fail-safe), and splices a freshly built `cmap` (replicating
//! the original subtables, restricted to retained glyphs, with remapped
//! glyph ids) into the subsetter's output, rebuilding the table directory
//! and checksums.

use std::collections::BTreeMap;

/// Upper bound on total parsed cmap mappings across all subtables; a table
/// expanding beyond this is pathological and disqualifies the font.
const MAX_CMAP_ENTRIES: usize = 1 << 20;

/// One `cmap` encoding record, fully enumerated. Mappings to glyph 0 are
/// omitted (equivalent to absence: both mean `.notdef`).
pub(crate) struct CmapSubtable {
    pub(crate) platform: u16,
    pub(crate) encoding: u16,
    pub(crate) map: BTreeMap<u32, u16>,
}

fn be16(data: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_be_bytes([
        *data.get(off)?,
        *data.get(off.checked_add(1)?)?,
    ]))
}

fn be32(data: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *data.get(off)?,
        *data.get(off.checked_add(1)?)?,
        *data.get(off.checked_add(2)?)?,
        *data.get(off.checked_add(3)?)?,
    ]))
}

/// Locate a top-level table in a single (non-collection) sfnt.
pub(crate) fn find_table(font: &[u8], tag: &[u8; 4]) -> Option<(usize, usize)> {
    let count = usize::from(be16(font, 4)?);
    for i in 0..count {
        let rec = 12 + i * 16;
        if font.get(rec..rec + 4)? == tag {
            let off = be32(font, rec + 8)? as usize;
            let len = be32(font, rec + 12)? as usize;
            font.get(off..off.checked_add(len)?)?;
            return Some((off, len));
        }
    }
    None
}

/// Parse and fully enumerate every `cmap` encoding record. `None` on any
/// structural doubt, including a subtable format outside {0, 4, 6, 12}: the
/// caller cannot replicate semantics it cannot read.
pub(crate) fn parse_cmap(font: &[u8]) -> Option<Vec<CmapSubtable>> {
    let (cmap_off, _) = find_table(font, b"cmap")?;
    let table = font.get(cmap_off..)?;
    let record_count = usize::from(be16(table, 2)?);
    let mut out = Vec::with_capacity(record_count);
    let mut total = 0usize;
    for i in 0..record_count {
        let rec = 4 + i * 8;
        let platform = be16(table, rec)?;
        let encoding = be16(table, rec + 2)?;
        let offset = be32(table, rec + 4)? as usize;
        let map = parse_subtable(table.get(offset..)?)?;
        total = total.checked_add(map.len())?;
        if total > MAX_CMAP_ENTRIES {
            return None;
        }
        out.push(CmapSubtable {
            platform,
            encoding,
            map,
        });
    }
    Some(out)
}

fn parse_subtable(data: &[u8]) -> Option<BTreeMap<u32, u16>> {
    let mut map = BTreeMap::new();
    match be16(data, 0)? {
        0 => {
            for code in 0u32..256 {
                let gid = u16::from(*data.get(6 + code as usize)?);
                if gid != 0 {
                    map.insert(code, gid);
                }
            }
        }
        4 => {
            let seg_count = usize::from(be16(data, 6)?) / 2;
            let end_base = 14;
            let start_base = end_base + seg_count * 2 + 2;
            let delta_base = start_base + seg_count * 2;
            let range_base = delta_base + seg_count * 2;
            for seg in 0..seg_count {
                let end = be16(data, end_base + seg * 2)?;
                let start = be16(data, start_base + seg * 2)?;
                if start > end {
                    return None;
                }
                let delta = be16(data, delta_base + seg * 2)?;
                let range_off = be16(data, range_base + seg * 2)?;
                for code in start..=end {
                    if code == 0xFFFF {
                        continue;
                    }
                    let gid = if range_off == 0 {
                        code.wrapping_add(delta)
                    } else {
                        let glyph_at = range_base
                            + seg * 2
                            + usize::from(range_off)
                            + usize::from(code - start) * 2;
                        let raw = be16(data, glyph_at)?;
                        if raw == 0 {
                            0
                        } else {
                            raw.wrapping_add(delta)
                        }
                    };
                    if gid != 0 {
                        map.insert(u32::from(code), gid);
                    }
                    if map.len() > MAX_CMAP_ENTRIES {
                        return None;
                    }
                }
            }
        }
        6 => {
            let first = u32::from(be16(data, 6)?);
            let count = usize::from(be16(data, 8)?);
            for i in 0..count {
                let gid = be16(data, 10 + i * 2)?;
                if gid != 0 {
                    map.insert(first + i as u32, gid);
                }
            }
        }
        12 => {
            let group_count = be32(data, 12)? as usize;
            for g in 0..group_count {
                let rec = 16 + g * 12;
                let start = be32(data, rec)?;
                let end = be32(data, rec + 4)?;
                let start_gid = be32(data, rec + 8)?;
                if start > end || end > 0x10_FFFF {
                    return None;
                }
                if map.len() + (end - start) as usize + 1 > MAX_CMAP_ENTRIES {
                    return None;
                }
                for code in start..=end {
                    let gid = start_gid.wrapping_add(code - start);
                    if gid != 0 {
                        // A gid beyond u16 cannot exist in a real font.
                        map.insert(code, u16::try_from(gid).ok()?);
                    }
                }
            }
        }
        _ => return None,
    }
    Some(map)
}

// ---------------------------------------------------------------------------
// cmap synthesis
// ---------------------------------------------------------------------------

/// Build one format 4 subtable (BMP chars only; caller guarantees). `None`
/// when the segment list would overflow the format's 16-bit length field.
fn build_format4(map: &BTreeMap<u32, u16>) -> Option<Vec<u8>> {
    // Merge consecutive (char, gid) runs where both advance by 1 into
    // delta-only segments, then append the mandatory 0xFFFF sentinel.
    let mut segments: Vec<(u16, u16, u16)> = Vec::new(); // (start, end, start_gid)
    for (&code, &gid) in map {
        let code = code as u16;
        match segments.last_mut() {
            Some((start, end, sgid))
                if code == end.wrapping_add(1) && gid == sgid.wrapping_add(code - *start) =>
            {
                *end = code;
            }
            _ => segments.push((code, code, gid)),
        }
    }
    segments.push((0xFFFF, 0xFFFF, 0)); // sentinel; delta computed below maps it to 0
    let seg_count = segments.len();

    let length = 16 + seg_count * 8;
    if length > usize::from(u16::MAX) {
        return None;
    }
    let mut out = Vec::with_capacity(length);
    out.extend_from_slice(&4u16.to_be_bytes());
    out.extend_from_slice(&(length as u16).to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // language
    out.extend_from_slice(&((seg_count * 2) as u16).to_be_bytes());
    let floor_log2 = (usize::BITS - 1 - seg_count.leading_zeros()) as u16;
    let search_range = 2u16 << floor_log2;
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&floor_log2.to_be_bytes());
    out.extend_from_slice(&((seg_count * 2) as u16 - search_range).to_be_bytes());
    for &(_, end, _) in &segments {
        out.extend_from_slice(&end.to_be_bytes());
    }
    out.extend_from_slice(&0u16.to_be_bytes()); // reservedPad
    for &(start, _, _) in &segments {
        out.extend_from_slice(&start.to_be_bytes());
    }
    for &(start, _, sgid) in &segments {
        out.extend_from_slice(&sgid.wrapping_sub(start).to_be_bytes());
    }
    for _ in &segments {
        out.extend_from_slice(&0u16.to_be_bytes()); // idRangeOffset: delta-only
    }
    Some(out)
}

/// Build one format 6 subtable (byte codes; caller guarantees chars <= 0xFF).
fn build_format6(map: &BTreeMap<u32, u16>) -> Vec<u8> {
    let first = map.keys().next().copied().unwrap_or(0) as u16;
    let last = map.keys().next_back().copied().unwrap_or(0) as u16;
    let count = last - first + 1;
    let mut out = Vec::with_capacity(10 + usize::from(count) * 2);
    out.extend_from_slice(&6u16.to_be_bytes());
    out.extend_from_slice(&(10 + count * 2).to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // language
    out.extend_from_slice(&first.to_be_bytes());
    out.extend_from_slice(&count.to_be_bytes());
    for code in first..=last {
        let gid = map.get(&u32::from(code)).copied().unwrap_or(0);
        out.extend_from_slice(&gid.to_be_bytes());
    }
    out
}

fn build_cmap_table(subtables: &[CmapSubtable]) -> Option<Vec<u8>> {
    let mut records: Vec<&CmapSubtable> = subtables.iter().collect();
    records.sort_by_key(|s| (s.platform, s.encoding));
    let mut header = Vec::new();
    header.extend_from_slice(&0u16.to_be_bytes());
    header.extend_from_slice(&(records.len() as u16).to_be_bytes());
    let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(records.len());
    for sub in &records {
        // Platform 1 (Macintosh) readers expect byte-oriented formats; format
        // 6 covers its 0..=255 code space exactly. Everything else gets a
        // format 4 (the universally supported Windows shape).
        if sub.platform == 1 {
            bodies.push(build_format6(&sub.map));
        } else {
            bodies.push(build_format4(&sub.map)?);
        }
    }
    let mut offset = 4 + records.len() * 8;
    let mut out = header;
    for (sub, body) in records.iter().zip(&bodies) {
        out.extend_from_slice(&sub.platform.to_be_bytes());
        out.extend_from_slice(&sub.encoding.to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        offset += body.len();
    }
    for body in &bodies {
        out.extend_from_slice(body);
    }
    Some(out)
}

fn table_checksum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    for chunk in data.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum = sum.wrapping_add(u32::from_be_bytes(word));
    }
    sum
}

/// Neutralise the six-letter `ABCDEF+` subset tags the producer left inside
/// the `name` table's string storage, replacing each with `AAAAAA`.
///
/// Two embeds of the same subset of the same font differ only in those tag
/// letters and in `head.checkSumAdjustment` (which the tags perturb), so
/// masking them makes byte-equal subsets *actually* byte-equal and the
/// document-wide stream dedup collapses them. Names in the `name` table play
/// no part in rendering an embedded PDF font — the viewer takes the subset
/// tag from `/BaseFont`, which picamatl rewrites from a content hash anyway, so
/// the mask is no more of a mismatch than what already ships.
///
/// Both 1-byte and UTF-16BE name strings are handled. `None` on any
/// structural doubt (caller then keeps the font as-is, fail-safe).
pub(crate) fn mask_subset_tags(font: &[u8]) -> Option<Vec<u8>> {
    let (name_off, name_len) = find_table(font, b"name")?;
    let (head_off, head_len) = find_table(font, b"head")?;
    if head_len < 12 {
        return None;
    }
    let mut out = font.to_vec();
    let name = &mut out[name_off..name_off + name_len];
    let mut i = 0usize;
    while i < name.len() {
        if name[i] == b'+' && i >= 6 && name[i - 6..i].iter().all(u8::is_ascii_uppercase) {
            name[i - 6..i].fill(b'A');
        } else if name[i] == b'+' && i >= 13 && name[i - 1] == 0 {
            // UTF-16BE: 00 X 00 X ... 00 '+'
            let tag = &name[i - 13..i - 1];
            if tag
                .as_chunks::<2>()
                .0
                .iter()
                .all(|p| p[0] == 0 && p[1].is_ascii_uppercase())
            {
                for pair in name[i - 13..i - 1].as_chunks_mut::<2>().0 {
                    pair[1] = b'A';
                }
            }
        }
        i += 1;
    }
    if out[name_off..name_off + name_len] == font[name_off..name_off + name_len] {
        return Some(out); // nothing masked; checksums still valid
    }

    // Repair the `name` directory checksum, then `head.checkSumAdjustment`.
    let count = usize::from(be16(&out, 4)?);
    for i in 0..count {
        let rec = 12 + i * 16;
        if out.get(rec..rec + 4)? == b"name" {
            // table_checksum zero-pads the short final chunk, matching the
            // font's own 4-byte table padding.
            let sum = table_checksum(&out[name_off..name_off + name_len]);
            out[rec + 4..rec + 8].copy_from_slice(&sum.to_be_bytes());
        }
    }
    out[head_off + 8..head_off + 12].fill(0);
    let adjustment = 0xB1B0_AFBAu32.wrapping_sub(table_checksum(&out));
    out[head_off + 8..head_off + 12].copy_from_slice(&adjustment.to_be_bytes());
    Some(out)
}

/// An sfnt table: (tag, body).
type SfntTable = ([u8; 4], Vec<u8>);

/// Read every top-level table out of a single (non-collection) sfnt.
fn parse_tables(font: &[u8]) -> Option<(u32, Vec<SfntTable>)> {
    let sfnt_version = be32(font, 0)?;
    let count = usize::from(be16(font, 4)?);
    let mut tables: Vec<SfntTable> = Vec::with_capacity(count + 1);
    for i in 0..count {
        let rec = 12 + i * 16;
        let tag: [u8; 4] = font.get(rec..rec + 4)?.try_into().ok()?;
        let off = be32(font, rec + 8)? as usize;
        let len = be32(font, rec + 12)? as usize;
        tables.push((tag, font.get(off..off.checked_add(len)?)?.to_vec()));
    }
    Some((sfnt_version, tables))
}

/// Splice `subtables` into `font` as its `cmap`, rebuilding the table
/// directory, per-table checksums, and `head.checkSumAdjustment`. Replaces
/// any existing `cmap`. `None` on any structural doubt about the input.
pub(crate) fn insert_cmap(font: &[u8], subtables: &[CmapSubtable]) -> Option<Vec<u8>> {
    let (sfnt_version, mut tables) = parse_tables(font)?;
    tables.retain(|(tag, _)| tag != b"cmap");
    tables.push((*b"cmap", build_cmap_table(subtables)?));
    tables.sort_by_key(|(tag, _)| *tag);
    assemble_sfnt(sfnt_version, tables)
}

fn assemble_sfnt(sfnt_version: u32, mut tables: Vec<SfntTable>) -> Option<Vec<u8>> {
    let num = tables.len();
    let floor_log2 = usize::BITS - 1 - num.leading_zeros();
    let search_range = 16u16 << floor_log2;
    let mut out = Vec::new();
    out.extend_from_slice(&sfnt_version.to_be_bytes());
    out.extend_from_slice(&(num as u16).to_be_bytes());
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&(floor_log2 as u16).to_be_bytes());
    out.extend_from_slice(&(num as u16 * 16 - search_range).to_be_bytes());

    let mut offset = 12 + num * 16;
    let mut head_offset = None;
    for (tag, data) in &mut tables {
        if tag == b"head" {
            // checkSumAdjustment participates in neither the table checksum
            // nor (by construction, once zeroed) the whole-font sum.
            if data.len() < 12 {
                return None;
            }
            data[8..12].fill(0);
            head_offset = Some(offset);
        }
        out.extend_from_slice(tag);
        out.extend_from_slice(&table_checksum(data).to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        offset += data.len().next_multiple_of(4);
    }
    for (_, data) in &tables {
        out.extend_from_slice(data);
        out.resize(out.len().next_multiple_of(4), 0);
    }

    let head_offset = head_offset?;
    let adjustment = 0xB1B0_AFBAu32.wrapping_sub(table_checksum(&out));
    out[head_offset + 8..head_offset + 12].copy_from_slice(&adjustment.to_be_bytes());
    Some(out)
}

// ---------------------------------------------------------------------------
// Hinting removal (opt-in; changes rasterization at small sizes)
// ---------------------------------------------------------------------------

/// Strip TrueType hinting: drop the `fpgm`/`prep`/`cvt ` tables and every
/// per-glyph instruction block, rebuilding `glyf`, `loca`, the directory
/// checksums, and `head.checkSumAdjustment`.
///
/// Outlines, metrics, and mappings are untouched, but rasterization at small
/// sizes can change — callers must gate this behind explicit consent.
/// Returns `None` on structural doubt *and* when there is nothing to strip
/// (so the caller keeps the original bytes in both cases, fail-safe).
pub(crate) fn strip_hinting(font: &[u8]) -> Option<Vec<u8>> {
    let (sfnt_version, mut tables) = parse_tables(font)?;
    if sfnt_version != 0x0001_0000 && sfnt_version != u32::from_be_bytes(*b"true") {
        return None; // only glyf-flavoured sfnts carry TrueType hinting
    }
    let get = |t: &[u8; 4]| tables.iter().position(|(tag, _)| tag == t);
    let had_programs = get(b"fpgm").is_some() || get(b"prep").is_some() || get(b"cvt ").is_some();

    let head_idx = get(b"head")?;
    let loca_idx = get(b"loca")?;
    let glyf_idx = get(b"glyf")?;
    let maxp_idx = get(b"maxp")?;
    let long_loca = match be16(&tables[head_idx].1, 50)? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let num_glyphs = usize::from(be16(&tables[maxp_idx].1, 4)?);

    // Decode loca into byte offsets.
    let loca = &tables[loca_idx].1;
    let mut offsets = Vec::with_capacity(num_glyphs + 1);
    for i in 0..=num_glyphs {
        offsets.push(if long_loca {
            be32(loca, i * 4)? as usize
        } else {
            usize::from(be16(loca, i * 2)?) * 2
        });
    }

    // Rewrite each glyph without its instruction block.
    let glyf = &tables[glyf_idx].1;
    let mut new_glyf: Vec<u8> = Vec::with_capacity(glyf.len());
    let mut new_offsets = Vec::with_capacity(num_glyphs + 1);
    let mut stripped_any = false;
    for w in offsets.windows(2) {
        new_offsets.push(new_glyf.len());
        let (start, end) = (w[0], w[1]);
        if start > end {
            return None;
        }
        let glyph = glyf.get(start..end)?;
        if glyph.is_empty() {
            continue; // empty glyph stays empty
        }
        let before = new_glyf.len();
        strip_glyph_instructions(glyph, &mut new_glyf)?;
        stripped_any |= new_glyf.len() - before != glyph.len();
        // Both loca formats conventionally align glyphs; short loca requires
        // even offsets.
        new_glyf.resize(new_glyf.len().next_multiple_of(2), 0);
    }
    new_offsets.push(new_glyf.len());
    if !had_programs && !stripped_any {
        return None; // nothing to strip; keep the original bytes
    }

    // Re-encode loca in the original format (offsets only shrank, so a short
    // loca stays representable; verify anyway).
    let mut new_loca = Vec::with_capacity(new_offsets.len() * if long_loca { 4 } else { 2 });
    for &off in &new_offsets {
        if long_loca {
            new_loca.extend_from_slice(&u32::try_from(off).ok()?.to_be_bytes());
        } else {
            new_loca.extend_from_slice(&u16::try_from(off / 2).ok()?.to_be_bytes());
        }
    }
    tables[glyf_idx].1 = new_glyf;
    tables[loca_idx].1 = new_loca;
    // maxp instruction maxima are upper bounds; zero them now that no
    // instructions remain (version 1.0 layout only).
    let maxp = &mut tables[maxp_idx].1;
    if be32(maxp, 0)? == 0x0001_0000 && maxp.len() >= 32 {
        maxp[24..26].fill(0); // maxSizeOfInstructions
    }
    tables.retain(|(tag, _)| tag != b"fpgm" && tag != b"prep" && tag != b"cvt ");
    assemble_sfnt(sfnt_version, tables)
}

/// Append `glyph` to `out` with its instruction block removed. Simple glyphs
/// get `instructionLength = 0`; composite glyphs get WE_HAVE_INSTRUCTIONS
/// cleared and the trailing block dropped. `None` on any structural doubt.
fn strip_glyph_instructions(glyph: &[u8], out: &mut Vec<u8>) -> Option<()> {
    let contours = i16::from_be_bytes([*glyph.first()?, *glyph.get(1)?]);
    if contours >= 0 {
        // Simple glyph: header (10) + endPtsOfContours + instructions + rest.
        let end_pts = 10 + usize::from(contours as u16) * 2;
        let instr_len = usize::from(be16(glyph, end_pts)?);
        let rest = glyph.get(end_pts + 2 + instr_len..)?;
        out.extend_from_slice(glyph.get(..end_pts)?);
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(rest);
        return Some(());
    }
    if contours != -1 {
        return None;
    }
    // Composite glyph: walk the component records.
    const ARG_1_AND_2_ARE_WORDS: u16 = 0x0001;
    const WE_HAVE_A_SCALE: u16 = 0x0008;
    const MORE_COMPONENTS: u16 = 0x0020;
    const WE_HAVE_AN_X_AND_Y_SCALE: u16 = 0x0040;
    const WE_HAVE_A_TWO_BY_TWO: u16 = 0x0080;
    const WE_HAVE_INSTRUCTIONS: u16 = 0x0100;

    let base = out.len();
    out.extend_from_slice(glyph.get(..10)?);
    let mut pos = 10usize;
    let mut have_instructions = false;
    loop {
        let flags = be16(glyph, pos)?;
        have_instructions |= flags & WE_HAVE_INSTRUCTIONS != 0;
        let mut len = 4 + if flags & ARG_1_AND_2_ARE_WORDS != 0 {
            4
        } else {
            2
        };
        if flags & WE_HAVE_A_SCALE != 0 {
            len += 2;
        } else if flags & WE_HAVE_AN_X_AND_Y_SCALE != 0 {
            len += 4;
        } else if flags & WE_HAVE_A_TWO_BY_TWO != 0 {
            len += 8;
        }
        let record_start = out.len();
        out.extend_from_slice(glyph.get(pos..pos + len)?);
        // Clear the instructions flag in the copied record.
        let cleared = (flags & !WE_HAVE_INSTRUCTIONS).to_be_bytes();
        out[record_start..record_start + 2].copy_from_slice(&cleared);
        pos += len;
        if flags & MORE_COMPONENTS == 0 {
            break;
        }
    }
    if have_instructions {
        // Trailing instruction block: length + bytes; validate it fits.
        let instr_len = usize::from(be16(glyph, pos)?);
        glyph.get(pos + 2..pos + 2 + instr_len)?;
    } else if pos != glyph.len() {
        // Unexpected trailing bytes on a glyph that declared no instructions;
        // keep them verbatim rather than guess.
        out.truncate(base);
        out.extend_from_slice(glyph);
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_map(pairs: &[(u32, u16)]) -> BTreeMap<u32, u16> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn format4_roundtrips_through_parser() {
        let map = sample_map(&[(0x20, 1), (0x21, 2), (0x22, 3), (0x41, 9), (0x2022, 4)]);
        let body = build_format4(&map).unwrap();
        assert_eq!(parse_subtable(&body).unwrap(), map);
    }

    #[test]
    fn format6_roundtrips_through_parser() {
        let map = sample_map(&[(0x20, 5), (0x7E, 2), (0xCA, 7)]);
        let body = build_format6(&map);
        assert_eq!(parse_subtable(&body).unwrap(), map);
    }

    /// Minimal two-table sfnt (`head`, `name`) whose `name` body is `body`.
    fn tiny_font(body: &[u8]) -> Vec<u8> {
        let head = vec![0u8; 54];
        let tables: [(&[u8; 4], &[u8]); 2] = [(b"head", &head), (b"name", body)];
        let mut out = vec![0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0, 32, 0, 1, 0, 0];
        let mut offset = 12 + 32;
        for (tag, data) in tables {
            out.extend_from_slice(tag);
            out.extend_from_slice(&table_checksum(data).to_be_bytes());
            out.extend_from_slice(&(offset as u32).to_be_bytes());
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            offset += data.len().next_multiple_of(4);
        }
        for (_, data) in tables {
            out.extend_from_slice(data);
            out.resize(out.len().next_multiple_of(4), 0);
        }
        out
    }

    #[test]
    fn masking_makes_same_subset_under_different_tags_byte_equal() {
        let a = tiny_font(b"SKOJEB+ArialMT\x00\x00");
        let b = tiny_font(b"RZEDQD+ArialMT\x00\x00");
        assert_ne!(a, b);
        let (ma, mb) = (mask_subset_tags(&a).unwrap(), mask_subset_tags(&b).unwrap());
        assert_eq!(ma, mb);
        assert!(contains_window(&ma, b"AAAAAA+ArialMT"));
        // Whole-font checksum invariant holds after the repair.
        let head = find_table(&ma, b"head").unwrap().0;
        let mut zeroed = ma.clone();
        let adj = u32::from_be_bytes(ma[head + 8..head + 12].try_into().unwrap());
        zeroed[head + 8..head + 12].fill(0);
        assert_eq!(adj, 0xB1B0_AFBAu32.wrapping_sub(table_checksum(&zeroed)));
    }

    #[test]
    fn masking_handles_utf16be_names_and_leaves_untagged_fonts_alone() {
        let mut utf16 = Vec::new();
        for ch in "SKOJEB+Arial".chars() {
            utf16.extend_from_slice(&[0, ch as u8]);
        }
        let masked = mask_subset_tags(&tiny_font(&utf16)).unwrap();
        assert!(contains_window(
            &masked,
            b"\x00A\x00A\x00A\x00A\x00A\x00A\x00+"
        ));

        let plain = tiny_font(b"ArialMT\x00");
        assert_eq!(mask_subset_tags(&plain).unwrap(), plain);
    }

    fn contains_window(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn simple_glyph_instructions_are_removed() {
        // 1 contour, 2 points, 3 instruction bytes, then flags/coords.
        let mut g = Vec::new();
        g.extend_from_slice(&1i16.to_be_bytes());
        g.extend_from_slice(&[0u8; 8]); // bbox
        g.extend_from_slice(&1u16.to_be_bytes()); // endPts
        g.extend_from_slice(&3u16.to_be_bytes()); // instructionLength
        g.extend_from_slice(&[0xB0, 0x01, 0x21]); // instructions
        g.extend_from_slice(&[0x01, 0x01, 5, 5, 5, 5]); // flags + coords
        let mut out = Vec::new();
        strip_glyph_instructions(&g, &mut out).unwrap();
        assert_eq!(out.len(), g.len() - 3);
        assert_eq!(&out[10 + 2..10 + 4], &0u16.to_be_bytes());
        assert_eq!(&out[out.len() - 6..], &[0x01, 0x01, 5, 5, 5, 5]);
    }

    #[test]
    fn composite_glyph_instruction_flag_is_cleared() {
        let mut g = Vec::new();
        g.extend_from_slice(&(-1i16).to_be_bytes());
        g.extend_from_slice(&[0u8; 8]); // bbox
        g.extend_from_slice(&0x0101u16.to_be_bytes()); // WORDS | INSTRUCTIONS
        g.extend_from_slice(&7u16.to_be_bytes()); // glyphIndex
        g.extend_from_slice(&[0u8; 4]); // word args
        g.extend_from_slice(&2u16.to_be_bytes()); // instr length
        g.extend_from_slice(&[0xB0, 0x00]);
        let mut out = Vec::new();
        strip_glyph_instructions(&g, &mut out).unwrap();
        assert_eq!(out.len(), g.len() - 4);
        assert_eq!(&out[10..12], &0x0001u16.to_be_bytes());
    }

    #[test]
    fn empty_map_still_builds_valid_tables() {
        let map = BTreeMap::new();
        assert_eq!(parse_subtable(&build_format4(&map).unwrap()).unwrap(), map);
        assert_eq!(parse_subtable(&build_format6(&map)).unwrap(), map);
    }
}
