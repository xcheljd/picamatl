#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (h, rest) = match data.split_first() { Some(x) => x, None => return };
    picamatl::fuzz_api::inflate(rest, 1 << 22);
    let bpr = (*h as usize % 64) + 1;
    picamatl::fuzz_api::png_defilter(rest, 1 + (*h as usize >> 6), bpr);
});
