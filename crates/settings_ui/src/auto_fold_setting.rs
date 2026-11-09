//! The settings window's "Fold Imports on Open" toggle, over the `auto_fold` list.

use settings::{AutoFoldKind, LanguageSettingsContent};

pub(crate) fn imports_enabled(language: &LanguageSettingsContent) -> Option<&bool> {
    language.auto_fold.as_ref().map(|kinds| {
        if kinds.contains(&AutoFoldKind::Imports) {
            &true
        } else {
            &false
        }
    })
}

/// `None` removes the setting, so the default applies again.
pub(crate) fn set_imports(language: &mut LanguageSettingsContent, enabled: Option<bool>) {
    language.auto_fold = enabled.map(|enabled| {
        let mut kinds = language.auto_fold.take().unwrap_or_default();
        kinds.retain(|kind| *kind != AutoFoldKind::Imports);
        if enabled {
            kinds.push(AutoFoldKind::Imports);
        }
        kinds
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn language(auto_fold: Option<Vec<AutoFoldKind>>) -> LanguageSettingsContent {
        LanguageSettingsContent {
            auto_fold,
            ..Default::default()
        }
    }

    #[test]
    fn the_toggle_reads_whether_the_list_names_imports() {
        assert_eq!(imports_enabled(&language(None)), None);
        assert_eq!(imports_enabled(&language(Some(vec![]))), Some(&false));
        assert_eq!(
            imports_enabled(&language(Some(vec![AutoFoldKind::Imports]))),
            Some(&true)
        );
    }

    #[test]
    fn turning_it_on_and_off_edits_the_list() {
        let mut settings = language(None);
        set_imports(&mut settings, Some(true));
        assert_eq!(settings.auto_fold, Some(vec![AutoFoldKind::Imports]));

        set_imports(&mut settings, Some(true));
        assert_eq!(
            settings.auto_fold,
            Some(vec![AutoFoldKind::Imports]),
            "turning it on twice does not repeat it"
        );

        set_imports(&mut settings, Some(false));
        assert_eq!(settings.auto_fold, Some(vec![]));

        set_imports(&mut settings, None);
        assert_eq!(settings.auto_fold, None, "reset falls back to the default");
    }
}
