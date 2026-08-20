//! `workspace/executeCommand` through the protocol.
//!
//! What the in-crate tests cannot show is the shape on the wire: that the
//! command is advertised under the name a client would match against, that the
//! answer is a bare JSON string rather than an object wrapping one, and that
//! the two arguments survive a round trip as the client wrote them. Every
//! literal here is the protocol's or the server's documented argument shape,
//! never a constant taken from the crate.

mod harness;

use harness::{TestClient, INVALID_PARAMS, REQUEST_FAILED};
use serde_json::{json, Value};

const URI: &str = "file:///a.fix";

const HEARTBEAT: &str =
    "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";

fn open(lines: usize) -> TestClient {
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    let text = vec![HEARTBEAT; lines].join("\n");
    client.did_open(URI, &text, 1);
    client
}

fn range(start_line: u32, start_ch: u32, end_line: u32, end_ch: u32) -> Value {
    json!({
        "start": { "line": start_line, "character": start_ch },
        "end": { "line": end_line, "character": end_ch },
    })
}

fn decode(client: &mut TestClient, ranges: Value) -> Result<Value, i32> {
    client.request(
        "workspace/executeCommand",
        json!({ "command": "fix.decode", "arguments": [URI, ranges] }),
    )
}

#[test]
fn the_command_is_advertised_by_name() {
    let mut client = TestClient::start();
    let result = client.initialize(&["utf-8"]);
    assert_eq!(
        result["capabilities"]["executeCommandProvider"]["commands"],
        json!(["fix.decode", "fix.useDictionary", "fix.describeOverrides"]),
        "{result}"
    );
    client.shutdown();
}

/// The answer is the string itself: a client puts it straight into a register.
#[test]
fn two_ranges_come_back_as_one_string_in_the_order_asked() {
    let mut client = open(3);
    // `35=0` on line 2, then the same field on line 0 — ranges are not sorted.
    let answer = decode(
        &mut client,
        json!([range(2, 15, 2, 19), range(0, 15, 0, 19)]),
    )
    .expect("answered");
    assert_eq!(
        answer,
        json!("35(MsgType)=0(Heartbeat)\n35(MsgType)=0(Heartbeat)")
    );
    client.shutdown();
}

/// Nothing selected is the empty string, not `null` and not an error: a client
/// that asked about a blank region gets an empty register rather than a popup.
#[test]
fn a_selection_that_covers_no_field_is_an_empty_string() {
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    client.did_open(URI, "\n\n", 1);
    assert_eq!(
        decode(&mut client, json!([range(0, 0, 1, 0)])).expect("answered"),
        json!("")
    );
    client.shutdown();
}

/// The hint formats reach this channel and the hint switches do not: how a
/// field is spelled is one choice, whether it is drawn on screen is another.
#[test]
fn a_decode_is_written_by_the_hint_formats_and_not_their_switches() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": { "general": { "positionEncodings": ["utf-8"] } },
        "initializationOptions": { "fix": { "inlayHints": {
            "tagNames": { "enabled": false, "format": "[{}]" },
            "enumValues": { "enabled": false, "format": "->{}" },
        }}},
    }));
    client.did_open(URI, HEARTBEAT, 1);
    let answer = decode(&mut client, json!([range(0, 15, 0, 19)])).expect("answered");
    assert_eq!(answer, json!("35[MsgType]=0->Heartbeat"));
    client.shutdown();
}

#[test]
fn an_unknown_command_is_refused() {
    let mut client = open(1);
    let code = client
        .request(
            "workspace/executeCommand",
            json!({ "command": "fix.notACommand", "arguments": [URI, [range(0, 0, 0, 1)]] }),
        )
        .expect_err("refused");
    assert_eq!(code, INVALID_PARAMS);
    client.shutdown();
}

/// Every shape a client can get wrong, and none of them a panic: the protocol
/// types `arguments` as `any[]`, so this server is the only thing checking.
#[test]
fn a_malformed_argument_list_is_refused_whatever_is_wrong_with_it() {
    let mut client = open(1);
    let one_range = json!([range(0, 0, 0, 1)]);
    for arguments in [
        json!([URI]),                               // too few
        json!([URI, one_range, "extra"]),           // too many
        json!([]),                                  // none at all
        json!([7, one_range]),                      // a number where the uri goes
        json!([URI, "not a range"]),                // a string where the ranges go
        json!([URI, [{ "start": { "line": 0 } }]]), // a range missing its end
        json!([URI, []]),                           // nothing to decode
    ] {
        let code = client
            .request(
                "workspace/executeCommand",
                json!({ "command": "fix.decode", "arguments": arguments }),
            )
            .expect_err("refused");
        assert_eq!(code, INVALID_PARAMS, "{arguments}");
    }
    client.shutdown();
}

/// Past the bound the answer would be a register full of half a selection, so
/// the request fails and the message names the setting to raise.
#[test]
fn a_selection_past_the_bound_is_refused_and_says_which_setting() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": { "general": { "positionEncodings": ["utf-8"] } },
        "initializationOptions": { "fix": { "limits": { "rangeLines": 2 } } },
    }));
    client.did_open(URI, &[HEARTBEAT; 5].join("\n"), 1);
    let code = decode(&mut client, json!([range(0, 0, 4, 0)])).expect_err("refused");
    assert_eq!(code, REQUEST_FAILED, "well-formed arguments, too much work");
    client.shutdown();
}
