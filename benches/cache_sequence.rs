//! What the line cache is hit for, over a sequence a reader actually performs.
//!
//! Work-order item 16 asks for hit rate on a realistic sequence. This measures
//! it as a **count**, not a timing: item 12 established that wall-clock figures
//! on this machine move 70–80% between runs of the same binary, while a count
//! of cache lookups is immune to core class and neighbours.
//!
//! The client profile explicitly enables tag, enum and summary hints. Responses
//! must be non-empty before counters are read: empty responses with zero counts
//! would make a disabled channel look deterministic.
//!
//! **Two traces, because one of them flatters the cache.** `navigate` resolves
//! the few hints a reader inspects; `resolve-everything` resolves every hint of
//! a viewport. Aggregated together, the resolve flood would dominate the hit
//! count and read as navigation benefiting from the cache when it does not.
//!
//! **The bound does not reach this trace.** The loop sweeps after every
//! request, but forty screens of fifty lines is half the default
//! `limits.cachedLines`, so no sweep ever finds anything and every count
//! below is the cache's own. A trace over more lines than the bound would
//! measure the bound, which is a different question.
//!
//! **Recorded assumption:** dispatch is synchronous today, which is what keeps
//! these counts ordered and reproducible. The worker pool will invalidate that;
//! whoever adds it should re-read this bench.

mod common;
#[path = "support/config_ready.rs"]
mod config_ready;

use common::generate_lines;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use serde_json::{json, Value};
use venue_reader::document::stats;

/// Big enough that a scroll leaves most of it untouched, small enough that the
/// lens phase can see a list. Not the 100k corpus: the point of the lens phase
/// is the population contract, and the cap would answer `[]`.
const LINES: u32 = 10_000;

/// A screen. Not `limits.rangeLines` (2 000) — that is what the server
/// refuses to exceed, not what a client asks for.
const VIEWPORT: u32 = 50;
const SCREENS: u32 = 40;

/// Fields of one line a reader inspects, and then resolves the tooltip of.
const INSPECTED: usize = 6;
const HOVERS: usize = 8;

const URI: &str = "file:///bench.fix";

struct Phase {
    name: &'static str,
    hits: u64,
    misses: u64,
}

fn main() {
    for resolve_everything in [false, true] {
        let trace = if resolve_everything {
            "resolve-everything"
        } else {
            "navigate"
        };
        let phases = run(resolve_everything);

        println!("\n{trace}:");
        let (mut hits, mut misses) = (0u64, 0u64);
        for phase in &phases {
            println!(
                "  {:<22} {:>7} hits {:>7} misses",
                phase.name, phase.hits, phase.misses
            );
            hits += phase.hits;
            misses += phase.misses;
        }
        let looked = hits + misses;
        println!(
            "  {:<22} {hits} of {looked} lookups hit ({:.0}%)",
            "total",
            100.0 * hits as f64 / looked.max(1) as f64,
        );
        // Derived, not measured, and per shape because `benches/analyze.rs`
        // reports four: resolve traffic weights the field-dense ones. The two
        // bounds are that bench's cheapest and dearest lines.
        println!(
            "  {:<22} {:.1}–{:.1} ms of analysis avoided across the whole session (derived)",
            "saving",
            hits as f64 * 1.16 / 1000.0,
            hits as f64 * 3.75 / 1000.0,
        );

        check(&phases);
    }
}

/// The phase table, asserted rather than eyeballed. Repetition across runs
/// proves determinism; only this proves the counts mean what the names say.
fn check(phases: &[Phase]) {
    let find = |name: &str| {
        phases
            .iter()
            .find(|phase| phase.name == name)
            .unwrap_or_else(|| panic!("phase {name}"))
    };

    let down = find("scroll down");
    assert_eq!(down.hits, 0, "a first pass can hit nothing");
    assert!(down.misses > 0, "and it must have looked");

    let coloured = find("semantic tokens");
    assert_eq!(
        (coloured.hits, coloured.misses),
        (u64::from(SCREENS * VIEWPORT), 0),
        "the scroll cached exactly these lines; colouring re-reads them and \
         reaches no further — the end of a range is exclusive"
    );

    let hover = find("hover one line");
    assert_eq!(
        (hover.hits, hover.misses),
        (HOVERS as u64, 0),
        "the line was cached by the scroll that just passed it"
    );

    let resolve = find("resolve tooltips");
    assert_eq!(resolve.misses, 0, "same line, same reason");
    assert!(resolve.hits > 0);

    let back = find("edit + scroll back");
    assert_eq!(
        back.misses, 1,
        "exactly the edited line: an edit rebuilds that line and no other"
    );

    let lens = find("code lens");
    assert_eq!(
        (lens.hits, lens.misses),
        (0, 0),
        "neither lens path may populate — the contract, stated over a whole \
         document rather than one line at a time"
    );
}

fn run(resolve_everything: bool) -> Vec<Phase> {
    let text = String::from_utf8(generate_lines(LINES as usize)).expect("the corpus is ASCII");
    let (server_conn, client) = Connection::memory();
    let server = std::thread::spawn(move || venue_reader::server::main_loop(server_conn));
    let mut id = 0;
    let mut next = || {
        id += 1;
        id
    };

    request(
        &client,
        next(),
        "initialize",
        json!({
            "capabilities": {
                "general": { "positionEncodings": ["utf-8"] },
                // Tooltips are resolved rather than filled eagerly, which is
                // what makes the resolve phase a separate cache reader at all.
                "textDocument": {
                    "inlayHint": { "resolveSupport": {
                        "properties": ["tooltip"],
                    }},
                    // Declared, or the server offers the whole-document shape
                    // instead — and that one may not populate a cache at all.
                    "semanticTokens": {
                        "requests": { "range": true },
                        "tokenTypes": [], "tokenModifiers": [], "formats": ["relative"],
                    },
                },
            },
            "initializationOptions": { "fix": {
                "inlayHints": {
                    "tagNames": { "enabled": true },
                    "enumValues": { "enabled": true },
                },
                "messageSummary": { "channels": { "inlayHints": true } },
                // A deliberate departure from the defaults: the lens phase
                // exists to prove the population contract over a whole
                // document, not to represent what a default client sees.
                "limits": { "codeLensLines": LINES + 1 },
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

    let mut phases = Vec::new();
    let screens: Vec<u32> = (0..SCREENS).map(|nth| nth * VIEWPORT).collect();

    config_ready::wait(&client, URI, |_| {});

    // Everything the document did to the cache while opening is not this
    // sequence's doing.
    let _ = stats::take();

    let mut last = Vec::new();
    for &from in &screens {
        last = hints(&client, next(), from);
        assert!(!last.is_empty(), "a viewport drew nothing at line {from}");
    }
    phases.push(phase("scroll down"));

    // The second viewport reader over the lines the first just cached: two
    // requests, one cache. What this shows is that colouring a screen adds
    // nothing to what is retained, because the hints already paid for it.
    for &from in &screens {
        let data = tokens(&client, next(), from);
        assert!(
            !data.is_empty(),
            "a viewport coloured nothing at line {from}"
        );
    }
    phases.push(phase("semantic tokens"));

    // One line of the last viewport, read the way a person reads one: several
    // fields of it, one after another. The positions come from that line's own
    // hints rather than from arithmetic — a guessed column lands on a separator
    // as often as on a field, and `utf-8` makes a byte offset a column.
    let line = screens.last().copied().expect("a screen");
    let fields: Vec<u64> = last
        .iter()
        .filter(|hint| hint["position"]["line"] == line && hint["data"]["kind"] == "tag")
        .filter_map(|hint| hint["data"]["offset"].as_u64())
        .take(HOVERS)
        .collect();
    assert_eq!(fields.len(), HOVERS, "not enough fields on line {line}");
    for offset in fields {
        let answer = request(
            &client,
            next(),
            "textDocument/hover",
            json!({
                "textDocument": { "uri": URI },
                "position": { "line": line, "character": offset },
            }),
        );
        assert!(
            !answer.is_null(),
            "hover at {offset} on {line} said nothing"
        );
    }
    phases.push(phase("hover one line"));

    let wanted: Vec<Value> = if resolve_everything {
        last.clone()
    } else {
        last.iter().take(INSPECTED).cloned().collect()
    };
    let mut resolved = 0usize;
    for hint in wanted {
        let back = request(&client, next(), "inlayHint/resolve", hint);
        resolved += usize::from(back.get("tooltip").is_some());
    }
    assert!(resolved > 0, "no tooltip came back");
    phases.push(phase("resolve tooltips"));

    // One line inside a viewport already visited, so the scroll back below
    // meets exactly one line that has to be analysed again.
    let edited = screens[SCREENS as usize / 2];
    notify(
        &client,
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": URI, "version": 2 },
            "contentChanges": [{
                "range": {
                    "start": { "line": edited, "character": 0 },
                    "end": { "line": edited, "character": 1 },
                },
                "text": "#",
            }],
        }),
    );
    for &from in screens.iter().rev() {
        hints(&client, next(), from);
    }
    phases.push(phase("edit + scroll back"));

    let lenses = request(
        &client,
        next(),
        "textDocument/codeLens",
        json!({ "textDocument": { "uri": URI } }),
    );
    let lenses = lenses.as_array().expect("an array").clone();
    assert!(!lenses.is_empty(), "the lens list was empty");
    for lens in lenses.iter().take(20) {
        request(&client, next(), "codeLens/resolve", lens.clone());
    }
    phases.push(phase("code lens"));

    notify(&client, "exit", Value::Null);
    drop(client);
    let _ = server.join();
    phases
}

fn phase(name: &'static str) -> Phase {
    let (hits, misses) = stats::take();
    Phase { name, hits, misses }
}

fn tokens(client: &Connection, id: i32, from: u32) -> Vec<Value> {
    request(
        client,
        id,
        "textDocument/semanticTokens/range",
        json!({
            "textDocument": { "uri": URI },
            "range": {
                "start": { "line": from, "character": 0 },
                "end": { "line": from + VIEWPORT, "character": 0 },
            },
        }),
    )["data"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn hints(client: &Connection, id: i32, from: u32) -> Vec<Value> {
    request(
        client,
        id,
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": URI },
            "range": {
                "start": { "line": from, "character": 0 },
                "end": { "line": from + VIEWPORT, "character": 0 },
            },
        }),
    )
    .as_array()
    .cloned()
    .unwrap_or_default()
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
