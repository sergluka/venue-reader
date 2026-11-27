//! Owned request parameters waiting for their configuration dependency.

use super::dispatch::{Dispatched, HandlerResult};
use super::{method, ServerState};
use lsp_server::{Request, ResponseError};
use lsp_types::*;
use serde_json::{json, Value};

pub enum PreparedRequest {
    Hover(HoverParams),
    Action(CodeActionParams),
    ActionResolve(Value),
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
            Self::ActionResolve(_) => method::CODE_ACTION_RESOLVE,
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
            Self::InlayResolve(params)
            | Self::LensResolve(params)
            | Self::ActionResolve(params) => params.clone(),
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
            Self::Action(_) | Self::ActionResolve(_) => true,
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
            method::INLAY_HINT_RESOLVE
            | method::CODE_LENS_RESOLVE
            | method::CODE_ACTION_RESOLVE => {
                if request.method == method::CODE_ACTION_RESOLVE {
                    super::code_action::validate_action(&request.params)?;
                }
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
                } else if request.method == method::CODE_ACTION_RESOLVE {
                    PreparedRequest::ActionResolve(request.params.clone())
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
        if self.client.range_only_tokens
            && matches!(
                parsed,
                PreparedRequest::TokensFull(_) | PreparedRequest::TokensDelta(_)
            )
        {
            return Ok(Some(Dispatched::Now(Ok(Value::Null))));
        }
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
            Self::InlayResolve(value) | Self::LensResolve(value) | Self::ActionResolve(value) => {
                value
                    .pointer("/data/uri")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse().ok())
            }
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
                PreparedRequest::LensResolve(_) | PreparedRequest::ActionResolve(_) => Err(
                    super::content_modified(self.request.method(), "the document is closed"),
                ),
                PreparedRequest::Action(params) if super::code_action::wants_fix_all(params) => {
                    Err(super::content_modified(
                        self.request.method(),
                        "the document has changed or closed",
                    ))
                }
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
            Self::ActionResolve(params) => state.code_action_resolve_task_owned(params),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{external, testing};
    use crossbeam_channel::bounded;
    use lsp_server::Message;
    use std::time::{Duration, Instant};

    #[test]
    fn waiting_source_actions_reject_close_and_reused_document_versions() {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Change {
            Close,
            Reopen,
            Edit,
        }
        for change in [Change::Close, Change::Reopen, Change::Edit] {
            for (context, source) in [
                (json!({"only":["source.fixAll"]}), true),
                (json!({"only":["source"]}), true),
                (json!({"only":[""]}), true),
                (json!({"only":["quickfix","source.fixAll"]}), true),
                (json!({"only":["quickfix"]}), false),
                (json!({"only":["source.fixAll"],"triggerKind":2}), true),
                (json!({}), true),
            ] {
                let (sender, receiver) = crossbeam_channel::unbounded();
                let mut state = ServerState::new(sender);
                let (entered, arrived) = bounded(1);
                let (release, wait) = bounded(1);
                state
                    .loader
                    .send(external::Job::Barrier {
                        entered,
                        release: wait,
                    })
                    .unwrap();
                arrived.recv_timeout(Duration::from_secs(2)).unwrap();
                let uri = testing::file_uri("waiting-fix-all.fix");
                let opened = json!({"textDocument": {
                    "uri":uri,"version":1,"text":testing::HEARTBEAT
                }});
                state.on_did_open(opened.clone());
                let mut context = context;
                context["diagnostics"] = json!([]);
                state.on_request(Request::new(
                    1.into(),
                    method::CODE_ACTION.into(),
                    json!({
                        "textDocument":{"uri":uri},
                        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},
                        "context":context,
                    }),
                ));
                assert!(state.pending_requests.contains(&1.into()));
                match change {
                    Change::Close => state.on_did_close(json!({"textDocument":{"uri":uri}})),
                    Change::Reopen => {
                        state.on_did_close(json!({"textDocument":{"uri":uri}}));
                        state.on_did_open(opened);
                    }
                    Change::Edit => state.on_did_change(json!({
                        "textDocument":{"uri":uri,"version":2},
                        "contentChanges":[{"text":testing::HEARTBEAT}],
                    })),
                }
                state.process_pending_requests(Instant::now());
                release.send(()).unwrap();
                testing::settle(&mut state);
                state.process_pending_requests(Instant::now());
                let responses: Vec<_> = receiver
                    .try_iter()
                    .filter_map(|message| match message {
                        Message::Response(response) => Some(response),
                        _ => None,
                    })
                    .collect();
                assert_eq!(responses.len(), 1, "{change:?}: {context}");
                let result = &responses[0].response_result;
                if source || change == Change::Edit {
                    assert_eq!(result.as_ref().unwrap_err().code, -32801);
                } else {
                    assert_eq!(result.as_ref().unwrap(), &json!([]));
                }
                assert!(!state.pending_requests.contains(&1.into()));
                assert!(!state.req_queue.incoming.has_pending());
            }
        }
    }
    #[test]
    fn waiting_action_resolves_reject_changes_and_can_be_cancelled() {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Event {
            Close,
            Reopen,
            Edit,
            Cancel,
        }
        for event in [Event::Close, Event::Reopen, Event::Edit, Event::Cancel] {
            let (sender, receiver) = crossbeam_channel::unbounded();
            let mut state = ServerState::new(sender);
            let (entered, arrived) = bounded(1);
            let (release, wait) = bounded(1);
            state
                .loader
                .send(external::Job::Barrier {
                    entered,
                    release: wait,
                })
                .unwrap();
            arrived.recv_timeout(Duration::from_secs(2)).unwrap();
            let uri = testing::file_uri("waiting-action-resolve.fix");
            let opened = json!({"textDocument":{
                "uri":uri,"version":1,"text":testing::HEARTBEAT
            }});
            state.on_did_open(opened.clone());
            state.on_request(Request::new(1.into(), method::CODE_ACTION_RESOLVE.into(), json!({
                "title":"Fix all", "kind":"source.fixAll",
                "data":{"action":"fixAll","uri":uri,"version":1,"lifetime":1,"repairGeneration":0}
            })));
            assert!(state.pending_requests.contains(&1.into()));
            match event {
                Event::Close => state.on_did_close(json!({"textDocument":{"uri":uri}})),
                Event::Reopen => {
                    state.on_did_close(json!({"textDocument":{"uri":uri}}));
                    state.on_did_open(opened);
                }
                Event::Edit => state.on_did_change(json!({
                    "textDocument":{"uri":uri,"version":2},
                    "contentChanges":[{"text":testing::HEARTBEAT}]
                })),
                Event::Cancel => state.on_cancel(json!({"id":1})),
            }
            state.process_pending_requests(Instant::now());
            release.send(()).unwrap();
            testing::settle(&mut state);
            state.process_pending_requests(Instant::now());
            let responses: Vec<_> = receiver
                .try_iter()
                .filter_map(|message| match message {
                    Message::Response(response) => Some(response),
                    _ => None,
                })
                .collect();
            assert_eq!(responses.len(), 1, "{event:?}");
            assert_eq!(
                responses[0].response_result.as_ref().unwrap_err().code,
                if event == Event::Cancel {
                    -32800
                } else {
                    -32801
                },
                "{event:?}"
            );
            assert!(!state.pending_requests.contains(&1.into()));
            assert!(!state.req_queue.incoming.has_pending());
        }
    }
    #[test]
    fn malformed_action_resolve_does_not_wait_for_configuration() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = ServerState::new(sender);
        let (entered, arrived) = bounded(1);
        let (release, wait) = bounded(1);
        state
            .loader
            .send(external::Job::Barrier {
                entered,
                release: wait,
            })
            .unwrap();
        arrived.recv_timeout(Duration::from_secs(2)).unwrap();
        let uri = testing::file_uri("invalid-action-resolve.fix");
        state.on_did_open(json!({"textDocument":{
            "uri":uri,"version":1,"text":testing::HEARTBEAT
        }}));
        assert!(state.effective_config(&uri).is_none());
        state.on_request(Request::new(1.into(), method::CODE_ACTION_RESOLVE.into(), json!({
            "title":"Fix all", "kind":"source.fixAll",
            "data":{"action":"fixAll","uri":uri,"version":"invalid","lifetime":1,"repairGeneration":0}
        })));
        let responses: Vec<_> = receiver
            .try_iter()
            .filter_map(|message| match message {
                Message::Response(response) => Some(response),
                _ => None,
            })
            .collect();
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0].response_result.as_ref().unwrap_err().code,
            -32602
        );
        assert!(!state.pending_requests.contains(&1.into()));
        assert!(!state.req_queue.incoming.has_pending());
        release.send(()).unwrap();
        testing::settle(&mut state);
        state.process_pending_requests(Instant::now());
        assert!(!receiver
            .try_iter()
            .any(|message| matches!(message, Message::Response(_))));
    }
}
