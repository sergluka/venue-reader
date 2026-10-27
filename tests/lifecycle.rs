mod harness;

use harness::TestClient;
use serde_json::{json, Value};

#[test]
fn initialize_negotiates_utf8_when_offered() {
    let mut client = TestClient::start();
    let res = client.initialize(&["utf-8", "utf-16"]);
    assert_eq!(res["capabilities"]["positionEncoding"], "utf-8");
    assert_eq!(res["capabilities"]["hoverProvider"], true);
    assert_eq!(res["capabilities"]["textDocumentSync"], 2);
    assert_eq!(res["serverInfo"]["name"], "venue-reader");
    client.shutdown();
}

#[test]
fn initialize_defaults_to_utf16() {
    let mut client = TestClient::start();
    let res = client.initialize(&["utf-16"]);
    assert_eq!(res["capabilities"]["positionEncoding"], "utf-16");
    client.shutdown();
}

/// The round trip, not the markdown — `tests/hover.rs` owns what it says.
#[test]
fn hover_answers_about_the_field_under_the_cursor() {
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    client.did_open("file:///a.fix", "8=FIX.4.4\u{1}35=D", 1);
    let res = client.hover("file:///a.fix", 0, 0).unwrap();
    assert!(res["contents"]["value"]
        .as_str()
        .unwrap()
        .starts_with("**BeginString** (8)"));
    client.shutdown();
}

#[test]
fn didopen_with_invalid_byte_survives() {
    // U+F780 represents raw byte 0x80 after transport sanitisation.
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    client.did_open("file:///b.fix", "A\u{F780}B", 1);
    // Answering `null` is the right answer here — nothing on that line is a
    // field. What this test is about is that the server is still there to say
    // so, having been handed a byte that is not text.
    let res = client.hover("file:///b.fix", 0, 0);
    assert!(res.is_ok(), "server still answers: {res:?}");
    client.shutdown();
}

#[test]
fn didchange_updates_document_without_crash() {
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    client.did_open("file:///d.fix", "abc", 1);
    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": "file:///d.fix", "version": 2 },
            "contentChanges": [{
                "range": { "start": {"line":0,"character":0}, "end": {"line":0,"character":1} },
                "text": "Z"
            }]
        }),
    );
    let res = client.hover("file:///d.fix", 0, 0);
    assert!(
        res.is_ok(),
        "the edit landed and the server answers: {res:?}"
    );
    client.shutdown();
}

#[test]
fn request_after_shutdown_is_invalid() {
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    client.request("shutdown", Value::Null).unwrap();
    let err = client.hover("file:///x.fix", 0, 0).unwrap_err();
    assert_eq!(err, -32600, "InvalidRequest");
    // `exit` + join happen in Drop.
}
