// Builds the tray icon masks from the wordmark SVG. The tray embeds raw
// pixels (see assets/README.md), so the rasterization happens once here
// instead of shipping a runtime image decoder. The output is one 8-bit
// coverage mask per glyph, because the tray colours and rotates the U and
// the N at runtime.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Mask width and height in pixels. Panels draw the badge at 22–48 logical
/// pixels and scale it down, so a square 128 leaves room for a crisp panel.
const SIZE: u32 = 128;

/// The square of the wordmark that the masks show, in wordmark units. It is
/// 152 units on a side and centred on the U/N art at (202.3, 47.65). The art
/// reaches about 73 units from that centre. This is the smallest square that
/// lets the tray rotate a mask by any angle without clipping it.
const VIEW_BOX: &str = "126.3 -28.35 152 152";

fn main() {
    println!("cargo:rerun-if-changed=assets/spun-wordmark.svg");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let svg = fs::read_to_string("assets/spun-wordmark.svg").unwrap();
    // Everything after the root tag: the mask with one group per letter
    // (`spun-s`, `spun-p`, `spun-u`, `spun-n`) and the rect that it masks.
    let (_, body) = svg.split_once('>').unwrap();

    let mut code = format!("pub const SIZE: u32 = {SIZE};\n");
    for glyph in ["u", "n"] {
        // A square root tag crops the wordmark to the badge, and the style
        // hides every letter group except this glyph.
        let path = out_dir.join(format!("{glyph}.svg"));
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{SIZE}" height="{SIZE}" viewBox="{VIEW_BOX}"><style>mask g g:not(#spun-{glyph}){{display:none}}</style>{body}"#
        );
        fs::write(&path, svg).unwrap();
        let mask = rasterize_alpha(&path);
        let name = glyph.to_uppercase();
        code += &format!("pub const MASK_{name}: [u8; {}] = {mask:?};\n", mask.len());
    }
    fs::write(out_dir.join("tray_icon.rs"), code).unwrap();
}

/// Renders the SVG file at `path` through rsvg and keeps the alpha channel.
/// The art is one colour under a mask, so the alpha is the glyph coverage.
fn rasterize_alpha(path: &Path) -> Vec<u8> {
    let out = Command::new("rsvg-convert")
        .arg(path)
        .output()
        .expect("rsvg-convert not installed");
    assert!(
        out.status.success(),
        "rsvg-convert failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut reader = png::Decoder::new(std::io::Cursor::new(out.stdout))
        .read_info()
        .expect("valid PNG metadata");
    let mut buf = vec![0; reader.output_buffer_size().expect("PNG header read")];
    let info = reader.next_frame(&mut buf).expect("valid PNG frame");
    assert_eq!(info.color_type, png::ColorType::Rgba, "rsvg outputs RGBA8");
    assert_eq!((info.width, info.height), (SIZE, SIZE));
    let (pixels, _) = buf[..info.buffer_size()].as_chunks::<4>();
    pixels.iter().map(|[_, _, _, alpha]| *alpha).collect()
}
