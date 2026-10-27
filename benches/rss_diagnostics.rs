//! One complete diagnostics publish cycle, measured against the running server.
//!
//! The spec's Stage 4a acceptance. Driven through the real protocol rather than
//! by calling the walker: what is being gated is the whole cycle — the open
//! document, the walk, the accumulated set and the published payload — and only
//! the server holds all four at once.
//!
//! The corpus is `common`'s **generated 100 000-line** one, not the "90k-line
//! log" the spec sentence names; that number predates the generated corpus.
//!
//! The numbers asserted are a **regression baseline**, not an independently
//! justified budget: nothing in the design says what a diagnostics cycle ought
//! to cost. They were set from an honest measurement and are here to notice a
//! change, not to certify a limit.
//!
//! Its own target for the reason `common` gives.

mod common;
#[path = "support/config_ready.rs"]
mod config_ready;

use common::{corpus, report, rss};
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use serde_json::{json, Value};

/// Everything the server does with the document: parse the notification, build
/// the `Document`, walk it, publish. Measured 2026-09-02 at 7.5–7.9x of the
/// file — three runs, and the spread is real rather than the byte-identical
/// numbers the smaller payload used to give.
///
/// It was 2.2x until `limits.diagnosticsPerDocument` was raised from 2000 to
/// 10 000 the same day. Nothing about the walk changed; five times the
/// published set is what moved it, which is the shape this gate has always
/// had — the walk retains nothing and the payload is everything.
///
/// What the number is made of is worth knowing before changing it. The walk
/// itself retains nothing (`rss_transient` gates that at 0.05x), so it is the
/// `Document` plus the *published payload* — at the default cap of 2000, one
/// set is 2001 diagnostics and 400 KB of JSON, and the high-water mark covers
/// the sets built during the walk.
///
/// The baseline sits where it does because of what the previous one missed.
/// It used to be taken *after* `didOpen` had been handled, on the argument that
/// a baseline before it would gate the payload path rather than the walk. That
/// argument let a regression through as a threefold improvement: deserialising
/// `didOpen` from a borrowed `Value` copied the document's own text, 16 MB of
/// it, and because the copy happened before the baseline it *shrank* the delta
/// from 1.44x to 0.44x while the peak stayed flat. A delta gate can always be
/// passed by moving allocation earlier, so the baseline now sits just before
/// the document reaches the server — the harness's own copy of the corpus is
/// already resident and stays on the baseline side.
const GATE: f64 = 8.7;

/// And the peak, which is what makes the delta undeceivable: a delta cannot be
/// shrunk by moving allocation earlier without this noticing. Coarser on
/// purpose — it counts the harness's copy of the corpus too. Measured 9.3x at
/// the 10 000 cap (9.76–9.78x over three runs); it was 4.5x at 2000.
const PEAK_GATE: f64 = 10.2;

/// What opening the document costs, before the walk starts. Measured 0.70x:
/// the `Document` and its line vector, and nothing else.
///
/// This is the gate the copy above would have failed — it took `didOpen`'s
/// text to 1.72x. Neither of the other two sees it: the copy is freed before
/// the walk sets the high-water mark, so it never reaches the peak, and it
/// falls between the other baseline and its reading either way. A transient
/// that large is worth gating on its own, being one copy of the file on every
/// open.
const OPEN_GATE: f64 = 1.1;

const URI: &str = "file:///bench.fix";

fn main() {
    let (server_conn, client) = Connection::memory();
    let server = std::thread::spawn(move || venue_reader::server::main_loop(server_conn));

    request(
        &client,
        1,
        "initialize",
        json!({ "capabilities": {
            "general": { "positionEncodings": ["utf-8"] },
            "window": { "workDoneProgress": true },
        }}),
    );
    notify(&client, "initialized", json!({}));

    let bytes = corpus();
    let file = bytes.len();
    let text = String::from_utf8(bytes).expect("the generated corpus is ASCII");

    let params = json!({ "textDocument": {
        "uri": URI, "languageId": "fix", "version": 1, "text": text,
    }});
    // Built first and measured second: the corpus buffer the harness holds is
    // its own cost, and counting it would gate the bench rather than the
    // server. Everything after this line is the server's.
    let before = rss();
    notify(&client, "textDocument/didOpen", params);
    let mut count = 0usize;
    let mut payload = 0usize;
    // Describe waits for configuration without analyzing or retaining corpus text.
    config_ready::wait(&client, URI, |note| {
        if note.method == "textDocument/publishDiagnostics" {
            count = note.params["diagnostics"].as_array().map_or(0, Vec::len);
            payload = note.params.to_string().len();
        }
    });
    let opened = rss();

    // Drain as it arrives, keeping only the latest set: letting the channel
    // back up would measure a queue instead of a document.
    //
    // Done when the server goes quiet. Not a fixed timer, which would gate
    // whatever the machine happened to finish in time — this waits for as long
    // as the server keeps talking. `$/progress` `end` used to be the marker and
    // is not one any more: a walk shorter than the report cadence deliberately
    // announces nothing, and this corpus is walked in ~170 ms.
    // Counted and dropped, never kept: a real client is another process, and
    // holding the payload here would charge the server for the harness.
    while let Ok(message) = client
        .receiver
        .recv_timeout(std::time::Duration::from_millis(500))
    {
        match message {
            Message::Request(req) => {
                // Whatever it asks for — the progress token, configuration —
                // answer at once, so the walk is never waiting on the client.
                let _ = client
                    .sender
                    .send(Message::Response(Response::new_ok(req.id, Value::Null)));
            }
            Message::Notification(note) if note.method == "textDocument/publishDiagnostics" => {
                count = note.params["diagnostics"].as_array().map_or(0, Vec::len);
                payload = note.params.to_string().len();
            }
            Message::Notification(_) => {}
            Message::Response(_) => {}
        }
    }

    let after = rss();
    println!("diagnostics cycle: {count} diagnostics published, last set {payload} bytes");
    report("diagnostics_open", before, opened, file, OPEN_GATE);
    report("diagnostics_cycle", before, after, file, GATE);
    report("diagnostics_peak", Some(0), after, file, PEAK_GATE);

    notify(&client, "exit", Value::Null);
    drop(client);
    let _ = server.join();
}

fn request(client: &Connection, id: i32, method: &str, params: Value) {
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
            Message::Response(_) => return,
            _ => continue,
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
