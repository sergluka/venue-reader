//! Configuration batches commit only fully prepared snapshots.
//!
//! The main loop owns the committed configuration and permits one batch at a
//! time. [`super::external`] reads filesystem-backed sources on its worker;
//! results return here for freshness checks and atomic commit. Document changes
//! wait behind the active batch, while global updates coalesce to the newest
//! request. Invalid client configuration keeps the last committed state live.

use super::external::{self, DocumentConfig, DocumentInput, Input, Job, Prepared, Report};
use super::{OutgoingRequest, ServerState};
use crate::config::{Config, Severity};
use crate::ctx::Ctx;
use crate::overrides::{self, SettingsLayer, SourceKind};
use lsp_server::{ErrorCode, ResponseError};
use lsp_types::{Diagnostic, MessageType, Position, Range, Uri};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// Main-loop-owned configuration state, split into committed, pending and queued work.
pub struct ConfigState {
    /// Effective committed global configuration.
    pub base: Config,
    /// Monotone identity assigned to configuration batches.
    pub global_generation: u64,
    /// Last committed user-file layer.
    pub(super) user: SettingsLayer,
    /// Last committed client-provided layer.
    pub(super) client: SettingsLayer,
    /// Last committed global merge, including sources and warnings.
    pub(super) base_resolved: overrides::Resolved,
    /// Effective committed state for each open document.
    pub(super) documents: HashMap<Uri, Arc<DocumentConfig>>,
    /// Open/close identity used to reject results from an earlier lifetime.
    pub(super) lifetimes: HashMap<Uri, u64>,
    /// Modeline revision used to reject results prepared from stale overrides.
    pub(super) revisions: HashMap<Uri, u64>,
    /// Most recently scanned modeline layer for each open document.
    pub(super) modelines: HashMap<Uri, SettingsLayer>,
    /// Documents requiring preparation after the active batch finishes.
    pub(super) waiting: HashSet<Uri>,
    /// Whether the next document batch must reread project files.
    waiting_projects: bool,
    /// The batch waiting for client input or worker output.
    pub(super) pending: Option<Batch>,
    /// Newest global refresh received while another batch is active.
    queued_global: Option<GlobalRefresh>,
    /// Source of unique document lifetime identities.
    pub(super) next_lifetime: u64,
    /// Whether invalid client configuration blocks incremental batches.
    pub(super) rejected: bool,
    /// Committed session-level dictionary selector.
    pub(super) session: Option<String>,
    /// Initialization settings consumed by the first global pull.
    pub(super) initial_client: Option<SettingsLayer>,
    /// Dictionary choice resolved from `session`.
    pub(super) session_choice: Option<crate::dictionary::DictionaryChoice>,
    /// Changes whenever the committed session selection changes.
    session_revision: u64,
    /// Changes whenever client configuration is accepted or rejected.
    validity_revision: u64,
}

impl Default for ConfigState {
    fn default() -> Self {
        Self {
            base: Config::default(),
            global_generation: 0,
            user: external::empty_user(),
            client: overrides::parse_client(json!({})).expect("empty client"),
            base_resolved: overrides::merge(&[], crate::rules::Registry::known_ids())
                .expect("defaults"),
            documents: HashMap::new(),
            lifetimes: HashMap::new(),
            revisions: HashMap::new(),
            modelines: HashMap::new(),
            waiting: HashSet::new(),
            waiting_projects: false,
            pending: None,
            queued_global: None,
            next_lifetime: 0,
            rejected: false,
            session: None,
            initial_client: None,
            session_choice: None,
            session_revision: 0,
            validity_revision: 0,
        }
    }
}

/// One configuration transaction from capture through worker completion.
pub(super) struct Batch {
    /// Identity used to discard replies and reports from superseded work.
    pub generation: u64,
    /// Deadline origin covering both the client pull and worker preparation.
    pub started: Instant,
    /// Present until an optional client pull completes and the job is submitted.
    input: Option<Input>,
    /// Client-validity revision that authorized clearing the rejected state,
    /// when a successful commit may clear it at all.
    clears_rejected_at: Option<u64>,
}

/// A global refresh request that may wait behind the active batch.
struct GlobalRefresh {
    /// Parsed client layer, or `None` to retain or pull the current value.
    client: Option<SettingsLayer>,
    /// Whether to request the current layer through `workspace/configuration`.
    pull: bool,
    /// Client-validity revision whose success proves that the latest rejected
    /// input was replaced, when it proves anything.
    clears_rejected_at: Option<u64>,
}

impl ServerState {
    /// Return the committed effective configuration for an open document.
    pub(super) fn effective_config(&self, uri: &Uri) -> Option<&Config> {
        self.config
            .documents
            .get(uri)
            .map(|document| &document.resolved.config)
    }

    /// Duration until the configuration deadline, or a long idle interval.
    pub(super) fn config_tick_in(&self) -> std::time::Duration {
        self.config
            .pending
            .as_ref()
            .map_or(std::time::Duration::from_secs(3600), |batch| {
                external::TIMEOUT.saturating_sub(batch.started.elapsed())
            })
    }

    /// Turn an expired batch into a fatal server error.
    pub(super) fn check_config_deadline(&mut self) {
        if self
            .config
            .pending
            .as_ref()
            .is_some_and(|batch| batch.started.elapsed() >= external::TIMEOUT)
        {
            self.fail_configuration("configuration batch exceeded 500 ms".into());
        }
    }

    /// Record a configuration failure that makes the main loop terminate.
    pub(super) fn fail_configuration(&mut self, message: String) {
        log::error!("fix-lsp: {message}");
        eprintln!("fix-lsp: {message}");
        self.fatal = Some(message);
    }

    /// Parse pushed client settings and start a global refresh when valid.
    pub(super) fn apply_base_value(&mut self, value: &Value) {
        match overrides::parse_client(if value.is_null() {
            json!({})
        } else {
            value.clone()
        }) {
            Ok(client) => {
                self.config.validity_revision += 1;
                self.begin_batch(Refresh::Global(GlobalRefresh {
                    client: Some(client),
                    pull: false,
                    clears_rejected_at: Some(self.config.validity_revision),
                }));
            }
            Err(error) => self.reject_configuration(&error),
        }
    }

    /// Handle `workspace/didChangeConfiguration` in pull or push mode.
    pub(super) fn on_did_change_configuration(&mut self, params: Value) {
        if self.client.configuration {
            self.pull_base_config();
        } else if let Some(value) = params.pointer("/settings/fix") {
            self.apply_base_value(value);
        } else {
            self.begin_batch(Refresh::Global(GlobalRefresh {
                client: None,
                pull: false,
                clears_rejected_at: None,
            }));
        }
    }

    /// Refresh global configuration through the client's negotiated delivery mode.
    pub(super) fn pull_base_config(&mut self) {
        if self.config.rejected
            && !self.client.configuration
            && self.config.initial_client.is_none()
        {
            return;
        }
        let initial = self.config.initial_client.take();
        self.begin_batch(Refresh::Global(GlobalRefresh {
            client: initial,
            pull: self.client.configuration,
            clears_rejected_at: None,
        }));
    }

    /// Capture the freshness keys and reusable state for one open document.
    fn capture_document(&self, uri: &Uri) -> Option<DocumentInput> {
        let open = self.docs.get(uri)?;
        Some(DocumentInput {
            uri: uri.clone(),
            lifetime: *self.config.lifetimes.get(uri)?,
            revision: *self.config.revisions.get(uri)?,
            modeline: modeline_layer(open, uri),
            previous: self.config.documents.get(uri).cloned(),
        })
    }

    /// Start a batch or queue a newer global refresh behind the active one.
    fn begin_batch(&mut self, refresh: Refresh) {
        let projects_only = matches!(refresh, Refresh::Projects);
        let (client, global, pull, clears_rejected_at) = match refresh {
            Refresh::Global(refresh) => {
                if self.config.pending.is_some() {
                    self.config.queued_global = Some(refresh);
                    self.requeue_all_documents();
                    return;
                }
                (
                    refresh.client,
                    true,
                    refresh.pull,
                    refresh.clears_rejected_at,
                )
            }
            Refresh::Projects | Refresh::Documents => (None, false, false, None),
        };
        let started = Instant::now();
        debug_assert!(self.config.pending.is_none());
        let projects = global || projects_only || self.config.waiting_projects;
        self.config.global_generation += 1;
        let generation = self.config.global_generation;
        let documents: Vec<DocumentInput> = if global {
            self.docs
                .keys()
                .filter_map(|uri| self.capture_document(uri))
                .collect()
        } else {
            self.config
                .waiting
                .iter()
                .filter_map(|uri| self.capture_document(uri))
                .collect()
        };
        self.config.waiting.clear();
        self.config.waiting_projects = false;
        let input = Input {
            generation,
            user_settings: self.config.user.clone(),
            client_settings: client.unwrap_or_else(|| self.config.client.clone()),
            base_settings: self.config.base_resolved.clone(),
            user_path: self.user_path.clone(),
            refresh_global: global,
            refresh_projects: projects,
            roots: self.workspace_folders.clone(),
            root_uri: self.root_uri.clone(),
            dictionary_root: self.root_dir.clone(),
            home: self.home_dir.clone(),
            registry: self.registry.clone(),
            session: self.config.session.clone(),
            session_choice: self.config.session_choice.clone(),
            session_revision: self.config.session_revision,
            documents,
        };
        self.config.pending = Some(Batch {
            generation,
            started,
            input: Some(input),
            clears_rejected_at,
        });
        if pull {
            let request = self.req_queue.outgoing.register(
                super::method::CONFIGURATION.into(),
                json!({"items":[{"section":"fix"}]}),
                OutgoingRequest::BaseConfig { generation },
            );
            self.sender.send(request);
        } else {
            self.submit_batch();
        }
    }

    /// Move a fully populated pending input to the configuration worker.
    fn submit_batch(&mut self) {
        if let Some(input) = self
            .config
            .pending
            .as_mut()
            .and_then(|batch| batch.input.take())
        {
            if let Err(error) = self.loader.send(Job::Batch(Box::new(input))) {
                self.fail_configuration(error);
            }
        }
    }

    /// Validate a `workspace/configuration` reply before submitting its batch.
    pub(super) fn on_base_reply(&mut self, generation: u64, result: Result<Value, ResponseError>) {
        if self
            .config
            .pending
            .as_ref()
            .is_none_or(|batch| batch.generation != generation || batch.input.is_none())
        {
            return;
        }
        let value = match result {
            Ok(Value::Array(mut values)) if values.len() == 1 => values.pop().expect("one item"),
            Ok(_) => {
                self.abort_pending_configuration();
                self.reject_configuration("workspace/configuration must return one item");
                self.start_waiting_documents();
                return;
            }
            Err(error) => {
                self.fail_configuration(format!(
                    "workspace/configuration failed: {}",
                    error.message
                ));
                return;
            }
        };
        match overrides::parse_client(if value.is_null() { json!({}) } else { value }) {
            Ok(client) => {
                self.config.validity_revision += 1;
                self.config
                    .pending
                    .as_mut()
                    .expect("pending")
                    .input
                    .as_mut()
                    .expect("not submitted")
                    .client_settings = client;
                let batch = self.config.pending.as_mut().expect("pending");
                batch.clears_rejected_at = Some(self.config.validity_revision);
                self.submit_batch();
            }
            Err(error) => {
                self.abort_pending_configuration();
                self.reject_configuration(&error);
                self.start_waiting_documents();
            }
        }
    }

    /// Drop an unsubmitted batch and preserve its documents for another attempt.
    fn abort_pending_configuration(&mut self) {
        self.config.pending = None;
        self.requeue_all_documents();
    }

    /// Hold every open document for a later batch. A document left out of
    /// `waiting` is one nothing will recompute.
    fn requeue_all_documents(&mut self) {
        self.config.waiting.extend(self.docs.keys().cloned());
        self.config.waiting_projects = true;
    }

    /// Keep committed state active while reporting invalid client configuration.
    fn reject_configuration(&mut self, error: &str) {
        self.config.validity_revision += 1;
        self.config.rejected = true;
        self.report_config_messages(vec![(
            MessageType::Error,
            format!("configuration rejected; keeping committed state: {error}"),
        )]);
    }

    /// Validate a worker report against the active generation and commit it.
    pub(super) fn on_configuration_report(&mut self, report: Report) {
        let Report::Batch(generation, result) = report else {
            return;
        };
        if self
            .config
            .pending
            .as_ref()
            .is_none_or(|batch| batch.generation != generation)
        {
            return;
        }
        self.check_config_deadline();
        if self.fatal.is_some() {
            return;
        }
        let batch = self.config.pending.take().expect("matching batch");
        match result {
            Err(error) => self.fail_configuration(error),
            Ok(prepared) => {
                self.commit_configuration(*prepared);
                if batch.clears_rejected_at == Some(self.config.validity_revision) {
                    self.config.rejected = false;
                }
                self.start_waiting_documents();
            }
        }
    }

    /// Commit fresh portions of a prepared batch and schedule affected channels.
    ///
    /// Global state commits as one unit. Each document additionally checks its
    /// lifetime, modeline revision and session revision; stale documents return
    /// to `waiting` instead of mixing old local state into the new global state.
    fn commit_configuration(&mut self, mut prepared: Prepared) {
        let mut hints = false;
        let mut lenses = false;
        let mut tokens = false;
        let mut diagnostics = Vec::new();
        let base_unchanged = self.config.base == prepared.base.config;
        self.config.user = prepared.user;
        self.config.client = prepared.client;
        self.config.base = prepared.base.config.clone();
        self.registry = prepared.registry;
        let session_current = prepared.session_revision == self.config.session_revision;
        if session_current {
            self.config.session = prepared.session;
            self.config.session_choice = prepared.session_choice;
        } else {
            prepared
                .base
                .warnings
                .retain(|warning| warning.origin.layer != SourceKind::Session);
            if prepared.refresh_global {
                match self.config.session.as_deref() {
                    Some(raw) => match self.registry.choice(raw) {
                        Some(choice) => self.config.session_choice = Some(choice),
                        None => {
                            prepared
                                .base
                                .warnings
                                .push(external::session_dictionary_gone(raw));
                            self.config.session = None;
                            self.config.session_choice = None;
                            self.config.session_revision += 1;
                        }
                    },
                    None => self.config.session_choice = None,
                }
            }
        }
        self.config.base_resolved = prepared.base;
        let candidate = Ctx::new(&self.config.base, self.registry.clone(), None);
        if candidate.fingerprint() != self.ctx.fingerprint() {
            self.ctx = candidate;
        }
        if !base_unchanged {
            let (render, bad_templates) = crate::render::Options::from_config(&self.config.base);
            self.render = Arc::new(render);
            self.base_bad_templates = Arc::new(bad_templates);
            self.rules = Arc::new(crate::rules::Registry::from_config(
                &self.config.base.diagnostics,
            ));
        }
        for (input, mut document) in prepared.documents {
            if self.config.lifetimes.get(&input.uri) != Some(&input.lifetime) {
                continue;
            }
            if self.config.revisions.get(&input.uri) != Some(&input.revision) {
                self.config.waiting.insert(input.uri);
                self.config.waiting_projects |= prepared.refresh_projects;
                continue;
            }
            if !session_current {
                self.config.waiting.insert(input.uri);
                continue;
            }
            self.config
                .modelines
                .insert(input.uri.clone(), input.modeline);
            let previous = self.config.documents.get(&input.uri);
            let fingerprint = Ctx::fingerprint_for(
                &document.resolved.config,
                &document.project.registry,
                document.choice.as_ref(),
            );
            let ctx = if fingerprint == self.ctx.fingerprint() {
                self.ctx.clone()
            } else {
                previous
                    .filter(|previous| previous.ctx.fingerprint() == fingerprint)
                    .map_or_else(
                        || {
                            Ctx::with_choice(
                                &document.resolved.config,
                                document.project.registry.clone(),
                                document.choice.clone(),
                            )
                        },
                        |previous| previous.ctx.clone(),
                    )
            };
            let base_config = document.resolved.config == self.config.base;
            let (render, invalid_templates) = if base_config {
                (self.render.clone(), self.base_bad_templates.clone())
            } else {
                let (options, bad) = crate::render::Options::from_config(&document.resolved.config);
                let render = previous
                    .filter(|previous| previous.resolved.config == document.resolved.config)
                    .map_or_else(|| Arc::new(options), |previous| previous.render.clone());
                (render, Arc::new(bad))
            };
            for template in invalid_templates.iter() {
                let origin = document
                    .resolved
                    .sources
                    .get(template.setting())
                    .cloned()
                    .unwrap_or_else(|| overrides::Origin::new(SourceKind::Default, "default"));
                document.resolved.warnings.push(overrides::Warning::new(
                    template.setting(),
                    template.error().to_string(),
                    origin,
                ));
            }
            let rules = if document.resolved.config.diagnostics == self.config.base.diagnostics
                && ctx.selected_version().is_none()
            {
                self.rules.clone()
            } else {
                previous
                    .filter(|previous| {
                        previous.resolved.config.diagnostics == document.resolved.config.diagnostics
                            && previous.ctx.selected_version() == ctx.selected_version()
                    })
                    .map_or_else(
                        || {
                            Arc::new(crate::rules::Registry::with_selected_version(
                                &document.resolved.config.diagnostics,
                                ctx.selected_version(),
                            ))
                        },
                        |previous| previous.rules.clone(),
                    )
            };
            let names = Arc::new(crate::ctx::DictionaryNames::new(
                document.project.registry.clone(),
                document.choice.as_ref(),
            ));
            let document = Arc::new(DocumentConfig {
                resolved: document.resolved,
                project: document.project,
                ctx,
                names,
                render,
                rules,
            });
            let interpretation =
                previous.is_none_or(|previous| !Arc::ptr_eq(&previous.ctx, &document.ctx));
            let config = &document.resolved.config;
            hints |= interpretation
                || previous.is_none_or(|previous| {
                    previous.resolved.config.inlay_hints != config.inlay_hints
                        || previous.resolved.config.message_summary != config.message_summary
                        || previous.resolved.config.document_link != config.document_link
                        || previous.resolved.config.limits != config.limits
                });
            lenses |= interpretation
                || previous.is_none_or(|previous| {
                    previous.resolved.config.message_summary != config.message_summary
                        || previous.resolved.config.limits != config.limits
                });
            let token_changed = interpretation
                || previous.is_none_or(|previous| {
                    previous.resolved.config.semantic_tokens != config.semantic_tokens
                        || previous.resolved.config.limits != config.limits
                });
            tokens |= token_changed;
            if token_changed {
                self.tokens.remove(&input.uri);
            }
            if interpretation
                || previous.is_none_or(|previous| {
                    previous.resolved.config.diagnostics != config.diagnostics
                        || previous.resolved.config.limits != config.limits
                        || previous.resolved.warnings != document.resolved.warnings
                })
            {
                diagnostics.push(input.uri.clone());
            }
            if let Some(open) = self.docs.get_mut(&input.uri) {
                open.set_ctx(document.ctx.clone());
            }
            let warnings = configuration_diagnostics(
                &document,
                &input.uri,
                self.docs.get(&input.uri),
                self.encoding,
            );
            self.set_configuration_diagnostics(&input.uri, warnings);
            self.config.documents.insert(input.uri, document);
        }
        let messages = self
            .config
            .base_resolved
            .warnings
            .iter()
            .map(|warning| {
                (
                    if warning.severity == Severity::Error {
                        MessageType::Error
                    } else {
                        MessageType::Warning
                    },
                    warning.message.clone(),
                )
            })
            .collect();
        self.report_config_messages(messages);
        for uri in diagnostics {
            self.diagnostics_changed_for(&uri);
        }
        self.refresh_configuration(hints, lenses, tokens);
    }

    /// Ask capable clients to re-request channels affected by the commit.
    fn refresh_configuration(&mut self, hints: bool, lenses: bool, tokens: bool) {
        if !self.started {
            return;
        }
        for (enabled, method, what) in [
            (
                hints && self.client.refreshes_inlay_hints,
                super::method::INLAY_HINT_REFRESH,
                "inlay hint",
            ),
            (
                lenses && self.client.refreshes_code_lens,
                super::method::CODE_LENS_REFRESH,
                "code lens",
            ),
            (
                tokens && self.client.refreshes_semantic_tokens,
                super::method::SEMANTIC_TOKENS_REFRESH,
                "semantic tokens",
            ),
        ] {
            if enabled {
                let request = self.req_queue.outgoing.register(
                    method.into(),
                    Value::Null,
                    OutgoingRequest::Refresh { what },
                );
                self.sender.send(request);
            }
        }
    }

    /// Log every message but show only messages absent from the previous set.
    fn report_config_messages(&mut self, messages: Vec<(MessageType, String)>) {
        let mut texts = Vec::new();
        for (kind, message) in messages {
            log::warn!("{message}");
            self.log_message(kind, &message);
            if !self.last_config_messages.contains(&message) {
                self.show_message(kind, &message);
            }
            texts.push(message);
        }
        self.last_config_messages = texts;
    }

    /// Register a new document lifetime and schedule its initial configuration.
    pub(super) fn configure_open_document(&mut self, uri: &Uri) {
        self.config.next_lifetime += 1;
        self.config
            .lifetimes
            .insert(uri.clone(), self.config.next_lifetime);
        self.config.revisions.insert(uri.clone(), 0);
        self.config.documents.remove(uri);
        self.rescan_modeline(uri);
        self.config.waiting.insert(uri.clone());
        self.config.waiting_projects = true;
        if self.config.pending.is_none() && !self.config.rejected {
            self.begin_batch(Refresh::Projects);
        }
    }

    /// Rescan a modeline and advance its revision only when the layer changed.
    pub(super) fn rescan_modeline(&mut self, uri: &Uri) -> bool {
        let Some(document) = self.docs.get(uri) else {
            return false;
        };
        let modeline = modeline_layer(document, uri);
        if self.config.modelines.get(uri) == Some(&modeline) {
            return false;
        }
        self.config.modelines.insert(uri.clone(), modeline);
        *self.config.revisions.entry(uri.clone()).or_default() += 1;
        true
    }

    /// Schedule reconfiguration when an edit changes effective modeline input.
    pub(super) fn configure_edited_document(&mut self, uri: &Uri) {
        if !self.rescan_modeline(uri) {
            return;
        }
        if self.config.base.overrides.modeline || self.config.pending.is_some() {
            self.config.waiting.insert(uri.clone());
            if self.config.pending.is_none() && !self.config.rejected {
                self.begin_batch(Refresh::Documents);
            }
        }
    }

    /// Apply workspace-folder changes and reread project configuration.
    pub(super) fn on_workspace_folders(&mut self, params: Value) {
        let event = &params["event"];
        if let Some(removed) = event["removed"].as_array() {
            for folder in removed {
                if let Some(path) = folder_path(folder) {
                    self.workspace_folders.retain(|existing| existing != &path);
                }
            }
        }
        if let Some(added) = event["added"].as_array() {
            for folder in added {
                if let Some(path) = folder_path(folder) {
                    if !self.workspace_folders.contains(&path) {
                        self.workspace_folders.push(path);
                    }
                }
            }
        }
        self.requeue_all_documents();
        self.start_waiting_documents();
    }

    /// Commit a named global dictionary as the session selection.
    pub(super) fn select_dictionary(&mut self, raw: String) -> Result<Value, ResponseError> {
        if self.config.rejected {
            return Err(request_failed(
                "configuration is not ready; send a valid configuration update",
            ));
        }
        let Some(choice) = self.registry.choice(&raw) else {
            return Err(request_failed(
                "fix.useDictionary accepts bundled versions and committed global dictionary names",
            ));
        };
        self.config.session = Some(raw);
        self.config.session_choice = Some(choice.clone());
        self.config.session_revision += 1;
        let result = json!({"name":choice.name,"version":choice.version,"source":choice.source});
        self.config.waiting.extend(self.docs.keys().cloned());
        self.start_waiting_documents();
        Ok(result)
    }

    /// Prefer a queued global refresh, then prepare any waiting documents.
    fn start_waiting_documents(&mut self) {
        if self.config.pending.is_some() {
            return;
        }
        if let Some(refresh) = self.config.queued_global.take() {
            self.begin_batch(Refresh::Global(refresh));
            return;
        }
        if !self.config.waiting.is_empty() && !self.config.rejected {
            // `begin_batch` folds in `waiting_projects`.
            self.begin_batch(Refresh::Documents);
        }
    }

    /// Forget pending and queued work during server shutdown.
    pub(super) fn stop_configuration(&mut self) {
        self.config.pending = None;
        self.config.queued_global = None;
    }
}

/// Parse edge modelines even when currently disabled.
///
/// The merge applies the enable setting, so a batch that enables modelines must
/// already carry the parsed layer rather than require a second pass.
fn modeline_layer(document: &crate::document::Document, uri: &Uri) -> SettingsLayer {
    if uri.to_file_path().is_err() {
        return SettingsLayer::new(overrides::Origin::new(SourceKind::Modeline, uri.as_str()));
    }
    let count = document.content_lines();
    overrides::modeline::parse_numbered(
        overrides::modeline::edge_indices(count)
            .map(|index| (index, document.lines[index].bytes.as_ref())),
        count,
        uri.as_str(),
    )
}

/// Convert a workspace-folder JSON value into a local filesystem path.
pub(super) fn folder_path(folder: &Value) -> Option<PathBuf> {
    folder["uri"]
        .as_str()
        .and_then(|uri| Uri::parse(uri).ok())
        .and_then(|uri| uri.to_file_path().ok())
}

/// Scope of external configuration input that a batch must refresh.
enum Refresh {
    /// User, client and every project layer, for every open document.
    Global(GlobalRefresh),
    /// Project layers for all waiting documents.
    Projects,
    /// Already captured layers for waiting documents only.
    Documents,
}

/// Build the standard LSP error returned for rejected configuration commands.
pub(super) fn request_failed(message: &str) -> ResponseError {
    ResponseError {
        code: ErrorCode::RequestFailed as i32,
        message: message.into(),
        data: None,
    }
}

/// Render project and modeline warnings as document diagnostics.
fn configuration_diagnostics(
    document: &DocumentConfig,
    uri: &Uri,
    open: Option<&crate::document::Document>,
    encoding: crate::document::PositionEncoding,
) -> Vec<Diagnostic> {
    document
        .resolved
        .warnings
        .iter()
        .filter(|warning| {
            matches!(
                warning.origin.layer,
                SourceKind::Modeline | SourceKind::Project
            )
        })
        .map(|warning| {
            let modeline = warning.origin.layer == SourceKind::Modeline
                && warning.origin.source == uri.as_str();
            let range = if modeline {
                let line = open.and_then(|open| open.lines.get(warning.origin.line));
                super::origin_range(&warning.origin, line.map(Arc::as_ref), encoding)
            } else {
                Range::new(Position::new(0, 0), Position::new(0, 0))
            };
            Diagnostic {
                range,
                code: Some(
                    if modeline {
                        "modeline"
                    } else {
                        "projectConfig"
                    }
                    .into(),
                ),
                severity: Some(super::diagnostics::lsp_severity(warning.severity)),
                source: Some("fix".into()),
                message: format!("{}: {}", warning.origin.source, warning.message).into(),
                ..Default::default()
            }
        })
        .collect()
}
