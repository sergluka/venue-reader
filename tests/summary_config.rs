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
