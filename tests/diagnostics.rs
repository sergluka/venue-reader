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

use harness::{TestClient, PROGRESS_CREATE};
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
