//! Personal defaults use the same bounded loader as project configuration.

use std::io;
use std::path::{Path, PathBuf};

use crate::overrides::project::{self, LoadedFile, ProjectFile};
use crate::overrides::SourceKind;

const FILE_NAME: &str = "fix-lsp.toml";

/// `$XDG_CONFIG_HOME/fix-lsp.toml`, or `~/.config/fix-lsp.toml` without it. The
/// XDG Base Directory specification says an unset, empty or relative
/// `XDG_CONFIG_HOME` must be ignored, so a bad value falls back rather than
/// resolving against the working directory.
fn location(config_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    config_home
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(FILE_NAME))
        .or_else(|| home.map(|directory| directory.join(".config").join(FILE_NAME)))
}

/// The location for this process. The only place the configuration directory is
/// read from the environment.
pub fn process_location() -> Option<PathBuf> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    location(config_home.as_deref(), std::env::home_dir().as_deref())
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
    fn xdg_config_home_replaces_the_home_default() {
        let config_home = Path::new(if cfg!(windows) { "C:/xdg" } else { "/xdg" });
        let home_directory = Path::new(if cfg!(windows) {
            "C:/home/test"
        } else {
            "/home/test"
        });
        assert_eq!(
            location(Some(config_home), Some(home_directory)),
            Some(config_home.join("fix-lsp.toml"))
        );
        assert_eq!(
            location(None, Some(home_directory)),
            Some(home_directory.join(".config").join("fix-lsp.toml"))
        );
        assert_eq!(location(None, None), None);
    }

    #[test]
    fn a_relative_or_empty_xdg_config_home_is_ignored() {
        let home_directory = Path::new(if cfg!(windows) {
            "C:/home/test"
        } else {
            "/home/test"
        });
        for value in ["relative/config", ""] {
            assert_eq!(
                location(Some(Path::new(value)), Some(home_directory)),
                Some(home_directory.join(".config").join("fix-lsp.toml")),
                "{value:?}"
            );
        }
        if cfg!(windows) {
            assert_eq!(
                location(Some(Path::new("/xdg")), Some(home_directory)),
                Some(home_directory.join(".config").join("fix-lsp.toml"))
            );
        }
        assert_eq!(location(Some(Path::new("relative/config")), None), None);
    }

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
