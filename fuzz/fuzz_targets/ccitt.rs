#![no_main]

use libfuzzer_sys::fuzz_target;

// The in-house strict G4 decoder (src/ccitt.rs). A 5-byte header: mode,
// then width and height as little-endian u16s (each 1..=4096). Modes on
// `head[0] % 3`:
// - 0, differential: the rest as a continuous (non-aligned) G4 stream.
//   Whenever the strict decoder accepts it, the fax-based strict decoder
//   the non-aligned bitonal path uses must accept it with the same pixels
//   (the strict decoder may reject what fax tolerates, never the reverse).
// - 1, byte-aligned: the rest as an /EncodedByteAlign stream. Must stay
//   bounded and, when it decodes, return exactly the declared geometry.
// - 2, round trip: the rest as packed rows of a `width`-pixel bitmap
//   (padding bits cleared). Byte-aligned encode, then decode, must
//   reproduce it.
fuzz_target!(|data: &[u8]| {
    if data.len() < 5 {
        return;
    }
    let (head, rest) = data.split_at(5);
    let dim = |lo: u8, hi: u8| 1 + u16::from_le_bytes([lo, hi]) % 4096;
    let w = u32::from(dim(head[1], head[2]));
    let h = usize::from(dim(head[3], head[4]));
    let stride = (w as usize).div_ceil(8);
    match head[0] % 3 {
        0 => {
            if let Some(new) = picamatl::fuzz_api::ccitt_g4_decode(rest, w, h, false) {
                assert_eq!(new.len(), stride * h);
                let fax = picamatl::fuzz_api::fax_g4_decode(rest, w, h);
                assert_eq!(fax.as_ref(), Some(&new), "strict decoder disagrees with fax");
            }
        }
        1 => {
            if let Some(out) = picamatl::fuzz_api::ccitt_g4_decode(rest, w, h, true) {
                assert_eq!(out.len(), stride * h);
            }
        }
        _ => {
            let rows = rest.len() / stride;
            if rows == 0 {
                return;
            }
            let mut bm = rest[..stride * rows].to_vec();
            if w % 8 != 0 {
                let keep = 0xFFu8 << (8 - w % 8);
                for r in 0..rows {
                    bm[r * stride + stride - 1] &= keep;
                }
            }
            let enc = picamatl::fuzz_api::ccitt_g4_encode_aligned(&bm, w, rows);
            let dec = picamatl::fuzz_api::ccitt_g4_decode(&enc, w, rows, true);
            assert_eq!(dec.as_ref(), Some(&bm), "byte-aligned G4 round trip failed");
        }
    }
});
