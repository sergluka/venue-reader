//! What a whole-document `textDocument/codeLens` costs, at exactly the cap.
//!
//! The stage's premise, measured before anything is built on it. Stage 4b
//! declined a worker pool on the grounds that classification is cheap — a claim
//! that had never been measured, and whose mechanism was misdescribed twice
//! before this bench existed.
//!
//! **At the cap, not over it.** A document larger than `limits.codeLensLines`
//! is answered `[]` without classifying anything, so benchmarking one would
//! have measured a fast path that does no work and called it fast.
//!
//! **Three numbers, not one**, because they fail for different reasons and only
//! one of them is about scheduling:
//!
//! - *main loop* — classification and lens construction inside the loop. Over
//!   this and the loop is unresponsive; a pool or a cheaper classifier is the
//!   answer.
//! - *payload* — the serialized response. Over this and the cap is wrong, not
//!   the architecture.
//! - *payload serialisation* — `answer.to_string()`, which is the bulk of what
//!   the writer thread will do. **Not** end to end: `Connection::memory()` has
//!   no writer, so the response envelope, the writer's scheduling, the framing
//!   and stdio itself are outside every number here. This bench cannot say what
//!   a client waits for; it says what the loop is busy with and how big the
//!   answer is.
//!
//! **Only the payload is asserted.** An earlier draft of this comment declared
//! a 20 ms main-loop threshold, derived from the 80 ms diagnostics debounce —
//! but no assertion ever enforced it, so classification and lens construction
//! could become arbitrarily slow with every gate still green. The threshold is
//! withdrawn rather than left standing as a claim nothing checks; item 12 is
//! why it was never enforced, and `worker-pool-timing.md` is where the decision
//! it was meant to inform actually lives.
//!
//! **Then the burst.** The list is one request; resolving what it returned is
//! `AT_CAP` of them, each analysing a line and dropping it. That is head-of-line
//! blocking rather than memory, and the list-only figure cannot see it — so the
//! per-resolve cost is measured on a sample and the whole-list cost derived
//! from it, which is what a client resolving everything would queue behind.

mod common;
#[path = "support/config_ready.rs"]
mod config_ready;

use std::time::Instant;

use common::generate_lines;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use serde_json::{json, Value};

/// The default `limits.codeLensLines`, measured exactly at it.
const AT_CAP: usize = 20_000;

/// The one number gated. Time is reported, not asserted — see the note by the
/// assertion.
const PAYLOAD_MB: usize = 4;

const URI: &str = "file:///bench.fix";

fn main() {
    let text = String::from_utf8(generate_lines(AT_CAP)).expect("the corpus is ASCII");
    let (server_conn, client) = Connection::memory();
    let server = std::thread::spawn(move || fix_lsp::server::main_loop(server_conn));

    request(
        &client,
        1,
        "initialize",
        json!({ "capabilities": { "general": { "positionEncodings": ["utf-8"] } } }),
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
        3,
        "textDocument/codeLens",
        json!({ "textDocument": { "uri": URI } }),
    );
    let elapsed = started.elapsed();

    // Where the time went, because "over the threshold" does not say what to
    // fix. Classification is what a cheaper `classify` would replace; if it is
    // a small share, replacing it buys nothing and the cost is the payload.
    let classified = Instant::now();
    let mut messages = 0usize;
    for line in text.split('\n') {
        if fix_lsp::codec::tokenize(line.as_bytes()).kind == fix_lsp::codec::LineKind::Message {
            messages += 1;
        }
    }
    let classifying = classified.elapsed();
    println!("  of which classification: {classifying:?} for {messages} messages");

    // The bulk of what the writer thread will do with this response.
    // `Connection::memory()` passes a `Message` through a channel and never
    // serialises, so this cost is invisible to the round trip above — it is
    // latency the client waits for, but not time the main loop is blocked. It
    // is the result value only: the envelope and the framing are not here.
    let serialising = Instant::now();
    let payload = answer.to_string().len();
    let wire = serialising.elapsed();

    let lenses = answer.as_array().map_or(0, Vec::len);
    println!(
        "lens_list at {AT_CAP} lines: {lenses} lenses, payload {} KB\n           round trip {elapsed:?}, of which classification {classifying:?}; \
         serialising would add {wire:?}",
        payload / 1024
    );
    assert!(lenses == AT_CAP, "a lens per line: {lenses}");
    // The payload is gated; the timings are printed and not.
    //
    // Work-order item 12 is why: a wall-clock figure taken inside an agent
    // session moves 30% between runs of the same binary, and this one measured
    // 96–127 ms across three. A gate that flaps is worse than none. What the
    // numbers were for was the decision recorded in `worker-pool-timing.md`,
    // and that decision has been taken: ~40 ms of blocked main loop, accepted.
    // These lines exist to notice a change of shape — classification becoming
    // the expensive third, say — not to pass or fail a build.
    assert!(
        payload <= PAYLOAD_MB * 1024 * 1024,
        "payload {payload} bytes exceeds the {PAYLOAD_MB} MB threshold"
    );

    resolve_burst(&client, answer.as_array().expect("an array"));

    notify(&client, "exit", Value::Null);
    drop(client);
    let _ = server.join();
}

/// What resolving costs, and what resolving *everything* would.
///
/// A sample rather than all 20 000: the per-resolve cost is what the number is
/// for, and the total is that times the count. Printed, not gated — the same
/// reason the timings above are.
fn resolve_burst(client: &Connection, lenses: &[Value]) {
    const SAMPLE: usize = 500;

    let started = Instant::now();
    let mut titles = 0usize;
    for (nth, lens) in lenses.iter().take(SAMPLE).enumerate() {
        let resolved = request(client, 1000 + nth as i32, "codeLens/resolve", lens.clone());
        titles += usize::from(resolved["command"]["title"].is_string());
    }
    let elapsed = started.elapsed();
    let each = elapsed / SAMPLE as u32;
    assert_eq!(titles, SAMPLE, "every sampled lens came back with a title");
    println!(
        "  resolve burst: {SAMPLE} lenses in {elapsed:?} ({each:?} each) \
         — all {} would queue for {:?}",
        lenses.len(),
        each * lenses.len() as u32,
    );
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
