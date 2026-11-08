#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (flag, rest) = match data.split_first() { Some(x) => x, None => return };
    picamatl::fuzz_api::type1(rest, flag & 1 == 1);
});
