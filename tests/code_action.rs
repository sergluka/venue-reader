//! `textDocument/codeAction`: the quickfix that repairs a line's trailer.
//!
//! Black box, through the protocol. The central test applies what the server
//! returns and sends the result back, which is the only statement that covers
//! the ranges, the version, the republish and Stage 4a-1's stale-walk fix at
//! once — a test that only inspected the JSON would pass on edits that do not
//! actually fix the line.

mod harness;

use std::time::Duration;

use harness::TestClient;
use serde_json::{json, Value};

const URI: &str = "file:///a.fix";

const BAD_BOTH: &str = "8=FIX.4.4|9=7|35=D|34=2|49=CLIENT1|56=BROKER1|\
52=20251026-09:00:02.000|11=ORDER123|21=1|55=BTCUSD|54=1|38=10|40=2|44=65000|59=0|\
60=20251026-09:00:02.000|10=001|";

const SOUND: &str =
    "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";

const QUIET: Duration = Duration::from_millis(300);

const REQUEST_FAILED: i32 = -32803;

fn open(text: &str) -> TestClient {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_taking_actions());
    client.did_open(URI, text, 1);
    client
}

/// Whole first line, invoked by the user rather than polled by the editor.
fn on_line(line: u32) -> Value {
    json!({
        "textDocument": { "uri": URI },
        "range": {
            "start": { "line": line, "character": 0 },
            "end": { "line": line, "character": 400 },
        },
        "context": { "diagnostics": [], "triggerKind": 1 },
    })
}

fn actions(client: &mut TestClient, params: Value) -> Vec<Value> {
    let result = client
        .request("textDocument/codeAction", params)
        .expect("answered");
    result.as_array().cloned().unwrap_or_default()
}

#[test]
fn a_sound_line_offers_nothing() {
    let mut client = open(SOUND);
    assert_eq!(actions(&mut client, on_line(0)), Vec::<Value>::new());
    client.shutdown();
}

#[test]
fn a_line_wrong_in_two_ways_offers_one_action() {
    let mut client = open(BAD_BOTH);
    let offered = actions(&mut client, on_line(0));
    assert_eq!(offered.len(), 1, "{offered:#?}");
    let action = &offered[0];
    assert_eq!(action["title"], "Fix BodyLength and CheckSum");
    assert_eq!(action["kind"], "quickfix");
    assert_eq!(action["isPreferred"], true);
    let changes = &action["edit"]["documentChanges"];
    assert_eq!(changes[0]["textDocument"]["uri"], URI);
    assert_eq!(changes[0]["textDocument"]["version"], 1);
    assert_eq!(changes[0]["edits"].as_array().map(Vec::len), Some(2));
    client.shutdown();
}

/// The action claims the diagnostics the server published, byte for byte. A
/// client matches them by range and message to decide which marks the fix
/// clears; anything re-rendered differently here would silently stop matching.
#[test]
fn the_action_claims_exactly_the_published_integrity_diagnostics() {
    let mut client = open(BAD_BOTH);
    let published = client.wait_for("textDocument/publishDiagnostics");
    let mut expected: Vec<Value> = published["diagnostics"]
        .as_array()
        .expect("a set")
        .iter()
        .filter(|diag| diag["data"]["rule"] == "integrity")
        .cloned()
        .collect();
    assert_eq!(expected.len(), 2, "{expected:#?}");

    let offered = actions(&mut client, on_line(0));
    let mut claimed = offered[0]["diagnostics"].as_array().cloned().unwrap();
    let key = |diag: &Value| diag["code"].as_str().unwrap_or_default().to_owned();
    expected.sort_by_key(key);
    claimed.sort_by_key(key);
    assert_eq!(claimed, expected);
    client.shutdown();
}

/// The whole point, and the only test that proves the edits are correct rather
/// than merely well-shaped: apply them, hand the result back, and require the
/// publish *at the new version* to be empty. Waiting for "the next publish"
/// would accept the stale one Stage 4a-1's window used to allow.
#[test]
fn applying_what_it_returns_leaves_a_line_with_nothing_to_report() {
    let mut client = open(BAD_BOTH);
    let offered = actions(&mut client, on_line(0));
    let edits = offered[0]["edit"]["documentChanges"][0]["edits"]
        .as_array()
        .expect("edits")
        .clone();

    // Back to front: correcting BodyLength lengthens the line, so applying in
    // field order would move the CheckSum range out from under its own edit.
    let mut fixed = BAD_BOTH.to_owned();
    for edit in edits.iter().rev() {
        let start = edit["range"]["start"]["character"].as_u64().unwrap() as usize;
        let end = edit["range"]["end"]["character"].as_u64().unwrap() as usize;
        fixed.replace_range(start..end, edit["newText"].as_str().unwrap());
    }
    assert_ne!(fixed, BAD_BOTH);

    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": URI, "version": 2 },
            "contentChanges": [{ "text": fixed }],
        }),
    );
    loop {
        let published = client.wait_for("textDocument/publishDiagnostics");
        if published["version"] == 2 {
            assert_eq!(
                published["diagnostics"].as_array().map(Vec::len),
                Some(0),
                "the repaired line: {fixed}"
            );
            break;
        }
    }
    client.shutdown();
}

/// A request for `source.fixAll` must not be answered with quickfixes: the
/// client asked for a different thing, and this server has none yet.
#[test]
fn a_request_that_excludes_quickfix_is_answered_with_nothing() {
    let mut client = open(BAD_BOTH);
    let mut params = on_line(0);
    params["context"]["only"] = json!(["source.fixAll"]);
    assert_eq!(actions(&mut client, params), Vec::<Value>::new());

    let mut asked_for = on_line(0);
    asked_for["context"]["only"] = json!(["quickfix"]);
    assert_eq!(actions(&mut client, asked_for).len(), 1);
    client.shutdown();
}

/// Editors poll this on every cursor move. Answering an unasked poll means
/// analysing a range for a lightbulb nobody looked at.
#[test]
fn an_automatic_trigger_without_only_is_answered_with_nothing() {
    let mut client = open(BAD_BOTH);
    let mut params = on_line(0);
    params["context"]["triggerKind"] = json!(2);
    assert_eq!(actions(&mut client, params.clone()), Vec::<Value>::new());

    params["context"]["only"] = json!(["quickfix"]);
    assert_eq!(
        actions(&mut client, params).len(),
        1,
        "unless it says what for"
    );
    client.shutdown();
}

/// Refused rather than truncated: a partial repair of a selection is less
/// predictable than being told the selection is too big. `[]` would be
/// indistinguishable from "nothing here is repairable".
#[test]
fn an_oversized_range_is_refused_and_says_so() {
    let long = vec![BAD_BOTH; 3000].join("\n");
    let mut client = open(&long);
    let params = json!({
        "textDocument": { "uri": URI },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 2999, "character": 400 },
        },
        "context": { "diagnostics": [], "triggerKind": 1 },
    });
    let error = client
        .request("textDocument/codeAction", params)
        .expect_err("refused");
    assert_eq!(error, REQUEST_FAILED);
    client.shutdown();
}

/// A poll has no user to tell, so it gets the empty answer rather than an
/// error the editor would surface as a server failure.
#[test]
fn an_oversized_automatic_request_is_answered_with_nothing() {
    let long = vec![BAD_BOTH; 3000].join("\n");
    let mut client = open(&long);
    let params = json!({
        "textDocument": { "uri": URI },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 2999, "character": 400 },
        },
        "context": { "diagnostics": [], "triggerKind": 2, "only": ["quickfix"] },
    });
    assert_eq!(actions(&mut client, params), Vec::<Value>::new());
    client.shutdown();
}

/// Clients send an end past the document, and a range clamped before it is
/// normalised loses the last line — which is the one the cursor is on.
#[test]
fn an_end_past_the_document_still_covers_the_last_line() {
    // The broken line is the last one, and the end is both past the document
    // and at column 0 — the shape that makes the order matter. Clamping first
    // turns the end into the last line's own number, where "ends at column 0"
    // then excludes it.
    let mut client = open(&format!("{SOUND}\n{BAD_BOTH}"));
    let params = json!({
        "textDocument": { "uri": URI },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 9, "character": 0 },
        },
        "context": { "diagnostics": [], "triggerKind": 1 },
    });
    assert_eq!(actions(&mut client, params).len(), 1);
    client.shutdown();
}

/// One action per line, not one per document: two broken lines in the range
/// are two separate repairs a user can apply independently.
#[test]
fn every_line_in_the_range_gets_its_own_action() {
    let mut client = open(&format!("{BAD_BOTH}\n{SOUND}\n{BAD_BOTH}"));
    let params = json!({
        "textDocument": { "uri": URI },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 2, "character": 400 },
        },
        "context": { "diagnostics": [], "triggerKind": 1 },
    });
    let offered = actions(&mut client, params);
    assert_eq!(offered.len(), 2, "{offered:#?}");
    let lines: Vec<u64> = offered
        .iter()
        .map(|action| {
            action["edit"]["documentChanges"][0]["edits"][0]["range"]["start"]["line"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(lines, vec![0, 2]);
    client.shutdown();
}

/// The edit carries the version its answer was computed against, so a client
/// holding an action across an edit applies it to text that has moved — or,
/// having said `documentChanges`, refuses to.
#[test]
fn an_action_carries_the_version_it_was_computed_against() {
    let mut client = open(BAD_BOTH);
    let held = actions(&mut client, on_line(0));
    assert_eq!(
        held[0]["edit"]["documentChanges"][0]["textDocument"]["version"],
        1
    );

    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": URI, "version": 2 },
            "contentChanges": [{ "text": BAD_BOTH }],
        }),
    );
    let fresh = actions(&mut client, on_line(0));
    assert_eq!(
        fresh[0]["edit"]["documentChanges"][0]["textDocument"]["version"],
        2
    );
    assert_eq!(
        held[0]["edit"]["documentChanges"][0]["textDocument"]["version"], 1,
        "the one already handed out does not change under the client"
    );
    client.shutdown();
}

/// Positions leave the crate under the negotiated encoding. A log prefix with
/// a character outside the BMP-free ASCII range moves every column after it.
#[test]
fn a_span_after_non_ascii_text_is_converted_under_the_negotiated_encoding() {
    let prefixed = format!("— {BAD_BOTH}");
    let mut client = TestClient::start();
    let mut caps = TestClient::caps_taking_actions();
    caps["general"]["positionEncodings"] = json!(["utf-16"]);
    client.initialize_caps(caps);
    client.did_open(URI, &prefixed, 1);

    let offered = actions(&mut client, on_line(0));
    let first = &offered[0]["edit"]["documentChanges"][0]["edits"][0];
    // "— " is three bytes and two utf-16 units, so the BodyLength value that
    // starts at byte 16 is at character 14.
    assert_eq!(prefixed.find("9=7").unwrap() + 2, 16);
    assert_eq!(first["range"]["start"]["character"], 14);
    client.shutdown();
}

/// One switch governs the report and the repair. There is no configuration
/// under which an action can claim a problem the user asked not to hear about.
#[test]
fn disabling_the_rule_removes_the_action_too() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": TestClient::caps_taking_actions(),
        "initializationOptions": { "fix": {
            "diagnostics": { "rules": { "integrity": false } },
        }},
    }));
    client.did_open(URI, BAD_BOTH, 1);

    assert_eq!(actions(&mut client, on_line(0)), Vec::<Value>::new());
    // And the same switch silenced the report: the two cannot come apart.
    let published = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(published["diagnostics"].as_array().map(Vec::len), Some(0));
    client.shutdown();
}

/// A client that cannot take a versioned literal is offered no provider, and
/// so never sends the request. Asserted on what is advertised, since that is
/// what stops it.
#[test]
fn a_client_that_cannot_take_the_answer_is_not_offered_the_provider() {
    let mut client = TestClient::start();
    let advertised = client.initialize_caps(json!({
        "general": { "positionEncodings": ["utf-8"] },
        "textDocument": { "codeAction": {
            "codeActionLiteralSupport": { "codeActionKind": { "valueSet": ["quickfix"] } },
        }},
    }));
    assert_eq!(
        advertised.pointer("/capabilities/codeActionProvider"),
        None,
        "no versioned edits, so no provider"
    );
    client.shutdown();
}

/// A request whose params do not parse is the client's error, not an empty
/// answer: `[]` would say "nothing here is repairable", which is a claim about
/// the document rather than about the request.
#[test]
fn malformed_params_are_an_error() {
    let mut client = open(BAD_BOTH);
    let error = client
        .request(
            "textDocument/codeAction",
            json!({ "textDocument": { "uri": URI } }),
        )
        .expect_err("refused");
    assert_eq!(error, harness::INVALID_PARAMS);
    client.shutdown();
}

/// A closed document is gone, not malformed.
#[test]
fn a_closed_document_is_answered_with_nothing() {
    let mut client = open(BAD_BOTH);
    client.did_close(URI);
    let _ = client.drain_notifications(QUIET);
    assert_eq!(actions(&mut client, on_line(0)), Vec::<Value>::new());
    client.shutdown();
}

/// The cursor, which is an empty range at column zero. A code action is about
/// the line the cursor sits on, so `(n,0)..(n,0)` **includes** line `n` — where
/// an inlay hint, being a point under an exclusive end, excludes it.
///
/// The two normalisations differ on purpose and each lives with the handler
/// that owns it. This test is what fails if either is ever hoisted into the
/// shared `LineRange`, which would look like tidying and would silently stop
/// answering the commonest invocation there is: a cursor on a broken line.
#[test]
fn a_bare_cursor_still_offers_the_line_it_sits_on() {
    let mut client = open(&format!("{SOUND}\n{BAD_BOTH}"));
    let cursor = json!({
        "textDocument": { "uri": URI },
        "range": {
            "start": { "line": 1, "character": 0 },
            "end": { "line": 1, "character": 0 },
        },
        "context": { "diagnostics": [], "triggerKind": 1 },
    });
    assert_eq!(actions(&mut client, cursor).len(), 1, "the broken line");

    // And the control: the same empty range on the sound line offers nothing,
    // so the test above is not passing because every cursor answers.
    let elsewhere = json!({
        "textDocument": { "uri": URI },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 0, "character": 0 },
        },
        "context": { "diagnostics": [], "triggerKind": 1 },
    });
    assert!(actions(&mut client, elsewhere).is_empty());
    client.shutdown();
}
