//! Diagnostic: decode JP2/J2C assets with dicom-toolkit-jpeg2000 and write
//! PNGs for visual verification. Scratch tool for the JPX conversion work.
use dicom_toolkit_jpeg2000::{DecodeSettings, Image};

fn main() {
    for path in std::env::args().skip(1) {
        let data = std::fs::read(&path).unwrap();
        let img = match Image::new(&data, &DecodeSettings::default()) {
            Ok(i) => i,
            Err(e) => {
                println!("{path}: header error {e:?}");
                continue;
            }
        };
        println!(
            "{path}: {}x{} depth={} cs={:?} alpha={}",
            img.width(),
            img.height(),
            img.original_bit_depth(),
            img.color_space(),
            img.has_alpha()
        );
        match img.decode() {
            Ok(pixels) => {
                let ncomp = pixels.len() as u32 / (img.width() * img.height());
                println!("  decoded {} bytes, {} comps", pixels.len(), ncomp);
                let color = match ncomp {
                    1 => image::ColorType::L8,
                    3 => image::ColorType::Rgb8,
                    4 => image::ColorType::Rgba8,
                    _ => {
                        println!("  unsupported comp count");
                        continue;
                    }
                };
                let out = format!("{path}.decoded.png");
                image::save_buffer(&out, &pixels, img.width(), img.height(), color).unwrap();
                println!("  wrote {out}");
            }
            Err(e) => println!("  decode error {e:?}"),
        }
    }
}
