//! Synthetic seeds for the jbig2 target (no PDF holds picamatl's own small
//! JBIG2 shape): text-like, noise, blank and striped bitmaps up to 256 px
//! wide, as round-trip inputs and as encoded streams for the decode mode.
use std::path::Path;

fn main() {
    let out = std::env::args().nth(1).expect("output dir");
    let dir = Path::new(&out);
    std::fs::create_dir_all(dir).unwrap();
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut rnd = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut n = 0;
    for &(w, h) in &[(1usize, 1usize), (7, 3), (8, 8), (33, 17), (64, 40), (120, 96), (256, 64), (200, 1), (1, 200)] {
        let stride = w.div_ceil(8);
        for kind in 0..4 {
            let mut bm = vec![0u8; stride * h];
            for y in 0..h {
                for x in 0..w {
                    let on = match kind {
                        0 => false,
                        1 => rnd() & 1 == 1,
                        2 => (y / 6) % 2 == 0 && x % 9 < 4,
                        _ => (rnd() % 100) < 12 && y % 11 < 7,
                    };
                    if on {
                        bm[y * stride + x / 8] |= 0x80 >> (x % 8);
                    }
                }
            }
            for sel in [0u8, 1, 2, 3, 4, 5, 6, 7] {
                let at = if sel & 3 == 0 { (3i8, -1i8) } else if sel & 3 == 1 { (3, -1) } else { (2, -1) };
                // round-trip mode: [odd, width-1, sel, at-nibbles]
                let atb = (((at.0 + 8) as u8) & 0x0F) | ((((-at.1) as u8) & 7) << 4);
                let mut rt = vec![1u8, (w - 1) as u8, sel, atb];
                rt.extend_from_slice(&bm);
                std::fs::write(dir.join(format!("rt-{n}")), &rt).unwrap();
                if h <= 256 {
                    if let Some((s, hh)) = picamatl::fuzz_api::jbig2_encode(w, &bm, sel, at) {
                        let mut d = vec![0u8, (w - 1) as u8, (hh - 1) as u8, 0];
                        d.extend_from_slice(&s);
                        std::fs::write(dir.join(format!("dec-{n}")), &d).unwrap();
                    }
                }
                n += 1;
            }
        }
    }
    println!("jbig2 seeds: {n} round-trip + decode inputs");
}
