//! The one Stage 3 behaviour a client can observe: a broken summary template
//! is reported once and the server keeps working on the default.

mod harness;

use serde_json::json;

#[test]
fn a_bad_summary_template_warns_once_and_the_server_keeps_working() {
    let mut client = harness::TestClient::start();
    // `initialize_caps` already sends `initialized`; sending it again would
    // produce a second pull and make the first reply stale.
    client.initialize_caps(harness::TestClient::caps_with_pull());
    let bad = json!([{ "messageSummary": { "template": "{nope}" } }]);
    let (id, method, _) = client.recv_server_request();
    assert_eq!(method, "workspace/configuration");
    client.reply(id, bad.clone());

    client.did_open("file:///a.fix", "8=FIX.4.4|35=0|", 1);
    let (result, notes) = client.request_collecting(
        "textDocument/hover",
        harness::TestClient::hover_params("file:///a.fix", 0, 0),
    );
    assert!(result.is_ok(), "the server still answers");
    let warnings: Vec<_> = notes
        .iter()
        .filter(|(method, _)| method == "window/showMessage")
        .collect();
    assert_eq!(warnings.len(), 1, "exactly one warning: {notes:?}");
    let text = warnings[0].1["message"].as_str().unwrap();
    assert!(text.contains("template"), "{text}");

    // The same broken configuration pulled again must not warn again.
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": null }),
    );
    let (id, _, _) = client.recv_server_request();
    client.reply(id, bad);
    let (_, notes) = client.request_collecting(
        "textDocument/hover",
        harness::TestClient::hover_params("file:///a.fix", 0, 0),
    );
    assert!(
        !notes
            .iter()
            .any(|(method, _)| method == "window/showMessage"),
        "an unchanged problem is not reported twice: {notes:?}"
    );
}

#[test]
fn project_and_client_templates_roundtrip_while_modelines_cannot_replace_them() {
    for template in [
        r#"{54="1"?"{54.valueName?:\"UNKNOWN\"}":"other"}"#,
        "{54=1?{54.valueName?:UNKNOWN}:other}",
    ] {
        for project in [false, true] {
            for attempted in ["unset", "invalid", "'{35.valueName}'"] {
                let fixture = harness::Workspace::new("summary-template-sources");
                if project {
                    fixture.write(
                        ".venue-reader.toml",
                        &format!("[messageSummary]\ntemplate = '{template}'\n"),
                    );
                }
                let mut client = harness::TestClient::start();
                let mut settings =
                    json!({"messageSummary":{"details":true,"channels":{"inlayHints":true}}});
                if !project {
                    settings["messageSummary"]["template"] = json!(template);
                }
                fixture.initialize(&mut client, settings);
                let uri = fixture.uri("sample.fix");
                let text = format!("# fix: messageSummary.template={attempted} messageSummary.details=false\n8=FIX.4.4|35=D|54=1|\n");
                let report = client.did_open(&uri, &text, 1);
                assert_eq!(report["effective"]["messageSummary.template"], template);
                assert_eq!(report["effective"]["messageSummary.details"], false);
                assert!(report["warnings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|warning| warning["key"] == "messageSummary.template"
                        && warning["message"]
                            .as_str()
                            .unwrap()
                            .contains("not allowed in modelines")));
                let hints = client.request("textDocument/inlayHint", json!({"textDocument":{"uri":uri},"range":{"start":{"line":1,"character":0},"end":{"line":2,"character":0}}})).unwrap();
                let summary = hints
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|hint| hint["data"]["kind"] == "summary")
                    .unwrap();
                assert_eq!(harness::hint_labels(std::slice::from_ref(summary)), ["Buy"]);
                client.shutdown();
            }
        }
    }
}
