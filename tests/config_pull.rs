mod harness;

use harness::{file_uri, frame, TestClient, Workspace, REQUEST_FAILED};
use serde_json::{json, Value};
use std::time::Duration;

fn application_message() -> String {
    frame("FIXT.1.1", "35=D|54=1|55=SYNTHETIC|")
}

fn assert_application_hover(
    client: &mut TestClient,
    uri: &str,
    message: &str,
    line: u32,
    expected_version_path: Option<&str>,
) {
    let column = message.find("54=").unwrap() as u32;
    let hover = client.hover(uri, line, column).unwrap().to_string();
    if let Some(version) = expected_version_path {
        assert!(hover.contains("**Buy**"), "{hover}");
        assert!(
            hover.contains(&format!("/fix-dictionary/{version}/")),
            "{hover}"
        );
    } else {
        assert!(hover.contains("Value: `1`"), "{hover}");
        assert!(!hover.contains("**Buy**"), "{hover}");
        assert!(!hover.contains("[FIX Reference"), "{hover}");
    }
}

#[test]
fn base_pull_is_sent_on_initialized() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let (id, method, params) = client.recv_server_request();
    assert_eq!(method, "workspace/configuration");
    assert_eq!(params, json!({ "items": [{ "section": "fix" }] }));
    client.reply(id, json!([{ "defaultApplVerID": "FIX.5.0" }]));
    client.did_open("untitled:base", "8=FIXT.1.1|35=0|", 1);
    assert_eq!(
        client.wait_ready("untitled:base")["effective"]["defaultApplVerID"],
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
    client.reply(id, json!([{ "defaultApplVerID": "FIX.4.2" }]));
    client.did_open("untitled:after", "35=0|", 1);
    assert_eq!(
        client.wait_ready("untitled:after")["effective"]["defaultApplVerID"],
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

#[test]
fn application_default_can_be_cleared_and_removed_version_key_stays_unknown() {
    let fixture = Workspace::new("application-default");
    let document = fixture.uri("orders.fix");
    let message = application_message();
    let mut client = TestClient::start();
    client.initialize_with(
        json!({"capabilities": {}, "initializationOptions": {"fix": {
            "defaultApplVerID": "FIX.5.0SP2", "defaultVersion": "FIX.4.2"
        }}}),
    );
    client.did_open(&document, &message, 1);
    let report = client.wait_ready(&document);
    assert_eq!(report["effective"]["defaultApplVerID"], "FIX.5.0SP2");
    assert!(report["effective"].get("defaultVersion").is_none());
    assert_application_hover(&mut client, &document, &message, 0, Some("5.0.SP2"));
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"fix":{"defaultApplVerID":null}}}),
    );
    let report = client.wait_effective(&document, "defaultApplVerID", Value::Null);
    assert_eq!(report["effective"]["defaultApplVerID"], Value::Null);
    assert_application_hover(&mut client, &document, &message, 0, None);
    client.did_change_full(
        &document,
        &format!("# fix: defaultApplVerID=FIX.5.0\n{message}"),
        2,
    );
    let report = client.wait_effective(&document, "defaultApplVerID", json!("FIX.5.0"));
    assert_eq!(report["effective"]["defaultApplVerID"], "FIX.5.0");
    assert_eq!(report["sources"]["defaultApplVerID"]["layer"], "modeline");
    assert_eq!(report["sources"]["defaultApplVerID"]["source"], document);
    assert_application_hover(&mut client, &document, &message, 1, Some("5.0"));
    client.shutdown();
}

#[test]
fn project_and_user_application_defaults_change_decoding_and_reject_invalid_files() {
    let fixture = Workspace::new("application-file-defaults");
    let user_file = fixture.write("personal/fix-lsp.toml", "defaultApplVerID = 'FIX.5.0'\n");
    let mut client = TestClient::start_with_user_file(Some(user_file.clone()));
    fixture.initialize(&mut client, json!({}));
    let document = fixture.uri("orders.fix");
    let message = application_message();
    client.did_open(&document, &message, 1);
    let report = client.wait_effective(&document, "defaultApplVerID", json!("FIX.5.0"));
    assert_eq!(report["sources"]["defaultApplVerID"]["layer"], "userFile");
    assert_eq!(
        report["sources"]["defaultApplVerID"]["source"],
        file_uri(&user_file.canonicalize().unwrap())
    );
    assert_application_hover(&mut client, &document, &message, 0, Some("5.0"));

    let project_file = fixture.write(".fix-lsp.toml", "defaultApplVerID = 'FIX.5.0SP1'\n");
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"fix":{}}}),
    );
    let report = client.wait_effective(&document, "defaultApplVerID", json!("FIX.5.0SP1"));
    assert_eq!(report["sources"]["defaultApplVerID"]["layer"], "project");
    assert_eq!(
        report["sources"]["defaultApplVerID"]["source"],
        file_uri(&project_file.canonicalize().unwrap())
    );
    assert_application_hover(&mut client, &document, &message, 0, Some("5.0.SP1"));

    std::fs::write(
        &project_file,
        "defaultApplVerID = '9'\nhover.enabled = false\n",
    )
    .unwrap();
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"fix":{}}}),
    );
    let report = client.wait_effective(&document, "defaultApplVerID", json!("FIX.5.0"));
    assert_eq!(report["sources"]["defaultApplVerID"]["layer"], "userFile");
    assert_eq!(report["effective"]["hover.enabled"], true);
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| {
                warning["source"]["layer"] == "project"
                    && warning["source"]["source"]
                        == file_uri(&project_file.canonicalize().unwrap())
                    && warning["message"]
                        .as_str()
                        .unwrap()
                        .contains("defaultApplVerID")
            }),
        "{report}"
    );
    assert_application_hover(&mut client, &document, &message, 0, Some("5.0"));

    std::fs::remove_file(project_file).unwrap();
    std::fs::write(&user_file, "defaultApplVerID = 'FIX.5.0SP2'\n").unwrap();
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"fix":{}}}),
    );
    let report = client.wait_effective(&document, "defaultApplVerID", json!("FIX.5.0SP2"));
    assert_eq!(report["sources"]["defaultApplVerID"]["layer"], "userFile");
    assert_application_hover(&mut client, &document, &message, 0, Some("5.0.SP2"));

    std::fs::write(
        &user_file,
        "defaultApplVerID = '9'\nhover.enabled = false\n",
    )
    .unwrap();
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"fix":{}}}),
    );
    let report = client.wait_effective(&document, "defaultApplVerID", Value::Null);
    assert_eq!(report["sources"]["defaultApplVerID"]["layer"], "default");
    assert_eq!(report["effective"]["hover.enabled"], true);
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| {
                warning["source"]["layer"] == "userFile"
                    && warning["source"]["source"] == file_uri(&user_file.canonicalize().unwrap())
                    && warning["message"]
                        .as_str()
                        .unwrap()
                        .contains("defaultApplVerID")
            }),
        "{report}"
    );
    assert_application_hover(&mut client, &document, &message, 0, None);
    client.shutdown();
}

#[test]
fn invalid_client_application_defaults_keep_committed_decoding_until_a_valid_update() {
    let fixture = Workspace::new("invalid-client-application-default");
    let mut client = fixture.client(json!({"defaultApplVerID":"FIX.5.0SP2"}));
    let document = fixture.uri("orders.fix");
    let message = application_message();
    client.did_open(&document, &message, 1);
    for invalid in [json!("9"), json!("FIXT.1.1"), json!(""), json!(9)] {
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"fix":{
                "defaultApplVerID": invalid, "hover":{"enabled":false}
            }}}),
        );
        let rejection = client.wait_for_log("configuration rejected");
        assert!(
            rejection["message"]
                .as_str()
                .unwrap()
                .contains("defaultApplVerID"),
            "{rejection}"
        );
        let report = client.describe(&document).unwrap();
        assert_eq!(report["effective"]["defaultApplVerID"], "FIX.5.0SP2");
        assert_eq!(report["effective"]["hover.enabled"], true);
        assert_application_hover(&mut client, &document, &message, 0, Some("5.0.SP2"));
    }
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"fix":{
            "defaultApplVerID":"FIX.5.0SP1"
        }}}),
    );
    client.wait_effective(&document, "defaultApplVerID", json!("FIX.5.0SP1"));
    assert_application_hover(&mut client, &document, &message, 0, Some("5.0.SP1"));
    client.shutdown();
}

#[test]
fn invalid_modeline_application_default_warns_and_keeps_global_decoding() {
    let fixture = Workspace::new("invalid-modeline-application-default");
    let mut client = fixture.client(json!({"defaultApplVerID":"FIX.5.0SP2"}));
    let document = fixture.uri("orders.fix");
    let message = application_message();
    let report = client.did_open(
        &document,
        &format!("# fix: defaultApplVerID=9\n{message}"),
        1,
    );
    assert_eq!(report["effective"]["defaultApplVerID"], "FIX.5.0SP2");
    assert_eq!(report["sources"]["defaultApplVerID"]["layer"], "client");
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| {
                warning["key"] == "defaultApplVerID"
                    && warning["source"]["layer"] == "modeline"
                    && warning["source"]["source"] == document
            }),
        "{report}"
    );
    assert_application_hover(&mut client, &document, &message, 1, Some("5.0.SP2"));
    client.shutdown();
}
