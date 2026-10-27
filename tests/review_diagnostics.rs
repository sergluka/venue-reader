mod harness;

use std::fs;
use std::time::Duration;

use harness::{file_uri, TestClient, Workspace};
use serde_json::{json, Value};

const BAD: &str = "8=FIX.4.4|9=7|35=0|34=1|49=A|56=B|10=001|";
const SOUND: &str =
    "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";
const QUIET: Duration = Duration::from_millis(400);

fn publishes(notes: &[(String, Value)]) -> impl Iterator<Item = &Value> {
    notes
        .iter()
        .filter(|(method, _)| method == "textDocument/publishDiagnostics")
        .map(|(_, params)| params)
}

#[test]
fn changed_modeline_never_republishes_diagnostics_from_removed_lines() {
    let fixture = Workspace::new("review-stale-diagnostics");
    let document = fixture.uri("orders.fix");
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    client.did_open(&document, &[BAD; 11].join("\n"), 1);
    let initial = client.wait_for("textDocument/publishDiagnostics");
    assert!(initial["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|diagnostic| diagnostic["range"]["start"]["line"] == 10));
    client.drain_notifications(QUIET);

    client.did_change_full(
        &document,
        &format!("# fix: unknownSetting=true\n{SOUND}"),
        2,
    );
    let mut reports = Vec::new();
    loop {
        let report = client.wait_for("textDocument/publishDiagnostics");
        let has_modeline = report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "modeline");
        reports.push(report);
        if has_modeline {
            break;
        }
    }
    let notes = client.drain_notifications(QUIET);
    reports.extend(publishes(&notes).cloned());
    for report in reports {
        assert_eq!(report["version"], 2);
        for diagnostic in report["diagnostics"].as_array().unwrap() {
            assert!(diagnostic["range"]["start"]["line"].as_u64().unwrap() < 2);
            assert_ne!(diagnostic["code"], "bodyLength");
            assert_ne!(diagnostic["code"], "checkSum");
        }
    }
    client.shutdown();
}

#[test]
fn configuration_warnings_never_republish_the_previous_rule_severity() {
    let fixture = Workspace::new("review-diagnostics");
    let config = fixture.write(".venue-reader.toml", "");
    let document = fixture.uri("orders.fix");
    let mut client = TestClient::start();
    client.initialize_with(json!({"rootUri":file_uri(&fixture.0),"capabilities":{}}));
    client.did_open(&document, BAD, 1);
    let initial = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(initial["diagnostics"][0]["severity"], 1);
    client.drain_notifications(QUIET);

    fs::write(
        &config,
        "[diagnostics.rules]\nintegritY = false\nintegrity = { severity = 'hint' }\n",
    )
    .unwrap();
    client.notify("workspace/didChangeConfiguration", json!({"settings":{}}));
    let notes = client.drain_notifications(QUIET);
    let reports: Vec<_> = publishes(&notes).collect();
    assert!(reports.iter().any(|report| report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|diagnostic| diagnostic["code"] == "projectConfig")));
    let mut found_integrity = false;
    for report in reports {
        assert_eq!(report["version"], 1);
        for diagnostic in report["diagnostics"].as_array().unwrap() {
            if diagnostic["code"] == "bodyLength" || diagnostic["code"] == "checkSum" {
                found_integrity = true;
                assert_eq!(diagnostic["severity"], 4, "{diagnostic}");
            }
        }
    }
    assert!(found_integrity);
    client.shutdown();
}
