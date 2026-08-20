//! Server main loop and message dispatch.
//!
//! Single-threaded in Stage 1: each request `prepare`s a task over a [`Snapshot`]
//! and runs it inline. Responses go through one point via [`ReqQueue`]; every
//! handler is wrapped in `catch_unwind` so a panic becomes an `InternalError` and
//! the loop survives. The worker pool is deferred to Stage 3/4 (see `dispatch`).
//!
//! This file owns only the loop, the dispatch tables, and the shared state. Each
//! capability lives in its own module and adds an `impl ServerState` block there;
//! later stages add a module per capability rather than growing this one.

pub mod capabilities;
mod code_action;
mod code_lens;
pub mod config_pull;
#[cfg(test)]
mod configuration_tests;
pub mod diagnostics;
pub mod dispatch;
mod document_symbol;
mod execute_command;
mod external;
pub mod hover;
mod inlay_hint;
pub mod lifecycle;
mod method;
mod selection_range;
mod semantic_tokens;
#[cfg(test)]
pub(crate) mod testing;
pub mod text_sync;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use lsp_server::{
    Connection, ErrorCode, Message, Notification, ReqQueue, Request, RequestId, Response,
    ResponseError,
};
use lsp_types::{LogMessageParams, MarkupKind, MessageType, Uri};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::ctx::Ctx;
use crate::dictionary::Registry;
use crate::document::{Document, PositionEncoding};
use crate::render::Options;
use crate::rules::Registry as RuleRegistry;
use crate::transport::{Link, Outbound, Sink};

use config_pull::ConfigState;
use dispatch::{Answer, Dispatched, Notices, Snapshot};

/// Outgoing `workspace/configuration` pulls carry their generation so a stale or
/// closed-document reply can be dropped.
pub enum OutgoingRequest {
    BaseConfig {
        generation: u64,
    },
    /// `window/workDoneProgress/create` for a walk already in flight: the token
    /// may not be used until the client has agreed to it.
    ProgressCreate {
        uri: Uri,
        generation: u64,
    },
    /// `workspace/*/refresh`: the client is asked to re-request a channel
    /// because the configuration changed what it would answer. Nothing comes
    /// back worth acting on, but the queue needs the reply completed.
    Refresh {
        what: &'static str,
    },
}

/// The loop for an in-process client, which reads `Message`s rather than
/// bytes: the tests' and benches' harness over `Connection::memory()`.
pub fn main_loop(conn: Connection) -> ServerExit {
    serve(Link {
        receiver: conn.receiver,
        sender: Sink::Memory(conn.sender),
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum ServerExit {
    Normal,
    Fatal(String),
}

pub fn serve(link: Link) -> ServerExit {
    serve_with_user_file(link, None)
}

pub struct Startup {
    loader: external::Loader,
    user: crate::overrides::SettingsLayer,
    path: Option<PathBuf>,
}

impl Startup {
    pub fn load(path: Option<PathBuf>) -> Result<Self, String> {
        let loader = external::Loader::start();
        let user = loader.startup(path.clone())?;
        for warning in &user.warnings {
            eprintln!("fix-lsp: {}: {}", warning.origin.source, warning.message);
        }
        Ok(Self { loader, user, path })
    }
}

pub fn serve_with_user_file(link: Link, path: Option<PathBuf>) -> ServerExit {
    match Startup::load(path) {
        Ok(startup) => serve_started(link, startup),
        Err(error) => ServerExit::Fatal(error),
    }
}

pub fn serve_started(link: Link, startup: Startup) -> ServerExit {
    let mut state = ServerState::with_loader(link.sender.clone(), startup.loader);
    state.user_path = startup.path;
    state.config.user = startup.user;
    // Three things can wake the loop: the client, the diagnostics worker, and
    // the debounce falling due. The worker's results come back here rather than
    // going straight out, so that a publish is ordered against `didClose` and
    // against a configuration change.
    let reports_rx = state.diagnostics.reports_rx.clone();
    let configuration_rx = state.loader.reports.clone();
    loop {
        state.check_config_deadline();
        if let Some(error) = state.fatal.take() {
            state.diagnostics.abandon();
            return ServerExit::Fatal(error);
        }
        crossbeam_channel::select! {
            recv(link.receiver) -> msg => {
                let Ok(msg) = msg else { break };
                match msg {
                    Message::Request(req) => state.on_request(req),
                    Message::Notification(note) => {
                        if note.method == method::EXIT {
                            break;
                        }
                        state.on_notification(note);
                    }
                    Message::Response(resp) => state.on_response(resp),
                }
            }
            recv(reports_rx) -> report => {
                let Ok(report) = report else { break };
                state.on_diagnostic_report(report);
            }
            recv(configuration_rx) -> report => {
                match report {
                    Ok(report) => state.on_configuration_report(report),
                    Err(_) => state.fail_configuration("configuration worker disconnected".into()),
                }
            }
            default(state.diagnostics.tick_in().min(state.config_tick_in())) => state.dispatch_due_diagnostics(),
        }
    }
    ServerExit::Normal
}

pub(crate) struct ServerState {
    loader: external::Loader,
    pub(super) user_path: Option<PathBuf>,
    pub(super) workspace_folders: Vec<PathBuf>,
    pub(super) root_uri: Option<PathBuf>,
    pub(super) fatal: Option<String>,
    pub(super) docs: HashMap<Uri, Document>,
    pub(super) encoding: PositionEncoding,
    pub(super) req_queue: ReqQueue<(), OutgoingRequest>,
    pub(super) shutdown_requested: bool,
    pub(super) config: ConfigState,
    pub(super) client: ClientSupport,
    /// What each open document was last coloured as, by URI. A whole answer
    /// replaces its entry; an edit splices it.
    pub(super) tokens: HashMap<Uri, semantic_tokens::Digest>,
    /// Monotone, so a result id is never reused across documents or answers.
    pub(super) token_results: u64,
    /// `initialized` has arrived. Until it has, a refresh would be asking a
    /// client to re-request something it has never requested —
    /// `initializationOptions.fix` commits a configuration before that point.
    pub(super) started: bool,
    /// First workspace folder or `rootUri`; relative dictionary paths resolve here.
    pub(super) root_dir: Option<PathBuf>,
    /// For `~/` in dictionary paths; process-constant, looked up once.
    pub(super) home_dir: Option<PathBuf>,
    /// Replaced wholesale on every committed base config.
    pub(super) registry: Arc<Registry>,
    /// Replaced only when its fingerprint moves, which is what makes a `Ctx`'s
    /// pointer identity mean "these cached analyses are still valid".
    pub(super) ctx: Arc<Ctx>,
    /// Rendering configuration, replaced on every committed base config: it
    /// invalidates no analysis, so it does not belong in `ctx`.
    pub(super) render: Arc<Options>,
    /// The template errors `render` was built through. Kept so a document that
    /// resolves to the base config reports them without parsing again.
    pub(super) base_bad_templates: Arc<Vec<crate::render::BadTemplate>>,
    /// The enabled diagnostic rules; same reasoning as `render`.
    pub(super) rules: Arc<RuleRegistry>,
    /// The background validator and its scheduling state.
    pub(super) diagnostics: diagnostics::Worker,
    /// What the last configuration attempt reported, so an unchanged problem is
    /// not shown again on the next pull.
    pub(super) last_config_messages: Vec<String>,
    pub(super) sender: Sink,
}

/// Client-advertised behavior retained after `initialize`.
pub(super) struct ClientSupport {
    pub configuration: bool,
    pub progress: bool,
    /// First supported hover format in the client's preference order.
    pub hover_format: MarkupKind,
    /// Whether inlay-hint tooltips may be deferred to resolve requests.
    pub defers_tooltips: bool,
    /// Whether the client supplied its supported `SymbolKind` values.
    pub extended_symbol_kinds: bool,
    /// Whether the client accepts refresh requests for each dynamic channel.
    pub refreshes_inlay_hints: bool,
    pub refreshes_code_lens: bool,
    pub refreshes_semantic_tokens: bool,
    /// Whether overlapping semantic tokens can be rendered.
    pub overlaps_tokens: bool,
    /// Whether code actions may be marked as preferred.
    pub prefers_actions: bool,
}

impl Default for ClientSupport {
    fn default() -> Self {
        Self {
            configuration: false,
            progress: false,
            // The protocol's own default when the client says nothing.
            hover_format: MarkupKind::Markdown,
            defers_tooltips: false,
            extended_symbol_kinds: false,
            refreshes_inlay_hints: false,
            refreshes_code_lens: false,
            refreshes_semantic_tokens: false,
            overlaps_tokens: false,
            prefers_actions: false,
        }
    }
}

impl ServerState {
    #[cfg(test)]
    fn new(sender: impl Into<Sink>) -> ServerState {
        Self::with_loader(sender, external::Loader::start())
    }

    fn with_loader(sender: impl Into<Sink>, loader: external::Loader) -> ServerState {
        let registry = Arc::new(Registry::bundled_only());
        ServerState {
            loader,
            user_path: None,
            workspace_folders: Vec::new(),
            root_uri: None,
            fatal: None,
            docs: HashMap::new(),
            encoding: PositionEncoding::Utf16, // until initialize negotiates
            req_queue: ReqQueue::default(),
            shutdown_requested: false,
            config: ConfigState::default(),
            client: ClientSupport::default(),
            tokens: HashMap::new(),
            token_results: 0,
            started: false,
            root_dir: None,
            home_dir: std::env::home_dir(),
            registry: registry.clone(),
            ctx: Ctx::new(&Config::default(), registry, None),
            render: Arc::new(Options::default()),
            base_bad_templates: Arc::new(Vec::new()),
            rules: Arc::new(RuleRegistry::from_config(&Config::default().diagnostics)),
            diagnostics: diagnostics::Worker::start(),
            last_config_messages: Vec::new(),
            sender: sender.into(),
        }
    }

    // --- requests ---

    fn on_request(&mut self, req: Request) {
        if req.method == method::SHUTDOWN {
            self.shutdown_requested = true;
            self.stop_configuration();
            self.respond(Response::new_ok(req.id, Value::Null));
            return;
        }
        if self.shutdown_requested {
            self.respond(Response::new_err(
                req.id,
                ErrorCode::InvalidRequest as i32,
                "server is shutting down".into(),
            ));
            return;
        }

        let id = req.id.clone();
        self.req_queue.incoming.register(id.clone(), ());
        // `handle_request` is called *inside* the guard, not before it: it
        // parses params, takes snapshots and rebuilds line caches, and a panic
        // in any of those has to become an InternalError rather than kill the
        // loop. A test against `catch_handler_panic` alone cannot hold this —
        // when it regressed, the helper was fine and the call site was not.
        let outcome = catch_handler_panic(&req.method, || match self.handle_request(&req) {
            Dispatched::Now(result) => result.map(|value| Some(Answer::Json(value))),
            // The one place a task is executed. Stage 3/4 submits it to the
            // bounded executor here and completes the response from the
            // executor's channel; no handler changes.
            Dispatched::Task(snapshot, task) => {
                task(&snapshot, &mut Notices::to(self.sender.clone()))
                    .map(|value| Some(Answer::Json(value)))
            }
            Dispatched::Lazy(result) => result.map(|value| Some(Answer::Lazy(value))),
        });
        let outbound: Outbound = match outcome {
            // Built rather than `Response::new_ok`, which runs the value
            // through `serde_json::to_value` and so copies the whole tree.
            Ok(Ok(None)) => {
                self.sweep_caches();
                return;
            }
            Ok(Ok(Some(Answer::Json(value)))) => Response {
                id: id.clone(),
                response_result: Ok(value),
            }
            .into(),
            Ok(Ok(Some(Answer::Lazy(body)))) => Outbound::Result {
                id: id.clone(),
                body,
            },
            Ok(Err(err)) => Response {
                id: id.clone(),
                response_result: Err(err),
            }
            .into(),
            Err(message) => {
                log::error!("{message}");
                self.log_message(MessageType::Error, &message);
                Response::new_err(id.clone(), ErrorCode::InternalError as i32, message).into()
            }
        };
        // Only respond if the request was not cancelled/completed meanwhile.
        if self.req_queue.incoming.complete(&id).is_some() {
            self.sender.send(outbound);
        }
        self.sweep_caches();
    }

    /// Evict what the last request left over the bound. Here, after every
    /// request, rather than inside a task: only the main loop evicts, so tasks
    /// on a pool never race one another over a slot. Below the bound this is
    /// one load per open document.
    fn sweep_caches(&mut self) {
        let bound = self.config.base.limits.cache_bound();
        for doc in self.docs.values() {
            doc.evict_over(bound);
        }
    }

    fn handle_request(&mut self, req: &Request) -> Dispatched {
        match req.method.as_str() {
            // Compiled out of a release build; see `on_request` for why it
            // exists.
            #[cfg(test)]
            method::TEST_PANIC_IN_DISPATCH => panic!("dispatch exploded"),
            // `params` is a `Value` because `lsp_server` is transport only: the
            // method string is what selects the payload shape, so the type is not
            // known until here. Each handler types its own.
            method::INITIALIZE => Dispatched::Now(Ok(self.on_initialize(&req.params))),
            method::HOVER => self.hover_task(&req.params),
            method::CODE_ACTION => self.code_action_task(&req.params),
            method::CODE_LENS => self.code_lens_task(&req.params),
            method::INLAY_HINT => self.inlay_hint_task(&req.params),
            method::INLAY_HINT_RESOLVE => self.inlay_hint_resolve_task(&req.params),
            method::CODE_LENS_RESOLVE => self.code_lens_resolve_task(&req.params),
            method::SELECTION_RANGE => self.selection_range_task(&req.params),
            method::EXECUTE_COMMAND => self.execute_command_task(&req.params),
            method::DOCUMENT_SYMBOL => self.document_symbol_task(&req.params),
            method::SEMANTIC_TOKENS_RANGE => self.semantic_tokens_range_task(&req.params),
            method::SEMANTIC_TOKENS_FULL => self.semantic_tokens_full(&req.params),
            method::SEMANTIC_TOKENS_DELTA => self.semantic_tokens_delta(&req.params),
            other => Dispatched::Now(Err(lsp_server::ResponseError {
                code: ErrorCode::MethodNotFound as i32,
                message: format!("unhandled method: {other}"),
                data: None,
            })),
        }
    }

    fn on_response(&mut self, resp: Response) {
        let id = resp.id.clone();
        let Some(data) = self.req_queue.outgoing.complete(id) else {
            log::warn!("response for unknown request id {}", resp.id);
            return;
        };
        match data {
            OutgoingRequest::BaseConfig { generation } => {
                self.on_base_reply(generation, resp.response_result);
            }
            OutgoingRequest::Refresh { what } => {
                if let Err(e) = resp.response_result {
                    // A refusal costs the user a stale channel until they
                    // scroll, and nothing else — so it is logged, not shown.
                    log::info!("client refused the {what} refresh: {}", e.message);
                }
            }
            OutgoingRequest::ProgressCreate { uri, generation } => {
                self.on_progress_created(&uri, generation, resp.response_result.is_ok());
            }
        }
    }

    /// The only place a `Snapshot` is built, and therefore the only place that
    /// has to notice a context change: a document adopts the current one here,
    /// which makes "forgot to invalidate" unrepresentable.
    fn snapshot_for(&mut self, uri: &Uri) -> Option<Snapshot> {
        let configuration = self.config.documents.get(uri)?;
        let ctx = configuration.ctx.clone();
        let encoding = self.encoding;
        let render = configuration.render.clone();
        let rules = configuration.rules.clone();
        let doc = self.docs.get_mut(uri)?;
        doc.set_ctx(ctx);
        Some(Snapshot {
            lines: doc.lines.clone(),
            encoding,
            ctx: doc.ctx.clone(),
            population: doc.population.clone(),
            render,
            rules,
            version: doc.version,
        })
    }

    // --- notifications ---

    fn on_notification(&mut self, note: Notification) {
        match note.method.as_str() {
            method::INITIALIZED => {
                self.started = true;
                self.pull_base_config();
            }
            method::DID_CHANGE_CONFIGURATION => self.on_did_change_configuration(note.params),
            // TODO: move to method::
            "workspace/didChangeWorkspaceFolders" => self.on_workspace_folders(note.params),
            method::DID_OPEN => self.on_did_open(note.params),
            method::DID_CHANGE => self.on_did_change(note.params),
            method::DID_CLOSE => self.on_did_close(note.params),
            method::CANCEL_REQUEST => self.on_cancel(note.params),
            method::SET_TRACE => self.on_set_trace(note.params),
            _ => {}
        }
    }

    fn on_cancel(&mut self, params: Value) {
        let Some(params) = from_params::<CancelParams>(method::CANCEL_REQUEST, params) else {
            return;
        };
        if let Some(resp) = self.req_queue.incoming.cancel(params.id) {
            self.respond(resp);
        }
    }

    // --- outgoing ---

    fn respond(&self, resp: Response) {
        self.sender.send(resp);
    }

    pub(super) fn show_message(&self, typ: MessageType, message: &str) {
        self.window_message(method::SHOW_MESSAGE, typ, message);
    }

    pub(super) fn log_message(&self, typ: MessageType, message: &str) {
        self.window_message(method::LOG_MESSAGE, typ, message);
    }

    /// `showMessage` and `logMessage` share one params shape.
    fn window_message(&self, method: &str, typ: MessageType, message: &str) {
        let note = Notification::new(
            method.into(),
            json!(LogMessageParams {
                kind: typ,
                message: message.to_owned()
            }),
        );
        self.sender.send(note);
    }
}

/// A request's params, or the `InvalidParams` a client can act on.
///
/// Through `serde_path_to_error` so the message names the offending key rather
/// than the offset of a byte in a JSON document the user never sees. One copy
/// because six handlers wanted the same nine lines, and six places agreeing by
/// coincidence is five places to forget.
pub(super) fn params<T: serde::de::DeserializeOwned>(
    method: &str,
    raw: &Value,
) -> Result<T, ResponseError> {
    serde_path_to_error::deserialize(raw).map_err(|e| ResponseError {
        code: ErrorCode::InvalidParams as i32,
        message: format!("{method} params: `{}`: {}", e.path(), e.inner()),
        data: None,
    })
}

/// The item is about text that is no longer there, or a configuration that no
/// longer justifies it.
///
/// `ContentModified` is the protocol's way of saying so — a client re-requests
/// rather than showing what it has. What it cannot do is *make* one: LSP has no
/// refresh for a code lens's staleness, so a stale item can outlive
/// its justification until the client asks again.
pub(super) fn content_modified(method: &str, why: &str) -> ResponseError {
    ResponseError {
        code: ErrorCode::ContentModified as i32,
        message: format!("{method}: {why}"),
        data: None,
    }
}

/// Document identity is URL equality (normalised), not byte equality of the
/// string the client sent. An unparsable URI rejects the whole message.
pub(super) fn parse_uri(what: &str, raw: &str) -> Option<Uri> {
    match Uri::parse(raw) {
        Ok(uri) => Some(uri),
        Err(e) => {
            log::warn!("{what} ignored: invalid uri {raw:?}: {e}");
            None
        }
    }
}

#[derive(Deserialize)]
struct CancelParams {
    id: RequestId,
}

/// Run a request handler, turning a panic into an error message so one broken
/// message cannot take the server down. Separated from the router so the
/// containment can be tested with a panicking handler instead of a magic method.
fn catch_handler_panic<T, F>(method: &str, handler: F) -> Result<T, String>
where
    F: FnOnce() -> T,
{
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(handler)).map_err(|payload| {
        format!(
            "internal error handling {method}: {}",
            panic_reason(&*payload)
        )
    })
}

/// What the panic said. The default hook writes this to stderr, which no client
/// reads, so a contained panic that does not carry its reason forward is an
/// error report with the error removed.
pub(super) fn panic_reason(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_owned();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    "panicked".to_owned()
}

/// Decode a message's params. The single place that decides what "malformed"
/// means: anything that does not match the schema is rejected whole and logged,
/// never patched up with a default.
pub(super) fn from_params<T: serde::de::DeserializeOwned>(what: &str, params: Value) -> Option<T> {
    // By value, not by reference. Deserializing from `&Value` copies every
    // string it takes; from an owned one the buffers move. `didOpen` carries
    // the whole document as one of them, so the difference is a copy of the
    // file on every open.
    match serde_path_to_error::deserialize(params) {
        Ok(value) => Some(value),
        Err(e) => {
            // The path, so the log names the field rather than only its type.
            log::warn!("{what} ignored: `{}`: {}", e.path(), e.inner());
            None
        }
    }
}

/// A URI that cannot identify a document: a client may send `file://` for an
/// unnamed buffer, which parses to a bare root path.
pub(super) fn has_empty_path(uri: &Uri) -> bool {
    matches!(uri.path(), "" | "/")
}

/// A modeline origin's byte span as a range on the line it was read from. The
/// one conversion, so the negotiated encoding is applied in one place; without
/// the line the span collapses to its start.
pub(super) fn origin_range(
    origin: &crate::overrides::Origin,
    line: Option<&crate::document::Line>,
    encoding: PositionEncoding,
) -> lsp_types::Range {
    let column = |byte: usize| {
        line.map_or(0, |line| {
            crate::document::byte_to_position(
                line,
                crate::codec::ByteOffset::new(byte as u32),
                encoding,
            ) - crate::document::Column::new(0)
        })
    };
    lsp_types::Range::new(
        lsp_types::Position::new(origin.line as u32, column(origin.start)),
        lsp_types::Position::new(origin.line as u32, column(origin.end)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch::HandlerResult;

    /// Nothing else asserts that a server which has pulled no configuration
    /// renders with the default template rather than an empty one.
    /// A capability that writes needs the version its answer was computed
    /// against, and the rules it was computed under; neither belongs to the
    /// document, which is why both ride on the snapshot beside `render`.
    #[test]
    fn a_snapshot_carries_the_version_and_the_rules_it_was_taken_under() {
        let (sender, _receiver) = crossbeam_channel::unbounded::<lsp_server::Message>();
        let mut state = ServerState::new(sender);
        let uri: Uri = "file:///a.fix".parse().unwrap();
        state.on_notification(Notification::new(
            "textDocument/didOpen".into(),
            json!({ "textDocument": {
                "uri": uri.as_str(), "languageId": "fix", "version": 7, "text": "8=FIX.4.4|35=0"
            }}),
        ));
        testing::settle(&mut state);
        let snapshot = state.snapshot_for(&uri).expect("open");
        assert_eq!(snapshot.version, 7);
        assert!(Arc::ptr_eq(&snapshot.rules, &state.rules));
    }

    fn initialized_with(capabilities: Value) -> (ServerState, Value) {
        let (sender, _receiver) = crossbeam_channel::unbounded::<lsp_server::Message>();
        let mut state = ServerState::new(sender);
        let result = state.on_initialize(&json!({ "capabilities": capabilities }));
        (state, result)
    }

    /// Read by truth, not by presence: a client sending `false` is saying no.
    /// Asserted through what is advertised rather than through the flags, since
    /// the advertisement is the only thing the client ever sees.
    #[test]
    fn the_capabilities_a_code_action_depends_on_are_recorded() {
        let (state, result) = initialized_with(json!({
            "textDocument": { "codeAction": {
                "codeActionLiteralSupport": { "codeActionKind": { "valueSet": ["quickfix"] } },
                "isPreferredSupport": true,
            }},
            "workspace": { "workspaceEdit": { "documentChanges": true } },
        }));
        assert!(state.client.prefers_actions);
        assert!(result.pointer("/capabilities/codeActionProvider").is_some());

        let (state, result) = initialized_with(json!({
            "textDocument": { "codeAction": { "isPreferredSupport": false } },
            "workspace": { "workspaceEdit": { "documentChanges": false } },
        }));
        assert!(!state.client.prefers_actions);
        assert_eq!(
            result.pointer("/capabilities/codeActionProvider"),
            None,
            "false is a no, not a silence"
        );

        let (state, result) = initialized_with(json!({}));
        assert!(!state.client.prefers_actions);
        assert_eq!(result.pointer("/capabilities/codeActionProvider"), None);
    }

    /// `contentFormat` is an ordered preference, not a set: a client listing
    /// plaintext first is asking for plaintext, however much it also
    /// understands markdown.
    #[test]
    fn the_hover_format_is_the_first_one_the_client_asked_for() {
        let asked = |formats: Value| {
            let (state, _) = initialized_with(json!({
                "textDocument": { "hover": { "contentFormat": formats } }
            }));
            state.client.hover_format
        };
        assert_eq!(asked(json!(["markdown"])), MarkupKind::Markdown);
        assert_eq!(asked(json!(["plaintext"])), MarkupKind::PlainText);
        assert_eq!(
            asked(json!(["plaintext", "markdown"])),
            MarkupKind::PlainText,
            "first, not any"
        );
        assert_eq!(
            asked(json!(["markdown", "plaintext"])),
            MarkupKind::Markdown
        );
        // Something we do not produce is skipped rather than answered.
        assert_eq!(
            asked(json!(["asciidoc", "plaintext"])),
            MarkupKind::PlainText
        );

        let (silent, _) = initialized_with(json!({}));
        assert_eq!(
            silent.client.hover_format,
            MarkupKind::Markdown,
            "the protocol's own default when the client says nothing"
        );
    }

    /// A capability of the wrong shape costs its own branch and nothing else.
    /// Typed parsing is what makes a misspelled pointer impossible; parsing the
    /// tree in one piece is what would make one client quirk cost the position
    /// encoding, which is a correctness matter rather than a missing feature.
    #[test]
    fn a_malformed_capability_costs_only_its_own_branch() {
        let (state, result) = initialized_with(json!({
            "general": { "positionEncodings": ["utf-8"] },
            "window": { "workDoneProgress": true },
            "workspace": { "configuration": "yes", "workspaceEdit": { "documentChanges": true } },
            "textDocument": { "codeAction": {
                "codeActionLiteralSupport": { "codeActionKind": { "valueSet": ["quickfix"] } },
            }},
        }));
        assert!(
            !state.client.configuration,
            "the malformed branch is unadvertised entirely"
        );
        assert_eq!(
            result.pointer("/capabilities/codeActionProvider"),
            None,
            "and `workspaceEdit` goes down with it, being in that branch"
        );
        assert!(state.client.progress, "another branch is untouched");
        assert_eq!(
            result["capabilities"]["positionEncoding"], "utf-8",
            "and so is the encoding, which no feature flag may cost"
        );

        let (state, _) = initialized_with(json!({ "workspace": null, "window": null }));
        assert!(
            !state.client.configuration && !state.client.progress,
            "an explicit null is how a client says it has nothing, not a mistake"
        );
    }

    #[test]
    fn a_fresh_server_renders_with_the_default_options() {
        let (sender, _receiver) = crossbeam_channel::unbounded::<lsp_server::Message>();
        let state = ServerState::new(sender);
        assert_eq!(
            &state.render.template,
            crate::render::Template::default_template()
        );
        assert!(state.render.details, "the default config renders details");
    }

    #[test]
    fn a_panic_while_dispatching_becomes_an_internal_error() {
        let (sender, receiver) = crossbeam_channel::unbounded::<lsp_server::Message>();
        let mut state = ServerState::new(sender);
        state.on_request(Request {
            id: RequestId::from(1),
            method: "$/testPanicInDispatch".into(),
            params: Value::Null,
        });

        let replied = receiver
            .try_iter()
            .filter_map(|m| match m {
                Message::Response(r) => Some(r),
                _ => None,
            })
            .next()
            .expect("the request is answered rather than killing the loop");
        let err = replied.response_result.expect_err("an error response");
        assert_eq!(err.code, ErrorCode::InternalError as i32);

        // And the loop is still usable afterwards.
        state.on_did_open(json!({
            "textDocument": { "uri": "file:///a.fix", "version": 1, "text": "8=FIX.4.4|35=0|" }
        }));
        assert!(state
            .docs
            .contains_key(&Uri::parse("file:///a.fix").unwrap()));
    }

    /// The bound, seen from the loop: three viewports of the bound's size, and
    /// the third sweep is the first that finds anything unread. Second chance
    /// is what keeps all eight after the second.
    #[test]
    #[cfg(not(feature = "no-line-cache"))] // asserts a cache exists
    fn the_loop_sweeps_a_cache_that_grew_past_its_bound() {
        let (mut state, uri) = testing::state_with(&testing::heartbeats(12));
        state.config.base.limits.cached_lines = 4;
        super::testing::reconfigure(&mut state);
        state.config.base.limits.range_lines = 4;
        super::testing::reconfigure(&mut state);
        let mut viewport = |first: u32| {
            state.on_request(Request {
                id: RequestId::from(first as i32),
                method: method::SEMANTIC_TOKENS_RANGE.into(),
                params: json!({
                    "textDocument": { "uri": uri.as_str() },
                    "range": {
                        "start": { "line": first, "character": 0 },
                        "end": { "line": first + 4, "character": 0 },
                    },
                }),
            });
            testing::cached(&mut state, &uri)
        };
        let t = true;
        let f = false;
        assert_eq!(viewport(0), [t, t, t, t, f, f, f, f, f, f, f, f]);
        assert_eq!(
            viewport(4),
            [t, t, t, t, t, t, t, t, f, f, f, f],
            "over the bound, but everything was read since the last sweep"
        );
        assert_eq!(
            viewport(8),
            [f, f, f, f, f, f, f, f, t, t, t, t],
            "the first two screens went unread across a sweep"
        );
    }

    #[test]
    fn a_panicking_handler_becomes_an_internal_error() {
        let outcome = catch_handler_panic("textDocument/hover", || panic!("boom"));
        assert_eq!(
            outcome.unwrap_err(),
            "internal error handling textDocument/hover: boom",
            "an error report with the reason removed is not a report"
        );
    }

    /// Both shapes a `panic!` produces, plus the one it does not. The boxed
    /// payload has to be dereferenced before the downcast, and getting that
    /// wrong fails silently into "panicked" — which is why the assertion above
    /// is exact rather than a `contains`.
    #[test]
    fn a_panic_reason_survives_both_payload_shapes() {
        let literal = std::panic::catch_unwind(|| panic!("boom")).unwrap_err();
        assert_eq!(panic_reason(&*literal), "boom");
        let formatted = std::panic::catch_unwind(|| panic!("boom {}", 1)).unwrap_err();
        assert_eq!(panic_reason(&*formatted), "boom 1");
        let opaque = std::panic::catch_unwind(|| std::panic::panic_any(7u8)).unwrap_err();
        assert_eq!(panic_reason(&*opaque), "panicked");
    }

    #[test]
    fn a_healthy_handler_passes_its_result_through() {
        let outcome: Result<HandlerResult, String> =
            catch_handler_panic("m", || Ok(json!({ "ok": true })));
        assert_eq!(outcome.unwrap().unwrap(), json!({ "ok": true }));
    }

    #[test]
    fn server_state_is_still_usable_after_a_caught_panic() {
        let (sender, _) = crossbeam_channel::unbounded::<lsp_server::Message>();
        let mut state = ServerState::new(sender);
        state.on_did_open(json!({
            "textDocument": { "uri": "file:///a.fix", "version": 1, "text": "abc" }
        }));

        let outcome = catch_handler_panic("x", || panic!("boom"));
        assert!(outcome.is_err());

        // The document opened before the panic is still there and still answerable.
        testing::settle(&mut state);
        let uri = Uri::parse("file:///a.fix").unwrap();
        assert!(state.snapshot_for(&uri).is_some());
    }

    #[test]
    fn cancel_yields_one_request_canceled_and_suppresses_late_result() {
        let mut queue: ReqQueue<(), OutgoingRequest> = ReqQueue::default();
        let id = RequestId::from(7);
        queue.incoming.register(id.clone(), ());

        let resp = queue
            .incoming
            .cancel(id.clone())
            .expect("cancel emits a response");
        let err = resp
            .response_result
            .expect_err("cancel is an error response");
        assert_eq!(err.code, ErrorCode::RequestCanceled as i32);

        // The late result is suppressed: the id is no longer completable.
        assert!(queue.incoming.complete(&id).is_none());
    }
}
