//! What a dense `textDocument/inlayHint` response weighs, at the range cap.
//!
//! §4 asks for this figure specifically, and for one decision from it: `data`
//! is repeated per hint, so if the URI dominates it, a short document id should
//! replace it. A dense viewport is where that shows — every tag labelled, every
//! enum labelled, and a summary on every line, which is more than any default
//! draws.
//!
//! **The `data` share is measured, not estimated.** A hint is a position, a
//! label and its `data`; only the last is the server's to shrink.
//!
//! **Twice, because tooltips dominate.** A client that resolves tooltips gets a
//! label and a `data`; one that does not gets the whole hover markdown inline,
//! which is the adaptation §4 prescribes and by far the larger response. Both
//! are measured; the gate is on the eager shape, which is the ceiling.

mod common;
#[path = "support/config_ready.rs"]
mod config_ready;

use std::time::Instant;

use common::generate_lines;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use serde_json::{json, Value};

/// The default `limits.rangeLines`, measured exactly at it.
const AT_CAP: u32 = 2_000;

/// Gated, unlike the timings: a response this size is the cap being wrong
/// rather than the architecture. Roughly twice the measured figures, so it
/// fails on a change of shape and not on noise.
const DEFERRED_MB: usize = 16;
const EAGER_MB: usize = 48;

/// The share above which §4 says to replace the URI with a short document id.
/// **Measured over it**, once the fixture stopped using a 20-character path:
/// 36% of a deferred response. So this is no longer the gate on a question —
/// the question is answered — and the assertion below is only a tripwire for
/// the share moving again.
const URI_SHARE: f64 = 0.25;
const URI_SHARE_TRIPWIRE: f64 = 0.50;

/// Deliberately a plausible absolute path, not `file:///bench.fix`. The URI is
/// repeated once per hint, so its share of the payload is exactly as long as
/// the path is — and a short fixture would settle the document-id question in
/// its own favour by construction.
const URI: &str =
    "file:///home/engineer/work/venue-connectivity/logs/2026-09-03/session-oe-prod-01.fix";

fn main() {
    let deferred = measure(true);
    let eager = measure(false);
    println!(
        "  the tooltip is {:.0}% of an eager response — {} KB of hover markdown \
         a resolving client never receives up front",
        100.0 * (eager.payload - deferred.payload) as f64 / eager.payload as f64,
        (eager.payload - deferred.payload) / 1024,
    );

    assert!(
        deferred.payload <= DEFERRED_MB * 1024 * 1024,
        "deferred payload {} bytes exceeds the {DEFERRED_MB} MB threshold",
        deferred.payload
    );
    assert!(
        eager.payload <= EAGER_MB * 1024 * 1024,
        "eager payload {} bytes exceeds the {EAGER_MB} MB threshold",
        eager.payload
    );
    // Taken against the smaller of the two, where the URI's share is largest
    // because there is least else to carry.
    let share = deferred.uris as f64 / deferred.payload as f64;
    if share >= URI_SHARE {
        println!(
            "  the URI is {:.0}% of a deferred response, over the {:.0}% §4 set: \
             a short document id is warranted",
            100.0 * share,
            100.0 * URI_SHARE,
        );
    }
    assert!(
        share < URI_SHARE_TRIPWIRE,
        "the URI is {:.0}% of a deferred response — past even the tripwire",
        100.0 * share
    );
}

struct Measured {
    payload: usize,
    uris: usize,
}

fn measure(defers_tooltips: bool) -> Measured {
    let text = String::from_utf8(generate_lines(AT_CAP as usize)).expect("the corpus is ASCII");
    let (server_conn, client) = Connection::memory();
    let server = std::thread::spawn(move || fix_lsp::server::main_loop(server_conn));

    request(
        &client,
        1,
        "initialize",
        json!({
            "capabilities": {
                "general": { "positionEncodings": ["utf-8"] },
                "textDocument": { "inlayHint": { "resolveSupport": {
                    "properties": if defers_tooltips { vec!["tooltip"] } else { Vec::new() },
                }}},
            },
            // Everything drawn, which no default does: the point is the
            // ceiling. The summary channel needs saying too — it is off by
            // default, and this bench used to promise a summary per line in its
            // documentation while measuring a viewport that had none.
            "initializationOptions": { "fix": {
                "inlayHints": {
                    "tagNames": { "enabled": true },
                    "enumValues": { "enabled": true },
                    "groupPaths": { "enabled": true },
                },
                "messageSummary": { "channels": { "inlayHints": true } },
            }},
        }),
    );
    notify(&client, "initialized", json!({}));
    notify(
        &client,
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": URI, "languageId": "fix", "version": 1, "text": text,
        }}),
    );

    config_ready::wait(&client, URI, |_| {});
    let started = Instant::now();
    let answer = request(
        &client,
        2,
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": URI },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": AT_CAP, "character": 0 },
            },
        }),
    );
    let elapsed = started.elapsed();

    let hints = answer.as_array().expect("an array");
    let payload = answer.to_string().len();
    let data: usize = hints
        .iter()
        .map(|hint| hint["data"].to_string().len())
        .sum();
    // Every hint repeats it, so this is the whole of what a document id saves.
    let uris = hints.len() * (URI.len() + 2);

    println!(
        "hint_payload at {AT_CAP} lines, tooltips {}: {} hints, payload {} KB \
         ({} bytes/hint), of which data {} KB and uri {} KB ({:.0}%)\n\
         \x20          round trip {elapsed:?}",
        if defers_tooltips {
            "resolved"
        } else {
            "inline"
        },
        hints.len(),
        payload / 1024,
        payload / hints.len().max(1),
        data / 1024,
        uris / 1024,
        100.0 * uris as f64 / payload as f64,
    );

    assert!(!hints.is_empty(), "a dense viewport drew nothing");
    assert_eq!(
        hints
            .iter()
            .filter(|hint| hint["data"]["kind"] == "summary")
            .count(),
        AT_CAP as usize,
        "the ceiling is a summary per line, which is what the doc claims"
    );
    // Every field hint, not any: keeping one eager tooltip and dropping the
    // other 38 000 makes the payload *smaller* and would pass every bound below.
    let (eager, bare) = hints
        .iter()
        .filter(|hint| hint["data"]["kind"] != "summary")
        .fold((0usize, 0usize), |(eager, bare), hint| {
            match hint.get("tooltip") {
                Some(_) => (eager + 1, bare),
                None => (eager, bare + 1),
            }
        });
    let (want_eager, want_bare) = if defers_tooltips {
        (0, eager + bare)
    } else {
        (eager + bare, 0)
    };
    assert_eq!(
        (eager, bare),
        (want_eager, want_bare),
        "the client's resolveSupport decides this for every hint, not for one"
    );

    notify(&client, "exit", Value::Null);
    drop(client);
    let _ = server.join();
    Measured { payload, uris }
}

fn request(client: &Connection, id: i32, method: &str, params: Value) -> Value {
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(id),
            method.into(),
            params,
        )))
        .expect("send");
    loop {
        match client.receiver.recv().expect("a response") {
            Message::Response(response) => return response.response_result.unwrap_or(Value::Null),
            Message::Request(req) => {
                let _ = client
                    .sender
                    .send(Message::Response(Response::new_ok(req.id, Value::Null)));
            }
            Message::Notification(_) => {}
        }
    }
}

fn notify(client: &Connection, method: &str, params: Value) {
    client
        .sender
        .send(Message::Notification(Notification::new(
            method.into(),
            params,
        )))
        .expect("send");
}
