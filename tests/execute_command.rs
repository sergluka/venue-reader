//! Execute-command discovery, argument rejection and document-free references.

mod harness;

use harness::{TestClient, INVALID_PARAMS, REQUEST_FAILED};
use serde_json::json;

#[test]
fn commands_are_advertised_without_decode_aliases() {
    let mut client = TestClient::start();
    let result = client.initialize(&["utf-8"]);
    assert_eq!(
        result["capabilities"]["executeCommandProvider"]["commands"],
        json!([
            "fix.decode",
            "fix.useDictionary",
            "fix.describeOverrides",
            "fix.describeTag"
        ]),
        "{result}"
    );
    client.shutdown();
}

#[test]
fn unknown_commands_and_the_removed_decode_name_are_refused() {
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    for command in ["fix.notACommand", "fix.decodeRanges"] {
        assert_eq!(
            client.request(
                "workspace/executeCommand",
                json!({"command":command,"arguments":[]})
            ),
            Err(INVALID_PARAMS)
        );
    }
    client.shutdown();
}

#[test]
fn positional_decode_arguments_are_refused() {
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    let uri = "file:///a.fix";
    client.did_open(uri, "8=FIX.4.4|35=0|", 1);
    for arguments in [
        json!([]),
        json!([uri]),
        json!([uri, []]),
        json!([uri,[{"start":{"line":0,"character":10},"end":{"line":0,"character":14}}]]),
    ] {
        assert_eq!(
            client.request(
                "workspace/executeCommand",
                json!({"command":"fix.decode","arguments":arguments})
            ),
            Err(INVALID_PARAMS),
            "{arguments}"
        );
    }
    client.shutdown();
}

/// `fix.describeTag` is a workspace command: `initialize`, `initialized`, the
/// configuration answer, and no `didOpen` anywhere.
#[test]
fn a_tag_is_described_with_no_document_open() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer_base_pull();
    let text = client.wait_base_ready();
    assert!(text.starts_with("**RejectText** (1328) — String"), "{text}");
    let valued = client
        .request(
            "workspace/executeCommand",
            json!({"command": "fix.describeTag", "arguments": [54, "1"]}),
        )
        .expect("described");
    assert!(
        valued
            .as_str()
            .expect("a string")
            .contains("Value: `1` — **Buy**"),
        "{valued}"
    );
    client.shutdown();
}

/// Until the configuration answer commits, the reference-link template is a
/// guess the command declines to answer under; the pull it is waiting on is
/// still there to answer afterwards.
#[test]
fn a_description_before_the_configuration_answer_is_refused() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let early = client.request(
        "workspace/executeCommand",
        json!({"command": "fix.describeTag", "arguments": [1328]}),
    );
    assert_eq!(early, Err(REQUEST_FAILED));
    client.answer_base_pull();
    client.wait_base_ready();
    client.shutdown();
}

/// A client that offers no `workspace/configuration` still commits a base
/// configuration, so the command becomes answerable without any pull.
#[test]
fn a_client_without_configuration_support_still_gets_a_description() {
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    client.wait_base_ready();
    client.shutdown();
}
