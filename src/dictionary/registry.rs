//! Registry construction is pure except for the bounded, reusable XML loader.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::SystemTime;

use xxhash_rust::xxh3::{xxh3_64, Xxh3};

use super::binding::{is_begin_string, requires_fixt, to_wire, FIXT_1_1};
pub use super::binding::{ApplicationIdentity, ApplicationKey, DictionarySources, SourceRole};
use super::dict::{bundled, Dict, MessageView, Version};
use super::model::{Fault, ParsedDict};
use super::parse::{self, ParseError, Source};
use super::statics::DICTS_BUILD_HASH;
use crate::config::{DictionarySpec, Limits};

/// One problem that a registry build reports as a configuration warning or error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryEvent {
    /// The dictionary file did not load. The entry is not registered.
    LoadFailed { path: String, detail: String },
    /// The dictionary loaded, with one defect in it; `error` as
    /// `Fault::is_error`.
    DictionaryFault {
        path: String,
        detail: String,
        error: bool,
    },
    /// The faults past `MAX_FAULT_EVENTS` for one dictionary, counted; an
    /// error when any of them is.
    MoreDictionaryFaults {
        path: String,
        count: usize,
        error: bool,
    },
    /// The sources or the identity of a binding are not valid together. The
    /// entry is not registered.
    InvalidBinding { name: String, detail: String },
    /// The default claim of a binding conflicts with another claim. The server
    /// disables the claim, but the binding stays selectable by name.
    DefaultConflict {
        name: String,
        /// BeginString(8) value of the disabled claim.
        begin_string: String,
        detail: String,
    },
    /// An event from one configuration layer, with the name of that layer.
    AtSource {
        source: String,
        event: Box<RegistryEvent>,
    },
    /// The name is the same as an earlier name or a bundled identifier. The
    /// entry is not registered.
    BadName { name: String, detail: String },
    /// The entry has a blank name and is not registered.
    BlankName {
        /// Description of the entry sources, because the entry has no name.
        path: String,
    },
}

impl RegistryEvent {
    /// The configuration layer that the event comes from, if the event records it.
    pub fn source(&self) -> Option<&str> {
        match self {
            Self::AtSource { source, .. } => Some(source),
            _ => None,
        }
    }

    pub fn message(&self) -> String {
        self.message_in(None)
    }

    /// `layer` names the configuration layer in the messages of binding events.
    fn message_in(&self, layer: Option<&str>) -> String {
        let in_layer = layer
            .map(|layer| format!(" in {layer}"))
            .unwrap_or_default();
        match self {
            Self::LoadFailed { path, detail } => {
                format!("dictionary {path:?} not loaded: {detail}")
            }
            Self::DictionaryFault {
                path,
                detail,
                error,
            } => {
                format!(
                    "dictionary {path:?} loaded with {}: {detail}",
                    a_fault(*error)
                )
            }
            Self::MoreDictionaryFaults { path, count, error } => {
                format!(
                    "dictionary {path:?} loaded with {}: … and {count} more",
                    a_fault(*error)
                )
            }
            Self::InvalidBinding { name, detail } => {
                format!("dictionary binding {name:?}{in_layer} rejected: {detail}")
            }
            Self::DefaultConflict {
                name,
                begin_string,
                detail,
            } => {
                format!(
                    "dictionary default {name:?}{in_layer} disabled for {begin_string}: {detail}"
                )
            }
            Self::AtSource { source, event } => event.message_in(Some(source)),
            Self::BadName { name, detail } => {
                format!("dictionary name {name:?} rejected: {detail}")
            }
            Self::BlankName { path } => {
                format!("dictionary {path:?} has a blank name; the entry is ignored")
            }
        }
    }

    pub fn is_error(&self) -> bool {
        if let Self::AtSource { event, .. } = self {
            return event.is_error();
        }
        if let Self::DictionaryFault { error, .. } | Self::MoreDictionaryFaults { error, .. } = self
        {
            return *error;
        }
        matches!(
            self,
            Self::LoadFailed { .. }
                | Self::InvalidBinding { .. }
                | Self::DefaultConflict { .. }
                | Self::BadName { .. }
                | Self::BlankName { .. }
        )
    }
}

/// Beyond this many, one dictionary's faults are counted rather than listed:
/// a generated file can repeat one mistake hundreds of times.
const MAX_FAULT_EVENTS: usize = 20;

fn a_fault(error: bool) -> &'static str {
    if error {
        "an error"
    } else {
        "a warning"
    }
}

/// The events reporting the faults `path` loaded with: errors first, each
/// kind in the faults' own sorted order.
pub fn fault_events(path: &str, faults: &[Fault]) -> Vec<RegistryEvent> {
    let (errors, warnings): (Vec<&Fault>, Vec<&Fault>) =
        faults.iter().partition(|fault| fault.is_error());
    let ordered: Vec<&Fault> = errors.into_iter().chain(warnings).collect();
    let (listed, hidden) = ordered.split_at(ordered.len().min(MAX_FAULT_EVENTS));
    let mut events: Vec<RegistryEvent> = listed
        .iter()
        .map(|fault| RegistryEvent::DictionaryFault {
            path: path.to_owned(),
            detail: fault.to_string(),
            error: fault.is_error(),
        })
        .collect();
    if !hidden.is_empty() {
        events.push(RegistryEvent::MoreDictionaryFaults {
            path: path.to_owned(),
            count: hidden.len(),
            error: hidden.iter().any(|fault| fault.is_error()),
        });
    }
    events
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

/// Resolved transport and optional application binding, retaining independent sources.
#[derive(Clone)]
pub struct DictionaryChoice {
    /// Application source when `application` is set; otherwise the transport source.
    pub dict: Dict,
    /// Bundled identifier or canonical file path of `dict`.
    pub source: String,
    /// Hash of source identities and application metadata; selection names do not affect it.
    pub identity: u64,
    pub name: Option<String>,
    /// Header, trailer and administrative source; conventional FIX4 shares it with `dict`.
    pub transport: Dict,
    /// Exact release/extension/custom identity; `None` means no application source.
    pub application: Option<ApplicationIdentity>,
    /// Bundled identifier or canonical file path of `transport`.
    pub transport_source: String,
}

impl DictionaryChoice {
    pub fn sources(&self) -> DictionarySources {
        DictionarySources {
            transport: Some(self.transport.clone()),
            application: self.application.as_ref().map(|_| self.dict.clone()),
        }
    }

    pub fn source(&self, role: SourceRole) -> Option<&Dict> {
        match role {
            SourceRole::Transport => Some(&self.transport),
            SourceRole::Application => self.application.as_ref().map(|_| &self.dict),
        }
    }

    pub fn application_version(&self) -> Option<&str> {
        self.application
            .as_ref()
            .map(|identity| identity.version.as_str())
    }

    /// Canonical application release, or transport BeginString(8) for a transport-only choice.
    pub fn version(&self) -> &str {
        self.application_version()
            .unwrap_or_else(|| self.transport.version().expect("transport version"))
    }
}

pub fn is_reserved_selector(name: &str) -> bool {
    Version::is_version_identifier(name) || to_wire(name).is_some()
}

#[derive(Clone)]
pub struct DictionaryDefinition {
    pub spec: DictionarySpec,
    pub source: String,
    pub choice: DictionaryChoice,
    /// Position of the declaring layer, weakest first.
    priority: usize,
}

#[derive(Clone)]
pub struct Registry {
    defaults: HashMap<DefaultKey, DictionaryChoice>,
    transport_identities: HashMap<String, u64>,
    default_names: HashMap<u64, String>,
    transport_defaults: HashMap<String, DictionaryChoice>,
    pairs: HashMap<String, Vec<DictionaryChoice>>,
    by_name: HashMap<String, DictionaryChoice>,
    definitions: Vec<DictionaryDefinition>,
    identity: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DefaultKey {
    begin_string: String,
    application: Option<ApplicationIdentity>,
}

impl DefaultKey {
    fn of(choice: &DictionaryChoice) -> Self {
        Self {
            begin_string: choice
                .transport
                .version()
                .expect("transport version")
                .to_owned(),
            application: choice.application.clone(),
        }
    }
}

#[derive(Clone)]
struct LoadedSource {
    dict: Dict,
    source: String,
    identity: u64,
}

// Shared across registry layers without retaining unused dictionaries.
type ParsedCache = HashMap<FileStamp, (u64, Weak<ParsedDict>)>;

#[derive(Default)]
pub struct DictionaryStore {
    cache: Mutex<ParsedCache>,
    #[cfg(test)]
    parse_count: AtomicUsize,
}

/// A dictionary file as `plan` stat-checked it; the cache key, so a change
/// to any of it is a re-read.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct FileStamp {
    path: PathBuf,
    len: u64,
    mtime: SystemTime,
}

impl Registry {
    pub fn bundled_only() -> Registry {
        let mut registry = Registry {
            defaults: HashMap::new(),
            transport_identities: HashMap::new(),
            default_names: HashMap::new(),
            transport_defaults: Version::ALL
                .into_iter()
                .filter(|version| is_begin_string(version.as_str()))
                .map(|version| {
                    (
                        version.as_str().to_owned(),
                        make_choice(bundled_source(version), None, None, None),
                    )
                })
                .collect(),
            pairs: HashMap::new(),
            by_name: HashMap::new(),
            definitions: Vec::new(),
            identity: empty_identity(),
        };
        registry.rebuild_pairs();
        registry
    }

    #[cfg(test)]
    pub fn build(
        specs: &[DictionarySpec],
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
            ctx,
        ) {
            Ok((registry, events)) => (
                registry,
                events
                    .into_iter()
                    .map(|event| match event {
                        RegistryEvent::AtSource { event, .. } => *event,
                        event => event,
                    })
                    .collect(),
            ),
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
        ctx: &BuildContext<'_>,
    ) -> Result<(Registry, Vec<RegistryEvent>), LoadError> {
        let mut registry = Registry::bundled_only();
        let mut events = Vec::new();
        let mut claims: HashMap<DefaultKey, Vec<usize>> = HashMap::new();
        for (priority, layer) in layers.iter().enumerate() {
            let layer_ctx = BuildContext {
                root_dir: layer.root_dir,
                home_dir: ctx.home_dir,
                limits: ctx.limits,
            };
            let mut layer_claims: HashMap<DefaultKey, Vec<usize>> = HashMap::new();
            let mut layer_events = Vec::new();
            for spec in layer.specs {
                if spec.name.trim().is_empty() {
                    layer_events.push(RegistryEvent::BlankName {
                        path: spec.source_description(),
                    });
                    continue;
                }
                if let Some(detail) = registry.name_problem(&spec.name, layer.source) {
                    layer_events.push(RegistryEvent::BadName {
                        name: spec.name.clone(),
                        detail,
                    });
                    continue;
                }
                let (choice, transport_identity) =
                    match self.registration(spec, &layer_ctx, &mut layer_events) {
                        Ok(loaded) => loaded,
                        Err(error @ LoadError::Io { .. }) => return Err(error),
                        Err(LoadError::InvalidBinding { detail }) => {
                            layer_events.push(RegistryEvent::InvalidBinding {
                                name: spec.name.clone(),
                                detail,
                            });
                            continue;
                        }
                        Err(error) => {
                            layer_events.push(RegistryEvent::LoadFailed {
                                path: match &error {
                                    LoadError::Source { path, .. } => path.clone(),
                                    _ => spec.source_description(),
                                },
                                detail: error.to_string(),
                            });
                            continue;
                        }
                    };
                if spec.default {
                    layer_claims
                        .entry(DefaultKey::of(&choice))
                        .or_default()
                        .push(registry.definitions.len());
                }
                registry
                    .transport_identities
                    .insert(choice.transport_source.clone(), transport_identity);
                registry.by_name.insert(spec.name.clone(), choice.clone());
                registry.definitions.push(DictionaryDefinition {
                    spec: spec.clone(),
                    source: layer.source.to_owned(),
                    choice,
                    priority,
                });
            }
            // A stronger layer replaces a weaker claim before conflicts are checked.
            claims.extend(layer_claims);
            events.extend(
                layer_events
                    .into_iter()
                    .map(|event| RegistryEvent::AtSource {
                        source: layer.source.to_owned(),
                        event: Box::new(event),
                    }),
            );
        }
        registry.apply_defaults(claims, &mut events);
        registry.rebuild_pairs();
        registry.update_identity();
        Ok((registry, events))
    }

    fn source(
        &self,
        source: &str,
        ctx: &BuildContext<'_>,
        events: &mut Vec<RegistryEvent>,
    ) -> Result<LoadedSource, LoadError> {
        if let Some(version) = Version::parse(source) {
            return Ok(bundled_source(version));
        }
        if is_reserved_selector(source) {
            return Err(LoadError::InvalidBinding {
                detail: format!("{source} has no bundled dictionary; supply a public XML source"),
            });
        }
        let (stamp, identity, parsed) = self.load(source, ctx).map_err(|error| {
            if error.is_fatal() {
                error
            } else {
                LoadError::Source {
                    path: source.to_owned(),
                    error: Box::new(error),
                }
            }
        })?;
        for event in fault_events(source, &parsed.faults) {
            if !events.contains(&event) {
                events.push(event);
            }
        }
        let source = display_path(&stamp);
        let mut physical_identity = Xxh3::new();
        physical_identity.update(source.as_bytes());
        physical_identity.update(&identity.to_le_bytes());
        Ok(LoadedSource {
            dict: Dict::Owned(parsed),
            source,
            identity: physical_identity.digest(),
        })
    }

    fn registration(
        &self,
        spec: &DictionarySpec,
        ctx: &BuildContext<'_>,
        events: &mut Vec<RegistryEvent>,
    ) -> Result<(DictionaryChoice, u64), LoadError> {
        spec.validate_shape()
            .map_err(|detail| LoadError::InvalidBinding { detail })?;
        let (transport, application) = if let Some(path) = &spec.path {
            let loaded = self.source(path, ctx, events)?;
            let version = loaded.dict.version().expect("validated XML version");
            if version == FIXT_1_1 {
                if spec.application_version.is_some() {
                    (loaded.clone(), Some(loaded))
                } else {
                    if let Dict::Owned(parsed) = &loaded.dict {
                        if parsed
                            .messages
                            .iter()
                            .any(|message| !MessageView::Owned(message).is_session())
                        {
                            return Err(LoadError::InvalidBinding {
                                detail: "combined FIXT source requires applicationVersion".into(),
                            });
                        }
                    }
                    (loaded, None)
                }
            } else if requires_fixt(version) {
                (bundled_source(Version::Fixt11), Some(loaded))
            } else {
                (loaded.clone(), Some(loaded))
            }
        } else {
            let transport = self.source(
                spec.transport.as_deref().expect("validated shape"),
                ctx,
                events,
            )?;
            let application = spec
                .application
                .as_deref()
                .map(|source| self.source(source, ctx, events))
                .transpose()?;
            (transport, application)
        };
        let begin_string = transport.dict.version().expect("transport version");
        if !is_begin_string(begin_string) {
            return Err(LoadError::InvalidBinding {
                detail: format!("{begin_string} is not a transport BeginString"),
            });
        }
        let identity = if let Some(application) = &application {
            let declared = application.dict.version().expect("application version");
            let version = spec.application_version.as_deref().unwrap_or(declared);
            if to_wire(version).is_none() {
                return Err(LoadError::InvalidBinding {
                    detail: "application source requires a canonical applicationVersion".into(),
                });
            }
            if declared != FIXT_1_1 && declared != version {
                return Err(LoadError::InvalidBinding {
                    detail: format!(
                        "applicationVersion {version} contradicts XML version {declared}"
                    ),
                });
            }
            if begin_string != FIXT_1_1
                && begin_string != version
                && !(begin_string == "FIX.4.4" && spec.transport.is_some())
            {
                return Err(LoadError::InvalidBinding {
                    detail: format!(
                        "application {version} requires FIXT.1.1 or an explicit FIX.4.4 profile"
                    ),
                });
            }
            Some(ApplicationIdentity {
                version: version.to_owned(),
                extension_pack: spec.extension_pack,
                custom_version: spec.custom_version.clone(),
            })
        } else {
            if spec.application_version.is_some()
                || spec.extension_pack.is_some()
                || spec.custom_version.is_some()
            {
                return Err(LoadError::InvalidBinding {
                    detail: "application identity requires an application source".into(),
                });
            }
            None
        };
        let transport_identity = transport.identity;
        Ok((
            make_choice(transport, application, identity, Some(spec.name.clone())),
            transport_identity,
        ))
    }

    pub fn load_choice(
        &self,
        path: &str,
        ctx: &BuildContext<'_>,
    ) -> Result<DictionaryChoice, LoadError> {
        let (mut choice, _) = self.registration(
            &DictionarySpec {
                path: Some(path.to_owned()),
                name: "direct-path".into(),
                ..DictionarySpec::default()
            },
            ctx,
            &mut Vec::new(),
        )?;
        choice.name = None;
        Ok(choice)
    }

    fn load(
        &self,
        path: &str,
        ctx: &BuildContext<'_>,
    ) -> Result<(FileStamp, u64, Arc<ParsedDict>), LoadError> {
        load(path, ctx, self)
    }

    #[cfg(test)]
    pub fn parse_count(&self) -> usize {
        self.parse_count.load(Ordering::Relaxed)
    }
}

impl Registry {
    pub fn choice(&self, name: &str) -> Option<DictionaryChoice> {
        if let Some(version) = Version::parse(name) {
            let source = bundled_source(version);
            let choice = if version == Version::Fixt11 {
                make_choice(source, None, None, Some(name.to_owned()))
            } else {
                let transport = if requires_fixt(name) {
                    bundled_source(Version::Fixt11)
                } else {
                    source.clone()
                };
                make_choice(
                    transport,
                    Some(source),
                    Some(ApplicationIdentity::release(name)),
                    Some(name.to_owned()),
                )
            };
            return Some(choice);
        }
        self.by_name.get(name).cloned()
    }

    /// Binary search keeps profile lookup allocation-free as custom defaults grow.
    pub fn default_pair<'a>(
        &self,
        begin_string: &str,
        application: impl Into<ApplicationKey<'a>>,
    ) -> Option<&DictionaryChoice> {
        let application = Some(application.into());
        let pairs = self.pairs.get(begin_string)?;
        let index = pairs
            .binary_search_by(|choice| {
                choice
                    .application
                    .as_ref()
                    .map(ApplicationIdentity::key)
                    .cmp(&application)
            })
            .ok()?;
        pairs.get(index)
    }

    pub fn default_transport(&self, begin_string: &str) -> Option<&DictionaryChoice> {
        self.transport_defaults.get(begin_string)
    }

    fn rebuild_pairs(&mut self) {
        let mut pairs: HashMap<String, HashMap<ApplicationIdentity, DictionaryChoice>> =
            HashMap::new();
        for version in Version::ALL {
            let application_version = version.as_str();
            if to_wire(application_version).is_none() {
                continue;
            }
            let application_identity = ApplicationIdentity::release(application_version);
            let mut begin_strings = vec![FIXT_1_1];
            if is_begin_string(application_version) {
                begin_strings.push(application_version);
            }
            for begin_string in begin_strings {
                let family = self.defaults.get(&DefaultKey {
                    begin_string: begin_string.to_owned(),
                    application: None,
                });
                let transport = match family {
                    Some(family) => self.transport_source(family),
                    None => {
                        bundled_source(Version::parse(begin_string).expect("bundled transport"))
                    }
                };
                let application_source = if begin_string == application_version {
                    transport.clone()
                } else {
                    bundled_source(version)
                };
                let choice = make_choice(
                    transport,
                    Some(application_source),
                    Some(application_identity.clone()),
                    None,
                );
                pairs
                    .entry(begin_string.to_owned())
                    .or_default()
                    .insert(application_identity.clone(), choice);
            }
        }
        for (key, choice) in &self.defaults {
            if let Some(application) = &key.application {
                pairs
                    .entry(key.begin_string.clone())
                    .or_default()
                    .insert(application.clone(), choice.clone());
            }
        }
        self.pairs = pairs
            .into_iter()
            .map(|(begin_string, pairs)| {
                let mut choices: Vec<_> = pairs.into_values().collect();
                choices.sort_by(|left, right| left.application.cmp(&right.application));
                (begin_string, choices)
            })
            .collect();
    }

    fn transport_source(&self, choice: &DictionaryChoice) -> LoadedSource {
        LoadedSource {
            dict: choice.transport.clone(),
            source: choice.transport_source.clone(),
            identity: self.transport_identities[&choice.transport_source],
        }
    }

    pub fn definitions(&self) -> &[DictionaryDefinition] {
        &self.definitions
    }

    // Compatibility for consumers migrating to exact pair resolution.
    #[cfg(test)]
    pub fn resolve(&self, version: &str) -> Option<Dict> {
        if version == FIXT_1_1 {
            return self
                .default_transport(version)
                .map(|choice| choice.dict.clone());
        }
        let begin_string = super::binding::transport_begin_string(version);
        self.default_pair(begin_string, &ApplicationIdentity::release(version))
            .map(|choice| choice.dict.clone())
    }

    pub fn default_name_for_identity(&self, identity: u64) -> Option<&str> {
        self.default_names.get(&identity).map(String::as_str)
    }

    pub fn default_names(&self) -> &HashMap<u64, String> {
        &self.default_names
    }

    /// Bindings reachable without an explicit dictionary selection.
    pub(crate) fn default_binding_identities(&self) -> impl Iterator<Item = u64> + '_ {
        self.transport_defaults
            .values()
            .chain(self.pairs.values().flat_map(|pairs| pairs.iter()))
            .map(|choice| choice.identity)
    }
    #[cfg(test)]
    pub fn default_name(&self, version: &str) -> Option<&str> {
        let begin_string = super::binding::transport_begin_string(version);
        let choice = if version == FIXT_1_1 {
            self.default_transport(version)
        } else {
            self.default_pair(begin_string, &ApplicationIdentity::release(version))
        }?;
        self.default_name_for_identity(choice.identity)
    }
    #[cfg(test)]
    pub fn bindings(&self) -> Vec<(&str, Dict)> {
        let mut bindings: Vec<_> = Version::ALL
            .into_iter()
            .filter_map(|version| {
                self.resolve(version.as_str())
                    .map(|dict| (version.as_str(), dict))
            })
            .collect();
        for key in self.defaults.keys() {
            let version = key
                .application
                .as_ref()
                .map_or(key.begin_string.as_str(), |application| {
                    application.version.as_str()
                });
            if !bindings.iter().any(|(bound, _)| *bound == version) {
                if let Some(dict) = self.resolve(version) {
                    bindings.push((version, dict));
                }
            }
        }
        bindings
    }
    #[cfg(test)]
    pub fn named(&self, name: &str) -> Option<Dict> {
        self.choice(name).map(|choice| choice.dict)
    }
    pub fn identity(&self) -> u64 {
        self.identity
    }

    fn apply_defaults(
        &mut self,
        claims: HashMap<DefaultKey, Vec<usize>>,
        events: &mut Vec<RegistryEvent>,
    ) {
        let mut disabled = HashSet::new();
        let mut reasons = HashMap::new();
        for indices in claims.values() {
            if indices.len() > 1 {
                for index in indices {
                    disabled.insert(*index);
                    reasons.insert(*index, "competing defaults for the same exact identity");
                }
            }
        }
        let mut transports: HashMap<&str, Vec<usize>> = HashMap::new();
        for index in claims
            .values()
            .flatten()
            .copied()
            .filter(|index| !disabled.contains(index))
        {
            let begin_string = self.definitions[index]
                .choice
                .transport
                .version()
                .expect("transport version");
            transports.entry(begin_string).or_default().push(index);
        }
        for indices in transports.values() {
            let first = &self.definitions[indices[0]].choice.transport;
            if indices
                .iter()
                .any(|index| !first.ptr_eq(&self.definitions[*index].choice.transport))
            {
                for index in indices {
                    disabled.insert(*index);
                    reasons.insert(
                        *index,
                        "different physical transport sources for one BeginString",
                    );
                }
            }
        }
        let mut conflicting: Vec<_> = disabled.iter().copied().collect();
        conflicting.sort_unstable();
        for index in conflicting {
            let entry = &self.definitions[index];
            events.push(RegistryEvent::AtSource {
                source: entry.source.clone(),
                event: Box::new(RegistryEvent::DefaultConflict {
                    name: entry.spec.name.clone(),
                    begin_string: entry
                        .choice
                        .transport
                        .version()
                        .expect("transport version")
                        .to_owned(),
                    detail: reasons[&index].to_owned(),
                }),
            });
        }
        let mut transport_claims: HashMap<String, Vec<usize>> = HashMap::new();
        for (key, indices) in &claims {
            for index in indices {
                if !disabled.contains(index) {
                    transport_claims
                        .entry(key.begin_string.clone())
                        .or_default()
                        .push(*index);
                }
            }
        }
        for (begin_string, indices) in transport_claims {
            let index = indices
                .into_iter()
                .min_by_key(|index| {
                    let entry = &self.definitions[*index];
                    (
                        entry.choice.application.is_some(),
                        std::cmp::Reverse(self.definitions[*index].priority),
                        entry.spec.name.as_str(),
                    )
                })
                .expect("effective transport claim");
            let choice = &self.definitions[index].choice;
            let projected = make_choice(
                self.transport_source(choice),
                None,
                None,
                choice.name.clone(),
            );
            self.default_names.insert(
                projected.identity,
                projected.name.clone().expect("registered name"),
            );
            self.transport_defaults.insert(begin_string, projected);
        }
        for (key, indices) in claims {
            if indices.iter().any(|index| disabled.contains(index)) {
                continue;
            }
            let choice = self.definitions[indices[0]].choice.clone();
            self.default_names.insert(
                choice.identity,
                choice.name.clone().expect("registered name"),
            );
            self.defaults.insert(key, choice);
        }
    }

    fn update_identity(&mut self) {
        let mut sources: Vec<_> = self
            .by_name
            .values()
            .map(|choice| choice.identity)
            .collect();
        sources.sort_unstable();
        sources.dedup();
        let mut defaults: Vec<_> = self
            .defaults
            .iter()
            .map(|(key, choice)| (format!("{key:?}"), choice.identity))
            .collect();
        defaults.sort_unstable();
        let mut identity = Xxh3::new();
        for content in sources {
            identity.update(&content.to_le_bytes());
        }
        for (key, content) in defaults {
            bind(&mut identity, b'd', &key, content);
        }
        identity.update(&DICTS_BUILD_HASH.to_le_bytes());
        self.identity = identity.digest();
    }

    fn name_problem(&self, name: &str, source: &str) -> Option<String> {
        if is_reserved_selector(name) {
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
                    entry.source,
                    entry.spec.source_description()
                )
            })
    }
}

fn bundled_source(version: Version) -> LoadedSource {
    let source = version.as_str().to_owned();
    let mut identity = Xxh3::new();
    identity.update(&DICTS_BUILD_HASH.to_le_bytes());
    identity.update(source.as_bytes());
    LoadedSource {
        dict: Dict::Static(bundled(version)),
        source,
        identity: identity.digest(),
    }
}

fn make_choice(
    transport: LoadedSource,
    application: Option<LoadedSource>,
    application_identity: Option<ApplicationIdentity>,
    name: Option<String>,
) -> DictionaryChoice {
    let mut identity = Xxh3::new();
    identity.update(&transport.identity.to_le_bytes());
    if let Some(application) = &application {
        identity.update(&application.identity.to_le_bytes());
    }
    identity.update(format!("{application_identity:?}").as_bytes());
    let dict = application
        .as_ref()
        .map_or(&transport.dict, |application| &application.dict)
        .clone();
    let source = application.map_or_else(
        || transport.source.clone(),
        |application| application.source,
    );
    DictionaryChoice {
        dict,
        source,
        identity: identity.digest(),
        name,
        transport: transport.dict,
        application: application_identity,
        transport_source: transport.source,
    }
}

fn display_path(stamp: &FileStamp) -> String {
    stamp.path.to_string_lossy().into_owned()
}

/// Why one `fix.dictionaries` entry could not be loaded. Rendered into the
/// `RegistryEvent::LoadFailed` text the client sees.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("invalid dictionary binding: {detail}")]
    InvalidBinding { detail: String },
    #[error("{error}")]
    Source { path: String, error: Box<LoadError> },
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
    #[error("{path}: is a directory; a dictionary is one QuickFIX XML file")]
    Directory { path: PathBuf },
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

/// Resolve the path and stat the file the entry would read — once, and
/// without opening it.
fn plan(raw: &str, ctx: &BuildContext<'_>) -> Result<FileStamp, LoadError> {
    let path = resolve_path(raw, ctx)?;
    let meta = fs::metadata(&path).map_err(|e| LoadError::io(&path, e))?;
    // Refused for this entry alone: an `Io` error would abort the whole build.
    if meta.is_dir() {
        return Err(LoadError::Directory { path });
    }
    stamp_from(&path, &meta, ctx.limits.dictionary_bytes)
}

/// Reuse live parsed XML only after checking current metadata and limits.
fn load(
    raw: &str,
    ctx: &BuildContext<'_>,
    store: &DictionaryStore,
) -> Result<(FileStamp, u64, Arc<ParsedDict>), LoadError> {
    let stamp = plan(raw, ctx)?;
    let mut cache = store.cache.lock().expect("dictionary cache lock");
    if let Some((content, hit)) = cache.get(&stamp) {
        if let Some(hit) = hit.upgrade() {
            return Ok((stamp, *content, hit));
        }
    }
    cache.retain(|_, (_, parsed)| parsed.strong_count() != 0);
    drop(cache);
    let source = read_stamped(&stamp, ctx.limits.dictionary_bytes)?;
    let identity = xxh3_64(&source.bytes);
    let parsed = parse::parse_quickfix_bytes(&source)?;
    let version = parsed
        .version
        .as_deref()
        .ok_or_else(|| LoadError::NoVersion {
            path: stamp.path.clone(),
        })?;
    if !valid_declared_version(version) {
        return Err(LoadError::InvalidVersion {
            path: stamp.path.clone(),
            version: version.to_owned(),
        });
    }
    let parsed = Arc::new(parsed);
    #[cfg(test)]
    store.parse_count.fetch_add(1, Ordering::Relaxed);
    store
        .cache
        .lock()
        .expect("dictionary cache lock")
        .insert(stamp.clone(), (identity, Arc::downgrade(&parsed)));
    Ok((stamp, identity, parsed))
}

/// The identity of a registry with no custom bindings.
fn empty_identity() -> u64 {
    let mut identity = Xxh3::new();
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

fn valid_declared_version(version: &str) -> bool {
    if version == "FIX.Latest" {
        return true;
    }
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
    const QF_44: &str = "<fix major='4' minor='4'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields>\
        <field number='8' name='BeginString' type='STRING'/>\
        <field number='9999' name='Custom' type='STRING'/></fields></fix>";
    const QF_44_OTHER: &str = "<fix major='4' minor='4'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields>\
        <field number='9998' name='Other' type='STRING'/></fields></fix>";

    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    static LIMITS: LazyLock<Limits> = LazyLock::new(Limits::default);

    fn tmp() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("venue-reader-registry-{}-{n}", std::process::id()));
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
            path: Some(path.to_owned()),
            // Prefixed: these fixtures are named after FIX versions on
            // purpose, and a name that looks like one is refused.
            name: format!("dict-{}", path.rsplit('/').next().unwrap_or(path)),
            default: true,
            ..DictionarySpec::default()
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
        Registry::build(specs, &ctx(None, None), &DictionaryStore::default())
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
    fn application_defaults_do_not_replace_transport_sources() {
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
            alias.1.is_static(),
            "application defaults preserve bundled transport"
        );
        assert_eq!(alias.1.version(), Some("FIXT.1.1"));
        assert!(!registry.resolve("FIX.5.0SP2").unwrap().is_static());
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

    fn load_err(path: &str) -> LoadError {
        load(path, &ctx(None, None), &DictionaryStore::default()).unwrap_err()
    }

    #[test]
    fn version_entry_becomes_default_for_its_version() {
        let d = tmp();
        let (reg, events) = build(&[spec(&write(&d, "a.xml", QF_44))]);
        assert!(events.is_empty(), "{events:?}");
        let dict = reg.resolve("FIX.4.4").unwrap();
        assert!(!dict.is_static() && has_tag(&dict, 9999));
        assert!(reg.resolve("FIX.4.2").unwrap().is_static());
        assert_eq!(reg.resolve("FIX.4.4").unwrap().version(), Some("FIX.4.4"));
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
        let (reg, events) = build(&[versioned(&write(&d, "a.xml", QF_44), "FIX.2.7")]);
        assert!(events.is_empty());
        assert!(has_tag(&reg.resolve("FIX.2.7").unwrap(), 9999));
        assert_eq!(reg.resolve("FIX.2.7").unwrap().version(), Some("FIX.2.7"));
        assert!(reg.resolve("FIX.4.4").unwrap().is_static());
    }

    #[test]
    fn version_never_falls_back_to_file_and_directory_names() {
        let d = tmp();
        // A version-like filename cannot repair missing XML metadata.
        let f = write(&d, "FIX42-prod.xml", "<fix><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields/></fix>");
        let (reg, events) = build(&[spec(&f)]);
        assert!(matches!(
            events.as_slice(),
            [RegistryEvent::LoadFailed { .. }]
        ));
        assert!(reg.resolve("FIX.4.2").unwrap().is_static());
    }

    #[test]
    fn duplicate_defaults_disable_every_claim_and_keep_names() {
        let d = tmp();
        let first = write(&d, "a.xml", QF_44);
        let second = write(&d, "b.xml", QF_44_OTHER);
        let both = DictionarySpec {
            ..named(&second, "other")
        };
        let (reg, events) = build(&[spec(&first), both]);
        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .all(|event| matches!(event, RegistryEvent::DefaultConflict { .. })));
        assert!(reg.resolve("FIX.4.4").unwrap().is_static());
        assert!(reg.named(&spec(&first).name).is_some());
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
                reg.resolve("FIX.2.7").is_none(),
                "{blank:?}: the version binding went with it"
            );
        }
    }

    #[test]
    fn blank_name_warnings_describe_legacy_and_split_sources() {
        for (entry, expected) in [
            (
                DictionarySpec {
                    path: Some("synthetic.xml".into()),
                    ..DictionarySpec::default()
                },
                "dictionary \"synthetic.xml\" has a blank name; the entry is ignored",
            ),
            (
                DictionarySpec {
                    transport: Some("FIXT.1.1".into()),
                    application: Some("synthetic-application.xml".into()),
                    ..DictionarySpec::default()
                },
                "dictionary \"transport: FIXT.1.1, application: synthetic-application.xml\" has a blank name; the entry is ignored",
            ),
            (
                DictionarySpec {
                    transport: Some("synthetic-transport.xml".into()),
                    ..DictionarySpec::default()
                },
                "dictionary \"transport: synthetic-transport.xml\" has a blank name; the entry is ignored",
            ),
        ] {
            let (registry, events) = build(&[entry]);
            assert!(registry.definitions().is_empty());
            assert!(matches!(events.as_slice(), [RegistryEvent::BlankName { .. }]));
            assert_eq!(events[0].message(), expected);
        }
    }

    #[test]
    fn duplicate_name_warnings_describe_the_original_sources() {
        let directory = tmp();
        let legacy = write(&directory, "legacy.xml", QF_44);
        let transport = write(
            &directory,
            "transport.xml",
            "<fix version='FIXT.1.1'><messages><message name='Heartbeat' msgtype='0' msgcat='admin'/></messages><fields/></fix>",
        );
        let application = write(&directory, "application.xml", &synthetic_xml("FIX.5.0SP2"));
        for (entry, expected_sources) in [
            (
                DictionarySpec {
                    name: "synthetic".into(),
                    path: Some(legacy.clone()),
                    ..DictionarySpec::default()
                },
                legacy,
            ),
            (
                DictionarySpec {
                    name: "synthetic".into(),
                    transport: Some(transport.clone()),
                    application: Some(application.clone()),
                    ..DictionarySpec::default()
                },
                format!("transport: {transport}, application: {application}"),
            ),
            (
                DictionarySpec {
                    name: "synthetic".into(),
                    transport: Some(transport.clone()),
                    ..DictionarySpec::default()
                },
                format!("transport: {transport}"),
            ),
        ] {
            let original = [entry.clone()];
            let duplicate = [entry];
            let layers = [
                DictionaryLayer {
                    specs: &original,
                    source: "user.toml:4",
                    root_dir: None,
                },
                DictionaryLayer {
                    specs: &duplicate,
                    source: "project.toml:8",
                    root_dir: None,
                },
            ];
            let (registry, events) = DictionaryStore::default()
                .build_registry(&layers, &ctx(None, None))
                .unwrap();
            assert_eq!(registry.definitions().len(), 1);
            assert!(matches!(
                events.as_slice(),
                [RegistryEvent::AtSource { event, .. }]
                    if matches!(event.as_ref(), RegistryEvent::BadName { .. })
            ));
            assert_eq!(
                events[0].message(),
                format!("dictionary name \"synthetic\" rejected: project.toml:8 conflicts with user.toml:4 ({expected_sources})")
            );
        }
    }

    #[test]
    fn load_failure_warnings_keep_the_specific_failing_source() {
        let directory = tmp();
        let broken = write(&directory, "broken.xml", "<fix><messages/><fields/></fix>");
        for entry in [
            named(&broken, "legacy"),
            DictionarySpec {
                name: "pair".into(),
                transport: Some("FIXT.1.1".into()),
                application: Some(broken.clone()),
                ..DictionarySpec::default()
            },
            DictionarySpec {
                name: "family".into(),
                transport: Some(broken.clone()),
                ..DictionarySpec::default()
            },
        ] {
            let (registry, events) = build(&[entry]);
            assert!(registry.definitions().is_empty());
            let [event @ RegistryEvent::LoadFailed { path, .. }] = events.as_slice() else {
                panic!("{events:?}");
            };
            assert_eq!(path, &broken);
            assert!(event
                .message()
                .starts_with(&format!("dictionary {broken:?} not loaded: ")));
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
            events.len() == 2
                && events
                    .iter()
                    .all(|event| matches!(event, RegistryEvent::DefaultConflict { .. })),
            "{events:?}"
        );
    }

    #[test]
    fn custom_fixt11_transport_preserves_application_defaults() {
        let d = tmp();
        let (reg, events) = build(&[versioned(
            &write(
                &d,
                "a.xml",
                &QF_44.replace("msgcat='app'", "msgcat='admin'"),
            ),
            "FIXT.1.1",
        )]);
        assert!(events.is_empty());
        assert!(!reg.resolve("FIXT.1.1").unwrap().is_static());
        assert!(reg.resolve("FIX.5.0SP2").unwrap().is_static());
    }

    #[test]
    fn resolve_fixt_returns_standalone_bundled_transport() {
        let reg = Registry::bundled_only();
        let fixt = reg.resolve("FIXT.1.1").unwrap();
        assert!(fixt.is_static());
        assert_eq!(fixt.version(), Some("FIXT.1.1"));
        assert!(reg.resolve("FIX.9.9").is_none());
        assert_eq!(reg.resolve("FIX.4.4").unwrap().version(), Some("FIX.4.4"));
    }

    #[test]
    fn missing_file_and_bad_xml_drop_entry_with_event() {
        let d = tmp();
        let bad = write(
            &d,
            "bad.xml",
            "<fix major='4' minor='4'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields><field number='x'/></fields></fix>",
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
        assert!(matches!(load_err(&bad), LoadError::Parse(_)));
        assert!(matches!(load_err(&missing), LoadError::Io { .. }));
    }

    #[test]
    fn undetectable_version_is_a_load_failure() {
        let d = tmp();
        let p = write(
            &d,
            "dict.xml",
            "<fix><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields><field number='1' name='A'/></fields></fix>",
        );
        assert!(matches!(load_err(&p), LoadError::NoVersion { .. }));
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
        let err = load(&p, &ctx, &store).unwrap_err();
        assert!(
            matches!(err, LoadError::TooLarge { limit: 16, .. }),
            "{err}"
        );
        let (_, events) = Registry::build(&[spec(&p)], &ctx, &store);
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
        let err = load_err(&fifo.to_string_lossy());
        assert!(matches!(err, LoadError::NotRegularFile { .. }), "{err}");
    }

    #[test]
    fn tilde_expands_and_no_other_expansion_happens() {
        let home = tmp();
        write(&home, "d.xml", QF_44);
        let ctx = ctx(None, Some(&home));
        let store = DictionaryStore::default();
        let (reg, events) = Registry::build(&[spec("~/d.xml")], &ctx, &store);
        assert!(events.is_empty(), "{events:?}");
        assert!(has_tag(&reg.resolve("FIX.4.4").unwrap(), 9999));

        for literal in ["$HOME/d.xml", "`echo x`", "~/*.xml", "~user/d.xml"] {
            // Taken literally: either a relative path without a root, or a
            // file that does not exist — never an expansion.
            let err = load(literal, &ctx, &store).unwrap_err();
            assert!(
                matches!(err, LoadError::Io { .. } | LoadError::NoRoot),
                "{literal}: {err}"
            );
        }
    }

    #[test]
    fn home_dir_unavailable_makes_tilde_paths_fail() {
        assert!(matches!(load_err("~/d.xml"), LoadError::NoHome));
        assert!(matches!(load_err(""), LoadError::EmptyPath));
    }

    #[test]
    fn relative_path_resolves_against_root_not_cwd() {
        let root = tmp();
        write(&root, "d.xml", QF_44);
        let (reg, events) = Registry::build(
            &[spec("d.xml")],
            &ctx(Some(&root), None),
            &DictionaryStore::default(),
        );
        assert!(events.is_empty(), "{events:?}");
        assert!(has_tag(&reg.resolve("FIX.4.4").unwrap(), 9999));

        // No root: a relative path cannot be resolved, even if cwd would contain it.
        assert!(matches!(load_err("Cargo.toml"), LoadError::NoRoot));
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
        let (_, _, parsed) = load(&p, &ctx, &DictionaryStore::default()).unwrap();
        assert!(parsed.field(9999).is_some());
    }

    #[test]
    fn a_rebuild_that_reuses_everything_has_the_same_identity() {
        let d = tmp();
        let p = write(&d, "a.xml", QF_44);
        let ctx = ctx(None, None);
        let store = DictionaryStore::default();
        let (first, _) = Registry::build(&[spec(&p)], &ctx, &store);
        let (second, _) = Registry::build(&[spec(&p)], &ctx, &store);
        assert_eq!(second.identity(), first.identity());

        // Names and the compatibility argument do not affect analysis identity.
        write(&d, "a.xml", QF_44_OTHER);
        let (reloaded, _) = Registry::build(&[spec(&p)], &ctx, &store);
        assert_ne!(reloaded.identity(), second.identity());
        let (renamed, _) = Registry::build(&[named(&p, "x")], &ctx, &store);
        assert_eq!(renamed.identity(), reloaded.identity());
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
        // The point of hashing content rather than stamps: the stamp moves,
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
        let (first, _) = Registry::build(&[spec(&p)], &ctx(None, None), &store);
        assert!(has_tag(&first.resolve("FIX.4.4").unwrap(), 9999));
        write(&d, "a.xml", QF_44_OTHER);
        // Re-stat sees the new size/mtime: the cache is bypassed.
        let (reg, _) = Registry::build(&[spec(&p)], &ctx(None, None), &store);
        assert!(!has_tag(&reg.resolve("FIX.4.4").unwrap(), 9999));
        assert!(has_tag(&reg.resolve("FIX.4.4").unwrap(), 9998));
    }

    #[test]
    fn unchanged_files_are_reused_without_reparse() {
        let d = tmp();
        let p = write(&d, "a.xml", QF_44);
        let ctx = ctx(None, None);
        let store = DictionaryStore::default();
        let (first, _) = Registry::build(&[spec(&p)], &ctx, &store);
        let (second, events) = Registry::build(&[spec(&p)], &ctx, &store);
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
        let (third, _) = Registry::build(&[versioned(&p, "FIX.2.7")], &ctx, &store);
        let Dict::Owned(c) = third.resolve("FIX.2.7").unwrap() else {
            panic!()
        };
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(c.version.as_deref(), Some("FIX.2.7"));
    }

    const QF_44_DOUBLED: &str = "<fix major='4' minor='4'><messages>\
        <message name='NewOrderSingle' msgtype='D' msgcat='app'/>\
        <message name='OrderSingle' msgtype='D' msgcat='app'/></messages><fields>\
        <field number='9999' name='Custom' type='STRING'/></fields></fix>";
    const DOUBLED_DETAIL: &str = "message NewOrderSingle, MsgType(35) = D, is defined twice, \
        also as OrderSingle; merged into one under NewOrderSingle";
    const SAME_NAME_DETAIL: &str =
        "message NewOrderSingle, MsgType(35) = D, is defined twice; merged into one";

    fn doubled_fault(path: &str) -> RegistryEvent {
        RegistryEvent::DictionaryFault {
            path: path.to_owned(),
            detail: DOUBLED_DETAIL.to_owned(),
            error: true,
        }
    }

    #[test]
    fn a_dictionary_with_a_fault_loads_and_reports_it_as_an_error() {
        let d = tmp();
        let p = write(&d, "a.xml", QF_44_DOUBLED);
        let (reg, events) = build(&[spec(&p)]);
        assert_eq!(events, vec![doubled_fault(&p)]);
        assert!(events[0].is_error());
        assert_eq!(
            events[0].message(),
            format!("dictionary {p:?} loaded with an error: {DOUBLED_DETAIL}")
        );
        assert!(
            has_tag(&reg.resolve("FIX.4.4").unwrap(), 9999),
            "and it still applies"
        );
    }

    #[test]
    fn faults_replay_on_a_cache_hit() {
        let d = tmp();
        let p = write(&d, "a.xml", QF_44_DOUBLED);
        let ctx = ctx(None, None);
        let store = DictionaryStore::default();
        // Held: the cache keeps only what a live registry still uses.
        let (_held, first_events) = Registry::build(&[spec(&p)], &ctx, &store);
        let (_, second_events) = Registry::build(&[spec(&p)], &ctx, &store);
        assert_eq!(store.parse_count(), 1, "the second build is a cache hit");
        assert_eq!(first_events, vec![doubled_fault(&p)]);
        assert_eq!(second_events, first_events);
    }

    #[test]
    fn one_file_under_two_names_reports_its_faults_once() {
        let d = tmp();
        let p = write(&d, "a.xml", QF_44_DOUBLED);
        let other = DictionarySpec {
            default: false,
            ..named(&p, "other")
        };
        let (reg, events) = build(&[spec(&p), other]);
        assert_eq!(events, vec![doubled_fault(&p)]);
        assert!(reg.named("other").is_some());
    }

    #[test]
    fn faults_past_twenty_are_counted_not_listed() {
        let d = tmp();
        let mut messages = String::new();
        for i in 0..25 {
            for name in ["First", "Second"] {
                messages.push_str(&format!("<message name='{name}{i}' msgtype='U{i}'/>"));
            }
        }
        let p = write(
            &d,
            "a.xml",
            &format!("<fix major='4' minor='4'><messages>{messages}</messages><fields/></fix>"),
        );
        let (_, events) = build(&[spec(&p)]);
        assert_eq!(events.len(), 21, "{events:?}");
        assert!(events[..20]
            .iter()
            .all(|event| matches!(event, RegistryEvent::DictionaryFault { .. })));
        assert_eq!(
            events[20],
            RegistryEvent::MoreDictionaryFaults {
                path: p.clone(),
                count: 5,
                error: true,
            }
        );
        assert!(events[20].is_error());
        assert_eq!(
            events[20].message(),
            format!("dictionary {p:?} loaded with an error: … and 5 more")
        );
    }

    /// The same message defined twice under one name loses nothing.
    #[test]
    fn a_fault_that_loses_nothing_is_a_warning() {
        let d = tmp();
        let p = write(
            &d,
            "a.xml",
            "<fix major='4' minor='4'><messages>\
             <message name='NewOrderSingle' msgtype='D'/><message name='NewOrderSingle' msgtype='D'/>\
             </messages><fields><field number='9999' name='Custom' type='STRING'/></fields></fix>",
        );
        let (reg, events) = build(&[spec(&p)]);
        assert_eq!(
            events,
            [RegistryEvent::DictionaryFault {
                path: p.clone(),
                detail: SAME_NAME_DETAIL.to_owned(),
                error: false,
            }]
        );
        assert!(!events[0].is_error());
        assert_eq!(
            events[0].message(),
            format!("dictionary {p:?} loaded with a warning: {SAME_NAME_DETAIL}")
        );
        assert!(has_tag(&reg.resolve("FIX.4.4").unwrap(), 9999));
    }

    fn renamed(i: usize, error: bool) -> Fault {
        Fault::DoubledMessage {
            msg_type: format!("U{i:02}"),
            name: format!("Custom{i:02}"),
            definitions: 2,
            others: if error {
                vec![format!("Other{i:02}")]
            } else {
                Vec::new()
            },
        }
    }

    fn severities(events: &[RegistryEvent]) -> Vec<bool> {
        events.iter().map(RegistryEvent::is_error).collect()
    }

    /// Errors come first under the cap; the count line is as severe as the
    /// worst fault it hides.
    #[test]
    fn errors_are_listed_first_and_the_count_takes_the_worst_hidden_severity() {
        // 21 warnings, then one error: the error leads, two warnings are hidden.
        let mut faults: Vec<Fault> = (0..21).map(|i| renamed(i, false)).collect();
        faults.push(renamed(21, true));
        let events = fault_events("a.xml", &faults);
        assert_eq!(events.len(), 21);
        assert_eq!(
            events[0],
            RegistryEvent::DictionaryFault {
                path: "a.xml".into(),
                detail: renamed(21, true).to_string(),
                error: true,
            }
        );
        assert_eq!(severities(&events[1..20]), [false; 19]);
        assert_eq!(
            events[20],
            RegistryEvent::MoreDictionaryFaults {
                path: "a.xml".into(),
                count: 2,
                error: false,
            }
        );
        assert_eq!(
            events[20].message(),
            "dictionary \"a.xml\" loaded with a warning: … and 2 more"
        );

        // 22 errors and one warning: two errors and the warning are hidden.
        let mut faults: Vec<Fault> = (0..22).map(|i| renamed(i, true)).collect();
        faults.push(renamed(22, false));
        let events = fault_events("a.xml", &faults);
        assert_eq!(severities(&events[..20]), [true; 20]);
        assert_eq!(
            events[20],
            RegistryEvent::MoreDictionaryFaults {
                path: "a.xml".into(),
                count: 3,
                error: true,
            }
        );
    }

    #[test]
    fn a_refused_dictionary_is_an_error_and_a_sound_one_beside_it_still_binds() {
        let d = tmp();
        let broken = write(
            &d,
            "broken.xml",
            "<fix major='4' minor='4'><messages>\
             <message name='NewOrderSingle' msgtype='D'><component name='Instrumnt'/></message>\
             </messages><fields><field number='55' name='Symbol' type='STRING'/></fields></fix>",
        );
        let sound = write(&d, "sound.xml", QF_44);
        let (reg, events) = build(&[named(&broken, "broken"), spec(&sound)]);
        let [event @ RegistryEvent::LoadFailed { path, detail }] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert_eq!(*path, broken);
        assert!(
            detail.ends_with(
                "message NewOrderSingle refers to component \"Instrumnt\", which the file does not define"
            ),
            "{detail}"
        );
        assert!(event.is_error());
        assert!(reg.named("broken").is_none());
        assert!(has_tag(&reg.resolve("FIX.4.4").unwrap(), 9999));
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
        let (first, events) = store.build_registry(&layers, &context).unwrap();
        assert!(events.is_empty());
        let (second, events) = store.build_registry(&layers, &context).unwrap();
        assert!(events.is_empty());
        assert!(first
            .named("venue")
            .unwrap()
            .ptr_eq(&second.named("venue").unwrap()));
        assert_eq!(store.parse_count(), 1);
        let (third, _) = store.build_registry(&layers, &context).unwrap();
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
        let (first, _) = store.build_registry(&layers, &context).unwrap();
        let limits = Limits {
            dictionary_bytes: 16,
            ..Limits::default()
        };
        let (limited, events) = store
            .build_registry(
                &layers,
                &BuildContext {
                    limits: &limits,
                    ..context
                },
            )
            .unwrap();
        assert!(limited.named("venue").is_none());
        assert!(matches!(
            events.as_slice(),
            [RegistryEvent::AtSource { event, .. }] if matches!(event.as_ref(), RegistryEvent::LoadFailed { .. })
        ));
        write(&directory, "venue.xml", QF_44_OTHER);
        let (changed, _) = store.build_registry(&layers, &context).unwrap();
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
        let (project, _) = store.build_registry(&layers, &ctx(None, None)).unwrap();
        let Dict::Owned(parsed) = project.named("venue").unwrap() else {
            panic!("custom dictionary expected");
        };
        let weak = Arc::downgrade(&parsed);
        drop(parsed);
        drop(project);
        assert!(weak.upgrade().is_none());
        let (reopened, events) = store.build_registry(&layers, &ctx(None, None)).unwrap();
        assert!(events.is_empty());
        assert!(reopened.named("venue").is_some());
        assert_eq!(store.cache.lock().unwrap().len(), 1);
        assert_eq!(store.parse_count(), 2);
    }

    /// Only one QuickFIX XML file is read. A directory — a FIX Repository package
    /// among others — is refused for its own entry, not as an I/O error that
    /// would abort the whole configuration.
    #[test]
    fn a_directory_is_refused_as_a_dictionary() {
        let directory = tmp();
        write(&directory, "Fields.xml", "<Fields/>");
        let sound = tmp();
        let file = write(&sound, "desk.xml", QF_44);
        let (registry, events) = build(&[spec(&directory.to_string_lossy()), spec(&file)]);
        assert!(
            matches!(&events[..], [RegistryEvent::LoadFailed { detail, .. }]
                if detail.contains("one QuickFIX XML file")),
            "{events:?}"
        );
        assert!(
            !registry.resolve("FIX.4.4").unwrap().is_static(),
            "the sound entry still loads"
        );
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
            .build_registry(&layers, &ctx(None, None))
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
        let (registry, events) = store.build_registry(&layers, &ctx(None, None)).unwrap();
        assert_eq!(registry.definitions().len(), 1);
        assert!(
            matches!(events.as_slice(), [RegistryEvent::AtSource { event, .. }] if matches!(event.as_ref(), RegistryEvent::BadName { .. }))
        );
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
            .build_registry(&extended_layers, &ctx(None, None))
            .unwrap();
        assert!(events[0].is_error());
        assert!(extended.resolve("FIX.2.7").is_none());
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
            .build_registry(&layers, &ctx(None, None))
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
            .build_registry(&layer(&specs), &ctx(None, None))
            .unwrap();
        assert!(events.is_empty());
        write(&directory, "selected.xml", QF_44_OTHER);
        let (updated, events) = store
            .build_registry(&layer(&specs), &ctx(None, None))
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
                &format!("<fix version='{version}'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields/></fix>"),
            );
            assert!(matches!(load_err(&path), LoadError::InvalidVersion { .. }));
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
        let (previous, events) = store.build_registry(&layers, &ctx(None, None)).unwrap();
        assert!(events.is_empty());
        let project_choice = previous.choice("project").unwrap();
        write(
            &directory,
            "global.xml",
            &QF_44.replace("Custom", "Updated"),
        );
        let (updated, events) = store.build_registry(&layers, &ctx(None, None)).unwrap();
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

    fn application_identity(version: &str) -> ApplicationIdentity {
        ApplicationIdentity::release(version)
    }

    fn synthetic_xml(version: &str) -> String {
        format!("<fix version='{version}'><messages><message name='Synthetic' msgtype='D' msgcat='app'/></messages><fields><field number='9999' name='SyntheticField' type='STRING'/></fields></fix>")
    }

    #[test]
    fn complete_defaults_fall_back_atomically_and_only_families_compose() {
        let directory = tmp();
        let transport = write(&directory, "transport.xml", "<fix version='FIXT.1.1'><messages><message name='Heartbeat' msgtype='0' msgcat='admin'/></messages><fields/></fix>");
        let complete = DictionarySpec {
            name: "complete".into(),
            transport: Some(transport.clone()),
            application: Some("FIX.5.0".into()),
            default: true,
            ..DictionarySpec::default()
        };
        let (registry, events) = build(std::slice::from_ref(&complete));
        assert!(events.is_empty(), "{events:?}");
        assert!(!registry
            .default_transport("FIXT.1.1")
            .unwrap()
            .dict
            .is_static());
        let exact = registry
            .default_pair("FIXT.1.1", &application_identity("FIX.5.0"))
            .unwrap();
        assert!(!exact.transport.is_static());
        let fallback = registry
            .default_pair("FIXT.1.1", &application_identity("FIX.5.0SP2"))
            .unwrap();
        assert!(
            fallback.transport.is_static(),
            "a complete default never lends its transport"
        );
        let family = DictionarySpec {
            name: "family".into(),
            transport: Some(transport),
            default: true,
            ..DictionarySpec::default()
        };
        let (registry, events) = build(&[complete, family]);
        assert!(events.is_empty(), "{events:?}");
        let composed = registry
            .default_pair("FIXT.1.1", &application_identity("FIX.5.0SP2"))
            .unwrap();
        assert!(!composed.transport.is_static());
        assert!(composed
            .transport
            .ptr_eq(&registry.choice("family").unwrap().transport));
        assert!(registry
            .default_transport("FIXT.1.1")
            .unwrap()
            .application
            .is_none());
        assert!(registry
            .default_pair("FIX.5.0", &application_identity("FIX.5.0"))
            .is_none());
    }

    #[test]
    fn many_default_profiles_keep_exact_application_identity() {
        let specs: Vec<_> = (0..512)
            .rev()
            .map(|profile| DictionarySpec {
                name: format!("profile-{profile}"),
                transport: Some("FIXT.1.1".into()),
                application: Some("FIX.5.0SP2".into()),
                custom_version: Some(format!("SYNTHETIC_{profile}")),
                extension_pack: Some(profile % 3),
                default: true,
                ..DictionarySpec::default()
            })
            .collect();
        let (registry, events) = build(&specs);
        assert!(events.is_empty(), "{events:?}");
        for spec in &specs {
            let key = ApplicationKey {
                version: "FIX.5.0SP2",
                extension_pack: spec.extension_pack,
                custom_version: spec.custom_version.as_deref(),
            };
            assert_eq!(
                registry
                    .default_pair("FIXT.1.1", key)
                    .unwrap()
                    .name
                    .as_deref(),
                Some(spec.name.as_str())
            );
            assert!(registry
                .default_pair(
                    "FIXT.1.1",
                    ApplicationKey {
                        extension_pack: None,
                        ..key
                    }
                )
                .is_none());
        }
        assert!(registry
            .default_pair(
                "FIXT.1.1",
                ApplicationKey {
                    version: "FIX.5.0SP2",
                    extension_pack: Some(0),
                    custom_version: Some("SYNTHETIC_MISSING"),
                }
            )
            .is_none());
    }

    #[test]
    fn exact_extension_and_custom_identity_do_not_fall_back_to_base_release() {
        let directory = tmp();
        let path = write(&directory, "app.xml", &synthetic_xml("FIX.5.0SP2"));
        let extension = DictionarySpec {
            name: "extended".into(),
            path: Some(path),
            default: true,
            extension_pack: Some(0),
            custom_version: Some("synthetic".into()),
            ..DictionarySpec::default()
        };
        let (registry, events) = build(&[extension]);
        assert!(events.is_empty(), "{events:?}");
        let identity = ApplicationIdentity {
            version: "FIX.5.0SP2".into(),
            extension_pack: Some(0),
            custom_version: Some("synthetic".into()),
        };
        let pair = registry.default_pair("FIXT.1.1", &identity).unwrap();
        assert!(!pair.dict.is_static());
        assert_eq!(
            registry.default_name_for_identity(pair.identity),
            Some("extended")
        );
        assert!(registry
            .default_pair(
                "FIXT.1.1",
                &ApplicationIdentity {
                    extension_pack: None,
                    ..identity.clone()
                }
            )
            .is_none());
        assert!(registry
            .default_pair(
                "FIXT.1.1",
                &ApplicationIdentity {
                    custom_version: None,
                    ..identity
                }
            )
            .is_none());
        assert!(registry
            .default_pair("FIXT.1.1", &application_identity("FIX.5.0SP2"))
            .unwrap()
            .dict
            .is_static());
    }

    #[test]
    fn paired_sources_remain_independent_and_shared_files_parse_once() {
        let directory = tmp();
        let path = write(&directory, "app.xml", &synthetic_xml("FIX.5.0SP2"));
        let first = DictionarySpec {
            name: "first".into(),
            path: Some(path.clone()),
            ..DictionarySpec::default()
        };
        let second = DictionarySpec {
            name: "second".into(),
            transport: Some("FIXT.1.1".into()),
            application: Some(path),
            ..DictionarySpec::default()
        };
        let store = DictionaryStore::default();
        let (registry, events) = Registry::build(&[first, second], &ctx(None, None), &store);
        assert!(events.is_empty(), "{events:?}");
        let first = registry.choice("first").unwrap();
        let second = registry.choice("second").unwrap();
        assert_eq!(store.parse_count(), 1);
        assert!(first.dict.ptr_eq(&second.dict));
        assert_eq!(first.identity, second.identity);
        assert_eq!(first.application_version(), Some("FIX.5.0SP2"));
        assert!(first
            .source(SourceRole::Transport)
            .unwrap()
            .field_idx(9999)
            .is_none());
        assert!(first
            .source(SourceRole::Application)
            .unwrap()
            .field_idx(9999)
            .is_some());
    }

    #[test]
    fn a_fixt_path_with_only_session_categories_is_transport_only() {
        let directory = tmp();
        for category in ["admin", "session", "administrative", "Administrative"] {
            let path = write(
                &directory,
                &format!("transport-{category}.xml"),
                &format!("<fix version='FIXT.1.1'><messages><message name='Heartbeat' msgtype='0' msgcat='{category}'/></messages><fields/></fix>"),
            );
            let (registry, events) = build(&[named(&path, "transport")]);
            assert!(events.is_empty(), "{category}: {events:?}");
            let choice = registry.choice("transport").expect(category);
            assert_eq!(choice.application, None, "{category}");
        }
    }

    #[test]
    fn combined_fixt_requires_identity_and_hashes_each_identity_dimension() {
        let directory = tmp();
        let path = write(&directory, "combined.xml", &synthetic_xml("FIXT.1.1"));
        let (registry, events) = build(&[named(&path, "invalid")]);
        assert!(matches!(
            events.as_slice(),
            [RegistryEvent::InvalidBinding { .. }]
        ));
        assert!(registry.choice("invalid").is_none());
        let base = DictionarySpec {
            name: "base".into(),
            path: Some(path.clone()),
            application_version: Some("FIX.5.0SP2".into()),
            ..DictionarySpec::default()
        };
        let variants = [
            base.clone(),
            DictionarySpec {
                name: "release".into(),
                application_version: Some("FIX.Latest".into()),
                ..base.clone()
            },
            DictionarySpec {
                name: "extension".into(),
                extension_pack: Some(0),
                ..base.clone()
            },
            DictionarySpec {
                name: "custom".into(),
                custom_version: Some("synthetic".into()),
                ..base.clone()
            },
            DictionarySpec {
                name: "transport".into(),
                path: None,
                transport: Some(path),
                application_version: None,
                ..base
            },
        ];
        let store = DictionaryStore::default();
        let (registry, events) = Registry::build(&variants, &ctx(None, None), &store);
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(store.parse_count(), 1);
        let identities: HashSet<_> = variants
            .iter()
            .map(|spec| registry.choice(&spec.name).unwrap().identity)
            .collect();
        assert_eq!(identities.len(), variants.len());
        let combined = registry.choice("base").unwrap();
        assert!(combined.transport.ptr_eq(&combined.dict));
        assert!(registry
            .choice("transport")
            .unwrap()
            .source(SourceRole::Application)
            .is_none());
    }

    #[test]
    fn explicit_modern_fix44_profile_supports_latest_without_implicit_fallback() {
        let directory = tmp();
        let latest = write(&directory, "latest.xml", &synthetic_xml("FIX.Latest"));
        let profile = DictionarySpec {
            name: "modern".into(),
            transport: Some("FIX.4.4".into()),
            application: Some(latest),
            application_version: Some("FIX.Latest".into()),
            default: true,
            ..DictionarySpec::default()
        };
        let (registry, events) = build(&[profile]);
        assert!(events.is_empty(), "{events:?}");
        let pair = registry
            .default_pair("FIX.4.4", &application_identity("FIX.Latest"))
            .unwrap();
        assert_eq!(pair.transport.version(), Some("FIX.4.4"));
        assert_eq!(pair.dict.version(), Some("FIX.Latest"));
        assert!(registry
            .default_pair("FIXT.1.1", &application_identity("FIX.Latest"))
            .is_none());
        assert!(registry
            .default_pair("FIX.4.4", &application_identity("FIX.5.0"))
            .is_none());
    }

    #[test]
    fn conflicting_physical_transports_disable_all_claims_but_keep_unrelated_defaults() {
        let directory = tmp();
        let transport_xml = "<fix version='FIXT.1.1'><messages><message name='Heartbeat' msgtype='0' msgcat='admin'/></messages><fields/></fix>";
        let first = write(&directory, "first.xml", transport_xml);
        let second = write(&directory, "second.xml", transport_xml);
        let pair = |name: &str, transport: String, application: &str| DictionarySpec {
            name: name.into(),
            transport: Some(transport),
            application: Some(application.into()),
            default: true,
            ..DictionarySpec::default()
        };
        let unrelated = named(&write(&directory, "fix44.xml", QF_44), "unrelated");
        let specs = [
            pair("first", first, "FIX.5.0"),
            pair("second", second, "FIX.5.0SP2"),
            unrelated,
        ];
        let (registry, events) = build(&specs);
        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .all(|event| matches!(event, RegistryEvent::DefaultConflict { .. })));
        assert_ne!(
            registry.choice("first").unwrap().identity,
            registry.choice("second").unwrap().identity
        );
        for name in ["first", "second"] {
            assert!(registry.choice(name).is_some());
        }
        assert!(registry
            .default_transport("FIXT.1.1")
            .unwrap()
            .dict
            .is_static());
        assert!(registry
            .default_pair("FIXT.1.1", &application_identity("FIX.5.0"))
            .unwrap()
            .dict
            .is_static());
        assert!(!registry
            .default_pair("FIX.4.4", &application_identity("FIX.4.4"))
            .unwrap()
            .dict
            .is_static());
        let reversed: Vec<_> = specs.into_iter().rev().collect();
        let (reversed, _) = build(&reversed);
        assert_eq!(registry.identity(), reversed.identity());
    }

    #[test]
    fn competing_identity_defaults_do_not_disable_an_independent_transport_claim() {
        let directory = tmp();
        let transport_xml = "<fix version='FIXT.1.1'><messages><message name='Heartbeat' msgtype='0' msgcat='admin'/></messages><fields/></fix>";
        let first = write(&directory, "first.xml", transport_xml);
        let second = write(&directory, "second.xml", transport_xml);
        let pair = |name: &str, transport: String, application: &str| DictionarySpec {
            name: name.into(),
            transport: Some(transport),
            application: Some(application.into()),
            default: true,
            ..DictionarySpec::default()
        };
        let specs = [
            pair("conflict-a", first.clone(), "FIX.5.0"),
            pair("conflict-b", second, "FIX.5.0"),
            pair("independent", first, "FIX.5.0SP2"),
        ];
        let (registry, events) = build(&specs);
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(events.iter().all(|event| matches!(
            event,
            RegistryEvent::DefaultConflict { name, detail, .. }
                if name.starts_with("conflict-")
                    && detail == "competing defaults for the same exact identity"
        )));
        for spec in &specs {
            assert!(registry.choice(&spec.name).is_some());
        }
        let fallback = registry
            .default_pair("FIXT.1.1", &application_identity("FIX.5.0"))
            .unwrap();
        assert!(fallback.transport.is_static());
        let independent = registry
            .default_pair("FIXT.1.1", &application_identity("FIX.5.0SP2"))
            .unwrap();
        assert_eq!(
            registry.default_name_for_identity(independent.identity),
            Some("independent")
        );
        assert!(independent
            .transport
            .ptr_eq(&registry.choice("independent").unwrap().transport));
        let reversed: Vec<_> = specs.into_iter().rev().collect();
        let (reversed, reversed_events) = build(&reversed);
        assert_eq!(reversed_events.len(), 2);
        assert_eq!(registry.identity(), reversed.identity());
    }

    #[test]
    fn stronger_claim_replaces_lower_conflicts_before_validation() {
        let directory = tmp();
        let first = [
            named(&write(&directory, "a.xml", QF_44), "a"),
            named(&write(&directory, "b.xml", QF_44_OTHER), "b"),
        ];
        let second = [named(&write(&directory, "c.xml", QF_44), "c")];
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
            .build_registry(&layers, &ctx(None, None))
            .unwrap();
        assert!(events.is_empty(), "{events:?}");
        let chosen = registry
            .default_pair("FIX.4.4", &application_identity("FIX.4.4"))
            .unwrap();
        assert_eq!(
            registry.default_name_for_identity(chosen.identity),
            Some("c")
        );
        for name in ["a", "b", "c"] {
            assert!(registry.choice(name).is_some());
        }
    }

    #[test]
    fn source_identity_ignores_names_and_defaults_report_the_active_name() {
        let directory = tmp();
        let path = write(&directory, "shared.xml", QF_44);
        let alpha = DictionarySpec {
            default: false,
            ..named(&path, "alpha")
        };
        let beta = named(&path, "beta");
        let (registry, events) = build(&[alpha, beta]);
        assert!(events.is_empty(), "{events:?}");
        let identity = registry.choice("alpha").unwrap().identity;
        assert_eq!(identity, registry.choice("beta").unwrap().identity);
        assert_eq!(registry.default_name_for_identity(identity), Some("beta"));
        let renamed = [named(&path, "renamed")];
        let (renamed, _) = build(&renamed);
        assert_eq!(registry.identity(), renamed.identity());
        assert_eq!(renamed.default_name_for_identity(identity), Some("renamed"));
    }

    #[test]
    fn projected_admin_names_follow_active_defaults_and_transport_identity() {
        let directory = tmp();
        let transport = write(&directory, "transport.xml", "<fix version='FIXT.1.1'><messages><message name='Heartbeat' msgtype='0' msgcat='admin'/></messages><fields/></fix>");
        let pair = |name: &str, application: &str| DictionarySpec {
            name: name.into(),
            transport: Some(transport.clone()),
            application: Some(application.into()),
            default: true,
            ..DictionarySpec::default()
        };
        let lower = [pair("parent", "FIX.5.0")];
        let upper = [pair("active", "FIX.5.0SP2")];
        let layers = [
            DictionaryLayer {
                specs: &lower,
                source: "parent",
                root_dir: None,
            },
            DictionaryLayer {
                specs: &upper,
                source: "child",
                root_dir: None,
            },
        ];
        let store = DictionaryStore::default();
        let (registry, events) = store.build_registry(&layers, &ctx(None, None)).unwrap();
        assert!(events.is_empty(), "{events:?}");
        let projected = registry.default_transport("FIXT.1.1").unwrap();
        assert_eq!(
            registry.default_name_for_identity(projected.identity),
            Some("active")
        );
        assert_eq!(names(registry.clone(), None).of("FIXT.1.1"), Some("active"));
        let family = DictionarySpec {
            name: "family".into(),
            transport: Some(transport),
            default: true,
            ..DictionarySpec::default()
        };
        let (with_family, events) = Registry::build(
            &[lower[0].clone(), upper[0].clone(), family],
            &ctx(None, None),
            &store,
        );
        assert!(events.is_empty(), "{events:?}");
        let family_transport = with_family.default_transport("FIXT.1.1").unwrap();
        assert_eq!(projected.identity, family_transport.identity);
        assert_eq!(
            with_family.default_name_for_identity(family_transport.identity),
            Some("family")
        );
        assert_eq!(names(with_family, None).of("FIXT.1.1"), Some("family"));
    }

    struct NamedAnalyses {
        names: crate::ctx::DictionaryNames,
        context: Arc<crate::ctx::Ctx>,
    }

    impl NamedAnalyses {
        fn of(&self, version: &str) -> Option<&str> {
            let message = if version == FIXT_1_1 {
                "8=FIXT.1.1|35=0|".to_owned()
            } else if version.starts_with("FIX.5.") {
                format!("8=FIXT.1.1|35=D|1128={}|", to_wire(version).unwrap())
            } else {
                format!("8={version}|35=D|")
            };
            let analysis = crate::ctx::analyze(message.as_bytes(), &self.context);
            let labels = self.names.of_analysis(analysis.message()?);
            labels.application.or(labels.transport)
        }
    }

    fn names(registry: Registry, choice: Option<DictionaryChoice>) -> NamedAnalyses {
        let registry = Arc::new(registry);
        NamedAnalyses {
            names: crate::ctx::DictionaryNames::new(&registry, choice.as_ref()),
            context: crate::ctx::Ctx::with_choice(
                &crate::config::Config::default(),
                registry,
                choice,
            ),
        }
    }

    /// A selected dictionary answers only the versions it is compatible with:
    /// a FIX.4.2 line is still decoded by, and named after, the FIX.4.2 default.
    #[test]
    fn each_version_names_the_dictionary_that_answers_it() {
        let directory = tmp();
        let venue = write(&directory, "venue.xml", QF_44);
        let desk = write(&directory, "desk.xml", QF_44_OTHER);
        let venue = named(&versioned(&venue, "FIX.4.2").path.unwrap(), "venue42");
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

    /// A selected transport family and a default application pair are two
    /// bindings, and each role keeps the name of its own binding.
    #[test]
    fn a_family_and_a_default_application_name_their_own_roles() {
        let directory = tmp();
        let transport = write(&directory, "desk-fixt.xml", "<fix version='FIXT.1.1'><messages><message name='Heartbeat' msgtype='0' msgcat='admin'/></messages><fields/></fix>");
        let application = write(&directory, "desk-app.xml", QF_44);
        let family = DictionarySpec {
            name: "desk-fixt".into(),
            transport: Some(transport),
            ..DictionarySpec::default()
        };
        let application = named(
            &versioned(&application, "FIX.5.0SP2").path.unwrap(),
            "desk-app",
        );
        let (registry, events) = build(&[family, application]);
        assert!(events.is_empty(), "{events:?}");
        let chosen = registry.choice("desk-fixt");
        let named = names(registry, chosen);
        let analysis = crate::ctx::analyze(b"8=FIXT.1.1|35=D|1128=9|54=1|", &named.context);
        let labels = named.names.of_analysis(analysis.message().unwrap());
        assert_eq!(labels.transport, Some("desk-fixt"));
        assert_eq!(labels.application, Some("desk-app"));
    }

    /// Complete defaults also name their projected transport.
    #[test]
    fn complete_application_default_names_its_projected_transport() {
        let directory = tmp();
        let path = write(&directory, "session.xml", QF_44);
        let (registry, events) = build(&[named(
            &versioned(&path, "FIX.5.0SP2").path.unwrap(),
            "venue",
        )]);
        assert!(events.is_empty(), "{events:?}");
        let names = names(registry, None);
        assert_eq!(names.of("FIXT.1.1"), Some("venue"));
        assert_eq!(names.of("FIX.5.0SP2"), Some("venue"));
    }

    #[test]
    fn renaming_an_unselected_nondefault_preserves_active_labels() {
        let directory = tmp();
        let path = write(&directory, "inactive.xml", QF_44);
        let before = DictionarySpec {
            default: false,
            ..named(&path, "before")
        };
        let after = DictionarySpec {
            name: "after".into(),
            ..before.clone()
        };
        let (old_registry, old_events) = build(&[before]);
        let (new_registry, new_events) = build(&[after]);
        assert!(old_events.is_empty(), "{old_events:?}");
        assert!(new_events.is_empty(), "{new_events:?}");
        assert_eq!(old_registry.identity(), new_registry.identity());
        let old_names = crate::ctx::DictionaryNames::new(&old_registry, None);
        let new_names = crate::ctx::DictionaryNames::new(&new_registry, None);
        assert!(old_names.same_as(&new_names));
        let old_selected = old_registry.choice("before");
        let new_selected = new_registry.choice("after");
        let old_names = crate::ctx::DictionaryNames::new(&old_registry, old_selected.as_ref());
        let new_names = crate::ctx::DictionaryNames::new(&new_registry, new_selected.as_ref());
        assert!(!old_names.same_as(&new_names));
    }

    #[test]
    fn a_nondefault_alias_of_a_reachable_pair_keeps_its_label() {
        let directory = tmp();
        let path = write(&directory, "transport.xml", "<fix version='FIXT.1.1'><messages><message name='Heartbeat' msgtype='0' msgcat='admin'/></messages><fields/></fix>");
        let family = DictionarySpec {
            name: "family".into(),
            transport: Some(path.clone()),
            default: true,
            ..DictionarySpec::default()
        };
        let alias = DictionarySpec {
            name: "alias".into(),
            transport: Some(path),
            application: Some("FIX.5.0SP2".into()),
            ..DictionarySpec::default()
        };
        let (registry, events) = build(&[family, alias]);
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(names(registry, None).of("FIX.5.0SP2"), Some("alias"));
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
            names(registry, Some(choice)).of("FIX.4.4"),
            Some("desk.xml")
        );
    }
}
