#![no_main]
//! Every opt-in pass on: the deepest reach per input (forms flattening,
//! Type1 conversion, hint stripping, lossy re-encodes, bitonal, gray
//! collapse). Same contract as `optimize`: never larger, and any rewrite
//! must load.

use libfuzzer_sys::fuzz_target;
use picamatl::{optimize_with_options, OptimizeOptions};

fuzz_target!(|data: &[u8]| {
    let opts = OptimizeOptions::default()
        .with_allow_lossy_reencode(true)
        .with_strip_accessibility(true)
        .with_strip_metadata(true)
        .with_strip_private_data(true)
        .with_convert_type1(true)
        .with_strip_hinting(true)
        .with_recompress_bitonal_images(true)
        .with_collapse_gray_images(true)
        .with_flatten_forms(true);
    let out = optimize_with_options(data, opts);
    assert!(out.len() <= data.len(), "output larger than input");
    if out != data {
        lopdf::Document::load_mem(&out).expect("rewritten output must load");
    }
});
