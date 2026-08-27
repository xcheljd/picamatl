//! Fixture tool: rewrite a plain-CMYK (Adobe APP14 transform = 0) JPEG as a
//! YCCK (transform = 2) JPEG carrying the same raw stored samples.
//!
//!     cargo run --release --example gen_cmyk_ycck -- \
//!         fixtures/jpeg/cmyk_plain.jpg fixtures/jpeg/cmyk_ycck.jpg
//!
//! No YCCK encoder is available in this tree's Python/ImageMagick tooling, so
//! this generates the transform = 2 fixture. It deliberately calls libjpeg
//! directly rather than reusing the library's own private helpers, and the
//! result is validated against `djpeg` and Pillow (see docs/CMYK-JPEG.md) —
//! the fixture is therefore not simply "whatever picamatl happens to emit".
use mozjpeg::{ColorSpace, Compress, Decompress};

fn main() {
    let mut args = std::env::args_os().skip(1);
    let (src, dst) = (
        args.next()
            .expect("usage: gen_cmyk_ycck <in.jpg> <out.jpg>"),
        args.next()
            .expect("usage: gen_cmyk_ycck <in.jpg> <out.jpg>"),
    );
    let data = std::fs::read(&src).expect("read input");

    let dec = Decompress::new_mem(&data).expect("jpeg header");
    assert_eq!(
        dec.color_space(),
        ColorSpace::JCS_CMYK,
        "input must be a plain-CMYK (transform = 0) JPEG"
    );
    let (w, h) = (dec.width(), dec.height());
    let mut started = dec
        .to_colorspace(ColorSpace::JCS_CMYK)
        .expect("start decompress");
    let pixels: Vec<u8> = started.read_scanlines().expect("scanlines");
    started.finish().expect("finish decompress");

    let mut comp = Compress::new(ColorSpace::JCS_CMYK);
    comp.set_size(w, h);
    comp.set_color_space(ColorSpace::JCS_YCCK);
    comp.set_quality(92.0);
    let mut started = comp.start_compress(Vec::new()).expect("start compress");
    started.write_scanlines(&pixels).expect("write scanlines");
    let out = started.finish().expect("finish compress");

    std::fs::write(&dst, &out).expect("write output");
    println!(
        "{} -> {} ({} bytes, {}x{})",
        src.to_string_lossy(),
        dst.to_string_lossy(),
        out.len(),
        w,
        h
    );
}
