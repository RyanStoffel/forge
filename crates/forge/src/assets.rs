//! Embedded asset source for SVG icons.
//!
//! Icons are compiled into the binary with `include_bytes!` rather than read
//! from disk, so the app stays a single relocatable executable with no runtime
//! asset-path resolution.
//!
//! GPUI rasterizes SVGs into an alpha mask and tints them with the element's
//! `text_color`, so the fill/stroke colors declared in the files themselves are
//! irrelevant — only coverage matters. That is what lets one icon follow the
//! theme's text tokens across states.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

pub struct Assets;

/// Registered icons, keyed by the path passed to `svg().path(..)`.
const ICONS: &[(&str, &[u8])] = &[
    ("icons/info.svg", include_bytes!("../assets/icons/info.svg")),
    (
        "icons/sidebar.svg",
        include_bytes!("../assets/icons/sidebar.svg"),
    ),
    (
        "icons/sidebar-right.svg",
        include_bytes!("../assets/icons/sidebar-right.svg"),
    ),
    ("icons/plus.svg", include_bytes!("../assets/icons/plus.svg")),
    (
        "icons/terminal.svg",
        include_bytes!("../assets/icons/terminal.svg"),
    ),
    (
        "icons/git-branch.svg",
        include_bytes!("../assets/icons/git-branch.svg"),
    ),
    (
        "icons/editor.svg",
        include_bytes!("../assets/icons/editor.svg"),
    ),
    ("icons/file.svg", include_bytes!("../assets/icons/file.svg")),
    (
        "icons/folder.svg",
        include_bytes!("../assets/icons/folder.svg"),
    ),
    (
        "icons/filter.svg",
        include_bytes!("../assets/icons/filter.svg"),
    ),
    (
        "icons/refresh.svg",
        include_bytes!("../assets/icons/refresh.svg"),
    ),
    ("icons/more.svg", include_bytes!("../assets/icons/more.svg")),
    (
        "icons/agents.svg",
        include_bytes!("../assets/icons/agents.svg"),
    ),
    (
        "icons/check.svg",
        include_bytes!("../assets/icons/check.svg"),
    ),
    (
        "icons/chevron-down.svg",
        include_bytes!("../assets/icons/chevron-down.svg"),
    ),
    (
        "icons/chevron-right.svg",
        include_bytes!("../assets/icons/chevron-right.svg"),
    ),
    (
        "icons/minus.svg",
        include_bytes!("../assets/icons/minus.svg"),
    ),
    ("icons/undo.svg", include_bytes!("../assets/icons/undo.svg")),
    ("icons/x.svg", include_bytes!("../assets/icons/x.svg")),
    ("icons/user.svg", include_bytes!("../assets/icons/user.svg")),
];

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_every_registered_icon() {
        for (path, _) in ICONS {
            let bytes = Assets.load(path).unwrap().expect("icon present");
            assert!(bytes.starts_with(b"<svg"), "expected SVG markup: {path}");
        }
    }

    #[test]
    fn unknown_asset_is_none_not_an_error() {
        assert!(Assets.load("icons/nope.svg").unwrap().is_none());
    }

    #[test]
    fn lists_icons_under_a_prefix() {
        assert!(!Assets.list("icons/").unwrap().is_empty());
    }
}
