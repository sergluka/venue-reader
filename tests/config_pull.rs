mod harness;

use harness::{TestClient, REQUEST_FAILED};
use serde_json::{json, Value};
use std::time::Duration;

#[test]
fn base_pull_is_sent_on_initialized() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let (id, method, params) = client.recv_server_request();
    assert_eq!(method, "workspace/configuration");
    assert_eq!(params, json!({ "items": [{ "section": "fix" }] }));
    client.reply(id, json!([{ "defaultVersion": "FIX.5.0" }]));
    client.did_open("untitled:base", "8=FIX.5.0|35=0|", 1);
    assert_eq!(
        client.wait_ready("untitled:base")["effective"]["defaultVersion"],
        "FIX.5.0"
    );
    client.shutdown();
}

#[test]
fn only_bundled_dictionary_names_are_available_before_the_first_global_commit() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let (pull_id, method, _) = client.recv_server_request();
    assert_eq!(method, "workspace/configuration");

    let bundled = client
        .request(
            "workspace/executeCommand",
            json!({"command":"fix.useDictionary","arguments":["FIX.4.2"]}),
        )
        .unwrap();
    assert_eq!(bundled["name"], "FIX.4.2");
    assert_eq!(
        client.request(
            "workspace/executeCommand",
            json!({"command":"fix.useDictionary","arguments":["venue"]}),
        ),
        Err(REQUEST_FAILED)
    );

    client.reply(pull_id, json!([{}]));
    client.shutdown();
}

#[test]
fn document_events_and_describe_never_request_scoped_configuration() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer_base_pull();
    for uri in ["file:///a.fix", "file:///b.fix", "untitled:buffer"] {
        client.did_open(uri, "8=FIX.4.4|35=0|", 1);
        client.did_change_full(uri, "8=FIX.4.4|35=1|", 2);
        client.wait_ready(uri);
    }
    let methods = client.drain_server_methods(Duration::from_millis(20));
    assert!(
        !methods
            .iter()
            .any(|method| method == "workspace/configuration"),
        "{methods:?}"
    );
    client.shutdown();
}

#[test]
fn did_change_configuration_null_triggers_only_global_repull() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer_base_pull();
    client.did_open("untitled:first", "8=FIX.4.4|35=0|", 1);
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": null }),
    );
    let (id, method, params) = client.recv_server_request();
    assert_eq!(method, "workspace/configuration");
    assert_eq!(params, json!({ "items": [{ "section": "fix" }] }));
    client.reply(id, json!([{ "defaultVersion": "FIX.4.2" }]));
    client.did_open("untitled:after", "35=0|", 1);
    assert_eq!(
        client.wait_ready("untitled:after")["effective"]["defaultVersion"],
        "FIX.4.2"
    );
    client.shutdown();
}

#[test]
fn closing_a_document_while_the_global_reply_is_pending_does_not_resurrect_it() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let (id, _, _) = client.recv_server_request();
    client.did_open_unready("file:///z.fix", "8=FIX.4.4|35=0|", 1);
    client.did_close("file:///z.fix");
    client.reply(id, json!([{}]));
    client.did_open("untitled:barrier", "8=FIX.4.4|35=0|", 1);
    assert!(client.hover("file:///z.fix", 0, 0).unwrap().is_null());
    assert_eq!(
        client.request(
            "workspace/executeCommand",
            json!({
                "command": "fix.describeOverrides", "arguments": ["file:///z.fix"]
            })
        ),
        Err(REQUEST_FAILED)
    );
    client.shutdown();
}

#[test]
fn a_required_client_configuration_error_is_fatal() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.refuse("workspace/configuration");
    let outcome = client.wait_for_exit();
    assert!(
        matches!(outcome, fix_lsp::server::ServerExit::Fatal(message) if message.contains("workspace/configuration"))
    );
}

#[test]
fn unknown_document_override_client_object_has_no_effect() {
    let mut client = TestClient::start();
    client.initialize_with(
        json!({ "capabilities": {}, "initializationOptions": { "fix": {
        "documentOverrides": { "hover": { "enabled": false } }
    } } }),
    );
    client.did_open("untitled:ignored", "8=FIX.4.4|35=0|", 1);
    let report = client.wait_ready("untitled:ignored");
    assert_eq!(report["effective"]["hover.enabled"], Value::Bool(true));
    assert!(!client.hover("untitled:ignored", 0, 0).unwrap().is_null());
    client.shutdown();
}
