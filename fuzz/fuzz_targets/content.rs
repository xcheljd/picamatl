#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    picamatl::fuzz_api::number_tokens(data);
    picamatl::fuzz_api::replan(data);
});
