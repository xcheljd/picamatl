#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (h, rest) = match data.split_first() { Some(x) => x, None => return };
    picamatl::fuzz_api::cffhint(rest, h & 1 == 1);
    picamatl::fuzz_api::cffmerge(data);
});
