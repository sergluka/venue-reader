//! `textDocument/selectionRange`: what "expand selection" expands to.
//!
//! Three levels, innermost first — but **which three depends on where the
//! cursor is**, because every level must contain the position it was asked
//! about. `field_at` matches anywhere in `tag_start..value_end`, so a cursor on
//! the tag digits or on the `=` is inside the *field* and not inside the value;
//! handing it a value range there would return something that excludes it,
//! which the protocol forbids outright.
//!
//! | cursor sits on | levels, innermost first |
//! | --- | --- |
//! | a value byte | value → field → … → line |
//! | a tag digit or the `=` | field → … → line |
//! | no field at all | line |
//!
//! The `…` is the group chain, and it is **every ancestor pair**: the entry a
//! field sits in, then the whole group that entry belongs to, then the same for
//! that group's parent, out to the line. A count field also expands through the
//! group it *opens* — it is deliberately not inside that group, so nothing else
//! would reach it, and `453=2` would otherwise jump straight to the line.
//! Equal ranges collapse, so a one-entry group whose entry is its only content
//! does not cost a keypress that moves nothing.
//!
//! That table is written once in code, in [`levels`]: a level is a byte range
//! and nothing else, so adding one — the group levels 4c-2 owes — is pushing a
//! range there. Converting the endpoints and folding the list into the
//! protocol's nested shape are separate steps that never learn what a level
//! means.
//!
//! **The response is positional**, so it cannot be filtered: one range per
//! requested position, in the order asked. A position past its line clamps to
//! the line; one past the document is answered with an empty range at itself.
//!
//! This is the contract's fourth permitted cache populator, and the narrowest:
//! a cursor is a point, and a user expanding a selection re-issues on the same
//! line repeatedly. It is permitted only under the bound below.

use lsp_types::{MessageType, Position, SelectionRangeParams};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ops::Range;

use super::dispatch::{Dispatched, HandlerResult, Notices, RequestTask, Snapshot};
use super::ServerState;
use crate::codec::{ByteOffset, GroupSpans, MessageAnalysis};
use crate::document::{Column, Columns, LineNo, Offsets};

impl ServerState {
    pub(super) fn selection_range_task(&mut self, params: &Value) -> Dispatched {
        let params: SelectionRangeParams =
            match super::params(super::method::SELECTION_RANGE, params) {
                Ok(params) => params,
                Err(e) => return Dispatched::Now(Err(e)),
            };
        let uri = params.text_document.uri.clone();
        if let Some(answer) = super::dispatch::configuration_not_ready(self, &uri) {
            return answer;
        }
        let cap = self
            .effective_config(&uri)
            .unwrap_or(&self.config.base)
            .limits
            .range_lines;
        let Some(snapshot) = self.snapshot_for(&uri) else {
            // Nothing to be inside.
            return Dispatched::Now(Ok(Value::Null));
        };

        // A bound on *distinct lines*, not on positions — see
        // `Limits::range_lines`.
        let touched: std::collections::BTreeSet<u32> =
            params.positions.iter().map(|at| at.line).collect();
        let coarse = touched.len() > cap;
        if coarse {
            self.log_message(
                MessageType::Info,
                &format!(
                    "fix-lsp: {} — selectionRange over {cap} lines is answered with whole \
                     lines only; raise fix.limits.rangeLines to change that",
                    uri.as_str(),
                ),
            );
        }
        Dispatched::Task(snapshot, prepare(params, coarse))
    }
}

fn prepare(params: SelectionRangeParams, coarse: bool) -> RequestTask {
    Box::new(move |snapshot: &Snapshot, _: &mut Notices| answer(&params, coarse, snapshot))
}

fn answer(params: &SelectionRangeParams, coarse: bool, snapshot: &Snapshot) -> HandlerResult {
    // Grouped, because otherwise every cost here is per *position*: converting
    // a column scans the line under utf-16 and `field_at` scans its fields, so
    // a thousand cursors on one line would walk it a thousand times. Grouping
    // alone does not fix that — the batching in `Offsets`/`Columns` below does,
    // and it is what bounds the walks at two per line.
    let mut by_line: BTreeMap<u32, Vec<(usize, Position)>> = BTreeMap::new();
    for (at, position) in params.positions.iter().enumerate() {
        by_line
            .entry(position.line)
            .or_default()
            .push((at, *position));
    }

    let mut out = vec![Value::Null; params.positions.len()];
    for (line_number, wanted) in by_line {
        let number = LineNo::new(line_number);
        // Past the document — or a line no offset can address, which is what
        // the `from_len` arm means: an empty range at the position itself keeps
        // the array aligned without claiming there is text there.
        let addressable = snapshot
            .line(number)
            .and_then(|line| Some((line, ByteOffset::from_len(line.bytes.len())?)));
        let Some((line, line_end)) = addressable else {
            for (at, position) in wanted {
                out[at] = json!({ "range": { "start": position, "end": position } });
            }
            continue;
        };

        // Past the bound the answer is the whole line, which needs neither an
        // analysis nor a cursor — so the bound that stops the work stops the
        // retention *and* the per-position conversion with it.
        if coarse {
            let levels = levels(line_end, None, line_end);
            let columns = Columns::of(line, [ByteOffset::new(0), line_end], snapshot.encoding);
            let whole = nested(&levels, number, &columns);
            for (at, _) in wanted {
                out[at] = whole.clone();
            }
            continue;
        }

        // The contract's narrowest permitted caller.
        let analysis = snapshot.analysis(line);
        let message = analysis.message();

        // Two walks of the line, whatever the array holds: one turning the
        // distinct columns into offsets, one turning the offsets the levels
        // need back into columns. Both batch, sort and deduplicate for the
        // caller — which is why neither is done here.
        let offsets = Offsets::of(
            line,
            wanted
                .iter()
                .map(|(_, position)| Column::new(position.character)),
            snapshot.encoding,
        );
        // Never `None`: an offset from `Offsets` is clamped to a length that
        // `line_end` already narrowed.
        let cursor_at = |position: &Position| {
            ByteOffset::from_len(offsets.at(Column::new(position.character))).unwrap_or(line_end)
        };

        let grouped = message.map(|message| Grouped {
            message,
            spans: message.group_spans(),
        });

        let cursors: BTreeMap<ByteOffset, Vec<Range<ByteOffset>>> = wanted
            .iter()
            .map(|(_, position)| cursor_at(position))
            .map(|byte| (byte, levels(byte, grouped.as_ref(), line_end)))
            .collect();

        // Every endpoint of every level, in one walk — the levels are the only
        // thing that knows which offsets those are.
        let columns = Columns::of(
            line,
            cursors
                .values()
                .flatten()
                .flat_map(|level| [level.start, level.end]),
            snapshot.encoding,
        );
        let answers: BTreeMap<ByteOffset, Value> = cursors
            .iter()
            .map(|(byte, levels)| (*byte, nested(levels, number, &columns)))
            .collect();

        for (at, position) in wanted {
            out[at] = answers[&cursor_at(&position)].clone();
        }
    }
    Ok(Value::Array(out))
}

/// One analysed line and its group spans: everything a cursor on it expands
/// through, built once per line.
///
/// A per-*cursor* value would carry the same two members plus a field index —
/// and two thirds of it would not vary with the cursor. The index is a lookup
/// instead, which is where it belongs.
struct Grouped<'a> {
    message: &'a MessageAnalysis,
    spans: GroupSpans,
}

/// What a cursor expands through, innermost first — the module doc's table, and
/// the only place it is written in code.
///
/// A level is a byte range and carries no other meaning. The line is always the
/// outermost, which is what makes the list non-empty — and why this is a
/// function over `Option<&Grouped>` rather than a method on `Grouped`: a line
/// that is not a message still has a line level, and a method would leave that
/// one written somewhere else.
fn levels(
    cursor: ByteOffset,
    grouped: Option<&Grouped<'_>>,
    line_end: ByteOffset,
) -> Vec<Range<ByteOffset>> {
    let mut out = Vec::with_capacity(3);
    if let Some(Grouped { message, spans }) = grouped {
        if let Some(field_idx) = message.field_idx_at(cursor) {
            let field = &message.fields[field_idx];
            // A cursor on the tag digits or the `=` is inside the field and not
            // inside its value; a value level there would exclude the position
            // the request was about.
            if cursor >= field.value_start() {
                out.push(field.value_start()..field.value_end());
            }
            out.push(field.span());

            // The group this field *opens*, if it opens one. A count field is
            // not inside its own group, so the ancestor walk below would never
            // reach it, and a cursor on `453=3` would jump straight to the line
            // — past the very group that tag announces.
            if let Some(own) = spans.group(field_idx as u32) {
                out.push(own);
            }

            // Then every ancestor pair, innermost first: the entry a field sits
            // in, then the whole group that entry belongs to, then the same for
            // the group's own parent. Walked by index rather than through
            // `ancestors`, which returns references outermost-first and
            // allocates.
            let mut climbing = field.group_idx();
            while let Some(idx) = climbing {
                if let Some(instance) = spans.instance(idx) {
                    out.push(instance);
                }
                let instance = &message.groups[idx as usize];
                if let Some(group) = spans.group(instance.count_field_idx) {
                    out.push(group);
                }
                climbing = instance.parent_idx();
            }
        }
    }
    out.push(ByteOffset::new(0)..line_end);
    out
}

/// The levels as the protocol wants them: a range carrying its parent.
///
/// Folded from the outside in, so each level is built once and handed to the
/// next as its parent. The list is never empty — the line is always a level.
fn nested(levels: &[Range<ByteOffset>], line: LineNo, columns: &Columns) -> Value {
    let mut node: Option<Value> = None;
    for level in levels.iter().rev() {
        let here = span(line, columns.at(level.start), columns.at(level.end));
        node = Some(match node {
            Some(parent) => nest(here, parent),
            None => here,
        });
    }
    node.expect("the line is always a level")
}

fn span(line: LineNo, from: Column, to: Column) -> Value {
    json!({
        "range": {
            "start": { "line": line, "character": from },
            "end": { "line": line, "character": to },
        },
    })
}

/// `inner` inside `parent`, unless they are the same range — a single-field
/// line whose field spans it gives field == line, and a level that expands
/// nothing is a step the user presses twice for no movement.
fn nest(mut inner: Value, parent: Value) -> Value {
    if inner["range"] == parent["range"] {
        return parent;
    }
    inner["parent"] = parent;
    inner
}

/// The levels alone — no snapshot, no cache, so this holds under every feature.
#[cfg(test)]
mod level_tests {
    use super::*;
    use crate::codec::{byte, LineAnalysis};
    use crate::ctx::{analyze, Ctx};

    const ORDER: &str = "8=FIX.4.4|35=D|11=ord-1|10=000|";
    /// Two parties, the second carrying two sub-IDs: a group, sibling entries
    /// and one nested group, which is every ancestor pair in one line.
    const PARTIES: &str = "8=FIX.4.4|35=D|453=2|448=A|447=D|452=1\
                           |448=B|447=D|452=2|802=2|523=S1|803=1|523=S2|803=2|54=1";

    /// Everything here asks the same question of a line: what does a cursor at
    /// each byte expand through?
    struct Line {
        analysis: LineAnalysis,
        end: ByteOffset,
    }

    fn line(text: &str) -> Line {
        Line {
            analysis: analyze(text.as_bytes(), &Ctx::for_test("FIX.4.4")),
            end: byte(text.len() as u32),
        }
    }

    impl Line {
        fn levels(&self, at: u32) -> Vec<Range<ByteOffset>> {
            let message = self.analysis.message().expect("a message");
            let grouped = Grouped {
                message,
                spans: message.group_spans(),
            };
            levels(byte(at), Some(&grouped), self.end)
        }

        /// The levels a client actually receives: `nest` collapses a level that
        /// expands nothing, so strictness is a property of the collapsed chain
        /// and not of the raw list.
        fn collapsed(&self, at: u32) -> Vec<Range<ByteOffset>> {
            let mut levels = self.levels(at);
            levels.dedup();
            levels
        }
    }

    /// The protocol's rule, asserted on ranges rather than on JSON: every level
    /// contains the cursor, and each sits strictly inside the next.
    #[test]
    fn every_level_contains_the_cursor_and_nests_in_its_parent() {
        for text in [ORDER, PARTIES] {
            let line = line(text);
            for at in 0..text.len() as u32 {
                let levels = line.collapsed(at);
                assert!(!levels.is_empty(), "{text:?} at {at}");
                for level in &levels {
                    assert!(
                        level.contains(&byte(at)),
                        "level {level:?} excludes the cursor at {at}"
                    );
                }
                for pair in levels.windows(2) {
                    let (inner, outer) = (&pair[0], &pair[1]);
                    assert!(
                        outer.start <= inner.start && inner.end <= outer.end,
                        "at {at}: {inner:?} is not inside {outer:?}"
                    );
                    assert!(inner != outer, "at {at}: {inner:?} expands nothing");
                }
            }
        }
    }

    /// The table in the module doc, read off the level count.
    #[test]
    fn a_value_gets_three_levels_a_tag_two_and_a_separator_one() {
        let line = line(ORDER);
        // "8=FIX.4.4|": tag at 0, `=` at 1, value from 2, separator at 9.
        assert_eq!(line.collapsed(2).len(), 3, "a value byte");
        assert_eq!(line.collapsed(0).len(), 2, "a tag digit");
        assert_eq!(line.collapsed(1).len(), 2, "the `=`");
        assert_eq!(line.collapsed(9).len(), 1, "a separator");
    }

    /// A field inside a group expands through its entry and then the whole
    /// group, and through the same pair again for every ancestor.
    #[test]
    fn a_field_in_a_nested_group_climbs_every_ancestor_pair() {
        let line = line(PARTIES);
        let at = PARTIES.find("523=S1").expect("a sub-ID") as u32 + 4;
        let levels = line.collapsed(at);
        let widths: Vec<u32> = levels.iter().map(|level| level.end - level.start).collect();
        assert_eq!(
            levels.len(),
            7,
            "value, field, entry, sub-group, party entry, party group, line — got {widths:?}"
        );
        assert!(
            widths.windows(2).all(|pair| pair[0] < pair[1]),
            "each level strictly wider than the last: {widths:?}"
        );
    }

    /// A count field is not inside the group it opens, so the ancestor walk
    /// never reaches it — without its own level, `453=2` would expand straight
    /// to the line, past the very group that tag announces.
    #[test]
    fn a_count_field_expands_through_the_group_it_opens() {
        let line = line(PARTIES);
        let at = PARTIES.find("453=2").expect("the count") as u32 + 4;
        let levels = line.collapsed(at);
        let group = &levels[2];
        assert!(
            group.start == byte(PARTIES.find("453=2").expect("the count") as u32),
            "the group starts at its own count tag: {group:?}"
        );
        assert!(
            group.end > byte(PARTIES.find("802=2").expect("the sub-count") as u32),
            "and covers every entry: {group:?}"
        );
    }
}

#[cfg(all(test, not(feature = "no-line-cache")))]
mod tests {
    use super::*;
    use crate::server::testing::{answered, cached, heartbeats, state_with};

    /// The contract's narrowest permitted caller: it populates the lines it was
    /// pointed at, and no others.
    #[test]
    fn it_populates_only_the_lines_it_was_asked_about() {
        let (mut state, uri) = state_with(&heartbeats(5));
        let params = json!({
            "textDocument": { "uri": uri.as_str() },
            "positions": [
                { "line": 1, "character": 18 },
                { "line": 3, "character": 18 },
            ],
        });
        let answers = answered(state.selection_range_task(&params));
        assert_eq!(answers.as_array().expect("an array").len(), 2);
        assert_eq!(
            cached(&mut state, &uri),
            vec![false, true, false, true, false]
        );
    }

    /// Past the bound it answers whole lines, which needs no analysis at all —
    /// so the bound that stops the work also stops the retention.
    #[test]
    fn past_the_bound_it_populates_nothing() {
        let (mut state, uri) = state_with(&heartbeats(5));
        state.config.base.limits.range_lines = 1;
        super::super::testing::reconfigure(&mut state);
        let params = json!({
            "textDocument": { "uri": uri.as_str() },
            "positions": [
                { "line": 1, "character": 18 },
                { "line": 3, "character": 18 },
            ],
        });
        answered(state.selection_range_task(&params));
        assert!(cached(&mut state, &uri).iter().all(|seen| !seen));
    }

    /// The shape the batching exists for: a thousand cursors on one long
    /// non-ASCII line. Unbatched it is O(positions x line) plus a quadratic
    /// dedupe — measured at ~2.2 s in this profile before `Offsets`, ~14 ms
    /// after. The bound is two orders of magnitude above the observed time
    /// rather than near it: this asserts the algorithm, not the machine.
    ///
    /// **Wide as well as long, and at distinct positions**: `field_at` scans a
    /// line's fields per cursor, so a line of three fields would leave that
    /// dimension untested, and repeating one position would let the dedupe hide
    /// every cursor but the first.
    #[test]
    fn a_thousand_cursors_on_one_long_line_stay_linear() {
        let mut line = String::from("8=FIX.4.4");
        for _ in 0..2000 {
            line.push_str("|58=");
            line.push_str(&"\u{43f}".repeat(50));
        }
        line.push_str("|10=001");
        let width = line.chars().count() as u32;
        let (mut state, uri) = state_with(&line);
        let params = json!({
            "textDocument": { "uri": uri.as_str() },
            "positions": (0..1000)
                .map(|at| json!({ "line": 0, "character": at * width / 1000 }))
                .collect::<Vec<Value>>(),
        });
        let started = std::time::Instant::now();
        let answers = answered(state.selection_range_task(&params));
        let elapsed = started.elapsed();
        assert_eq!(answers.as_array().expect("an array").len(), 1000);
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "a thousand cursors on one line took {elapsed:?}"
        );
    }
}
