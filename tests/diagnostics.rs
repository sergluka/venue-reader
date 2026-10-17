//! Diagnostics end to end, through the generic in-process client.
//!
//! At this level rather than against the worker, because the paths that matter
//! most — `diagnostics.enabled: false`, a URI with an empty path, a document
//! that closes while a walk is in flight — never produce a job at all, so a
//! worker-only test cannot reach them.
//!
//! What a rule reports, and over which span, belongs to its own unit tests;
//! here the question is whether the notification arrives, carries the right
//! version, and stops arriving when it should.

mod harness;

use std::time::Duration;

use harness::{TestClient, Workspace, PROGRESS_CREATE};
use serde_json::{json, Value};

/// One BodyLength problem and one CheckSum problem; the same line as
/// `tests/fixtures/validation/bad-both.fix`, inlined so this file reads whole.
const BAD_BOTH: &str = "8=FIX.4.4|9=7|35=D|34=2|49=CLIENT1|56=BROKER1|\
52=20251026-09:00:02.000|11=ORDER123|21=1|55=BTCUSD|54=1|38=10|40=2|44=65000|59=0|\
60=20251026-09:00:02.000|10=001|";
const SOUND: &str =
    "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";
const URI: &str = "file:///a.fix";

/// Long enough that the client's own quiet period cannot be mistaken for the
/// server's; the debounce is 80 ms.
const QUIET: Duration = Duration::from_millis(400);

fn open(client: &mut TestClient, text: &str) {
    client.initialize(&["utf-8"]);
    client.did_open(URI, text, 1);
}

fn codes(params: &Value) -> Vec<String> {
    params["diagnostics"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|d| d["code"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The last publish for `uri` among everything received, since a walk may
/// publish more than once.
fn last_publish(notes: &[(String, Value)], uri: &str) -> Option<Value> {
    notes
        .iter()
        .filter(|(method, params)| {
            method == "textDocument/publishDiagnostics" && params["uri"] == uri
        })
        .map(|(_, params)| params.clone())
        .next_back()
}

#[test]
fn opening_a_document_publishes_its_problems() {
    let mut client = TestClient::start();
    open(&mut client, BAD_BOTH);
    let params = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(params["uri"], URI);
    assert_eq!(params["version"], 1);
    let mut found = codes(&params);
    found.sort();
    assert_eq!(found, vec!["bodyLength", "checkSum"]);
    let first = &params["diagnostics"][0];
    assert_eq!(first["source"], "fix");
    assert_eq!(
        first["data"]["rule"], "integrity",
        "the rule that governs it, not the code"
    );
    client.shutdown();
}

/// An empty set rather than silence: with push diagnostics a client cannot
/// otherwise tell "validated, clean" from "not looked at yet". Sent once —
/// a second walk over an unchanged document repeats nothing.
#[test]
fn a_sound_document_is_published_as_clean_exactly_once() {
    let mut client = TestClient::start();
    open(&mut client, SOUND);
    let first = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(first["version"], 1);
    assert_eq!(codes(&first), Vec::<String>::new());

    client.did_change_full(URI, SOUND, 2);
    let notes = client.drain_notifications(QUIET);
    assert_eq!(
        last_publish(&notes, URI),
        None,
        "the set did not change, so nothing was sent"
    );
    client.shutdown();
}

#[test]
fn an_edit_republishes_at_the_new_version() {
    let mut client = TestClient::start();
    open(&mut client, BAD_BOTH);
    let first = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(first["version"], 1);

    client.did_change_full(URI, SOUND, 2);
    let notes = client.drain_notifications(QUIET);
    let last = last_publish(&notes, URI).expect("the fix is published too");
    assert_eq!(last["version"], 2);
    assert_eq!(codes(&last), Vec::<String>::new(), "the problems are gone");
    client.shutdown();
}

#[test]
fn a_rejected_edit_changes_nothing() {
    let mut client = TestClient::start();
    open(&mut client, BAD_BOTH);
    let _ = client.wait_for("textDocument/publishDiagnostics");

    // Version 1 again: stale, so `apply_changes` rejects it and the document
    // did not move.
    client.did_change_full(URI, SOUND, 1);
    let notes = client.drain_notifications(QUIET);
    assert_eq!(
        last_publish(&notes, URI),
        None,
        "a rejected change is not a change"
    );
    client.shutdown();
}

#[test]
fn closing_a_document_clears_its_marks_once() {
    let mut client = TestClient::start();
    open(&mut client, BAD_BOTH);
    let _ = client.wait_for("textDocument/publishDiagnostics");

    client.did_close(URI);
    let notes = client.drain_notifications(QUIET);
    let publishes: Vec<&(String, Value)> = notes
        .iter()
        .filter(|(m, p)| m == "textDocument/publishDiagnostics" && p["uri"] == URI)
        .collect();
    assert_eq!(publishes.len(), 1, "{publishes:?}");
    assert_eq!(codes(&publishes[0].1), Vec::<String>::new());
    client.shutdown();
}

#[test]
fn a_document_with_no_path_is_never_published_for() {
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    client.did_open_unready("file://", BAD_BOTH, 1);
    let notes = client.drain_notifications(QUIET);
    assert!(
        last_publish(&notes, "file://").is_none(),
        "an unnamed buffer is not a document"
    );
    client.shutdown();
}

#[test]
fn turning_diagnostics_off_clears_what_was_published() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer_base_pull();
    client.did_open(URI, BAD_BOTH, 1);
    let first = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(codes(&first).len(), 2);

    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": null }),
    );
    let (id, method, _) = client.recv_server_request();
    assert_eq!(method, "workspace/configuration");
    client.reply(id, json!([{ "diagnostics": { "enabled": false } }]));

    let notes = client.drain_notifications(QUIET);
    let last = last_publish(&notes, URI).expect("the marks are cleared, not abandoned");
    assert_eq!(codes(&last), Vec::<String>::new());
    client.shutdown();
}

#[test]
fn a_severity_change_republishes_at_the_same_version() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer_base_pull();
    client.did_open(URI, BAD_BOTH, 1);
    let first = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(first["diagnostics"][0]["severity"], 1, "error by default");

    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": null }),
    );
    let (id, _, _) = client.recv_server_request();
    client.reply(
        id,
        json!([{ "diagnostics": { "rules": { "integrity": { "severity": "hint" } } } }]),
    );

    let notes = client.drain_notifications(QUIET);
    let last = last_publish(&notes, URI).expect("republished");
    assert_eq!(last["version"], 1, "the document did not move");
    let regraded = last["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "bodyLength")
        .expect("still reported");
    assert_eq!(regraded["severity"], 4, "hint");
    client.shutdown();
}

#[test]
fn the_cap_says_how_many_it_left_out() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer_base_pull();
    // Three broken lines, room to publish one.
    let text = [BAD_BOTH, BAD_BOTH, BAD_BOTH].join("\n");
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": null }),
    );
    let (id, _, _) = client.recv_server_request();
    client.reply(id, json!([{ "limits": { "diagnosticsPerDocument": 1 } }]));
    client.did_open(URI, &text, 1);

    let notes = client.drain_notifications(QUIET);
    let last = last_publish(&notes, URI).expect("published");
    let mut found = codes(&last);
    found.sort();
    assert_eq!(found.len(), 2, "one problem plus the count: {found:?}");
    assert!(found.contains(&"truncated".to_owned()), "{found:?}");
    let more = last["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "truncated")
        .unwrap();
    assert_eq!(more["message"], "5 more problems not shown");

    // The diagnostic marks the file; the log message names the setting that
    // would show the rest. A user looking at a document that stops being
    // annotated halfway down has no other way to learn why.
    let logged = notes
        .iter()
        .filter(|(method, _)| method == "window/logMessage")
        .map(|(_, params)| params.clone())
        .find(|params| {
            params["message"]
                .as_str()
                .is_some_and(|text| text.contains("diagnosticsPerDocument"))
        })
        .expect("the cap is reported to the client's log");
    assert_eq!(
        logged["type"], 3,
        "info: a log too large for the cap is ordinary, not a defect"
    );
    let text = logged["message"].as_str().unwrap();
    assert!(text.contains("showing 1 problems of 6"), "{text}");
    assert!(text.contains("diagnosticsPerDocument"), "{text}");
    client.shutdown();
}

/// Answer the server's pending configuration pull with `settings`.
fn answer(client: &mut TestClient, settings: Value) {
    let (id, _, _) = client.recv_server_request();
    client.reply(id, json!([settings]));
}

/// Announce a configuration change and answer the pull it causes.
fn pull(client: &mut TestClient, settings: Value) {
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": null }),
    );
    answer(client, settings);
}

/// `(code, severity, start line)` of every diagnostic in a publish.
fn shown(publish: &Value) -> Vec<(String, u64, u64)> {
    publish["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|diagnostic| {
            (
                diagnostic["code"].as_str().unwrap().to_owned(),
                diagnostic["severity"].as_u64().unwrap(),
                diagnostic["range"]["start"]["line"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// The message of the `code` diagnostic in a publish that mentions `needle`.
fn message_on(publish: &Value, code: &str, needle: &str) -> String {
    publish["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|diagnostic| diagnostic["code"] == code)
        .map(|diagnostic| diagnostic["message"].as_str().unwrap().to_owned())
        .find(|message| message.contains(needle))
        .unwrap_or_else(|| panic!("{code} on {needle}: {publish}"))
}

/// The message of the last diagnostic, which the cap's `truncated` always is.
fn truncated_message(publish: &Value) -> &str {
    publish["diagnostics"].as_array().unwrap().last().unwrap()["message"]
        .as_str()
        .unwrap()
}

/// Findings regraded to hint fill the cap first; the errors further down still
/// take it, and what they displaced is counted, not lost.
#[test]
fn the_cap_keeps_the_most_severe_problems() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer_base_pull();
    // Two fragments reported by `beginString`, then one line with two errors.
    let text = ["55=BTCUSD|", "55=BTCUSD|", BAD_BOTH].join("\n");
    pull(
        &mut client,
        json!({
            "limits": { "diagnosticsPerDocument": 2 },
            "diagnostics": { "rules": { "beginString": { "severity": "hint" } } },
        }),
    );
    client.did_open(URI, &text, 1);

    let notes = client.drain_notifications(QUIET);
    let last = last_publish(&notes, URI).expect("published");
    assert_eq!(
        shown(&last),
        [
            ("bodyLength".to_owned(), 1, 2),
            ("checkSum".to_owned(), 1, 2),
            ("truncated".to_owned(), 3, 0),
        ]
    );
    assert_eq!(truncated_message(&last), "2 more problems not shown");
    client.shutdown();
}

/// A desk's FIX.4.4 dictionary that defines Symbol(55) and nothing else.
const DESK_44: &str = "<fix type='FIX' major='4' minor='4' servicepack='0'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields>\
<field number='55' name='Symbol' type='STRING'/></fields></fix>";

/// Side(54) is standard FIX.4.4 and missing from `DESK_44`.
const DESK_LINE: &str = "8=FIX.4.4|35=0|55=X|54=1|";

/// Settings pulled in turn; what `unknownTag` says about Side(54) after each.
fn messages_under(settings: &[Value]) -> Vec<String> {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let mut said = Vec::new();
    for (at, fix) in settings.iter().enumerate() {
        if at > 0 {
            pull(&mut client, fix.clone());
        } else {
            answer(&mut client, fix.clone());
        }
        if at == 0 {
            client.did_open(URI, DESK_LINE, 1);
        }
        let last = last_publish(&client.drain_notifications(QUIET), URI).expect("published");
        said.push(message_on(&last, "unknownTag", "Side(54)"));
    }
    client.shutdown();
    said
}

#[test]
fn an_undefined_tag_is_published_in_the_desks_name() {
    let workspace = Workspace::new("unknown-tag-desk");
    let said = messages_under(&[workspace.desk_dictionary(DESK_44)]);
    assert_eq!(
        said,
        ["\"desk\" does not define Side(54); standard FIX.4.4"]
    );
}

/// A rename republishes because the registry's identity covers names. A
/// default moved between two names for one file changes no dictionary and no
/// identity: only the comparison of names makes the walk run again.
#[test]
fn a_new_name_for_the_same_dictionary_republishes() {
    let workspace = Workspace::new("unknown-tag-rename");
    let path = workspace.write("desk.xml", DESK_44);
    let named = |entries: Value| json!({ "allowDictionaryPath": true, "dictionaries": entries });
    let said = messages_under(&[
        named(json!([{ "path": path, "name": "desk", "default": true }])),
        named(json!([{ "path": path, "name": "book", "default": true }])),
    ]);
    assert_eq!(
        said[1],
        "\"book\" does not define Side(54); standard FIX.4.4"
    );

    let said = messages_under(&[
        named(json!([
            { "path": path, "name": "alpha", "default": true },
            { "path": path, "name": "beta" },
        ])),
        named(json!([
            { "path": path, "name": "alpha" },
            { "path": path, "name": "beta", "default": true },
        ])),
    ]);
    assert_eq!(
        said,
        [
            "\"alpha\" does not define Side(54); standard FIX.4.4",
            "\"beta\" does not define Side(54); standard FIX.4.4",
        ]
    );
}

/// A venue's own message type carrying a venue tag: one finding for each
/// rule, beside the integrity errors of a line without a trailer.
const VENUE_LINE: &str = "8=FIX.4.4|35=U1|9000=x|";

/// `(code, severity, rule)` of the publish's `unknownTag` and `unknownMsgType`
/// findings, sorted.
fn unknown_findings(publish: &Value) -> Vec<(String, u64, String)> {
    let mut found: Vec<(String, u64, String)> = publish["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|diagnostic| {
            diagnostic["code"] == "unknownTag" || diagnostic["code"] == "unknownMsgType"
        })
        .map(|diagnostic| {
            (
                diagnostic["code"].as_str().unwrap().to_owned(),
                diagnostic["severity"].as_u64().unwrap(),
                diagnostic["data"]["rule"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    found.sort();
    found
}

/// Every notification in arrival order, compact enough for an assertion
/// message: the sequence is what a failure on another machine has to show.
fn trace(notes: &[(String, Value)]) -> String {
    notes
        .iter()
        .map(|(method, params)| match method.as_str() {
            "textDocument/publishDiagnostics" => format!(
                "publish v{} {:?}",
                params["version"],
                unknown_findings(params)
            ),
            "window/logMessage" | "window/showMessage" => {
                format!("{method} {}", params["message"])
            }
            _ => method.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn finding(code: &str, severity: u64) -> (String, u64, String) {
    (code.to_owned(), severity, code.to_owned())
}

#[test]
fn unsupported_dictionary_names_on_the_wire_publish_no_unknown_message_types() {
    let mut client = TestClient::start();
    open(
        &mut client,
        "8=FIX.5.0|35=0|\n8=FIX.5.0|35=U1|\n8=FIX.5.0SP1|35=A|\n8=FIX.5.0SP1|35=U1|",
    );
    let published = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(unknown_findings(&published), []);
    assert_eq!(
        codes(&published)
            .iter()
            .filter(|code| code.as_str() == "unknownVersion")
            .count(),
        4
    );
    client.shutdown();
}

#[test]
fn an_application_only_custom_dictionary_publishes_unknown_message_types() {
    let workspace = Workspace::new("unknown-message-type-application");
    let dictionary = "<fix version='FIX.4.4'><messages>\
        <message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields>\
        <field number='8' name='BeginString' type='STRING'/>\
        <field number='35' name='MsgType' type='STRING'/></fields></fix>";
    let mut client = workspace.client(workspace.desk_dictionary(dictionary));
    client.did_open(URI, "8=FIX.4.4|35=U1|", 1);
    let published = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(unknown_findings(&published), [finding("unknownMsgType", 1)]);
    assert_eq!(
        message_on(&published, "unknownMsgType", "U1"),
        "\"desk\" does not define MsgType(35) = U1"
    );
    let diagnostic = published["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|diagnostic| diagnostic["code"] == "unknownMsgType")
        .unwrap();
    assert_eq!(
        diagnostic["range"],
        json!({ "start": { "line": 0, "character": 13 }, "end": { "line": 0, "character": 15 } })
    );

    client.did_change_full(URI, "8=FIX.4.4|35=D|", 2);
    let published = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(published["version"], 2);
    assert_eq!(unknown_findings(&published), []);
    client.shutdown();
}

#[test]
fn the_two_rules_are_toggled_and_regraded_apart() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer_base_pull();
    client.did_open(URI, VENUE_LINE, 1);
    let first = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(
        unknown_findings(&first),
        [finding("unknownMsgType", 1), finding("unknownTag", 3)],
        "message types are errors; unknown tags remain information"
    );
    assert_eq!(
        message_on(&first, "unknownMsgType", "U1"),
        "FIX.4.4 does not define MsgType(35) = U1"
    );
    assert_eq!(
        message_on(&first, "unknownTag", "9000"),
        "FIX.4.4 does not define tag 9000"
    );

    for (rules, expected) in [
        (
            json!({ "unknownTag": false }),
            vec![finding("unknownMsgType", 1)],
        ),
        (
            json!({ "unknownTag": { "severity": "hint" } }),
            vec![finding("unknownMsgType", 1), finding("unknownTag", 4)],
        ),
        (
            json!({ "unknownMsgType": false }),
            vec![finding("unknownTag", 3)],
        ),
        (
            json!({ "unknownMsgType": { "severity": "hint" } }),
            vec![finding("unknownMsgType", 4), finding("unknownTag", 3)],
        ),
        (
            json!({ "unknownMsgType": { "severity": "info" } }),
            vec![finding("unknownMsgType", 3), finding("unknownTag", 3)],
        ),
    ] {
        pull(&mut client, json!({ "diagnostics": { "rules": rules } }));
        let last = last_publish(&client.drain_notifications(QUIET), URI).expect("republished");
        assert_eq!(last["version"], 1, "the document did not move");
        assert_eq!(unknown_findings(&last), expected, "{rules}");
    }
    client.shutdown();
}

/// Each modeline is read with the edit that writes it, and governs only the
/// rule it names.
#[test]
fn a_modeline_toggles_and_regrades_each_rule_apart() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer_base_pull();
    client.did_open(URI, VENUE_LINE, 1);
    for (version, setting, expected) in [
        (2, "unknownTag=false", vec![finding("unknownMsgType", 1)]),
        (
            3,
            "unknownTag.severity=hint",
            vec![finding("unknownMsgType", 1), finding("unknownTag", 4)],
        ),
        (4, "unknownMsgType=false", vec![finding("unknownTag", 3)]),
        (
            5,
            "unknownMsgType.severity=hint",
            vec![finding("unknownMsgType", 4), finding("unknownTag", 3)],
        ),
    ] {
        let text = format!("# fix: diagnostics.rules.{setting}\n{VENUE_LINE}");
        client.did_change_full(URI, &text, version);
        let notes = client.drain_notifications(QUIET);
        let last = last_publish(&notes, URI).expect("published");
        assert_eq!(last["version"], version, "{setting}\n{}", trace(&notes));
        assert_eq!(
            unknown_findings(&last),
            expected,
            "{setting}\n{}",
            trace(&notes)
        );
    }
    client.shutdown();
}

/// An edit that changes the modeline is walked once, under the configuration
/// the modeline asks for: a client never sees a set the old rules produced.
#[test]
fn a_modeline_edit_publishes_once_and_fresh() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer_base_pull();
    client.did_open(URI, VENUE_LINE, 1);
    let _ = client.wait_for("textDocument/publishDiagnostics");

    let text = format!("# fix: diagnostics.rules.unknownTag=false\n{VENUE_LINE}");
    client.did_change_full(URI, &text, 2);
    let notes = client.drain_notifications(QUIET);
    let at_two: Vec<&Value> = notes
        .iter()
        .filter(|(method, params)| {
            method == "textDocument/publishDiagnostics"
                && params["uri"] == URI
                && params["version"] == 2
        })
        .map(|(_, params)| params)
        .collect();
    assert_eq!(
        at_two.len(),
        1,
        "one publish for the edit\n{}",
        trace(&notes)
    );
    assert_eq!(
        unknown_findings(at_two[0]),
        [finding("unknownMsgType", 1)],
        "the first and only set already follows the modeline\n{}",
        trace(&notes)
    );
    client.shutdown();
}

/// An edit made while the document's configuration is being pulled waits for
/// the answer: the latest text is walked once, under the rules it brings.
#[test]
fn an_edit_during_a_pull_waits_for_its_rules() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer_base_pull();
    client.did_open(URI, VENUE_LINE, 1);
    let _ = client.wait_for("textDocument/publishDiagnostics");

    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": null }),
    );
    let (pull, method, _) = client.recv_server_request();
    assert_eq!(method, "workspace/configuration");
    client.did_change_full(URI, &format!("{VENUE_LINE}9000=y|"), 2);
    let notes = client.drain_notifications(Duration::from_millis(200));
    assert_eq!(
        last_publish(&notes, URI),
        None,
        "nothing is walked under rules a pull is about to replace"
    );

    client.reply(
        pull,
        json!([{ "diagnostics": { "rules": { "unknownTag": false } } }]),
    );
    let notes = client.drain_notifications(QUIET);
    let at_two: Vec<&Value> = notes
        .iter()
        .filter(|(method, params)| {
            method == "textDocument/publishDiagnostics"
                && params["uri"] == URI
                && params["version"] == 2
        })
        .map(|(_, params)| params)
        .collect();
    assert_eq!(at_two.len(), 1, "one publish for the edit: {at_two:#?}");
    assert_eq!(unknown_findings(at_two[0]), [finding("unknownMsgType", 1)]);
    client.shutdown();
}

/// Hover and the diagnostic say the same thing about one tag.
#[test]
fn the_diagnostic_and_hover_agree_on_a_tag() {
    let workspace = Workspace::new("unknown-tag-hover");
    let mut client = workspace.client(workspace.desk_dictionary(DESK_44));
    let line = "8=FIX.4.4|35=0|55=X|1328=x|";
    client.did_open(URI, line, 1);
    let published = client.wait_for("textDocument/publishDiagnostics");
    let said = message_on(&published, "unknownTag", "RejectText(1328)");
    let start = line.find("1328=").unwrap() as u32;
    let hover = client.hover(URI, 0, start).expect("answered");
    let markdown = hover["contents"]["value"].as_str().unwrap();
    assert!(markdown.contains("*Not defined by \"desk\"*"), "{markdown}");
    let note = "not in FIX.4.4, introduced in FIX.5.0SP1";
    assert!(
        markdown.contains("*Not in FIX.4.4, introduced in FIX.5.0SP1*"),
        "{markdown}"
    );
    assert_eq!(
        said,
        format!("\"desk\" does not define RejectText(1328); {note}")
    );
    client.shutdown();
}

/// `line` with BodyLength(9) and CheckSum(10) set right by the server's own
/// repair, as `tests/fixtures.rs` frames its lines.
fn repaired(line: &str) -> String {
    let ctx = harness::test_ctx();
    let analysis = fix_lsp::ctx::analyze(line.as_bytes(), &ctx);
    let message = analysis.message().expect("a message");
    let repair =
        fix_lsp::rules::integrity::repair(&fix_lsp::rules::LineCtx::new(line.as_bytes(), message))
            .expect("a repair");
    String::from_utf8(harness::apply_edits(line.as_bytes(), &repair.edits)).unwrap()
}

/// `info` findings fill the cap first and keep arriving after it is full;
/// the errors further down still take it.
#[test]
fn info_findings_never_keep_an_error_out() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer_base_pull();
    let venue = repaired("8=FIX.4.4|9=0|35=D|9000=a|9000=b|10=000|");
    let unknown_type = repaired("8=FIX.4.4|9=0|35=U1|9000=a|9000=b|10=000|");
    let text = [venue.as_str(), &venue, BAD_BOTH, &unknown_type].join("\n");
    pull(
        &mut client,
        json!({ "limits": { "diagnosticsPerDocument": 3 } }),
    );
    client.did_open(URI, &text, 1);

    let last = last_publish(&client.drain_notifications(QUIET), URI).expect("published");
    assert_eq!(
        shown(&last),
        [
            ("bodyLength".to_owned(), 1, 2),
            ("checkSum".to_owned(), 1, 2),
            ("unknownMsgType".to_owned(), 1, 3),
            ("truncated".to_owned(), 3, 0),
        ]
    );
    assert_eq!(truncated_message(&last), "6 more problems not shown");
    client.shutdown();
}

/// The `$/progress` values for `kind`, in arrival order.
fn progress_kinds(notes: &[(String, Value)]) -> Vec<String> {
    notes
        .iter()
        .filter(|(method, _)| method == "$/progress")
        .map(|(_, params)| params["value"]["kind"].as_str().unwrap_or("?").to_owned())
        .collect()
}

/// More lines than any test needs, and still walked in a few milliseconds —
/// which is the point of the test below.
const MANY: usize = 2_000;

/// A token is proposed for every walk, and then not used unless the walk lasts
/// long enough to be worth a bar. Two lines need no visible progress bar; a
/// bar that appears and vanishes inside one frame is worse than none.
///
/// That the bar does open, at the percentage reached, is
/// `the_bar_opens_on_the_first_report_and_not_before` — at unit level, because
/// reaching the report cadence here would need a document too large to build
/// in a test.
#[test]
fn a_walk_too_short_to_be_worth_a_bar_announces_nothing() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull_and_progress());
    client.answer_base_pull();
    let short = [BAD_BOTH; 2].join("\n");
    client.did_open(URI, &short, 1);

    let created = client.answer(PROGRESS_CREATE, json!(null));
    assert!(
        created["token"].as_str().is_some(),
        "a token was proposed: {created}"
    );
    let published = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(published["uri"], URI);
    assert!(!codes(&published).is_empty(), "the walk completed");

    let notes = client.drain_notifications(QUIET);
    assert_eq!(
        progress_kinds(&notes),
        Vec::<String>::new(),
        "nothing is said under it: {notes:?}"
    );
    client.shutdown();
}

/// A client that does not advertise progress must never see a token: creating
/// one it did not agree to is a protocol violation, not a degradation.
#[test]
fn a_client_without_progress_support_sees_none_and_still_gets_diagnostics() {
    let mut client = TestClient::start();
    open(&mut client, BAD_BOTH);
    let _ = client.wait_for("textDocument/publishDiagnostics");
    let notes = client.drain_notifications(QUIET);
    assert!(progress_kinds(&notes).is_empty(), "{notes:?}");
    client.shutdown();
}

#[test]
fn a_refused_progress_token_is_never_used() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull_and_progress());
    client.answer_base_pull();
    client.did_open(URI, BAD_BOTH, 1);
    client.refuse(PROGRESS_CREATE);

    let notes = client.drain_notifications(QUIET);
    assert!(
        progress_kinds(&notes).is_empty(),
        "nothing may be reported under a token the client refused: {notes:?}"
    );
    assert!(
        last_publish(&notes, URI).is_some(),
        "the walk runs regardless"
    );
    client.shutdown();
}

/// Balance, not presence: on a one-line document the walk is over before the
/// client answers the `create`, so there is nothing to begin. That the `end` is
/// owed on every exit path is asserted deterministically in the unit tests
/// (`dropping_a_begun_progress_ends_it`); this checks the two never diverge in
/// the real exchange.
#[test]
fn progress_stays_balanced_when_a_document_closes() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull_and_progress());
    client.answer_base_pull();
    client.did_open(URI, BAD_BOTH, 1);
    client.answer(PROGRESS_CREATE, json!(null));
    client.did_close(URI);

    let notes = client.drain_notifications(QUIET);
    let kinds = progress_kinds(&notes);
    assert_eq!(
        kinds.iter().filter(|k| *k == "end").count(),
        kinds.iter().filter(|k| *k == "begin").count(),
        "every begin is ended: {kinds:?}"
    );
    client.shutdown();
}

/// A client may advertise `window.workDoneProgress` and still refuse a token.
/// Asking again on every document puts an error in the user's log for a feature
/// it will never grant, so the refusal corrects what the client advertised.
#[test]
fn a_client_that_refuses_the_create_is_not_asked_again() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull_and_progress());
    client.answer_base_pull();

    client.did_open(URI, BAD_BOTH, 1);
    client.refuse(PROGRESS_CREATE);
    let _ = client.wait_for("textDocument/publishDiagnostics");

    // No `create` is answered for this one: asking again is what the fix stops,
    // so `wait_for` would block on a request that never comes.
    let second = "file:///b.fix";
    client.did_open(second, BAD_BOTH, 1);
    let published = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(
        published["uri"], second,
        "the second document is still validated"
    );

    let notes = client.drain_notifications(QUIET);
    assert!(
        progress_kinds(&notes).is_empty(),
        "and no progress is reported under a token that was refused: {notes:?}"
    );
    client.shutdown();
}

/// A regression net, not the proof: an edit arriving inside the debounce window
/// is usually coalesced before any walk starts, so this rarely creates the race
/// it guards against. What the fix actually states is asserted deterministically
/// in `a_walk_about_text_that_has_moved_is_not_accepted`.
#[test]
fn no_publish_ever_carries_a_version_the_document_has_left() {
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    let long = vec![BAD_BOTH; MANY].join("\n");
    client.did_open(URI, &long, 1);
    client.did_change_full(URI, SOUND, 2);

    let notes = client.drain_notifications(QUIET);
    for (method, params) in &notes {
        if method == "textDocument/publishDiagnostics" && params["uri"] == URI {
            assert_eq!(
                params["version"], 2,
                "a publish about text that has moved: {params}"
            );
        }
    }
    client.shutdown();
}

#[test]
fn fixt_application_tables_publish_unknown_types_without_a_session_layer() {
    let mut client = TestClient::start();
    open(
        &mut client,
        "8=FIXT.1.1|35=U1|1128=7|\n8=FIXT.1.1|35=U1|1128=8|\n8=FIXT.1.1|35=U1|1128=9|",
    );
    let published = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(
        unknown_findings(&published),
        [
            finding("unknownMsgType", 1),
            finding("unknownMsgType", 1),
            finding("unknownMsgType", 1)
        ]
    );
    assert!(!codes(&published)
        .iter()
        .any(|code| code == "unknownVersion"));
    client.shutdown();
}

#[test]
fn a_logon_does_not_supply_an_application_default_to_another_line() {
    let mut client = TestClient::start();
    open(
        &mut client,
        "8=FIXT.1.1|35=A|1137=9|\n8=FIXT.1.1|35=D|55=X|9000=x|",
    );
    let published = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(unknown_findings(&published), []);
    let problems = published["diagnostics"].as_array().unwrap();
    let version_problems: Vec<_> = problems
        .iter()
        .filter(|diagnostic| diagnostic["code"] == "unknownVersion")
        .collect();
    assert_eq!(version_problems.len(), 1);
    assert_eq!(version_problems[0]["range"]["start"]["line"], 1);
    client.shutdown();
}

#[test]
fn administrative_selectors_are_reported_without_an_application_default() {
    let mut client = TestClient::start();
    open(&mut client, "8=FIXT.1.1|35=0|1128=9|");
    let published = client.wait_for("textDocument/publishDiagnostics");
    let problems = published["diagnostics"].as_array().unwrap();
    let problem = problems
        .iter()
        .find(|diagnostic| diagnostic["code"] == "unknownVersion")
        .unwrap();
    assert_eq!(
        problem["message"],
        "ApplVerID(1128) is prohibited on an administrative message"
    );
    assert_eq!(unknown_findings(&published), []);
    client.shutdown();
}

#[test]
fn missing_message_type_and_integrity_fields_keep_their_existing_rule_controls() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer_base_pull();
    client.did_open(URI, "8=FIX.4.4|55=X|", 1);
    let published = client.wait_for("textDocument/publishDiagnostics");
    let mut found = codes(&published);
    found.sort();
    assert_eq!(found, ["bodyLength", "checkSum", "missingMsgType"]);
    let problem = published["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|diagnostic| diagnostic["code"] == "missingMsgType")
        .unwrap();
    assert_eq!(problem["data"]["rule"], "unknownMsgType");
    assert_eq!(problem["severity"], 1, "missing MsgType(35) is an error");
    pull(
        &mut client,
        json!({"diagnostics": {"rules": {"unknownMsgType": {"severity": "info"}}}}),
    );
    let notes = client.drain_notifications(QUIET);
    let published = last_publish(&notes, URI).expect("republished");
    assert_eq!(
        shown(&published),
        [
            ("bodyLength".to_owned(), 1, 0),
            ("checkSum".to_owned(), 1, 0),
            ("missingMsgType".to_owned(), 3, 0),
        ]
    );
    pull(
        &mut client,
        json!({"diagnostics": {"rules": {"unknownMsgType": false}}}),
    );
    let notes = client.drain_notifications(QUIET);
    let published = last_publish(&notes, URI).expect("republished");
    let mut found = codes(&published);
    found.sort();
    assert_eq!(found, ["bodyLength", "checkSum"]);
    client.shutdown();
}

#[test]
fn disabling_resolution_diagnostics_never_selects_an_application() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer_base_pull();
    client.did_open(URI, "8=FIXT.1.1|35=U1|9000=x|", 1);
    let published = client.wait_for("textDocument/publishDiagnostics");
    assert!(codes(&published)
        .iter()
        .any(|code| code == "unknownVersion"));
    assert_eq!(unknown_findings(&published), []);
    pull(
        &mut client,
        json!({"diagnostics": {"rules": {"unknownVersion": false}}}),
    );
    let notes = client.drain_notifications(QUIET);
    let published = last_publish(&notes, URI).expect("republished");
    assert!(!codes(&published)
        .iter()
        .any(|code| code == "unknownVersion"));
    assert_eq!(unknown_findings(&published), []);
    client.shutdown();
}

#[test]
fn invalid_and_repeated_selectors_publish_resolution_errors_without_body_guesses() {
    let lines = [
        "8=FIXT.1.1|35=D|1128=99|9000=x|",
        "8=FIXT.1.1|35=D|1128=9|1128=9|9000=x|",
        "8=FIX.4.4|8=FIX.4.4|35=D|9000=x|",
        "8=FIX.4.4|35=D|35=D|9000=x|",
        "8=FIXT.1.1|35=D|1128=9|1156=0|1156=0|9000=x|",
        "8=FIXT.1.1|35=D|1128=9|1129=sample|1129=sample|9000=x|",
        "8=FIXT.1.1|35=D|1128=9|1156=4294967295|9000=x|",
    ];
    let mut client = TestClient::start();
    open(&mut client, &lines.join("\n"));
    let published = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(unknown_findings(&published), []);
    let problems = published["diagnostics"].as_array().unwrap();
    let version_problems: Vec<_> = problems
        .iter()
        .filter(|diagnostic| diagnostic["code"] == "unknownVersion")
        .collect();
    assert_eq!(version_problems.len(), lines.len());
    for (line, problem) in version_problems.iter().enumerate() {
        assert_eq!(problem["range"]["start"]["line"], line);
    }
    client.shutdown();
}
