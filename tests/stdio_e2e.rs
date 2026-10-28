//! End-to-end checks over the built binary's real stdio transport.

mod stdio_harness;

use std::time::Duration;

use serde_json::{json, Value};
use stdio_harness::{cli_output, StdioClient};
use venue_reader::{SERVER_NAME, SERVER_VERSION};

const RAW_BYTE_MARKER: &str = "VENUE_READER_RAW_BYTE";

#[test]
fn version_flag_prints_and_exits() {
    let output = cli_output("--version");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{SERVER_NAME} {SERVER_VERSION}\n")
    );
}

#[test]
fn help_flag_names_the_executable() {
    let output = cli_output("--help");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("Usage: venue-reader --stdio"));
}

#[test]
fn release_artifact_smoke() {
    let version = cli_output("--version");
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        format!("{SERVER_NAME} {SERVER_VERSION}\n")
    );

    let mut client = StdioClient::start("release-smoke", None, None);
    let message = "8=FIX.4.4|35=0|";
    let uri = client.write_workspace("heartbeat.fix", message.as_bytes());
    let initialize = client.request(
        "initialize",
        json!({
            "rootUri": client.root_uri(),
            "capabilities": {
                "general": { "positionEncodings": ["utf-8"] },
                "textDocument": { "hover": { "contentFormat": ["markdown"] } }
            }
        }),
    );
    assert_eq!(initialize["serverInfo"]["name"], SERVER_NAME);
    assert_eq!(initialize["serverInfo"]["version"], SERVER_VERSION);
    assert_eq!(initialize["capabilities"]["hoverProvider"], true);
    assert_eq!(
        initialize["capabilities"]["codeLensProvider"]["resolveProvider"],
        true
    );
    let commands = initialize["capabilities"]["executeCommandProvider"]["commands"]
        .as_array()
        .expect("execute commands");
    for command in [
        "fix.decode",
        "fix.useDictionary",
        "fix.describeOverrides",
        "fix.describeTag",
    ] {
        assert!(
            commands.iter().any(|value| value == command),
            "missing {command}"
        );
    }
    client.notify("initialized", json!({}));
    client.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": uri, "languageId": "fix", "version": 1, "text": message }
        }),
    );
    client.wait_for_notification("textDocument/publishDiagnostics", |params| {
        params["uri"] == uri && params["version"] == 1
    });
    let hover = client.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 0, "character": 11 } }),
    );
    assert!(hover["contents"]["value"]
        .as_str()
        .expect("hover markdown")
        .contains("**MsgType** (35)"));
    let lenses = client.request(
        "textDocument/codeLens",
        json!({ "textDocument": { "uri": uri } }),
    );
    let lenses = lenses.as_array().expect("code lenses");
    assert_eq!(lenses.len(), 1);
    let resolved = client.request("codeLens/resolve", lenses[0].clone());
    assert!(!resolved["command"]["title"]
        .as_str()
        .expect("summary title")
        .is_empty());
    let decoded = client.request(
        "workspace/executeCommand",
        json!({
            "command": "fix.decode",
            "arguments": [uri, [{
                "start": { "line": 0, "character": 10 },
                "end": { "line": 0, "character": 14 }
            }]]
        }),
    );
    assert_eq!(decoded, "35(MsgType)=0(Heartbeat)");
    assert!(client.shutdown_and_exit().success(), "{}", client.stderr());
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
    assert_eq!(initialize["serverInfo"]["name"], "venue-reader");
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
    assert!(diagnostics["diagnostics"]
        .as_array()
        .expect("diagnostic array")
        .iter()
        .all(|diagnostic| diagnostic["source"] == "fix"));

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
    assert_eq!(initialize["serverInfo"]["name"], "venue-reader");
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
fn personal_configuration_and_fix_initialization_options_reach_fix_commands() {
    for in_config_home in [true, false] {
        let personal_config = Some("inlayHints.tagNames.format = '<{}>'\n");
        let mut client = if in_config_home {
            StdioClient::start("xdg-personal-config", personal_config, None)
        } else {
            StdioClient::start_without_config_home("home-personal-config", personal_config, None)
        };
        let message = "8=FIX.4.4|35=0|";
        let uri = client.write_workspace("heartbeat.fix", message.as_bytes());
        let initialized = client.request(
            "initialize",
            json!({
                "capabilities": {},
                "initializationOptions": { "fix": {
                    "inlayHints": { "enumValues": { "format": "[{}]" } }
                } }
            }),
        );
        assert_eq!(
            initialized["capabilities"]["executeCommandProvider"]["commands"],
            json!([
                "fix.decode",
                "fix.useDictionary",
                "fix.describeOverrides",
                "fix.describeTag"
            ]),
        );
        client.notify("initialized", json!({}));
        client.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri, "languageId": "fix", "version": 1, "text": message
                }
            }),
        );
        client.wait_for_notification("textDocument/publishDiagnostics", |params| {
            params["uri"] == uri && params["version"] == 1
        });
        let decoded = client.request(
            "workspace/executeCommand",
            json!({
                "command": "fix.decode",
                "arguments": [uri, [{
                    "start": { "line": 0, "character": 10 },
                    "end": { "line": 0, "character": 14 }
                }]]
            }),
        );
        assert_eq!(decoded, "35<MsgType>=0[Heartbeat]");
        assert!(client.shutdown_and_exit().success(), "{}", client.stderr());
    }
}

#[test]
fn project_configuration_is_discovered() {
    let mut client = StdioClient::start("project-configuration", None, None);
    client.write_workspace(".venue-reader.toml", b"hover.enabled = false\n");
    let message = include_str!("fixtures/validation/valid.fix");
    let uri = client.write_workspace("heartbeat.fix", message.as_bytes());
    client.request(
        "initialize",
        json!({ "rootUri": client.root_uri(), "capabilities": {} }),
    );
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
    assert!(hover.is_null(), "{hover}");
    assert!(client.shutdown_and_exit().success(), "{}", client.stderr());
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
