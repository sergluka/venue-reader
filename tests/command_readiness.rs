//! Document-free commands wait without blocking configuration or cancellation.

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
fn both_commands_wait_through_a_second_pull_in_either_notification_order() {
    for refresh_first in [true, false] {
        let mut client = TestClient::start();
        client.initialize_caps(TestClient::caps_with_pull());
        let (first, method, _) = client.recv_server_request();
        assert_eq!(method, "workspace/configuration");
        if refresh_first {
            client.notify("workspace/didChangeConfiguration", json!({"settings":null}));
            client.reply(first, json!([null]));
        } else {
            client.reply(first, json!([null]));
            client.notify("workspace/didChangeConfiguration", json!({"settings":null}));
        }
        let (second, method, _) = client.recv_server_request();
        assert_eq!(method, "workspace/configuration");
        let listing = client.send_request(EXECUTE, list());
        let description = client.send_request(EXECUTE, tag("1"));
        assert!(
            barrier(&mut client).is_empty(),
            "commands must remain pending"
        );
        client.reply(second, json!([null]));
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
            .contains("Buy"));
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
