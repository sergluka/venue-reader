//! End-to-end checks over the built binary's real stdio transport.

mod stdio_harness;

use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};
use stdio_harness::StdioClient;

const RAW_BYTE_MARKER: &str = "FIX_LSP_RAW_BYTE";

#[test]
fn version_flag_prints_and_exits() {
    let output = Command::new(env!("CARGO_BIN_EXE_fix-lsp"))
        .arg("--version")
        .output()
        .expect("run --version");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("fix-lsp"));
}

#[test]
fn a_real_client_round_trips_the_full_stdio_lifecycle() {
    let mut client = StdioClient::start(
        "lifecycle",
        Some("[inlayHints.tagNames]\nformat = '<{}>'\n"),
        Some(json!([{
            "inlayHints": { "tagNames": { "enabled": true } }
        }])),
    );
    assert!(client.personal_file().is_file());

    let bad_both = include_str!("fixtures/validation/bad-both.fix").trim_end();
    let raw_line = format!("8=FIX.4.4|58=A{RAW_BYTE_MARKER}B|35=0|49=CLIENT1|56=BROKER1|10=000|");
    let document_text = format!("{bad_both}\n{raw_line}");
    let uri = client.write_workspace("logs/session.fix", document_text.as_bytes());
    let root_uri = client.root_uri();

    let initialize = client.request(
        "initialize",
        json!({
            "rootUri": root_uri,
            "capabilities": {
                "general": { "positionEncodings": ["utf-8"] },
                "workspace": { "configuration": true },
                "textDocument": {
                    "hover": { "contentFormat": ["markdown"] },
                    "inlayHint": {},
                    "semanticTokens": {
                        "requests": { "full": { "delta": true } },
                        "tokenTypes": [],
                        "tokenModifiers": [],
                        "formats": ["relative"]
                    }
                }
            }
        }),
    );
    assert_eq!(initialize["serverInfo"]["name"], "fix-lsp");
    assert_eq!(initialize["capabilities"]["positionEncoding"], "utf-8");
    assert_eq!(initialize["capabilities"]["hoverProvider"], true);
    assert!(initialize["capabilities"]["inlayHintProvider"].is_object());
    let semantic_provider = &initialize["capabilities"]["semanticTokensProvider"];
    assert_eq!(semantic_provider["full"], json!({ "delta": true }));
    assert!(
        semantic_provider.get("range").is_none(),
        "{semantic_provider}"
    );

    client.notify("initialized", json!({}));
    let configuration_request = client.wait_for_request("workspace/configuration");
    assert_eq!(
        configuration_request["params"],
        json!({ "items": [{ "section": "fix" }] })
    );

    let did_open = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": uri,
                "languageId": "fix",
                "version": 1,
                "text": document_text,
            }
        }
    });
    let mut raw_body = serde_json::to_vec(&did_open).expect("serialize didOpen");
    replace_marker_with_raw_byte(&mut raw_body, RAW_BYTE_MARKER.as_bytes(), 0x80);
    client.send_raw_body(&raw_body);

    let diagnostics = client.wait_for_notification("textDocument/publishDiagnostics", |params| {
        params["uri"] == uri
            && params["version"] == 1
            && diagnostic_codes(params).contains(&"bodyLength")
            && diagnostic_codes(params).contains(&"checkSum")
    });
    assert_eq!(diagnostics["version"], 1);

    let raw_line_number = bad_both.lines().count() as u32;
    let raw_line_bytes = raw_line.replace(RAW_BYTE_MARKER, "x");
    let msg_type_start = raw_line_bytes.find("35=0").expect("MsgType field") as u32;
    let hints = client.request(
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": raw_line_number, "character": 0 },
                "end": { "line": raw_line_number, "character": raw_line_bytes.len() }
            }
        }),
    );
    let msg_type_hint = hints
        .as_array()
        .expect("hint array")
        .iter()
        .find(|hint| {
            hint["position"]["line"] == raw_line_number
                && hint["position"]["character"] == msg_type_start + 2
                && hint["label"] == "<MsgType>"
        })
        .expect("configured MsgType hint after the raw byte");
    assert_eq!(msg_type_hint["kind"], 1);

    let hover = client.request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": raw_line_number, "character": msg_type_start }
        }),
    );
    assert!(hover["contents"]["value"]
        .as_str()
        .expect("hover markdown")
        .starts_with("**MsgType** (35)"));
    assert_eq!(hover["range"]["start"]["character"], msg_type_start);
    assert_eq!(hover["range"]["end"]["character"], msg_type_start + 4);

    let semantic_tokens = client.request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert!(semantic_tokens["data"].is_array(), "{semantic_tokens}");
    assert!(semantic_tokens["resultId"].is_string(), "{semantic_tokens}");

    let sound = include_str!("fixtures/validation/valid.fix");
    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": sound }]
        }),
    );
    let cleared = client.wait_for_notification("textDocument/publishDiagnostics", |params| {
        params["uri"] == uri && params["version"] == 2 && params["diagnostics"] == json!([])
    });
    assert_eq!(cleared["diagnostics"], json!([]));

    let status = client.shutdown_and_exit();
    assert!(
        status.success(),
        "status={status}; stderr={}",
        client.stderr()
    );
    assert!(
        !client.stderr().contains("configuration batch exceeded"),
        "{}",
        client.stderr()
    );
}

#[test]
fn unanswered_configuration_exits_nonzero_without_waiting_for_stdin_to_close() {
    let mut client = StdioClient::start("fatal-configuration", None, None);
    let initialize = client.request(
        "initialize",
        json!({ "capabilities": { "workspace": { "configuration": true } } }),
    );
    assert_eq!(initialize["serverInfo"]["name"], "fix-lsp");
    client.notify("initialized", json!({}));

    let status = client.wait_for_exit(Duration::from_secs(5));
    assert!(!status.success());
    assert!(
        client
            .stderr()
            .contains("configuration batch exceeded 500 ms"),
        "{}",
        client.stderr()
    );
}

#[test]
fn without_xdg_config_home_the_personal_file_comes_from_the_home_default() {
    // `location`'s unit tests cover the rule; only a child process covers the
    // startup glue that hands it the home directory. A malformed file names
    // itself in a startup warning, which is the observable.
    let mut client = StdioClient::start_without_config_home("home-default", Some("bad ="), None);

    client.request("initialize", json!({ "capabilities": {} }));
    client.notify("initialized", json!({}));
    let status = client.shutdown_and_exit();
    assert!(
        status.success(),
        "status={status}; stderr={}",
        client.stderr()
    );

    // The server names the file by the URI of its canonical path. Windows can
    // hand the test an 8.3 short name for the temp directory, so the path the
    // test wrote to is not always the one the server prints.
    let canonical = client
        .personal_file()
        .canonicalize()
        .expect("personal file exists");
    let expected = lsp_types::Uri::from_file_path(canonical)
        .expect("file URI")
        .to_string();
    let stderr = client.stderr();
    assert!(stderr.contains(&expected), "{stderr}");
}

#[test]
fn old_personal_config_paths_are_ignored() {
    for in_config_home in [true, false] {
        let mut client = if in_config_home {
            StdioClient::start("old-xdg-config", None, None)
        } else {
            StdioClient::start_without_config_home("old-home-config", None, None)
        };
        assert!(!client.personal_file().exists());
        let old_file = client
            .personal_file()
            .parent()
            .and_then(std::path::Path::parent)
            .expect("configuration base directory")
            .join("fix-lsp.toml");
        std::fs::write(&old_file, "hover.enabled = false\n").expect("write old configuration");

        let message = include_str!("fixtures/validation/valid.fix");
        let uri = client.write_workspace("heartbeat.fix", message.as_bytes());
        client.request("initialize", json!({ "capabilities": {} }));
        client.notify("initialized", json!({}));
        client.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "fix",
                    "version": 1,
                    "text": message
                }
            }),
        );
        client.wait_for_notification("textDocument/publishDiagnostics", |params| {
            params["uri"] == uri && params["version"] == 1
        });
        let hover = client.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": 0, "character": 0 }
            }),
        );
        assert!(hover["contents"].is_object(), "{hover}");
        assert!(client.shutdown_and_exit().success(), "{}", client.stderr());
    }
}

fn replace_marker_with_raw_byte(body: &mut Vec<u8>, marker: &[u8], byte: u8) {
    let positions: Vec<usize> = body
        .windows(marker.len())
        .enumerate()
        .filter_map(|(position, candidate)| (candidate == marker).then_some(position))
        .collect();
    assert_eq!(positions.len(), 1, "one raw-byte marker in didOpen");
    let position = positions[0];
    body.splice(position..position + marker.len(), [byte]);
}

fn diagnostic_codes(params: &Value) -> Vec<&str> {
    params["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|diagnostic| diagnostic["code"].as_str())
        .collect()
}
