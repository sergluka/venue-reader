//! Semantic tokens on the wire: five integers per token, each relative to the
//! one before it.
//!
//! The protocol's encoding is `deltaLine, deltaStart, length, type, modifiers`.
//! A delta is measured from the previous *emitted* token, so a line that emits
//! nothing does not move the cursor — which is why the run is walked line by
//! line rather than token by token, and why every line reports a count even
//! when that count is zero.

use lsp_types::{
    MessageType, SemanticTokensDelta, SemanticTokensDeltaParams, SemanticTokensEdit,
    SemanticTokensParams, SemanticTokensRangeParams, Uri,
};
use serde_json::{json, Value};

use super::dispatch::{Dispatched, Notices, Snapshot};
use super::ServerState;
use crate::ctx::analyze;
use crate::document::{Column, Columns, Edited, LineNo};
use crate::render::tokens::{classify, Token};

/// Integers per token, fixed by the protocol.
const STRIDE: usize = 5;

/// Append one run of lines to `data`, and one count per line to `counts`.
///
/// `origin` is the line the first delta is measured from: line 0 for a whole
/// answer, and for an edit the last line before the run that emitted anything.
/// Every line of the run appends a count, empty ones too, so `counts` indexes
/// by line — a zero is what tells an edit that a line has no tokens, and
/// without it nothing could be spliced by line number.
pub fn encode(
    lines: impl IntoIterator<Item = (LineNo, Vec<Token>)>,
    snapshot: &Snapshot,
    origin: LineNo,
    data: &mut Vec<u32>,
    counts: &mut Vec<u32>,
) {
    let mut previous_line = origin;
    let mut previous_start = Column::new(0);

    for (no, tokens) in lines {
        let Some(line) = snapshot.line(no) else {
            // A zero rather than nothing: `counts` indexes by line, and a count
            // recorded without its five integers would move every later offset.
            counts.push(0);
            continue;
        };
        counts.push(tokens.len() as u32);
        // One walk of the line for every offset on it. Both ends of every
        // token, because a length is a distance in the negotiated encoding and
        // not a count of bytes; duplicates — an overlay starts where its first
        // field starts — are dropped inside.
        let columns = Columns::of(
            line,
            tokens
                .iter()
                .flat_map(|token| [token.range.start, token.range.end]),
            snapshot.encoding,
        );

        for token in &tokens {
            let start = columns.at(token.range.start);
            let delta_line = no.raw().saturating_sub(previous_line.raw());
            // A new line resets the start; the same line measures from the
            // token before it.
            let delta_start = if delta_line == 0 {
                start - previous_start
            } else {
                start - Column::new(0)
            };
            data.extend_from_slice(&[
                delta_line,
                delta_start,
                columns.at(token.range.end) - start,
                token.kind as u32,
                token.mods,
            ]);
            previous_line = no;
            previous_start = start;
        }
    }

    debug_assert_eq!(
        data.len() % STRIDE,
        0,
        "a token is {STRIDE} integers, always"
    );
}

/// What a client was last told, so an edit can be answered as one.
///
/// `counts` is per line rather than per token: a line is the unit the
/// classifier works in, so a splice by line number is the whole of what an
/// edit needs, and a zero says a line has no tokens rather than saying nothing.
pub(crate) struct Digest {
    pub result_id: u64,
    pub counts: Vec<u32>,
    /// What has happened to the document since the answer that carries this id.
    pub edited: Edited,
}

/// The whole-document answer.
///
/// Hand-written because `lsp_types::SemanticTokens` holds a `Vec` of five-field
/// structs, and the wire wants the flat array we already have.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Answer {
    result_id: String,
    /// Never `null`: a client that asked is told "no tokens", not "no answer".
    data: Vec<u32>,
}

impl ServerState {
    /// The viewport shape. Widened to whole lines, because a token is a whole
    /// `tag=value` and half of one would be a token the client cannot place.
    pub(super) fn semantic_tokens_range_task(&mut self, params: &Value) -> Dispatched {
        let params: SemanticTokensRangeParams =
            match super::params(super::method::SEMANTIC_TOKENS_RANGE, params) {
                Ok(params) => params,
                Err(e) => return Dispatched::Now(Err(e)),
            };
        let uri = params.text_document.uri.clone();
        if let Some(answer) = super::dispatch::configuration_not_ready(self, &uri) {
            return answer;
        }
        if !self
            .effective_config(&uri)
            .unwrap_or(&self.config.base)
            .semantic_tokens
            .enabled
        {
            return Dispatched::Now(Ok(Value::Null));
        }
        let cap = self
            .effective_config(&uri)
            .unwrap_or(&self.config.base)
            .limits
            .range_lines;
        let Some(snapshot) = self.snapshot_for(&uri) else {
            return Dispatched::Now(Ok(Value::Null));
        };
        let first = params.range.start.line;
        // The end is *exclusive*, so a range ending at column zero does not
        // reach that line — the same reading `inlay_hint.rs` makes. Colouring
        // it anyway is permitted, but it would read a line outside the viewport
        // and cache it, which is the one thing this path is bounded by.
        let past = if params.range.end.character == 0 {
            params.range.end.line
        } else {
            params.range.end.line.saturating_add(1)
        };
        if past <= first {
            return Dispatched::Now(Ok(json!({ "data": [] })));
        }
        let last = past - 1;
        // `[]` and a reason, never a shorter answer: the protocol lets a server
        // colour a *broader* range than it was asked for and says nothing about
        // a narrower one, so a truncated answer is one a client cannot tell
        // from "nothing to colour there".
        if (last - first) as usize >= cap {
            self.log_message(
                MessageType::Info,
                &format!(
                    "venue-reader: {} — a viewport over {cap} lines is not coloured; \
                     ask for a smaller range, or raise fix.limits.rangeLines",
                    uri.as_str(),
                ),
            );
            return Dispatched::Now(Ok(json!({ "data": [] })));
        }
        let overlays = self.client.overlaps_tokens;
        Dispatched::Task(
            snapshot,
            Box::new(move |snapshot: &Snapshot, _: &mut Notices| {
                // The one shape here that may populate a line's cache: a
                // viewport is bounded by the screen, and the user scrolls back.
                let run: Vec<(LineNo, Vec<Token>)> = (first..=last)
                    .filter_map(|at| {
                        let no = LineNo::new(at);
                        let line = snapshot.line(no)?;
                        let mut out = Vec::new();
                        classify(&line.bytes, &snapshot.analysis(line), overlays, &mut out);
                        Some((no, out))
                    })
                    .collect();
                let mut data = Vec::new();
                let mut counts = Vec::new();
                // Line 0, not the viewport's first line: a range answer is a
                // standalone array whose first token names its absolute line.
                encode(run, snapshot, LineNo::new(0), &mut data, &mut counts);
                Ok(json!({ "data": data }))
            }),
        )
    }

    /// The whole-document shape, capped from the top.
    pub(super) fn semantic_tokens_full(&mut self, params: &Value) -> Dispatched {
        let params: SemanticTokensParams =
            match super::params(super::method::SEMANTIC_TOKENS_FULL, params) {
                Ok(params) => params,
                Err(e) => return Dispatched::Now(Err(e)),
            };
        let uri = params.text_document.uri.clone();
        if let Some(answer) = super::dispatch::configuration_not_ready(self, &uri) {
            return answer;
        }
        let Some(snapshot) = self.coloured(&uri) else {
            return Dispatched::Now(Ok(Value::Null));
        };
        Dispatched::Lazy(Ok(Box::new(self.colour_whole(uri, &snapshot, true))))
    }

    /// An edit to the last answer, or a whole one when nothing can be spliced.
    pub(super) fn semantic_tokens_delta(&mut self, params: &Value) -> Dispatched {
        let params: SemanticTokensDeltaParams =
            match super::params(super::method::SEMANTIC_TOKENS_DELTA, params) {
                Ok(params) => params,
                Err(e) => return Dispatched::Now(Err(e)),
            };
        let uri = params.text_document.uri.clone();
        if let Some(answer) = super::dispatch::configuration_not_ready(self, &uri) {
            return answer;
        }
        let Some(snapshot) = self.coloured(&uri) else {
            return Dispatched::Now(Ok(Value::Null));
        };
        let cap = self
            .effective_config(&uri)
            .unwrap_or(&self.config.base)
            .limits
            .semantic_tokens_full_lines;
        let overlays = self.client.overlaps_tokens;

        // Taken out before the answer is built, not after: if the work below
        // fails, the client's id must match nothing, or it would retry the same
        // failing edit for as long as the document stays open.
        //
        // A stale or unknown id is the routine path after a refresh — a client
        // may keep the old one on purpose — so it is answered whole, not
        // refused.
        let spliced = self
            .tokens
            .remove(&uri)
            .filter(|digest| digest.result_id.to_string() == params.previous_result_id)
            .and_then(|digest| splice(&digest, &snapshot, cap, overlays));

        let Some(Spliced { edits, counts }) = spliced else {
            // No notice here: after the first whole answer a client sends
            // edits, and saying it again would say it on every keystroke.
            return Dispatched::Lazy(Ok(Box::new(self.colour_whole(uri, &snapshot, false))));
        };
        let result_id = self.publish(uri, counts);
        Dispatched::Lazy(Ok(Box::new(SemanticTokensDelta {
            result_id: Some(result_id.to_string()),
            edits,
        })))
    }

    /// The snapshot to colour, or nothing — a closed document, or the switch
    /// off, which also drops whatever the client was last told.
    fn coloured(&mut self, uri: &Uri) -> Option<Snapshot> {
        if !self
            .effective_config(uri)
            .unwrap_or(&self.config.base)
            .semantic_tokens
            .enabled
        {
            self.tokens.remove(uri);
            return None;
        }
        self.snapshot_for(uri)
    }

    /// Colour a document from its first line, and publish what was sent.
    fn colour_whole(&mut self, uri: Uri, snapshot: &Snapshot, notify: bool) -> Answer {
        let cap = self
            .effective_config(&uri)
            .unwrap_or(&self.config.base)
            .limits
            .semantic_tokens_full_lines;
        let lines = snapshot.content_lines();
        let last = lines.min(cap);
        let mut data = Vec::new();
        let mut counts = Vec::new();
        // `analyze` and drop: a whole-document walk may not populate a cache,
        // which is what `dirty_run` does with no line to stop at.
        let (run, _) = dirty_run(snapshot, 0, usize::MAX, last, self.client.overlaps_tokens);
        encode(run, snapshot, LineNo::new(0), &mut data, &mut counts);

        if notify && lines > cap {
            self.log_message(
                MessageType::Info,
                &format!(
                    "venue-reader: {} — colouring the first {cap} of {lines} lines; \
                     raise fix.limits.semanticTokensFullLines to colour more",
                    uri.as_str(),
                ),
            );
        }
        // Published last: the answer is built before the digest is replaced, so
        // a failure above leaves the client's id matching what it still holds.
        let result_id = self.publish(uri, counts);
        Answer {
            result_id: result_id.to_string(),
            data,
        }
    }

    fn publish(&mut self, uri: Uri, counts: Vec<u32>) -> u64 {
        self.token_results += 1;
        let result_id = self.token_results;
        self.tokens.insert(
            uri,
            Digest {
                result_id,
                counts,
                edited: Edited::Nothing,
            },
        );
        result_id
    }
}

/// What an edit turned the previous answer into.
struct Spliced {
    edits: Vec<SemanticTokensEdit>,
    counts: Vec<u32>,
}

/// Where a token run starts in the flat array: five integers per token, so an
/// edit's offsets count *integers* and not tokens.
fn offset_of(counts: &[u32], line: usize) -> Option<u32> {
    // An edit's offsets are `u32` by protocol, so an answer too large to
    // address is one to refuse rather than to wrap: a wrapped `start` points
    // into the middle of a token and silently recolours the rest of the file.
    let tokens: u32 = counts[..line.min(counts.len())].iter().sum();
    tokens.checked_mul(STRIDE as u32)
}

/// The last line before `line` that emitted anything — the token an edit's
/// first delta is measured from.
fn predecessor(counts: &[u32], line: usize) -> LineNo {
    LineNo::new(
        counts[..line.min(counts.len())]
            .iter()
            .rposition(|count| *count > 0)
            .unwrap_or(0) as u32,
    )
}

/// The edits that turn the last answer into the current one, or `None` when no
/// line of it can be identified in the document as it is now — for which the
/// answer is a whole one, which a delta request permits.
///
/// Two edits at most, ascending and disjoint **in the old array's offsets**,
/// because that is what a client applying them against the original requires.
/// The first re-encodes the changed lines; the second exists whenever the
/// change moved the cap boundary, which an insertion near the top does even
/// though it is nowhere near the cap.
fn splice(digest: &Digest, snapshot: &Snapshot, cap: usize, overlays: bool) -> Option<Spliced> {
    let (first, last, delta) = match digest.edited {
        Edited::Nothing => {
            return Some(Spliced {
                edits: Vec::new(),
                counts: digest.counts.clone(),
            })
        }
        Edited::Everything => return None,
        Edited::Lines {
            first,
            last,
            line_delta,
        } => (first as usize, last as usize, line_delta),
    };
    let old = &digest.counts;
    let k_old = old.len();
    let k_new = snapshot.content_lines().min(cap);

    let mut edits = Vec::new();
    let mut counts = old.clone();
    // Where the two answers stop agreeing: an index into the old array, and the
    // new line that takes its place.
    let (join_old, join_new) = if first >= k_new {
        // The change is past the cap, so every line the answer covers is still
        // where it was and only the boundary itself can have moved.
        (k_new.min(k_old), k_new.min(k_old))
    } else {
        let (run, end) = dirty_run(snapshot, first, last, k_new, overlays);
        let mut data = Vec::new();
        let mut run_counts = Vec::new();
        encode(
            run,
            snapshot,
            predecessor(old, first),
            &mut data,
            &mut run_counts,
        );

        // Only a line past the changed span has an old identity at all. Without
        // one the run already reaches the cap and replaces everything after it.
        let successor = (end > last).then(|| end as i64 - delta);
        let old_end = match successor {
            Some(was) if was >= first as i64 && was < k_old as i64 => was as usize + 1,
            Some(_) => return None,
            None => k_old,
        };
        let from = offset_of(old, first)?;
        let deleted = offset_of(old, old_end)?.checked_sub(from)?;
        // A run that yields no tokens and replaced none is not an edit. Sending
        // it would have the client copy an empty stretch of its own array, and
        // `merged` cannot take it back out afterwards.
        if deleted > 0 || !data.is_empty() {
            edits.push(SemanticTokensEdit {
                start: from,
                delete_count: deleted,
                data: Some(data),
            });
        }
        counts.splice(first..old_end, run_counts);

        match successor {
            // `k_new - delta` is the old index of the new cap boundary. Below
            // the run there is nothing left to identify, above `k_old` nothing
            // left to delete.
            Some(_) => {
                let join = (k_new as i64 - delta).clamp(old_end as i64, k_old as i64);
                (join as usize, (join + delta).max(0) as usize)
            }
            None => (k_old, counts.len()),
        }
    };

    let join_new = join_new.min(k_new).min(counts.len());
    let (run, _) = dirty_run(snapshot, join_new, usize::MAX, k_new, overlays);
    counts.truncate(join_new);
    let mut tail = Vec::new();
    let mut tail_counts = Vec::new();
    encode(
        run,
        snapshot,
        predecessor(&counts, join_new),
        &mut tail,
        &mut tail_counts,
    );
    counts.extend(tail_counts);

    let from = offset_of(old, join_old)?;
    let deleted = offset_of(old, k_old)?.checked_sub(from)?;
    // A line that yields no tokens still reports a count, so an edit that
    // deletes nothing and inserts nothing is a real shape here, and one a
    // client would copy an empty stretch of its array for.
    if deleted > 0 || !tail.is_empty() {
        edits.push(SemanticTokensEdit {
            start: from,
            delete_count: deleted,
            data: Some(tail),
        });
    }
    Some(Spliced {
        edits: merged(edits),
        counts,
    })
}

/// Lines `first..stop` with their tokens, stopping after the first tokenised
/// line past `last`.
///
/// That line is the only one an earlier change can invalidate: re-encoding
/// whole lines fixes every `deltaStart`, and only the first following token's
/// `deltaLine` is measured across the span. Widening to its whole line rather
/// than repairing one integer is the simplification — a line is the unit the
/// digest counts in.
fn dirty_run(
    snapshot: &Snapshot,
    first: usize,
    last: usize,
    stop: usize,
    overlays: bool,
) -> (Vec<(LineNo, Vec<Token>)>, usize) {
    let mut run = Vec::new();
    let mut end = first;
    for at in first..stop {
        let no = LineNo::new(at as u32);
        let Some(line) = snapshot.line(no) else {
            break;
        };
        let mut out = Vec::new();
        // An edit is not viewport-bounded, so it may not populate a cache.
        let analysis = analyze(&line.bytes, &snapshot.ctx);
        classify(&line.bytes, &analysis, overlays, &mut out);
        let tokenised = !out.is_empty();
        run.push((no, out));
        end = at;
        if at > last && tokenised {
            break;
        }
    }
    (run, end)
}

/// Two edits that touch are one edit; a client sorting them would otherwise
/// copy a zero-length stretch of the original between them.
fn merged(mut edits: Vec<SemanticTokensEdit>) -> Vec<SemanticTokensEdit> {
    let [one, two] = edits.as_slice() else {
        return edits;
    };
    if one.start + one.delete_count != two.start {
        return edits;
    }
    let two = edits.pop().expect("two");
    let one = edits.last_mut().expect("one");
    one.delete_count += two.delete_count;
    match (one.data.as_mut(), two.data) {
        (Some(head), Some(tail)) => head.extend(tail),
        // The protocol permits a pure deletion to carry no data, so the second
        // edit's own data must survive a first that has none.
        (None, tail) => one.data = tail,
        (Some(_), None) => {}
    }
    edits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::ByteOffset;
    use crate::ctx::analyze;
    use crate::document::PositionEncoding;
    use crate::render::tokens::{classify, Kind, Mod};
    use crate::server::ServerState;
    use proptest::prelude::*;

    fn snapshot(text: &str, encoding: PositionEncoding) -> Snapshot {
        let (mut state, uri) = super::super::testing::state_with(text);
        let mut snapshot = state.snapshot_for(&uri).expect("open");
        snapshot.encoding = encoding;
        snapshot
    }

    /// Every line of the document, tokenised — what a whole answer does.
    fn whole(text: &str, encoding: PositionEncoding, overlays: bool) -> (Vec<u32>, Vec<u32>) {
        let snapshot = snapshot(text, encoding);
        let runs: Vec<(LineNo, Vec<Token>)> = (0..snapshot.lines.len())
            .map(|at| {
                let no = LineNo::new(at as u32);
                let line = snapshot.line(no).expect("in range");
                let mut out = Vec::new();
                // `analyze` and not the caching accessor: a whole-document walk
                // may not populate a line's cache.
                let analysis = analyze(&line.bytes, &snapshot.ctx);
                classify(&line.bytes, &analysis, overlays, &mut out);
                (no, out)
            })
            .collect();
        let (mut data, mut counts) = (Vec::new(), Vec::new());
        encode(runs, &snapshot, LineNo::new(0), &mut data, &mut counts);
        (data, counts)
    }

    /// The client's own loop: edits are ascending and disjoint in the original's
    /// offsets, so the untouched stretches are copied from the original.
    fn apply_edits(old: &[u32], edits: &[SemanticTokensEdit]) -> Vec<u32> {
        let mut out = Vec::new();
        let mut at = 0usize;
        for edit in edits {
            let start = edit.start as usize;
            assert!(start >= at, "edits are ascending and disjoint: {edits:?}");
            out.extend_from_slice(&old[at..start]);
            out.extend(edit.data.clone().unwrap_or_default());
            at = start + edit.delete_count as usize;
        }
        out.extend_from_slice(&old[at..]);
        out
    }

    fn coloured(text: &str, cap: usize) -> (ServerState, Uri) {
        let (mut state, uri) = super::super::testing::state_with(text);
        state.config.base.limits.semantic_tokens_full_lines = cap;
        super::super::testing::reconfigure(&mut state);
        state.config.base.limits.range_lines = cap;
        super::super::testing::reconfigure(&mut state);
        (state, uri)
    }

    fn ask_full(state: &mut ServerState, uri: &Uri) -> (String, Vec<u32>) {
        let answer = super::super::testing::answered(
            state.semantic_tokens_full(&json!({ "textDocument": { "uri": uri.as_str() } })),
        );
        (
            answer["resultId"].as_str().expect("an id").to_owned(),
            integers(&answer["data"]),
        )
    }

    fn ask_range(state: &mut ServerState, uri: &Uri, first: u32, last: u32) -> Value {
        super::super::testing::answered(state.semantic_tokens_range_task(&json!({
            "textDocument": { "uri": uri.as_str() },
            "range": {
                "start": { "line": first, "character": 0 },
                "end": { "line": last, "character": 0 },
            },
        })))
    }

    fn ask_delta(state: &mut ServerState, uri: &Uri, previous: &str) -> Value {
        super::super::testing::answered(state.semantic_tokens_delta(&json!({
            "textDocument": { "uri": uri.as_str() },
            "previousResultId": previous,
        })))
    }

    fn integers(value: &Value) -> Vec<u32> {
        value
            .as_array()
            .expect("an array")
            .iter()
            .map(|at| at.as_u64().expect("an integer") as u32)
            .collect()
    }

    fn edits_of(answer: &Value) -> Vec<SemanticTokensEdit> {
        answer["edits"]
            .as_array()
            .expect("edits")
            .iter()
            .map(|edit| SemanticTokensEdit {
                start: edit["start"].as_u64().expect("start") as u32,
                delete_count: edit["deleteCount"].as_u64().expect("deleteCount") as u32,
                data: edit.get("data").map(integers),
            })
            .collect()
    }

    fn edit_at(
        state: &mut ServerState,
        uri: &Uri,
        version: i32,
        at: (u32, u32, u32, u32),
        text: &str,
    ) {
        let (start_line, start_ch, end_line, end_ch) = at;
        state.on_did_change(json!({
            "textDocument": { "uri": uri.as_str(), "version": version },
            "contentChanges": [{
                "range": {
                    "start": { "line": start_line, "character": start_ch },
                    "end": { "line": end_line, "character": end_ch },
                },
                "text": text,
            }],
        }));
    }

    /// What the whole thing is for: the client's array after the edits is the
    /// array it would have got by asking again.
    fn agrees_with_a_fresh_answer(state: &mut ServerState, uri: &Uri, before: &[u32], id: &str) {
        let answer = ask_delta(state, uri, id);
        assert!(answer.get("edits").is_some(), "a delta, not a whole answer");
        let patched = apply_edits(before, &edits_of(&answer));
        let (_, fresh) = ask_full(state, uri);
        assert_eq!(patched, fresh, "the client's array is the fresh one");
    }

    const THREE: &str = "8=FIX.4.4|35=0\n8=FIX.4.4|35=1\n8=FIX.4.4|35=2";

    #[test]
    fn an_edit_inside_one_line_moves_only_that_line() {
        let (mut state, uri) = coloured(THREE, 100);
        let (id, before) = ask_full(&mut state, &uri);
        edit_at(&mut state, &uri, 2, (1, 13, 1, 14), "5");
        agrees_with_a_fresh_answer(&mut state, &uri, &before, &id);
    }

    #[test]
    fn an_inserted_line_shifts_everything_after_it() {
        let (mut state, uri) = coloured(THREE, 100);
        let (id, before) = ask_full(&mut state, &uri);
        edit_at(&mut state, &uri, 2, (0, 14, 0, 14), "\n8=FIX.4.4|35=A");
        agrees_with_a_fresh_answer(&mut state, &uri, &before, &id);
    }

    #[test]
    fn a_deleted_line_admits_a_line_the_cap_had_excluded() {
        let text = format!("{THREE}\n8=FIX.4.4|35=3\n8=FIX.4.4|35=4");
        let (mut state, uri) = coloured(&text, 3);
        let (id, before) = ask_full(&mut state, &uri);
        assert_eq!(before.len(), 3 * 4 * STRIDE, "three lines of four tokens");
        edit_at(&mut state, &uri, 2, (0, 0, 1, 0), "");
        agrees_with_a_fresh_answer(&mut state, &uri, &before, &id);
    }

    /// An insertion near the top pushes the last line out of a capped answer,
    /// which is a second edit even though the change is nowhere near the cap.
    #[test]
    fn an_insertion_at_the_top_trims_the_capped_tail() {
        let text = format!("{THREE}\n8=FIX.4.4|35=3");
        let (mut state, uri) = coloured(&text, 4);
        let (id, before) = ask_full(&mut state, &uri);
        edit_at(&mut state, &uri, 2, (0, 0, 0, 0), "8=FIX.4.4|35=A\n");
        agrees_with_a_fresh_answer(&mut state, &uri, &before, &id);
    }

    #[test]
    fn an_empty_line_between_two_messages_is_still_a_predecessor() {
        let (mut state, uri) = coloured("8=FIX.4.4|35=0\n\n8=FIX.4.4|35=2", 100);
        let (id, before) = ask_full(&mut state, &uri);
        edit_at(&mut state, &uri, 2, (0, 13, 0, 14), "9");
        agrees_with_a_fresh_answer(&mut state, &uri, &before, &id);
    }

    /// Several changes arrive before one delta is asked for, and each addressed
    /// the document the one before it left.
    #[test]
    fn a_run_of_changes_is_one_report() {
        let (mut state, uri) = coloured(THREE, 100);
        let (id, before) = ask_full(&mut state, &uri);
        edit_at(&mut state, &uri, 2, (2, 14, 2, 14), "\n8=FIX.4.4|35=B");
        edit_at(&mut state, &uri, 3, (0, 13, 0, 14), "7");
        agrees_with_a_fresh_answer(&mut state, &uri, &before, &id);
    }

    #[test]
    fn deleting_every_line_leaves_an_empty_answer() {
        let (mut state, uri) = coloured(THREE, 100);
        let (id, before) = ask_full(&mut state, &uri);
        edit_at(&mut state, &uri, 2, (0, 0, 2, 14), "");
        let answer = ask_delta(&mut state, &uri, &id);
        assert!(apply_edits(&before, &edits_of(&answer)).is_empty());
    }

    /// An id the server does not know is the routine path after a refresh, so
    /// it is answered whole rather than refused.
    #[test]
    fn an_unknown_id_is_answered_whole() {
        let (mut state, uri) = coloured(THREE, 100);
        ask_full(&mut state, &uri);
        let answer = ask_delta(&mut state, &uri, "not an id we issued");
        assert!(answer.get("data").is_some(), "a whole answer: {answer}");
        assert!(answer.get("edits").is_none());
    }

    /// The protocol makes the id mandatory, so a request without one is
    /// malformed rather than a request for a whole answer.
    #[test]
    fn a_delta_without_an_id_is_refused() {
        let (mut state, uri) = coloured(THREE, 100);
        let dispatched =
            state.semantic_tokens_delta(&json!({ "textDocument": { "uri": uri.as_str() } }));
        let Dispatched::Now(Err(e)) = dispatched else {
            panic!("an error");
        };
        assert_eq!(e.code, lsp_server::ErrorCode::InvalidParams as i32);
        assert!(e.message.contains("previousResultId"), "{}", e.message);
    }

    /// The id is taken out before the answer is built, so a failure cannot
    /// leave the client retrying the same edit for the life of the document.
    #[test]
    fn asking_twice_with_one_id_gets_a_whole_answer_the_second_time() {
        let (mut state, uri) = coloured(THREE, 100);
        let (id, _) = ask_full(&mut state, &uri);
        edit_at(&mut state, &uri, 2, (1, 13, 1, 14), "5");
        assert!(ask_delta(&mut state, &uri, &id).get("edits").is_some());
        let again = ask_delta(&mut state, &uri, &id);
        assert!(again.get("data").is_some(), "the id is spent: {again}");
    }

    #[test]
    fn a_replaced_document_cannot_be_spliced() {
        let (mut state, uri) = coloured(THREE, 100);
        let (id, _) = ask_full(&mut state, &uri);
        state.on_did_change(json!({
            "textDocument": { "uri": uri.as_str(), "version": 2 },
            "contentChanges": [{ "text": "8=FIX.4.4|35=9" }],
        }));
        let answer = ask_delta(&mut state, &uri, &id);
        assert!(answer.get("data").is_some(), "a whole answer: {answer}");
    }

    /// Driven through the commit rather than by clearing the map, or the test
    /// would still pass with the invalidation taken out.
    #[test]
    fn a_configuration_change_drops_every_id() {
        let (mut state, uri) = coloured(THREE, 100);
        let (id, _) = ask_full(&mut state, &uri);
        assert!(state.tokens.contains_key(&uri));

        let mut config = state.config.base.clone();
        config.limits.semantic_tokens_full_lines = 1;
        state.config.base = config;
        super::super::testing::reconfigure(&mut state);
        assert!(state.tokens.is_empty(), "the ids the client holds are gone");

        let answer = ask_delta(&mut state, &uri, &id);
        assert!(answer.get("data").is_some(), "a whole answer: {answer}");
    }

    /// A viewport answer is a standalone array, so its first token names an
    /// absolute line — colouring from the top of the viewport instead would
    /// draw every token `range.start.line` lines too high.
    #[test]
    fn a_viewport_is_measured_from_the_top_of_the_document() {
        let text = format!("{THREE}\n8=FIX.4.4|35=3");
        let (mut state, uri) = coloured(&text, 100);
        let answer = ask_range(&mut state, &uri, 2, 3);
        let data = integers(&answer["data"]);
        assert_eq!(data[0], 2, "the first token is on line 2, not line 0");
    }

    /// The contract, stated for this stage's two shapes rather than left to
    /// the module doc: a viewport is bounded by the screen and may cache, and
    /// nothing that walks a whole document may.
    #[test]
    #[cfg(not(feature = "bench-no-line-cache"))] // asserts a cache exists
    fn a_viewport_caches_exactly_the_lines_it_answered_about() {
        let (mut state, uri) = coloured(&super::super::testing::heartbeats(5), 100);
        ask_range(&mut state, &uri, 1, 3);
        assert_eq!(
            super::super::testing::cached(&mut state, &uri),
            vec![false, true, true, false, false],
            "the range end is exclusive, so line 3 is not one of them"
        );
    }

    #[test]
    fn a_whole_answer_and_an_edit_cache_nothing() {
        let (mut state, uri) = coloured(&super::super::testing::heartbeats(5), 100);
        let (id, _) = ask_full(&mut state, &uri);
        assert!(
            super::super::testing::cached(&mut state, &uri)
                .iter()
                .all(|seen| !seen),
            "a whole answer walks every line and keeps none"
        );

        edit_at(&mut state, &uri, 2, (1, 13, 1, 14), "5");
        ask_delta(&mut state, &uri, &id);
        assert!(
            super::super::testing::cached(&mut state, &uri)
                .iter()
                .all(|seen| !seen),
            "and neither does an edit, which is bounded by the change and not \
             by a viewport"
        );
    }

    /// A truncated answer is one a client cannot tell from "nothing to colour
    /// there", so past the bound the answer is empty and says why.
    #[test]
    fn a_viewport_past_the_bound_is_answered_empty() {
        let text = format!("{THREE}\n8=FIX.4.4|35=3");
        let (mut state, uri) = coloured(&text, 2);
        let answer = ask_range(&mut state, &uri, 0, 3);
        assert_eq!(integers(&answer["data"]), Vec::<u32>::new());
        // And a viewport within the bound is answered normally.
        let answer = ask_range(&mut state, &uri, 0, 1);
        assert!(!integers(&answer["data"]).is_empty());
    }

    /// The schema permits zero, and no request is answered under it.
    #[test]
    fn a_bound_of_zero_colours_nothing_rather_than_overflowing() {
        let (mut state, uri) = coloured(THREE, 0);
        let answer = ask_range(&mut state, &uri, 0, 0);
        assert_eq!(integers(&answer["data"]), Vec::<u32>::new());
    }

    /// A line that yields no tokens still reports a count, so an edit on one
    /// must not send the client an edit that copies an empty stretch.
    #[test]
    fn editing_a_line_without_tokens_sends_no_edit() {
        let (mut state, uri) = coloured("8=FIX.4.4|35=0\nnot a fix line", 100);
        let (id, _) = ask_full(&mut state, &uri);
        edit_at(&mut state, &uri, 2, (1, 0, 1, 3), "NOT");
        let answer = ask_delta(&mut state, &uri, &id);
        assert_eq!(edits_of(&answer).len(), 0, "{answer}");
    }

    proptest::proptest! {
        /// The property the whole delta exists to keep, over random line edits
        /// against random caps: whatever the server sends — edits or a whole
        /// answer — the client ends up with the array it would have been given
        /// had it asked again.
        #[test]
        fn a_delta_leaves_the_client_holding_the_fresh_answer(
            lines in 1usize..7,
            cap in 1usize..7,
            edits in proptest::collection::vec((0usize..7, 0usize..7, 0usize..3), 1..3),
        ) {
            let mut model: Vec<String> =
                (0..lines).map(|at| format!("8=FIX.4.4|35={}", at % 10)).collect();
            let (mut state, uri) = coloured(&model.join("\n"), cap);
            let (id, before) = ask_full(&mut state, &uri);

            for (version, (from, to, inserted)) in edits.into_iter().enumerate() {
                let from = from.min(model.len() - 1);
                let to = to.min(model.len() - 1).max(from);
                let text: Vec<String> = (0..inserted)
                    .map(|at| format!("8=FIX.4.4|35={}", (at + 5) % 10))
                    .collect();
                edit_at(
                    &mut state,
                    &uri,
                    version as i32 + 2,
                    (from as u32, 0, to as u32, model[to].len() as u32),
                    &text.join("\n"),
                );
                let replacement: Vec<String> =
                    text.join("\n").split('\n').map(str::to_owned).collect();
                model.splice(from..=to, replacement);
            }

            let answer = ask_delta(&mut state, &uri, &id);
            let patched = match answer.get("edits") {
                Some(_) => apply_edits(&before, &edits_of(&answer)),
                // A whole answer is a permitted reply to a delta request.
                None => integers(&answer["data"]),
            };
            let (_, fresh) = ask_full(&mut state, &uri);
            prop_assert_eq!(patched, fresh);
        }
    }

    /// The one invariant the protocol rests on, asserted about every case here.
    fn records(data: &[u32], counts: &[u32]) -> Vec<[u32; STRIDE]> {
        assert_eq!(
            data.len(),
            STRIDE * counts.iter().sum::<u32>() as usize,
            "five integers per counted token"
        );
        data.as_chunks::<STRIDE>().0.to_vec()
    }

    /// A heartbeat, so every token is session traffic and both its tags belong
    /// to the header — which is what makes the modifiers identical here.
    #[test]
    fn one_line_is_measured_from_its_own_start() {
        let (data, counts) = whole("8=FIX.4.4|35=0", PositionEncoding::Utf8, false);
        assert_eq!(counts, [4]);
        let mods = Mod::Header.bit() | Mod::Session.bit();
        assert_eq!(
            records(&data, &counts),
            [
                [0, 0, 1, Kind::Property as u32, mods],
                [0, 2, 7, Kind::Namespace as u32, mods],
                [0, 8, 2, Kind::Property as u32, mods],
                [0, 3, 1, Kind::Keyword as u32, mods],
            ]
        );
    }

    /// A length is a distance in the negotiated encoding, so the same line
    /// carries different numbers under utf-16 — and only after the character
    /// that is wider there.
    #[test]
    fn a_wide_character_shifts_the_columns_only_under_utf16() {
        let line = "— 8=FIX.4.4|35=0";
        let (utf8, _) = whole(line, PositionEncoding::Utf8, false);
        let (utf16, _) = whole(line, PositionEncoding::Utf16, false);
        // The log prefix before the first tag is one comment token.
        assert_eq!(utf8[..STRIDE], [0, 0, 4, Kind::Comment as u32, 0]);
        assert_eq!(utf16[..STRIDE], [0, 0, 2, Kind::Comment as u32, 0]);
        assert_eq!(utf8.len(), utf16.len(), "the same tokens either way");
    }

    #[test]
    fn a_new_line_resets_the_start_and_an_empty_one_moves_nothing() {
        let (data, counts) = whole("8=FIX.4.4\n\n8=FIX.4.4", PositionEncoding::Utf8, false);
        assert_eq!(counts, [2, 0, 2], "a line reports zero rather than nothing");
        let records = records(&data, &counts);
        assert_eq!(records[2][0], 2, "two lines on, the empty one skipped");
        assert_eq!(records[2][1], 0, "and measured from the line start");
    }

    /// The overlay begins where its entry's first field begins, so it is the
    /// case where two tokens share a start and the second delta is zero.
    #[test]
    fn an_overlay_shares_a_start_with_the_field_after_it() {
        let (data, counts) = whole(
            "8=FIX.4.4|35=W|268=1|269=0|270=1.5",
            PositionEncoding::Utf8,
            true,
        );
        let records = records(&data, &counts);
        let at = records
            .iter()
            .position(|record| record[3] == Kind::Group as u32)
            .expect("an overlay");
        assert_eq!(records[at][1], 0, "it starts where the tag before it did");
        assert_eq!(records[at][0], 0, "on the same line");
    }

    /// An edit re-encodes a run in the middle of a document, so the first delta
    /// is measured from the last line before it that emitted anything.
    #[test]
    fn a_run_measures_its_first_delta_from_the_origin_it_was_given() {
        let snapshot = snapshot("8=FIX.4.4\n8=FIX.4.4\n8=FIX.4.4", PositionEncoding::Utf8);
        let no = LineNo::new(2);
        let token = Token {
            range: ByteOffset::new(0)..ByteOffset::new(1),
            kind: Kind::Property,
            mods: 0,
        };
        let (mut data, mut counts) = (Vec::new(), Vec::new());
        encode(
            [(no, vec![token])],
            &snapshot,
            LineNo::new(0),
            &mut data,
            &mut counts,
        );
        assert_eq!(
            records(&data, &counts)[0][0],
            2,
            "two lines past the origin"
        );
    }
}
