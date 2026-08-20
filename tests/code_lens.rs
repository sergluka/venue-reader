//! `textDocument/codeLens` through the protocol.
//!
//! The list is classification only — a lens per message line, `command`
//! unfilled — because `CodeLensParams` carries no range and the summary would
//! otherwise be whole-document work. What `resolve` puts in the title is its
//! own test.

mod harness;

use harness::TestClient;
use serde_json::{json, Value};

const URI: &str = "file:///a.fix";

const HEARTBEAT: &str =
    "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";

/// A heartbeat has no second half to its summary — `render::details` finds
/// nothing to say about a session message. An order does.
const ORDER: &str = "8=FIX.4.4|9=139|35=D|34=2|49=CLIENT1|56=BROKER1|\
52=20251026-09:00:02.000|11=ORDER123|21=1|55=BTCUSD|54=1|38=10|40=2|44=65000|59=0|\
60=20251026-09:00:02.000|10=213|";

fn open(text: &str) -> TestClient {
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    client.did_open(URI, text, 1);
    client
}

fn lenses(client: &mut TestClient) -> Vec<Value> {
    client
        .request(
            "textDocument/codeLens",
            json!({ "textDocument": { "uri": URI } }),
        )
        .expect("answered")
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// One lens per message line and none for anything else. A comment, a blank and
/// a line no `tag=value` reads from all earn nothing.
#[test]
fn a_lens_marks_every_message_line_and_only_those() {
    let text = format!("# a comment\n\n{HEARTBEAT}\nnot a fix line at all\n{HEARTBEAT}");
    let mut client = open(&text);
    let found = lenses(&mut client);
    let lines: Vec<u64> = found
        .iter()
        .map(|lens| lens["range"]["start"]["line"].as_u64().unwrap())
        .collect();
    assert_eq!(lines, vec![2, 4], "{found:#?}");
    client.shutdown();
}

/// The list is classification: it says *where* a summary would go, not what it
/// says. Filling `command` here would render every summary in the document.
#[test]
fn the_list_carries_no_command() {
    let mut client = open(HEARTBEAT);
    let found = lenses(&mut client);
    assert_eq!(found.len(), 1);
    assert!(found[0].get("command").is_none(), "{:#?}", found[0]);
    assert_eq!(found[0]["data"]["line"], 0);
    assert_eq!(found[0]["data"]["version"], 1);
    client.shutdown();
}

/// A u64 hash as a JSON number would come back rounded from a JavaScript
/// client — past 2^53 the value is not representable — and then never match.
#[test]
fn the_identity_hash_is_a_hex_string() {
    let mut client = open(HEARTBEAT);
    let found = lenses(&mut client);
    let hash = found[0]["data"]["hash"].as_str().expect("a string");
    assert_eq!(hash.len(), 16, "fixed width: {hash}");
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{hash}");
    client.shutdown();
}

/// Past the cap, nothing — not the first `cap` lenses. A truncated list has
/// gaps a client cannot tell from lines that earned no lens.
#[test]
fn a_document_over_the_cap_answers_with_nothing() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": { "general": { "positionEncodings": ["utf-8"] } },
        "initializationOptions": { "fix": { "limits": { "codeLensLines": 3 } } },
    }));
    client.did_open(URI, &[HEARTBEAT; 4].join("\n"), 1);
    assert_eq!(lenses(&mut client), Vec::<Value>::new());
    client.shutdown();
}

/// Every log file ends with a newline, which leaves an empty last line. Counting
/// it against the cap would refuse a file of exactly `cap` lines — the boundary
/// a cap is most likely to be set at, and the one the benchmark hit first.
#[test]
fn a_trailing_newline_does_not_count_against_the_cap() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": { "general": { "positionEncodings": ["utf-8"] } },
        "initializationOptions": { "fix": { "limits": { "codeLensLines": 3 } } },
    }));
    // Three lines and the newline that ends the third.
    client.did_open(URI, &format!("{}\n", [HEARTBEAT; 3].join("\n")), 1);
    assert_eq!(lenses(&mut client).len(), 3);
    client.shutdown();
}

#[test]
fn a_document_at_the_cap_is_answered_in_full() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": { "general": { "positionEncodings": ["utf-8"] } },
        "initializationOptions": { "fix": { "limits": { "codeLensLines": 3 } } },
    }));
    client.did_open(URI, &[HEARTBEAT; 3].join("\n"), 1);
    assert_eq!(lenses(&mut client).len(), 3);
    client.shutdown();
}

/// Two switches, and the master one is not the channel.
#[test]
fn either_switch_silences_the_channel() {
    for settings in [
        json!({ "messageSummary": { "channels": { "codeLens": false } } }),
        json!({ "messageSummary": { "enabled": false } }),
    ] {
        let mut client = TestClient::start();
        client.initialize_with(json!({
            "capabilities": { "general": { "positionEncodings": ["utf-8"] } },
            "initializationOptions": { "fix": settings },
        }));
        client.did_open(URI, HEARTBEAT, 1);
        assert_eq!(lenses(&mut client), Vec::<Value>::new(), "{settings}");
        client.shutdown();
    }
}

#[test]
fn a_closed_document_answers_with_nothing() {
    let mut client = open(HEARTBEAT);
    client.did_close(URI);
    assert_eq!(lenses(&mut client), Vec::<Value>::new());
    client.shutdown();
}

#[test]
fn malformed_params_are_an_error() {
    let mut client = open(HEARTBEAT);
    let error = client
        .request("textDocument/codeLens", json!({}))
        .expect_err("refused");
    assert_eq!(error, harness::INVALID_PARAMS);
    client.shutdown();
}

/// The lens comes back with its title. This is where the summary is rendered —
/// the half a client asks for only where it draws.
#[test]
fn resolve_fills_the_title_with_the_summary() {
    let mut client = open(HEARTBEAT);
    let lens = lenses(&mut client)[0].clone();
    let resolved = client.request("codeLens/resolve", lens).expect("answered");
    let title = resolved["command"]["title"].as_str().expect("a title");
    assert!(!title.is_empty(), "{resolved}");
    assert_eq!(
        resolved["command"]["command"], "",
        "a title with no command is how a lens becomes a label"
    );
    client.shutdown();
}

/// `Command.title` is one flat string, so this channel joins what the inlay
/// channel keeps apart — and only when there is a second half to join.
#[test]
fn the_title_joins_both_halves_and_only_when_there_are_two() {
    let mut both = open(ORDER);
    let lens = lenses(&mut both)[0].clone();
    let joined = both.request("codeLens/resolve", lens).expect("answered")["command"]["title"]
        .as_str()
        .expect("a title")
        .to_owned();
    both.shutdown();

    let mut alone = TestClient::start();
    alone.initialize_with(json!({
        "capabilities": { "general": { "positionEncodings": ["utf-8"] } },
        "initializationOptions": { "fix": { "messageSummary": { "details": false } } },
    }));
    alone.did_open(URI, ORDER, 1);
    let lens = lenses(&mut alone)[0].clone();
    let main_only = alone.request("codeLens/resolve", lens).expect("answered")["command"]["title"]
        .as_str()
        .expect("a title")
        .to_owned();
    alone.shutdown();

    // Not "the title has no pipe": the default template puts one between its
    // own slots, so the character proves nothing. What joining means is that
    // the second half is appended to the first with the separator between.
    assert!(
        joined.starts_with(&format!("{main_only} | ")),
        "joined: {joined}\nmain alone: {main_only}"
    );
    assert!(
        joined.len() > main_only.len() + 3,
        "and a second half exists"
    );
}

/// Two ways to be stale, reported apart because they mean different things: the
/// document moved on, or this one line did. A `didOpen` that replaces a
/// document keeps its version, which is why the hash is not redundant.
#[test]
fn a_stale_version_and_a_stale_line_are_both_refused() {
    let mut client = open(HEARTBEAT);
    let held = lenses(&mut client)[0].clone();
    client.did_change_full(URI, HEARTBEAT, 2);
    assert_eq!(
        client
            .request("codeLens/resolve", held)
            .expect_err("refused"),
        harness::CONTENT_MODIFIED,
        "the version moved"
    );

    let mut fresh = open(HEARTBEAT);
    let mut tampered = lenses(&mut fresh)[0].clone();
    tampered["data"]["hash"] = json!("0000000000000000");
    assert_eq!(
        fresh
            .request("codeLens/resolve", tampered)
            .expect_err("refused"),
        harness::CONTENT_MODIFIED,
        "the line changed"
    );
    fresh.shutdown();
    client.shutdown();
}

/// `codeLens.resolveSupport` is `@since 3.18.0` and almost nothing sends it, so
/// silence cannot mean refusal — that would withhold the default summary
/// channel from every client there is. Only an explicit list without `command`
/// is a client saying it will not ask.
#[test]
fn the_provider_is_withheld_only_from_a_client_that_says_it_will_not_resolve() {
    let advertised = |caps: Value| {
        let mut client = TestClient::start();
        let result = client.initialize_caps(caps);
        let has = result.pointer("/capabilities/codeLensProvider").is_some();
        client.shutdown();
        has
    };

    assert!(advertised(json!({})), "silence is not refusal");
    assert!(advertised(json!({
        "textDocument": { "codeLens": { "resolveSupport": { "properties": ["command"] } } }
    })));
    assert!(
        !advertised(json!({
            "textDocument": { "codeLens": { "resolveSupport": { "properties": ["range"] } } }
        })),
        "a list that omits `command` is a refusal"
    );
}

/// Withholding the provider takes the summary away from a client whose
/// configuration still asks for it — and the other channel is off by default,
/// so without this the summary would simply be absent, with nothing said.
#[test]
fn a_client_that_loses_the_lens_channel_is_told_which_setting_restores_it() {
    let mut client = TestClient::start();
    client.initialize_caps(json!({
        "general": { "positionEncodings": ["utf-8"] },
        "textDocument": { "codeLens": { "resolveSupport": { "properties": ["range"] } } },
    }));
    let logged = client.wait_for("window/logMessage");
    let text = logged["message"].as_str().expect("text");
    assert!(
        text.contains("messageSummary.channels.inlayHints"),
        "{text}"
    );
    client.shutdown();

    // And nothing is said to a client that keeps the channel.
    let mut fine = TestClient::start();
    fine.initialize_caps(json!({ "general": { "positionEncodings": ["utf-8"] } }));
    fine.did_open(URI, HEARTBEAT, 1);
    assert!(
        !fine
            .drain_server_methods(std::time::Duration::from_millis(200))
            .contains(&"window/logMessage".to_owned()),
        "a client that resolves commands is told nothing"
    );
    fine.shutdown();
}

/// Unlike an inlay hint, a lens the server cannot place is an error: a lens
/// without its title is a blank annotation in the gutter, and `ContentModified`
/// is the protocol's way of telling a client to ask again.
#[test]
fn a_resolve_it_cannot_trust_is_refused() {
    let mut client = open(HEARTBEAT);
    let lens = lenses(&mut client)[0].clone();

    let mut without_data = lens.clone();
    without_data
        .as_object_mut()
        .expect("an object")
        .remove("data");
    let mut malformed = lens.clone();
    malformed["data"] = json!("not an object");
    let mut no_line = lens.clone();
    no_line["data"]["line"] = Value::Null;
    let mut past_the_end = lens.clone();
    past_the_end["data"]["line"] = json!(99);

    for (what, sent) in [
        ("no data", without_data),
        ("data that is not an object", malformed),
        ("no line", no_line),
        ("a line the document does not have", past_the_end),
    ] {
        assert_eq!(
            client.request("codeLens/resolve", sent).expect_err(what),
            harness::CONTENT_MODIFIED,
            "{what}"
        );
    }

    client.did_close(URI);
    assert_eq!(
        client
            .request("codeLens/resolve", lens)
            .expect_err("the document is closed"),
        harness::CONTENT_MODIFIED
    );
    client.shutdown();
}
