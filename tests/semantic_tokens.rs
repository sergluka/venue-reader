//! `textDocument/semanticTokens` through the protocol.
//!
//! What the in-crate tests cannot show is the shape on the wire: which provider
//! a client is actually offered, that the legend goes out in the order a token
//! index means, and that an edit round-trips as the client's own array. Every
//! literal here is the protocol's, not the server's constants — a test that
//! took the name from `method.rs` would agree with the server whatever it sent.

mod harness;

use harness::TestClient;
use serde_json::{json, Value};

const URI: &str = "file:///a.fix";

const HEARTBEAT: &str =
    "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";

/// `shape` is what the client says it will send: `"range"` or `"full"`.
fn caps(shape: &str) -> Value {
    let requests = if shape == "range" {
        json!({ "range": true })
    } else {
        json!({ "full": { "delta": true } })
    };
    json!({
        "general": { "positionEncodings": ["utf-8"] },
        "textDocument": { "semanticTokens": {
            "requests": requests,
            "tokenTypes": [], "tokenModifiers": [], "formats": ["relative"],
        }},
    })
}

fn open(shape: &str, lines: usize) -> (TestClient, Value) {
    let mut client = TestClient::start();
    let result = client.initialize_caps(caps(shape));
    let text = vec![HEARTBEAT; lines].join("\n");
    client.did_open(URI, &text, 1);
    (client, result)
}

fn provider(result: &Value) -> &Value {
    &result["capabilities"]["semanticTokensProvider"]
}

fn integers(value: &Value) -> Vec<u64> {
    value
        .as_array()
        .expect("an array")
        .iter()
        .map(|at| at.as_u64().expect("an integer"))
        .collect()
}

#[test]
fn a_client_is_offered_the_shape_it_declared_and_not_the_other() {
    let (client, viewport) = open("range", 1);
    assert_eq!(provider(&viewport)["range"], json!(true));
    assert!(provider(&viewport).get("full").is_none(), "{viewport}");
    client.shutdown();

    let (client, whole) = open("full", 1);
    assert_eq!(provider(&whole)["full"], json!({ "delta": true }));
    assert!(provider(&whole).get("range").is_none(), "{whole}");
    client.shutdown();
}

/// The legend is what a token's index *means*, so its order is protocol and
/// asserted here against literals rather than against the server's own table.
#[test]
fn the_legend_goes_out_in_wire_order() {
    let (client, result) = open("full", 1);
    let legend = &provider(&result)["legend"];
    insta::assert_compact_json_snapshot!("legend", legend);
    client.shutdown();
}

#[test]
fn a_whole_answer_carries_an_id_and_an_array_that_is_never_null() {
    let (mut client, _) = open("full", 2);
    let answer = client
        .request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": URI } }),
        )
        .expect("answered");
    assert!(answer["resultId"].is_string(), "{answer}");
    let data = integers(&answer["data"]);
    assert_eq!(data.len() % 5, 0, "five integers per token");
    assert_eq!(data[0], 0, "the first token is on line 0");

    // A document with nothing to colour is `[]`, not `null`.
    client.did_change_full(URI, "", 2);
    let empty = client
        .request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": URI } }),
        )
        .expect("answered");
    assert_eq!(empty["data"], json!([]), "{empty}");
    client.shutdown();
}

/// A viewport answer is a standalone array measured from the top of the
/// document, so its first token names an absolute line.
#[test]
fn a_viewport_is_measured_from_the_top_of_the_document() {
    let (mut client, _) = open("range", 4);
    let answer = client
        .request(
            "textDocument/semanticTokens/range",
            json!({ "textDocument": { "uri": URI }, "range": {
                "start": { "line": 2, "character": 0 },
                "end": { "line": 3, "character": 0 },
            }}),
        )
        .expect("answered");
    assert_eq!(integers(&answer["data"])[0], 2);
    client.shutdown();
}

fn absolute_tokens(data: &Value) -> Vec<[u64; 5]> {
    let mut line = 0;
    let mut start = 0;
    integers(data)
        .as_chunks::<5>()
        .0
        .iter()
        .map(|token| {
            line += token[0];
            start = if token[0] == 0 {
                start + token[1]
            } else {
                token[1]
            };
            [line, start, token[2], token[3], token[4]]
        })
        .collect()
}

#[test]
fn column_ranges_return_exactly_the_intersecting_full_tokens() {
    let message = "🚀 > 8=FIX.4.4|35=D|453=1|448=甲🎵|447=D|452=1|802=1|523=S|803=1|54=1";
    let text = format!("# привет\n{message}\n# end");
    for encoding in ["utf-8", "utf-16"] {
        let column = |needle: &str| {
            let prefix = &message[..message.find(needle).unwrap()];
            (if encoding == "utf-8" {
                prefix.len()
            } else {
                prefix.encode_utf16().count()
            }) as u64
        };
        for overlays in [false, true] {
            let open_client = |shape| {
                let mut client = TestClient::start();
                let mut capabilities = caps(shape);
                capabilities["general"]["positionEncodings"] = json!([encoding]);
                capabilities["textDocument"]["semanticTokens"]["overlappingTokenSupport"] =
                    json!(overlays);
                client.initialize_caps(capabilities);
                client.did_open(URI, &text, 1);
                client
            };
            let mut whole = open_client("full");
            let params = json!({ "textDocument": { "uri": URI } });
            let full = absolute_tokens(
                &whole
                    .request("textDocument/semanticTokens/full", params)
                    .unwrap()["data"],
            );
            whole.shutdown();
            let mut client = open_client("range");
            let separator = column("|803");
            for (from, to) in [
                ((0, 1), (1, 1)),
                ((1, 0), (1, column("8="))),
                ((1, column("甲")), (1, column("|447"))),
                ((1, column("🎵") + 1), (1, column("🎵") + 2)),
                ((1, column("523=S") + 1), (1, column("523=S") + 2)),
                ((1, separator), (1, separator + 1)),
                ((1, separator), (1, separator)),
                ((1, column("54=1") + 2), (2, 0)),
                ((1, column("54=1")), (2, 2)),
                ((0, 0), (3, 0)),
            ] {
                let expected: Vec<_> = full
                    .iter()
                    .copied()
                    .filter(|&[line, start, length, _, _]| {
                        from < to && (line, start) < to && (line, start + length) > from
                    })
                    .collect();
                let response = client.request("textDocument/semanticTokens/range", json!({
                    "textDocument": { "uri": URI },
                    "range": {"start":{"line":from.0, "character":from.1}, "end":{"line":to.0, "character":to.1}}
                })).unwrap();
                assert_eq!(
                    absolute_tokens(&response["data"]),
                    expected,
                    "{encoding}, overlays={overlays}, {from:?}..{to:?}"
                );
            }
            client.shutdown();
        }
    }
}

/// The whole point, end to end: apply what the server sent to what it sent
/// before, and hold what asking again would have given.
#[test]
fn an_edit_round_trips_as_the_array_a_fresh_request_would_return() {
    let (mut client, _) = open("full", 4);
    let first = client
        .request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": URI } }),
        )
        .expect("answered");
    let before = integers(&first["data"]);
    let id = first["resultId"].as_str().expect("an id").to_owned();

    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": URI, "version": 2 },
            "contentChanges": [{
                "range": { "start": { "line": 1, "character": 16 },
                           "end": { "line": 1, "character": 17 } },
                "text": "8",
            }],
        }),
    );

    let delta = client
        .request(
            "textDocument/semanticTokens/full/delta",
            json!({ "textDocument": { "uri": URI }, "previousResultId": id }),
        )
        .expect("answered");
    let edits = delta["edits"].as_array().expect("edits").clone();

    // The client's own loop: ascending and disjoint in the original's offsets.
    let mut patched: Vec<u64> = Vec::new();
    let mut at = 0usize;
    for edit in &edits {
        let start = edit["start"].as_u64().expect("start") as usize;
        assert!(start >= at, "ascending and disjoint: {edits:?}");
        patched.extend_from_slice(&before[at..start]);
        if let Some(data) = edit.get("data") {
            patched.extend(integers(data));
        }
        at = start + edit["deleteCount"].as_u64().expect("deleteCount") as usize;
    }
    patched.extend_from_slice(&before[at..]);

    let fresh = client
        .request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": URI } }),
        )
        .expect("answered");
    assert_eq!(patched, integers(&fresh["data"]));
    client.shutdown();
}

/// An id the server does not know is the routine path after a refresh, so it is
/// answered whole rather than refused.
#[test]
fn an_unknown_id_is_answered_whole() {
    let (mut client, _) = open("full", 2);
    client
        .request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": URI } }),
        )
        .expect("answered");
    let answer = client
        .request(
            "textDocument/semanticTokens/full/delta",
            json!({ "textDocument": { "uri": URI }, "previousResultId": "never issued" }),
        )
        .expect("answered");
    assert!(answer.get("data").is_some(), "a whole answer: {answer}");
    assert!(answer.get("edits").is_none());
    client.shutdown();
}

/// The protocol makes the id mandatory, so a request without one is malformed
/// rather than a request for a whole answer.
#[test]
fn a_delta_without_an_id_is_refused() {
    let (mut client, _) = open("full", 2);
    let code = client
        .request(
            "textDocument/semanticTokens/full/delta",
            json!({ "textDocument": { "uri": URI } }),
        )
        .expect_err("refused");
    assert_eq!(code, -32602, "InvalidParams");
    client.shutdown();
}

/// A truncated answer is one a client cannot tell from "nothing to colour
/// there", so past the bound the answer is empty and says which setting.
#[test]
fn a_viewport_past_the_bound_is_answered_empty_and_says_why() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": caps("range"),
        "initializationOptions": { "fix": { "limits": { "rangeLines": 2 } } },
    }));
    client.did_open(URI, &[HEARTBEAT; 5].join("\n"), 1);
    let answer = client
        .request(
            "textDocument/semanticTokens/range",
            json!({ "textDocument": { "uri": URI }, "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 5, "character": 0 },
            }}),
        )
        .expect("answered");
    insta::assert_compact_json_snapshot!("viewport_past_bound", answer);
    client.wait_for_log("rangeLines");
    client.shutdown();
}

/// Switched off, the provider stays advertised — the switch gates the answer,
/// so changing it needs no reconnect.
#[test]
fn the_switch_gates_the_answer_and_not_the_capability() {
    let mut client = TestClient::start();
    let result = client.initialize_with(json!({
        "capabilities": caps("full"),
        "initializationOptions": { "fix": { "semanticTokens": { "enabled": false } } },
    }));
    assert!(provider(&result).is_object(), "still advertised: {result}");
    client.did_open(URI, HEARTBEAT, 1);
    let answer = client
        .request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": URI } }),
        )
        .expect("answered");
    assert_eq!(answer, Value::Null);
    client.shutdown();
}

/// NoHops(627) is a header group under FIX.4.4 and under `8=FIXT.1.1`: its
/// counter carries `count`, and a hop's tag sits in the first entry.
#[test]
fn a_header_group_counter_counts_and_its_entry_is_marked() {
    for line in [
        "8=FIX.4.4|35=0|627=1|628=A|",
        "8=FIXT.1.1|35=0|627=1|628=A|",
    ] {
        let mut client = TestClient::start();
        client.initialize_caps(caps("full"));
        client.did_open(URI, line, 1);
        let answer = client
            .request(
                "textDocument/semanticTokens/full",
                json!({ "textDocument": { "uri": URI } }),
            )
            .expect("answered");
        let data = integers(&answer["data"]);
        let mut column = 0;
        let mut by_column = Vec::new();
        for token in data.chunks(5) {
            column += token[1];
            by_column.push((column, token[3], token[4]));
        }
        // Modifier bits in legend order: header 1<<1, session 1<<3, count
        // 1<<6, group1a 1<<7. Type 0 is `property`.
        let counter = line.find("627").expect("the counter") as u64;
        let hop = line.find("628").expect("a hop") as u64;
        assert!(
            by_column.contains(&(counter, 0, (1 << 1) | (1 << 3) | (1 << 6))),
            "{line}: {by_column:?}"
        );
        assert!(
            by_column.contains(&(hop, 0, (1 << 1) | (1 << 3) | (1 << 7))),
            "{line}: {by_column:?}"
        );
        client.shutdown();
    }
}

fn tokens_by_column(line: &str) -> Vec<(u64, u64, u64)> {
    let mut client = TestClient::start();
    client.initialize_caps(caps("full"));
    client.did_open(URI, line, 1);
    let answer = client
        .request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": URI } }),
        )
        .expect("answered");
    let data = integers(&answer["data"]);
    let mut column = 0;
    let tokens = data
        .as_chunks::<5>()
        .0
        .iter()
        .map(|token| {
            column += token[1];
            (column, token[3], token[4])
        })
        .collect();
    client.shutdown();
    tokens
}

#[test]
fn unresolved_ownership_is_not_an_unknown_tag() {
    for line in [
        "8=FIXT.1.1|35=D|54=1|9999=x|",
        "8=FIX.5.0|35=D|54=1|9999=x|",
        "8=FIX.5.0SP1|35=D|54=1|9999=x|",
        "8=FIX.5.0SP2|35=D|54=1|9999=x|",
    ] {
        let tokens = tokens_by_column(line);
        assert!(
            tokens.iter().all(|token| token.2 & (1 << 5) == 0),
            "{line}: {tokens:?}"
        );
        let side = line.find("54=").unwrap() as u64;
        assert!(tokens.contains(&(side + 3, 6, 0)), "{line}: {tokens:?}");
    }
    let resolved = "8=FIXT.1.1|35=D|1128=9|54=1|9999=x|";
    let tokens = tokens_by_column(resolved);
    let unknown = resolved.find("9999=").unwrap() as u64;
    assert!(tokens.contains(&(unknown, 0, 1 << 5)), "{tokens:?}");
    let side = resolved.find("54=").unwrap() as u64;
    assert!(tokens.contains(&(side + 3, 1, 0)), "{tokens:?}");
}

#[test]
fn fixt_group_counts_and_transport_fields_keep_their_scope_modifiers() {
    const LINE: &str = "8=FIXT.1.1|35=D|1128=9|627=1|628=H|453=0|10=0|";
    let tokens = tokens_by_column(LINE);
    for (tag, modifiers) in [
        ("1128=", 1 << 1),
        ("627=", (1 << 1) | (1 << 6)),
        ("628=", (1 << 1) | (1 << 7)),
        ("453=", 1 << 6),
        ("10=", 1 << 2),
    ] {
        let column = LINE.find(tag).unwrap() as u64;
        assert!(
            tokens.contains(&(column, 0, modifiers)),
            "{tag}: {tokens:?}"
        );
    }
}

#[test]
fn joined_oversized_lines_drop_tokens_and_splitting_restores_the_exact_delta() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities":caps("full"),
        "initializationOptions":{"fix":{"limits":{"lineBytes":HEARTBEAT.len()}}}
    }));
    client.did_open(URI, &[HEARTBEAT; 4].join("\n"), 1);
    let full = client
        .request(
            "textDocument/semanticTokens/full",
            json!({"textDocument":{"uri":URI}}),
        )
        .unwrap();
    let initial = absolute_tokens(&full["data"]);
    assert!(!initial.is_empty());
    let mut live = integers(&full["data"]);
    let mut previous = full["resultId"].clone();
    let end = HEARTBEAT.len();
    for (version, end_line, end_column, replacement) in [(2, 2, 0, ""), (3, 1, end, "\n")] {
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":URI,"version":version},
                "contentChanges":[{"range":{
                    "start":{"line":1,"character":end},
                    "end":{"line":end_line,"character":end_column}
                },"text":replacement}]
            }),
        );
        let delta = client
            .request(
                "textDocument/semanticTokens/full/delta",
                json!({
                    "textDocument":{"uri":URI},"previousResultId":previous
                }),
            )
            .unwrap();
        previous = delta["resultId"].clone();
        for edit in delta["edits"].as_array().unwrap().iter().rev() {
            let first = edit["start"].as_u64().unwrap() as usize;
            let count = edit["deleteCount"].as_u64().unwrap() as usize;
            let data = edit.get("data").map(integers).unwrap_or_default();
            live.splice(first..first + count, data);
        }
        let actual = absolute_tokens(&json!(live));
        if replacement.is_empty() {
            let expected: Vec<_> = initial
                .iter()
                .filter_map(|&[line, start, len, kind, mods]| match line {
                    0 => Some([0, start, len, kind, mods]),
                    3 => Some([2, start, len, kind, mods]),
                    _ => None,
                })
                .collect();
            assert_eq!(actual, expected);
        } else {
            assert_eq!(actual, initial);
        }
    }
    client.shutdown();
}

#[test]
fn full_highlighting_logs_the_line_cutoff_even_if_the_byte_budget_skips_a_line() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": caps("full"),
        "initializationOptions": {"fix": {"limits": {
            "semanticTokensFullLines": 2, "semanticTokensBytes": 2
        }}}
    }));
    let params = json!({"textDocument": {"uri": URI}});
    client.did_open(URI, "#long\n#x\n#y", 1);
    let answer = client
        .request("textDocument/semanticTokens/full", params.clone())
        .unwrap();
    let tokens = absolute_tokens(&answer["data"]);
    assert_eq!(tokens.len(), 1);
    assert_eq!(&tokens[0][..3], &[1, 0, 2]);
    let log = client.wait_for_log("fix.limits.semanticTokensFullLines");
    assert_eq!(log["type"], 3);
    assert_eq!(
        log["message"],
        "venue-reader: file:///a.fix — full highlighting considers the first 2 of 3 lines \
         (fix.limits.semanticTokensFullLines). Within those lines, fix.limits.semanticTokensBytes \
         limits the total source bytes highlighted."
    );
    client.drain_notifications(std::time::Duration::ZERO);

    client.did_change_full(URI, "#long\n#x", 2);
    let (answer, notifications) =
        client.request_collecting("textDocument/semanticTokens/full", params);
    assert_eq!(absolute_tokens(&answer.unwrap()["data"]), tokens);
    assert!(
        notifications
            .iter()
            .all(|(method, _)| method != "window/logMessage"),
        "byte-only skips are silent: {notifications:?}"
    );
    client.shutdown();
}

#[test]
fn the_default_highlight_budget_admits_exactly_65536_source_bytes() {
    let exact = format!("#{}", "x".repeat(65_535));
    let text = format!("{exact}x\r\n{exact}\r\n#tail");
    for shape in ["range", "full"] {
        let mut client = TestClient::start();
        client.initialize_caps(caps(shape));
        client.did_open(URI, &text, 1);
        let mut params = json!({"textDocument":{"uri":URI}});
        if shape == "range" {
            params["range"] = json!({
                "start":{"line":0,"character":0}, "end":{"line":3,"character":0}
            });
        }
        let result = client
            .request(&format!("textDocument/semanticTokens/{shape}"), params)
            .unwrap();
        let tokens = absolute_tokens(&result["data"]);
        assert_eq!(tokens.len(), 1, "{shape}");
        assert_eq!(&tokens[0][..3], &[1, 0, 65_536]);
        client.shutdown();
    }
}

#[test]
fn a_utf16_fragment_costs_source_bytes_and_returns_the_intersecting_token() {
    let mut client = TestClient::start();
    let mut capabilities = caps("range");
    capabilities["general"]["positionEncodings"] = json!(["utf-16"]);
    client.initialize_with(json!({
        "capabilities": capabilities,
        "initializationOptions":{"fix":{"limits":{"semanticTokensBytes":4}}}
    }));
    client.did_open(URI, "#x🙂y", 1);
    for (end, fits) in [(4, true), (5, false)] {
        let result = client
            .request(
                "textDocument/semanticTokens/range",
                json!({
                    "textDocument":{"uri":URI},"range":{
                        "start":{"line":0,"character":2},"end":{"line":0,"character":end}
                    }
                }),
            )
            .unwrap();
        let tokens = absolute_tokens(&result["data"]);
        if fits {
            assert_eq!(tokens.len(), 1);
            assert_eq!(&tokens[0][..3], &[0, 0, 5]);
        } else {
            assert!(tokens.is_empty());
        }
    }
    client.shutdown();
}

#[test]
fn editing_an_early_fragment_moves_the_budget_across_unchanged_lines_in_a_delta() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities":caps("full"),
        "initializationOptions":{"fix":{"limits":{"semanticTokensBytes":10}}}
    }));
    client.did_open(URI, "#abc\n#123456\n#end\n#x", 1);
    let params = json!({"textDocument":{"uri":URI}});
    let mut result = client
        .request("textDocument/semanticTokens/full", params.clone())
        .unwrap();
    let mut data = integers(&result["data"]);
    let rows = |data: &[u64]| {
        absolute_tokens(&json!(data))
            .iter()
            .map(|token| token[0])
            .collect::<Vec<_>>()
    };
    assert_eq!(rows(&data), [0, 2, 3]);
    for (version, end, replacement, expected) in [(2, 4, "#", [0, 1, 3]), (3, 1, "#abc", [0, 2, 3])]
    {
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":URI,"version":version}, "contentChanges":[{
                    "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":end}},
                    "text":replacement
                }]
            }),
        );
        result = client
            .request(
                "textDocument/semanticTokens/full/delta",
                json!({
                    "textDocument":{"uri":URI},"previousResultId":result["resultId"]
                }),
            )
            .unwrap();
        if let Some(full) = result.get("data") {
            data = integers(full);
        } else {
            for edit in result["edits"].as_array().unwrap().iter().rev() {
                let start = edit["start"].as_u64().unwrap() as usize;
                let delete = edit["deleteCount"].as_u64().unwrap() as usize;
                data.splice(
                    start..start + delete,
                    edit.get("data").map(integers).unwrap_or_default(),
                );
            }
        }
        assert_eq!(rows(&data), expected);
        let fresh = client
            .request("textDocument/semanticTokens/full", params.clone())
            .unwrap();
        assert_eq!(data, integers(&fresh["data"]));
        result = fresh;
    }
    client.shutdown();
}

#[test]
fn a_range_client_keeps_getting_ranges_after_unadvertised_full_requests() {
    let mut client = TestClient::start();
    let mut capabilities = caps("range");
    capabilities["textDocument"]["semanticTokens"]["requests"]["full"] = json!({"delta":true});
    let negotiated = client.initialize_caps(capabilities);
    assert!(provider(&negotiated).get("full").is_none());
    client.did_open(URI, "#a", 1);
    for version in [1, 2] {
        if version == 2 {
            client.did_change_full(URI, "#b", version);
        }
        let params = json!({"textDocument":{"uri":URI}});
        let mut range = params.clone();
        range["range"] = json!({"start":{"line":0,"character":0},"end":{"line":1,"character":0}});
        assert_eq!(
            absolute_tokens(
                &client
                    .request("textDocument/semanticTokens/range", range)
                    .unwrap()["data"]
            )
            .len(),
            1
        );
        assert!(client
            .request("textDocument/semanticTokens/full", params.clone())
            .unwrap()
            .is_null());
        let mut delta = params;
        delta["previousResultId"] = json!("old");
        assert!(client
            .request("textDocument/semanticTokens/full/delta", delta)
            .unwrap()
            .is_null());
    }
    client.shutdown();
}
