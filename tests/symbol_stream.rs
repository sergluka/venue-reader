//! Generic LSP document-symbol streaming and budget contracts.

mod harness;

use harness::{nesting_client, TestClient};
use serde_json::{json, Value};
use std::time::Duration;

const URI: &str = "file:///synthetic-stream.fix";
const SYMBOLS: &str = "textDocument/documentSymbol";
const RESULT_TOKEN: &str = "synthetic-result";
const WORK_TOKEN: &str = "synthetic-work";

/// A client with these settings and diagnostics off, so only symbol work reports progress.
fn client(mut settings: Value, text: &str) -> TestClient {
    settings["diagnostics"] = json!({"enabled": false});
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": nesting_client(),
        "initializationOptions": {"fix": settings}
    }));
    client.did_open(URI, text, 1);
    client
}

fn params(streaming: bool) -> Value {
    let mut params = json!({"textDocument": {"uri": URI}, "workDoneToken": WORK_TOKEN});
    if streaming {
        params["partialResultToken"] = json!(RESULT_TOKEN);
    }
    params
}

/// The `$/progress` values sent under one token, in order.
fn progress_values<'a>(notes: &'a [(String, Value)], token: &str) -> Vec<&'a Value> {
    notes
        .iter()
        .filter(|(method, params)| method == "$/progress" && params["token"] == token)
        .map(|(_, params)| &params["value"])
        .collect()
}

fn roots(notes: &[(String, Value)]) -> Vec<Value> {
    progress_values(notes, RESULT_TOKEN)
        .into_iter()
        .flat_map(|chunk| chunk.as_array().expect("partial array").clone())
        .collect()
}

fn assert_progress(notes: &[(String, Value)]) {
    let progress = progress_values(notes, WORK_TOKEN);
    assert_eq!(progress.first().expect("begin")["kind"], "begin");
    assert_eq!(progress.last().expect("end")["kind"], "end");
    assert_eq!(
        progress
            .iter()
            .filter(|value| value["kind"] == "begin")
            .count(),
        1
    );
    assert_eq!(
        progress
            .iter()
            .filter(|value| value["kind"] == "end")
            .count(),
        1
    );
    let percentages: Vec<_> = progress
        .iter()
        .filter_map(|value| value["percentage"].as_u64())
        .collect();
    assert_eq!(percentages.first(), Some(&0));
    assert!(percentages.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(percentages.iter().all(|percentage| *percentage <= 100));
}

fn count(nodes: &[Value]) -> usize {
    nodes
        .iter()
        .map(|node| {
            1 + node["children"]
                .as_array()
                .map_or(0, |children| count(children))
        })
        .sum()
}

fn symbols(client: &mut TestClient, streaming: bool) -> Vec<Value> {
    let (response, notes) = client.request_collecting(SYMBOLS, params(streaming));
    let response = response.expect("symbols succeed");
    assert_progress(&notes);
    if streaming {
        assert_eq!(
            response,
            json!([]),
            "streamed request has empty final result"
        );
        roots(&notes)
    } else {
        assert!(
            roots(&notes).is_empty(),
            "no partial token means no partial notifications"
        );
        response.as_array().expect("final array").clone()
    }
}

#[test]
fn streamed_roots_are_ordered_complete_and_equal_to_final_only_output() {
    let text = (0..600)
        .map(|number| format!("8=FIX.4.4|35=0|34={number}|"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut client = client(json!({}), &text);
    let (response, notes) = client.request_collecting(SYMBOLS, params(true));
    assert_eq!(response, Ok(json!([])));
    assert_progress(&notes);
    let chunks = progress_values(&notes, RESULT_TOKEN);
    assert!(
        chunks.len() > 2,
        "multiple partial arrays: {}",
        chunks.len()
    );
    assert_eq!(
        chunks[0].as_array().expect("first batch").len(),
        1,
        "first safe root flushes immediately"
    );
    for chunk in &chunks {
        let nodes = chunk.as_array().expect("partial array");
        assert!(count(nodes) <= 256);
        assert!(serde_json::to_vec(nodes).expect("JSON").len() <= 64 * 1024);
        assert!(nodes.iter().all(|node| node["children"]
            .as_array()
            .is_some_and(|children| children.len() == 3)));
    }
    let streamed = roots(&notes);
    assert_eq!(streamed.len(), 600);
    assert!(streamed
        .iter()
        .enumerate()
        .all(|(number, node)| node["range"]["start"]["line"] == number));
    client
        .hover(URI, 0, 11)
        .expect("viewport hover warms the first line analysis");
    assert_eq!(streamed, symbols(&mut client, false));
    client.shutdown();
}

#[test]
fn streams_after_initial_configuration_deferral() {
    let mut client = TestClient::start();
    let mut caps = nesting_client();
    caps["workspace"] = json!({"configuration": true});
    client.initialize_caps(caps);
    let (pull, method, _) = client.recv_server_request();
    assert_eq!(method, "workspace/configuration");
    client.did_open_unready(URI, "8=FIX.4.4|35=0|", 1);
    let request = client.send_request(SYMBOLS, params(true));
    let barrier = client.send_request("synthetic/unsupported", json!({}));
    let responses = client.responses_through_barrier(&barrier);
    assert_eq!(
        responses.len(),
        1,
        "configuration-dependent symbols remain pending"
    );
    assert_eq!(responses[0].id, barrier);
    client.reply(pull, json!([{"diagnostics": {"enabled": false}}]));
    let responses = client.responses_through_barrier(&request);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].id, request);
    assert_eq!(
        responses[0]
            .response_result
            .as_ref()
            .expect("stream completes"),
        &json!([])
    );
    let notes = client.drain_notifications(Duration::ZERO);
    assert_eq!(roots(&notes).len(), 1);
    assert_progress(&notes);
    client.shutdown();
}

#[test]
fn empty_and_disabled_documents_finish_without_symbols() {
    for (settings, text) in [
        (json!({}), ""),
        (
            json!({"documentSymbol": {"enabled": false}}),
            "8=FIX.4.4|35=0|",
        ),
    ] {
        let mut client = client(settings, text);
        for streaming in [false, true] {
            let (response, notes) = client.request_collecting(SYMBOLS, params(streaming));
            assert_eq!(response, Ok(json!([])));
            assert!(roots(&notes).is_empty());
            if !progress_values(&notes, WORK_TOKEN).is_empty() {
                assert_progress(&notes);
            }
        }
        client.shutdown();
    }
}

#[test]
fn budget_markers_have_exact_text_anchor_and_shared_item_accounting() {
    let two = "8=FIX.4.4|35=0|\n8=FIX.4.4|35=0|";
    for streaming in [false, true] {
        for (cap, depth, text, expected_items, marker_name, anchor) in [
            (
                1,
                "full",
                "8=FIX.4.4|35=0|",
                1,
                "… 1 more messages; some fields/groups omitted",
                0,
            ),
            (
                2,
                "full",
                two,
                2,
                "… 1 more messages; some fields/groups omitted",
                0,
            ),
            (
                4,
                "full",
                two,
                4,
                "… 1 more messages; some fields/groups omitted",
                1,
            ),
            (5, "full", two, 5, "… Some fields/groups omitted", 1),
            (1, "messages", two, 1, "… 2 more messages", 0),
        ] {
            let mut client = client(
                json!({"documentSymbol": {"depth": depth}, "limits": {"documentSymbolItems": cap}}),
                text,
            );
            let found = symbols(&mut client, streaming);
            assert_eq!(count(&found), expected_items, "{found:?}");
            assert!(count(&found) <= cap);
            let marker = found.last().expect("marker");
            assert_eq!(marker["name"], marker_name);
            assert_eq!(marker["kind"], 15);
            assert_eq!(
                marker["range"],
                json!({"start": {"line": anchor, "character": 0}, "end": {"line": anchor, "character": 0}})
            );
            assert_eq!(marker["selectionRange"], marker["range"]);
            assert!(client.drain_notifications(Duration::ZERO).iter().all(
                |(method, params)| method != "window/logMessage"
                    || !params["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("outline is truncated"))
            ));
            client.shutdown();
        }
    }
}

#[test]
fn complete_exact_cap_does_not_lose_a_root_to_a_marker() {
    for streaming in [false, true] {
        for (depth, cap) in [("messages", 2), ("full", 6)] {
            let mut client = client(
                json!({"documentSymbol": {"depth": depth}, "limits": {"documentSymbolItems": cap}}),
                "8=FIX.4.4|35=0|\n8=FIX.4.4|35=0|",
            );
            let found = symbols(&mut client, streaming);
            assert_eq!(found.len(), 2);
            assert_eq!(count(&found), cap);
            assert!(found
                .iter()
                .all(|root| !root["name"].as_str().expect("name").starts_with('…')));
            client.shutdown();
        }
    }
}

#[test]
fn oversized_root_is_sent_alone_without_trimming_its_value() {
    let value = "SYNTHETIC_🙂".repeat(10_000);
    let text = format!("8=FIX.4.4|35=0|\n8=FIX.4.4|35=0|58={value}|\n8=FIX.4.4|35=0|");
    let mut client = client(json!({}), &text);
    let (response, notes) = client.request_collecting(SYMBOLS, params(true));
    assert_eq!(response, Ok(json!([])));
    let chunks = progress_values(&notes, RESULT_TOKEN);
    let oversized = chunks
        .iter()
        .find(|chunk| serde_json::to_vec(chunk).expect("encoded array").len() > 64 * 1024)
        .expect("oversized batch");
    assert_eq!(oversized.as_array().expect("batch").len(), 1);
    let found = roots(&notes);
    assert_eq!(found.len(), 3);
    assert_eq!(
        found[1]["children"][2]["name"],
        format!("Text (58) = {value}")
    );
    assert_eq!(found, symbols(&mut client, false));
    client.shutdown();
}
