//! `textDocument/hover`: what the dictionary knows about the field under the
//! cursor.

use lsp_types::{HoverParams, MarkupKind};
use serde_json::{json, Value};

use super::dispatch::{Dispatched, HandlerResult, Notices, RequestTask, Snapshot};
use super::ServerState;
use crate::codec::ByteOffset;
use crate::document::{position_to_byte, Column, Columns, LineNo};
use crate::render::hover;

impl ServerState {
    /// Resolves the document and hands the dispatcher a task; it never runs the
    /// task itself, so the main loop stays the only execution site.
    pub(super) fn hover_task(&mut self, params: &Value) -> Dispatched {
        let params: HoverParams = match super::params(super::method::HOVER, params) {
            Ok(params) => params,
            Err(e) => return Dispatched::Now(Err(e)),
        };
        let uri = &params.text_document_position_params.text_document.uri;
        if let Some(answer) = super::dispatch::configuration_not_ready(self, uri) {
            return answer;
        }
        if !self
            .effective_config(uri)
            .unwrap_or(&self.config.base)
            .hover
            .enabled
        {
            return Dispatched::Now(Ok(Value::Null));
        }
        match self.snapshot_for(uri) {
            Some(snapshot) => {
                Dispatched::Task(snapshot, prepare(params, self.client.hover_format.clone()))
            }
            // Closed between the request and here.
            None => Dispatched::Now(Ok(Value::Null)),
        }
    }
}

fn prepare(params: HoverParams, format: MarkupKind) -> RequestTask {
    Box::new(move |snapshot: &Snapshot, _: &mut Notices| answer(&params, &format, snapshot))
}

fn answer(params: &HoverParams, format: &MarkupKind, snapshot: &Snapshot) -> HandlerResult {
    let at = params.text_document_position_params.position;
    let number = LineNo::new(at.line);
    let Some(line) = snapshot.line(number) else {
        return Ok(Value::Null);
    };
    // The first caller the cache-population contract permits, and the reason it
    // was written: hover is viewport-bounded, and a user reading a line hovers
    // several of its fields. Every walker so far has been required to `analyze`
    // and drop instead.
    let analysis = snapshot.analysis(line);
    let Some(message) = analysis.message() else {
        return Ok(Value::Null);
    };

    // Fallible rather than asserted: see `ByteOffset::from_len`.
    let Some(byte) = ByteOffset::from_len(position_to_byte(
        line,
        Column::new(at.character),
        snapshot.encoding,
    )) else {
        return Ok(Value::Null);
    };
    // Between two fields — on a separator, or past the last one — is nothing,
    // not the nearest field: answering about something the cursor is not on is
    // worse than not answering.
    let Some(index) = message.field_idx_at(byte) else {
        return Ok(Value::Null);
    };
    let label = snapshot
        .names
        .of_analysis(message)
        .of(message.fields[index].owner());
    let Some(markdown) = hover(&line.bytes, message, index, &snapshot.render, label) else {
        return Ok(Value::Null);
    };

    let span = message.fields[index].span();
    let columns = Columns::of(line, [span.start, span.end], snapshot.encoding);
    Ok(json!({
        // One text under whichever kind the client asked for: two renderers
        // would drift, and markdown read verbatim is still readable.
        "contents": { "kind": format, "value": markdown },
        // What the answer is about, so the client can highlight it.
        "range": {
            "start": { "line": number, "character": columns.at(span.start) },
            "end": { "line": number, "character": columns.at(span.end) },
        },
    }))
}

// Every test here asserts the cache exists, so the module goes with it.
#[cfg(all(test, not(feature = "no-line-cache")))]
mod tests {
    use super::*;
    use crate::server::testing::{state_with, HEARTBEAT};
    use std::sync::Arc;

    /// Hover is the first caller the cache-population contract permits, and the
    /// reason it exists: a user reading a line hovers several of its fields.
    /// Asserted by identity rather than by a benchmark — `rss_viewport`
    /// populates 150 analyses directly, so its number neither moves nor says
    /// anything about who populated them.
    #[test]
    fn a_second_hover_on_one_line_reuses_the_first_analysis() {
        let (mut state, uri) = state_with(HEARTBEAT);
        let snapshot = state.snapshot_for(&uri).expect("open");
        let line = &snapshot.lines[0];

        assert!(
            line.analysis_if_cached().is_none(),
            "nothing has looked at this line yet"
        );

        let params: HoverParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 0, "character": 3 },
        }))
        .expect("params");
        answer(&params, &MarkupKind::Markdown, &snapshot).expect("answered");

        let after_first = line
            .analysis_if_cached()
            .expect("the hover populated the cache");
        answer(&params, &MarkupKind::Markdown, &snapshot).expect("answered");
        assert!(
            Arc::ptr_eq(
                &line.analysis_if_cached().expect("still cached"),
                &after_first
            ),
            "and the second hover read it rather than analysing again"
        );
    }
}
