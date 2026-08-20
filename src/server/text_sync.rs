//! Text-document synchronisation: `didOpen`, `didChange`, `didClose`.
//!
//! Params are deserialised into typed structs, so a notification that does not
//! match the protocol's schema is rejected whole — a `didOpen` without `text` is
//! a malformed message, not an empty document at version 0. This is also where
//! the transport's byte escaping is recovered: the document model below this
//! layer knows nothing about the wire format.

use lsp_types::Range;
use serde::Deserialize;
use serde_json::Value;

use super::{from_params, has_empty_path, parse_uri, ServerState};
use crate::document::{Change, Document};
use crate::overrides::modeline::EDGE_LINES;
use crate::transport::text_into_bytes;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DidOpenParams {
    text_document: OpenedDocument,
}

#[derive(Deserialize)]
struct OpenedDocument {
    uri: String,
    version: i32,
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DidChangeParams {
    text_document: VersionedDocument,
    content_changes: Vec<RawChange>,
}

#[derive(Deserialize)]
struct VersionedDocument {
    uri: String,
    version: i32,
}

#[derive(Deserialize)]
struct RawChange {
    /// Absent or null means a full-document replacement. A *malformed* range is
    /// a deserialisation error, so it rejects the notification instead of
    /// quietly degrading into one.
    #[serde(default)]
    range: Option<Range>,
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DidCloseParams {
    text_document: DocumentId,
}

#[derive(Deserialize)]
struct DocumentId {
    uri: String,
}

impl ServerState {
    pub(super) fn on_did_open(&mut self, params: Value) {
        let Some(params) = from_params::<DidOpenParams>("didOpen", params) else {
            return;
        };
        let OpenedDocument { uri, version, text } = params.text_document;
        let Some(uri) = parse_uri("didOpen", &uri) else {
            return;
        };
        if has_empty_path(&uri) {
            // Not a client error: `vim.uri_from_bufnr` yields this for an unnamed
            // buffer. Skipped because `file://` resolves to a fresh buffer on every
            // call, so it can never identify a document. Debug, not warn: a client
            // may re-send it and we would spam the log.
            log::debug!("skipping document with an empty URI path");
            return;
        }
        let doc = Document::open(&text_into_bytes(text), version, self.ctx.clone());
        let reopened = self.docs.insert(uri.clone(), doc).is_some();
        // A reopened document keeps its URI, so an id from the last time it was
        // open would match text that is gone.
        self.tokens.remove(&uri);
        if reopened {
            self.clear_diagnostics(&uri, version);
        }
        self.configure_open_document(&uri);
    }

    pub(super) fn on_did_change(&mut self, params: Value) {
        let Some(params) = from_params::<DidChangeParams>("didChange", params) else {
            return;
        };
        let Some(uri) = parse_uri("didChange", &params.text_document.uri) else {
            return;
        };
        let version = params.text_document.version;
        let changes: Vec<Change> = params
            .content_changes
            .into_iter()
            .map(|change| {
                let text = text_into_bytes(change.text);
                match change.range {
                    Some(range) => Change::Ranged { range, text },
                    None => Change::Full(text),
                }
            })
            .collect();

        let encoding = self.encoding;
        let Some(doc) = self.docs.get_mut(&uri) else {
            log::warn!(
                "didChange for a document that is not open: {}",
                uri.as_str()
            );
            return;
        };
        // A rejected change did not move the document, so nothing to re-validate.
        let edited = match doc.apply_changes(&changes, version, encoding) {
            Ok(edited) => edited,
            Err(e) => {
                // A rejected batch changed nothing, so the digest keeps saying
                // what the client still holds.
                log::warn!("didChange rejected for {}: {e}", uri.as_str());
                return;
            }
        };
        let rescan = match edited {
            crate::document::Edited::Everything => true,
            crate::document::Edited::Nothing => false,
            crate::document::Edited::Lines {
                first,
                last,
                line_delta,
            } => {
                line_delta != 0
                    || (first as usize) < EDGE_LINES
                    || last as usize >= doc.content_lines().saturating_sub(EDGE_LINES)
            }
        };
        if let Some(digest) = self.tokens.get_mut(&uri) {
            digest.edited = digest.edited.then(edited);
        }
        if rescan {
            self.configure_edited_document(&uri);
        }
        self.schedule_diagnostics(&uri, version);
    }

    pub(super) fn on_did_close(&mut self, params: Value) {
        let Some(params) = from_params::<DidCloseParams>("didClose", params) else {
            return;
        };
        let Some(uri) = parse_uri("didClose", &params.text_document.uri) else {
            return;
        };
        let version = self.docs.remove(&uri).map_or(0, |doc| doc.version);
        self.clear_diagnostics(&uri, version);
        // Drop the document's configuration state too, or it accumulates for the
        // whole session. This is also what rejects a late pull reply for it.
        self.config.documents.remove(&uri);
        self.config.lifetimes.remove(&uri);
        self.config.revisions.remove(&uri);
        self.config.modelines.remove(&uri);
        self.config.waiting.remove(&uri);
        self.tokens.remove(&uri);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Uri;
    use serde_json::json;

    fn state() -> ServerState {
        // The client advertises no configuration support here, so no pull is
        // ever sent; a disconnected channel would be ignored anyway.
        let (sender, _) = crossbeam_channel::unbounded::<lsp_server::Message>();
        ServerState::new(sender)
    }

    fn opened() -> ServerState {
        let mut state = state();
        state.on_did_open(json!({
            "textDocument": { "uri": "file:///a.fix", "version": 1, "text": "abc" }
        }));
        state
    }

    fn line0(state: &ServerState) -> Vec<u8> {
        state.docs[&Uri::parse("file:///a.fix").unwrap()].lines[0]
            .bytes
            .to_vec()
    }

    #[test]
    fn did_open_without_text_is_rejected() {
        let mut state = state();
        state.on_did_open(json!({
            "textDocument": { "uri": "file:///a.fix", "version": 1 }
        }));
        assert!(
            state.docs.is_empty(),
            "malformed didOpen must not open a document"
        );
    }

    #[test]
    fn did_open_without_version_is_rejected() {
        let mut state = state();
        state.on_did_open(json!({
            "textDocument": { "uri": "file:///a.fix", "text": "abc" }
        }));
        assert!(state.docs.is_empty());
    }

    #[test]
    fn did_open_recovers_escaped_bytes() {
        let mut state = state();
        state.on_did_open(json!({
            "textDocument": { "uri": "file:///a.fix", "version": 1, "text": "A\u{F780}B" }
        }));
        assert_eq!(line0(&state), vec![b'A', 0x80, b'B']);
    }

    #[test]
    fn did_open_with_empty_uri_path_is_skipped() {
        let mut state = state();
        state.on_did_open(json!({
            "textDocument": { "uri": "file://", "version": 1, "text": "abc" }
        }));
        assert!(state.docs.is_empty());
    }

    #[test]
    fn unparsable_uri_rejects_the_notification() {
        let mut state = state();
        state.on_did_open(json!({
            "textDocument": { "uri": "not a uri", "version": 1, "text": "abc" }
        }));
        assert!(state.docs.is_empty(), "no document for an unparsable URI");
    }

    #[test]
    fn url_equal_spellings_are_one_document() {
        // The url crate normalises the scheme; identity is the parsed URL.
        let mut state = state();
        state.on_did_open(json!({
            "textDocument": { "uri": "FILE:///tmp/a.fix", "version": 1, "text": "abc" }
        }));
        state.on_did_change(json!({
            "textDocument": { "uri": "file:///tmp/a.fix", "version": 2 },
            "contentChanges": [{ "text": "zzz" }]
        }));
        assert_eq!(state.docs.len(), 1);
        let doc = &state.docs[&Uri::parse("file:///tmp/a.fix").unwrap()];
        assert_eq!(&*doc.lines[0].bytes, b"zzz");
    }

    #[test]
    fn did_change_without_version_leaves_document_untouched() {
        let mut state = opened();
        state.on_did_change(json!({
            "textDocument": { "uri": "file:///a.fix" },
            "contentChanges": [{ "text": "zzz" }]
        }));
        assert_eq!(line0(&state), b"abc");
    }

    #[test]
    fn did_change_with_malformed_range_is_rejected_wholesale() {
        let mut state = opened();
        state.on_did_change(json!({
            "textDocument": { "uri": "file:///a.fix", "version": 2 },
            "contentChanges": [
                { "range": { "start": {"line":0,"character":0}, "end": {"line":0,"character":1} },
                  "text": "Z" },
                { "range": { "start": {"line":0} }, "text": "Y" }
            ]
        }));
        assert_eq!(
            line0(&state),
            b"abc",
            "a malformed later change must not leave the earlier one applied"
        );
    }

    #[test]
    fn did_change_with_a_change_missing_text_is_rejected() {
        let mut state = opened();
        state.on_did_change(json!({
            "textDocument": { "uri": "file:///a.fix", "version": 2 },
            "contentChanges": [{ "range": { "start": {"line":0,"character":0},
                                            "end": {"line":0,"character":1} } }]
        }));
        assert_eq!(line0(&state), b"abc");
    }

    #[test]
    fn did_change_with_null_range_replaces_the_document() {
        let mut state = opened();
        state.on_did_change(json!({
            "textDocument": { "uri": "file:///a.fix", "version": 2 },
            "contentChanges": [{ "range": null, "text": "zzz" }]
        }));
        assert_eq!(line0(&state), b"zzz");
    }

    #[test]
    fn well_formed_did_change_still_applies() {
        let mut state = opened();
        state.on_did_change(json!({
            "textDocument": { "uri": "file:///a.fix", "version": 2 },
            "contentChanges": [{ "range": { "start": {"line":0,"character":0},
                                            "end": {"line":0,"character":1} },
                                 "text": "Z" }]
        }));
        assert_eq!(line0(&state), b"Zbc");
    }

    #[test]
    fn did_close_clears_document_configuration_state() {
        let mut state = opened();
        let uri = Uri::parse("file:///a.fix").unwrap();
        super::super::testing::settle(&mut state);
        assert!(state.config.documents.contains_key(&uri));

        state.on_did_close(json!({ "textDocument": { "uri": "file:///a.fix" } }));

        assert!(state.docs.is_empty());
        assert!(!state.config.documents.contains_key(&uri));
        assert!(!state.config.lifetimes.contains_key(&uri));
    }
}
