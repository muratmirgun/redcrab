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
        Network,
        Pencil,
        AppWindow,
        CircleArrowDown,
        Command,
        Plug,
        Power,
        Save,
        SunMoon
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `icons/square-terminal.svg` for `SquareTerminal`, as the icon set
    /// names its files.
    fn path(name: &str) -> String {
        let mut file = String::new();
        for (i, c) in name.chars().enumerate() {
            if c.is_ascii_uppercase() && i > 0 {
                file.push('-');
            }
            file.push(c.to_ascii_lowercase());
        }
        format!("icons/{file}.svg")
    }

    /// Every icon the app names in its source is one it can load: the
    /// component library's set covers only some, and a missing one draws
    /// nothing and logs an error on every frame.
    #[test]
    fn every_icon_the_app_uses_is_embedded() {
        assert_eq!(
            path("SquareTerminal"),
            gpui_kit::assets::IconName::SquareTerminal.path().as_ref()
        );
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut missing = Vec::new();
        for entry in std::fs::read_dir(src).unwrap() {
            let file = std::fs::read_to_string(entry.unwrap().path()).unwrap();
            for prefix in ["IconName::", "Lucide::"] {
                for (at, _) in file.match_indices(prefix) {
                    let name: String = file[at + prefix.len()..]
                        .chars()
                        .take_while(char::is_ascii_alphanumeric)
                        .collect();
                    let path = path(&name);
                    if !name.is_empty() && !matches!(AppAssets.load(&path), Ok(Some(_))) {
                        missing.push(name);
                    }
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "add these to ExtraIcons: {missing:?}");
    }
}
