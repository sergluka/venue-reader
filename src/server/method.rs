//! Every LSP method name this server dispatches on or sends, taken from the
//! protocol's own metamodel rather than typed out.
//!
//! `lsp_server` carries a method as a `String`, so a name has to become a
//! `&str` somewhere. Doing it here means it happens once and cannot be
//! misspelled: `window/workDoneProgressCreate` — a name the protocol does not
//! have — went out on every diagnostics walk for a whole stage, and the test
//! that was supposed to catch it agreed with the same typo.
//!
//! `as_str()` is `const`, so these are usable as match patterns.
//!
//! Tests keep their own literals on purpose: a test that took the name from
//! here would agree with the server whatever the server sent.

use lsp_types::{
    CancelNotification, CodeActionRequest, CodeLensRefreshRequest, CodeLensRequest,
    CodeLensResolveRequest, ConfigurationRequest, DidChangeConfigurationNotification,
    DidChangeTextDocumentNotification, DidCloseTextDocumentNotification,
    DidOpenTextDocumentNotification, DocumentSymbolRequest, ExecuteCommandRequest,
    ExitNotification, HoverRequest, InitializeRequest, InitializedNotification,
    InlayHintRefreshRequest, InlayHintRequest, InlayHintResolveRequest, LogMessageNotification,
    Notification as _, ProgressNotification, PublishDiagnosticsNotification, Request as _,
    SelectionRangeRequest, SemanticTokensDeltaRequest, SemanticTokensRangeRequest,
    SemanticTokensRefreshRequest, SemanticTokensRequest, SetTraceNotification,
    ShowMessageNotification, ShutdownRequest, WorkDoneProgressCreateRequest,
};

pub const INITIALIZE: &str = InitializeRequest::METHOD.as_str();
pub const SHUTDOWN: &str = ShutdownRequest::METHOD.as_str();
pub const HOVER: &str = HoverRequest::METHOD.as_str();
pub const CODE_ACTION: &str = CodeActionRequest::METHOD.as_str();
pub const CODE_LENS: &str = CodeLensRequest::METHOD.as_str();
pub const INLAY_HINT: &str = InlayHintRequest::METHOD.as_str();
pub const INLAY_HINT_RESOLVE: &str = InlayHintResolveRequest::METHOD.as_str();
pub const CODE_LENS_RESOLVE: &str = CodeLensResolveRequest::METHOD.as_str();
pub const INLAY_HINT_REFRESH: &str = InlayHintRefreshRequest::METHOD.as_str();
pub const CODE_LENS_REFRESH: &str = CodeLensRefreshRequest::METHOD.as_str();
pub const SELECTION_RANGE: &str = SelectionRangeRequest::METHOD.as_str();
pub const EXECUTE_COMMAND: &str = ExecuteCommandRequest::METHOD.as_str();
pub const DOCUMENT_SYMBOL: &str = DocumentSymbolRequest::METHOD.as_str();
pub const SEMANTIC_TOKENS_FULL: &str = SemanticTokensRequest::METHOD.as_str();
pub const SEMANTIC_TOKENS_DELTA: &str = SemanticTokensDeltaRequest::METHOD.as_str();
pub const SEMANTIC_TOKENS_RANGE: &str = SemanticTokensRangeRequest::METHOD.as_str();
pub const SEMANTIC_TOKENS_REFRESH: &str = SemanticTokensRefreshRequest::METHOD.as_str();
pub const CONFIGURATION: &str = ConfigurationRequest::METHOD.as_str();
pub const PROGRESS_CREATE: &str = WorkDoneProgressCreateRequest::METHOD.as_str();

pub const INITIALIZED: &str = InitializedNotification::METHOD.as_str();
pub const EXIT: &str = ExitNotification::METHOD.as_str();
pub const DID_CHANGE_CONFIGURATION: &str = DidChangeConfigurationNotification::METHOD.as_str();
pub const DID_OPEN: &str = DidOpenTextDocumentNotification::METHOD.as_str();
pub const DID_CHANGE: &str = DidChangeTextDocumentNotification::METHOD.as_str();
pub const DID_CLOSE: &str = DidCloseTextDocumentNotification::METHOD.as_str();
pub const CANCEL_REQUEST: &str = CancelNotification::METHOD.as_str();
pub const SET_TRACE: &str = SetTraceNotification::METHOD.as_str();
pub const PROGRESS: &str = ProgressNotification::METHOD.as_str();
pub const PUBLISH_DIAGNOSTICS: &str = PublishDiagnosticsNotification::METHOD.as_str();
pub const SHOW_MESSAGE: &str = ShowMessageNotification::METHOD.as_str();
pub const LOG_MESSAGE: &str = LogMessageNotification::METHOD.as_str();

/// `$/` is the LSP-reserved implementation-defined namespace, so this is ours
/// to name; it is compiled out of a release build.
#[cfg(test)]
pub const TEST_PANIC_IN_DISPATCH: &str = "$/testPanicInDispatch";
