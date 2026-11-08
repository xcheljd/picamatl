#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let out = picamatl::optimize(data);
    assert!(out.len() <= data.len(), "output larger than input");
    if out != data {
        lopdf::Document::load_mem(&out).expect("rewritten output must load");
    }
});
