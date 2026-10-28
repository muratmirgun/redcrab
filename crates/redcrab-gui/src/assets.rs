use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString, assets::Assets};

// Icons the app uses beyond the component library's default set. Only these
// SVGs are embedded, not the whole Lucide catalog.
gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        Trash,
        Key,
        Database,
        Terminal,
        Clock,
        Server,
        Unplug,
        Braces,
        Lock,
        ShieldAlert,
        Network
    ]
);

/// The one-color redcrab mark, drawn with `svg().path(LOGO)`.
pub const LOGO: &str = "brand/redcrab.svg";
const LOGO_SVG: &[u8] = include_bytes!("../../../assets/logo/mark-mono.svg");

/// The default component icons plus [`ExtraIcons`] and the logo.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == LOGO {
            return Ok(Some(Cow::Borrowed(LOGO_SVG)));
        }
        match ExtraIcons.load(path)? {
            Some(svg) => Ok(Some(svg)),
            None => Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        if LOGO.starts_with(path) {
            paths.push(LOGO.into());
        }
        Ok(paths)
    }
}
