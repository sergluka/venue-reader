//! Registry construction is pure except for the bounded, reusable XML loader.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::SystemTime;

use xxhash_rust::xxh3::Xxh3;

use super::dict::{bundled, Dict, Version, DEFAULT_VERSION};
use super::model::ParsedDict;
use super::parse::{self, ParseError, RepoSources, Source};
use super::statics::DICTS_BUILD_HASH;
use crate::config::{DictionaryMode, DictionarySpec, Limits};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryEvent {
    LoadFailed { path: String, detail: String },
    DuplicateDefault { version: String, path: String },
    BadName { name: String, detail: String },
    BlankName { path: String },
    DefaultVersionUnresolved { requested: String },
}

impl RegistryEvent {
    pub fn message(&self) -> String {
        match self {
            Self::LoadFailed { path, detail } => {
                format!("dictionary {path:?} not loaded: {detail}")
            }
            Self::DuplicateDefault { version, path } => format!(
                "dictionary {path:?} is a second default for {version}; the first one is kept"
            ),
            Self::BadName { name, detail } => {
                format!("dictionary name {name:?} rejected: {detail}")
            }
            Self::BlankName { path } => {
                format!("dictionary {path:?} has a blank name; the entry is ignored")
            }
            Self::DefaultVersionUnresolved { requested } => {
                format!("defaultVersion {requested:?} has no dictionary; using {DEFAULT_VERSION}")
            }
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(
            self,
            Self::DuplicateDefault { .. } | Self::BadName { .. } | Self::BlankName { .. }
        )
    }
}

pub struct BuildContext<'a> {
    pub root_dir: Option<&'a Path>,
    pub home_dir: Option<&'a Path>,
    pub limits: &'a Limits,
}

pub struct DictionaryLayer<'a> {
    pub specs: &'a [DictionarySpec],
    pub source: &'a str,
    pub root_dir: Option<&'a Path>,
}

#[derive(Clone)]
pub struct DictionaryChoice {
    pub dict: Dict,
    pub version: String,
    pub source: String,
    pub identity: u64,
    pub name: Option<String>,
}

impl DictionaryChoice {
    pub fn compatible_with(&self, version: &str) -> bool {
        versions_compatible(&self.version, version)
    }
}

pub fn versions_compatible(dictionary_version: &str, message_version: &str) -> bool {
    dictionary_version == message_version
        || dictionary_version == Version::resolve_alias_str(message_version)
}

pub fn is_reserved_selector(name: &str) -> bool {
    Version::is_version_identifier(name)
}

#[derive(Clone)]
pub struct DictionaryDefinition {
    pub spec: DictionarySpec,
    pub source: String,
    pub choice: DictionaryChoice,
}

#[derive(Clone)]
pub struct Registry {
    by_version: HashMap<String, Dict>,
    /// The configured name of each `by_version` dictionary, for hover.
    default_names: HashMap<String, String>,
    by_name: HashMap<String, Dict>,
    definitions: Vec<DictionaryDefinition>,
    default_version: String,
    identity: u64,
}

// Shared across registry layers without retaining unused dictionaries.
type ParsedCache = HashMap<LoadKey, (u64, Weak<ParsedDict>)>;

#[derive(Default)]
pub struct DictionaryStore {
    cache: Mutex<ParsedCache>,
    #[cfg(test)]
    parse_count: AtomicUsize,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct LoadKey {
    mode: DictionaryMode,
    source: PathBuf,
    files: Vec<FileStamp>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct FileStamp {
    path: PathBuf,
    name: std::ffi::OsString,
    len: u64,
    mtime: SystemTime,
}

impl Registry {
    pub fn bundled_only() -> Registry {
        Registry {
            by_version: HashMap::new(),
            default_names: HashMap::new(),
            by_name: HashMap::new(),
            definitions: Vec::new(),
            default_version: DEFAULT_VERSION.to_owned(),
            identity: empty_identity(DEFAULT_VERSION),
        }
    }

    #[cfg(test)]
    pub fn build(
        specs: &[DictionarySpec],
        default_version: &str,
        ctx: &BuildContext<'_>,
        store: &DictionaryStore,
    ) -> (Registry, Vec<RegistryEvent>) {
        // The synchronous compatibility entry point is not used by the server loader.
        match store.build_registry(
            &[DictionaryLayer {
                specs,
                source: "client",
                root_dir: ctx.root_dir,
            }],
            default_version,
            ctx,
        ) {
            Ok(built) => built,
            Err(error) => (
                Self::bundled_only(),
                vec![RegistryEvent::LoadFailed {
                    path: "configuration".into(),
                    detail: error.to_string(),
                }],
            ),
        }
    }
}

impl DictionaryStore {
    pub fn build_registry(
        &self,
        layers: &[DictionaryLayer<'_>],
        default_version: &str,
        ctx: &BuildContext<'_>,
    ) -> Result<(Registry, Vec<RegistryEvent>), LoadError> {
        let mut registry = Registry::bundled_only();
        let mut events = Vec::new();
        for layer in layers {
            let layer_ctx = BuildContext {
                root_dir: layer.root_dir,
                home_dir: ctx.home_dir,
                limits: ctx.limits,
            };
            let mut defaults = HashSet::new();
            for spec in layer.specs {
                if spec.name.trim().is_empty() {
                    events.push(RegistryEvent::BlankName {
                        path: spec.path.clone(),
                    });
                    continue;
                }
                if let Some(detail) = registry.name_problem(&spec.name, layer.source) {
                    events.push(RegistryEvent::BadName {
                        name: spec.name.clone(),
                        detail,
                    });
                    continue;
                }
                let (key, content, parsed) = match self.load(spec, &layer_ctx) {
                    Ok(loaded) => loaded,
                    Err(error @ LoadError::Io { .. }) => return Err(error),
                    Err(error) => {
                        events.push(RegistryEvent::LoadFailed {
                            path: spec.path.clone(),
                            detail: error.to_string(),
                        });
                        continue;
                    }
                };
                let version = parsed.version.clone().expect("validated XML version");
                let source = display_load_key(&key);
                let dict = Dict::Owned(parsed);
                if spec.default {
                    if defaults.insert(version.clone()) {
                        registry.by_version.insert(version.clone(), dict.clone());
                        registry
                            .default_names
                            .insert(version.clone(), spec.name.clone());
                    } else {
                        events.push(RegistryEvent::DuplicateDefault {
                            version: version.clone(),
                            path: spec.path.clone(),
                        });
                    }
                }
                registry.by_name.insert(spec.name.clone(), dict.clone());
                registry.definitions.push(DictionaryDefinition {
                    spec: spec.clone(),
                    source: layer.source.to_owned(),
                    choice: DictionaryChoice {
                        dict,
                        version,
                        source,
                        identity: content,
                        name: Some(spec.name.clone()),
                    },
                });
            }
        }
        registry.validate_default(default_version, &mut events);
        registry.update_identity();
        Ok((registry, events))
    }

    pub fn load_choice(
        &self,
        path: &str,
        ctx: &BuildContext<'_>,
    ) -> Result<DictionaryChoice, LoadError> {
        let spec = DictionarySpec {
            path: path.to_owned(),
            name: String::new(),
            mode: DictionaryMode::Auto,
            default: false,
        };
        let (key, identity, parsed) = self.load(&spec, ctx)?;
        Ok(DictionaryChoice {
            version: parsed.version.clone().expect("validated XML version"),
            dict: Dict::Owned(parsed),
            source: display_load_key(&key),
            identity,
            name: None,
        })
    }

    fn load(
        &self,
        spec: &DictionarySpec,
        ctx: &BuildContext<'_>,
    ) -> Result<(LoadKey, u64, Arc<ParsedDict>), LoadError> {
        load(spec, ctx, self)
    }

    #[cfg(test)]
    pub fn parse_count(&self) -> usize {
        self.parse_count.load(Ordering::Relaxed)
    }
}

impl Registry {
    pub fn choice(&self, name: &str) -> Option<DictionaryChoice> {
        if let Some(version) = Version::parse(name) {
            let dict = Dict::Static(bundled(version));
            let declared = dict.version().expect("bundled version").to_owned();
            let mut identity = Xxh3::new();
            identity.update(&DICTS_BUILD_HASH.to_le_bytes());
            identity.update(declared.as_bytes());
            return Some(DictionaryChoice {
                dict,
                version: declared,
                source: name.to_owned(),
                identity: identity.digest(),
                name: Some(name.to_owned()),
            });
        }
        self.definitions
            .iter()
            .find(|entry| entry.spec.name == name)
            .map(|entry| entry.choice.clone())
    }

    pub fn definitions(&self) -> &[DictionaryDefinition] {
        &self.definitions
    }

    pub fn resolve(&self, version: &str) -> Option<Dict> {
        self.by_version
            .get(version)
            .or_else(|| self.by_version.get(Version::resolve_alias_str(version)))
            .cloned()
            .or_else(|| Version::parse(version).map(|version| Dict::Static(bundled(version))))
    }

    /// The configured name of the dictionary `resolve(version)` answers with,
    /// `None` for bundled tables.
    pub fn default_name(&self, version: &str) -> Option<&str> {
        self.default_names
            .get(version)
            .or_else(|| self.default_names.get(Version::resolve_alias_str(version)))
            .map(String::as_str)
    }

    pub fn bindings(&self) -> Vec<(&str, Dict)> {
        let mut bindings: Vec<_> = self
            .by_version
            .iter()
            .map(|(version, dict)| (version.as_str(), dict.clone()))
            .collect();
        for version in Version::ALL {
            let name = version.as_str();
            if !bindings.iter().any(|(bound, _)| *bound == name) {
                bindings.push((name, self.resolve(name).expect("bundled version")));
            }
        }
        bindings
    }

    pub fn named(&self, name: &str) -> Option<Dict> {
        self.choice(name).map(|choice| choice.dict)
    }

    pub fn default_version(&self) -> &str {
        &self.default_version
    }

    pub fn default_dict(&self) -> Dict {
        self.resolve(&self.default_version)
            .expect("validated default")
    }

    pub fn identity(&self) -> u64 {
        self.identity
    }

    fn validate_default(&mut self, requested: &str, events: &mut Vec<RegistryEvent>) {
        self.default_version = if self.resolve(requested).is_some() {
            requested.to_owned()
        } else {
            events.push(RegistryEvent::DefaultVersionUnresolved {
                requested: requested.to_owned(),
            });
            DEFAULT_VERSION.to_owned()
        };
    }

    fn update_identity(&mut self) {
        let mut bindings = Vec::new();
        for (name, dict) in &self.by_name {
            bindings.push((b'n', name, dict));
        }
        for (version, dict) in &self.by_version {
            bindings.push((b'v', version, dict));
        }
        bindings.sort_by_key(|(kind, key, _)| (*kind, *key));
        let mut identity = Xxh3::new();
        for (kind, key, dict) in bindings {
            let content = self
                .definitions
                .iter()
                .find(|entry| entry.choice.dict.ptr_eq(dict))
                .expect("custom binding has definition")
                .choice
                .identity;
            bind(&mut identity, kind, key, content);
        }
        identity.update(self.default_version.as_bytes());
        identity.update(&DICTS_BUILD_HASH.to_le_bytes());
        self.identity = identity.digest();
    }

    fn name_problem(&self, name: &str, source: &str) -> Option<String> {
        if Version::is_version_identifier(name) {
            return Some(format!(
                "{source} conflicts with reserved bundled identifier {name}"
            ));
        }
        self.definitions
            .iter()
            .find(|entry| entry.spec.name == name)
            .map(|entry| {
                format!(
                    "{source} conflicts with {} ({})",
                    entry.source, entry.spec.path
                )
            })
    }
}

fn display_load_key(key: &LoadKey) -> String {
    key.source.to_string_lossy().into_owned()
}

/// Why one `fix.dictionaries` entry could not be loaded. Rendered into the
/// `RegistryEvent::LoadFailed` text the client sees.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("dictionary paths are disabled; enable allowDictionaryPath in user TOML or global LSP settings")]
    PathsDisabled,
    #[error("unknown dictionary name {name:?}")]
    UnknownName { name: String },
    #[error("empty path")]
    EmptyPath,
    #[error("`~/` path but no home directory is known")]
    NoHome,
    #[error("relative path but no workspace root is known (rootUri/workspaceFolders)")]
    NoRoot,
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: not a regular file")]
    NotRegularFile { path: PathBuf },
    #[error("{path}: larger than limits.dictionaryBytes ({limit} bytes)")]
    TooLarge { path: PathBuf, limit: u64 },
    #[error("{path}: missing FIX version metadata; correct the XML header")]
    NoVersion { path: PathBuf },
    #[error("{path}: invalid XML FIX version {version:?}; correct the XML header")]
    InvalidVersion { path: PathBuf, version: String },
    #[error(transparent)]
    Parse(#[from] ParseError),
}

impl LoadError {
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Io { .. })
    }

    fn io(path: &Path, source: std::io::Error) -> LoadError {
        LoadError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// The files an entry reads, stat-checked once by `plan`.
enum Files {
    Quickfix(FileStamp),
    Repository { dir: PathBuf, files: Vec<FileStamp> },
}

impl Files {
    fn stamps(&self) -> Vec<FileStamp> {
        match self {
            Files::Quickfix(stamp) => vec![stamp.clone()],
            Files::Repository { files, .. } => files.clone(),
        }
    }

    fn display_path(&self) -> &Path {
        match self {
            Files::Quickfix(stamp) => &stamp.path,
            Files::Repository { dir, .. } => dir,
        }
    }
}

/// Resolve the path and stat every file the entry would read — exactly once
/// each, and without opening any of them.
fn plan(spec: &DictionarySpec, ctx: &BuildContext<'_>) -> Result<(LoadKey, Files), LoadError> {
    let limit = ctx.limits.dictionary_bytes;
    let path = resolve_path(&spec.path, ctx)?;
    let meta = fs::metadata(&path).map_err(|e| LoadError::io(&path, e))?;
    let repository = match spec.mode {
        DictionaryMode::Repository => true,
        DictionaryMode::Quickfix => false,
        DictionaryMode::Auto => {
            meta.is_dir() || path.file_name().is_some_and(|n| n == "Fields.xml")
        }
    };
    let files = if repository {
        let dir = repository_dir(&path, meta.is_dir())?;
        let mut files = Vec::new();
        for name in RepoSources::REQUIRED {
            files.push(stamp(&dir.join(name), limit)?);
        }
        for name in RepoSources::OPTIONAL {
            if let Some(s) = stamp_optional(&dir.join(name), limit)? {
                files.push(s);
            }
        }
        Files::Repository { dir, files }
    } else {
        Files::Quickfix(stamp_from(&path, &meta, limit)?)
    };
    let key = LoadKey {
        mode: spec.mode,
        source: files.display_path().to_path_buf(),
        files: files.stamps(),
    };
    Ok((key, files))
}

/// Reuse live parsed XML only after checking current metadata and limits.
fn load(
    spec: &DictionarySpec,
    ctx: &BuildContext<'_>,
    store: &DictionaryStore,
) -> Result<(LoadKey, u64, Arc<ParsedDict>), LoadError> {
    let (key, files) = plan(spec, ctx)?;
    let mut cache = store.cache.lock().expect("dictionary cache lock");
    if let Some((content, hit)) = cache.get(&key) {
        if let Some(hit) = hit.upgrade() {
            return Ok((key, *content, hit));
        }
    }
    cache.retain(|_, (_, parsed)| parsed.strong_count() != 0);
    drop(cache);
    let limit = ctx.limits.dictionary_bytes;
    let mut content = Xxh3::new();
    let parsed = match &files {
        Files::Quickfix(stamp) => {
            let source = read_stamped(stamp, limit)?;
            content.update(&source.bytes);
            parse::parse_quickfix_bytes(&source)?
        }
        Files::Repository { dir, files } => {
            let src = RepoSources::read_with(dir, |path, required| {
                match files
                    .iter()
                    .find(|stamp| Some(stamp.name.as_os_str()) == path.file_name())
                {
                    Some(stamp) => read_stamped(stamp, limit).map(Some),
                    None if required => Err(LoadError::io(
                        path,
                        std::io::Error::from(std::io::ErrorKind::NotFound),
                    )),
                    None => Ok(None),
                }
            })?;
            for source in [
                Some(&src.fields),
                Some(&src.enums),
                src.messages.as_ref(),
                src.msg_contents.as_ref(),
                src.components.as_ref(),
            ] {
                // An absent optional file is part of the identity too.
                match source {
                    Some(source) => {
                        content.update(&[1]);
                        content.update(&source.bytes);
                    }
                    None => content.update(&[0]),
                }
            }
            parse::parse_repository_bytes(&src)?
        }
    };
    let version = parsed
        .version
        .as_deref()
        .ok_or_else(|| LoadError::NoVersion {
            path: files.display_path().to_path_buf(),
        })?;
    if !valid_declared_version(version) {
        return Err(LoadError::InvalidVersion {
            path: files.display_path().to_path_buf(),
            version: version.to_owned(),
        });
    }
    let identity = content.digest();
    let parsed = Arc::new(parsed);
    #[cfg(test)]
    store.parse_count.fetch_add(1, Ordering::Relaxed);
    store
        .cache
        .lock()
        .expect("dictionary cache lock")
        .insert(key.clone(), (identity, Arc::downgrade(&parsed)));
    Ok((key, identity, parsed))
}

/// The identity of a registry with no custom bindings.
fn empty_identity(default_version: &str) -> u64 {
    let mut identity = Xxh3::new();
    identity.update(default_version.as_bytes());
    identity.update(&DICTS_BUILD_HASH.to_le_bytes());
    identity.digest()
}

/// Mixes one binding into the registry identity.
fn bind(identity: &mut Xxh3, kind: u8, key: &str, content: u64) {
    identity.update(&[kind]);
    identity.update(key.as_bytes());
    identity.update(&content.to_le_bytes());
}

/// Stat-only identity and policy check: a regular file within the limit.
fn stamp(path: &Path, limit: u64) -> Result<FileStamp, LoadError> {
    let meta = fs::metadata(path).map_err(|e| LoadError::io(path, e))?;
    stamp_from(path, &meta, limit)
}

/// `Ok(None)` when the file does not exist; any other problem is an error.
fn stamp_optional(path: &Path, limit: u64) -> Result<Option<FileStamp>, LoadError> {
    match fs::metadata(path) {
        Ok(meta) => stamp_from(path, &meta, limit).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(LoadError::io(path, e)),
    }
}

fn stamp_from(path: &Path, meta: &fs::Metadata, limit: u64) -> Result<FileStamp, LoadError> {
    if !meta.is_file() {
        return Err(LoadError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }
    if meta.len() > limit {
        return Err(LoadError::TooLarge {
            path: path.to_path_buf(),
            limit,
        });
    }
    let mtime = meta.modified().map_err(|e| LoadError::io(path, e))?;
    Ok(FileStamp {
        path: fs::canonicalize(path).map_err(|error| LoadError::io(path, error))?,
        name: path.file_name().unwrap_or_default().to_owned(),
        len: meta.len(),
        mtime,
    })
}

/// How much to pre-allocate from the size `stat` claimed. Both the size and
/// `limits.dictionaryBytes` are attacker/typo-controlled, and an oversized
/// `Vec::with_capacity` aborts the process instead of failing the entry.
const MAX_PREALLOC: u64 = 8 * 1024 * 1024;

/// Read a file that `plan` already stat-checked, at most `limit + 1` bytes so
/// a file that grew meanwhile is still caught.
fn read_stamped(stamp: &FileStamp, limit: u64) -> Result<Source, LoadError> {
    let path = &stamp.path;
    let mut bytes = Vec::with_capacity(stamp.len.min(MAX_PREALLOC) as usize);
    fs::File::open(path)
        // `limit` can be as large as the config says, so the +1 must not wrap.
        .and_then(|f| f.take(limit.saturating_add(1)).read_to_end(&mut bytes))
        .map_err(|e| LoadError::io(path, e))?;
    if bytes.len() as u64 > limit {
        return Err(LoadError::TooLarge {
            path: path.clone(),
            limit,
        });
    }
    Ok(Source {
        path: path.clone(),
        bytes,
    })
}

/// A version directory, a `…/Fields.xml` path, or a directory with `Base/`.
fn repository_dir(path: &Path, is_dir: bool) -> Result<PathBuf, LoadError> {
    let mut dir = if is_dir {
        path.to_path_buf()
    } else {
        // A canonical file path always has a parent.
        path.parent().unwrap_or(path).to_path_buf()
    };
    match fs::metadata(dir.join("Fields.xml")) {
        Ok(_) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let base = dir.join("Base");
            match fs::metadata(&base) {
                Ok(metadata) if metadata.is_dir() => dir = base,
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(LoadError::io(&base, error)),
            }
        }
        Err(error) => return Err(LoadError::io(&dir.join("Fields.xml"), error)),
    }
    Ok(dir)
}

fn valid_declared_version(version: &str) -> bool {
    let Some(rest) = version
        .strip_prefix("FIX.")
        .or_else(|| version.strip_prefix("FIXT."))
    else {
        return false;
    };
    let (base, service_pack) = rest
        .split_once("SP")
        .map_or((rest, None), |(base, pack)| (base, Some(pack)));
    let Some((major, minor)) = base.split_once('.') else {
        return false;
    };
    let numeric =
        |value: &str| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    numeric(major) && numeric(minor) && service_pack.is_none_or(numeric)
}

/// A selector spelled as a path rather than a name. The prefixes are the ones
/// `resolve_path` gives a meaning to, so the two are decided together.
pub fn looks_like_path(raw: &str) -> bool {
    Path::new(raw).is_absolute()
        || raw.starts_with("./")
        || raw.starts_with("../")
        || raw.starts_with("~/")
}

/// A plain string: only a leading `~/` expands; relative paths need a root.
fn resolve_path(raw: &str, ctx: &BuildContext<'_>) -> Result<PathBuf, LoadError> {
    if raw.is_empty() {
        return Err(LoadError::EmptyPath);
    }
    let expanded = match raw.strip_prefix("~/") {
        Some(rest) => ctx.home_dir.ok_or(LoadError::NoHome)?.join(rest),
        None => PathBuf::from(raw),
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        ctx.root_dir.ok_or(LoadError::NoRoot)?.join(expanded)
    };
    fs::canonicalize(&absolute).map_err(|e| LoadError::io(&absolute, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::LazyLock;

    const XML: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/xml");
    const QF_44: &str = "<fix major='4' minor='4'><fields>\
        <field number='8' name='BeginString' type='STRING'/>\
        <field number='9999' name='Custom' type='STRING'/></fields></fix>";
    const QF_44_OTHER: &str = "<fix major='4' minor='4'><fields>\
        <field number='9998' name='Other' type='STRING'/></fields></fix>";

    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    static LIMITS: LazyLock<Limits> = LazyLock::new(Limits::default);

    fn tmp() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("fix-lsp-registry-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, content: &str) -> String {
        let p = dir.join(name);
        fs::write(&p, content).unwrap();
        p.to_string_lossy().into_owned()
    }

    /// A name is mandatory now, so the helper supplies one: the file's own
    /// last segment, which is distinct per fixture in these tests.
    fn spec(path: &str) -> DictionarySpec {
        DictionarySpec {
            path: path.to_owned(),
            mode: DictionaryMode::Auto,
            // Prefixed: these fixtures are named after FIX versions on
            // purpose, and a name that looks like one is refused.
            name: format!("dict-{}", path.rsplit('/').next().unwrap_or(path)),
            default: true,
        }
    }

    fn named(path: &str, name: &str) -> DictionarySpec {
        DictionarySpec {
            name: name.to_owned(),
            ..spec(path)
        }
    }

    fn versioned(path: &str, version: &str) -> DictionarySpec {
        let xml = fs::read_to_string(path).unwrap();
        let xml = xml.replacen("<fix", &format!("<fix version='{version}'"), 1);
        fs::write(path, xml).unwrap();
        spec(path)
    }

    fn ctx<'a>(root: Option<&'a Path>, home: Option<&'a Path>) -> BuildContext<'a> {
        BuildContext {
            root_dir: root,
            home_dir: home,
            limits: &LIMITS,
        }
    }

    fn build(specs: &[DictionarySpec]) -> (Registry, Vec<RegistryEvent>) {
        Registry::build(
            specs,
            "FIX.4.4",
            &ctx(None, None),
            &DictionaryStore::default(),
        )
    }

    #[test]
    fn bindings_cover_every_bundled_version() {
        let (registry, _) = build(&[]);
        let listed: Vec<&str> = registry.bindings().iter().map(|(v, _)| *v).collect();
        for version in Version::ALL {
            assert!(listed.contains(&version.as_str()), "{version:?} missing");
        }
        assert_eq!(listed.len(), Version::ALL.len(), "and nothing else");
    }

    #[test]
    fn bindings_apply_the_fixt11_alias_to_a_custom_dictionary() {
        // The bug this method exists to make unrepresentable: a custom
        // FIX.5.0SP2 dictionary has to be what `8=FIXT.1.1` resolves to.
        let d = tmp();
        let p = write(&d, "sp2.xml", QF_44);
        let (registry, events) = build(&[versioned(&p, "FIX.5.0SP2")]);
        assert!(events.is_empty(), "{events:?}");

        let bindings = registry.bindings();
        let alias = bindings
            .iter()
            .find(|(v, _)| *v == "FIXT.1.1")
            .expect("FIXT.1.1 is bound");
        assert!(
            !alias.1.is_static(),
            "the custom dictionary answers the alias"
        );
        assert!(alias.1.ptr_eq(&registry.resolve("FIXT.1.1").unwrap()));
    }

    #[test]
    fn a_custom_binding_shadows_the_bundled_version_it_names() {
        let d = tmp();
        let p = write(&d, "a.xml", QF_44);
        let (registry, _) = build(&[versioned(&p, "FIX.4.4")]);
        let bindings = registry.bindings();
        let fix44 = bindings.iter().find(|(v, _)| *v == "FIX.4.4").unwrap();
        assert!(!fix44.1.is_static());
        let fix42 = bindings.iter().find(|(v, _)| *v == "FIX.4.2").unwrap();
        assert!(fix42.1.is_static(), "an unshadowed version stays bundled");
    }

    fn has_tag(dict: &Dict, tag: u32) -> bool {
        dict.field_idx(tag).is_some()
    }

    fn load_err(spec: &DictionarySpec) -> LoadError {
        load(spec, &ctx(None, None), &DictionaryStore::default()).unwrap_err()
    }

    #[test]
    fn version_entry_becomes_default_for_its_version() {
        let d = tmp();
        let (reg, events) = build(&[spec(&write(&d, "a.xml", QF_44))]);
        assert!(events.is_empty(), "{events:?}");
        let dict = reg.resolve("FIX.4.4").unwrap();
        assert!(!dict.is_static() && has_tag(&dict, 9999));
        assert!(reg.resolve("FIX.4.2").unwrap().is_static());
        assert_eq!(reg.default_dict().version(), Some("FIX.4.4"));
    }

    #[test]
    fn nondefault_entry_is_only_reachable_by_name() {
        let d = tmp();
        let s = DictionarySpec {
            default: false,
            ..named(&write(&d, "a.xml", QF_44), "bn")
        };
        let (reg, events) = build(&[s]);
        assert!(events.is_empty());
        assert!(reg.resolve("FIX.4.4").unwrap().is_static());
        assert!(has_tag(&reg.named("bn").unwrap(), 9999));
    }

    #[test]
    fn entry_with_both_registers_both() {
        let d = tmp();
        let s = DictionarySpec {
            ..named(&write(&d, "a.xml", QF_44), "bn")
        };
        let (reg, events) = build(&[s]);
        assert!(events.is_empty());
        assert!(has_tag(&reg.resolve("FIX.4.4").unwrap(), 9999));
        assert!(has_tag(&reg.named("bn").unwrap(), 9999));
    }

    #[test]
    fn declared_version_does_not_depend_on_the_filename() {
        let d = tmp();
        let (reg, events) = build(&[versioned(&write(&d, "a.xml", QF_44), "FIX.9.1")]);
        assert!(events.is_empty());
        assert!(has_tag(&reg.resolve("FIX.9.1").unwrap(), 9999));
        assert_eq!(reg.resolve("FIX.9.1").unwrap().version(), Some("FIX.9.1"));
        assert!(reg.resolve("FIX.4.4").unwrap().is_static());
    }

    #[test]
    fn version_never_falls_back_to_file_and_directory_names() {
        let d = tmp();
        // A version-like filename cannot repair missing XML metadata.
        let f = write(&d, "FIX42-prod.xml", "<fix><fields/></fix>");
        let (reg, events) = build(&[spec(&f)]);
        assert!(matches!(
            events.as_slice(),
            [RegistryEvent::LoadFailed { .. }]
        ));
        assert!(reg.resolve("FIX.4.2").unwrap().is_static());
        let repo = d.join("FIX.4.1");
        fs::create_dir(&repo).unwrap();
        write(&repo, "Fields.xml", "<Fields/>");
        write(&repo, "Enums.xml", "<Enums/>");
        let (reg, events) = build(&[spec(&repo.to_string_lossy())]);
        assert!(matches!(
            events.as_slice(),
            [RegistryEvent::LoadFailed { .. }]
        ));
        assert!(reg.resolve("FIX.4.1").unwrap().is_static());
    }

    #[test]
    fn duplicate_default_first_wins_and_reports() {
        let d = tmp();
        let first = write(&d, "a.xml", QF_44);
        let second = write(&d, "b.xml", QF_44_OTHER);
        let both = DictionarySpec {
            ..named(&second, "other")
        };
        let (reg, events) = build(&[spec(&first), both]);
        assert_eq!(
            events,
            vec![RegistryEvent::DuplicateDefault {
                version: "FIX.4.4".into(),
                path: second.clone()
            }]
        );
        assert!(has_tag(&reg.resolve("FIX.4.4").unwrap(), 9999));
        // The losing entry's name binding still registered (bindings are independent).
        assert!(has_tag(&reg.named("other").unwrap(), 9998));
    }

    #[test]
    fn name_colliding_with_version_identifier_is_rejected() {
        let d = tmp();
        let p = write(&d, "a.xml", QF_44);
        for bad in ["FIX.4.4", "FIX.5.0SP1", "FIXT.1.1", "FIX.2.7"] {
            let s = DictionarySpec { ..named(&p, bad) };
            let (reg, events) = build(&[s]);
            assert!(
                matches!(&events[..], [RegistryEvent::BadName { name, .. }] if name == bad),
                "{bad:?}: {events:?}"
            );
            assert!(reg.named(bad).is_none_or(|dict| dict.is_static()));
            assert!(reg.resolve("FIX.4.4").unwrap().is_static());
        }
    }

    #[test]
    fn a_blank_name_rejects_the_whole_entry() {
        let d = tmp();
        let p = write(&d, "a.xml", QF_44);
        for blank in ["", "   ", "\t"] {
            let s = DictionarySpec { ..named(&p, blank) };
            let (reg, events) = build(&[s]);
            assert!(
                matches!(&events[..], [RegistryEvent::BlankName { .. }]),
                "{blank:?}: {events:?}"
            );
            assert!(
                reg.resolve("FIX.9.1").is_none(),
                "{blank:?}: the version binding went with it"
            );
        }
    }

    #[test]
    fn duplicate_name_is_rejected_first_wins() {
        let d = tmp();
        let second = DictionarySpec {
            default: false,
            ..named(&write(&d, "b.xml", QF_44_OTHER), "x")
        };
        let (reg, events) = build(&[named(&write(&d, "a.xml", QF_44), "x"), second]);
        assert!(matches!(&events[..], [RegistryEvent::BadName { .. }]));
        assert!(has_tag(&reg.named("x").unwrap(), 9999));
    }

    /// The configuration `default` exists for: two venues, one FIX version.
    /// Their user-defined tags mean different things and the log says only
    /// `8=FIX.4.4`, so which one answers globally has to be said rather than
    /// decided by array order — and the one that does not answer must not be
    /// an error.
    #[test]
    fn two_dictionaries_of_one_version_coexist_when_one_declines_the_default() {
        let d = tmp();
        let venue_a = DictionarySpec {
            default: true,
            ..named(&write(&d, "a.xml", QF_44), "venue-a")
        };
        // Spelled out because `named` inherits the helper's `default: true`;
        // that the *schema* default is false is `config`'s test to make.
        let venue_b = DictionarySpec {
            default: false,
            ..named(&write(&d, "b.xml", QF_44_OTHER), "venue-b")
        };
        let (reg, events) = build(&[venue_a, venue_b]);

        assert!(
            events.is_empty(),
            "a legitimate desk setup is silent: {events:?}"
        );
        assert!(
            has_tag(&reg.resolve("FIX.4.4").unwrap(), 9999),
            "the one that asked for it answers for the version"
        );
        assert!(
            has_tag(&reg.named("venue-b").unwrap(), 9998),
            "and the other is still reachable by name"
        );

        // Neither asking is silent too, and leaves the bundled dictionary in
        // place: registering a dictionary is not by itself a claim on a version.
        let declining = |path: &str, name: &str| DictionarySpec {
            default: false,
            ..named(path, name)
        };
        let (reg, events) = build(&[
            declining(&write(&d, "c.xml", QF_44), "venue-c"),
            declining(&write(&d, "e.xml", QF_44_OTHER), "venue-e"),
        ]);
        assert!(events.is_empty(), "{events:?}");
        assert!(reg.resolve("FIX.4.4").unwrap().is_static());

        // Both asking is the collision the key exists to let a user avoid.
        let claiming = |path: &str, name: &str| DictionarySpec {
            default: true,
            ..named(path, name)
        };
        let (_, events) = build(&[
            claiming(&write(&d, "f.xml", QF_44), "venue-f"),
            claiming(&write(&d, "g.xml", QF_44_OTHER), "venue-g"),
        ]);
        assert!(
            matches!(&events[..], [RegistryEvent::DuplicateDefault { .. }]),
            "{events:?}"
        );
    }

    #[test]
    fn custom_fixt11_shadows_the_alias_but_not_fix50sp2() {
        let d = tmp();
        let (reg, events) = build(&[versioned(&write(&d, "a.xml", QF_44), "FIXT.1.1")]);
        assert!(events.is_empty());
        assert!(!reg.resolve("FIXT.1.1").unwrap().is_static());
        assert!(reg.resolve("FIX.5.0SP2").unwrap().is_static());
    }

    #[test]
    fn resolve_falls_back_to_bundled_via_alias() {
        let reg = Registry::bundled_only();
        let fixt = reg.resolve("FIXT.1.1").unwrap();
        assert!(fixt.is_static());
        assert_eq!(fixt.version(), Some("FIX.5.0SP2"));
        assert!(reg.resolve("FIX.9.9").is_none());
        assert_eq!(reg.default_dict().version(), Some("FIX.4.4"));
    }

    #[test]
    fn missing_file_and_bad_xml_drop_entry_with_event() {
        let d = tmp();
        let bad = write(
            &d,
            "bad.xml",
            "<fix major='4' minor='4'><fields><field number='x'/></fields></fix>",
        );
        let missing = d.join("missing.xml").to_string_lossy().into_owned();
        let (reg, events) = build(&[spec(&bad)]);
        assert_eq!(events.len(), 1);
        assert!(events
            .iter()
            .all(|e| matches!(e, RegistryEvent::LoadFailed { .. })));
        assert!(
            reg.resolve("FIX.4.4").unwrap().is_static(),
            "falls back to bundled"
        );
        assert!(matches!(load_err(&spec(&bad)), LoadError::Parse(_)));
        assert!(matches!(load_err(&spec(&missing)), LoadError::Io { .. }));
    }

    #[test]
    fn undetectable_version_is_a_load_failure() {
        let d = tmp();
        let p = write(
            &d,
            "dict.xml",
            "<fix><fields><field number='1' name='A'/></fields></fix>",
        );
        assert!(matches!(load_err(&spec(&p)), LoadError::NoVersion { .. }));
    }

    #[test]
    fn oversized_file_is_rejected_on_stat() {
        let d = tmp();
        let p = write(&d, "a.xml", QF_44);
        let limits = Limits {
            dictionary_bytes: 16,
            ..Limits::default()
        };
        let ctx = BuildContext {
            limits: &limits,
            ..ctx(None, None)
        };
        let store = DictionaryStore::default();
        let err = load(&spec(&p), &ctx, &store).unwrap_err();
        assert!(
            matches!(err, LoadError::TooLarge { limit: 16, .. }),
            "{err}"
        );
        let (_, events) = Registry::build(&[spec(&p)], "FIX.4.4", &ctx, &store);
        assert!(
            matches!(&events[..], [RegistryEvent::LoadFailed { detail, .. }] if detail.contains("dictionaryBytes"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn fifo_is_rejected_without_opening() {
        let d = tmp();
        let fifo = d.join("pipe.xml");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        // Opening a FIFO for reading with no writer would block forever; the
        // stat check must reject it first.
        let err = load_err(&spec(&fifo.to_string_lossy()));
        assert!(matches!(err, LoadError::NotRegularFile { .. }), "{err}");
    }

    #[test]
    fn tilde_expands_and_no_other_expansion_happens() {
        let home = tmp();
        write(&home, "d.xml", QF_44);
        let ctx = ctx(None, Some(&home));
        let store = DictionaryStore::default();
        let (reg, events) = Registry::build(&[spec("~/d.xml")], "FIX.4.4", &ctx, &store);
        assert!(events.is_empty(), "{events:?}");
        assert!(has_tag(&reg.resolve("FIX.4.4").unwrap(), 9999));

        for literal in ["$HOME/d.xml", "`echo x`", "~/*.xml", "~user/d.xml"] {
            // Taken literally: either a relative path without a root, or a
            // file that does not exist — never an expansion.
            let err = load(&spec(literal), &ctx, &store).unwrap_err();
            assert!(
                matches!(err, LoadError::Io { .. } | LoadError::NoRoot),
                "{literal}: {err}"
            );
        }
    }

    #[test]
    fn home_dir_unavailable_makes_tilde_paths_fail() {
        assert!(matches!(load_err(&spec("~/d.xml")), LoadError::NoHome));
        assert!(matches!(load_err(&spec("")), LoadError::EmptyPath));
    }

    #[test]
    fn relative_path_resolves_against_root_not_cwd() {
        let root = tmp();
        write(&root, "d.xml", QF_44);
        let (reg, events) = Registry::build(
            &[spec("d.xml")],
            "FIX.4.4",
            &ctx(Some(&root), None),
            &DictionaryStore::default(),
        );
        assert!(events.is_empty(), "{events:?}");
        assert!(has_tag(&reg.resolve("FIX.4.4").unwrap(), 9999));

        // No root: a relative path cannot be resolved, even if cwd would contain it.
        assert!(matches!(load_err(&spec("Cargo.toml")), LoadError::NoRoot));
    }

    #[test]
    fn an_absurd_byte_limit_neither_overflows_nor_preallocates() {
        let d = tmp();
        let p = write(&d, "a.xml", QF_44);
        let limits = Limits {
            dictionary_bytes: u64::MAX,
            ..Limits::default()
        };
        let ctx = BuildContext {
            limits: &limits,
            ..ctx(None, None)
        };
        // `limit + 1` would wrap to take(0) in release and panic in debug.
        let (_, _, parsed) = load(&spec(&p), &ctx, &DictionaryStore::default()).unwrap();
        assert!(parsed.field(9999).is_some());
    }

    #[test]
    fn a_rebuild_that_reuses_everything_has_the_same_identity() {
        let d = tmp();
        let p = write(&d, "a.xml", QF_44);
        let ctx = ctx(None, None);
        let store = DictionaryStore::default();
        let (first, _) = Registry::build(&[spec(&p)], "FIX.4.4", &ctx, &store);
        let (second, _) = Registry::build(&[spec(&p)], "FIX.4.4", &ctx, &store);
        assert_eq!(second.identity(), first.identity());

        // A changed file, a new binding, or a different default all differ.
        write(&d, "a.xml", QF_44_OTHER);
        let (reloaded, _) = Registry::build(&[spec(&p)], "FIX.4.4", &ctx, &store);
        assert_ne!(reloaded.identity(), second.identity());
        let (renamed, _) = Registry::build(&[named(&p, "x")], "FIX.4.4", &ctx, &store);
        assert_ne!(renamed.identity(), reloaded.identity());
        let (bundled, _) = Registry::build(&[], "FIX.4.2", &ctx, &store);
        assert_ne!(bundled.identity(), Registry::bundled_only().identity());
    }

    #[test]
    fn two_independent_builds_of_the_same_files_agree() {
        // Different `Arc`s, same content — the case a pointer comparison got
        // wrong and the reason identity is a content hash.
        let d = tmp();
        let p = write(&d, "a.xml", QF_44);
        let (one, _) = build(&[spec(&p)]);
        let (two, _) = build(&[spec(&p)]);
        assert_eq!(one.identity(), two.identity());
        assert_ne!(one.identity(), Registry::bundled_only().identity());
    }

    #[test]
    fn rewriting_a_file_with_the_same_bytes_keeps_the_identity() {
        // The point of hashing content rather than stamps: the `LoadKey` moves,
        // the file is genuinely re-read and re-parsed, a fresh `Arc` is
        // produced — and no cached analysis anywhere has to die.
        let d = tmp();
        let p = write(&d, "a.xml", QF_44);
        let (before, _) = build(&[spec(&p)]);
        write(&d, "a.xml", QF_44);
        let (after, _) = build(&[spec(&p)]);
        assert_eq!(before.identity(), after.identity());
    }

    #[test]
    fn a_rejected_binding_does_not_enter_the_identity() {
        // A conflicting name contributes no binding or parsed-cache entry.
        let d = tmp();
        let p = write(&d, "a.xml", QF_44);
        let (one, _) = build(&[spec(&p)]);
        let (two, events) = build(&[spec(&p), spec(&p)]);
        assert!(events
            .iter()
            .any(|e| matches!(e, RegistryEvent::BadName { .. })));
        assert_eq!(one.identity(), two.identity());
    }

    #[test]
    fn repeated_build_picks_up_changed_file() {
        let d = tmp();
        let p = write(&d, "a.xml", QF_44);
        let store = DictionaryStore::default();
        let (first, _) = Registry::build(&[spec(&p)], "FIX.4.4", &ctx(None, None), &store);
        assert!(has_tag(&first.resolve("FIX.4.4").unwrap(), 9999));
        write(&d, "a.xml", QF_44_OTHER);
        // Re-stat sees the new size/mtime: the cache is bypassed.
        let (reg, _) = Registry::build(&[spec(&p)], "FIX.4.4", &ctx(None, None), &store);
        assert!(!has_tag(&reg.resolve("FIX.4.4").unwrap(), 9999));
        assert!(has_tag(&reg.resolve("FIX.4.4").unwrap(), 9998));
    }

    #[test]
    fn unchanged_files_are_reused_without_reparse() {
        let d = tmp();
        let p = write(&d, "a.xml", QF_44);
        let ctx = ctx(None, None);
        let store = DictionaryStore::default();
        let (first, _) = Registry::build(&[spec(&p)], "FIX.4.4", &ctx, &store);
        let (second, events) = Registry::build(&[spec(&p)], "FIX.4.4", &ctx, &store);
        assert!(events.is_empty());
        assert_eq!(store.parse_count(), 1);
        let (Dict::Owned(a), Dict::Owned(b)) = (
            first.resolve("FIX.4.4").unwrap(),
            second.resolve("FIX.4.4").unwrap(),
        ) else {
            panic!("custom dictionaries expected");
        };
        assert!(Arc::ptr_eq(&a, &b), "same files → same parsed dictionary");

        // Changed XML metadata produces a new dictionary identity.
        let (third, _) = Registry::build(&[versioned(&p, "FIX.9.1")], "FIX.4.4", &ctx, &store);
        let Dict::Owned(c) = third.resolve("FIX.9.1").unwrap() else {
            panic!()
        };
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(c.version.as_deref(), Some("FIX.9.1"));
    }

    #[test]
    fn sibling_project_registries_share_parsed_xml_across_global_rebuilds() {
        let directory = tmp();
        let path = write(&directory, "venue.xml", QF_44);
        let store = DictionaryStore::default();
        let specs = [named(&path, "venue")];
        let layers = [DictionaryLayer {
            specs: &specs,
            source: "project.toml",
            root_dir: None,
        }];
        let context = ctx(None, None);
        let (first, events) = store
            .build_registry(&layers, DEFAULT_VERSION, &context)
            .unwrap();
        assert!(events.is_empty());
        let (second, events) = store
            .build_registry(&layers, DEFAULT_VERSION, &context)
            .unwrap();
        assert!(events.is_empty());
        assert!(first
            .named("venue")
            .unwrap()
            .ptr_eq(&second.named("venue").unwrap()));
        assert_eq!(store.parse_count(), 1);
        let (third, _) = store
            .build_registry(&layers, DEFAULT_VERSION, &context)
            .unwrap();
        assert!(first
            .named("venue")
            .unwrap()
            .ptr_eq(&third.named("venue").unwrap()));
    }

    #[test]
    fn shared_xml_cache_rechecks_metadata_and_limits() {
        let directory = tmp();
        let path = write(&directory, "venue.xml", QF_44);
        let store = DictionaryStore::default();
        let specs = [named(&path, "venue")];
        let layers = [DictionaryLayer {
            specs: &specs,
            source: "project.toml",
            root_dir: None,
        }];
        let context = ctx(None, None);
        let (first, _) = store
            .build_registry(&layers, DEFAULT_VERSION, &context)
            .unwrap();
        let limits = Limits {
            dictionary_bytes: 16,
            ..Limits::default()
        };
        let (limited, events) = store
            .build_registry(
                &layers,
                DEFAULT_VERSION,
                &BuildContext {
                    limits: &limits,
                    ..context
                },
            )
            .unwrap();
        assert!(limited.named("venue").is_none());
        assert!(matches!(
            events.as_slice(),
            [RegistryEvent::LoadFailed { .. }]
        ));
        write(&directory, "venue.xml", QF_44_OTHER);
        let (changed, _) = store
            .build_registry(&layers, DEFAULT_VERSION, &context)
            .unwrap();
        assert!(!first
            .named("venue")
            .unwrap()
            .ptr_eq(&changed.named("venue").unwrap()));
        assert!(has_tag(&changed.named("venue").unwrap(), 9998));
    }

    #[test]
    fn shared_xml_cache_does_not_retain_unused_project_dictionaries() {
        let directory = tmp();
        let path = write(&directory, "venue.xml", QF_44);
        let store = DictionaryStore::default();
        let specs = [named(&path, "venue")];
        let layers = [DictionaryLayer {
            specs: &specs,
            source: "project.toml",
            root_dir: None,
        }];
        let (project, _) = store
            .build_registry(&layers, DEFAULT_VERSION, &ctx(None, None))
            .unwrap();
        let Dict::Owned(parsed) = project.named("venue").unwrap() else {
            panic!("custom dictionary expected");
        };
        let weak = Arc::downgrade(&parsed);
        drop(parsed);
        drop(project);
        assert!(weak.upgrade().is_none());
        let (reopened, events) = store
            .build_registry(&layers, DEFAULT_VERSION, &ctx(None, None))
            .unwrap();
        assert!(events.is_empty());
        assert!(reopened.named("venue").is_some());
        assert_eq!(store.cache.lock().unwrap().len(), 1);
        assert_eq!(store.parse_count(), 2);
    }

    #[test]
    fn unresolved_default_version_falls_back_to_fix44_with_event() {
        let (reg, events) = Registry::build(
            &[],
            "FIX.9.9",
            &ctx(None, None),
            &DictionaryStore::default(),
        );
        assert_eq!(
            events,
            vec![RegistryEvent::DefaultVersionUnresolved {
                requested: "FIX.9.9".into()
            }]
        );
        assert_eq!(reg.default_version(), "FIX.4.4");

        // A custom default for that version satisfies it.
        let d = tmp();
        let (reg, events) = Registry::build(
            &[versioned(&write(&d, "a.xml", QF_44), "FIX.9.9")],
            "FIX.9.9",
            &ctx(None, None),
            &DictionaryStore::default(),
        );
        assert!(events.is_empty());
        assert_eq!(reg.default_version(), "FIX.9.9");
        assert!(has_tag(&reg.default_dict(), 9999));
    }

    #[test]
    fn repository_directory_descends_into_base_and_reads_version() {
        // A version directory (auto → repository → Base/) and a Fields.xml path.
        let dir = format!("{XML}/custom/synthetic/repository/FIX.4.4");
        let fields = format!("{XML}/custom/synthetic/repository/FIX.5.0SP2/Base/Fields.xml");
        let (reg, events) = build(&[spec(&dir), spec(&fields)]);
        assert!(events.is_empty(), "{events:?}");
        let d44 = reg.resolve("FIX.4.4").unwrap();
        assert!(!d44.is_static());
        assert!(d44.message_idx("0").is_some(), "groups/messages loaded too");
        assert!(!reg.resolve("FIX.5.0SP2").unwrap().is_static());
    }

    #[test]
    fn synthetic_quickfix_files_declare_their_versions() {
        let cases = [
            ("custom/synthetic/oe-fix44.xml", "FIX.4.4"),
            ("custom/synthetic/md-fix50.xml", "FIX.5.0"),
            ("custom/synthetic/session-fixt11.xml", "FIXT.1.1"),
        ];
        for (rel, version) in cases {
            let (reg, events) = build(&[spec(&format!("{XML}/{rel}"))]);
            assert!(events.is_empty(), "{rel}: {events:?}");
            let dict = reg.resolve(version).unwrap();
            assert!(
                !dict.is_static(),
                "{rel} should be the custom default for {version}"
            );
        }
    }

    #[test]
    fn required_missing_dictionary_is_a_fatal_load_error() {
        let directory = tmp();
        let specs = [spec(&directory.join("missing.xml").to_string_lossy())];
        let layers = [DictionaryLayer {
            specs: &specs,
            source: "user",
            root_dir: None,
        }];
        let error = DictionaryStore::default()
            .build_registry(&layers, DEFAULT_VERSION, &ctx(None, None))
            .err()
            .expect("missing XML must fail the transaction");
        assert!(error.is_fatal());
        assert!(
            matches!(error, LoadError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn conflicting_name_is_rejected_before_io_and_cannot_bind_another_version() {
        let directory = tmp();
        let first = [named(&write(&directory, "first.xml", QF_44), "venue")];
        let second = [named(
            &directory.join("missing.xml").to_string_lossy(),
            "venue",
        )];
        let layers = [
            DictionaryLayer {
                specs: &first,
                source: "user.toml:4",
                root_dir: None,
            },
            DictionaryLayer {
                specs: &second,
                source: "project.toml:8",
                root_dir: None,
            },
        ];
        let store = DictionaryStore::default();
        let (registry, events) = store
            .build_registry(&layers, DEFAULT_VERSION, &ctx(None, None))
            .unwrap();
        assert_eq!(registry.definitions().len(), 1);
        assert!(matches!(events.as_slice(), [RegistryEvent::BadName { .. }]));
        assert!(events[0].is_error());
        assert!(events[0].message().contains("user.toml:4"));
        assert!(events[0].message().contains("project.toml:8"));
        let changed = write(
            &directory,
            "second.xml",
            &QF_44_OTHER.replace("major='4' minor='4'", "major='9' minor='1'"),
        );
        let duplicate = [named(&changed, "venue")];
        let extended_layers = [
            DictionaryLayer {
                specs: &first,
                source: "user.toml:4",
                root_dir: None,
            },
            DictionaryLayer {
                specs: &second,
                source: "project.toml:8",
                root_dir: None,
            },
            DictionaryLayer {
                specs: &duplicate,
                source: "child",
                root_dir: None,
            },
        ];
        let (extended, events) = store
            .build_registry(&extended_layers, DEFAULT_VERSION, &ctx(None, None))
            .unwrap();
        assert!(events[0].is_error());
        assert!(extended.resolve("FIX.9.1").is_none());
        assert_eq!(extended.identity(), registry.identity());
    }

    #[test]
    fn stronger_distinct_name_can_replace_a_version_default_without_rebinding_bundled_name() {
        let directory = tmp();
        let first = [named(&write(&directory, "first.xml", QF_44), "parent")];
        let second = [named(
            &write(&directory, "second.xml", QF_44_OTHER),
            "child",
        )];
        let layers = [
            DictionaryLayer {
                specs: &first,
                source: "parent.toml",
                root_dir: None,
            },
            DictionaryLayer {
                specs: &second,
                source: "child.toml",
                root_dir: None,
            },
        ];
        let (registry, events) = DictionaryStore::default()
            .build_registry(&layers, DEFAULT_VERSION, &ctx(None, None))
            .unwrap();
        assert!(events.is_empty());
        assert!(has_tag(&registry.resolve("FIX.4.4").unwrap(), 9998));
        assert!(has_tag(&registry.named("parent").unwrap(), 9999));
        assert!(registry.named("FIX.4.4").unwrap().is_static());
        assert!(registry.choice("FIX.2.7").is_none());
        assert!(is_reserved_selector("FIX.2.7"));
    }

    #[test]
    fn a_rebuild_changes_name_and_default_together() {
        let directory = tmp();
        let selected = write(&directory, "selected.xml", QF_44);
        let unaffected = write(&directory, "unaffected.xml", QF_44);
        let second = DictionarySpec {
            default: false,
            ..named(&unaffected, "unaffected")
        };
        let store = DictionaryStore::default();
        let specs = [named(&selected, "selected"), second];
        let layer = |specs| {
            [DictionaryLayer {
                specs,
                source: "client",
                root_dir: None,
            }]
        };
        let (registry, events) = store
            .build_registry(&layer(&specs), DEFAULT_VERSION, &ctx(None, None))
            .unwrap();
        assert!(events.is_empty());
        write(&directory, "selected.xml", QF_44_OTHER);
        let (updated, events) = store
            .build_registry(&layer(&specs), DEFAULT_VERSION, &ctx(None, None))
            .unwrap();
        assert!(events.is_empty());
        assert!(updated
            .named("selected")
            .unwrap()
            .ptr_eq(&updated.resolve("FIX.4.4").unwrap()));
        assert!(registry
            .named("unaffected")
            .unwrap()
            .ptr_eq(&updated.named("unaffected").unwrap()));
        assert_ne!(updated.identity(), registry.identity());
        assert!(has_tag(&registry.named("selected").unwrap(), 9999));
        assert!(has_tag(&updated.named("selected").unwrap(), 9998));
    }

    #[test]
    fn invalid_xml_version_is_not_repaired_by_the_filename() {
        let directory = tmp();
        for version in ["", "banana", "FIX.x.4", "FIX.4.4/path", "FIX.4.4SP"] {
            let path = write(
                &directory,
                "FIX44.xml",
                &format!("<fix version='{version}'><fields/></fix>"),
            );
            assert!(matches!(
                load_err(&spec(&path)),
                LoadError::InvalidVersion { .. }
            ));
        }
    }

    #[test]
    fn complete_rebuild_reuses_unchanged_project_xml() {
        let directory = tmp();
        let global_path = write(&directory, "global.xml", QF_44);
        let project_path = write(&directory, "project.xml", QF_44_OTHER);
        let global_specs = [named(&global_path, "global")];
        let project_specs = [named(&project_path, "project")];
        let layers = [
            DictionaryLayer {
                specs: &global_specs,
                source: "client",
                root_dir: None,
            },
            DictionaryLayer {
                specs: &project_specs,
                source: "project.toml",
                root_dir: None,
            },
        ];
        let store = DictionaryStore::default();
        let (previous, events) = store
            .build_registry(&layers, DEFAULT_VERSION, &ctx(None, None))
            .unwrap();
        assert!(events.is_empty());
        let project_choice = previous.choice("project").unwrap();
        write(
            &directory,
            "global.xml",
            &QF_44.replace("Custom", "Updated"),
        );
        let (updated, events) = store
            .build_registry(&layers, DEFAULT_VERSION, &ctx(None, None))
            .unwrap();
        assert!(events.is_empty());
        assert!(updated
            .named("project")
            .unwrap()
            .ptr_eq(&project_choice.dict));
        assert!(updated
            .resolve("FIX.4.4")
            .unwrap()
            .ptr_eq(&project_choice.dict));
        assert_eq!(store.parse_count(), 3);
    }

    // --- the name hover calls a custom dictionary by ----------------------

    fn names(registry: Registry, choice: Option<DictionaryChoice>) -> crate::ctx::DictionaryNames {
        crate::ctx::DictionaryNames::new(Arc::new(registry), choice.as_ref())
    }

    /// A selected dictionary answers only the versions it is compatible with:
    /// a FIX.4.2 line is still decoded by, and named after, the FIX.4.2 default.
    #[test]
    fn each_version_names_the_dictionary_that_answers_it() {
        let directory = tmp();
        let venue = write(&directory, "venue.xml", QF_44);
        let desk = write(&directory, "desk.xml", QF_44_OTHER);
        let venue = named(&versioned(&venue, "FIX.4.2").path, "venue42");
        let desk = DictionarySpec {
            default: false,
            ..named(&desk, "desk")
        };
        let (registry, events) = build(&[venue, desk]);
        assert!(events.is_empty(), "{events:?}");
        let chosen = registry.choice("desk");
        let names = names(registry, chosen);
        assert_eq!(names.of("FIX.4.4"), Some("desk"));
        assert_eq!(names.of("FIX.4.2"), Some("venue42"));
        assert_eq!(names.of("FIX.4.0"), None, "bundled tables have no name");
    }

    /// A custom FIX.5.0SP2 default is what `8=FIXT.1.1` reads, so it names it.
    #[test]
    fn the_fixt_alias_is_named_after_the_tables_it_reads() {
        let directory = tmp();
        let path = write(&directory, "session.xml", QF_44);
        let (registry, events) = build(&[named(&versioned(&path, "FIX.5.0SP2").path, "venue")]);
        assert!(events.is_empty(), "{events:?}");
        let names = names(registry, None);
        assert_eq!(names.of("FIXT.1.1"), Some("venue"));
        assert_eq!(names.of("FIX.5.0SP2"), Some("venue"));
    }

    /// Both names share one parsed file; each is named as it was selected.
    #[test]
    fn one_file_under_two_names_keeps_both_names() {
        let directory = tmp();
        let path = write(&directory, "shared.xml", QF_44);
        let alpha = named(&path, "alpha");
        let beta = DictionarySpec {
            default: false,
            ..named(&path, "beta")
        };
        let (registry, events) = build(&[alpha, beta]);
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(registry.default_name("FIX.4.4"), Some("alpha"));
        let beta = registry.choice("beta").expect("defined");
        assert!(
            beta.dict.ptr_eq(&registry.resolve("FIX.4.4").unwrap()),
            "one parse"
        );
        // The name a rename shows follows at once: `DictionaryNames` is built
        // from the selection in force, not from the reusable `Ctx`.
        assert_eq!(names(registry, Some(beta)).of("FIX.4.4"), Some("beta"));
    }

    /// A dictionary chosen by path has no name: its last path component
    /// stands in, and the full path never does.
    #[test]
    fn a_path_selection_is_named_by_its_last_component() {
        let directory = tmp();
        let file = write(&directory, "desk.xml", QF_44);
        let (registry, _) = build(&[]);
        let store = DictionaryStore::default();
        let choice = store.load_choice(&file, &ctx(None, None)).expect("loads");
        assert_eq!(
            names(registry.clone(), Some(choice)).of("FIX.4.4"),
            Some("desk.xml")
        );

        let repository =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("xml/custom/synthetic/repository/FIX.4.4");
        let choice = store
            .load_choice(&repository.to_string_lossy(), &ctx(None, None))
            .expect("loads");
        assert_eq!(names(registry, Some(choice)).of("FIX.4.4"), Some("FIX.4.4"));
    }
}
