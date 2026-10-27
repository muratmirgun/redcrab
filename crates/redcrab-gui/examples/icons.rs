//! Renders the PNG sizes of the macOS app icon into an `.iconset` folder.
//! Driven by `assets/logo/build-icns.sh`.

use std::path::Path;

use resvg::{tiny_skia, usvg};

/// (source drawing, pixel size, file name inside the iconset)
const SIZES: [(&str, u32, &str); 10] = [
    // Small sizes use the simplified drawing so the crab stays legible.
    ("icon-small.svg", 16, "icon_16x16.png"),
    ("icon-small.svg", 32, "icon_16x16@2x.png"),
    ("icon-small.svg", 32, "icon_32x32.png"),
    ("icon.svg", 64, "icon_32x32@2x.png"),
    ("icon.svg", 128, "icon_128x128.png"),
    ("icon.svg", 256, "icon_128x128@2x.png"),
    ("icon.svg", 256, "icon_256x256.png"),
    ("icon.svg", 512, "icon_256x256@2x.png"),
    ("icon.svg", 512, "icon_512x512.png"),
    ("icon.svg", 1024, "icon_512x512@2x.png"),
];

fn main() -> anyhow::Result<()> {
    let out = std::env::args().nth(1).expect("usage: icons <iconset dir>");
    let logo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/logo");
    for (source, px, name) in SIZES {
        let svg = std::fs::read(logo.join(source))?;
        let tree = usvg::Tree::from_data(&svg, &usvg::Options::default())?;
        let mut pixmap = tiny_skia::Pixmap::new(px, px).expect("non-zero size");
        let scale = px as f32 / tree.size().width();
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        pixmap.save_png(Path::new(&out).join(name))?;
    }
    Ok(())
}
