//! Personal defaults use the same bounded loader as project configuration.

use std::io;
use std::path::{Path, PathBuf};

use crate::overrides::project::{self, LoadedFile, ProjectFile};
use crate::overrides::SourceKind;

pub fn location(home: Option<&Path>) -> Option<PathBuf> {
    home.map(|directory| directory.join(".config/fix-lsp.toml"))
}

pub fn parse(text: &str, path: &Path) -> Result<ProjectFile, String> {
    project::parse_toml(text, path, SourceKind::UserFile)
}

pub fn load(path: &Path) -> io::Result<LoadedFile> {
    project::load_source(path, SourceKind::UserFile, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overrides::{merge, parse_client};
    use serde_json::json;

    #[test]
    fn sparse_client_leaves_do_not_erase_user_defaults() {
        let user = parse(
            "hover.enabled = false\ninlayHints.tagNames.enabled = true\nallowDictionaryPath = true",
            Path::new("/home/test/.config/fix-lsp.toml"),
        )
        .unwrap();
        let client = parse_client(json!({"hover": {"enabled": true}})).unwrap();
        let resolved = merge(&[user.layer.clone(), client], &[]).unwrap();
        assert!(resolved.config.hover.enabled);
        assert!(resolved.config.inlay_hints.tag_names());
        assert!(resolved.config.allow_dictionary_path);
        assert!(!merge(&[user.layer], &[]).unwrap().config.hover.enabled);
    }

    #[test]
    fn personal_file_has_no_filter_selector_or_format_version() {
        for text in [
            "files = ['*.fix']",
            "dictionary = 'venue'",
            "version = 1",
            "unknown = true",
        ] {
            assert!(
                parse(text, Path::new("/home/test/.config/fix-lsp.toml")).is_err(),
                "{text}"
            );
        }
        assert!(parse_client(json!({"unknown": true}))
            .unwrap()
            .candidates
            .is_empty());
    }

    #[test]
    fn dictionary_versions_are_not_a_config_setting() {
        assert!(parse(
            "[[dictionaries]]\nname='venue'\npath='dict.xml'\nversion='FIX.4.4'",
            Path::new("/home/test/.config/fix-lsp.toml")
        )
        .is_err());
    }
}
