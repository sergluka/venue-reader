//! Bounded local configuration discovery and per-batch loading.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use super::{parse_structured, Origin, SettingsLayer, SourceKind, Warning};
use crate::config::Severity;

pub const MAX_TOML_BYTES: u64 = 1_048_576;
pub const FILE_NAME: &str = ".venue-reader.toml";

#[derive(Debug, Clone)]
pub struct ProjectFile {
    pub path: PathBuf,
    // Applicability follows the discovered link, not its target.
    pub directory: PathBuf,
    pub layer: SettingsLayer,
    pub files: Option<Vec<String>>,
    matcher: Option<GlobSet>,
}

impl ProjectFile {
    pub fn matches(&self, document: &Path) -> bool {
        let Ok(relative) = document.strip_prefix(&self.directory) else {
            return false;
        };
        self.matcher
            .as_ref()
            .is_none_or(|matcher| matcher.is_match(relative))
    }
}

#[derive(Debug, Clone)]
pub enum LoadedFile {
    Absent,
    Valid(ProjectFile),
    Rejected(Warning),
}

pub fn parse(text: &str, path: &Path) -> Result<ProjectFile, String> {
    parse_toml(text, path, SourceKind::Project)
}

pub fn parse_toml(text: &str, path: &Path, kind: SourceKind) -> Result<ProjectFile, String> {
    let parsed: toml::Value = toml::from_str(text).map_err(|error| error.to_string())?;
    let mut value = serde_json::to_value(parsed).map_err(|error| error.to_string())?;
    let files = if kind == SourceKind::Project {
        value
            .as_object_mut()
            .and_then(|object| object.remove("files"))
            .map(|patterns| {
                serde_json::from_value::<Vec<String>>(patterns)
                    .map_err(|error| format!("files: {error}"))
            })
            .transpose()?
    } else {
        None
    };
    let matcher = files.as_deref().map(compile_globs).transpose()?;
    let source = source_name(path);
    let layer = parse_structured(value, Origin::new(kind, source), true)?;
    Ok(ProjectFile {
        path: path.to_owned(),
        directory: path.parent().unwrap_or(Path::new("")).to_owned(),
        layer,
        files,
        matcher,
    })
}

fn compile_globs(patterns: &[String]) -> Result<GlobSet, String> {
    if patterns.is_empty() {
        return Err("files: expected a non-empty array".into());
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if pattern.is_empty()
            || pattern.starts_with('/')
            || pattern.starts_with('\\')
            || pattern.as_bytes().get(1) == Some(&b':')
            || pattern.split(['/', '\\']).any(|part| part == "..")
        {
            return Err(format!(
                "files: pattern {pattern:?} must stay relative to its configuration directory"
            ));
        }
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .case_insensitive(false)
            .build()
            .map_err(|error| format!("files: {error}"))?;
        builder.add(glob);
    }
    builder.build().map_err(|error| format!("files: {error}"))
}

pub fn source_name(path: &Path) -> String {
    lsp_types::Uri::from_file_path(path)
        .map(|uri| uri.to_string())
        .unwrap_or_else(|()| path.display().to_string())
}

pub fn load(path: &Path) -> io::Result<LoadedFile> {
    load_source(path, SourceKind::Project, true)
}

pub fn load_discovered(path: &Path) -> io::Result<LoadedFile> {
    load_source(path, SourceKind::Project, false).map(|mut loaded| {
        if let LoadedFile::Valid(file) = &mut loaded {
            file.directory = path.parent().unwrap_or(Path::new("")).to_owned();
        }
        loaded
    })
}

pub fn load_source(path: &Path, kind: SourceKind, optional: bool) -> io::Result<LoadedFile> {
    match fs::symlink_metadata(path) {
        Err(error) if optional && error.kind() == io::ErrorKind::NotFound => {
            return Ok(LoadedFile::Absent)
        }
        Err(error) => return Err(with_path(path, error)),
        Ok(_) => {}
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| with_path(path, error))?;
    let metadata = fs::metadata(&canonical).map_err(|error| with_path(path, error))?;
    let origin = Origin::new(kind.clone(), source_name(&canonical));
    if !metadata.is_file() || metadata.len() > MAX_TOML_BYTES {
        return Ok(rejected(
            origin,
            Severity::Warning,
            "configuration must be a regular file no larger than 1 MiB",
        ));
    }
    let bytes = File::open(&canonical)
        .and_then(read_bounded)
        .map_err(|error| with_path(path, error))?;
    let Some(bytes) = bytes else {
        return Ok(rejected(
            origin,
            Severity::Warning,
            "configuration exceeds 1 MiB",
        ));
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(error) => {
            return Ok(rejected(
                origin,
                Severity::Error,
                &format!("invalid UTF-8: {error}"),
            ))
        }
    };
    Ok(match parse_toml(text, &canonical, kind) {
        Ok(file) => LoadedFile::Valid(file),
        Err(error) => rejected(origin, Severity::Error, &error),
    })
}

fn read_bounded(reader: impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    reader.take(MAX_TOML_BYTES + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= MAX_TOML_BYTES).then_some(bytes))
}

fn rejected(origin: Origin, severity: Severity, message: &str) -> LoadedFile {
    let key = if origin.layer == SourceKind::UserFile {
        "userFile"
    } else {
        "projectConfig"
    };
    LoadedFile::Rejected(Warning {
        key: key.into(),
        message: message.into(),
        origin,
        severity,
    })
}

fn with_path(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

pub fn canonical_document(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "document path must be absolute",
        ));
    }
    let mut ancestor = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(&ancestor) {
            Ok(_) => {
                let mut result = ancestor
                    .canonicalize()
                    .map_err(|error| with_path(&ancestor, error))?;
                for component in missing.into_iter().rev() {
                    result.push(component);
                }
                return Ok(normalize(&result));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(name) = ancestor.components().next_back() else {
                    return Err(with_path(&ancestor, error));
                };
                missing.push(name.as_os_str().to_owned());
                ancestor.pop();
            }
            Err(error) => return Err(with_path(&ancestor, error)),
        }
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

pub fn discover(
    document: &Path,
    roots: &[PathBuf],
    root_uri: Option<&Path>,
) -> io::Result<Vec<PathBuf>> {
    let document = canonical_document(document)?;
    let Some(directory) = document.parent() else {
        return Ok(Vec::new());
    };
    let mut containing = Vec::new();
    for root in roots {
        let root = canonical_document(root)?;
        if document.starts_with(&root) {
            containing.push(root);
        }
    }
    let root = containing
        .into_iter()
        .max_by_key(|root| root.components().count());
    let fallback = root_uri
        .map(canonical_document)
        .transpose()?
        .filter(|root| document.starts_with(root));
    let boundary = root.or(fallback).unwrap_or_else(|| directory.to_path_buf());
    let mut directories = Vec::new();
    let mut current = directory;
    loop {
        directories.push(current.to_path_buf());
        if current == boundary {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    let mut files = Vec::new();
    for directory in directories.into_iter().rev() {
        let path = directory.join(FILE_NAME);
        match fs::symlink_metadata(&path) {
            Ok(_) => files.push(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(with_path(&path, error)),
        }
    }
    Ok(files)
}

#[derive(Debug, Default)]
pub struct BatchCache {
    entries: HashMap<PathBuf, LoadedFile>,
}

impl BatchCache {
    pub fn load_discovered(&mut self, path: &Path) -> io::Result<LoadedFile> {
        if let Some(cached) = self.entries.get(path) {
            return Ok(cached.clone());
        }
        let loaded = load_discovered(path)?;
        self.entries.insert(path.to_owned(), loaded.clone());
        Ok(loaded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let directory = std::env::temp_dir().join(format!(
                "venue-reader-config-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&directory).unwrap();
            Self(directory)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn bounded_read_accepts_the_ceiling_and_refuses_growth_beyond_it() {
        assert!(read_bounded(io::repeat(b' ').take(MAX_TOML_BYTES))
            .unwrap()
            .is_some());
        assert!(read_bounded(io::repeat(b' ').take(MAX_TOML_BYTES + 1))
            .unwrap()
            .is_none());
    }

    #[test]
    fn optional_missing_and_discovered_missing_are_distinct() {
        let fixture = Fixture::new();
        let path = fixture.0.join(FILE_NAME);
        assert!(matches!(load(&path).unwrap(), LoadedFile::Absent));
        assert_eq!(
            load_discovered(&path).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn schema_errors_and_non_regular_files_keep_their_severities() {
        let fixture = Fixture::new();
        assert!(matches!(
            load(&fixture.0).unwrap(),
            LoadedFile::Rejected(Warning {
                severity: Severity::Warning,
                ..
            })
        ));
        let path = fixture.0.join(FILE_NAME);
        fs::write(&path, "version = 1").unwrap();
        assert!(matches!(
            load(&path).unwrap(),
            LoadedFile::Rejected(Warning {
                severity: Severity::Error,
                ..
            })
        ));
        fs::write(&path, vec![b' '; MAX_TOML_BYTES as usize]).unwrap();
        assert!(matches!(load(&path).unwrap(), LoadedFile::Valid(_)));
        fs::write(&path, vec![b' '; MAX_TOML_BYTES as usize + 1]).unwrap();
        assert!(matches!(
            load(&path).unwrap(),
            LoadedFile::Rejected(Warning {
                severity: Severity::Warning,
                ..
            })
        ));
    }

    #[test]
    fn discovery_uses_longest_root_and_preserves_missing_ancestor_files() {
        let fixture = Fixture::new();
        let nested = fixture.0.join("nested");
        let logs = nested.join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(fixture.0.join(FILE_NAME), "hover.enabled = false").unwrap();
        fs::write(nested.join(FILE_NAME), "semanticTokens.enabled = false").unwrap();
        fs::write(logs.join(FILE_NAME), "inlayHints.tagNames.enabled = true").unwrap();
        let document = logs.join("new.fix");
        assert_eq!(
            discover(&document, &[fixture.0.clone(), nested.clone()], None).unwrap(),
            vec![
                nested.join(FILE_NAME).canonicalize().unwrap(),
                logs.join(FILE_NAME).canonicalize().unwrap(),
            ]
        );
        assert_eq!(discover(&document, &[], Some(&fixture.0)).unwrap().len(), 3);
        assert_eq!(
            discover(&document, &[], None).unwrap(),
            vec![logs.join(FILE_NAME).canonicalize().unwrap()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_workspace_and_new_files_have_one_boundary() {
        let fixture = Fixture::new();
        let real = fixture.0.join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join(FILE_NAME), "hover.enabled = false").unwrap();
        let link = fixture.0.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            discover(&link.join("not-created/new.fix"), &[link], None).unwrap(),
            vec![real.join(FILE_NAME).canonicalize().unwrap()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn cached_symlink_targets_keep_each_discovery_directory() {
        let fixture = Fixture::new();
        let target = fixture.0.join("shared.toml");
        fs::write(&target, "files = ['*.fix']\nhover.enabled = false").unwrap();
        let mut cache = BatchCache::default();
        for name in ["left", "right"] {
            let directory = fixture.0.join(name);
            fs::create_dir(&directory).unwrap();
            let path = directory.join(FILE_NAME);
            std::os::unix::fs::symlink(&target, &path).unwrap();
            let LoadedFile::Valid(file) = cache.load_discovered(&path).unwrap() else {
                panic!("valid symlinked file");
            };
            assert_eq!(file.path, target.canonicalize().unwrap());
            assert_eq!(file.directory, directory);
            assert!(file.matches(&directory.join("orders.fix")));
            assert!(!file.matches(&fixture.0.join("orders.fix")));
        }
        assert_eq!(cache.entries.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn fifo_without_a_writer_is_rejected_before_opening() {
        let fixture = Fixture::new();
        let path = fixture.0.join(FILE_NAME);
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(matches!(
            load(&path).unwrap(),
            LoadedFile::Rejected(Warning {
                severity: Severity::Warning,
                ..
            })
        ));
    }

    #[test]
    fn a_new_batch_observes_file_changes() {
        let fixture = Fixture::new();
        let path = fixture.0.join(FILE_NAME);
        fs::write(&path, "hover.enabled = true").unwrap();
        let mut cache = BatchCache::default();
        let LoadedFile::Valid(first) = cache.load_discovered(&path).unwrap() else {
            panic!("valid file");
        };
        assert_eq!(first.layer.get("hover.enabled"), Some(&json!(true)));
        fs::write(&path, "hover.enabled = false").unwrap();
        let LoadedFile::Valid(same_batch) = cache.load_discovered(&path).unwrap() else {
            panic!("valid cached file");
        };
        assert_eq!(same_batch.layer.get("hover.enabled"), Some(&json!(true)));
        let mut cache = BatchCache::default();
        let LoadedFile::Valid(next) = cache.load_discovered(&path).unwrap() else {
            panic!("valid file");
        };
        assert_eq!(next.layer.get("hover.enabled"), Some(&json!(false)));
    }

    #[test]
    fn toml_merges_native_tables_with_file_origins() {
        let file = parse(
            "dictionary = 'venue'\n[hover]\nenabled = false\n",
            Path::new("/work/.venue-reader.toml"),
        )
        .unwrap();
        assert_eq!(file.layer.get("hover.enabled"), Some(&json!(false)));
        assert_eq!(
            file.layer.candidate("hover.enabled").unwrap().origin.line,
            0
        );
        assert_eq!(file.layer.candidate("dictionary").unwrap().origin.line, 0);
    }

    #[test]
    fn multiline_templates_keep_the_file_as_their_source() {
        let text =
            "hover.enabled = false\nmessageSummary.template = '''\nhover.enabled = true\n'''\n";
        let file = parse(text, Path::new("/work/.venue-reader.toml")).unwrap();
        assert_eq!(
            file.layer.candidate("hover.enabled").unwrap().origin.line,
            0
        );
        assert_eq!(
            file.layer
                .candidate("messageSummary.template")
                .unwrap()
                .origin
                .line,
            0
        );
    }

    #[test]
    fn schema_errors_reject_the_file_but_foreign_settings_are_silent() {
        for text in [
            "version = 1",
            "future = true",
            "hover.enabled = 3",
            "hover.enabled = false\nhover.enabled = false",
            "dictionary = unset",
        ] {
            assert!(
                parse(text, Path::new("/work/.venue-reader.toml")).is_err(),
                "{text}"
            );
        }
        let file = parse(
            "formatter.width = 80\nrender.summary.position = 'above'\nhover.enabled = false",
            Path::new("/work/.venue-reader.toml"),
        )
        .unwrap();
        assert_eq!(file.layer.candidates.len(), 1);
        assert!(file.layer.warnings.is_empty());
    }

    #[test]
    fn global_only_candidates_warn_without_rejecting_local_settings() {
        let file = parse(
            "allowDictionaryPath = true\nhover.enabled = false",
            Path::new("/work/.venue-reader.toml"),
        )
        .unwrap();
        assert_eq!(file.layer.candidates.len(), 1);
        assert_eq!(file.layer.warnings.len(), 1);
    }

    #[test]
    fn glob_matching_is_anchored_case_sensitive_and_separator_aware() {
        let file = parse(
            "files = ['logs/*.fix']",
            Path::new("/work/.venue-reader.toml"),
        )
        .unwrap();
        assert!(file.matches(Path::new("/work/logs/a.fix")));
        assert!(!file.matches(Path::new("/work/logs/nested/a.fix")));
        assert!(!file.matches(Path::new("/work/logs/A.FIX")));
        assert!(!file.matches(Path::new("/work/else/logs/a.fix")));
        let deep = parse(
            "files = ['logs/**/*.fix']",
            Path::new("/work/.venue-reader.toml"),
        )
        .unwrap();
        assert!(deep.matches(Path::new("/work/logs/a.fix")));
        assert!(deep.matches(Path::new("/work/logs/nested/a.fix")));
        for text in [
            "files = []",
            "files = ['../a']",
            "files = ['/a']",
            "files = ['[']",
        ] {
            assert!(parse(text, Path::new("/work/.venue-reader.toml")).is_err());
        }
    }
}
