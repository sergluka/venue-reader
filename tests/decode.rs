//! Row attribution and refusal behavior through generic LSP events.

mod harness;

use harness::{TestClient, Workspace, CONTENT_MODIFIED, INVALID_PARAMS, REQUEST_FAILED};
use serde_json::{json, Value};

const URI: &str = "file:///decode.fix";
const HEARTBEAT: &str = "8=FIX.4.4|35=0|";
const DECODED_TYPE: &str = "35(MsgType)=0(Heartbeat)";

fn range(first: u32, start: u32, last: u32, end: u32) -> Value {
    json!({"start":{"line":first,"character":start},"end":{"line":last,"character":end}})
}

fn params(uri: &str, version: i32, ranges: Value) -> Value {
    json!({"command":"fix.decode","arguments":[{
        "textDocument":{"uri":uri,"version":version},"ranges":ranges
    }]})
}

fn start(text: &str, settings: Value) -> TestClient {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities":{"general":{"positionEncodings":["utf-8"]}},
        "initializationOptions":{"fix":settings}
    }));
    client.did_open(URI, text, 1);
    client
}

fn decode(client: &mut TestClient, ranges: Value) -> Result<Value, i32> {
    client.request("workspace/executeCommand", params(URI, 1, ranges))
}

fn result(lines: &[&str]) -> Value {
    json!({
        "textDocument":{"uri":URI,"version":1},
        "results":[{"rangeIndex":0,"lines":lines.iter().enumerate().map(|(number,text)|
            json!({"line":number,"text":text})).collect::<Vec<_>>()}]
    })
}

#[test]
fn every_range_and_empty_source_row_keeps_its_identity() {
    let mut client = start(
        &format!("{HEARTBEAT}\n\n   \nnot a message\n{HEARTBEAT}\n"),
        json!({}),
    );
    let answer = decode(
        &mut client,
        json!([
            range(4, 10, 4, 14),
            range(1, 0, 1, 0),
            range(0, 9, 0, 10),
            range(3, 0, 3, 13),
            range(0, 11, 0, 11),
            range(0, 10, 0, 11)
        ]),
    )
    .unwrap();
    assert_eq!(
        answer,
        json!({
            "textDocument":{"uri":URI,"version":1},
            "results":[
                {"rangeIndex":0,"lines":[{"line":4,"text":DECODED_TYPE}]},
                {"rangeIndex":1,"lines":[{"line":1,"text":""}]},
                {"rangeIndex":2,"lines":[{"line":0,"text":""}]},
                {"rangeIndex":3,"lines":[{"line":3,"text":""}]},
                {"rangeIndex":4,"lines":[{"line":0,"text":DECODED_TYPE}]},
                {"rangeIndex":5,"lines":[{"line":0,"text":DECODED_TYPE}]}
            ]
        })
    );
    assert_eq!(
        decode(&mut client, json!([range(3, 0, 0, 10)])).unwrap(),
        result(&[DECODED_TYPE, "", "", ""])
    );
    let trailing = decode(&mut client, json!([range(4, 10, u32::MAX, 0)])).unwrap();
    assert_eq!(
        trailing["results"][0]["lines"],
        json!([{"line":4,"text":DECODED_TYPE},{"line":5,"text":""}])
    );
    assert_eq!(
        decode(&mut client, json!([range(6, 0, 6, 0)])),
        Err(INVALID_PARAMS)
    );
    client.shutdown();
}

#[test]
fn an_empty_document_has_one_explicit_empty_row() {
    let mut client = start("", json!({}));
    assert_eq!(
        decode(&mut client, json!([range(0, 0, 0, 0)])).unwrap(),
        result(&[""])
    );
    client.shutdown();
}

#[test]
fn unicode_selections_use_the_negotiated_encoding_and_existing_field_spelling() {
    for encoding in ["utf-8", "utf-16"] {
        let mut client = TestClient::start();
        let initialized = client.initialize(&[encoding]);
        assert!(
            initialized["capabilities"]["executeCommandProvider"]["commands"]
                .as_array()
                .unwrap()
                .contains(&json!("fix.decode"))
        );
        assert_eq!(initialized["capabilities"]["positionEncoding"], encoding);
        client.did_open(URI, "8=FIX.4.4|58=é😀|35=0|9999=value|", 1);
        let (type_start, emoji_start, emoji_end) = if encoding == "utf-8" {
            (20, 15, 19)
        } else {
            (17, 14, 16)
        };
        let ranges = json!([
            range(0, type_start, 0, type_start + 1),
            range(0, emoji_start, 0, emoji_end)
        ]);
        let answer = decode(&mut client, ranges.clone()).unwrap();
        assert_eq!(answer["results"][0]["lines"][0]["text"], DECODED_TYPE);
        assert_eq!(answer["results"][1]["lines"][0]["text"], "58(Text)=é😀");
        client.shutdown();
    }
}

#[test]
fn malformed_envelopes_and_ranges_are_invalid_params() {
    let mut client = start(HEARTBEAT, json!({}));
    for arguments in [
        json!([]),
        json!([URI, []]),
        json!([{}]),
        json!([{"textDocument":{"uri":URI},"ranges":[range(0,10,0,14)]}]),
        json!([{"textDocument":{"uri":URI,"version":null},"ranges":[range(0,10,0,14)]}]),
        json!([{"textDocument":{"uri":URI,"version":1},"ranges":[]}]),
        json!([{"textDocument":{"uri":URI,"version":1},"ranges":"bad"}]),
        json!([{"textDocument":{"uri":URI,"version":1},"ranges":[null]}]),
        json!([{"textDocument":{"uri":URI,"version":1},"ranges":[{"start":{"line":-1,"character":0},"end":{"line":0,"character":1}}]}]),
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

#[test]
fn range_count_and_work_limits_have_explicit_validation_precedence() {
    let mut client = start(
        &format!("{HEARTBEAT}\n{HEARTBEAT}"),
        json!({"limits":{"rangeLines":2}}),
    );
    assert_eq!(
        decode(&mut client, json!([range(0, 10, 1, 0)])).unwrap(),
        result(&[DECODED_TYPE, ""])
    );
    assert!(decode(
        &mut client,
        json!([range(0, 11, 0, 11), range(0, 11, 0, 11)])
    )
    .is_ok());
    assert_eq!(
        decode(
            &mut client,
            json!([range(0, 10, 1, 0), range(0, 10, 0, 14)])
        ),
        Err(REQUEST_FAILED)
    );
    assert_eq!(
        decode(&mut client, json!([null, null, null])),
        Err(REQUEST_FAILED)
    );
    assert_eq!(decode(&mut client, json!([null])), Err(INVALID_PARAMS));
    assert_eq!(
        client.request(
            "workspace/executeCommand",
            params(URI, 0, json!([null, null, null]))
        ),
        Err(CONTENT_MODIFIED)
    );
    assert_eq!(
        client.request(
            "workspace/executeCommand",
            params("file:///closed.fix", 1, json!([null]))
        ),
        Err(REQUEST_FAILED)
    );
    client.shutdown();
    let mut disabled = start(HEARTBEAT, json!({"limits":{"rangeLines":0}}));
    assert_eq!(
        decode(&mut disabled, json!([range(0, 10, 0, 14)])),
        Err(REQUEST_FAILED)
    );
    disabled.shutdown();
}

#[test]
fn edits_close_and_reopen_are_checked_against_the_requested_version() {
    let mut client = start(HEARTBEAT, json!({}));
    client.did_change_full(URI, "8=FIX.4.4|35=1|", 2);
    assert_eq!(
        decode(&mut client, json!([range(0, 10, 0, 14)])),
        Err(CONTENT_MODIFIED)
    );
    let current = client
        .request(
            "workspace/executeCommand",
            params(URI, 2, json!([range(0, 10, 0, 14)])),
        )
        .unwrap();
    assert_eq!(current["textDocument"]["version"], 2);
    assert_eq!(
        current["results"][0]["lines"][0]["text"],
        "35(MsgType)=1(TestRequest)"
    );
    client.did_close(URI);
    assert_eq!(
        decode(&mut client, json!([range(0, 10, 0, 14)])),
        Err(REQUEST_FAILED)
    );
    client.did_open(URI, HEARTBEAT, 2);
    let reopened = client
        .request(
            "workspace/executeCommand",
            params(URI, 2, json!([range(0, 10, 0, 14)])),
        )
        .unwrap();
    assert_eq!(reopened["results"][0]["lines"][0]["text"], DECODED_TYPE);
    client.shutdown();
}

#[test]
fn unavailable_configuration_is_reported_before_parsing_range_elements() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.did_open_unready(URI, HEARTBEAT, 1);
    assert_eq!(decode(&mut client, json!([null])), Err(REQUEST_FAILED));
    client.shutdown();
}

#[test]
fn result_byte_limit_counts_escaped_json_and_accepts_the_exact_boundary() {
    let settings = json!({"inlayHints":{"tagNames":{"enabled":false,"format":"\n\"{}\\{}"},"enumValues":{"enabled":false,"format":"[{}]"}}});
    let ranges = json!([range(0, 10, 0, 14)]);
    let mut baseline = start(HEARTBEAT, settings.clone());
    let expected = decode(&mut baseline, ranges.clone()).unwrap();
    assert_eq!(
        expected["results"][0]["lines"][0]["text"],
        "35\n\"MsgType\\MsgType=0[Heartbeat]"
    );
    baseline.shutdown();
    let needed = serde_json::to_vec(&expected).unwrap().len();
    for (cap, accepted) in [(needed, true), (needed - 1, false), (1, false), (0, false)] {
        let mut config = settings.clone();
        config["limits"] = json!({"decodeResultBytes":cap});
        let mut client = start(HEARTBEAT, config);
        let answer = decode(&mut client, ranges.clone());
        if accepted {
            assert_eq!(answer.unwrap(), expected);
        } else {
            assert_eq!(answer, Err(REQUEST_FAILED));
        }
        client.shutdown();
    }
}

#[test]
fn empty_metadata_and_large_values_cannot_bypass_the_result_limit() {
    for (text, limit) in [
        (String::new(), 32),
        (format!("8=FIX.4.4|58={}|", "x".repeat(1024)), 256),
    ] {
        let mut client = start(&text, json!({"limits":{"decodeResultBytes":limit}}));
        assert_eq!(
            decode(&mut client, json!([range(0, 0, 0, u32::MAX)])),
            Err(REQUEST_FAILED)
        );
        client.shutdown();
    }
    let mut client = start(
        HEARTBEAT,
        json!({
            "limits":{"decodeResultBytes":256},
            "inlayHints":{"tagNames":{"format":"{}".repeat(100)}}
        }),
    );
    assert_eq!(
        decode(&mut client, json!([range(0, 10, 0, 14)])),
        Err(REQUEST_FAILED)
    );
    client.shutdown();
}

#[test]
fn unknown_fields_group_paths_and_fixt_fields_have_explicit_rendering() {
    for (message,expected) in [
        ("8=FIX.4.4|35=0|9999=synthetic|", "8(BeginString)=FIX.4.4|35(MsgType)=0(Heartbeat)|9999=synthetic"),
        ("8=FIX.4.4|35=W|268=1|269=0|270=1|", "8(BeginString)=FIX.4.4|35(MsgType)=W(MarketDataSnapshotFullRefresh)|268(NoMDEntries)=1|269(NoMDEntries/1/MDEntryType)=0(Bid)|270(NoMDEntries/1/MDEntryPx)=1"),
        ("8=FIXT.1.1|35=D|1128=7|54=1|", "8(BeginString)=FIXT.1.1|35(MsgType)=D(NewOrderSingle)|1128(ApplVerID)=7(FIX50)|54(Side)=1(Buy)"),
    ] {
        let mut client = start(message, json!({}));
        let answer = decode(&mut client, json!([range(0,0,0,u32::MAX)])).unwrap();
        assert_eq!(answer, result(&[expected]));
        client.shutdown();
    }
}

#[test]
fn a_committed_render_change_applies_without_a_document_version_change() {
    let mut client = start(HEARTBEAT, json!({}));
    let ranges = json!([range(0, 10, 0, 14)]);
    assert_eq!(
        decode(&mut client, ranges.clone()).unwrap()["results"][0]["lines"][0]["text"],
        DECODED_TYPE
    );
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"fix":{"inlayHints":{"tagNames":{"format":"[{}]"}}}}}),
    );
    client.wait_effective(URI, "inlayHints.tagNames.format", json!("[{}]"));
    let answer = decode(&mut client, ranges).unwrap();
    assert_eq!(answer["textDocument"]["version"], 1);
    assert_eq!(
        answer["results"][0]["lines"][0]["text"],
        "35[MsgType]=0(Heartbeat)"
    );
    client.shutdown();
}

#[test]
fn local_settings_cannot_override_the_global_result_budget() {
    let workspace = Workspace::new("decode-budget-scope");
    workspace.write(".venue-reader.toml", "limits.decodeResultBytes = 1\n");
    let uri = workspace.uri("messages.fix");
    let mut client = workspace.client(json!({"limits":{"decodeResultBytes":2048}}));
    let report = client.did_open(
        &uri,
        &format!("# fix: limits.decodeResultBytes=0\n{HEARTBEAT}"),
        1,
    );
    assert_eq!(report["effective"]["limits.decodeResultBytes"], 2048);
    let warnings = report["warnings"].as_array().unwrap();
    assert_eq!(
        warnings
            .iter()
            .filter(|warning| warning["key"] == "limits.decodeResultBytes"
                && warning["message"].as_str().unwrap().contains("global-only"))
            .count(),
        2,
        "{warnings:?}"
    );
    assert!(client
        .request(
            "workspace/executeCommand",
            params(&uri, 1, json!([range(1, 10, 1, 14)]))
        )
        .is_ok());
    client.shutdown();
    let mut defaults = start(HEARTBEAT, json!({}));
    assert_eq!(
        defaults.describe(URI).unwrap()["effective"]["limits.decodeResultBytes"],
        8 * 1024 * 1024
    );
    defaults.shutdown();
}

#[test]
fn cancellation_after_dispatch_produces_no_duplicate_response() {
    let mut client = start(HEARTBEAT, json!({}));
    let request = client.send_request(
        "workspace/executeCommand",
        params(URI, 1, json!([range(0, 10, 0, 14)])),
    );
    client.notify("$/cancelRequest", json!({"id":request}));
    let barrier = client.send_request(
        "workspace/executeCommand",
        json!({"command":"fix.describeOverrides","arguments":[URI]}),
    );
    let responses = client.responses_through_barrier(&barrier);
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0].id, request);
    assert_eq!(
        responses[0].response_result.as_ref().unwrap(),
        &result(&[DECODED_TYPE])
    );
    assert_eq!(responses[1].id, barrier);
    client.notify("$/cancelRequest", json!({"id":request}));
    let next = client.send_request(
        "workspace/executeCommand",
        json!({"command":"fix.describeOverrides","arguments":[URI]}),
    );
    let responses = client.responses_through_barrier(&next);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].id, next);
    assert!(responses[0].response_result.is_ok());
    client.shutdown();
}
