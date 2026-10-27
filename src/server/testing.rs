//! Fixtures the in-crate handler tests share.
//!
//! `cached` above all: the cache-population contract is a rule about who calls
//! what, so every permitted caller asserts it the same way — and four modules
//! writing that assertion out separately is four chances to write it weaker.

use lsp_types::{MessageType, Uri};
use serde_json::Value;

use super::dispatch::{Dispatched, Notices, Snapshot};
use super::ServerState;
use crate::ctx::Ctx;
use crate::document::Document;

pub(crate) const HEARTBEAT: &str =
    "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";

pub(crate) fn file_uri(name: &str) -> Uri {
    let path = std::env::temp_dir()
        .join(format!("venue-reader-server-tests-{}", std::process::id()))
        .join(name);
    Uri::from_file_path(path).unwrap()
}

pub(crate) fn heartbeats(lines: usize) -> String {
    vec![HEARTBEAT; lines].join("\n")
}

/// A server holding one open document, and the URI it answers about.
pub(crate) fn state_with(text: &str) -> (ServerState, Uri) {
    let (sender, _receiver) = crossbeam_channel::unbounded::<lsp_server::Message>();
    let mut state = ServerState::new(sender);
    let uri = file_uri("a.fix");
    state.docs.insert(
        uri.clone(),
        Document::open(text.as_bytes(), 1, Ctx::for_test("FIX.4.4")),
    );
    state.configure_open_document(&uri);
    settle(&mut state);
    (state, uri)
}

pub(crate) fn settle(state: &mut ServerState) {
    while state.config.pending.is_some() {
        let report = state
            .loader
            .reports
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("configuration loader completes");
        state.on_configuration_report(report);
        assert!(state.fatal.is_none(), "{:?}", state.fatal);
    }
}

pub(crate) fn reconfigure(state: &mut ServerState) {
    let value = crate::overrides::public_config(&state.config.base);
    state.apply_base_value(&value);
    settle(state);
}

/// Which lines hold an analysis — looked at rather than asked for, because
/// `cached_analysis` would populate the slot and make every caller compliant.
pub(crate) fn cached_lines(snapshot: &Snapshot) -> Vec<bool> {
    snapshot
        .lines
        .iter()
        .map(|line| line.analysis_if_cached().is_some())
        .collect()
}

pub(crate) fn cached(state: &mut ServerState, uri: &Uri) -> Vec<bool> {
    cached_lines(&state.snapshot_for(uri).expect("open"))
}

/// Run whatever a handler dispatched, and collect what it said while running.
///
/// The sink is a collecting one, so a notice is asserted as data. What it
/// cannot show is *ordering* against the response — that is transport, and
/// needs a test through the protocol.
pub(crate) fn answered_with(dispatched: Dispatched) -> (Value, Vec<(MessageType, String)>) {
    let mut notices = Notices::collecting();
    let answer = match dispatched {
        Dispatched::Task(snapshot, task) => task(&snapshot, &mut notices).expect("answered"),
        Dispatched::Now(result) => result.expect("answered"),
        // Serialised and parsed back so a lazy answer is asserted like any
        // other; the wire path is what `tests/stdio_e2e.rs` covers.
        Dispatched::Lazy(result) => {
            let mut out = Vec::new();
            result
                .expect("answered")
                .write_json(&mut out)
                .expect("serialised");
            serde_json::from_slice(&out).expect("valid JSON")
        }
    };
    (answer, notices.said().to_vec())
}

pub(crate) fn answered(dispatched: Dispatched) -> Value {
    answered_with(dispatched).0
}
