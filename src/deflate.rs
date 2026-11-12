//! Deflate/inflate and PNG row-filter primitives.
//!
//! The zlib container every `FlateDecode` stream uses, the capped inflate that
//! is the crate's decompression-bomb guard, and the PNG predictor codec that
//! `/DecodeParms /Predictor >= 10` streams need. Shared by every path that
//! touches a Flate stream; no policy lives here.

use crate::DeflateBackend;

/// Inflate a zlib stream with a hard output cap. Returns `None` for corrupt
/// or truncated input (unlike lopdf, which logs and returns partial data as
/// `Ok`) and for streams that would produce more than `limit` bytes — the
/// actual decompression-bomb guard, enforced *during* inflation.
pub(crate) fn inflate_capped(data: &[u8], limit: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    // Read one byte past the cap so an over-limit stream is distinguishable
    // from one that is exactly at it.
    let mut dec = flate2::read::ZlibDecoder::new(data).take(limit as u64 + 1);
    dec.read_to_end(&mut out).ok()?;
    if out.len() > limit {
        return None;
    }
    Some(out)
}

/// PNG Paeth predictor (PNG spec §9.4).
pub(crate) fn paeth_predict(left: u8, up: u8, upper_left: u8) -> u8 {
    let (a, b, c) = (i16::from(left), i16::from(up), i16::from(upper_left));
    let p = a + b - c;
    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
    if pa <= pb && pa <= pc {
        left
    } else if pb <= pc {
        up
    } else {
        upper_left
    }
}

/// Spec-correct inversion of PNG row filters over a whole frame of
/// `[tag][filtered row]` records. Hand-rolled rather than lopdf's
/// `filters::png::decode_frame` because lopdf up to 0.44 mis-computed the Avg
/// filter (`left + up/2` instead of `(left + up)/2`, verified by round-trip
/// test) — silently wrong pixels with the correct length, which the outer
/// length check cannot catch. lopdf 0.45 fixed it. Returns `None` on any
/// malformed shape.
pub(crate) fn png_defilter(
    data: &[u8],
    bytes_per_pixel: usize,
    bytes_per_row: usize,
) -> Option<Vec<u8>> {
    let stride = bytes_per_row.checked_add(1)?;
    if bytes_per_row == 0 || !data.len().is_multiple_of(stride) {
        return None;
    }
    let rows = data.len() / stride;
    let mut out = vec![0u8; rows * bytes_per_row];
    for r in 0..rows {
        let src = &data[r * stride..(r + 1) * stride];
        let tag = src[0];
        let (done, rest) = out.split_at_mut(r * bytes_per_row);
        let prev = &done[done.len().saturating_sub(bytes_per_row)..];
        let cur = &mut rest[..bytes_per_row];
        cur.copy_from_slice(&src[1..]);
        let bpp = bytes_per_pixel;
        let up_at = |i: usize| if r == 0 { 0 } else { prev[i] };
        match tag {
            0 => {}
            1 => {
                for i in bpp..bytes_per_row {
                    cur[i] = cur[i].wrapping_add(cur[i - bpp]);
                }
            }
            2 => {
                for (i, c) in cur.iter_mut().enumerate() {
                    *c = c.wrapping_add(up_at(i));
                }
            }
            3 => {
                for i in 0..bytes_per_row {
                    let left = if i >= bpp { u16::from(cur[i - bpp]) } else { 0 };
                    let up = u16::from(up_at(i));
                    cur[i] = cur[i].wrapping_add(((left + up) / 2) as u8);
                }
            }
            4 => {
                for i in 0..bytes_per_row {
                    let left = if i >= bpp { cur[i - bpp] } else { 0 };
                    let up = up_at(i);
                    let upper_left = if i >= bpp { up_at(i - bpp) } else { 0 };
                    cur[i] = cur[i].wrapping_add(paeth_predict(left, up, upper_left));
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

/// PNG filter-type tag: Up (PNG spec §9.2). The tag the xref-stream predictor
/// uses; the image paths try all five (see [`PNG_FILTERS`]).
pub(crate) const PNG_UP: u8 = 2;

/// Every PNG filter type (None, Sub, Up, Average, Paeth), in tag order.
pub(crate) const PNG_FILTERS: [u8; 5] = [0, 1, 2, 3, 4];

/// Apply ONE PNG filter type to every row, producing the predictor-encoded
/// byte stream FlateDecode + `/Predictor >= 10` expects: each row is a
/// leading filter-type byte (`tag`, 0-4) followed by the filtered row bytes.
/// Spec-correct forward filtering (PNG spec §9) against the UNFILTERED prior
/// row; [`png_defilter`] is its exact inverse for every tag. Hand-rolled for
/// the same reason the inverse is: lopdf's `encode_row` mis-computed Avg up
/// to 0.44 (`(left wrapping+ up)/2` overflows; 0.45 sums in `u16`).
///
/// `bytes_per_pixel` is the predictor's left distance (the PDF `/Colors`
/// value for 8-bit samples). A trailing partial row is dropped, and a zero
/// row width or an unknown tag yields an empty output — callers check the
/// length (`rows * (bytes_per_row + 1)`) before using it.
pub(crate) fn png_filter(
    raw: &[u8],
    bytes_per_row: usize,
    bytes_per_pixel: usize,
    tag: u8,
) -> Vec<u8> {
    if bytes_per_row == 0 || tag > 4 {
        // No pixel bytes per row: `chunks_exact(0)` would panic, and there is
        // nothing to filter.
        return Vec::new();
    }
    let bpp = bytes_per_pixel;
    let rows = raw.len() / bytes_per_row;
    let mut out = Vec::with_capacity(raw.len() + rows);
    let zero = vec![0u8; bytes_per_row];
    let mut previous: &[u8] = &zero;
    for row in raw.chunks_exact(bytes_per_row) {
        out.push(tag);
        match tag {
            0 => out.extend_from_slice(row),
            1 => out.extend(
                row.iter()
                    .enumerate()
                    .map(|(i, &x)| x.wrapping_sub(if i >= bpp { row[i - bpp] } else { 0 })),
            ),
            2 => out.extend(row.iter().zip(previous).map(|(&x, &up)| x.wrapping_sub(up))),
            3 => out.extend(row.iter().enumerate().map(|(i, &x)| {
                let left = if i >= bpp { u16::from(row[i - bpp]) } else { 0 };
                x.wrapping_sub(((left + u16::from(previous[i])) / 2) as u8)
            })),
            _ => out.extend(row.iter().enumerate().map(|(i, &x)| {
                let (left, upper_left) = if i >= bpp {
                    (row[i - bpp], previous[i - bpp])
                } else {
                    (0, 0)
                };
                x.wrapping_sub(paeth_predict(left, previous[i], upper_left))
            })),
        }
        previous = row;
    }
    out
}

/// Deflate (zlib container, as FlateDecode requires) at maximum compression.
pub(crate) fn deflate_level9(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(9));
    enc.write_all(data).ok()?;
    enc.finish().ok()
}

/// Deflate with the exhaustive zopfli search (default 15 iterations). Same
/// zlib container as [`deflate_level9`] — any inflate reads it — just a more
/// expensive hunt for a smaller encoding.
pub(crate) fn deflate_zopfli(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    zopfli::compress(
        zopfli::Options::default(),
        zopfli::Format::Zlib,
        data,
        &mut out,
    )
    .ok()?;
    Some(out)
}

/// Dispatch to the configured deflate backend (final-pass call sites only —
/// planning-time deflates always use [`deflate_level9`] for speed, and the
/// final pass revisits their output anyway).
pub(crate) fn deflate_backend(data: &[u8], backend: DeflateBackend) -> Option<Vec<u8>> {
    match backend {
        DeflateBackend::Zlib => deflate_level9(data),
        DeflateBackend::Zopfli => deflate_zopfli(data),
    }
}
