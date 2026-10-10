mod harness;

use harness::{Workspace, REQUEST_FAILED};
use serde_json::json;
use std::time::{Duration, Instant};

#[test]
fn rejected_project_dictionary_name_is_not_opened_as_a_path() {
    let fixture = Workspace::new("review-dictionary");
    fixture.write("invalid.xml", "<fix>");
    fixture.write(
        ".fix-lsp.toml",
        "dictionary = 'venue'\n[[dictionaries]]\nname = 'venue'\npath = 'invalid.xml'\n",
    );
    let mut client = fixture.client(json!({
        "allowDictionaryPath":true,"diagnostics":{"enabled":false}
    }));
    let document = fixture.uri("orders.fix");
    let report = client.did_open(&document, "8=FIX.4.4|35=D|", 1);
    assert!(report["effective"]["dictionary"].is_null());
    assert!(report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| { warning["key"] == "dictionaries" }));
    assert!(report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| {
            warning["key"] == "dictionary" && warning["message"].as_str().unwrap().contains("venue")
        }));
    assert!(client.hover(&document, 0, 0).is_ok());
    client.shutdown();
}

#[test]
fn session_dictionary_commands_preserve_project_load_warnings_until_refresh() {
    let fixture = Workspace::new("review-dictionary");
    fixture.write("bad.xml", "<fix>");
    fixture.write(
        ".fix-lsp.toml",
        "[[dictionaries]]\nname = 'venue'\npath = 'bad.xml'\n",
    );
    let settings = json!({"allowDictionaryPath":true,"diagnostics":{"enabled":false}});
    let mut client = fixture.client(settings.clone());
    let document = fixture.uri("orders.fix");
    let original = client.did_open(&document, "8=FIX.4.4|35=D|", 1)["warnings"].clone();
    assert!(original.as_array().unwrap().iter().any(|warning| {
        warning["key"] == "dictionaries" && warning["message"].as_str().unwrap().contains("bad.xml")
    }));
    // Commands reuse project definitions, even after the file has been fixed.
    fixture.write(
        "bad.xml",
        "<fix major='4' minor='4'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields><field number='8' name='BeginString' type='STRING'/></fields></fix>",
    );
    for selected in ["FIX.4.4", "FIX.4.2"] {
        client
            .request(
                "workspace/executeCommand",
                json!({
                    "command":"fix.useDictionary","arguments":[selected]
                }),
            )
            .unwrap();
        assert_eq!(client.describe(&document).unwrap()["warnings"], original);
    }
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"fix":settings}}),
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if client.describe(&document).unwrap()["warnings"]
            .as_array()
            .unwrap()
            .is_empty()
        {
            break;
        }
        assert!(Instant::now() < deadline, "configuration did not refresh");
        std::thread::yield_now();
    }
    client.shutdown();
}

#[test]
fn rejected_global_dictionary_command_fails_without_terminating_the_server() {
    let fixture = Workspace::new("review-dictionary");
    fixture.write("invalid.xml", "<fix>");
    let mut client = fixture.client(json!({
        "allowDictionaryPath":true,"diagnostics":{"enabled":false},
        "dictionaries":[{"name":"venue","path":"invalid.xml"}]
    }));
    let document = fixture.uri("orders.fix");
    client.did_open(&document, "8=FIX.4.4|35=D|", 1);
    assert_eq!(
        client.request(
            "workspace/executeCommand",
            json!({"command":"fix.useDictionary","arguments":["venue"]})
        ),
        Err(REQUEST_FAILED)
    );
    assert!(client.describe(&document).unwrap()["effective"]["dictionary"].is_null());
    client.shutdown();
}

#[test]
fn successful_bottom_modeline_selector_reports_its_actual_line() {
    let fixture = Workspace::new("review-dictionary");
    let mut client = fixture.client(json!({"diagnostics":{"enabled":false}}));
    let document = fixture.uri("orders.fix");
    client.did_open(
        &document,
        &format!(
            "{}# fix: dictionary=FIX.4.4",
            "8=FIX.4.4|35=D|\n".repeat(11)
        ),
        1,
    );
    let report = client.describe(&document).unwrap();
    assert_eq!(report["effective"]["dictionary"], "FIX.4.4");
    assert_eq!(report["sources"]["dictionary"]["layer"], "modeline");
    assert_eq!(report["sources"]["dictionary"]["source"], document);
    assert_eq!(
        report["sources"]["dictionary"]["range"]["start"],
        json!({"line":11,"character":7})
    );
    client.shutdown();
}

#[test]
fn rejected_bottom_modeline_selector_diagnostic_uses_its_actual_range() {
    let fixture = Workspace::new("review-dictionary");
    let mut client = fixture.client(json!({"diagnostics":{"enabled":false}}));
    let document = fixture.uri("orders.fix");
    client.did_open(
        &document,
        &format!(
            "{}# fix: dictionary=FIX.3.0",
            "8=FIX.4.4|35=D|\n".repeat(11)
        ),
        1,
    );
    let report = client.describe(&document).unwrap();
    let warning = report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|warning| warning["key"] == "dictionary")
        .unwrap();
    assert_eq!(warning["source"]["layer"], "modeline");
    assert_eq!(warning["source"]["source"], document);
    assert_eq!(
        warning["source"]["range"]["start"],
        json!({"line":11,"character":7})
    );
    let published = client.wait_for("textDocument/publishDiagnostics");
    let diagnostic = published["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|diagnostic| diagnostic["code"] == "modeline")
        .unwrap();
    assert_eq!(
        diagnostic["range"]["start"],
        json!({"line":11,"character":7})
    );
    client.shutdown();
}

#[test]
fn unknown_global_rule_is_omitted_and_reported_as_an_error() {
    let fixture = Workspace::new("review-dictionary");
    let mut client = fixture.client(json!({
        "diagnostics":{"enabled":false,"rules":{"integritY":false}}
    }));
    let document = fixture.uri("orders.fix");
    let report = client.did_open(&document, "8=FIX.4.4|35=D|", 1);
    assert!(report["effective"]["diagnostics.rules.integritY.enabled"].is_null());
    let warning = report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|warning| warning["message"].as_str().unwrap().contains("integritY"))
        .unwrap();
    assert_eq!(warning["severity"], "error");
    assert_eq!(warning["source"]["layer"], "client");
    client.wait_for_log("integritY");
    client.shutdown();
}

#[test]
fn unknown_rule_with_only_opaque_options_is_omitted_and_reported() {
    let fixture = Workspace::new("review-dictionary");
    let mut client = fixture.client(json!({
        "diagnostics":{"enabled":false,"rules":{"futureRule":{"autoCorrect":true}}}
    }));
    let document = fixture.uri("orders.fix");
    let report = client.did_open(&document, "8=FIX.4.4|35=D|", 1);
    assert!(report["effective"]["diagnostics.rules.futureRule.autoCorrect"].is_null());
    assert!(report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| {
            warning["message"].as_str().unwrap().contains("futureRule")
                && warning["severity"] == "error"
                && warning["source"]["layer"] == "client"
        }));
    client.shutdown();
}

#[test]
fn unknown_modeline_rule_is_omitted_and_diagnosed_at_its_source() {
    let fixture = Workspace::new("review-dictionary");
    let mut client = fixture.client(json!({"diagnostics":{"enabled":false}}));
    let document = fixture.uri("orders.fix");
    client.did_open(
        &document,
        "8=FIX.4.4|35=D|\n# fix: diagnostics.rules.integritY=false",
        1,
    );
    let report = client.describe(&document).unwrap();
    assert!(report["effective"]["diagnostics.rules.integritY.enabled"].is_null());
    let warning = report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|warning| warning["message"].as_str().unwrap().contains("integritY"))
        .unwrap();
    assert_eq!(warning["severity"], "error");
    assert_eq!(warning["source"]["layer"], "modeline");
    assert_eq!(warning["source"]["source"], document);
    assert_eq!(
        warning["source"]["range"]["start"],
        json!({"line":1,"character":7})
    );
    let published = client.wait_for("textDocument/publishDiagnostics");
    assert!(published["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|diagnostic| {
            diagnostic["message"]
                .as_str()
                .unwrap()
                .contains("integritY")
                && diagnostic["range"]["start"]["line"] == 1
        }));
    client.shutdown();
}
