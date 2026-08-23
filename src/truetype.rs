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

/// Splice `subtables` into `font` as its `cmap`, rebuilding the table
/// directory, per-table checksums, and `head.checkSumAdjustment`. Replaces
/// any existing `cmap`. `None` on any structural doubt about the input.
pub(crate) fn insert_cmap(font: &[u8], subtables: &[CmapSubtable]) -> Option<Vec<u8>> {
    let sfnt_version = be32(font, 0)?;
    let count = usize::from(be16(font, 4)?);
    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::with_capacity(count + 1);
    for i in 0..count {
        let rec = 12 + i * 16;
        let tag: [u8; 4] = font.get(rec..rec + 4)?.try_into().ok()?;
        let off = be32(font, rec + 8)? as usize;
        let len = be32(font, rec + 12)? as usize;
        if &tag == b"cmap" {
            continue;
        }
        tables.push((tag, font.get(off..off.checked_add(len)?)?.to_vec()));
    }
    tables.push((*b"cmap", build_cmap_table(subtables)?));
    tables.sort_by_key(|(tag, _)| *tag);

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

    #[test]
    fn empty_map_still_builds_valid_tables() {
        let map = BTreeMap::new();
        assert_eq!(parse_subtable(&build_format4(&map).unwrap()).unwrap(), map);
        assert_eq!(parse_subtable(&build_format6(&map)).unwrap(), map);
    }
}
