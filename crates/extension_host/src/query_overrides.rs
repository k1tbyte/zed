//! Local additions to the language queries of installed extensions.
//!
//! A file at `<config dir>/extension_overrides/<extension id>/languages/<language>/<query>.scm`
//! is appended to the extension's own `<query>.scm` whenever the language loads. Installed
//! extension files are never touched, so updating an extension keeps the additions. Later
//! patterns win over earlier ones for the same node, so appended patterns can restyle.
//! Only query files the extension ships are extended: which ones load is decided from the
//! extension's own files, so an addition alone does not introduce a new kind of query.

use fs::Fs;
use std::path::{Path, PathBuf};

const OVERRIDES_DIR: &str = "extension_overrides";

/// The query file's content with the user's additions appended, if there are any.
pub(crate) async fn load(fs: &dyn Fs, path: &Path) -> Option<String> {
    let installed = paths::extensions_dir().join("installed");
    load_with_overlay(
        fs,
        path,
        overlay_path(path, &installed, paths::config_dir()),
    )
    .await
}

fn overlay_path(path: &Path, installed: &Path, config: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(installed).ok()?;
    Some(config.join(OVERRIDES_DIR).join(relative))
}

async fn load_with_overlay(fs: &dyn Fs, path: &Path, overlay: Option<PathBuf>) -> Option<String> {
    let base = fs.load(path).await.ok();
    let addition = match overlay {
        Some(overlay) => fs.load(&overlay).await.ok(),
        None => None,
    };
    match (base, addition) {
        (Some(base), Some(addition)) => Some(format!("{base}\n{addition}")),
        (base, addition) => base.or(addition),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::BackgroundExecutor;
    use serde_json::json;

    #[test]
    fn only_installed_extension_files_have_an_overlay() {
        let installed = Path::new("/data/extensions/installed");
        let config = Path::new("/config");
        assert_eq!(
            overlay_path(
                Path::new("/data/extensions/installed/vue/languages/vue/highlights.scm"),
                installed,
                config
            ),
            Some(PathBuf::from(
                "/config/extension_overrides/vue/languages/vue/highlights.scm"
            ))
        );
        assert_eq!(
            overlay_path(
                Path::new("/elsewhere/vue/highlights.scm"),
                installed,
                config
            ),
            None
        );
    }

    #[gpui::test]
    async fn additions_follow_the_extension_text(executor: BackgroundExecutor) {
        let fs = FakeFs::new(executor);
        fs.insert_tree(
            "/",
            json!({
                "ext": { "highlights.scm": "(a) @a", "outline.scm": "(o) @o" },
                "mine": { "highlights.scm": "(b) @b", "injections.scm": "(i) @i" },
            }),
        )
        .await;
        let overlay = |name: &str| Some(PathBuf::from("/mine").join(name));

        assert_eq!(
            load_with_overlay(
                fs.as_ref(),
                Path::new("/ext/highlights.scm"),
                overlay("highlights.scm")
            )
            .await,
            Some("(a) @a\n(b) @b".to_string())
        );
        assert_eq!(
            load_with_overlay(
                fs.as_ref(),
                Path::new("/ext/outline.scm"),
                overlay("outline.scm")
            )
            .await,
            Some("(o) @o".to_string()),
            "no addition leaves the extension's text alone"
        );
        assert_eq!(
            load_with_overlay(
                fs.as_ref(),
                Path::new("/ext/injections.scm"),
                overlay("injections.scm")
            )
            .await,
            Some("(i) @i".to_string()),
            "without a base file the addition is returned as is"
        );
        assert_eq!(
            load_with_overlay(fs.as_ref(), Path::new("/ext/highlights.scm"), None).await,
            Some("(a) @a".to_string())
        );
    }
}
