use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString, assets::Assets};

// Icons the app uses beyond the component library's default set. Only these
// SVGs are embedded, not the whole Lucide catalog.
gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        Trash, Key, Database, Terminal, Clock, Server, Unplug, Braces
    ]
);

/// The default component icons plus [`ExtraIcons`].
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match ExtraIcons.load(path)? {
            Some(svg) => Ok(Some(svg)),
            None => Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        Ok(paths)
    }
}
