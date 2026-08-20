//! `textDocument/codeLens`: one lens per message line, carrying the summary.
//!
//! **The list is whole-document by protocol** — `CodeLensParams` has no range —
//! so the first response is *classification only*: a line's range and the fact
//! that it is a message, which one `tokenize` answers without a dictionary,
//! groups or integrity. The summary is rendered in `codeLens/resolve`, which a
//! client calls for the lenses it displays. That is what keeps the expensive
//! half viewport-bounded when the cheap half cannot be.
//!
//! **Neither path populates the line cache.** The list walks the document, and
//! the protocol does not promise a client resolves only what it shows — so a
//! resolve is not viewport-bounded either. Both analyse and drop.

use lsp_types::CodeLensParams;
use serde_json::{json, Value};
use std::sync::Arc;

use super::dispatch::{Dispatched, HandlerResult, Notices, RequestTask, Snapshot};
use super::ServerState;
use crate::codec::{tokenize, LineKind};
use crate::document::LineNo;

impl ServerState {
    pub(super) fn code_lens_task(&mut self, params: &Value) -> Dispatched {
        let params: CodeLensParams = match super::params(super::method::CODE_LENS, params) {
            Ok(params) => params,
            Err(e) => return Dispatched::Now(Err(e)),
        };
        let uri = &params.text_document.uri;
        if let Some(answer) = super::dispatch::configuration_not_ready(self, uri) {
            return answer;
        }
        let summary = &self
            .effective_config(uri)
            .unwrap_or(&self.config.base)
            .message_summary;
        // The master switch and the channel, not the channel alone.
        if !summary.enabled || !summary.channels.code_lens {
            return Dispatched::Now(Ok(json!([])));
        }
        let cap = self
            .effective_config(uri)
            .unwrap_or(&self.config.base)
            .limits
            .code_lens_lines;
        match self.snapshot_for(&params.text_document.uri) {
            Some(snapshot) => Dispatched::Task(snapshot, prepare(params, cap)),
            None => Dispatched::Now(Ok(json!([]))),
        }
    }
}

fn prepare(params: CodeLensParams, cap: usize) -> RequestTask {
    Box::new(move |snapshot: &Snapshot, _: &mut Notices| list(&params, cap, snapshot))
}

fn list(params: &CodeLensParams, cap: usize, snapshot: &Snapshot) -> HandlerResult {
    // Past the cap, nothing — not the first `cap` lenses. A truncated list has
    // gaps a client cannot tell from lines that earned no lens.
    //
    // A trailing newline is not a line the user wrote, and every log has one:
    // counting it would refuse a file of exactly `cap` lines, which is the
    // boundary a cap is most likely to be set at. Found by the benchmark, which
    // returned nothing at what it believed was the cap.
    if snapshot.content_lines() > cap {
        return Ok(json!([]));
    }
    let uri = params.text_document.uri.as_str();
    let mut out = Vec::new();
    for (number, line) in snapshot.lines.iter().enumerate() {
        // `tokenize`, not `analyze`: the latter also resolves the dictionary,
        // decodes every field and annotates groups, none of which a lens needs
        // before it is resolved.
        if tokenize(&line.bytes).kind != LineKind::Message {
            continue;
        }
        let number = LineNo::new(number as u32);
        out.push(json!({
            "range": {
                "start": { "line": number, "character": 0 },
                "end": { "line": number, "character": 0 },
            },
            // No `command`: that is the half `resolve` fills.
            "data": snapshot.stamp(uri, number, line),
        }));
    }
    Ok(Value::Array(out))
}

impl ServerState {
    /// `codeLens/resolve`: the lens comes back and leaves with its title.
    ///
    /// This is where the summary is rendered — the half that is expensive and
    /// that a client asks for only where it draws.
    pub(super) fn code_lens_resolve_task(&mut self, params: &Value) -> Dispatched {
        let lens = params.clone();
        let data = &params["data"];
        let Some(uri) = data["uri"]
            .as_str()
            .and_then(|raw| super::parse_uri("codeLens/resolve", raw))
        else {
            return Dispatched::Now(Err(super::content_modified(
                super::method::CODE_LENS_RESOLVE,
                "the lens carries no document",
            )));
        };
        if let Some(answer) = super::dispatch::configuration_not_ready(self, &uri) {
            return answer;
        }
        match self.snapshot_for(&uri) {
            Some(snapshot) => Dispatched::Task(
                snapshot,
                Box::new(move |s, _: &mut Notices| resolve(lens, s)),
            ),
            None => Dispatched::Now(Err(super::content_modified(
                super::method::CODE_LENS_RESOLVE,
                "the document is closed",
            ))),
        }
    }
}

fn resolve(mut lens: Value, snapshot: &Snapshot) -> HandlerResult {
    let line = snapshot.stamped(&lens["data"], super::method::CODE_LENS_RESOLVE)?;

    // `analyze`, not `cached_analysis`: the protocol does not promise a client
    // resolves only the lenses it shows, so this is not viewport-bounded and
    // may not populate. Rendered from the owned result, which is then dropped.
    let analysis = Arc::new(crate::ctx::analyze(&line.bytes, &snapshot.ctx));
    let Some(message) = analysis.message() else {
        return Err(super::content_modified(
            super::method::CODE_LENS_RESOLVE,
            "the line is no longer a message",
        ));
    };
    // One flat string, because `Command.title` is one — this is the channel
    // that cannot keep the summary's halves apart.
    let Some(title) = crate::render::summary_joined(&line.bytes, message, &snapshot.render) else {
        // Nothing to say: a lens with no title at all, rather than an empty one.
        return Ok(lens);
    };
    lens["command"] = json!({ "title": title, "command": "" });
    Ok(lens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::testing::{cached_lines, heartbeats, state_with};

    fn uncached(snapshot: &Snapshot) -> bool {
        cached_lines(snapshot).iter().all(|seen| !seen)
    }

    /// Neither lens path may populate: the list walks the whole document, and
    /// the protocol does not promise a client resolves only the lenses it
    /// shows. Asserted by looking rather than by a memory number, because a
    /// benchmark measures how much was retained and not who retained it.
    #[test]
    fn neither_listing_nor_resolving_populates_the_cache() {
        let (mut state, uri) = state_with(&heartbeats(3));
        let snapshot = state.snapshot_for(&uri).expect("open");
        let params: CodeLensParams = serde_json::from_value(json!({
            "textDocument": { "uri": uri.as_str() },
        }))
        .expect("params");

        let lenses = list(&params, 100, &snapshot).expect("listed");
        let lenses = lenses.as_array().expect("an array");
        assert_eq!(lenses.len(), 3);
        assert!(uncached(&snapshot), "the list analysed nothing it kept");

        let resolved = resolve(lenses[0].clone(), &snapshot).expect("resolved");
        assert!(
            resolved["command"]["title"].is_string(),
            "and it did render: {resolved}"
        );
        assert!(
            uncached(&snapshot),
            "from an owned analysis it then dropped"
        );
    }
}
