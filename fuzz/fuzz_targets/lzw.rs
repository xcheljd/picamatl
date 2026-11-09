#![no_main]

use libfuzzer_sys::fuzz_target;

// Leading byte selects the decoder (low 3 bits: 0/1 = LZW EarlyChange 0/1,
// 2 = ASCII85, 3 = ASCIIHex, else RunLength); the rest is the payload. The
// output cap is small so bombs are exercised, and it must hold.
fuzz_target!(|data: &[u8]| {
    let Some((h, rest)) = data.split_first() else { return };
    const LIMIT: usize = 1 << 22;
    if let Some(out) = picamatl::fuzz_api::legacy_decode(h & 7, rest, LIMIT) {
        assert!(out.len() <= LIMIT);
    }
});
