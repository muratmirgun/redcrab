//! Renders the app icon for every platform from `assets/logo/icon*.svg`.
//! Driven by `assets/logo/build-icons.sh` and the packaging scripts.
//!
//!     icons iconset <dir>   PNGs for a macOS .iconset (then `iconutil`)
//!     icons pngs <dir>      hicolor PNGs for Linux: <dir>/<n>x<n>/apps/redcrab.png
//!     icons ico <file>      a Windows .ico holding PNG images

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use resvg::{tiny_skia, usvg};

/// (pixel size, file name inside the iconset)
const ICONSET: [(u32, &str); 10] = [
    (16, "icon_16x16.png"),
    (32, "icon_16x16@2x.png"),
    (32, "icon_32x32.png"),
    (64, "icon_32x32@2x.png"),
    (128, "icon_128x128.png"),
    (256, "icon_128x128@2x.png"),
    (256, "icon_256x256.png"),
    (512, "icon_256x256@2x.png"),
    (512, "icon_512x512.png"),
    (1024, "icon_512x512@2x.png"),
];

/// The sizes desktop environments look up in the hicolor theme.
const HICOLOR: [u32; 8] = [16, 24, 32, 48, 64, 128, 256, 512];

/// Sizes Windows picks from for title bars, the taskbar and Explorer views.
const ICO: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: icons (iconset <dir> | pngs <dir> | ico <file>)";
    let (mode, out) = match (args.next(), args.next()) {
        (Some(mode), Some(out)) => (mode, PathBuf::from(out)),
        _ => bail!(usage),
    };
    let logo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/logo");
    let icons = Icons::load(&logo)?;
    match mode.as_str() {
        "iconset" => {
            std::fs::create_dir_all(&out)?;
            for (px, name) in ICONSET {
                std::fs::write(out.join(name), icons.png(px)?)?;
            }
        }
        "pngs" => {
            for px in HICOLOR {
                let dir = out.join(format!("{px}x{px}")).join("apps");
                std::fs::create_dir_all(&dir)?;
                std::fs::write(dir.join("redcrab.png"), icons.png(px)?)?;
            }
        }
        "ico" => {
            let images = ICO
                .iter()
                .map(|&px| Ok((px, icons.png(px)?)))
                .collect::<anyhow::Result<Vec<_>>>()?;
            std::fs::write(&out, ico(&images))?;
        }
        _ => bail!(usage),
    }
    Ok(())
}

struct Icons {
    full: usvg::Tree,
    /// Simplified drawing that stays legible at 32 px and below.
    small: usvg::Tree,
}

impl Icons {
    fn load(dir: &Path) -> anyhow::Result<Self> {
        let parse = |name: &str| -> anyhow::Result<usvg::Tree> {
            let svg = std::fs::read(dir.join(name)).with_context(|| format!("reading {name}"))?;
            Ok(usvg::Tree::from_data(&svg, &usvg::Options::default())?)
        };
        Ok(Self {
            full: parse("icon.svg")?,
            small: parse("icon-small.svg")?,
        })
    }

    fn png(&self, px: u32) -> anyhow::Result<Vec<u8>> {
        let tree = if px <= 32 { &self.small } else { &self.full };
        let mut pixmap = tiny_skia::Pixmap::new(px, px).context("zero-sized icon")?;
        let scale = px as f32 / tree.size().width();
        resvg::render(
            tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        Ok(pixmap.encode_png()?)
    }
}

/// An ICO container with PNG-compressed images (supported since Windows
/// Vista): a 6-byte header, a 16-byte directory entry per image, then the
/// PNG data.
fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (px, png) in images {
        // 256 is stored as 0 in the one-byte width and height fields.
        let side = if *px >= 256 { 0 } else { *px as u8 };
        out.extend_from_slice(&[side, side, 0, 0]); // width, height, palette size, reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // color planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in images {
        out.extend_from_slice(png);
    }
    out
}
