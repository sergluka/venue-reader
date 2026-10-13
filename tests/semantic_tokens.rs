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
    assert_eq!(
        legend["tokenTypes"],
        json!([
            "property",
            "enumMember",
            "number",
            "price",
            "quantity",
            "money",
            "string",
            "keyword",
            "namespace",
            "comment",
            "timestamp",
            "group",
        ])
    );
    assert_eq!(
        legend["tokenModifiers"],
        json!([
            "deprecated",
            "header",
            "trailer",
            "session",
            "possDup",
            "unknown",
            "count",
            "group1a",
            "group1b",
            "group2a",
            "group2b",
            "group3a",
            "group3b",
        ])
    );
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
    assert_eq!(answer["data"], json!([]), "{answer}");
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
