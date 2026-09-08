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
/// `filters::png::decode_frame` because lopdf 0.42 mis-computes the Avg
/// filter (`left + up/2` instead of `(left + up)/2`, verified by round-trip
/// test) — silently wrong pixels with the correct length, which the outer
/// length check cannot catch. Returns `None` on any malformed shape.
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

/// Apply the PNG Up filter to every row, producing the predictor-encoded byte
/// stream FlateDecode + `/Predictor >= 10` expects: each row is a leading
/// filter-type byte (2 = Up) followed by the filtered row bytes.
pub(crate) fn png_up_filter(raw: &[u8], width: u32, channels: usize) -> Vec<u8> {
    use lopdf::filters::png;

    let bytes_per_row = width as usize * channels;
    let rows = raw.len() / bytes_per_row.max(1);
    let mut out = Vec::with_capacity(raw.len() + rows);
    let mut previous = vec![0u8; bytes_per_row];
    for row in raw.chunks_exact(bytes_per_row) {
        let mut current = row.to_vec();
        // `previous` must be the UNFILTERED prior row (encode_row subtracts it).
        png::encode_row(png::FilterType::Up, channels, &previous, &mut current);
        out.push(2); // PNG filter-type tag: Up
        out.extend_from_slice(&current);
        previous.copy_from_slice(row);
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
