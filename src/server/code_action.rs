//! `textDocument/codeAction`: the quickfix that repairs a line's trailer.
//!
//! The first capability that writes. Everything before it answered with text;
//! this hands back an edit, which needs the version its answer was computed
//! against and a client that said it can take one.

use lsp_server::{ErrorCode, ResponseError};
use lsp_types::{CodeActionKind, CodeActionParams, CodeActionTriggerKind};
use serde_json::{json, Value};

use super::diagnostics::render;
use super::dispatch::{Dispatched, HandlerResult, Notices, RequestTask, Snapshot};
use super::ServerState;
use crate::ctx::analyze;
use crate::document::{Columns, LineNo, LineRange};
use crate::rules::{integrity, Diag, Edit, LineCtx, Rule};

impl ServerState {
    pub(super) fn code_action_task(&mut self, params: &Value) -> Dispatched {
        let params: CodeActionParams = match super::params(super::method::CODE_ACTION, params) {
            Ok(params) => params,
            // A request, not a notification: an unparseable one is the client's
            // error. `[]` would claim the document has nothing to repair.
            Err(e) => return Dispatched::Now(Err(e)),
        };
        if let Some(answer) =
            super::dispatch::configuration_not_ready(self, &params.text_document.uri)
        {
            return answer;
        }
        let cap = self
            .effective_config(&params.text_document.uri)
            .unwrap_or(&self.config.base)
            .limits
            .range_lines;
        match self.snapshot_for(&params.text_document.uri) {
            Some(snapshot) => Dispatched::Task(snapshot, prepare(params, cap)),
            // Closed between the request and here: gone, not malformed.
            None => Dispatched::Now(Ok(json!([]))),
        }
    }
}

fn prepare(params: CodeActionParams, cap: usize) -> RequestTask {
    Box::new(move |snapshot: &Snapshot, _: &mut Notices| answer(&params, cap, snapshot))
}

fn answer(params: &CodeActionParams, cap: usize, snapshot: &Snapshot) -> HandlerResult {
    let invoked = params.context.trigger_kind != Some(CodeActionTriggerKind::Automatic);
    let Some(asked_for) = wants_quickfix(params, invoked) else {
        return Ok(json!([]));
    };
    let Some(lines) = requested_lines(params, cap, snapshot) else {
        // An explicitly invoked request has a user to tell; a poll does not,
        // and an error there is surfaced as a server failure. Refused rather
        // than truncated either way: a partial repair of a selection is less
        // predictable than being told the selection is too big.
        return match asked_for {
            Asked::Explicitly => Err(ResponseError {
                code: ErrorCode::RequestFailed as i32,
                message: format!("selection is over {cap} lines; select fewer and try again"),
                data: None,
            }),
            Asked::ByPolling => Ok(json!([])),
        };
    };

    let uri = params.text_document.uri.as_str();
    let mut out = Vec::new();
    for number in lines.lines() {
        let Some(line) = snapshot.line(number) else {
            continue;
        };
        // `analyze`, not `cached_analysis`: the range is bounded, so caching
        // would be permitted — but a code action is a one-shot gesture and
        // nothing will re-read these, so a wide selection would populate the
        // cache for one keystroke.
        let analysis = analyze(&line.bytes, &snapshot.ctx);
        let claimed: Vec<(LineNo, Diag)> = snapshot
            .rules
            .check_line(&line.bytes, &analysis)
            .into_iter()
            .filter(|diag| diag.rule == integrity::Integrity.id())
            .map(|diag| (number, diag))
            .collect();
        // No integrity diagnostic, no action: one switch governs the report and
        // the repair, so there is no configuration under which an action can
        // claim a problem the user asked not to hear about.
        if claimed.is_empty() {
            continue;
        }
        let Some(message) = analysis.message() else {
            continue;
        };
        let Some(repair) = integrity::repair(&LineCtx::new(&line.bytes, message)) else {
            continue;
        };
        out.push(json!({
            "title": repair.title,
            "kind": CodeActionKind::QuickFix,
            // Rendered by the publisher, so they are byte-identical to the
            // marks the client is already showing and match by range.
            "diagnostics": render(&claimed, snapshot),
            "isPreferred": true,
            "edit": { "documentChanges": [{
                "textDocument": { "uri": uri, "version": snapshot.version },
                "edits": as_text_edits(&repair.edits, number, snapshot),
            }]},
        }));
    }
    Ok(Value::Array(out))
}

/// Why the client is asking, or `None` when it is not asking for a quickfix.
enum Asked {
    Explicitly,
    /// The editor polling on a cursor move, having named what it wants.
    ByPolling,
}

fn wants_quickfix(params: &CodeActionParams, invoked: bool) -> Option<Asked> {
    match params.context.only.as_deref() {
        // A request for `source.fixAll` must not be answered with quickfixes.
        Some(kinds) if !kinds.contains(&CodeActionKind::QuickFix) => None,
        Some(_) => Some(if invoked {
            Asked::Explicitly
        } else {
            Asked::ByPolling
        }),
        // Editors poll this on every cursor move; without `only` there is
        // nothing to distinguish that from a user asking.
        None => invoked.then_some(Asked::Explicitly),
    }
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
