//! Lifecycle handlers: `initialize` and `$/setTrace`.

use std::path::PathBuf;

use lsp_types::{ClientCapabilities, MarkupKind};
use serde::Deserialize;
use serde_json::Value;

use super::{capabilities, from_params, parse_uri, ServerState};

#[derive(Deserialize)]
struct SetTraceParams {
    value: String,
}

impl ServerState {
    pub(super) fn on_initialize(&mut self, params: &Value) -> Value {
        let caps = client_capabilities(params);
        self.encoding = capabilities::negotiate_encoding(&caps);
        self.client.configuration = caps
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.configuration)
            .unwrap_or(false);
        // Progress is only creatable when the client advertises it; using a
        // token it did not agree to is a protocol violation, not a degradation.
        self.client.progress = caps
            .window
            .as_ref()
            .and_then(|window| window.work_done_progress)
            .unwrap_or(false);
        self.client.hover_format = hover_format(&caps);
        self.client.defers_tooltips = super::inlay_hint::defers_tooltips(&caps);
        self.client.extended_symbol_kinds = super::document_symbol::lists_symbol_kinds(&caps);

        let refreshes = |pick: fn(&lsp_types::WorkspaceClientCapabilities) -> Option<bool>| {
            caps.workspace.as_ref().and_then(pick).unwrap_or(false)
        };
        self.client.refreshes_inlay_hints = refreshes(|workspace| {
            workspace
                .inlay_hint
                .as_ref()
                .and_then(|it| it.refresh_support)
        });
        self.client.refreshes_code_lens = refreshes(|workspace| {
            workspace
                .code_lens
                .as_ref()
                .and_then(|it| it.refresh_support)
        });
        self.client.refreshes_semantic_tokens = capabilities::refreshes_semantic_tokens(&caps);
        // The group overlay covers ranges the member fields already carry, so a
        // client that does not render overlap gets the fields alone.
        self.client.overlaps_tokens = capabilities::overlaps_tokens(&caps);
        // Read by truth rather than by presence: a client that sends `false` is
        // refusing, not staying silent.
        self.client.prefers_actions = caps
            .text_document
            .as_ref()
            .and_then(|doc| doc.code_action.as_ref())
            .and_then(|action| action.is_preferred_support)
            .unwrap_or(false);
        if !self.client.configuration {
            log::info!(
                "client does not support workspace/configuration; \
                 falling back to settings pushed via didChangeConfiguration"
            );
        }
        // The root must be known before the first config is applied: relative
        // dictionary paths resolve against it.
        self.root_dir = initialization_root(params);
        self.root_uri = params
            .get("rootUri")
            .and_then(Value::as_str)
            .and_then(|uri| lsp_types::Uri::parse(uri).ok())
            .and_then(|uri| uri.to_file_path().ok());
        self.workspace_folders = params["workspaceFolders"]
            .as_array()
            .map(|folders| {
                folders
                    .iter()
                    .filter_map(super::config_pull::folder_path)
                    .collect()
            })
            .unwrap_or_default();
        if let Some(fix) = params.pointer("/initializationOptions/fix") {
            self.config.initial_client = crate::overrides::parse_client(fix.clone()).ok();
            self.apply_base_value(fix);
        } else {
            self.apply_base_value(&serde_json::json!({}));
        }
        // Withholding the lens provider takes the summary away from a client
        // whose configuration still asks for it, and the other channel is off
        // by default — so this is `[]` and a reason, as everywhere else.
        let summary = &self.config.base.message_summary;
        if summary.enabled
            && summary.channels.code_lens
            && !capabilities::resolves_lens_commands(&caps)
        {
            self.log_message(
                lsp_types::MessageType::Info,
                "fix-lsp: this client resolves no code lens `command`, so the summary's \
                 code-lens channel is not offered; set \
                 `fix.messageSummary.channels.inlayHints` to true to take the summary \
                 through inlay hints instead",
            );
        }
        // The provider is withheld from this client, so `initialize` is the
        // only moment it can be told: it will never send a request to be
        // answered on. A named limitation rather than a second answer shape —
        // `SymbolInformation`, what a client predating the capability would
        // take, is deprecated in the protocol this speaks.
        if self.config.base.document_symbol.enabled
            && !super::document_symbol::takes_nested_symbols(&caps)
        {
            self.log_message(
                lsp_types::MessageType::Info,
                "fix-lsp: this client advertises no \
                 `textDocument.documentSymbol.hierarchicalDocumentSymbolSupport`, so no \
                 document-symbol provider is offered; the outline is only produced nested",
            );
        }
        capabilities::initialize_result(self.encoding, &caps)
    }

    pub(super) fn on_set_trace(&mut self, params: Value) {
        if let Some(params) = from_params::<SetTraceParams>(super::method::SET_TRACE, params) {
            crate::logging::set_trace_level(&params.value);
        }
    }
}

/// The first content format the client prefers that we can produce.
///
/// `contentFormat` is ordered — `["plaintext", "markdown"]` asks for plaintext
/// first — so this walks it rather than testing for membership. We render one
/// text either way, so "can produce" is true of both.
///
/// Markdown when the client says nothing, and not because the protocol says so
/// — it names no default. The text we send is markdown regardless, so a client
/// that cannot render it sees the same characters under either label; the only
/// case the choice decides is a client that *can* render markdown and did not
/// say so, and there `plaintext` would show it raw for nothing.
fn hover_format(caps: &ClientCapabilities) -> MarkupKind {
    caps.text_document
        .as_ref()
        .and_then(|doc| doc.hover.as_ref())
        .and_then(|hover| hover.content_format.as_ref())
        .and_then(|formats| {
            formats
                .iter()
                .find(|format| matches!(format, MarkupKind::Markdown | MarkupKind::PlainText))
                .cloned()
        })
        .unwrap_or(MarkupKind::Markdown)
}

/// The client's capabilities, typed. Unknown keys are ignored and absent ones
/// read as "not advertised", so only a capability of the *wrong shape* can
/// fail — a client violating the spec, not merely a new or unusual one.
///
/// Parsed one branch at a time rather than as a whole, so that a bad field
/// anywhere costs only its own branch. Parsing the tree in one go would let a
/// quirk in a capability we never read take the position encoding down with it.
fn client_capabilities(params: &Value) -> ClientCapabilities {
    let Some(value) = params.get("capabilities") else {
        return ClientCapabilities::default();
    };
    ClientCapabilities {
        workspace: branch(value, "workspace"),
        text_document: branch(value, "textDocument"),
        window: branch(value, "window"),
        general: branch(value, "general"),
        ..Default::default()
    }
}

/// One capability branch. Downstream, "not sent" and "sent malformed" are the
/// same thing — not advertised — so both yield `None`; only the second is the
/// client's mistake, and only the second is logged.
fn branch<T: serde::de::DeserializeOwned>(caps: &Value, key: &str) -> Option<T> {
    match parse_branch(caps, key) {
        Ok(branch) => branch,
        Err(e) => {
            log::warn!(
                "client capability `{key}.{}` is malformed ({}); treating `{key}` as unadvertised",
                e.path(),
                e.inner()
            );
            None
        }
    }
}

/// The two cases kept apart, so that which is which can be tested rather than
/// only logged. `Ok(None)` is a client that said nothing — absent, or the
/// explicit `null` that LSP and JSON-RPC both use for an omitted optional
/// object, and which it would be wrong to blame anyone for.
///
/// The error carries the path: `serde_json` alone says "expected a boolean"
/// without naming which of `textDocument`'s dozens of fields it means.
fn parse_branch<T: serde::de::DeserializeOwned>(
    caps: &Value,
    key: &str,
) -> Result<Option<T>, serde_path_to_error::Error<serde_json::Error>> {
    let Some(value) = caps.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    serde_path_to_error::deserialize(value).map(Some)
}

/// First workspace folder, else `rootUri`; only `file:` roots yield a directory.
pub(super) fn initialization_root(params: &Value) -> Option<PathBuf> {
    let raw = params
        .pointer("/workspaceFolders/0/uri")
        .and_then(Value::as_str)
        .or_else(|| params["rootUri"].as_str())?;
    parse_uri("initialization root", raw)?.to_file_path().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Uri;
    use serde_json::json;
    use std::path::Path;

    /// A `file:` URI for an absolute path in the platform's own form — the
    /// round trip a real client's root goes through. Building the URI from a
    /// path rather than writing `file:///from-folder` by hand is what keeps
    /// this about precedence: a URI with no drive letter is not a path on
    /// Windows, so the literal form asserted a platform fact by accident.
    fn file_uri(path: &Path) -> String {
        Uri::from_file_path(path)
            .expect("an absolute path converts")
            .to_string()
    }

    #[test]
    fn workspace_folders_take_precedence_over_root_uri() {
        let folder = std::env::temp_dir().join("fix-lsp-folder");
        let root = std::env::temp_dir().join("fix-lsp-root");

        let both = json!({
            "rootUri": file_uri(&root),
            "workspaceFolders": [{ "uri": file_uri(&folder), "name": "f" }]
        });
        assert_eq!(initialization_root(&both), Some(folder));

        let only_root = json!({ "rootUri": file_uri(&root), "workspaceFolders": null });
        assert_eq!(initialization_root(&only_root), Some(root));
    }

    #[test]
    fn a_root_that_names_no_file_path_yields_none() {
        for raw in ["untitled:x", "not a uri", "http://example.com/x"] {
            assert_eq!(
                initialization_root(&json!({ "rootUri": raw })),
                None,
                "{raw}"
            );
        }
        assert_eq!(initialization_root(&json!({ "rootUri": null })), None);
    }

    /// A branch the client did not send is not the client's mistake, and an
    /// explicit `null` is one of the ways to not send it. Told apart here
    /// rather than only in the log, which a test cannot read.
    #[test]
    fn a_branch_the_client_did_not_send_is_not_a_malformed_one() {
        let parsed = |caps: Value| {
            parse_branch::<lsp_types::WorkspaceClientCapabilities>(&caps, "workspace")
                .map(|branch| branch.is_some())
                .map_err(|e| e.path().to_string())
        };
        assert_eq!(parsed(json!({})), Ok(false), "absent");
        assert_eq!(
            parsed(json!({ "workspace": null })),
            Ok(false),
            "explicit no"
        );
        assert_eq!(parsed(json!({ "workspace": {} })), Ok(true), "sent, empty");
    }

    /// The whole point of the path: a client author reading the log learns which
    /// field, not merely which type.
    #[test]
    fn a_malformed_branch_names_the_field_that_broke_it() {
        let caps = json!({ "workspace": { "workspaceEdit": { "documentChanges": "yes" } } });
        let e = parse_branch::<lsp_types::WorkspaceClientCapabilities>(&caps, "workspace")
            .expect_err("malformed");
        assert_eq!(e.path().to_string(), "workspaceEdit.documentChanges");
    }

    /// Percent-decoding is the `url` crate's job; this pins that we rely on it
    /// instead of decoding ourselves. Unix-only because the literal URI it needs
    /// carries no drive letter, which on Windows is not a path at all — the
    /// distinction the test above deliberately no longer makes.
    #[cfg(unix)]
    #[test]
    fn a_percent_encoded_root_decodes() {
        assert_eq!(
            initialization_root(&json!({ "rootUri": "file:///tmp/my%20dir/x" })),
            Some(PathBuf::from("/tmp/my dir/x"))
        );
    }
}
