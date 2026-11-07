//! Owned request parameters waiting for their configuration dependency.

use super::dispatch::{Dispatched, HandlerResult};
use super::{method, ServerState};
use lsp_server::{Request, ResponseError};
use lsp_types::*;
use serde_json::{json, Value};

pub enum PreparedRequest {
    Hover(HoverParams),
    Action(CodeActionParams),
    Lens(CodeLensParams),
    Hint(InlayHintParams),
    Selection(SelectionRangeParams),
    Symbols(DocumentSymbolParams),
    TokensRange(SemanticTokensRangeParams),
    TokensFull(SemanticTokensParams),
    TokensDelta(SemanticTokensDeltaParams),
    InlayResolve(Value),
    LensResolve(Value),
    Decode { uri: Uri, ranges: Vec<Range> },
    Overrides(Uri),
    Dictionary(String),
    Command(ExecuteCommandParams),
}
impl PreparedRequest {
    pub fn method(&self) -> &'static str {
        match self {
            Self::Hover(_) => method::HOVER,
            Self::Action(_) => method::CODE_ACTION,
            Self::Lens(_) => method::CODE_LENS,
            Self::Hint(_) => method::INLAY_HINT,
            Self::Selection(_) => method::SELECTION_RANGE,
            Self::Symbols(_) => method::DOCUMENT_SYMBOL,
            Self::TokensRange(_) => method::SEMANTIC_TOKENS_RANGE,
            Self::TokensFull(_) => method::SEMANTIC_TOKENS_FULL,
            Self::TokensDelta(_) => method::SEMANTIC_TOKENS_DELTA,
            Self::InlayResolve(_) => method::INLAY_HINT_RESOLVE,
            Self::LensResolve(_) => method::CODE_LENS_RESOLVE,
            _ => method::EXECUTE_COMMAND,
        }
    }
    pub fn params(&self) -> Value {
        match self {
            Self::Hover(params) => serde_json::to_value(params).expect("serializable parameters"),
            Self::Action(params) => serde_json::to_value(params).expect("serializable parameters"),
            Self::Lens(params) => serde_json::to_value(params).expect("serializable parameters"),
            Self::Hint(params) => serde_json::to_value(params).expect("serializable parameters"),
            Self::Selection(params) => {
                serde_json::to_value(params).expect("serializable parameters")
            }
            Self::Symbols(params) => serde_json::to_value(params).expect("serializable parameters"),
            Self::TokensRange(params) => {
                serde_json::to_value(params).expect("serializable parameters")
            }
            Self::TokensFull(params) => {
                serde_json::to_value(params).expect("serializable parameters")
            }
            Self::TokensDelta(params) => {
                serde_json::to_value(params).expect("serializable parameters")
            }
            Self::InlayResolve(params) | Self::LensResolve(params) => params.clone(),
            Self::Decode { uri, ranges } => {
                json!({"command":"fix.decode", "arguments":[{"textDocument":{"uri":uri},"ranges":ranges}]})
            }
            Self::Overrides(uri) => json!({"command":"fix.describeOverrides", "arguments":[uri]}),
            Self::Command(params) => serde_json::to_value(params).expect("serializable parameters"),
            Self::Dictionary(name) => json!({"command":"fix.useDictionary", "arguments":[name]}),
        }
    }
    pub fn positional(&self) -> bool {
        match self {
            Self::Hover(_) => true,
            Self::Action(_) => true,
            Self::Lens(_) => false,
            Self::Hint(_) => true,
            Self::Selection(_) => true,
            Self::Symbols(_) => false,
            Self::TokensRange(_) => true,
            Self::TokensFull(_) => false,
            Self::TokensDelta(_) => false,
            Self::Decode { .. } => true,
            _ => false,
        }
    }
}
impl ServerState {
    pub(super) fn prepare_request(
        &mut self,
        request: &Request,
    ) -> Result<Option<Dispatched>, ResponseError> {
        let parsed = match request.method.as_str() {
            method::HOVER => {
                PreparedRequest::Hover(super::params(request.method.as_str(), &request.params)?)
            }
            method::CODE_ACTION => {
                PreparedRequest::Action(super::params(request.method.as_str(), &request.params)?)
            }
            method::CODE_LENS => {
                PreparedRequest::Lens(super::params(request.method.as_str(), &request.params)?)
            }
            method::INLAY_HINT => {
                PreparedRequest::Hint(super::params(request.method.as_str(), &request.params)?)
            }
            method::SELECTION_RANGE => {
                PreparedRequest::Selection(super::params(request.method.as_str(), &request.params)?)
            }
            method::DOCUMENT_SYMBOL => {
                PreparedRequest::Symbols(super::params(request.method.as_str(), &request.params)?)
            }
            method::SEMANTIC_TOKENS_RANGE => PreparedRequest::TokensRange(super::params(
                request.method.as_str(),
                &request.params,
            )?),
            method::SEMANTIC_TOKENS_FULL => PreparedRequest::TokensFull(super::params(
                request.method.as_str(),
                &request.params,
            )?),
            method::SEMANTIC_TOKENS_DELTA => PreparedRequest::TokensDelta(super::params(
                request.method.as_str(),
                &request.params,
            )?),
            method::INLAY_HINT_RESOLVE | method::CODE_LENS_RESOLVE => {
                let uri = request
                    .params
                    .pointer("/data/uri")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse::<Uri>().ok());
                let waiting = uri.as_ref().is_some_and(|uri| {
                    self.docs.contains_key(uri) && self.effective_config(uri).is_none()
                });
                if !waiting {
                    return Ok(None);
                }
                if request.method == method::INLAY_HINT_RESOLVE {
                    PreparedRequest::InlayResolve(request.params.clone())
                } else {
                    PreparedRequest::LensResolve(request.params.clone())
                }
            }
            method::EXECUTE_COMMAND => self.prepare_configuration_command(super::params(
                method::EXECUTE_COMMAND,
                &request.params,
            )?)?,
            _ => return Ok(None),
        };
        let dependency = if let Some(uri) = parsed.uri() {
            let Some(document) = self.docs.get(&uri) else {
                return Ok(Some(parsed.execute(self)));
            };
            if self.effective_config(&uri).is_some() {
                return Ok(Some(parsed.execute(self)));
            }
            Some((
                uri.clone(),
                self.config.lifetimes.get(&uri).copied(),
                document.version,
            ))
        } else if matches!(parsed, PreparedRequest::Dictionary(_))
            && !self.base_configuration_ready()
        {
            None
        } else {
            return Ok(Some(parsed.execute(self)));
        };
        if self.config.rejected {
            return Err(super::config_pull::request_failed(
                super::pending_requests::REJECTED,
            ));
        }
        Ok(Some(Dispatched::WaitingForConfiguration(
            super::pending_requests::PendingRequest::Request(Box::new(DeferredRequest {
                request: parsed,
                document: dependency,
            })),
        )))
    }
}

impl PreparedRequest {
    fn uri(&self) -> Option<Uri> {
        match self {
            Self::Dictionary(_) | Self::Command(_) => None,
            Self::Decode { uri, .. } | Self::Overrides(uri) => Some(uri.clone()),
            Self::InlayResolve(value) | Self::LensResolve(value) => value
                .pointer("/data/uri")
                .and_then(Value::as_str)
                .and_then(|value| value.parse().ok()),
            Self::Hover(params) => Some(
                params
                    .text_document_position_params
                    .text_document
                    .uri
                    .clone(),
            ),
            Self::Action(params) => Some(params.text_document.uri.clone()),
            Self::Lens(params) => Some(params.text_document.uri.clone()),
            Self::Hint(params) => Some(params.text_document.uri.clone()),
            Self::Selection(params) => Some(params.text_document.uri.clone()),
            Self::Symbols(params) => Some(params.text_document.uri.clone()),
            Self::TokensRange(params) => Some(params.text_document.uri.clone()),
            Self::TokensFull(params) => Some(params.text_document.uri.clone()),
            Self::TokensDelta(params) => Some(params.text_document.uri.clone()),
        }
    }
}

pub struct DeferredRequest {
    pub request: PreparedRequest,
    document: Option<(Uri, Option<u64>, i32)>,
}

impl DeferredRequest {
    pub(super) fn retained_uri_bytes(&self) -> usize {
        self.document
            .as_ref()
            .map_or(0, |(uri, _, _)| uri.as_str().len())
    }

    pub(super) fn ready(&self, state: &ServerState) -> bool {
        match &self.document {
            None => state.base_configuration_ready(),
            Some((uri, lifetime, version)) => {
                state.config.lifetimes.get(uri) != lifetime.as_ref()
                    || state.docs.get(uri).is_none_or(|document| {
                        self.request.positional() && document.version != *version
                    })
                    || state.effective_config(uri).is_some()
            }
        }
    }
    fn terminal(&self, state: &ServerState) -> Option<HandlerResult> {
        let (uri, lifetime, version) = self.document.as_ref()?;
        if state.config.lifetimes.get(uri) != lifetime.as_ref() || !state.docs.contains_key(uri) {
            return Some(match &self.request {
                PreparedRequest::InlayResolve(value) => Ok(value.clone()),
                PreparedRequest::LensResolve(_) => Err(super::content_modified(
                    self.request.method(),
                    "the document is closed",
                )),
                PreparedRequest::Decode { .. } | PreparedRequest::Overrides(_) => {
                    Err(super::config_pull::request_failed("document is not open"))
                }
                PreparedRequest::Hover(_)
                | PreparedRequest::Selection(_)
                | PreparedRequest::TokensRange(_)
                | PreparedRequest::TokensFull(_)
                | PreparedRequest::TokensDelta(_) => Ok(Value::Null),
                _ => Ok(json!([])),
            });
        }
        (self.request.positional()
            && state
                .docs
                .get(uri)
                .is_some_and(|document| document.version != *version))
        .then(|| {
            Err(super::content_modified(
                self.request.method(),
                "the document has changed",
            ))
        })
    }
    pub(super) fn execute(self, state: &mut ServerState) -> Dispatched {
        if let Some(result) = self.terminal(state) {
            return Dispatched::Now(result);
        }
        self.request.execute(state)
    }
}

impl PreparedRequest {
    fn execute(self, state: &mut ServerState) -> Dispatched {
        match self {
            Self::Hover(params) => state.hover_task_prepared(params),
            Self::Action(params) => state.code_action_task_prepared(params),
            Self::Lens(params) => state.code_lens_task_prepared(params),
            Self::Hint(params) => state.inlay_hint_task_prepared(params),
            Self::Selection(params) => state.selection_range_task_prepared(params),
            Self::Symbols(params) => state.document_symbol_task_prepared(params),
            Self::TokensRange(params) => state.semantic_tokens_range_task_prepared(params),
            Self::TokensFull(params) => state.semantic_tokens_full_prepared(params),
            Self::TokensDelta(params) => state.semantic_tokens_delta_prepared(params),
            Self::InlayResolve(params) => state.inlay_hint_resolve_task_owned(params),
            Self::LensResolve(params) => state.code_lens_resolve_task_owned(params),
            Self::Decode { uri, ranges } => state.decode_prepared(uri, ranges),
            Self::Overrides(uri) => Dispatched::Now(state.describe_overrides_prepared(uri)),
            Self::Dictionary(name) => Dispatched::Now(state.select_dictionary(name)),
            Self::Command(params) => state.execute_command_prepared(params),
        }
    }
}
