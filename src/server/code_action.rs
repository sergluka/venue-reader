//! Shared range repairs and explicit whole-document fix-all actions.

use std::sync::atomic::{AtomicBool, Ordering};

use lsp_server::{ErrorCode, ResponseError};
use lsp_types::{CodeActionKind, CodeActionParams, CodeActionTriggerKind, MessageType};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::config_pull::request_failed;
use super::diagnostics::render;
use super::dispatch::{BackgroundEvent, BackgroundResult, BackgroundTask, Dispatched, Snapshot};
use super::ServerState;
use crate::codec::ByteOffset;
use crate::document::{byte_to_position, Columns, LineNo, LineRange};
use crate::rules::{
    repair::{repair_line, RepairError, FIX_ALL_TITLE},
    Edit,
};

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActionData {
    action: String,
    uri: lsp_types::Uri,
    version: i32,
    lifetime: u64,
    repair_generation: u64,
}

impl ServerState {
    pub(super) fn code_action_task_prepared(&mut self, mut params: CodeActionParams) -> Dispatched {
        if !self.client.takes_actions {
            return Dispatched::Now(Ok(json!([])));
        }
        let cap = self
            .effective_config(&params.text_document.uri)
            .unwrap_or(&self.config.base)
            .limits
            .range_lines;
        let Some(snapshot) = self.snapshot_for(&params.text_document.uri) else {
            return Dispatched::Now(Ok(json!([])));
        };
        params.context.diagnostics = Vec::new();
        let source = wants_fix_all(&params);
        let eager = source
            && !self.client.defers_action_edits
            && params.context.only.is_some()
            && params.context.trigger_kind != Some(CodeActionTriggerKind::Automatic);
        if eager {
            return Dispatched::Background(BackgroundTask {
                method: super::method::CODE_ACTION,
                title: FIX_ALL_TITLE,
                uri: params.text_document.uri.clone(),
                snapshot,
                work_done: params.work_done_progress_params.work_done_token.clone(),
                partial: None,
                repair_range_lines: wants_range(&params).then_some(cap),
                run: Box::new(move |snapshot, cancelled, emit| {
                    all_actions(&params, cap, snapshot, cancelled, emit)
                }),
            });
        }
        let aggregate = source.then(|| {
            let disabled = if !snapshot.rules.has_repairs() {
                Some("No automatic repair rules are enabled")
            } else if !self.client.defers_action_edits {
                Some("This client cannot resolve fix-all edits; request source.fixAll explicitly")
            } else {
                None
            };
            if let Some(reason) = disabled {
                return self.client.disabled_actions.then(|| json!({
                    "title": FIX_ALL_TITLE,
                    "kind": CodeActionKind::SourceFixAll,
                    "disabled": { "reason": reason },
                }));
            }
            let uri = &params.text_document.uri;
            Some(json!({
                "title": FIX_ALL_TITLE,
                "kind": CodeActionKind::SourceFixAll,
                "data": ActionData {
                    action: "fixAll".into(),
                    uri: uri.clone(),
                    version: snapshot.version,
                    lifetime: *self.config.lifetimes.get(uri).expect("open document lifetime"),
                    repair_generation: self.config.documents.get(uri)
                        .expect("committed repair configuration").repair_generation,
                },
            }))
        }).flatten();
        Dispatched::Task(
            snapshot,
            Box::new(move |snapshot, notices| {
                let mut actions =
                    match range_actions(&params, cap, snapshot, &AtomicBool::new(false)) {
                        Err(RepairError::Unsafe(message))
                            if params.context.trigger_kind
                                == Some(CodeActionTriggerKind::Automatic) =>
                        {
                            notices.log(MessageType::Error, &message);
                            Vec::new()
                        }
                        result => result.map_err(repair_error)?,
                    };
                actions.extend(aggregate);
                Ok(Value::Array(actions))
            }),
        )
    }

    pub(super) fn code_action_resolve_task_owned(&mut self, action: Value) -> Dispatched {
        let data = match action_data(&action) {
            Ok(data) => data,
            Err(error) => return Dispatched::Now(Err(error)),
        };
        let current = self
            .docs
            .get(&data.uri)
            .is_some_and(|document| document.version == data.version)
            && self.config.lifetimes.get(&data.uri) == Some(&data.lifetime)
            && self
                .config
                .documents
                .get(&data.uri)
                .is_some_and(|document| document.repair_generation == data.repair_generation);
        if !current {
            return Dispatched::Now(Err(super::content_modified(
                super::method::CODE_ACTION_RESOLVE,
                "the document or repair configuration has changed",
            )));
        }
        let Some(snapshot) = self.snapshot_for(&data.uri) else {
            return Dispatched::Now(Err(super::content_modified(
                super::method::CODE_ACTION_RESOLVE,
                "the document is unavailable",
            )));
        };
        if action.get("edit").is_some() {
            return Dispatched::Now(Ok(action));
        }
        Dispatched::Background(BackgroundTask {
            method: super::method::CODE_ACTION_RESOLVE,
            title: FIX_ALL_TITLE,
            uri: data.uri.clone(),
            snapshot,
            work_done: None,
            partial: None,
            repair_range_lines: None,
            run: Box::new(move |snapshot, cancelled, emit| {
                let mut output = serde_json::to_vec(&action).expect("JSON action serializes");
                output.pop();
                output.extend_from_slice(b",\"edit\":");
                write_workspace_edit(&data.uri, snapshot, cancelled, emit, &mut output)?;
                output.push(b'}');
                Ok(output)
            }),
        })
    }
}

pub(super) fn validate_action(action: &Value) -> Result<(), ResponseError> {
    action_data(action).map(|_| ())
}

fn action_data(action: &Value) -> Result<ActionData, ResponseError> {
    action
        .get("data")
        .cloned()
        .and_then(|data| serde_json::from_value::<ActionData>(data).ok())
        .filter(|data| {
            data.action == "fixAll"
                && action.get("kind").and_then(Value::as_str) == Some("source.fixAll")
                && action.get("title").is_some_and(Value::is_string)
                && action.get("command").is_none()
                && action.get("disabled").is_none()
        })
        .ok_or_else(|| ResponseError {
            code: ErrorCode::InvalidParams as i32,
            message: "codeAction/resolve requires a fix-all action returned by this server".into(),
            data: None,
        })
}

fn repair_error(error: RepairError) -> ResponseError {
    match error {
        RepairError::Cancelled => ResponseError {
            code: ErrorCode::RequestCanceled as i32,
            message: "repair cancelled".into(),
            data: None,
        },
        RepairError::Unsafe(message) => request_failed(message),
    }
}

fn range_actions(
    params: &CodeActionParams,
    cap: usize,
    snapshot: &Snapshot,
    cancelled: &AtomicBool,
) -> Result<Vec<Value>, RepairError> {
    let invoked = params.context.trigger_kind != Some(CodeActionTriggerKind::Automatic);
    if !wants_range(params) {
        return Ok(Vec::new());
    }
    let Some(lines) = requested_lines(params, cap, snapshot) else {
        return if invoked {
            Err(RepairError::Unsafe(format!(
                "selection is over {cap} lines; select fewer and try again"
            )))
        } else {
            Ok(Vec::new())
        };
    };
    let mut actions = Vec::new();
    for number in lines.lines() {
        let Some(line) = snapshot.line(number) else {
            continue;
        };
        let Some(repair) = repair_line(
            &line.bytes,
            line.analysis_if_cached().as_deref(),
            &snapshot.ctx,
            &snapshot.rules,
            &snapshot.names,
            cancelled,
        )?
        else {
            continue;
        };
        actions.push(json!({
            "title": repair.title,
            "kind": CodeActionKind::QuickFix,
            "diagnostics": render(
                &repair.diags.iter().map(|diag| (number, diag)).collect::<Vec<_>>(), snapshot),
            "isPreferred": true,
            "edit": { "documentChanges": [{
                "textDocument": { "uri": params.text_document.uri, "version": snapshot.version },
                "edits": as_text_edits(&repair.edits, number, snapshot),
            }]},
        }));
    }
    Ok(actions)
}

fn all_actions(
    params: &CodeActionParams,
    cap: usize,
    snapshot: &Snapshot,
    cancelled: &AtomicBool,
    emit: &mut dyn FnMut(BackgroundEvent),
) -> BackgroundResult {
    if cancelled.load(Ordering::Relaxed) {
        return Err(repair_error(RepairError::Cancelled));
    }
    let range = range_actions(params, cap, snapshot, cancelled).map_err(repair_error)?;
    let mut output = serde_json::to_vec(&range).expect("JSON actions serialize");
    if !snapshot.rules.has_repairs() {
        return Ok(output);
    }
    output.pop();
    let before_all = output.len();
    if !range.is_empty() {
        output.push(b',');
    }
    // Retain only the final response buffer while composing the enclosing edit.
    output.extend_from_slice(b"{\"title\":");
    serde_json::to_writer(&mut output, FIX_ALL_TITLE).expect("JSON title serializes");
    output.extend_from_slice(b",\"kind\":");
    serde_json::to_writer(&mut output, &CodeActionKind::SourceFixAll)
        .expect("JSON kind serializes");
    output.extend_from_slice(b",\"edit\":");
    let any = write_workspace_edit(
        &params.text_document.uri,
        snapshot,
        cancelled,
        emit,
        &mut output,
    )?;
    if any {
        output.push(b'}');
    } else {
        output.truncate(before_all);
    }
    output.push(b']');
    Ok(output)
}

fn write_workspace_edit(
    uri: &lsp_types::Uri,
    snapshot: &Snapshot,
    cancelled: &AtomicBool,
    emit: &mut dyn FnMut(BackgroundEvent),
    output: &mut Vec<u8>,
) -> Result<bool, ResponseError> {
    output.extend_from_slice(b"{\"documentChanges\":[{\"textDocument\":");
    serde_json::to_writer(
        &mut *output,
        &json!({"uri": uri, "version": snapshot.version}),
    )
    .expect("JSON document serializes");
    output.extend_from_slice(b",\"edits\":[");
    let mut first = None;
    let mut previous = None;
    let mut previous_percentage = 0;
    for number in snapshot.line_span(0, snapshot.line_count()).lines() {
        if cancelled.load(Ordering::Relaxed) {
            return Err(repair_error(RepairError::Cancelled));
        }
        let Some(line) = snapshot.line(number) else {
            continue;
        };
        if let Some(repair) = repair_line(
            &line.bytes,
            line.analysis_if_cached().as_deref(),
            &snapshot.ctx,
            &snapshot.rules,
            &snapshot.names,
            cancelled,
        )
        .map_err(|error| match error {
            RepairError::Unsafe(message) => {
                request_failed(format!("Line {}: {message}", u64::from(number.raw()) + 1))
            }
            error => repair_error(error),
        })? {
            for edit in &repair.edits {
                if cancelled.load(Ordering::Relaxed) {
                    return Err(repair_error(RepairError::Cancelled));
                }
                let start = (number, edit.start);
                if let Some(previous) = previous {
                    append_original(output, snapshot, previous, start, cancelled)?;
                }
                if first.is_none() {
                    first = Some(start);
                    output.extend_from_slice(b"{\"newText\":\"");
                }
                append_string_fragment(output, &edit.text);
                previous = Some((number, edit.end));
            }
        }
        let percentage =
            ((u64::from(number.raw()) + 1) * 100 / u64::from(snapshot.line_count())) as u32;
        if percentage > previous_percentage {
            emit(BackgroundEvent::Progress {
                percentage,
                phase: "Repairing",
            });
            previous_percentage = percentage;
        }
    }
    if cancelled.load(Ordering::Relaxed) {
        return Err(repair_error(RepairError::Cancelled));
    }
    if let (Some(first), Some(last)) = (first, previous) {
        finish_edit(output, snapshot, first, last);
    }
    output.extend_from_slice(b"]}]}");
    Ok(first.is_some())
}

fn finish_edit(
    output: &mut Vec<u8>,
    snapshot: &Snapshot,
    first: (LineNo, ByteOffset),
    last: (LineNo, ByteOffset),
) {
    let position = |(number, offset)| {
        let line = snapshot.line(number).expect("repair line exists");
        json!({"line": number, "character": byte_to_position(line, offset, snapshot.encoding)})
    };
    output.extend_from_slice(b"\",\"range\":");
    serde_json::to_writer(
        &mut *output,
        &json!({"start": position(first), "end": position(last)}),
    )
    .expect("JSON range serializes");
    output.push(b'}');
}

fn append_original(
    output: &mut Vec<u8>,
    snapshot: &Snapshot,
    start: (LineNo, ByteOffset),
    end: (LineNo, ByteOffset),
    cancelled: &AtomicBool,
) -> Result<(), ResponseError> {
    for number in start.0.raw()..=end.0.raw() {
        if cancelled.load(Ordering::Relaxed) {
            return Err(repair_error(RepairError::Cancelled));
        }
        let line = snapshot
            .line(LineNo::new(number))
            .expect("repair line exists");
        let from = if number == start.0.raw() {
            start.1.index()
        } else {
            0
        };
        let to = if number == end.0.raw() {
            end.1.index()
        } else {
            line.bytes.len()
        };
        let text = std::str::from_utf8(&line.bytes[from..to])
            .map_err(|_| request_failed("Cannot include non-UTF-8 text in a fix-all edit"))?;
        append_string_fragment(output, text);
        if number != end.0.raw() {
            append_string_fragment(
                output,
                std::str::from_utf8(line.ending()).expect("ASCII ending"),
            );
        }
    }
    Ok(())
}

fn append_string_fragment(output: &mut Vec<u8>, text: &str) {
    // Reuse JSON escaping without allocating the complete replacement string.
    let start = output.len();
    serde_json::to_writer(&mut *output, text).expect("JSON text serializes");
    let end = output.len();
    output.copy_within(start + 1..end - 1, start);
    output.truncate(end - 2);
}

fn includes_kind(requested: &CodeActionKind, offered: &str) -> bool {
    let requested = requested.as_str();
    requested.is_empty()
        || offered == requested
        || offered
            .strip_prefix(requested)
            .is_some_and(|suffix| suffix.starts_with('.'))
}

pub(super) fn wants_fix_all(params: &CodeActionParams) -> bool {
    wants_kind(params, CodeActionKind::SourceFixAll.as_str())
}

fn wants_range(params: &CodeActionParams) -> bool {
    wants_kind(params, CodeActionKind::QuickFix.as_str())
}

fn wants_kind(params: &CodeActionParams, offered: &str) -> bool {
    params
        .context
        .only
        .as_ref()
        .is_none_or(|kinds| kinds.iter().any(|kind| includes_kind(kind, offered)))
}

/// The lines the range covers, or `None` when there are more than `cap`.
///
/// Normalised before it is clamped: a client sending an end past the document
/// would otherwise lose the last line, which is the one the cursor is on.
fn requested_lines(
    params: &CodeActionParams,
    cap: usize,
    snapshot: &Snapshot,
) -> Option<LineRange> {
    let (start, end) = (params.range.start, params.range.end);
    // This normalisation is this handler's, and differs from the inlay one on
    // purpose: `end.line > start.line` is what keeps a cursor at `(n,0)..(n,0)`
    // on line `n`, because an action is about the line the cursor sits on.
    let last = if end.character == 0 && end.line > start.line {
        end.line // a selection ending at the start of a line does not include it
    } else {
        end.line.saturating_add(1)
    };
    let lines = snapshot.line_span(start.line, last);
    (lines.len() <= cap).then_some(lines)
}

/// One line's edits as LSP `TextEdit`s.
///
/// Through `Columns` rather than a sort and a binary search written here: on a
/// malformed line the two edits need not be in spatial order, and the batch
/// walk requires ascending input. This used to re-establish that at the call
/// site, which is exactly what `Columns` exists to stop.
fn as_text_edits(edits: &[Edit], number: LineNo, snapshot: &Snapshot) -> Vec<Value> {
    let Some(line) = snapshot.line(number) else {
        return Vec::new();
    };
    let columns = Columns::of(
        line,
        edits.iter().flat_map(|edit| [edit.start, edit.end]),
        snapshot.encoding,
    );
    edits
        .iter()
        .map(|edit| {
            json!({
                "range": {
                    "start": { "line": number, "character": columns.at(edit.start) },
                    "end": { "line": number, "character": columns.at(edit.end) },
                },
                "newText": edit.text,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::codec::ByteOffset;
    use crate::config::Severity;
    use crate::rules::{
        integrity::Integrity,
        repair::{Provider, RepairPolicy},
        Diag, DiagnosticKind, Fixability, LineCtx, Precondition, Registry, Repair, Rule,
    };
    use crate::server::testing;

    struct Synthetic;

    impl Rule for Synthetic {
        fn id(&self) -> &'static str {
            "synthetic"
        }
        fn precondition(&self) -> Precondition {
            Precondition::Fields
        }
        fn default_severity(&self) -> Severity {
            Severity::Warning
        }
        fn repair_policy(&self) -> RepairPolicy {
            RepairPolicy::Safe(Provider {
                claims: |_, diag| diag.kind.code == "synthetic.body",
                repair: |line| {
                    let start = line
                        .bytes
                        .windows(9)
                        .position(|window| window == b"58=broken")?
                        + 3;
                    Some(Repair {
                        title: "Repair synthetic fixture".into(),
                        edits: vec![Edit {
                            start: ByteOffset::new(start as u32),
                            end: ByteOffset::new((start + 6) as u32),
                            text: "fixed \\\"🙂".into(),
                        }],
                    })
                },
                eligible: |_| true,
                dependencies: &["integrity"],
                finalizer: false,
            })
        }
        fn check(&self, line: &LineCtx, severity: Severity) -> Vec<Diag> {
            let Some(start) = line
                .bytes
                .windows(9)
                .position(|window| window == b"58=broken")
            else {
                return Vec::new();
            };
            vec![Diag {
                kind: DiagnosticKind {
                    code: format!("{}.body", self.id()).into(),
                    fixability: Fixability::Automatic,
                },
                rule: self.id(),
                start: ByteOffset::new((start + 3) as u32),
                end: ByteOffset::new((start + 9) as u32),
                message: "Synthetic repair fixture".into(),
                severity,
                document_key: None,
            }]
        }
    }

    fn params(uri: &lsp_types::Uri) -> CodeActionParams {
        serde_json::from_value(json!({
            "textDocument": {"uri": uri},
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
            "context": {"diagnostics": [], "only": ["source.fixAll"], "triggerKind": 1},
            "partialResultToken": "never-stream-edits",
        }))
        .unwrap()
    }

    fn apply_action(text: &str, action: &Value) -> String {
        let mut lines: Vec<_> = text.split('\n').map(str::to_owned).collect();
        let edits = action["edit"]["documentChanges"][0]["edits"]
            .as_array()
            .unwrap();
        for edit in edits.iter().rev() {
            let number = edit["range"]["start"]["line"].as_u64().unwrap() as usize;
            let start = edit["range"]["start"]["character"].as_u64().unwrap() as usize;
            let end = edit["range"]["end"]["character"].as_u64().unwrap() as usize;
            lines[number].replace_range(start..end, edit["newText"].as_str().unwrap());
        }
        lines.join("\n")
    }

    #[test]
    fn fix_all_without_providers_does_not_run_diagnostic_checks() {
        struct DiagnosticOnly;
        impl Rule for DiagnosticOnly {
            fn id(&self) -> &'static str {
                "diagnosticOnly"
            }
            fn precondition(&self) -> Precondition {
                Precondition::Fields
            }
            fn default_severity(&self) -> Severity {
                Severity::Warning
            }
            fn repair_policy(&self) -> RepairPolicy {
                RepairPolicy::DiagnosticOnly
            }
            fn check(&self, _: &LineCtx, _: Severity) -> Vec<Diag> {
                panic!("fix-all without providers must not check lines");
            }
        }

        let (mut state, uri) = testing::state_with(&testing::heartbeats(100));
        let mut snapshot = state.snapshot_for(&uri).unwrap();
        snapshot.rules = Arc::new(Registry::of_rules(
            vec![(&DiagnosticOnly, Severity::Warning)],
            None,
        ));
        let params = params(&uri);
        let result = all_actions(&params, 1, &snapshot, &AtomicBool::new(false), &mut |_| {
            panic!("no scan progress is needed");
        })
        .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&result).unwrap(), json!([]));
        let cancelled =
            all_actions(&params, 1, &snapshot, &AtomicBool::new(true), &mut |_| {}).unwrap_err();
        assert_eq!(cancelled.code, ErrorCode::RequestCanceled as i32);

        let mut mixed = params;
        mixed
            .context
            .only
            .as_mut()
            .unwrap()
            .push(CodeActionKind::QuickFix);
        mixed.range.end.line = 99;
        let error =
            all_actions(&mixed, 1, &snapshot, &AtomicBool::new(false), &mut |_| {}).unwrap_err();
        assert_eq!(error.code, ErrorCode::RequestFailed as i32);
        assert!(error.message.contains("selection is over 1 lines"));
    }

    #[cfg(not(feature = "bench-no-line-cache"))]
    #[test]
    fn cached_repairs_match_cold_results_without_populating_or_refreshing_marks() {
        let text = format!(
            "8=FIX.4.4|9=1|35=0|58=broken|10=001|\n{}\n8=FIX.4.4|9=1|35=0|10=001|",
            testing::HEARTBEAT
        );
        let (mut state, uri) = testing::state_with(&text);
        let mut snapshot = state.snapshot_for(&uri).unwrap();
        snapshot.rules = Arc::new(Registry::of_rules(
            vec![
                (&Synthetic, Severity::Warning),
                (&Integrity, Severity::Error),
            ],
            None,
        ));
        let cancelled = AtomicBool::new(false);
        let mut request = params(&uri);
        request.range.end.line = 2;
        request.context.only = Some(vec![CodeActionKind::SourceFixAll, CodeActionKind::QuickFix]);
        let cold = all_actions(&request, 3, &snapshot, &cancelled, &mut |_| {}).unwrap();
        assert_eq!(testing::cached_lines(&snapshot), vec![false; 3]);
        let cold_range = range_actions(&request, 3, &snapshot, &cancelled).unwrap();
        let warmed = snapshot.analysis(snapshot.line(LineNo::new(0)).unwrap());
        assert_eq!(state.docs.get(&uri).unwrap().evict_over(0), 0);

        let warm = all_actions(&request, 3, &snapshot, &cancelled, &mut |_| {}).unwrap();
        assert_eq!(warm, cold);
        assert_eq!(
            range_actions(&request, 3, &snapshot, &cancelled).unwrap(),
            cold_range
        );
        assert!(!cold_range.is_empty());
        assert_eq!(testing::cached_lines(&snapshot), vec![true, false, false]);
        assert!(Arc::ptr_eq(
            &warmed,
            &snapshot
                .line(LineNo::new(0))
                .unwrap()
                .analysis_if_cached()
                .unwrap()
        ));
        assert_eq!(snapshot.population.count(), 1);
        assert_eq!(
            state.docs.get(&uri).unwrap().evict_over(0),
            1,
            "repairs must not refresh the cache use mark"
        );
        assert_eq!(testing::cached_lines(&snapshot), vec![false; 3]);
    }

    struct Faulty;

    impl Rule for Faulty {
        fn id(&self) -> &'static str {
            Synthetic.id()
        }
        fn precondition(&self) -> Precondition {
            Synthetic.precondition()
        }
        fn default_severity(&self) -> Severity {
            Synthetic.default_severity()
        }
        fn repair_policy(&self) -> RepairPolicy {
            let RepairPolicy::Safe(mut provider) = Synthetic.repair_policy() else {
                unreachable!()
            };
            provider.repair = |_| {
                Some(Repair {
                    title: "Invalid fixture repair".into(),
                    edits: vec![Edit {
                        start: ByteOffset::new(0),
                        end: ByteOffset::new(0),
                        text: "\n".into(),
                    }],
                })
            };
            RepairPolicy::Safe(provider)
        }
        fn check(&self, line: &LineCtx, severity: Severity) -> Vec<Diag> {
            Synthetic.check(line, severity)
        }
    }

    #[test]
    fn automatic_repair_failure_logs_and_discards_all_actions() {
        use lsp_server::{Message, Request};

        for trigger in [Some(2), Some(1), None] {
            let text = "8=FIX.4.4|9=1|35=0|10=001|\n8=FIX.4.4|9=1|35=0|58=broken|10=001|";
            let (mut state, uri) = testing::state_with(text);
            let (sender, receiver) = crossbeam_channel::unbounded::<Message>();
            state.sender = sender.into();
            state.client.takes_actions = true;
            Arc::make_mut(state.config.documents.get_mut(&uri).unwrap()).rules =
                Arc::new(Registry::of_rules(
                    vec![(&Faulty, Severity::Warning), (&Integrity, Severity::Error)],
                    None,
                ));
            let mut request = json!({
                "textDocument": {"uri": uri},
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                "context": {"diagnostics": [], "only": ["quickfix"]},
            });
            if let Some(trigger) = trigger {
                request["context"]["triggerKind"] = json!(trigger);
            }
            state.on_request(Request::new(
                1.into(),
                "textDocument/codeAction".into(),
                request.clone(),
            ));
            let Message::Response(control) = receiver.try_recv().unwrap() else {
                panic!("successful repair returns an action without logging");
            };
            assert_eq!(
                control.response_result.unwrap().as_array().unwrap().len(),
                1
            );
            request["range"]["end"]["line"] = json!(1);
            request["range"]["end"]["character"] = json!(1);
            state.on_request(Request::new(
                2.into(),
                "textDocument/codeAction".into(),
                request,
            ));
            if trigger == Some(2) {
                let Message::Notification(log) = receiver.try_recv().unwrap() else {
                    panic!("automatic failure is logged before the response");
                };
                assert_eq!(log.method, "window/logMessage");
                assert_eq!(log.params["type"], json!(1));
                assert!(log.params["message"]
                    .as_str()
                    .unwrap()
                    .contains("A repair contains an invalid line edit"));
            }
            let Message::Response(response) = receiver.try_recv().unwrap() else {
                panic!("request receives a response");
            };
            assert_eq!(response.id, 2.into());
            if trigger == Some(2) {
                assert_eq!(response.response_result.unwrap(), json!([]));
            } else {
                let error = response.response_result.unwrap_err();
                assert_eq!(error.code, ErrorCode::RequestFailed as i32);
                assert_eq!(error.message, "A repair contains an invalid line edit");
            }
            assert!(receiver.try_recv().is_err());

            let snapshot = state.snapshot_for(&uri).unwrap();
            for mixed in [false, true] {
                let mut source = params(&uri);
                if mixed {
                    source
                        .context
                        .only
                        .as_mut()
                        .unwrap()
                        .push(CodeActionKind::QuickFix);
                }
                let error =
                    all_actions(&source, 10, &snapshot, &AtomicBool::new(false), &mut |_| {})
                        .unwrap_err();
                assert_eq!(error.code, ErrorCode::RequestFailed as i32);
                assert_eq!(
                    error.message,
                    "Line 2: A repair contains an invalid line edit"
                );
            }
        }
    }

    #[test]
    fn registering_a_provider_adds_it_to_both_action_paths_without_cache_population() {
        let text = "8=FIX.4.4|9=1|35=0|58=broken|10=001|";
        let (mut state, uri) = testing::state_with(text);
        let mut snapshot = state.snapshot_for(&uri).unwrap();
        snapshot.rules = Arc::new(Registry::of_rules(
            vec![
                (&Synthetic, Severity::Warning),
                (&Integrity, Severity::Error),
            ],
            None,
        ));
        let cancelled = AtomicBool::new(false);
        let params = params(&uri);
        let encoded = all_actions(&params, 1, &snapshot, &cancelled, &mut |event| {
            assert!(matches!(event, BackgroundEvent::Progress { .. }));
        })
        .unwrap();
        let aggregate: Value = serde_json::from_slice(&encoded).unwrap();
        let edits = aggregate[0]["edit"]["documentChanges"][0]["edits"]
            .as_array()
            .unwrap();
        assert_eq!(
            edits.len(),
            1,
            "one enclosing edit including the registered body provider"
        );
        let mut range_params = params;
        range_params.context.only = None;
        let range = range_actions(&range_params, 1, &snapshot, &cancelled).unwrap();
        assert_eq!(
            range[0]["edit"]["documentChanges"][0]["edits"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            apply_action(text, &range[0]),
            apply_action(text, &aggregate[0])
        );
        assert!(range[0]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "synthetic.body"));
        let fixed = apply_action(text, &aggregate[0]);
        assert!(fixed.contains("58=fixed \\\"🙂|"));
        assert!(repair_line(
            fixed.as_bytes(),
            None,
            &snapshot.ctx,
            &snapshot.rules,
            &snapshot.names,
            &cancelled
        )
        .unwrap()
        .is_none());
        assert!(testing::cached_lines(&snapshot)
            .iter()
            .all(|cached| !cached));
        assert_eq!(snapshot.population.count(), 0);
    }

    #[test]
    fn disabled_integrity_preserves_envelope_fields_without_blocking_body_repairs() {
        let message = "8=FIX.4.4|9=15|35=0|58=broken|";
        let checksum: u32 = message
            .bytes()
            .map(|byte| u32::from(if byte == b'|' { 1 } else { byte }))
            .sum();
        for text in [
            format!("{message}10={:03}|", checksum % 256),
            "8=FIX.4.4|9=1|35=0|58=broken|10=001|".into(),
            "8=FIX.4.4|35=0|58=broken|".into(),
            "8=FIX.4.4|9=00015|35=0|58=broken|10=000|".into(),
        ] {
            let (mut state, uri) = testing::state_with(&text);
            let mut snapshot = state.snapshot_for(&uri).unwrap();
            snapshot.rules = Arc::new(Registry::of_rules(
                vec![(&Synthetic, Severity::Warning)],
                None,
            ));
            let cancelled = AtomicBool::new(false);
            let source_params = params(&uri);
            let mut range_params = source_params.clone();
            range_params.context.only = Some(vec![CodeActionKind::QuickFix]);
            let range = range_actions(&range_params, 1, &snapshot, &cancelled).unwrap();
            let aggregate: Vec<Value> = serde_json::from_slice(
                &all_actions(&source_params, 1, &snapshot, &cancelled, &mut |_| {}).unwrap(),
            )
            .unwrap();
            assert_eq!(range.len(), 1, "{text}");
            assert_eq!(aggregate.len(), 1, "{text}");
            let expected = text.replace("58=broken", "58=fixed \\\"🙂");
            for action in [&range[0], &aggregate[0]] {
                assert_eq!(apply_action(&text, action), expected, "{text}");
                assert_eq!(
                    action["edit"]["documentChanges"][0]["edits"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1,
                    "only the body value changes"
                );
            }

            snapshot.rules = Arc::new(Registry::of_rules(
                vec![(&Integrity, Severity::Error)],
                None,
            ));
            let range = range_actions(&range_params, 1, &snapshot, &cancelled).unwrap();
            let aggregate: Vec<Value> = serde_json::from_slice(
                &all_actions(&source_params, 1, &snapshot, &cancelled, &mut |_| {}).unwrap(),
            )
            .unwrap();
            for action in range.iter().chain(&aggregate) {
                assert!(apply_action(&text, action).contains("58=broken|"));
            }
        }
    }

    #[test]
    fn a_cancelled_scan_exposes_neither_partial_edits_nor_a_partial_success() {
        let text = "8=FIX.4.4|9=1|35=0|10=001|\n".repeat(500);
        let (mut state, uri) = testing::state_with(&text);
        let snapshot = state.snapshot_for(&uri).unwrap();
        let cancelled = AtomicBool::new(false);
        let result = all_actions(&params(&uri), 1, &snapshot, &cancelled, &mut |event| {
            assert!(matches!(event, BackgroundEvent::Progress { .. }));
            cancelled.store(true, Ordering::Relaxed);
        });
        let error = result.unwrap_err();
        assert_eq!(error.code, ErrorCode::RequestCanceled as i32);
        assert_eq!(error.message, "repair cancelled");
        assert!(testing::cached_lines(&snapshot)
            .iter()
            .all(|cached| !cached));
    }

    #[test]
    fn dispatch_keeps_ordinary_requests_inline_and_never_streams_edit_fragments() {
        let (mut state, uri) = testing::state_with(&testing::heartbeats(5000));
        state.client.takes_actions = true;
        let params = params(&uri);
        let dispatched = state.code_action_task_prepared(params.clone());
        let Dispatched::Background(task) = dispatched else {
            panic!("full scan goes to worker")
        };
        assert!(task.partial.is_none());
        let mut ordinary = params;
        ordinary.context.only = None;
        assert!(matches!(
            state.code_action_task_prepared(ordinary),
            Dispatched::Task(..)
        ));
        assert!(testing::cached(&mut state, &uri)
            .iter()
            .all(|cached| !cached));
    }

    #[test]
    fn scans_large_unchanged_documents_and_large_single_lines_without_new_limits() {
        let body = format!("35=0|58={}|", "x".repeat(1_000_000));
        let message = format!("8=FIX.4.4|9={}|{body}", body.len());
        let checksum: u32 = message
            .bytes()
            .map(|byte| u32::from(if byte == b'|' { 1 } else { byte }))
            .sum();
        let expected = format!("{message}10={:03}|", checksum % 256);
        for (text, expected) in [
            (testing::heartbeats(5000), None),
            (format!("8=FIX.4.4|9=1|{body}10=001|"), Some(expected)),
        ] {
            let (mut state, uri) = testing::state_with(&text);
            let snapshot = state.snapshot_for(&uri).unwrap();
            let encoded = all_actions(
                &params(&uri),
                1,
                &snapshot,
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .unwrap();
            let result: Value = serde_json::from_slice(&encoded).unwrap();
            if let Some(expected) = expected {
                assert_eq!(
                    result[0]["edit"]["documentChanges"][0]["edits"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
                let fixed = apply_action(&text, &result[0]);
                assert_eq!(fixed, expected);
                state.on_did_change(json!({
                    "textDocument": {"uri": uri, "version": 2},
                    "contentChanges": [{"text": fixed}],
                }));
                testing::settle(&mut state);
                let updated = state.snapshot_for(&uri).unwrap();
                let again = all_actions(
                    &params(&uri),
                    1,
                    &updated,
                    &AtomicBool::new(false),
                    &mut |_| {},
                )
                .unwrap();
                assert_eq!(serde_json::from_slice::<Value>(&again).unwrap(), json!([]));
            } else {
                assert_eq!(result, json!([]));
            }
            assert_eq!(snapshot.population.count(), 0);
        }
    }
}
