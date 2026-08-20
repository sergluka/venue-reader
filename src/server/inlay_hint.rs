//! `textDocument/inlayHint`: the dictionary's answer drawn beside the bytes.
//!
//! The request carries a `range`, so unlike code lens this channel is bounded by
//! what the client asked for. That is what makes it the second caller the
//! cache-population contract permits — a user reading a viewport reads it more
//! than once.

use lsp_types::{InlayHintParams, MarkupKind};
use serde_json::{json, Value};

use super::dispatch::{Dispatched, HandlerResult, Notices, RequestTask, Snapshot};
use super::ServerState;
use crate::codec::ByteOffset;
use crate::config::InlayHints;
use crate::document::{Column, Columns, LineNo, LineRange};
use crate::render::hint;

/// What a hint needs from configuration, gathered where the config lives so the
/// task carries a value rather than a borrow.
#[derive(Clone)]
pub(super) struct Wanted {
    hints: InlayHints,
    summary: bool,
    cap: usize,
    /// Whether the client resolves tooltips. When it does not, they are filled
    /// in the first response instead — the adaptation §4 prescribes.
    defer_tooltips: bool,
    format: MarkupKind,
}

impl Wanted {
    /// `group_paths` is absent on purpose: it is a modifier on a tag label, not
    /// a hint of its own, so with `tag_names` off it draws nothing.
    fn draws_anything(&self) -> bool {
        self.hints.tag_names() || self.hints.enum_values() || self.summary
    }
}

impl ServerState {
    pub(super) fn inlay_hint_task(&mut self, params: &Value) -> Dispatched {
        let params: InlayHintParams = match super::params(super::method::INLAY_HINT, params) {
            Ok(params) => params,
            Err(e) => return Dispatched::Now(Err(e)),
        };
        let uri = params.text_document.uri.clone();
        if let Some(answer) = super::dispatch::configuration_not_ready(self, &uri) {
            return answer;
        }
        let base = self.effective_config(&uri).unwrap_or(&self.config.base);
        let wanted = Wanted {
            hints: base.inlay_hints.clone(),
            // The master switch and the channel, not the channel alone.
            summary: base.message_summary.enabled && base.message_summary.channels.inlay_hints,
            cap: base.limits.range_lines,
            defer_tooltips: self.client.defers_tooltips,
            format: self.client.hover_format.clone(),
        };
        // Before the snapshot, because the walk below calls `cached_analysis`
        // per line: with every hint off it would populate the whole viewport to
        // produce `[]`.
        if !wanted.draws_anything() {
            return Dispatched::Now(Ok(json!([])));
        }
        let Some(snapshot) = self.snapshot_for(&uri) else {
            return Dispatched::Now(Ok(json!([])));
        };
        let lines = requested_lines(&params, &snapshot);
        if lines.len() > wanted.cap {
            // `[]` and a reason. A client that asked for the whole buffer would
            // otherwise get nothing back and nothing to act on — the lesson the
            // diagnostics cap taught.
            self.log_message(
                lsp_types::MessageType::Info,
                &format!(
                    "fix-lsp: {} — inlay hints over {} lines are not answered; \
                     ask for a smaller range",
                    uri.as_str(),
                    wanted.cap
                ),
            );
            return Dispatched::Now(Ok(json!([])));
        }
        Dispatched::Task(snapshot, prepare(params, wanted))
    }
}

fn prepare(params: InlayHintParams, wanted: Wanted) -> RequestTask {
    Box::new(move |snapshot: &Snapshot, _: &mut Notices| answer(&params, &wanted, snapshot))
}

/// The lines the range covers, clamped to the document.
fn requested_lines(params: &InlayHintParams, snapshot: &Snapshot) -> LineRange {
    let (start, end) = (params.range.start, params.range.end);
    // The end is *exclusive*, so a range ending at column zero does not reach
    // that line at all — including when it is also the start, which is where
    // this differs from `code_action.rs`: a hint is a point, so a cursor at
    // `(n,0)..(n,0)` covers nothing, while an action there is about line `n`.
    // The clipping below is per character for the same reason.
    let last = if end.character == 0 {
        end.line
    } else {
        end.line.saturating_add(1)
    };
    snapshot.line_span(start.line, last)
}

fn answer(params: &InlayHintParams, wanted: &Wanted, snapshot: &Snapshot) -> HandlerResult {
    // Typed once, above the loop: `character` is a number on the wire and stays
    // one, so this conversion is the boundary rather than scaffolding — and
    // repeating it per hint would put four constructors in the inner loop of a
    // response that can carry forty thousand of them.
    let from = (
        LineNo::new(params.range.start.line),
        Column::new(params.range.start.character),
    );
    let to = (
        LineNo::new(params.range.end.line),
        Column::new(params.range.end.character),
    );
    let uri = params.text_document.uri.as_str();
    let mut out = Vec::new();

    for number in requested_lines(params, snapshot).lines() {
        let Some(line) = snapshot.line(number) else {
            continue;
        };
        // The contract's second permitted caller: range-bounded, and a user
        // reading a viewport reads it repeatedly.
        let analysis = snapshot.analysis(line);
        let Some(message) = analysis.message() else {
            continue;
        };

        // Every offset this line needs, converted in one walk. They arrive
        // interleaved — a tag end precedes its own value end but follows the
        // previous field's — so `Columns` sorts rather than trusting the order.
        //
        // **A field label is rendered here and carried**, not recomputed
        // below: whether a hint exists and what it says are one question once
        // the shape is a user's to write, and asking it twice would render
        // every custom label twice. The summary's parts are still rendered
        // twice, which is worse and older — see `What::Summary`.
        let mut wants: Vec<Want> = Vec::new();
        if wanted.summary && hint::summary_parts(&line.bytes, message, &snapshot.render).is_some() {
            wants.push(Want {
                byte: ByteOffset::new(0),
                what: What::Summary,
            });
        }
        for (at, field) in message.fields.iter().enumerate() {
            let render = &snapshot.render;
            if let Some(label) = hint::tag(message, at, &wanted.hints, &line.bytes, render) {
                wants.push(Want {
                    byte: field.tag_end(),
                    what: What::Field {
                        kind: Kind::Tag,
                        at,
                        label,
                    },
                });
            }
            if let Some(label) = hint::value(message, at, &wanted.hints, render) {
                wants.push(Want {
                    byte: field.value_end(),
                    what: What::Field {
                        kind: Kind::Value,
                        at,
                        label,
                    },
                });
            }
        }
        if wants.is_empty() {
            continue;
        }
        let columns = Columns::of(line, wants.iter().map(|want| want.byte), snapshot.encoding);

        for Want { byte, what } in wants {
            let column = columns.at(byte);
            // Character-level clipping, because a hint is a point and the
            // range's end is exclusive. A line-level reading would emit hints
            // the client did not ask about on the first and last lines.
            if number == from.0 && column < from.1 {
                continue;
            }
            if number == to.0 && column >= to.1 {
                continue;
            }
            out.push(match what {
                What::Summary => summary_hint(line, message, snapshot, number, uri),
                What::Field { kind, at, label } => field_hint(
                    line, message, at, kind, label, wanted, snapshot, number, column, uri,
                ),
            });
        }
    }
    Ok(Value::Array(out))
}

/// One hint before its offset is a column.
struct Want {
    byte: ByteOffset,
    what: What,
}

/// What the hint is about. An enum rather than a `Kind` beside an index and a
/// label, because a summary has neither: carried flat, it needed a
/// `usize::MAX` and an empty string that were safe only as long as the
/// dispatch never handed a summary to the field builder.
enum What {
    /// Rendered twice — once to know it exists, once in `summary_hint`. Older
    /// than the field path and the more expensive of the two, since it walks
    /// the template and the details; carrying the parts here would fix it.
    Summary,
    Field {
        kind: Kind,
        at: usize,
        label: String,
    },
}

/// Which half of a field a hint is about. A summary is not a `Kind`: it is
/// about the line, and `What` is where the two part company.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Tag,
    Value,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Tag => "tag",
            Kind::Value => "value",
        }
    }

    /// `Parameter` for a name, `Type` for what a value means — the two the spec
    /// names, and what a client colours by.
    fn lsp(self) -> u8 {
        match self {
            Kind::Tag => 1,
            Kind::Value => 2,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn field_hint(
    line_text: &crate::document::Line,
    message: &crate::codec::MessageAnalysis,
    at: usize,
    kind: Kind,
    label: String,
    wanted: &Wanted,
    _snapshot: &Snapshot,
    line: LineNo,
    column: Column,
    uri: &str,
) -> Value {
    let field = &message.fields[at];
    let mut data = json!({
        "uri": uri,
        "version": _snapshot.version,
        // The line as well as the offset: `tag_start` is unique only *within*
        // a line, and a resolve that trusted the hint's position would take the
        // client's word for where its own hint was.
        "line": line,
        "offset": field.tag_start,
        "kind": kind.as_str(),
        "tag": field.tag,
    });
    if let Some(instance) = field.group_idx() {
        data["group"] = json!({ "index": instance });
    }
    let mut out = json!({
        "position": { "line": line, "character": column },
        "label": label,
        "kind": kind.lsp(),
        "data": data,
    });
    // Eager when the client does not resolve tooltips: sending it a hint whose
    // tooltip will never be filled is an empty popup, not a lazy one.
    if !wanted.defer_tooltips {
        if let Some(tip) = tooltip(line_text, message, at, _snapshot, &wanted.format) {
            out["tooltip"] = tip;
        }
    }
    out
}

fn summary_hint(
    line_text: &crate::document::Line,
    message: &crate::codec::MessageAnalysis,
    snapshot: &Snapshot,
    line: LineNo,
    uri: &str,
) -> Value {
    let parts = hint::summary_parts(&line_text.bytes, message, &snapshot.render)
        .expect("only collected when there is one");
    json!({
        "position": { "line": line, "character": 0 },
        // Parts rather than one string: this is the channel that can keep the
        // summary's halves apart, and `codeLens` is the one that cannot.
        "label": parts
            .into_iter()
            .map(|value| json!({ "value": value }))
            .collect::<Vec<_>>(),
        "paddingRight": true,
        "data": {
            "uri": uri,
            "version": snapshot.version,
            "line": line,
            "kind": "summary",
        },
    })
}

/// Whether the client wants tooltips filled in the first response.
///
/// `resolveSupport` is a *property list*, not a flag: a client that resolves
/// hints may still not resolve their tooltips, and sending it a hint without one
/// would show an empty popup rather than the dictionary's answer.
pub(super) fn defers_tooltips(caps: &lsp_types::ClientCapabilities) -> bool {
    caps.text_document
        .as_ref()
        .and_then(|doc| doc.inlay_hint.as_ref())
        .and_then(|hint| hint.resolve_support.as_ref())
        .is_some_and(|resolve| resolve.properties.iter().any(|name| name == "tooltip"))
}

/// The hover markdown a hint's tooltip carries, under the negotiated format.
pub(super) fn tooltip(
    line: &crate::document::Line,
    message: &crate::codec::MessageAnalysis,
    at: usize,
    snapshot: &Snapshot,
    format: &MarkupKind,
) -> Option<Value> {
    let markdown = crate::render::hover(&line.bytes, message, at, &snapshot.render)?;
    Some(json!({ "kind": format, "value": markdown }))
}

impl ServerState {
    /// `inlayHint/resolve`: the hint comes back and leaves with its tooltip.
    ///
    /// The hint is found by what `data` says, not by where the client says it
    /// is: `offset` is unique only within a line, so `line` travels with it and
    /// both are checked against the document rather than trusted.
    pub(super) fn inlay_hint_resolve_task(&mut self, params: &Value) -> Dispatched {
        let hint = params.clone();
        let data = &params["data"];
        let Some(uri) = data["uri"]
            .as_str()
            .and_then(|raw| super::parse_uri("inlayHint/resolve", raw))
        else {
            return Dispatched::Now(Ok(hint));
        };
        let format = self.client.hover_format.clone();
        if let Some(answer) = super::dispatch::configuration_not_ready(self, &uri) {
            return answer;
        }
        let Some(snapshot) = self.snapshot_for(&uri) else {
            // The document closed; the hint is still a hint, just an unadorned
            // one. An error would make a client discard what it is showing.
            return Dispatched::Now(Ok(hint));
        };
        Dispatched::Task(
            snapshot,
            Box::new(move |snapshot, _: &mut Notices| resolve(hint, &format, snapshot)),
        )
    }
}

fn resolve(mut hint: Value, format: &MarkupKind, snapshot: &Snapshot) -> HandlerResult {
    let data = hint["data"].clone();
    // A hint held across an edit is about text that has moved, and every edit
    // advances the version — so this is what makes an offset safe to trust.
    if data["version"].as_i64() != Some(snapshot.version as i64) {
        return Ok(hint);
    }
    let (Some(number), Some(offset)) = (data["line"].as_u64(), data["offset"].as_u64()) else {
        // A summary hint carries no offset and has no tooltip.
        return Ok(hint);
    };
    let Some(line) = LineNo::try_from(number)
        .ok()
        .and_then(|no| snapshot.line(no))
    else {
        return Ok(hint);
    };
    // One line, and one the user is looking at: the cache may take it.
    let analysis = snapshot.analysis(line);
    let Some(message) = analysis.message() else {
        return Ok(hint);
    };
    let Some(at) = message
        .fields
        .iter()
        .position(|field| u64::from(field.tag_start) == offset)
    else {
        return Ok(hint);
    };
    // The tag as well as the offset: only an *edit* must advance the version,
    // so a document reopened at the same version can put a different field at
    // the same byte — and the tooltip would then describe it under the old
    // label. Code lens guards the same lifecycle with its line hash.
    if data["tag"].as_u64() != Some(u64::from(message.fields[at].tag)) {
        return Ok(hint);
    }
    if let Some(tip) = tooltip(line, message, at, snapshot, format) {
        hint["tooltip"] = tip;
    }
    Ok(hint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::testing::{answered, cached, heartbeats, state_with};
    use lsp_types::Uri;

    fn viewport(uri: &Uri, from: u32, to: u32) -> Value {
        json!({
            "textDocument": { "uri": uri.as_str() },
            "range": {
                "start": { "line": from, "character": 0 },
                "end": { "line": to, "character": 0 },
            },
        })
    }

    fn run(state: &mut ServerState, params: Value) -> Value {
        answered(state.inlay_hint_task(&params))
    }

    // Only the cache-asserting tests use it.
    #[cfg(not(feature = "no-line-cache"))]
    fn all_on() -> Value {
        json!({
            "inlayHints": {
                "tagNames": { "enabled": true },
                "enumValues": { "enabled": true },
            }
        })
    }

    /// The contract's permitted caller, held to what it was permitted for: the
    /// lines the client asked about, and not one more.
    #[test]
    #[cfg(not(feature = "no-line-cache"))] // asserts a cache exists
    fn a_viewport_populates_the_lines_it_covers_and_no_others() {
        let (mut state, uri) = state_with(&heartbeats(6));
        state.config.base = serde_json::from_value(all_on()).expect("config");
        super::super::testing::reconfigure(&mut state);

        let hints = run(&mut state, viewport(&uri, 2, 4));
        assert!(!hints.as_array().expect("an array").is_empty());
        assert_eq!(
            cached(&mut state, &uri),
            vec![false, false, true, true, false, false],
            "the range's end is exclusive, so line 4 is outside it"
        );
    }

    /// With nothing to draw there is nothing to analyse. Without the early
    /// return this walk would cache the whole viewport to produce `[]` — the
    /// one way this channel could break the memory budget while answering
    /// nothing at all.
    #[test]
    fn a_channel_that_draws_nothing_populates_nothing() {
        let (mut state, uri) = state_with(&heartbeats(6));
        state.config.base = serde_json::from_value(json!({
            "messageSummary": { "channels": { "inlayHints": false } }
        }))
        .expect("config");
        super::super::testing::reconfigure(&mut state);
        assert!(
            !state.config.base.inlay_hints.tag_names()
                && !state.config.base.inlay_hints.enum_values(),
            "and the two field hints are off by schema default"
        );

        assert_eq!(run(&mut state, viewport(&uri, 0, 6)), json!([]));
        assert!(
            cached(&mut state, &uri).iter().all(|seen| !seen),
            "nothing was analysed"
        );
    }

    /// A resolve is one line the user is looking at, so it may populate — but
    /// only its own.
    #[test]
    #[cfg(not(feature = "no-line-cache"))] // asserts a cache exists
    fn a_resolve_populates_only_its_own_line() {
        let (mut state, uri) = state_with(&heartbeats(6));
        state.config.base = serde_json::from_value(all_on()).expect("config");
        super::super::testing::reconfigure(&mut state);
        let hints = run(&mut state, viewport(&uri, 3, 4));
        let hint = hints.as_array().expect("an array")[0].clone();
        assert_eq!(hint["data"]["line"], 3);

        // A fresh document, so the viewport above did not do the populating.
        let (mut state, uri) = state_with(&heartbeats(6));
        state.config.base = serde_json::from_value(all_on()).expect("config");
        super::super::testing::reconfigure(&mut state);
        let resolved = answered(state.inlay_hint_resolve_task(&hint));
        assert!(resolved["tooltip"]["value"].is_string(), "{resolved}");
        let mut expected = vec![false; 6];
        expected[3] = true;
        assert_eq!(cached(&mut state, &uri), expected);
    }

    /// Cumulative, and nothing evicts. Recorded rather than fixed: this stage
    /// keeps the permitted caller, and work-order item 16 decides from a number
    /// instead of from a worry.
    #[test]
    #[cfg(not(feature = "no-line-cache"))] // asserts a cache exists
    fn scrolling_a_document_populates_all_of_it() {
        const LINES: u32 = 60;
        const PAGE: u32 = 10;

        let (mut state, uri) = state_with(&heartbeats(LINES as usize));
        state.config.base = serde_json::from_value(all_on()).expect("config");
        super::super::testing::reconfigure(&mut state);
        for page in (0..LINES).step_by(PAGE as usize) {
            run(&mut state, viewport(&uri, page, page + PAGE));
        }
        assert_eq!(
            cached(&mut state, &uri)
                .iter()
                .filter(|seen| **seen)
                .count(),
            LINES as usize,
            "every line a viewport ever covered is still analysed"
        );
    }
}
