//! `initialize` result and position-encoding negotiation.

use lsp_types::{
    ClientCapabilities, ClientSemanticTokensRequestOptionsRange, CodeActionKind, CodeActionOptions,
    CodeLensOptions, ExecuteCommandOptions, InitializeResult, InlayHintOptions,
    PositionEncodingKind, SemanticTokensFullDelta, SemanticTokensLegend, SemanticTokensOptions,
    ServerCapabilities, ServerInfo, TextDocumentSync, TextDocumentSyncKind,
    WorkDoneProgressOptions, WorkspaceFoldersServerCapabilities, WorkspaceOptions,
};
use serde_json::Value;

use crate::document::PositionEncoding;
use crate::render::tokens;
use crate::{SERVER_NAME, SERVER_VERSION};

/// utf-8 if the client advertises it in `general.positionEncodings`, else
/// utf-16 — the protocol's default, and the main path rather than a fallback.
pub fn negotiate_encoding(caps: &ClientCapabilities) -> PositionEncoding {
    let offers_utf8 = caps.general.as_ref().is_some_and(|general| {
        general
            .position_encodings
            .as_ref()
            .is_some_and(|kinds| kinds.contains(&PositionEncodingKind::UTF8))
    });
    if offers_utf8 {
        PositionEncoding::Utf8
    } else {
        PositionEncoding::Utf16
    }
}

/// A code action is an edit the client applies. Without literals we could only
/// send a `Command`, which needs `executeCommand` (Stage 4d); without versioned
/// edits we would send a repair with no protection against the text having
/// moved. Either way the honest answer is to advertise nothing.
fn offers_code_actions(caps: &ClientCapabilities) -> bool {
    let literals = caps.text_document.as_ref().is_some_and(|doc| {
        doc.code_action
            .as_ref()
            .is_some_and(|action| action.code_action_literal_support.is_some())
    });
    literals && versioned_edits(caps)
}

/// Read by truth rather than by presence: a client that sends `false` is
/// refusing, not staying silent.
fn versioned_edits(caps: &ClientCapabilities) -> bool {
    caps.workspace.as_ref().is_some_and(|workspace| {
        workspace
            .workspace_edit
            .as_ref()
            .is_some_and(|edit| edit.document_changes.unwrap_or(false))
    })
}

/// Whether the client will resolve a lens's `command`.
///
/// **Silence means yes.** `codeLens.resolveSupport` is `@since 3.18.0` and
/// almost nothing sends it yet, while `codeLens/resolve` has existed since 3.0
/// and every client that draws lenses resolves them. Treating an absent
/// capability as refusal would withhold the *default summary channel* from
/// every client in existence, which is the opposite of adapting to what a
/// client said.
///
/// So this refuses only on an explicit refusal: a `resolveSupport` that is
/// present and does not name `command` is a client saying it will not ask, and
/// a lens it never resolves is a blank line above the code. Such a client gets
/// no provider — and, because `messageSummary.channels.inlayHints` defaults
/// off, no summary at all unless the user switches the other channel on. That
/// is why the withholding is *said out loud* rather than left to be noticed:
/// see `on_initialize`.
pub(super) fn resolves_lens_commands(caps: &ClientCapabilities) -> bool {
    let Some(support) = caps
        .text_document
        .as_ref()
        .and_then(|doc| doc.code_lens.as_ref())
        .and_then(|lens| lens.resolve_support.as_ref())
    else {
        return true;
    };
    support.properties.iter().any(|name| name == "command")
}

/// What is implemented, gated by what the client said it can receive.
/// Whether the client says it will send `semanticTokens/range`.
///
/// Read by truth rather than by presence: the protocol types the field as a
/// boolean or an empty object, so `{}` means yes and `false` means **no**. A
/// client that writes `false` is refusing, and reading it as a declaration
/// would leave it with the shape it refused and without the one it wanted.
pub(super) fn declares_range(caps: &ClientCapabilities) -> bool {
    caps.text_document
        .as_ref()
        .and_then(|text| text.semantic_tokens.as_ref())
        .and_then(|tokens| tokens.requests.range.as_ref())
        .is_some_and(|range| !matches!(range, ClientSemanticTokensRequestOptionsRange::Bool(false)))
}

pub(super) fn overlaps_tokens(caps: &ClientCapabilities) -> bool {
    caps.text_document
        .as_ref()
        .and_then(|text| text.semantic_tokens.as_ref())
        .and_then(|tokens| tokens.overlapping_token_support)
        .unwrap_or(false)
}

pub(super) fn refreshes_semantic_tokens(caps: &ClientCapabilities) -> bool {
    caps.workspace
        .as_ref()
        .and_then(|workspace| workspace.semantic_tokens.as_ref())
        .and_then(|tokens| tokens.refresh_support)
        .unwrap_or(false)
}

/// One shape or the other, never both.
///
/// A client that has the viewport path keeps it: `range` answers what is on
/// screen and costs the screen, while `full` answers the file and is capped.
/// Offering both would let a client take `full` and colour the whole log on
/// every keystroke, so the one it says it will send decides.
///
/// **An accepted limitation.** The spec permits a client offered only `range`
/// to degrade — no minimap, or no colouring at all. Declaring `range` says a
/// request will be sent, not that the client renders what comes back.
fn semantic_tokens_provider(caps: &ClientCapabilities) -> SemanticTokensOptions {
    SemanticTokensOptions {
        legend: SemanticTokensLegend {
            token_types: tokens::TYPES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            token_modifiers: tokens::MODIFIERS
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        },
        range: declares_range(caps).then(|| true.into()),
        full: (!declares_range(caps)).then(|| SemanticTokensFullDelta { delta: Some(true) }.into()),
        work_done_progress_options: WorkDoneProgressOptions::default(),
    }
}

pub fn initialize_result(encoding: PositionEncoding, caps: &ClientCapabilities) -> Value {
    let result = InitializeResult {
        capabilities: ServerCapabilities {
            position_encoding: Some(match encoding {
                PositionEncoding::Utf8 => PositionEncodingKind::UTF8,
                PositionEncoding::Utf16 => PositionEncodingKind::UTF16,
            }),
            text_document_sync: Some(TextDocumentSync::Kind(TextDocumentSyncKind::Incremental)),
            hover_provider: Some(true.into()),
            // Always advertised; `inlayHints.*` and `messageSummary.channels`
            // gate the response, so a client need not reconnect to change it.
            inlay_hint_provider: Some(
                InlayHintOptions {
                    resolve_provider: Some(true),
                    ..Default::default()
                }
                .into(),
            ),
            // Withheld only from a client that said it will not resolve a
            // `command`; see `resolves_lens_commands`.
            code_lens_provider: resolves_lens_commands(caps).then(|| CodeLensOptions {
                resolve_provider: Some(true),
                ..Default::default()
            }),
            // No configuration of its own: expanding a selection is not a
            // feature to switch off, it is what the cursor already means.
            selection_range_provider: Some(true.into()),
            // Withheld from a client that takes no nested answer, the way the
            // lens provider is withheld from one that resolves no command: the
            // outline is only offered nested, and advertising a provider whose
            // answer a client cannot read is worse than not offering it.
            // `documentSymbol.enabled` gates the *response* for everyone else,
            // so switching the outline off needs no reconnect.
            document_symbol_provider: super::document_symbol::takes_nested_symbols(caps)
                .then(|| true.into()),
            // No `resolveProvider`: a quickfix carries its edit already, and
            // `source.fixAll` — the only thing that would be resolved — waits
            // for the worker pool in Stage 4b.
            code_action_provider: offers_code_actions(caps).then(|| {
                CodeActionOptions {
                    code_action_kinds: Some(vec![CodeActionKind::QuickFix]),
                    ..Default::default()
                }
                .into()
            }),
            // Advertised whatever `semanticTokens.enabled` says, as hover is:
            // the switch gates the answer, not the capability.
            semantic_tokens_provider: Some(semantic_tokens_provider(caps).into()),
            // Always advertised. A command has no capability to negotiate
            // against — a client either invokes it or does not — and the list
            // is what a client matches a name against before sending one.
            execute_command_provider: Some(ExecuteCommandOptions {
                commands: vec![
                    super::execute_command::DECODE.to_owned(),
                    super::execute_command::USE_DICTIONARY.to_owned(),
                    super::execute_command::DESCRIBE_OVERRIDES.to_owned(),
                    super::execute_command::DESCRIBE_TAG.to_owned(),
                ],
                ..Default::default()
            }),
            workspace: Some(WorkspaceOptions {
                workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                    supported: Some(true),
                    change_notifications: Some(true.into()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        server_info: Some(ServerInfo {
            name: SERVER_NAME.to_owned(),
            version: Some(SERVER_VERSION.to_owned()),
        }),
    };
    serde_json::to_value(result).expect("InitializeResult serialises")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn caps(value: Value) -> ClientCapabilities {
        serde_json::from_value(value).expect("capabilities")
    }

    #[test]
    fn configuration_commands_and_workspace_folder_changes_are_advertised() {
        let result = initialize_result(PositionEncoding::Utf8, &ClientCapabilities::default());
        assert_eq!(
            result["capabilities"]["executeCommandProvider"]["commands"],
            json!([
                "fix.decode",
                "fix.useDictionary",
                "fix.describeOverrides",
                "fix.describeTag"
            ])
        );
        assert_eq!(
            result["capabilities"]["workspace"]["workspaceFolders"],
            json!({
                "supported": true, "changeNotifications": true,
            })
        );
    }

    fn provider(value: Value) -> Option<Value> {
        let caps = caps(value);
        initialize_result(PositionEncoding::Utf8, &caps)
            .pointer("/capabilities/codeActionProvider")
            .cloned()
    }

    fn taking_actions() -> Value {
        json!({
            "textDocument": { "codeAction": {
                "codeActionLiteralSupport": { "codeActionKind": { "valueSet": ["quickfix"] } },
            }},
            "workspace": { "workspaceEdit": { "documentChanges": true } },
        })
    }

    /// A code action is an edit the client applies. Offering one to a client
    /// that cannot take a versioned literal would mean either sending a shape
    /// it did not agree to, or sending an edit with no protection against the
    /// text having moved.
    #[test]
    fn code_actions_are_offered_only_to_a_client_that_can_take_them() {
        let offered = provider(taking_actions()).expect("advertised");
        assert_eq!(offered["codeActionKinds"], json!(["quickfix"]));
        assert!(
            offered.get("resolveProvider").is_none(),
            "nothing is left unfilled until source.fixAll exists"
        );

        assert_eq!(
            provider(json!({ "workspace": { "workspaceEdit": { "documentChanges": true } } })),
            None,
            "no literals"
        );
        assert_eq!(
            provider(json!({
                "textDocument": { "codeAction": {
                    "codeActionLiteralSupport": { "codeActionKind": { "valueSet": [] } },
                }},
                "workspace": { "workspaceEdit": { "documentChanges": false } },
            })),
            None,
            "literals, but the edit could not carry a version"
        );
        assert_eq!(provider(json!({})), None);
    }

    /// One shape or the other. A client with the viewport path keeps it and is
    /// never offered the whole-document one it would otherwise prefer.
    #[test]
    fn the_request_shape_a_client_declares_is_the_one_it_is_offered() {
        let shape = |value: Value| {
            let provider =
                serde_json::to_value(semantic_tokens_provider(&caps(value))).expect("serialises");
            (
                provider.get("range").cloned(),
                provider.get("full").cloned(),
            )
        };
        let asking_range = json!({
            "textDocument": { "semanticTokens": { "requests": { "range": true },
                "tokenTypes": [], "tokenModifiers": [], "formats": [] } }
        });
        assert_eq!(shape(asking_range), (Some(json!(true)), None));

        // An empty object says the same thing as `true`.
        let as_object = json!({
            "textDocument": { "semanticTokens": { "requests": { "range": {} },
                "tokenTypes": [], "tokenModifiers": [], "formats": [] } }
        });
        assert_eq!(shape(as_object), (Some(json!(true)), None));

        let asking_full = json!({
            "textDocument": { "semanticTokens": { "requests": { "full": true },
                "tokenTypes": [], "tokenModifiers": [], "formats": [] } }
        });
        assert_eq!(
            shape(asking_full),
            (None, Some(json!({ "delta": true }))),
            "and deltas with it"
        );
        assert_eq!(shape(json!({})), (None, Some(json!({ "delta": true }))));

        // `false` is a refusal, not a declaration: reading it as one would
        // leave the client with the shape it refused and without the other.
        let refusing = json!({
            "textDocument": { "semanticTokens": { "requests": { "range": false, "full": true },
                "tokenTypes": [], "tokenModifiers": [], "formats": [] } }
        });
        assert_eq!(shape(refusing), (None, Some(json!({ "delta": true }))));
    }

    /// The legend is what a token's index means, so it is sent verbatim.
    #[test]
    fn the_legend_is_advertised_in_wire_order() {
        let provider =
            serde_json::to_value(semantic_tokens_provider(&caps(json!({})))).expect("serialises");
        assert_eq!(provider["legend"]["tokenTypes"][0], "property");
        assert_eq!(
            provider["legend"]["tokenTypes"]
                .as_array()
                .expect("a list")
                .len(),
            tokens::TYPES.len()
        );
        assert_eq!(provider["legend"]["tokenModifiers"][0], "deprecated");
    }

    #[test]
    fn utf8_is_taken_only_when_the_client_lists_it() {
        let listed = caps(json!({ "general": { "positionEncodings": ["utf-16", "utf-8"] } }));
        assert_eq!(negotiate_encoding(&listed), PositionEncoding::Utf8);
        let other = caps(json!({ "general": { "positionEncodings": ["utf-16"] } }));
        assert_eq!(negotiate_encoding(&other), PositionEncoding::Utf16);
        assert_eq!(
            negotiate_encoding(&ClientCapabilities::default()),
            PositionEncoding::Utf16
        );
    }
}
