//! `textDocument/codeAction`: the quickfix that repairs a line's trailer.
//!
//! Black box, through the protocol. The central test applies what the server
//! returns and sends the result back, which is the only statement that covers
//! the ranges, the version, the republish and Stage 4a-1's stale-walk fix at
//! once — a test that only inspected the JSON would pass on edits that do not
//! actually fix the line.

mod harness;

use std::time::Duration;

use harness::{TestClient, CONTENT_MODIFIED, INVALID_PARAMS};
use lsp_types::PositionEncodingKind;
use serde_json::{json, Value};

const URI: &str = "file:///a.fix";

const BAD_BOTH: &str = "8=FIX.4.4|9=7|35=D|34=2|49=CLIENT1|56=BROKER1|\
52=20251026-09:00:02.000|11=ORDER123|21=1|55=BTCUSD|54=1|38=10|40=2|44=65000|59=0|\
60=20251026-09:00:02.000|10=001|";

const SOUND: &str =
    "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";

const QUIET: Duration = Duration::from_millis(300);

const REQUEST_FAILED: i32 = -32803;

fn open(text: &str) -> TestClient {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_taking_actions());
    client.did_open(URI, text, 1);
    client
}

/// Whole first line, invoked by the user rather than polled by the editor.
fn on_line(line: u32) -> Value {
    json!({
        "textDocument": { "uri": URI },
        "range": {
            "start": { "line": line, "character": 0 },
            "end": { "line": line, "character": 400 },
        },
        "context": { "diagnostics": [], "triggerKind": 1 },
    })
}

fn actions(client: &mut TestClient, params: Value) -> Vec<Value> {
    let result = client
        .request("textDocument/codeAction", params)
        .expect("answered");
    result.as_array().cloned().unwrap_or_default()
}

fn source_params() -> Value {
    let mut params = on_line(0);
    params["context"]["only"] = json!(["source.fixAll"]);
    params
}

fn resolving_caps() -> Value {
    let mut capabilities = TestClient::caps_taking_actions();
    let action = &mut capabilities["textDocument"]["codeAction"];
    action["dataSupport"] = json!(true);
    action["disabledSupport"] = json!(true);
    action["resolveSupport"] = json!({"properties": ["edit"]});
    capabilities
}

fn open_resolving(text: &str) -> TestClient {
    let mut client = TestClient::start();
    let initialized = client.initialize_caps(resolving_caps());
    assert_eq!(
        initialized["capabilities"]["codeActionProvider"]["resolveProvider"],
        true
    );
    client.did_open(URI, text, 1);
    client
}

fn discovered_fix_all(client: &mut TestClient) -> Value {
    actions(client, on_line(0))
        .into_iter()
        .find(|action| action["kind"] == "source.fixAll")
        .expect("ordinary menu offers fix-all")
}

fn apply_document_action(text: &str, action: &Value) -> String {
    apply_document_action_encoded(text, action, PositionEncodingKind::UTF8)
}

fn apply_document_action_encoded(
    text: &str,
    action: &Value,
    encoding: PositionEncodingKind,
) -> String {
    let line_starts: Vec<_> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
        .collect();
    let byte_offset = |position: &Value| {
        let start = line_starts[position["line"].as_u64().unwrap() as usize];
        let column = position["character"].as_u64().unwrap() as usize;
        if encoding == PositionEncodingKind::UTF8 {
            return start + column;
        }
        assert_eq!(encoding, PositionEncodingKind::UTF16);
        let mut units = 0;
        for (offset, character) in text[start..].char_indices() {
            if units == column {
                return start + offset;
            }
            assert_ne!(character, '\n', "column exceeds line");
            units += character.len_utf16();
        }
        assert_eq!(units, column);
        text.len()
    };
    let mut edits: Vec<_> = action["edit"]["documentChanges"][0]["edits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edit| {
            (
                byte_offset(&edit["range"]["start"]),
                byte_offset(&edit["range"]["end"]),
                edit["newText"].as_str().unwrap(),
            )
        })
        .collect();
    edits.sort_by_key(|&(start, end, _)| (start, end));
    let mut fixed = text.to_owned();
    for (start, end, replacement) in edits.into_iter().rev() {
        fixed.replace_range(start..end, replacement);
    }
    fixed
}

#[test]
fn trailing_body_length_repairs_work_for_invoked_and_automatic_quickfixes() {
    for separator in ['|', '\u{1}'] {
        for (text, expected) in [
            ("8=FIX.4.4|9=5", "8=FIX.4.4|9=0|10=200"),
            ("8=FIX.4.4|9=", "8=FIX.4.4|9=0|10=200"),
            ("8=FIX.4.4|35=0|9=7", "8=FIX.4.4|35=0|9=0|10=158"),
            ("8=FIX.4.4|9=5|", "8=FIX.4.4|9=0|10=200|"),
        ] {
            let text = text.replace('|', &separator.to_string());
            let expected = expected.replace('|', &separator.to_string());
            let mut client = open(&text);
            for trigger in [1, 2] {
                let mut params = on_line(0);
                params["context"]["only"] = json!(["quickfix"]);
                params["context"]["triggerKind"] = json!(trigger);
                let offered = actions(&mut client, params);
                assert_eq!(offered.len(), 1, "{text:?}, trigger={trigger}");
                assert_eq!(offered[0]["title"], "Fix BodyLength and CheckSum");
                assert_eq!(apply_document_action(&text, &offered[0]), expected);
            }
            client.did_change_full(URI, &expected, 2);
            assert!(actions(&mut client, on_line(0)).is_empty());
            client.shutdown();
        }
    }
}

#[test]
fn trailing_body_length_does_not_abort_the_document_repair() {
    for tail in ["8=FIX.4.4|9=5", "8=FIX.4.4|9=", "8=FIX.4.4|35=0|9=7"] {
        let text = format!("{BAD_BOTH}\n{SOUND}\n{tail}\r\n");
        let mut client = open(&text);
        let offered = actions(&mut client, source_params());
        assert_eq!(offered.len(), 1);
        let edits = offered[0]["edit"]["documentChanges"][0]["edits"]
            .as_array()
            .unwrap();
        assert_eq!(edits.len(), 1, "one replacement spans all repaired lines");
        let fixed = apply_document_action(&text, &offered[0]);
        assert_eq!(fixed.lines().nth(1), Some(SOUND));
        assert!(fixed.ends_with("\r\n"));
        client.did_change_full(URI, &fixed, 2);
        assert!(actions(&mut client, source_params()).is_empty());
        client.shutdown();
    }
}

#[test]
fn checksum_before_begin_string_preserves_the_line_and_its_diagnostics() {
    for separator in ['|', '\u{1}'] {
        for misplaced in [
            "10=167|8=FIX.4.4|35=0|",
            "10=123|8=FIX.4.4|35=0|",
            "55=X|10=123|8=FIX.4.4|9=5|35=0|10=163|",
        ] {
            let misplaced = misplaced.replace('|', &separator.to_string());
            let text = format!("{misplaced}\n{BAD_BOTH}\n{SOUND}");
            let mut client = open(&text);
            let published = client.wait_for("textDocument/publishDiagnostics");
            let original: Vec<_> = published["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|diag| diag["range"]["start"]["line"] == 0)
                .cloned()
                .collect();
            assert!(!original.is_empty());
            for trigger in [1, 2] {
                let mut params = on_line(0);
                params["context"]["only"] = json!(["quickfix"]);
                params["context"]["triggerKind"] = json!(trigger);
                assert!(actions(&mut client, params).is_empty());
            }
            let offered = actions(&mut client, source_params());
            assert_eq!(offered.len(), 1);
            let edits = offered[0]["edit"]["documentChanges"][0]["edits"]
                .as_array()
                .unwrap();
            assert!(!edits.is_empty());
            assert!(edits.iter().all(|edit| edit["range"]["start"]["line"] == 1));
            let fixed = apply_document_action(&text, &offered[0]);
            assert_eq!(fixed.lines().next(), Some(misplaced.as_str()));
            assert_eq!(fixed.lines().nth(2), Some(SOUND));
            client.did_change_full(URI, &fixed, 2);
            assert!(actions(&mut client, source_params()).is_empty());
            loop {
                let published = client.wait_for("textDocument/publishDiagnostics");
                if published["version"] == 2 {
                    assert_eq!(published["diagnostics"], json!(original));
                    break;
                }
            }
            client.shutdown();
        }
    }
}

#[test]
fn fix_all_repairs_the_document_beyond_the_range_and_publication_limits() {
    let text = format!("{BAD_BOTH}\n{SOUND}\n{BAD_BOTH}\n# untouched\n{BAD_BOTH}\n");
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": TestClient::caps_taking_actions(),
        "initializationOptions": { "fix": {
            "limits": { "rangeLines": 1, "diagnosticsPerDocument": 1 },
        }},
    }));
    client.did_open(URI, &text, 1);
    let mut params = source_params();
    params["range"]["end"]["line"] = json!(999);
    params["context"]["diagnostics"] = json!([]);
    let offered = actions(&mut client, params);
    assert_eq!(offered.len(), 1, "{offered:#?}");
    let action = &offered[0];
    assert_eq!(action["kind"], "source.fixAll");
    assert_eq!(action["title"], "Fix all");
    assert!(action.get("diagnostics").is_none());
    assert_eq!(
        action["edit"]["documentChanges"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        action["edit"]["documentChanges"][0]["textDocument"],
        json!({"uri": URI, "version": 1})
    );
    assert_eq!(
        action["edit"]["documentChanges"][0]["edits"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let fixed = apply_document_action(&text, action);
    assert_eq!(fixed.lines().nth(1), Some(SOUND));
    assert_eq!(fixed.lines().nth(3), Some("# untouched"));
    assert!(fixed.ends_with('\n'));
    client.did_change_full(URI, &fixed, 2);
    assert!(
        actions(&mut client, source_params()).is_empty(),
        "idempotent"
    );
    loop {
        let published = client.wait_for("textDocument/publishDiagnostics");
        if published["version"] == 2 {
            assert_eq!(published["diagnostics"], json!([]));
            break;
        }
    }
    client.shutdown();
}

#[test]
fn action_kind_filters_are_hierarchical_and_automatic_requests_never_scan_all() {
    let mut client = open(&format!("{BAD_BOTH}\n{BAD_BOTH}"));
    for (only, trigger, expected) in [
        (json!(["source.fixAll"]), json!(1), vec!["source.fixAll"]),
        (json!(["source"]), json!(null), vec!["source.fixAll"]),
        (json!([""]), json!(1), vec!["quickfix", "source.fixAll"]),
        (
            json!(["source.fixAll", "quickfix"]),
            json!(1),
            vec!["quickfix", "source.fixAll"],
        ),
        (json!(["source.fixAll.child"]), json!(1), vec![]),
        (json!(["quickfix.child"]), json!(1), vec![]),
        (json!(["source.fixAllOther"]), json!(1), vec![]),
        (json!([]), json!(1), vec![]),
        (json!(null), json!(1), vec!["quickfix"]),
        (json!(["source"]), json!(2), vec![]),
        (json!([""]), json!(2), vec!["quickfix"]),
        (
            json!(["quickfix", "source.fixAll"]),
            json!(2),
            vec!["quickfix"],
        ),
    ] {
        let mut params = on_line(0);
        params["context"]["only"] = only.clone();
        params["context"]["triggerKind"] = trigger.clone();
        let offered = actions(&mut client, params);
        let kinds: Vec<_> = offered
            .iter()
            .map(|action| action["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, expected, "only={only}, trigger={trigger}");
        if let Some(aggregate) = offered
            .iter()
            .find(|action| action["kind"] == "source.fixAll")
        {
            assert_eq!(
                aggregate["edit"]["documentChanges"][0]["edits"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }
    }
    client.shutdown();
}

#[test]
fn a_mixed_request_retains_the_range_limit() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": TestClient::caps_taking_actions(),
        "initializationOptions": {"fix": {"limits": {"rangeLines": 1}}},
    }));
    client.did_open(URI, &format!("{BAD_BOTH}\n{BAD_BOTH}"), 1);
    let mut params = source_params();
    params["context"]["only"] = json!(["quickfix", "source.fixAll"]);
    params["range"]["end"]["line"] = json!(1);
    assert_eq!(
        client.request("textDocument/codeAction", params),
        Err(REQUEST_FAILED)
    );
    client.shutdown();
}

#[test]
fn fix_all_respects_global_diagnostic_disablement() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": TestClient::caps_taking_actions(),
        "initializationOptions": {"fix": {"diagnostics": {"enabled": false}}},
    }));
    client.did_open(URI, BAD_BOTH, 1);
    assert!(actions(&mut client, source_params()).is_empty());
    client.shutdown();
}

fn calculated_fix_all(client: &mut TestClient, resolve: bool) -> Value {
    if resolve {
        let action = discovered_fix_all(client);
        client.request("codeAction/resolve", action).unwrap()
    } else {
        let offered = actions(client, source_params());
        assert_eq!(offered.len(), 1);
        offered.into_iter().next().unwrap()
    }
}

fn assert_no_remaining_repairs(client: &mut TestClient, resolve: bool) {
    if resolve {
        let action = calculated_fix_all(client, true);
        assert_eq!(action["edit"]["documentChanges"][0]["edits"], json!([]));
    } else {
        assert!(actions(client, source_params()).is_empty());
    }
}

#[test]
fn aggregate_edits_use_the_negotiated_encoding_and_preserve_framing() {
    let broken = "8=FIX.4.4|9=1|35=0|10=000|";
    let repaired = "8=FIX.4.4|9=5|35=0|10=163|";
    for resolve in [false, true] {
        for encoding in [PositionEncodingKind::UTF8, PositionEncodingKind::UTF16] {
            for ending in ["", "\n", "\r\n", "\r"] {
                let mut client = TestClient::start();
                let mut caps = if resolve {
                    resolving_caps()
                } else {
                    TestClient::caps_taking_actions()
                };
                caps["general"]["positionEncodings"] = json!([encoding.as_str()]);
                client.initialize_caps(caps);
                let first_prefix = "🙂 — ";
                let last_prefix = "最後🙂 ";
                let last_broken = broken.replace('|', "\u{1}");
                let text = format!(
                    "prelude 🙂\n{first_prefix}{broken} suffix\r\n{SOUND}\n\"untouched\" \\ 🙂\r\n8=FIX.4.4|\n{last_prefix}{last_broken} 尾🙂{ending}"
                );
                client.did_open(URI, &text, 1);
                let action = calculated_fix_all(&mut client, resolve);
                let edits = action["edit"]["documentChanges"][0]["edits"]
                    .as_array()
                    .unwrap();
                assert_eq!(edits.len(), 1);
                let column = |text: &str| {
                    if encoding == PositionEncodingKind::UTF8 {
                        text.len()
                    } else {
                        text.encode_utf16().count()
                    }
                };
                assert_eq!(
                    edits[0]["range"],
                    json!({
                        "start": {"line": 1, "character": column(&format!("{first_prefix}8=FIX.4.4|9="))},
                        "end": {"line": 5, "character": column(&format!("{last_prefix}{}", broken.trim_end_matches('|')))},
                    })
                );
                let fixed = apply_document_action_encoded(&text, &action, encoding.clone());
                assert_eq!(
                    fixed,
                    text.replace(broken, repaired)
                        .replace(&last_broken, &repaired.replace('|', "\u{1}")),
                    "resolve={resolve}, encoding={encoding:?}, ending={ending:?}"
                );
                client.did_change_full(URI, &fixed, 2);
                assert_no_remaining_repairs(&mut client, resolve);
                client.shutdown();
            }
        }
    }
}

#[test]
fn single_line_fix_all_encloses_only_changes_while_quickfix_stays_granular() {
    let text = "prefix 8=FIX.4.4|9=1|35=0|10=000| suffix";
    for resolve in [false, true] {
        let mut client = if resolve {
            open_resolving(text)
        } else {
            open(text)
        };
        let mut params = on_line(0);
        params["context"]["only"] = json!(["quickfix"]);
        let quickfix = actions(&mut client, params);
        let individual = quickfix[0]["edit"]["documentChanges"][0]["edits"]
            .as_array()
            .unwrap();
        assert_eq!(individual.len(), 2);
        let action = calculated_fix_all(&mut client, resolve);
        let edits = action["edit"]["documentChanges"][0]["edits"]
            .as_array()
            .unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0]["range"]["start"], individual[0]["range"]["start"]);
        assert_eq!(edits[0]["range"]["end"], individual[1]["range"]["end"]);
        assert_eq!(
            apply_document_action(text, &action),
            "prefix 8=FIX.4.4|9=5|35=0|10=163| suffix"
        );
        assert_eq!(
            apply_document_action(text, &action),
            apply_document_action(text, &quickfix[0])
        );
        client.shutdown();
    }
}

#[test]
fn fix_all_preserves_line_endings_after_incremental_changes() {
    let broken = "8=FIX.4.4|9=1|35=0|10=000|";
    let repaired = "8=FIX.4.4|9=5|35=0|10=163|";
    for resolve in [false, true] {
        let original = format!("{broken}\r\nuntouched\n{broken}\r");
        let mut client = if resolve {
            open_resolving(&original)
        } else {
            open(&original)
        };
        client.notify("textDocument/didChange", json!({
            "textDocument": {"uri": URI, "version": 2},
            "contentChanges": [
                {"range": {"start": {"line": 0, "character": broken.len()}, "end": {"line": 1, "character": 0}}, "text": "\n"},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 9}}, "text": "preserved\r\nextra"},
                {"range": {"start": {"line": 2, "character": 5}, "end": {"line": 3, "character": 0}}, "text": "\r\n"},
            ],
        }));
        let changed = format!("{broken}\npreserved\r\nextra\r\n{broken}\r");
        let action = calculated_fix_all(&mut client, resolve);
        assert_eq!(
            action["edit"]["documentChanges"][0]["textDocument"]["version"],
            2
        );
        let fixed = apply_document_action(&changed, &action);
        assert_eq!(fixed, changed.replace(broken, repaired));
        let edit = &action["edit"]["documentChanges"][0]["edits"][0];
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": URI, "version": 3},
                "contentChanges": [{"range": edit["range"], "text": edit["newText"]}],
            }),
        );
        assert_no_remaining_repairs(&mut client, resolve);
        client.shutdown();
    }
}

#[test]
fn unicode_integrity_repairs_work_through_quickfix_and_fix_all() {
    let body = "35=0|58=café 🙂|";
    let broken = format!("8=FIX.4.4|9=1|{body}10=000|");
    let message = format!("8=FIX.4.4|9={}|{body}", body.len());
    let checksum: u32 = message
        .bytes()
        .map(|byte| u32::from(if byte == b'|' { 1 } else { byte }))
        .sum();
    let expected = format!("{message}10={:03}|", checksum % 256);
    for resolve in [false, true] {
        let mut client = if resolve {
            open_resolving(&broken)
        } else {
            open(&broken)
        };
        for trigger in [1, 2] {
            let mut params = on_line(0);
            params["context"]["only"] = json!(["quickfix"]);
            params["context"]["triggerKind"] = json!(trigger);
            let offered = actions(&mut client, params);
            assert_eq!(offered.len(), 1);
            assert_eq!(apply_document_action(&broken, &offered[0]), expected);
        }
        let aggregate = calculated_fix_all(&mut client, resolve);
        assert_eq!(
            aggregate["edit"]["documentChanges"][0]["edits"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(apply_document_action(&broken, &aggregate), expected);
        client.did_change_full(URI, &expected, 2);
        assert_no_remaining_repairs(&mut client, resolve);
        client.shutdown();
    }
}

#[test]
fn non_utf8_repairable_lines_refuse_quickfix_and_fix_all_without_partial_edits() {
    for invalid in [
        b"8=FIX.4.4|9=1|35=0|58=caf\xe9|10=000|".as_slice(),
        b"8=FIX.4.4|9=1|35=0|58=\xe9\xf0\x9f\x99\x82\x80|10=000|",
        b"8=FIX.4.4|9=1|35=0|58=\xf0\x9f\x99\x82|10=\xff|",
    ] {
        let bytes = [BAD_BOTH.as_bytes(), b"\n", invalid].concat();
        let text = venue_reader::transport::sanitize_incoming_body(bytes);
        for resolve in [false, true] {
            let mut client = if resolve {
                open_resolving(&text)
            } else {
                open(&text)
            };
            let mut params = on_line(0);
            params["range"]["end"]["line"] = json!(1);
            params["context"]["only"] = json!(["quickfix"]);
            assert_eq!(
                client.request("textDocument/codeAction", params.clone()),
                Err(REQUEST_FAILED)
            );
            params["context"]["triggerKind"] = json!(2);
            let (result, notifications) =
                client.request_collecting("textDocument/codeAction", params);
            assert_eq!(result, Ok(json!([])));
            assert!(notifications.iter().any(|(method, params)| {
                method == "window/logMessage"
                    && params["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("Cannot repair a non-UTF-8 line"))
            }));
            let result = if resolve {
                let offered = actions(&mut client, source_params());
                assert_eq!(offered.len(), 1);
                client.request("codeAction/resolve", offered[0].clone())
            } else {
                client.request("textDocument/codeAction", source_params())
            };
            assert_eq!(result, Err(REQUEST_FAILED));
            let mut valid = on_line(0);
            valid["context"]["only"] = json!(["quickfix"]);
            assert_eq!(actions(&mut client, valid).len(), 1);
            client.shutdown();
        }
    }
}

#[test]
fn non_utf8_ineligible_fragments_do_not_block_repairs_outside_them() {
    let broken = "8=FIX.4.4|9=1|35=0|10=000|";
    let repaired = "8=FIX.4.4|9=5|35=0|10=163|";
    for bytes in [
        b"35=0|58=caf\xe9|".as_slice(),
        b"10=123|8=FIX.4.4|35=0|58=caf\xe9|",
    ] {
        let fragment = venue_reader::transport::sanitize_incoming_body(bytes.to_vec());
        let text = format!("{fragment}\r\n{broken}\n{fragment}");
        for resolve in [false, true] {
            let mut client = if resolve {
                open_resolving(&text)
            } else {
                open(&text)
            };
            for trigger in [1, 2] {
                for line in [0, 2] {
                    let mut params = on_line(line);
                    params["context"]["only"] = json!(["quickfix"]);
                    params["context"]["triggerKind"] = json!(trigger);
                    let (result, notifications) =
                        client.request_collecting("textDocument/codeAction", params);
                    assert_eq!(result, Ok(json!([])));
                    assert!(!notifications.iter().any(|(method, params)| {
                        method == "window/logMessage" && params["type"] == 1
                    }));
                }
                let mut params = on_line(0);
                params["range"]["end"]["line"] = json!(2);
                params["context"]["triggerKind"] = json!(trigger);
                let offered = actions(&mut client, params);
                let kinds: Vec<_> = offered
                    .iter()
                    .map(|action| action["kind"].as_str().unwrap())
                    .collect();
                assert_eq!(
                    kinds,
                    if resolve {
                        vec!["quickfix", "source.fixAll"]
                    } else {
                        vec!["quickfix"]
                    }
                );
            }
            let action = calculated_fix_all(&mut client, resolve);
            let edits = action["edit"]["documentChanges"][0]["edits"]
                .as_array()
                .unwrap();
            assert_eq!(edits.len(), 1);
            assert_eq!(edits[0]["range"]["start"]["line"], 1);
            assert_eq!(edits[0]["range"]["end"]["line"], 1);
            let fixed = apply_document_action(&text, &action);
            assert_eq!(fixed, format!("{fragment}\r\n{repaired}\n{fragment}"));
            client.did_change_full(URI, &fixed, 2);
            assert_no_remaining_repairs(&mut client, resolve);
            client.did_change_full(URI, &fragment, 3);
            assert_no_remaining_repairs(&mut client, resolve);
            client.shutdown();
        }
    }
}

#[test]
fn non_utf8_gap_refuses_the_whole_fix_all_instead_of_splitting_edits() {
    let broken = "8=FIX.4.4|9=1|35=0|10=000|";
    let bytes = [
        broken.as_bytes(),
        b"\r\n",
        SOUND.as_bytes(),
        b"\nuntouched \xe9\r\n",
        broken.as_bytes(),
    ]
    .concat();
    let text = venue_reader::transport::sanitize_incoming_body(bytes);
    for resolve in [false, true] {
        let mut client = if resolve {
            open_resolving(&text)
        } else {
            open(&text)
        };
        let result = if resolve {
            let offered = actions(&mut client, source_params());
            assert_eq!(offered.len(), 1);
            client.request("codeAction/resolve", offered[0].clone())
        } else {
            client.request("textDocument/codeAction", source_params())
        };
        assert_eq!(result, Err(REQUEST_FAILED));
        for line in [0, 3] {
            let mut params = on_line(line);
            params["context"]["only"] = json!(["quickfix"]);
            assert_eq!(actions(&mut client, params).len(), 1);
        }
        client.shutdown();
    }
}

#[test]
fn a_sound_line_offers_nothing() {
    let mut client = open(SOUND);
    assert_eq!(actions(&mut client, on_line(0)), Vec::<Value>::new());
    client.shutdown();
}

#[test]
fn missing_begin_string_and_a_lone_begin_string_offer_no_repair() {
    let lines = [
        "55=X|",
        "9=wrong|35=0|10=wrong|",
        "8=FIX.4.4|",
        "# fix: dictionary=FIX.4.4",
        "35=0|9=wrong|10=wrong|",
    ];
    let mut client = open(&lines.join("\n"));
    for line in [0, 1, 2, 4] {
        assert_eq!(
            actions(&mut client, on_line(line)),
            Vec::<Value>::new(),
            "{}",
            lines[line as usize]
        );
    }
    client.shutdown();
}

#[test]
fn fix_all_skips_ineligible_fragments_and_preserves_them_after_application() {
    let fragments = ["55=X|", "8=FIX.4.4|", "35=0|8=FIX.4.4|"];
    for separator in ['|', '\u{1}'] {
        let untouched = fragments.join("\r\n").replace('|', &separator.to_string());
        let broken = BAD_BOTH.replace('|', &separator.to_string());
        let text = format!("{untouched}\r\n{broken}\r\n");
        let mut client = open(&text);
        let offered = actions(&mut client, source_params());
        assert_eq!(offered.len(), 1);
        let edits = offered[0]["edit"]["documentChanges"][0]["edits"]
            .as_array()
            .unwrap();
        assert_eq!(edits.len(), 1);
        assert!(edits.iter().all(|edit| edit["range"]["start"]["line"] == 3));
        let fixed = apply_document_action(&text, &offered[0]);
        assert!(fixed.starts_with(&format!("{untouched}\r\n")));
        assert_ne!(fixed, text);
        assert!(fixed.ends_with("\r\n"));
        client.did_change_full(URI, &fixed, 2);
        assert!(actions(&mut client, source_params()).is_empty());
        client.shutdown();
    }
}

#[test]
fn a_line_wrong_in_two_ways_offers_one_action() {
    let mut client = open(BAD_BOTH);
    let offered = actions(&mut client, on_line(0));
    assert_eq!(offered.len(), 1, "{offered:#?}");
    let action = &offered[0];
    assert_eq!(action["title"], "Fix BodyLength and CheckSum");
    assert_eq!(action["kind"], "quickfix");
    assert_eq!(action["isPreferred"], true);
    let changes = &action["edit"]["documentChanges"];
    assert_eq!(changes[0]["textDocument"]["uri"], URI);
    assert_eq!(changes[0]["textDocument"]["version"], 1);
    assert_eq!(changes[0]["edits"].as_array().map(Vec::len), Some(2));
    client.shutdown();
}

/// The action claims the diagnostics the server published, byte for byte. A
/// client matches them by range and message to decide which marks the fix
/// clears; anything re-rendered differently here would silently stop matching.
#[test]
fn the_action_claims_exactly_the_published_integrity_diagnostics() {
    let mut client = open(BAD_BOTH);
    let published = client.wait_for("textDocument/publishDiagnostics");
    let mut expected: Vec<Value> = published["diagnostics"]
        .as_array()
        .expect("a set")
        .iter()
        .filter(|diag| diag["data"]["rule"] == "integrity")
        .cloned()
        .collect();
    assert_eq!(expected.len(), 2, "{expected:#?}");

    let offered = actions(&mut client, on_line(0));
    let mut claimed = offered[0]["diagnostics"].as_array().cloned().unwrap();
    let key = |diag: &Value| diag["code"].as_str().unwrap_or_default().to_owned();
    expected.sort_by_key(key);
    claimed.sort_by_key(key);
    assert_eq!(claimed, expected);
    client.shutdown();
}

/// The whole point, and the only test that proves the edits are correct rather
/// than merely well-shaped: apply them, hand the result back, and require the
/// publish *at the new version* to be empty. Waiting for "the next publish"
/// would accept the stale one Stage 4a-1's window used to allow.
#[test]
fn applying_what_it_returns_leaves_a_line_with_nothing_to_report() {
    let mut client = open(BAD_BOTH);
    let offered = actions(&mut client, on_line(0));
    let fixed = apply_document_action(BAD_BOTH, &offered[0]);
    assert_ne!(fixed, BAD_BOTH);

    client.did_change_full(URI, &fixed, 2);
    loop {
        let published = client.wait_for("textDocument/publishDiagnostics");
        if published["version"] == 2 {
            assert_eq!(
                published["diagnostics"].as_array().map(Vec::len),
                Some(0),
                "the repaired line: {fixed}"
            );
            break;
        }
    }
    client.shutdown();
}

#[test]
fn an_automatic_trigger_without_only_includes_applicable_range_actions() {
    let mut client = open(BAD_BOTH);
    let mut params = on_line(0);
    params["context"]["triggerKind"] = json!(2);
    let unfiltered = actions(&mut client, params.clone());
    assert_eq!(unfiltered.len(), 1);
    assert_eq!(unfiltered[0]["kind"], "quickfix");
    params["context"]["only"] = json!(["quickfix"]);
    assert_eq!(actions(&mut client, params), unfiltered);
    client.shutdown();
}

/// Refused rather than truncated: a partial repair of a selection is less
/// predictable than being told the selection is too big. `[]` would be
/// indistinguishable from "nothing here is repairable".
#[test]
fn an_oversized_range_is_refused_and_says_so() {
    let long = vec![BAD_BOTH; 3000].join("\n");
    let mut client = open(&long);
    let params = json!({
        "textDocument": { "uri": URI },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 2999, "character": 400 },
        },
        "context": { "diagnostics": [], "triggerKind": 1 },
    });
    let error = client
        .request("textDocument/codeAction", params)
        .expect_err("refused");
    assert_eq!(error, REQUEST_FAILED);
    client.shutdown();
}

/// A poll has no user to tell, so it gets the empty answer rather than an
/// error the editor would surface as a server failure.
#[test]
fn an_oversized_automatic_request_is_answered_with_nothing() {
    let long = vec![BAD_BOTH; 3000].join("\n");
    let mut client = open(&long);
    let params = json!({
        "textDocument": { "uri": URI },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 2999, "character": 400 },
        },
        "context": { "diagnostics": [], "triggerKind": 2, "only": ["quickfix"] },
    });
    assert_eq!(actions(&mut client, params), Vec::<Value>::new());
    client.shutdown();
}

/// Clients send an end past the document, and a range clamped before it is
/// normalised loses the last line — which is the one the cursor is on.
#[test]
fn an_end_past_the_document_still_covers_the_last_line() {
    // The broken line is the last one, and the end is both past the document
    // and at column 0 — the shape that makes the order matter. Clamping first
    // turns the end into the last line's own number, where "ends at column 0"
    // then excludes it.
    let mut client = open(&format!("{SOUND}\n{BAD_BOTH}"));
    let params = json!({
        "textDocument": { "uri": URI },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 9, "character": 0 },
        },
        "context": { "diagnostics": [], "triggerKind": 1 },
    });
    assert_eq!(actions(&mut client, params).len(), 1);
    client.shutdown();
}

/// One action per line, not one per document: two broken lines in the range
/// are two separate repairs a user can apply independently.
#[test]
fn every_line_in_the_range_gets_its_own_action() {
    let mut client = open(&format!("{BAD_BOTH}\n{SOUND}\n{BAD_BOTH}"));
    let params = json!({
        "textDocument": { "uri": URI },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 2, "character": 400 },
        },
        "context": { "diagnostics": [], "triggerKind": 1 },
    });
    let offered = actions(&mut client, params);
    assert_eq!(offered.len(), 2, "{offered:#?}");
    let lines: Vec<u64> = offered
        .iter()
        .map(|action| {
            action["edit"]["documentChanges"][0]["edits"][0]["range"]["start"]["line"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(lines, vec![0, 2]);
    client.shutdown();
}

/// The edit carries the version its answer was computed against, so a client
/// holding an action across an edit applies it to text that has moved — or,
/// having said `documentChanges`, refuses to.
#[test]
fn an_action_carries_the_version_it_was_computed_against() {
    let mut client = open(BAD_BOTH);
    let held = actions(&mut client, on_line(0));
    assert_eq!(
        held[0]["edit"]["documentChanges"][0]["textDocument"]["version"],
        1
    );

    client.did_change_full(URI, BAD_BOTH, 2);
    let fresh = actions(&mut client, on_line(0));
    assert_eq!(
        fresh[0]["edit"]["documentChanges"][0]["textDocument"]["version"],
        2
    );
    assert_eq!(
        held[0]["edit"]["documentChanges"][0]["textDocument"]["version"], 1,
        "the one already handed out does not change under the client"
    );
    client.shutdown();
}

/// Positions leave the crate under the negotiated encoding. A log prefix with
/// a character outside the BMP-free ASCII range moves every column after it.
#[test]
fn a_span_after_non_ascii_text_is_converted_under_the_negotiated_encoding() {
    let prefixed = format!("— {BAD_BOTH}");
    let mut client = TestClient::start();
    let mut caps = TestClient::caps_taking_actions();
    caps["general"]["positionEncodings"] = json!(["utf-16"]);
    client.initialize_caps(caps);
    client.did_open(URI, &prefixed, 1);

    let offered = actions(&mut client, on_line(0));
    let first = &offered[0]["edit"]["documentChanges"][0]["edits"][0];
    // "— " is three bytes and two utf-16 units, so the BodyLength value that
    // starts at byte 16 is at character 14.
    assert_eq!(prefixed.find("9=7").unwrap() + 2, 16);
    assert_eq!(first["range"]["start"]["character"], 14);
    client.shutdown();
}

/// One switch governs the report and the repair. There is no configuration
/// under which an action can claim a problem the user asked not to hear about.
#[test]
fn disabling_the_rule_removes_the_action_too() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": TestClient::caps_taking_actions(),
        "initializationOptions": { "fix": {
            "diagnostics": { "rules": { "integrity": false } },
        }},
    }));
    client.did_open(URI, BAD_BOTH, 1);

    assert_eq!(actions(&mut client, on_line(0)), Vec::<Value>::new());
    assert!(actions(&mut client, source_params()).is_empty());
    // And the same switch silenced the report: the two cannot come apart.
    let published = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(published["diagnostics"].as_array().map(Vec::len), Some(0));
    client.shutdown();
}

/// A client that cannot take a versioned literal is offered no provider, and
/// so never sends the request. Asserted on what is advertised, since that is
/// what stops it.
#[test]
fn a_client_that_cannot_take_the_answer_is_not_offered_the_provider() {
    let mut client = TestClient::start();
    let advertised = client.initialize_caps(json!({
        "general": { "positionEncodings": ["utf-8"] },
        "textDocument": { "codeAction": {
            "codeActionLiteralSupport": { "codeActionKind": { "valueSet": ["quickfix"] } },
        }},
    }));
    assert_eq!(
        advertised.pointer("/capabilities/codeActionProvider"),
        None,
        "no versioned edits, so no provider"
    );
    client.shutdown();
}

#[test]
fn unsupported_clients_receive_no_edits_even_if_they_request_actions() {
    for capabilities in [
        json!({}),
        json!({"workspace": {"workspaceEdit": {"documentChanges": true}}}),
        json!({
            "textDocument": {"codeAction": {"codeActionLiteralSupport": {
                "codeActionKind": {"valueSet": ["quickfix", "source.fixAll"]},
            }}},
            "workspace": {"workspaceEdit": {"documentChanges": false}},
        }),
    ] {
        let mut client = TestClient::start();
        client.initialize_caps(capabilities);
        client.did_open(URI, BAD_BOTH, 1);
        assert!(actions(&mut client, on_line(0)).is_empty());
        assert!(actions(&mut client, source_params()).is_empty());
        client.shutdown();
    }
}

#[test]
fn resolve_without_data_support_retains_eager_explicit_fix_all() {
    let mut client = TestClient::start();
    let mut capabilities = TestClient::caps_taking_actions();
    capabilities["textDocument"]["codeAction"]["resolveSupport"] = json!({"properties": ["edit"]});
    let advertised = client.initialize_caps(capabilities);
    assert!(advertised
        .pointer("/capabilities/codeActionProvider/resolveProvider")
        .is_none());
    client.did_open(URI, BAD_BOTH, 1);
    let offered = actions(&mut client, source_params());
    assert_eq!(offered.len(), 1);
    assert!(offered[0]["edit"]["documentChanges"].is_array());
    assert!(offered[0].get("command").is_none());
    assert!(offered[0].get("data").is_none());
    client.shutdown();
}

/// A request whose params do not parse is the client's error, not an empty
/// answer: `[]` would say "nothing here is repairable", which is a claim about
/// the document rather than about the request.
#[test]
fn malformed_params_are_an_error() {
    let mut client = open(BAD_BOTH);
    let error = client
        .request(
            "textDocument/codeAction",
            json!({ "textDocument": { "uri": URI } }),
        )
        .expect_err("refused");
    assert_eq!(error, harness::INVALID_PARAMS);
    client.shutdown();
}

/// A closed document is gone, not malformed.
#[test]
fn a_closed_document_is_answered_with_nothing() {
    let mut client = open(BAD_BOTH);
    client.did_close(URI);
    let _ = client.drain_notifications(QUIET);
    assert_eq!(actions(&mut client, on_line(0)), Vec::<Value>::new());
    client.shutdown();
}

/// The cursor, which is an empty range at column zero. A code action is about
/// the line the cursor sits on, so `(n,0)..(n,0)` **includes** line `n` — where
/// an inlay hint, being a point under an exclusive end, excludes it.
///
/// The two normalisations differ on purpose and each lives with the handler
/// that owns it. This test is what fails if either is ever hoisted into the
/// shared `LineRange`, which would look like tidying and would silently stop
/// answering the commonest invocation there is: a cursor on a broken line.
#[test]
fn a_bare_cursor_still_offers_the_line_it_sits_on() {
    let mut client = open(&format!("{SOUND}\n{BAD_BOTH}"));
    let cursor = json!({
        "textDocument": { "uri": URI },
        "range": {
            "start": { "line": 1, "character": 0 },
            "end": { "line": 1, "character": 0 },
        },
        "context": { "diagnostics": [], "triggerKind": 1 },
    });
    assert_eq!(actions(&mut client, cursor).len(), 1, "the broken line");

    // And the control: the same empty range on the sound line offers nothing,
    // so the test above is not passing because every cursor answers.
    let elsewhere = json!({
        "textDocument": { "uri": URI },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 0, "character": 0 },
        },
        "context": { "diagnostics": [], "triggerKind": 1 },
    });
    assert!(actions(&mut client, elsewhere).is_empty());
    client.shutdown();
}

#[test]
fn ordinary_menu_defers_fix_all_until_resolution_and_preserves_action_properties() {
    let text = format!("{BAD_BOTH}\n{SOUND}\n{BAD_BOTH}\r\n");
    let mut client = open_resolving(&text);
    let offered = actions(&mut client, on_line(0));
    assert_eq!(offered.len(), 2);
    assert_eq!(offered[0]["kind"], "quickfix");
    let action = &offered[1];
    assert_eq!(action["kind"], "source.fixAll");
    assert_eq!(action["title"], "Fix all");
    assert!(action.get("edit").is_none());
    assert!(action.get("command").is_none());
    assert!(action.get("disabled").is_none());
    assert_eq!(action["data"]["uri"], URI);
    assert_eq!(action["data"]["version"], 1);
    let resolved = client
        .request("codeAction/resolve", action.clone())
        .unwrap();
    for (key, value) in action.as_object().unwrap() {
        assert_eq!(&resolved[key], value, "resolve changed {key}");
    }
    assert_eq!(
        resolved["edit"]["documentChanges"][0]["textDocument"],
        json!({"uri": URI, "version": 1})
    );
    let fixed = apply_document_action(&text, &resolved);
    assert_ne!(fixed, text);
    assert_eq!(fixed.lines().nth(1), Some(SOUND));
    assert!(fixed.ends_with("\r\n"));
    client.did_change_full(URI, &fixed, 2);
    let fresh = discovered_fix_all(&mut client);
    let resolved = client.request("codeAction/resolve", fresh).unwrap();
    assert!(resolved["edit"]["documentChanges"][0]["edits"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(apply_document_action(&fixed, &resolved), fixed);
    client.shutdown();
}

#[test]
fn lazy_discovery_filters_kinds_for_both_triggers_without_scanning_fragments() {
    let mut client = open_resolving(BAD_BOTH);
    for trigger in [1, 2] {
        for (only, expected) in [
            (Value::Null, vec!["quickfix", "source.fixAll"]),
            (json!([""]), vec!["quickfix", "source.fixAll"]),
            (json!(["quickfix"]), vec!["quickfix"]),
            (json!(["source"]), vec!["source.fixAll"]),
            (json!(["source.fixAll"]), vec!["source.fixAll"]),
            (json!(["refactor"]), vec![]),
            (json!(["source.fixAll.child"]), vec![]),
            (json!([]), vec![]),
        ] {
            let mut params = on_line(0);
            params["context"]["only"] = only.clone();
            params["context"]["triggerKind"] = json!(trigger);
            let offered = actions(&mut client, params);
            let kinds: Vec<_> = offered
                .iter()
                .map(|action| action["kind"].as_str().unwrap())
                .collect();
            assert_eq!(kinds, expected, "only={only}, trigger={trigger}");
            for action in offered
                .iter()
                .filter(|action| action["kind"] == "source.fixAll")
            {
                assert!(action.get("edit").is_none());
                assert!(action["data"].is_object());
            }
        }
    }
    client.did_change_full(URI, "10=167|8=FIX.4.4|35=0|\n8=FIX.4.4|", 2);
    let action = discovered_fix_all(&mut client);
    assert!(action.get("edit").is_none());
    let resolved = client.request("codeAction/resolve", action).unwrap();
    assert!(resolved["edit"]["documentChanges"][0]["edits"]
        .as_array()
        .unwrap()
        .is_empty());
    client.shutdown();
}

#[test]
fn unresolved_actions_reject_changed_closed_and_reopened_documents() {
    let mut client = open_resolving(BAD_BOTH);
    let before_edit = discovered_fix_all(&mut client);
    client.did_change_full(URI, BAD_BOTH, 2);
    assert_eq!(
        client.request("codeAction/resolve", before_edit),
        Err(CONTENT_MODIFIED)
    );
    let before_close = discovered_fix_all(&mut client);
    client.did_close(URI);
    assert_eq!(
        client.request("codeAction/resolve", before_close.clone()),
        Err(CONTENT_MODIFIED)
    );
    client.did_open(URI, BAD_BOTH, 2);
    assert_eq!(
        client.request("codeAction/resolve", before_close),
        Err(CONTENT_MODIFIED)
    );
    let fresh = discovered_fix_all(&mut client);
    assert!(client.request("codeAction/resolve", fresh).is_ok());
    client.shutdown();
}

#[test]
fn unresolved_actions_depend_on_repair_settings_but_not_rendering_or_range_limits() {
    for (settings, stale) in [
        (json!({}), false),
        (json!({"hover": {"enabled": false}}), false),
        (json!({"limits": {"rangeLines": 1}}), false),
        (
            json!({"diagnostics": {"rules": {"integrity": false}}}),
            true,
        ),
        (json!({"defaultApplVerID": "FIX.4.2"}), true),
    ] {
        let mut client = open_resolving(BAD_BOTH);
        let held = discovered_fix_all(&mut client);
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings": {"fix": settings}}),
        );
        // Opening after the refresh waits for that batch and the new document's configuration.
        client.did_open("untitled:configuration-barrier", "", 1);
        let resolved = client.request("codeAction/resolve", held);
        if stale {
            assert_eq!(resolved, Err(CONTENT_MODIFIED), "{settings}");
        } else {
            let resolved = resolved.unwrap();
            assert_ne!(
                apply_document_action(BAD_BOTH, &resolved),
                BAD_BOTH,
                "{settings}"
            );
        }
        client.shutdown();
    }
}

#[test]
fn malformed_resolve_data_is_rejected_without_edits() {
    let mut client = open_resolving(BAD_BOTH);
    let action = discovered_fix_all(&mut client);
    for data in [
        Value::Null,
        json!({}),
        json!({"uri": URI}),
        json!("not a stamp"),
    ] {
        let mut malformed = action.clone();
        malformed["data"] = data;
        assert_eq!(
            client.request("codeAction/resolve", malformed),
            Err(INVALID_PARAMS)
        );
    }
    let mut malformed = action;
    malformed["kind"] = json!("refactor");
    assert_eq!(
        client.request("codeAction/resolve", malformed),
        Err(INVALID_PARAMS)
    );
    client.shutdown();
}

#[test]
fn ordinary_menu_explains_missing_resolve_support_without_changing_explicit_fallback() {
    for disabled_support in [false, true] {
        let mut client = TestClient::start();
        let mut capabilities = TestClient::caps_taking_actions();
        capabilities["textDocument"]["codeAction"]["disabledSupport"] = json!(disabled_support);
        client.initialize_caps(capabilities);
        client.did_open(URI, BAD_BOTH, 1);
        let offered = actions(&mut client, on_line(0));
        assert_eq!(offered[0]["kind"], "quickfix");
        assert_eq!(offered.len(), if disabled_support { 2 } else { 1 });
        if disabled_support {
            assert_eq!(offered[1]["kind"], "source.fixAll");
            assert!(!offered[1]["disabled"]["reason"]
                .as_str()
                .unwrap()
                .is_empty());
            assert!(offered[1].get("edit").is_none());
        }
        let explicit = actions(&mut client, source_params());
        assert_eq!(explicit.len(), 1);
        assert!(explicit[0]["edit"].is_object());
        let mut poll = source_params();
        poll["context"]["triggerKind"] = json!(2);
        let automatic = actions(&mut client, poll);
        assert_eq!(automatic.len(), usize::from(disabled_support));
        assert!(automatic.iter().all(|action| action.get("edit").is_none()));
        client.shutdown();
    }
}

#[test]
fn discovery_reports_disabled_repairs_without_offering_a_resolvable_action() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": resolving_caps(),
        "initializationOptions": {"fix": {"diagnostics": {"enabled": false}}},
    }));
    client.did_open(URI, BAD_BOTH, 1);
    let action = discovered_fix_all(&mut client);
    assert!(!action["disabled"]["reason"].as_str().unwrap().is_empty());
    assert!(action.get("edit").is_none());
    assert!(action.get("data").is_none());
    client.shutdown();
}

#[test]
fn resolve_can_be_cancelled_before_a_complete_edit_is_delivered() {
    let text = "8=FIX.4.4|9=1|35=0|10=001|\n".repeat(50_000);
    let mut client = open_resolving(&text);
    let action = discovered_fix_all(&mut client);
    let identifier = client.send_request("codeAction/resolve", action);
    client.notify("$/cancelRequest", json!({"id": identifier}));
    let responses = client.responses_for(std::slice::from_ref(&identifier));
    assert_eq!(responses.len(), 1);
    assert_eq!(
        responses[0].response_result.as_ref().unwrap_err().code,
        -32800
    );
    client.shutdown();
}
