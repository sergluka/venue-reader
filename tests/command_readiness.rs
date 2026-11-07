//! Configuration-dependent requests wait without blocking lifecycle events.

mod harness;

use harness::{TestClient, INVALID_PARAMS, REQUEST_FAILED};
use lsp_server::{RequestId, Response};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const EXECUTE: &str = "workspace/executeCommand";
const CANCELLED: i32 = -32800;

fn list() -> Value {
    json!({"command":"fix.listDictionaries"})
}

fn tag(value: &str) -> Value {
    json!({"command":"fix.describeTag","arguments":[54, value]})
}

fn barrier(client: &mut TestClient) -> Vec<Response> {
    let barrier = client.send_request(EXECUTE, json!({"command":"fix.unknown"}));
    let mut responses = client.responses_through_barrier(&barrier);
    let final_response = responses.pop().unwrap();
    assert_eq!(final_response.id, barrier);
    assert_eq!(
        final_response.response_result.unwrap_err().code,
        INVALID_PARAMS
    );
    responses
}

fn complete(client: &mut TestClient, last: &RequestId) -> Vec<Response> {
    let mut responses = client.responses_through_barrier(last);
    responses.extend(barrier(client));
    responses
}

#[test]
fn idle_loop_expires_a_command_without_an_active_configuration_batch() {
    let mut client = TestClient::start();
    // Exercise the defensive deadline before initialization starts a batch.
    let started = Instant::now();
    let listing = client.send_request(EXECUTE, list());
    let responses = client.responses_through_barrier(&listing);
    assert!(started.elapsed() >= Duration::from_secs(5));
    assert_eq!(responses.len(), 1);
    let error = responses[0].response_result.as_ref().unwrap_err();
    assert_eq!(error.code, REQUEST_FAILED);
    assert_eq!(
        error.message,
        "configuration command wait deadline exceeded (5 seconds)"
    );
    assert!(barrier(&mut client).is_empty(), "only one expiry response");
    client.initialize_caps(json!({}));
    assert!(client.request(EXECUTE, list()).is_ok());
    client.shutdown();
}

#[test]
fn both_commands_use_committed_settings_during_a_second_pull() {
    for refresh_first in [true, false] {
        let mut client = TestClient::start();
        client.initialize_caps(TestClient::caps_with_pull());
        let (first, method, _) = client.recv_server_request();
        assert_eq!(method, "workspace/configuration");
        if refresh_first {
            client.notify("workspace/didChangeConfiguration", json!({"settings":null}));
            client.reply(
                first,
                json!([{ "documentLink": {"url":"https://example.org/old/{tag}"}}]),
            );
        } else {
            client.reply(
                first,
                json!([{ "documentLink": {"url":"https://example.org/old/{tag}"}}]),
            );
            client.notify("workspace/didChangeConfiguration", json!({"settings":null}));
        }
        let (second, method, _) = client.recv_server_request();
        assert_eq!(method, "workspace/configuration");
        let listing = client.send_request(EXECUTE, list());
        let description = client.send_request(EXECUTE, tag("1"));
        let responses = complete(&mut client, &description);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].id, listing);
        assert!(responses[0].response_result.as_ref().unwrap()["dictionaries"].is_array());
        assert_eq!(responses[1].id, description);
        assert!(responses[1]
            .response_result
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap()
            .contains("https://example.org/old/54"));
        // The next pull is emitted only after this replacement commits.
        client.notify("workspace/didChangeConfiguration", json!({"settings":null}));
        client.reply(
            second,
            json!([{ "documentLink": {"url":"https://example.org/new/{tag}"}}]),
        );
        let (third, method, _) = client.recv_server_request();
        assert_eq!(method, "workspace/configuration");
        let description = client.request(EXECUTE, tag("1")).unwrap();
        assert!(description
            .as_str()
            .unwrap()
            .contains("https://example.org/new/54"));
        client.reply(third, json!([null]));
        client.shutdown();
    }
}

#[test]
fn cancellation_releases_the_id_and_duplicate_live_ids_preserve_the_original() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let id = RequestId::from("cancel-and-reuse".to_owned());
    client.send_request_with_id(id.clone(), EXECUTE, tag("1"));
    assert!(barrier(&mut client).is_empty());
    client.notify("$/cancelRequest", json!({"id":id}));
    client.notify("$/cancelRequest", json!({"id":id}));
    let cancelled = barrier(&mut client);
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].id, id);
    assert_eq!(
        cancelled[0].response_result.as_ref().unwrap_err().code,
        CANCELLED
    );
    client.send_request_with_id(id.clone(), EXECUTE, tag("2"));
    client.send_request_with_id(id.clone(), EXECUTE, tag("1"));
    assert!(barrier(&mut client).is_empty());
    client.answer_base_pull();
    let responses = complete(&mut client, &id);
    assert_eq!(responses.len(), 1);
    let text = responses[0]
        .response_result
        .as_ref()
        .unwrap()
        .as_str()
        .unwrap();
    assert!(text.contains("Sell"), "{text}");
    assert!(!text.contains("**Buy**"), "{text}");
    client.shutdown();
}

#[test]
fn rejected_configuration_fails_waiters_and_a_valid_update_accepts_new_requests() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let listing = client.send_request(EXECUTE, list());
    let description = client.send_request(EXECUTE, tag("1"));
    assert!(barrier(&mut client).is_empty());
    client.answer(
        "workspace/configuration",
        json!([{"dictionaries":"invalid"}]),
    );
    let responses = complete(&mut client, &description);
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0].id, listing);
    for response in responses {
        let error = response.response_result.unwrap_err();
        assert_eq!(error.code, REQUEST_FAILED);
        assert!(error.message.contains("valid configuration update"));
    }
    assert_eq!(client.request(EXECUTE, list()), Err(REQUEST_FAILED));
    client.notify("workspace/didChangeConfiguration", json!({"settings":null}));
    client.notify("workspace/didChangeConfiguration", json!({"settings":null}));
    client.answer_base_pull();
    // The queued second pull proves the recovering batch committed.
    client.answer_base_pull();
    assert!(client.request(EXECUTE, list()).is_ok());
    client.shutdown();
}

#[test]
fn shutdown_fails_waiters_before_its_response() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let listing = client.send_request(EXECUTE, list());
    let description = client.send_request(EXECUTE, tag("1"));
    assert!(barrier(&mut client).is_empty());
    let shutdown = client.send_request("shutdown", Value::Null);
    let responses = client.responses_through_barrier(&shutdown);
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0].id, listing);
    assert_eq!(responses[1].id, description);
    for response in &responses[..2] {
        assert_eq!(
            response.response_result.as_ref().unwrap_err().code,
            REQUEST_FAILED
        );
    }
    assert_eq!(responses[2].id, shutdown);
    assert_eq!(responses[2].response_result.as_ref().unwrap(), &Value::Null);
    client.notify("exit", Value::Null);
    assert_eq!(
        client.wait_for_exit(),
        venue_reader::server::ServerExit::Normal
    );
}

#[test]
fn count_limit_rejects_only_the_new_request_and_cancel_releases_capacity() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let pending: Vec<_> = (0..64)
        .map(|_| client.send_request(EXECUTE, list()))
        .collect();
    assert!(barrier(&mut client).is_empty());
    let overflow = client.send_request(EXECUTE, list());
    let responses = complete(&mut client, &overflow);
    assert_eq!(responses.len(), 1);
    let error = responses[0].response_result.as_ref().unwrap_err();
    assert_eq!(error.code, REQUEST_FAILED);
    assert_eq!(
        error.message,
        "pending configuration command capacity exceeded"
    );
    client.notify("$/cancelRequest", json!({"id":pending[0]}));
    let cancelled = barrier(&mut client);
    assert_eq!(cancelled.len(), 1);
    assert_eq!(
        cancelled[0].response_result.as_ref().unwrap_err().code,
        CANCELLED
    );
    let replacement = client.send_request(EXECUTE, list());
    assert!(barrier(&mut client).is_empty());
    client.answer_base_pull();
    let responses = complete(&mut client, &replacement);
    assert_eq!(responses.len(), 64);
    for (response, expected) in responses
        .iter()
        .zip(pending[1..].iter().chain([&replacement]))
    {
        assert_eq!(&response.id, expected);
        assert!(response.response_result.is_ok());
    }
    client.shutdown();
}

#[test]
fn payload_limit_counts_escaped_string_ids_and_releases_bytes_on_cancel() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let id = RequestId::from("quoted\"\\\n".to_owned());
    let id_bytes = serde_json::to_vec(&id).unwrap().len();
    let value = "x".repeat(1024 * 1024 - id_bytes);
    client.send_request_with_id(id.clone(), EXECUTE, tag(&value));
    assert!(
        barrier(&mut client).is_empty(),
        "exact payload limit is admitted"
    );
    let overflow = client.send_request(EXECUTE, list());
    let responses = complete(&mut client, &overflow);
    assert_eq!(responses.len(), 1);
    assert_eq!(
        responses[0].response_result.as_ref().unwrap_err().code,
        REQUEST_FAILED
    );
    client.notify("$/cancelRequest", json!({"id":id}));
    assert_eq!(barrier(&mut client).len(), 1);
    client.send_request_with_id(id.clone(), EXECUTE, tag(&(value + "x")));
    let responses = complete(&mut client, &id);
    assert_eq!(responses.len(), 1);
    assert_eq!(
        responses[0].response_result.as_ref().unwrap_err().code,
        REQUEST_FAILED
    );
    let replacement = client.send_request(EXECUTE, list());
    assert!(barrier(&mut client).is_empty());
    client.answer_base_pull();
    let responses = complete(&mut client, &replacement);
    assert_eq!(responses.len(), 1);
    assert!(responses[0].response_result.is_ok());
    client.shutdown();
}

fn document_requests(uri: &str) -> Vec<(&'static str, Value)> {
    let document = json!({"uri":uri});
    let position = json!({"line":0,"character":2});
    let range = json!({"start":{"line":0,"character":0},"end":{"line":0,"character":20}});
    vec![
        (
            "textDocument/hover",
            json!({"textDocument":document,"position":position}),
        ),
        (
            "textDocument/codeAction",
            json!({"textDocument":document,"range":range,"context":{"diagnostics":[]}}),
        ),
        ("textDocument/codeLens", json!({"textDocument":document})),
        (
            "textDocument/inlayHint",
            json!({"textDocument":document,"range":range}),
        ),
        (
            "textDocument/selectionRange",
            json!({"textDocument":document,"positions":[position]}),
        ),
        (
            "textDocument/documentSymbol",
            json!({"textDocument":document}),
        ),
        (
            "textDocument/semanticTokens/range",
            json!({"textDocument":document,"range":range}),
        ),
        (
            "textDocument/semanticTokens/full",
            json!({"textDocument":document}),
        ),
        (
            "textDocument/semanticTokens/full/delta",
            json!({"textDocument":document,"previousResultId":"unknown"}),
        ),
        (
            EXECUTE,
            json!({"command":"fix.decode","arguments":[{"textDocument":document,"ranges":[range]}]}),
        ),
        (
            EXECUTE,
            json!({"command":"fix.describeOverrides","arguments":[uri]}),
        ),
    ]
}

#[test]
fn all_document_requests_wait_for_initial_configuration() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let uri = "file:///synthetic-readiness.fix";
    client.did_open_unready(uri, "8=FIX.4.4|35=0|49=SYNTHETIC_SENDER|", 1);
    let pending: Vec<_> = document_requests(uri)
        .into_iter()
        .map(|(method, params)| client.send_request(method, params))
        .collect();
    assert!(barrier(&mut client).is_empty());
    client.answer_base_pull();
    let responses = complete(&mut client, pending.last().unwrap());
    assert_eq!(responses.len(), pending.len());
    for (response, expected) in responses.iter().zip(&pending) {
        assert_eq!(&response.id, expected);
        assert!(response.response_result.is_ok(), "{response:?}");
    }
    client.shutdown();
}

#[test]
fn queued_positions_reject_edits_but_whole_document_requests_survive() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let uri = "file:///synthetic-edit.fix";
    client.did_open_unready(uri, "8=FIX.4.4|35=0|", 1);
    let hover = client.send_request(
        "textDocument/hover",
        json!({"textDocument":{"uri":uri},"position":{"line":0,"character":2}}),
    );
    let symbols = client.send_request(
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":uri}}),
    );
    assert!(barrier(&mut client).is_empty());
    client.did_change_full(uri, "8=FIX.4.4|35=D|", 2);
    let early = barrier(&mut client);
    client.answer_base_pull();
    let mut responses = early;
    responses.extend(complete(&mut client, &symbols));
    assert_eq!(responses.len(), 2);
    let hover = responses
        .iter()
        .find(|response| response.id == hover)
        .unwrap();
    assert_eq!(
        hover.response_result.as_ref().unwrap_err().code,
        harness::CONTENT_MODIFIED
    );
    assert!(responses
        .iter()
        .find(|response| response.id == symbols)
        .unwrap()
        .response_result
        .is_ok());
    client.shutdown();
}

#[test]
fn many_documents_share_the_bound_and_closure_reclaims_capacity() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let mut pending = Vec::new();
    for index in 0..64 {
        let uri = format!("file:///synthetic-startup-{index}.fix");
        client.did_open_unready(&uri, "8=FIX.4.4|35=0|", 1);
        pending.push(client.send_request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
        ));
    }
    assert!(barrier(&mut client).is_empty());
    let overflow = client.send_request(
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":"file:///synthetic-startup-0.fix"}}),
    );
    let responses = complete(&mut client, &overflow);
    assert_eq!(responses.len(), 1);
    assert_eq!(
        responses[0].response_result.as_ref().unwrap_err().code,
        REQUEST_FAILED
    );
    client.did_close("file:///synthetic-startup-0.fix");
    let closed = barrier(&mut client);
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].id, pending[0]);
    assert_eq!(closed[0].response_result.as_ref().unwrap(), &json!([]));
    let replacement = client.send_request(EXECUTE, list());
    assert!(barrier(&mut client).is_empty());
    client.answer_base_pull();
    let mut responses = client.responses_through_barrier(pending.last().unwrap());
    responses.extend(barrier(&mut client));
    assert_eq!(responses.len(), 64);
    assert!(responses.iter().any(|response| response.id == replacement));
    assert!(responses
        .iter()
        .all(|response| response.response_result.is_ok()));
    client.shutdown();
}

#[test]
fn custom_dictionary_selection_waits_for_the_first_pull_commit() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let selection = client.send_request(
        EXECUTE,
        json!({"command":"fix.useDictionary","arguments":["synthetic"]}),
    );
    assert!(barrier(&mut client).is_empty());
    client.answer(
        "workspace/configuration",
        json!([{"dictionaries":[{
            "name":"synthetic","transport":"FIX.4.4","application":"FIX.4.4"
        }]}]),
    );
    let responses = complete(&mut client, &selection);
    assert_eq!(responses.len(), 1);
    assert_eq!(
        responses[0].response_result.as_ref().unwrap()["name"],
        "synthetic"
    );
    client.shutdown();
}

#[test]
fn closing_and_reopening_does_not_retarget_a_queued_hover() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let uri = "file:///synthetic-reopened.fix";
    client.did_open_unready(uri, "8=FIX.4.4|35=0|", 1);
    let hover = client.send_request(
        "textDocument/hover",
        json!({"textDocument":{"uri":uri},"position":{"line":0,"character":2}}),
    );
    assert!(barrier(&mut client).is_empty());
    client.did_close(uri);
    client.did_open_unready(uri, "8=FIX.4.4|35=D|", 1);
    let responses = complete(&mut client, &hover);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].response_result.as_ref().unwrap(), &Value::Null);
    client.answer_base_pull();
    assert!(client.describe(uri).is_ok());
    assert!(barrier(&mut client).is_empty());
    client.shutdown();
}

#[test]
fn queued_resolve_preserves_stale_hint_and_lens_contracts() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let uri = "file:///synthetic-resolve.fix";
    client.did_open_unready(uri, "8=FIX.4.4|35=0|", 1);
    let hint = json!({"position":{"line":0,"character":2},"label":"FIX","data":{"uri":uri,"version":1,"line":0,"offset":2}});
    let hint_id = client.send_request("inlayHint/resolve", hint.clone());
    let lens_id = client.send_request("codeLens/resolve", json!({"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"data":{"uri":uri,"version":1,"line":0,"hash":"0"}}));
    assert!(barrier(&mut client).is_empty());
    client.did_change_full(uri, "8=FIX.4.4|35=D|", 2);
    let mut responses = barrier(&mut client);
    client.answer_base_pull();
    responses.extend(complete(&mut client, &lens_id));
    assert_eq!(responses.len(), 2);
    assert_eq!(
        responses
            .iter()
            .find(|response| response.id == hint_id)
            .unwrap()
            .response_result
            .as_ref()
            .unwrap(),
        &hint
    );
    assert_eq!(
        responses
            .iter()
            .find(|response| response.id == lens_id)
            .unwrap()
            .response_result
            .as_ref()
            .unwrap_err()
            .code,
        harness::CONTENT_MODIFIED
    );
    client.shutdown();
}

#[test]
fn malformed_document_requests_fail_without_waiting_and_cancellation_releases_waiters() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let uri = "file:///synthetic-cancel.fix";
    client.did_open_unready(uri, "8=FIX.4.4|35=0|", 1);
    assert_eq!(
        client.request("textDocument/hover", json!({"textDocument":{"uri":uri}})),
        Err(INVALID_PARAMS)
    );
    let id = RequestId::from("document-reuse".to_owned());
    client.send_request_with_id(
        id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":uri}}),
    );
    assert!(barrier(&mut client).is_empty());
    client.notify("$/cancelRequest", json!({"id":id}));
    let responses = complete(&mut client, &id);
    assert_eq!(responses.len(), 1);
    assert_eq!(
        responses[0].response_result.as_ref().unwrap_err().code,
        CANCELLED
    );
    client.send_request_with_id(
        id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":uri}}),
    );
    assert!(barrier(&mut client).is_empty());
    client.answer_base_pull();
    let responses = complete(&mut client, &id);
    assert_eq!(responses.len(), 1);
    assert!(responses[0].response_result.is_ok());
    client.shutdown();
}

#[test]
fn rejected_refresh_blocks_base_commands_but_keeps_ready_documents_usable() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer_base_pull();
    client.wait_base_ready();
    let uri = "file:///synthetic-rejected-refresh.fix";
    client.did_open(uri, "8=FIX.4.4|35=0|", 1);
    client.notify("workspace/didChangeConfiguration", json!({"settings":null}));
    client.answer(
        "workspace/configuration",
        json!([{"dictionaries":"invalid"}]),
    );
    assert!(barrier(&mut client).is_empty());
    assert_eq!(client.request(EXECUTE, list()), Err(REQUEST_FAILED));
    assert!(client
        .request(
            "textDocument/hover",
            json!({"textDocument":{"uri":uri},"position":{"line":0,"character":2}})
        )
        .is_ok());
    assert!(client.describe(uri).is_ok());
    client.shutdown();
}

#[test]
fn closing_a_document_finishes_queued_decode_overrides_and_lens_resolution() {
    for reopen in [false, true] {
        let mut client = TestClient::start();
        client.initialize_caps(TestClient::caps_with_pull());
        let uri = "file:///synthetic-closed-commands.fix";
        client.did_open_unready(uri, "8=FIX.4.4|35=0|", 1);
        let decode = client.send_request(
            EXECUTE,
            json!({
                "command":"fix.decode", "arguments":[{
                    "textDocument":{"uri":uri},
                    "ranges":[{"start":{"line":0,"character":0},"end":{"line":0,"character":8}}]
                }]
            }),
        );
        let overrides = client.send_request(
            EXECUTE,
            json!({
                "command":"fix.describeOverrides", "arguments":[uri]
            }),
        );
        let lens = client.send_request(
            "codeLens/resolve",
            json!({
                "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},
                "data":{"uri":uri,"version":1,"line":0,"hash":"0"}
            }),
        );
        assert!(barrier(&mut client).is_empty());
        if reopen {
            // Replacing the lifetime without didClose must have the same outcome.
            client.did_open_unready(uri, "8=FIX.4.4|35=D|", 1);
        } else {
            client.did_close(uri);
        }
        let responses = complete(&mut client, &lens);
        assert_eq!(responses.len(), 3);
        for (response, (id, code)) in responses.iter().zip([
            (&decode, REQUEST_FAILED),
            (&overrides, REQUEST_FAILED),
            (&lens, harness::CONTENT_MODIFIED),
        ]) {
            assert_eq!(&response.id, id);
            assert_eq!(response.response_result.as_ref().unwrap_err().code, code);
        }
        client.answer_base_pull();
        client.wait_base_ready();
        if reopen {
            assert!(client.describe(uri).is_ok());
        }
        assert!(
            barrier(&mut client).is_empty(),
            "no second completion after commit"
        );
        client.shutdown();
    }
}
