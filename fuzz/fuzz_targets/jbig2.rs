#![no_main]

use libfuzzer_sys::fuzz_target;

// Two modes on the low bit of the leading byte:
// - decode: bytes 1-2 give a small declared geometry (1..=256 each), the
//   rest is fed to the JBIG2 decode-back gate as an embedded stream. It
//   must stay bounded and, when it decodes, return exactly that geometry.
// - round trip: bytes 1-3 pick the width (1..=256), the template/TPGD
//   selector and an AT pixel; the rest is the bitmap. Encode -> decode must
//   reproduce it whenever the parameters are valid.
fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let (head, rest) = data.split_at(4);
    if head[0] & 1 == 0 {
        let (w, h) = (u32::from(head[1]) + 1, u32::from(head[2]) + 1);
        if let Some(out) = picamatl::fuzz_api::jbig2_decode(rest, w, h) {
            assert_eq!(out.len(), (w as usize).div_ceil(8) * h as usize);
        }
    } else {
        let width = usize::from(head[1]) + 1;
        let sel = head[2];
        let at = ((head[3] & 0x0F) as i8 - 8, -(((head[3] >> 4) & 7) as i8));
        if let Some(ok) = picamatl::fuzz_api::jbig2_round_trip(width, rest, sel, at) {
            assert!(ok, "JBIG2 round trip failed");
        }
    }
});
