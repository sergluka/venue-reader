//! Filesystem work is confined to one detachable configuration worker.
//!
//! A batch snapshots the main-loop state, reads and merges external sources on
//! the worker, then returns prepared data. The main loop rejects stale results
//! before constructing runtime objects and committing the batch atomically.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use lsp_types::Uri;

use crate::config::{self, Severity};
use crate::ctx::Ctx;
use crate::dictionary::{
    looks_like_path, BuildContext, DictionaryChoice, DictionaryLayer, DictionaryStore, LoadError,
    Registry,
};
use crate::overrides::{self, project, Origin, Resolved, SettingsLayer, SourceKind, Warning};
use crate::render::Options;
use crate::rules::Registry as RuleRegistry;

pub const TIMEOUT: Duration = Duration::from_millis(500);

/// Handle for the configuration worker and its result channel.
pub(super) struct Loader {
    /// Queue used by the main loop to submit filesystem work.
    jobs: Sender<Job>,
    /// Completed work waiting to be validated by the main loop.
    pub reports: Receiver<Report>,
    #[cfg(test)]
    /// Number of jobs submitted through [`Loader::send`].
    pub jobs_submitted: std::sync::atomic::AtomicUsize,
}

/// Work accepted by the configuration worker.
pub(super) enum Job {
    /// Read the user configuration before the server starts accepting documents.
    Startup(Option<PathBuf>),
    /// Prepare one atomic configuration update outside the main loop.
    Batch(Box<Input>),
    #[cfg(test)]
    /// Hold the worker at a deterministic point in concurrency tests.
    Barrier {
        entered: Sender<()>,
        release: Receiver<()>,
    },
    #[cfg(test)]
    /// Exercise panic isolation without terminating the worker thread.
    Panic(u64),
}

/// Results returned to the main loop for validation and commit.
pub(super) enum Report {
    /// Result of loading the optional user configuration during startup.
    Startup(Result<SettingsLayer, String>),
    /// Prepared batch paired with the generation that requested it.
    Batch(u64, Result<Box<Prepared>, String>),
}

/// An open-document snapshot captured when a configuration batch starts.
///
/// `lifetime` rejects work from an earlier open/close cycle; `revision` rejects
/// work prepared against an obsolete modeline.
#[derive(Clone)]
pub(super) struct DocumentInput {
    /// Document whose effective configuration is being prepared.
    pub uri: Uri,
    /// Open/close cycle captured when the batch started.
    pub lifetime: u64,
    /// Modeline revision captured when the batch started.
    pub revision: u64,
    /// Modeline layer current at `revision`.
    pub modeline: SettingsLayer,
    /// Last committed state available for reuse.
    pub previous: Option<Arc<DocumentConfig>>,
}

/// All state needed to prepare one configuration batch without the main loop.
///
/// Refresh flags select which filesystem-backed layers must be read again;
/// other layers are immutable snapshots from the last committed batch.
pub(super) struct Input {
    /// Batch generation assigned by the main loop.
    pub generation: u64,
    /// Last committed user-file layer.
    pub user_settings: SettingsLayer,
    /// Last committed client-provided settings layer.
    pub client_settings: SettingsLayer,
    /// Last committed merge of the global settings layers.
    pub base_settings: Resolved,
    /// User configuration file to read during a global refresh.
    pub user_path: Option<PathBuf>,
    /// Whether global settings layers and dictionaries must be rebuilt.
    pub refresh_global: bool,
    /// Whether project files must be discovered and read again.
    pub refresh_projects: bool,
    /// Workspace roots used to bound project-file discovery.
    pub roots: Vec<PathBuf>,
    /// Legacy root used when no workspace folder applies.
    pub root_uri: Option<PathBuf>,
    /// Base directory for relative global dictionary paths.
    pub dictionary_root: Option<PathBuf>,
    /// Home directory used to expand `~/` dictionary paths.
    pub home: Option<PathBuf>,
    /// Last committed global dictionary registry.
    pub registry: Arc<Registry>,
    /// Session-level dictionary selector, if one is active.
    pub session: Option<String>,
    /// Resolved session choice paired with `session`.
    pub session_choice: Option<DictionaryChoice>,
    /// Session-selection revision captured when the batch started.
    pub session_revision: u64,
    /// Open-document snapshots included in the atomic batch.
    pub documents: Vec<DocumentInput>,
}

/// A fully prepared batch whose freshness still must be checked by the main loop.
pub(super) struct Prepared {
    /// Whether this batch rebuilt global sources.
    pub refresh_global: bool,
    /// Whether this batch reread project files.
    pub refresh_projects: bool,
    /// User layer to commit with the batch.
    pub user: SettingsLayer,
    /// Client layer to commit with the batch.
    pub client: SettingsLayer,
    /// Prepared effective global configuration.
    pub base: Resolved,
    /// Prepared global dictionary registry.
    pub registry: Arc<Registry>,
    /// Session selector that survived preparation.
    pub session: Option<String>,
    /// Choice resolved from `session` against `registry`.
    pub session_choice: Option<DictionaryChoice>,
    /// Session revision used to prepare the document results.
    pub session_revision: u64,
    /// Inputs paired with their prepared document state for freshness checks.
    pub documents: Vec<(DocumentInput, PreparedDocument)>,
}

/// Worker-produced document state without main-loop-owned runtime objects.
pub(super) struct PreparedDocument {
    /// Effective merged layers and their warnings.
    pub resolved: Resolved,
    /// Project state reusable by a later incremental batch.
    pub project: ProjectSnapshot,
    /// Dictionary selected after applying all document-local layers.
    pub choice: Option<DictionaryChoice>,
}

/// Reusable filesystem-backed state for one document's project layers.
#[derive(Clone)]
pub(super) struct ProjectSnapshot {
    /// Project-file layers ordered for configuration merging.
    pub layers: Vec<SettingsLayer>,
    /// Registry containing the global and project dictionary definitions.
    pub registry: Arc<Registry>,
    /// Project-origin warnings retained when project files are not reread.
    pub warnings: Vec<Warning>,
}

/// Committed effective configuration and runtime state for an open document.
#[derive(Clone)]
pub(super) struct DocumentConfig {
    /// Effective configuration and warnings committed for the document.
    pub resolved: Resolved,
    /// Filesystem-backed state retained for incremental preparation.
    pub project: ProjectSnapshot,
    /// Analysis context selected for the document.
    pub ctx: Arc<Ctx>,
    /// What hover calls each bound dictionary. Beside `ctx` for the reason
    /// `render` is: a name invalidates no analysis.
    pub names: Arc<crate::ctx::DictionaryNames>,
    /// Rendering options derived from `resolved`.
    pub render: Arc<Options>,
    /// Diagnostic rules derived from `resolved` and `ctx`.
    pub rules: Arc<RuleRegistry>,
}

#[derive(Clone)]
struct Selection {
    /// Selector text as it appeared in the winning layer.
    raw: String,
    /// Source of the winning selector.
    origin: Origin,
    /// Dictionary choice resolved from `raw`.
    choice: DictionaryChoice,
}

impl Loader {
    pub fn start() -> Self {
        let (jobs, incoming) = crossbeam_channel::unbounded();
        let (outgoing, reports) = crossbeam_channel::unbounded();
        std::thread::Builder::new()
            .name("fix-config".into())
            .spawn(move || {
                let store = DictionaryStore::default();
                for job in incoming {
                    let report = match job {
                        #[cfg(test)]
                        Job::Barrier { entered, release } => {
                            let _ = entered.send(());
                            let _ = release.recv();
                            continue;
                        }
                        #[cfg(test)]
                        Job::Panic(generation) => Report::Batch(
                            generation,
                            guard(|| panic!("injected configuration loader panic")),
                        ),
                        Job::Startup(path) => {
                            Report::Startup(guard(|| user_layer(path.as_deref())))
                        }
                        Job::Batch(input) => {
                            let generation = input.generation;
                            let mut cache = project::BatchCache::default();
                            Report::Batch(
                                generation,
                                guard(|| prepare(*input, &mut cache, &store).map(Box::new)),
                            )
                        }
                    };
                    if outgoing.send(report).is_err() {
                        break;
                    }
                }
            })
            .expect("configuration worker");
        Self {
            jobs,
            reports,
            #[cfg(test)]
            jobs_submitted: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn send(&self, job: Job) -> Result<(), String> {
        #[cfg(test)]
        self.jobs_submitted
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.jobs
            .send(job)
            .map_err(|_| "configuration worker disconnected".into())
    }

    pub fn startup(&self, path: Option<PathBuf>) -> Result<SettingsLayer, String> {
        self.send(Job::Startup(path))?;
        match self.reports.recv_timeout(TIMEOUT) {
            Ok(Report::Startup(result)) => result,
            Ok(_) => Err("unexpected configuration startup result".into()),
            Err(error) => Err(format!("user configuration: {error}")),
        }
    }
}

fn guard<T>(work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
        .unwrap_or_else(|_| Err("configuration loader panicked".into()))
}

pub(super) fn empty_user() -> SettingsLayer {
    SettingsLayer::new(Origin::new(SourceKind::UserFile, "user configuration"))
}

fn user_layer(path: Option<&Path>) -> Result<SettingsLayer, String> {
    let Some(path) = path else {
        return Ok(empty_user());
    };
    let loaded =
        config::user_file::load(path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(loaded_layer(
        loaded,
        Origin::new(SourceKind::UserFile, path.display().to_string()),
    ))
}

fn loaded_layer(loaded: project::LoadedFile, origin: Origin) -> SettingsLayer {
    match loaded {
        project::LoadedFile::Valid(file) => file.layer,
        project::LoadedFile::Absent => SettingsLayer::new(origin),
        project::LoadedFile::Rejected(warning) => {
            let mut layer = SettingsLayer::new(warning.origin.clone());
            layer.warnings.push(warning);
            layer
        }
    }
}

/// A session selection that its dictionary no longer backs. Said in two
/// places -- the worker rebuild and the main loop's own -- so it is written
/// once.
pub fn session_dictionary_gone(raw: &str) -> Warning {
    Warning::new(
        "dictionary",
        format!("dictionary `{raw}` is no longer available; session selection cleared"),
        Origin::new(SourceKind::Session, "session"),
    )
}

fn prepare(
    mut input: Input,
    cache: &mut project::BatchCache,
    store: &DictionaryStore,
) -> Result<Prepared, String> {
    let user = if input.refresh_global {
        user_layer(input.user_path.as_deref())?
    } else {
        input.user_settings.clone()
    };
    let base_layers = vec![user.clone(), input.client_settings.clone()];
    let mut base = if input.refresh_global {
        overrides::merge(&base_layers, RuleRegistry::known_ids())?
    } else {
        input.base_settings.clone()
    };
    base.warnings
        .retain(|warning| warning.origin.layer != SourceKind::Session);
    if input.refresh_global && !base.config.allow_dictionary_path {
        base.warnings.extend(blocked_dictionaries(
            &base_layers,
            "dictionaries",
            &input.client_settings.origin,
        ));
    }
    let context = BuildContext {
        root_dir: input.dictionary_root.as_deref(),
        home_dir: input.home.as_deref(),
        limits: &base.config.limits,
    };
    let registry = if input.refresh_global {
        let roots = layer_roots(&base_layers, input.dictionary_root.as_deref());
        let layers = dictionary_layers(&base_layers, &roots);
        let enabled = if base.config.allow_dictionary_path {
            layers.as_slice()
        } else {
            &[]
        };
        let (registry, events) = store
            .build_registry(enabled, &base.config.default_version, &context)
            .map_err(|error| error.to_string())?;
        append_events(
            &mut base.warnings,
            events,
            &base_layers,
            &input.client_settings.origin,
        );
        Arc::new(registry)
    } else {
        input.registry.clone()
    };
    let (session_name, session) = if input.refresh_global {
        match input.session.as_deref() {
            Some(raw) => match registry.choice(raw) {
                Some(choice) => (input.session.clone(), Some(choice)),
                None => {
                    base.warnings.push(session_dictionary_gone(raw));
                    (None, None)
                }
            },
            None => (None, None),
        }
    } else {
        (input.session.clone(), input.session_choice.clone())
    };
    let mut documents = Vec::new();
    let document_inputs = std::mem::take(&mut input.documents);
    let shared_base = Arc::new(base.clone());
    let global = PreparedGlobal {
        input: &input,
        layers: &base_layers,
        resolved: &shared_base,
        registry: &registry,
        session: session.clone(),
    };
    for document in document_inputs {
        let prepared = prepare_document(&global, &document, cache, store)?;
        documents.push((document, prepared));
    }
    Ok(Prepared {
        refresh_global: input.refresh_global,
        refresh_projects: input.refresh_projects,
        user,
        client: input.client_settings,
        base,
        registry,
        session: session_name,
        session_choice: session,
        session_revision: input.session_revision,
        documents,
    })
}

fn source_path(source: &str) -> PathBuf {
    Uri::parse(source)
        .ok()
        .and_then(|uri| uri.to_file_path().ok())
        .unwrap_or_else(|| PathBuf::from(source))
}

fn layer_roots(layers: &[SettingsLayer], root: Option<&Path>) -> Vec<Option<PathBuf>> {
    layers
        .iter()
        .map(|layer| match layer.origin.layer {
            SourceKind::UserFile | SourceKind::Project => source_path(&layer.origin.source)
                .parent()
                .map(Path::to_owned),
            _ => root.map(Path::to_owned),
        })
        .collect()
}

fn dictionary_layers<'a>(
    layers: &'a [SettingsLayer],
    roots: &'a [Option<PathBuf>],
) -> Vec<DictionaryLayer<'a>> {
    layers
        .iter()
        .zip(roots)
        .map(|(layer, root)| DictionaryLayer {
            specs: &layer.dictionaries,
            source: &layer.origin.source,
            root_dir: root.as_deref(),
        })
        .collect()
}

fn append_events(
    warnings: &mut Vec<Warning>,
    events: Vec<crate::dictionary::RegistryEvent>,
    layers: &[SettingsLayer],
    origin: &Origin,
) {
    warnings.extend(events.into_iter().map(|event| {
        use crate::dictionary::RegistryEvent;
        let source = layers
            .iter()
            .rev()
            .find(|layer| {
                layer.dictionaries.iter().any(|spec| match &event {
                    RegistryEvent::LoadFailed { path, .. }
                    | RegistryEvent::DuplicateDefault { path, .. }
                    | RegistryEvent::BlankName { path } => spec.path == *path,
                    RegistryEvent::BadName { name, .. } => spec.name == *name,
                    RegistryEvent::DefaultVersionUnresolved { .. } => false,
                })
            })
            .map_or(origin, |layer| &layer.origin);
        Warning {
            key: "dictionaries".into(),
            message: event.message(),
            origin: source.clone(),
            severity: if event.is_error() {
                Severity::Error
            } else {
                Severity::Warning
            },
        }
    }));
}

fn select(
    raw: &str,
    registry: &Registry,
    context: &BuildContext<'_>,
    enabled: bool,
    layers: &[SettingsLayer],
    store: &DictionaryStore,
) -> Result<DictionaryChoice, LoadError> {
    let reserved = crate::dictionary::is_reserved_selector(raw);
    if let Some(choice) = registry.choice(raw) {
        return if reserved || enabled {
            Ok(choice)
        } else {
            Err(LoadError::PathsDisabled)
        };
    }
    if reserved {
        return Err(LoadError::InvalidVersion {
            path: PathBuf::from(raw),
            version: raw.into(),
        });
    }
    let declared = layers
        .iter()
        .any(|layer| layer.dictionaries.iter().any(|spec| spec.name == raw));
    if declared || !looks_like_path(raw) {
        return Err(LoadError::UnknownName { name: raw.into() });
    }
    if !enabled {
        return Err(LoadError::PathsDisabled);
    }
    store.load_choice(raw, context)
}

fn blocked_dictionaries(layers: &[SettingsLayer], label: &str, origin: &Origin) -> Option<Warning> {
    let names: Vec<_> = layers
        .iter()
        .flat_map(|layer| &layer.dictionaries)
        .map(|spec| format!("{} ({})", spec.name, spec.path))
        .collect();
    (!names.is_empty()).then(|| {
        Warning::new(
            "dictionaries",
            format!(
                "allowDictionaryPath is false; {label} ignored: {}",
                names.join(", ")
            ),
            origin.clone(),
        )
    })
}

/// Batch-wide state borrowed while preparing each document.
struct PreparedGlobal<'a> {
    /// Batch input shared by every document.
    input: &'a Input,
    /// Global layers preceding document-local layers.
    layers: &'a [SettingsLayer],
    /// Prepared global merge.
    resolved: &'a Arc<Resolved>,
    /// Prepared global registry.
    registry: &'a Arc<Registry>,
    /// Active session choice, if still valid.
    session: Option<DictionaryChoice>,
}

fn prepare_document(
    global: &PreparedGlobal<'_>,
    document: &DocumentInput,
    cache: &mut project::BatchCache,
    store: &DictionaryStore,
) -> Result<PreparedDocument, String> {
    let input = global.input;
    let base = global.resolved;
    let base_layers = global.layers;
    let path = document.uri.to_file_path().ok();
    let projects = if !base.config.overrides.project_config {
        Vec::new()
    } else if !input.refresh_projects {
        document
            .previous
            .as_ref()
            .map(|previous| previous.project.layers.clone())
            .unwrap_or_default()
    } else if let Some(path) = &path {
        project_layers(
            path.as_ref(),
            &input.roots,
            input.root_uri.as_deref(),
            cache,
        )?
    } else {
        Vec::new()
    };
    let mut layers = base_layers.to_vec();
    if global.session.is_some() {
        let raw = input.session.as_ref().expect("session choice has a name");
        let mut layer = SettingsLayer::new(Origin::new(SourceKind::Session, "session"));
        layer.set("dictionary", raw.clone().into(), layer.origin.clone());
        layers.push(layer);
    }
    layers.extend(projects.iter().cloned());
    if base.config.overrides.modeline {
        layers.push(document.modeline.clone());
    }
    let mut resolved = overrides::merge(&layers, RuleRegistry::known_ids())?;
    resolved.warnings.extend(base.warnings.clone());
    if !base.config.allow_dictionary_path {
        for layer in &projects {
            resolved.warnings.extend(blocked_dictionaries(
                std::slice::from_ref(layer),
                "project dictionaries",
                &layer.origin,
            ));
        }
    }
    let context = BuildContext {
        root_dir: input.dictionary_root.as_deref(),
        home_dir: input.home.as_deref(),
        limits: &base.config.limits,
    };
    let sources_unchanged = !input.refresh_global && !input.refresh_projects;
    if sources_unchanged {
        if let Some(previous) = &document.previous {
            resolved
                .warnings
                .extend(previous.project.warnings.iter().cloned());
        }
    }
    let registry = if sources_unchanged {
        document.previous.as_ref().map_or_else(
            || global.registry.clone(),
            |previous| previous.project.registry.clone(),
        )
    } else if !base.config.allow_dictionary_path
        || projects.iter().all(|layer| layer.dictionaries.is_empty())
    {
        global.registry.clone()
    } else {
        let mut definition_layers = base_layers.to_vec();
        definition_layers.extend(projects.iter().cloned());
        let roots = layer_roots(&definition_layers, input.dictionary_root.as_deref());
        let definitions = dictionary_layers(&definition_layers, &roots);
        let (registry, events) = store
            .build_registry(&definitions, &base.config.default_version, &context)
            .map_err(|error| error.to_string())?;
        append_events(
            &mut resolved.warnings,
            events,
            &definition_layers,
            &Origin::new(SourceKind::Project, document.uri.as_str()),
        );
        Arc::new(registry)
    };
    let mut selection = global.session.clone().and_then(|choice| {
        input.session.as_ref().map(|raw| Selection {
            raw: raw.clone(),
            origin: Origin::new(SourceKind::Session, "session"),
            choice,
        })
    });
    // A rejected stronger selector falls through to the next valid selector.
    for layer in layers
        .iter()
        .rev()
        .filter(|layer| layer.origin.layer != SourceKind::Session)
    {
        let Some(candidate) = layer.candidate("dictionary") else {
            continue;
        };
        let Some(raw) = candidate.value.as_str() else {
            continue;
        };
        let origin = &candidate.origin;
        let source = source_path(&layer.origin.source);
        let root = match layer.origin.layer {
            SourceKind::Project => source.parent(),
            SourceKind::Modeline => path.as_deref().and_then(Path::parent),
            _ => input.dictionary_root.as_deref(),
        };
        let selected = select(
            raw,
            &registry,
            &BuildContext {
                root_dir: root,
                ..context
            },
            base.config.allow_dictionary_path,
            &layers,
            store,
        );
        match selected {
            Ok(choice) => {
                selection = Some(Selection {
                    raw: raw.into(),
                    origin: origin.clone(),
                    choice,
                });
                break;
            }
            Err(error) if error.is_fatal() => return Err(error.to_string()),
            Err(error) => resolved.warnings.push(Warning::new(
                "dictionary",
                error.to_string(),
                origin.clone(),
            )),
        }
    }
    resolved.values.insert(
        "dictionary".into(),
        selection
            .as_ref()
            .map_or(serde_json::Value::Null, |selection| {
                selection.raw.clone().into()
            }),
    );
    resolved.sources.insert(
        "dictionary".into(),
        selection.as_ref().map_or_else(
            || Origin::new(SourceKind::Default, "default"),
            |selection| selection.origin.clone(),
        ),
    );
    let mut unique = Vec::new();
    for warning in std::mem::take(&mut resolved.warnings) {
        if !unique.contains(&warning) {
            unique.push(warning);
        }
    }
    resolved.warnings = unique;
    let project_warnings = if sources_unchanged {
        document
            .previous
            .as_ref()
            .map(|previous| previous.project.warnings.clone())
            .unwrap_or_default()
    } else {
        resolved
            .warnings
            .iter()
            .filter(|warning| warning.origin.layer == SourceKind::Project)
            .cloned()
            .collect()
    };
    Ok(PreparedDocument {
        resolved,
        project: ProjectSnapshot {
            layers: projects,
            registry,
            warnings: project_warnings,
        },
        choice: selection.map(|selection| selection.choice),
    })
}

fn project_layers(
    document: &Path,
    roots: &[PathBuf],
    root_uri: Option<&Path>,
    cache: &mut project::BatchCache,
) -> Result<Vec<SettingsLayer>, String> {
    let canonical = project::canonical_document(document).map_err(|error| error.to_string())?;
    let paths = project::discover(document, roots, root_uri).map_err(|error| error.to_string())?;
    let mut layers = Vec::new();
    for path in paths {
        let loaded = cache
            .load_discovered(&path)
            .map_err(|error| error.to_string())?;
        if let project::LoadedFile::Valid(file) = &loaded {
            if !file.matches(&canonical) {
                continue;
            }
        }
        layers.push(loaded_layer(
            loaded,
            Origin::new(SourceKind::Project, project::source_name(&path)),
        ));
    }
    Ok(layers)
}
