//! `textDocument/documentSymbol`: the outline of a log.
//!
//! `DocumentSymbolParams` carries no range, so this walks the whole document
//! and therefore calls `ctx::analyze` and drops the result. It is not a
//! permitted cache populator, and `Line::cached_analysis` compiling here is the
//! easiest mistake in the module.

use lsp_types::{DocumentSymbolParams, MessageType, SymbolKind};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ops::Range;

use super::dispatch::{Dispatched, HandlerResult, Notices, RequestTask, Snapshot};
use super::ServerState;
use crate::codec::{tokenize, ByteOffset, GroupSpans, LineKind, MessageAnalysis};
use crate::config::SymbolDepth;
use crate::document::{Columns, LineNo};
use crate::render::{symbol, Options};

/// Deepest group nesting the outline represents.
///
/// The codec accepts 32 levels; each one is a symbol object plus a `children`
/// array, so 32 alternating group and entry levels serialise to roughly 130
/// levels of JSON — past the 128 a default `serde_json` accepts, and that is
/// the parser on both sides of this wire. Anything deeper attaches to the last
/// representable ancestor instead of growing the tree.
const MAX_NESTING: u8 = 24;

/// What the outline needs from configuration, gathered where the config lives.
#[derive(Clone)]
pub(super) struct Wanted {
    depth: SymbolDepth,
    kinds: Kinds,
    /// `limits.documentSymbolItems`: the most symbols one answer may hold, the
    /// truncation marker counted among them.
    max_symbols: usize,
}

/// The `SymbolKind` numbers to answer with.
///
/// A client that advertises no `symbolKind.valueSet` is only guaranteed to
/// understand `File` (1) through `Array` (18), so the two kinds outside that
/// range are answered as the nearest legacy ones.
#[derive(Clone, Copy)]
struct Kinds {
    message: SymbolKind,
    instance: SymbolKind,
    /// Both inside the legacy set, so neither adapts — kept here so that all
    /// four are read in one place and the next kind cannot be added past the
    /// adaptation.
    group: SymbolKind,
    field: SymbolKind,
    /// A line of text rather than a structure; the kinds that would read
    /// better are outside the legacy set.
    marker: SymbolKind,
}

impl Kinds {
    fn for_client(extended: bool) -> Kinds {
        let (message, instance) = if extended {
            (SymbolKind::Object, SymbolKind::Struct)
        } else {
            (SymbolKind::Class, SymbolKind::Array)
        };
        Kinds {
            message,
            instance,
            group: SymbolKind::Array,
            field: SymbolKind::Field,
            marker: SymbolKind::String,
        }
    }
}

/// Whether the client takes a nested answer. Absent means flat: that is the
/// protocol's older default, not a refusal read out of silence.
pub(super) fn takes_nested_symbols(caps: &lsp_types::ClientCapabilities) -> bool {
    caps.text_document
        .as_ref()
        .and_then(|doc| doc.document_symbol.as_ref())
        .and_then(|symbol| symbol.hierarchical_document_symbol_support)
        .unwrap_or(false)
}

/// Whether the client listed the kinds it understands. Absent, it gets the set
/// the protocol has always guaranteed.
pub(super) fn lists_symbol_kinds(caps: &lsp_types::ClientCapabilities) -> bool {
    caps.text_document
        .as_ref()
        .and_then(|doc| doc.document_symbol.as_ref())
        .and_then(|symbol| symbol.symbol_kind.as_ref())
        .is_some_and(|kinds| kinds.value_set.is_some())
}

impl ServerState {
    pub(super) fn document_symbol_task(&mut self, params: &Value) -> Dispatched {
        let params: DocumentSymbolParams =
            match super::params(super::method::DOCUMENT_SYMBOL, params) {
                Ok(params) => params,
                Err(e) => return Dispatched::Now(Err(e)),
            };
        let uri = &params.text_document.uri;
        if let Some(answer) = super::dispatch::configuration_not_ready(self, uri) {
            return answer;
        }
        let base = &self
            .effective_config(uri)
            .unwrap_or(&self.config.base)
            .document_symbol;
        if !base.enabled {
            return Dispatched::Now(Ok(json!([])));
        }
        let wanted = Wanted {
            depth: base.depth,
            kinds: Kinds::for_client(self.client.extended_symbol_kinds),
            max_symbols: self
                .effective_config(uri)
                .unwrap_or(&self.config.base)
                .limits
                .document_symbol_items,
        };
        match self.snapshot_for(&params.text_document.uri) {
            Some(snapshot) => Dispatched::Task(snapshot, prepare(wanted)),
            // Closed between the request and here.
            None => Dispatched::Now(Ok(json!([]))),
        }
    }
}

fn prepare(wanted: Wanted) -> RequestTask {
    Box::new(move |snapshot: &Snapshot, notices: &mut Notices| answer(&wanted, snapshot, notices))
}

/// How the budget is spent, decided before any tree is built.
///
/// Roots are counted first and reserved. Filling children in document order
/// without that reservation exhausts the cap on the first messages and returns
/// an answer truncated from the *middle* — on a document where every message
/// alone would have fitted.
struct Budget {
    /// Message lines the document holds, whether or not they are answered.
    total: usize,
    /// Roots the answer carries. Below `total` only when the marker is needed.
    roots: usize,
    /// Items left over for children once every root is reserved.
    children: usize,
}

impl Budget {
    fn spend(cap: usize, total: usize) -> Budget {
        // The schema refuses zero, because a cap of zero cannot hold the marker
        // it would need; a client that sends one anyway gets the marker.
        let cap = cap.max(1);
        if total > cap {
            // The marker counts, which is what makes this `cap` and not `cap+1`.
            Budget {
                total,
                roots: cap - 1,
                children: 0,
            }
        } else {
            Budget {
                total,
                roots: total,
                children: cap - total,
            }
        }
    }

    fn omitted(&self) -> Option<usize> {
        (self.total > self.roots).then(|| self.total - self.roots)
    }
}

fn answer(wanted: &Wanted, snapshot: &Snapshot, notices: &mut Notices) -> HandlerResult {
    // Classification, not analysis: `tokenize` needs no dictionary, and the
    // roots have to be counted before the first one is built.
    let lines: Vec<LineNo> = (0..snapshot.content_lines())
        .map(|number| LineNo::new(number as u32))
        .filter(|number| {
            snapshot
                .line(*number)
                .is_some_and(|line| tokenize(&line.bytes).kind == LineKind::Message)
        })
        .collect();
    let budget = Budget::spend(wanted.max_symbols, lines.len());
    // Taken before the loop consumes the list: the marker is anchored on the
    // first message it stands for.
    let first_omitted = lines.get(budget.roots).copied();

    let mut out = Vec::new();
    let mut room = budget.children;
    // Once a message's children do not fit, no later message gets any: a
    // deterministic prefix, rather than an outline where a small message three
    // screens down happens to have fields and its neighbours do not.
    //
    // False from the start when the roots took everything, so that a document
    // with more messages than the cap builds no subtree at all rather than one.
    let mut children_fit = budget.children > 0;
    for number in lines.into_iter().take(budget.roots) {
        let Some(line) = snapshot.line(number) else {
            continue;
        };
        // `analyze`, not `cached_analysis`: this walk is whole-document.
        let analysis = crate::ctx::analyze(&line.bytes, &snapshot.ctx);
        let Some(message) = analysis.message() else {
            continue;
        };
        let Some(line_end) = ByteOffset::from_len(line.bytes.len()) else {
            continue;
        };
        // All of a message's children or none of them. A message showing three
        // of its sixty fields says something false about the line; a message
        // showing none says only that the outline stopped.
        //
        // Whether a message's children fit is known only once they are built —
        // group symbols change the count — so the *first* rejected subtree is
        // built and dropped. Every later one is not built at all, which is what
        // makes the degraded path the cheap one it exists to be.
        let mut outline = tree(
            &line.bytes,
            message,
            wanted,
            &snapshot.render,
            line_end,
            children_fit,
        );
        if children_fit {
            let wanted_children = outline.count() - 1;
            if wanted_children <= room {
                room -= wanted_children;
            } else {
                children_fit = false;
                outline.children.clear();
            }
        }

        // Every offset of the line in one walk: a tree produces many, and
        // converting them one at a time rescans the line for each.
        let mut offsets = Vec::new();
        outline.offsets(&mut offsets);
        let columns = Columns::of(line, offsets, snapshot.encoding);
        out.push(outline.to_json(number, &columns));
    }

    if let Some(omitted) = budget.omitted() {
        // Sound because `omitted()` is `Some` only when `roots` is below the
        // number of message lines, so there is a first one it stands for.
        // Asserted rather than defaulted: a marker on the wrong line is worse
        // than a loud failure, and dropping it would break "never a bare `[]`".
        let at = first_omitted.expect("a message the marker stands for");
        out.push(marker(omitted, at, wanted.kinds.marker));
    }
    // Only when something was actually left out. Reaching the cap exactly is
    // not omission, and an empty document has nothing to report.
    if budget.omitted().is_some() || !children_fit {
        notices.log(
            MessageType::Info,
            &format!(
                "fix-lsp: the outline is truncated at {} symbols; raise \
                 fix.limits.documentSymbolItems or lower fix.documentSymbol.depth",
                wanted.max_symbols.max(1),
            ),
        );
    }
    Ok(Value::Array(out))
}

/// What the budget left out, as a symbol rather than a silence.
///
/// Anchored on the first message it stands for — "after the last symbol
/// emitted" does not exist when the marker is the only thing returned. Its
/// kind is `String` because it is a line of text rather than a structure, and
/// because the kinds that would read better are outside the set a client
/// understands without listing them.
fn marker(omitted: usize, at: LineNo, kind: SymbolKind) -> Value {
    let empty = json!({ "line": at, "character": 0 });
    let range = json!({ "start": empty, "end": empty });
    json!({
        "name": symbol::truncated(omitted),
        "kind": kind,
        "range": range,
        "selectionRange": range,
    })
}

/// One outline row, in bytes. Columns are a boundary concern and come later.
struct Symbol {
    name: String,
    kind: SymbolKind,
    range: Range<ByteOffset>,
    selection: Range<ByteOffset>,
    children: Vec<Symbol>,
}

impl Symbol {
    fn count(&self) -> usize {
        1 + self.children.iter().map(Symbol::count).sum::<usize>()
    }

    fn offsets(&self, out: &mut Vec<ByteOffset>) {
        out.extend([
            self.range.start,
            self.range.end,
            self.selection.start,
            self.selection.end,
        ]);
        for child in &self.children {
            child.offsets(out);
        }
    }

    fn to_json(&self, line: LineNo, columns: &Columns) -> Value {
        let span = |range: &Range<ByteOffset>| {
            json!({
                "start": { "line": line, "character": columns.at(range.start) },
                "end": { "line": line, "character": columns.at(range.end) },
            })
        };
        let mut node = json!({
            "name": self.name,
            "kind": self.kind,
            "range": span(&self.range),
            "selectionRange": span(&self.selection),
        });
        if !self.children.is_empty() {
            node["children"] = Value::Array(
                self.children
                    .iter()
                    .map(|child| child.to_json(line, columns))
                    .collect(),
            );
        }
        node
    }
}

fn tree(
    line: &[u8],
    message: &MessageAnalysis,
    wanted: &Wanted,
    options: &Options,
    line_end: ByteOffset,
    with_children: bool,
) -> Symbol {
    let whole = ByteOffset::new(0)..line_end;
    // MsgType(35) is what a message is *about*, so that is what a jump should land
    // on. A message line need not carry one, and then its first field is the
    // nearest thing to an identity it has.
    let selection = message
        .first(35)
        .or(message.fields.first())
        .map_or(whole.clone(), |field| field.span());
    Symbol {
        name: symbol::message(line, message, options),
        kind: wanted.kinds.message,
        range: whole,
        selection,
        children: if with_children {
            children(line, message, wanted)
        } else {
            Vec::new()
        },
    }
}

fn children(line: &[u8], message: &MessageAnalysis, wanted: &Wanted) -> Vec<Symbol> {
    match wanted.depth {
        SymbolDepth::Messages => Vec::new(),
        // Groups flattened away: every field is a row of the message, in wire
        // order, which is what a reader scanning for a tag wants.
        SymbolDepth::Fields => (0..message.fields.len())
            .map(|at| field_symbol(line, message, at, wanted))
            .collect(),
        SymbolDepth::Full => nested(line, message, wanted),
    }
}

/// A group symbol cannot be built until its entries are, so a field's slot
/// records which of the two it is and the second pass fills them in.
enum Slot {
    Field(Symbol),
    /// Index of the count field that opens the group.
    Group(usize),
}

fn nested(line: &[u8], message: &MessageAnalysis, wanted: &Wanted) -> Vec<Symbol> {
    let spans = message.group_spans();
    // Keyed by the instance a field belongs to; `None` is the message itself.
    let mut owned: BTreeMap<Option<u32>, Vec<Slot>> = BTreeMap::new();
    let mut first_field: BTreeMap<u32, usize> = BTreeMap::new();

    for (at, field) in message.fields.iter().enumerate() {
        let owner = representable(message, field.group_idx());
        if let Some(instance) = field.group_idx() {
            first_field.entry(instance).or_insert(at);
        }
        // A group symbol *is* how its count field appears; emitting a field row
        // for it as well would show the tag twice. A count field that opened no
        // entries — a zero, negative or unparseable count — has no group, and
        // stays an ordinary field.
        let opens = spans
            .group(at as u32)
            .filter(|_| group_depth(message, field.group_idx()) <= MAX_NESTING);
        let slot = match opens {
            Some(_) => Slot::Group(at),
            None => Slot::Field(field_symbol(line, message, at, wanted)),
        };
        owned.entry(owner).or_default().push(slot);
    }

    // Deepest first, so a group's entries exist by the time the entry that
    // contains that group is built. Instances are pushed parent-before-child,
    // so reverse index order is deepest first.
    let mut entries: BTreeMap<usize, Vec<Symbol>> = BTreeMap::new();
    for idx in (0..message.groups.len()).rev() {
        let instance = &message.groups[idx];
        if instance.depth > MAX_NESTING {
            continue;
        }
        let Some(range) = spans.instance(idx as u32) else {
            continue;
        };
        let selection = first_field
            .get(&(idx as u32))
            .map_or(range.clone(), |at| message.fields[*at].span());
        let children = resolve(
            owned.remove(&Some(idx as u32)).unwrap_or_default(),
            &mut entries,
            line,
            message,
            &spans,
            wanted,
        );
        entries
            .entry(instance.count_field_idx as usize)
            .or_default()
            .push(Symbol {
                name: symbol::instance(instance.entry),
                kind: wanted.kinds.instance,
                range,
                selection,
                children,
            });
    }

    resolve(
        owned.remove(&None).unwrap_or_default(),
        &mut entries,
        line,
        message,
        &spans,
        wanted,
    )
}

fn resolve(
    slots: Vec<Slot>,
    entries: &mut BTreeMap<usize, Vec<Symbol>>,
    line: &[u8],
    message: &MessageAnalysis,
    spans: &GroupSpans,
    wanted: &Wanted,
) -> Vec<Symbol> {
    slots
        .into_iter()
        .map(|slot| match slot {
            Slot::Field(sym) => sym,
            Slot::Group(at) => {
                // Built deepest first, so they are in reverse entry order here.
                let mut children = entries.remove(&at).unwrap_or_default();
                children.reverse();
                let count = &message.fields[at];
                // Both are `Some`: a `Group` slot is only made for a count
                // field the span table answered for.
                let (range, of) = spans
                    .group(at as u32)
                    .zip(spans.group_def(at as u32))
                    .expect("the slot was made from this table");
                Symbol {
                    name: symbol::group(line, message, at, of),
                    kind: wanted.kinds.group,
                    range,
                    selection: count.span(),
                    children,
                }
            }
        })
        .collect()
}

fn field_symbol(line: &[u8], message: &MessageAnalysis, at: usize, wanted: &Wanted) -> Symbol {
    let field = &message.fields[at];
    Symbol {
        name: symbol::field(line, message, at),
        kind: wanted.kinds.field,
        range: field.span(),
        // The tag, not the whole field: a jump lands where the name is.
        selection: field.tag_start..field.tag_end(),
        children: Vec::new(),
    }
}

/// One deeper than the instance the count field sits in.
fn group_depth(message: &MessageAnalysis, owner: Option<u32>) -> u8 {
    owner.map_or(1, |idx| message.groups[idx as usize].depth + 1)
}

/// The nearest ancestor instance the outline still represents.
///
/// Past [`MAX_NESTING`] a field attaches to the last representable ancestor
/// rather than to an entry that was never emitted.
fn representable(message: &MessageAnalysis, mut owner: Option<u32>) -> Option<u32> {
    while let Some(idx) = owner {
        let instance = &message.groups[idx as usize];
        if instance.depth <= MAX_NESTING {
            return Some(idx);
        }
        owner = instance.parent_idx();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SymbolDepth;
    #[cfg(not(feature = "bench-no-line-cache"))]
    use crate::server::testing::cached;
    use crate::server::testing::heartbeats;
    use crate::server::testing::{answered, answered_with, state_with, HEARTBEAT};

    /// Two parties, the first carrying a nested sub-ID group: every shape the
    /// outline can produce, on one line.
    const PARTIES: &str = "8=FIX.4.4|35=D|453=2|448=A|447=D|452=1|802=1|523=S1|803=1\
                           |448=B|447=D|452=2|54=1";

    fn request(uri: &lsp_types::Uri) -> Value {
        json!({ "textDocument": { "uri": uri.as_str() } })
    }

    fn outline(text: &str) -> Value {
        let (mut state, uri) = state_with(text);
        answered(state.document_symbol_task(&request(&uri)))
    }

    /// The tree as one readable block: names, kinds and ranges at once, so a
    /// change of shape fails as a shape rather than as an index.
    fn shape(answer: &Value) -> String {
        fn walk(node: &Value, depth: usize, out: &mut Vec<String>) {
            out.push(format!(
                "{}{} [{}] {}..{}",
                "  ".repeat(depth),
                node["name"].as_str().expect("a name"),
                node["kind"],
                node["range"]["start"]["character"],
                node["range"]["end"]["character"],
            ));
            for child in node["children"].as_array().unwrap_or(&Vec::new()) {
                walk(child, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        for node in answer.as_array().expect("an array") {
            walk(node, 0, &mut out);
        }
        out.join("\n")
    }

    #[test]
    fn a_group_becomes_an_array_of_entries_holding_their_own_fields() {
        assert_eq!(
            shape(&outline(PARTIES)),
            "\
# → NewOrderSingle (D) [5] 0..80
  BeginString (8) = FIX.4.4 [8] 0..9
  MsgType (35) = D (NewOrderSingle) [8] 10..14
  NoPartyIDs (453) = 2 [18] 15..75
    [1] [18] 21..57
      PartyID (448) = A [8] 21..26
      PartyIDSource (447) = D (Proprietary) [8] 27..32
      PartyRole (452) = 1 (ExecutingFirm) [8] 33..38
      NoPartySubIDs (802) = 1 [18] 39..57
        [1] [18] 45..57
          PartySubID (523) = S1 [8] 45..51
          PartySubIDType (803) = 1 (Firm) [8] 52..57
    [2] [18] 58..75
      PartyID (448) = B [8] 58..63
      PartyIDSource (447) = D (Proprietary) [8] 64..69
      PartyRole (452) = 2 (BrokerOfCredit) [8] 70..75
  Side (54) = 1 (Buy) [8] 76..80"
        );
    }

    /// The two rules the protocol states, over every node of a tree that has
    /// every shape in it.
    #[test]
    fn every_child_is_inside_its_parent_and_every_selection_inside_its_range() {
        fn check(node: &Value) {
            let range = |of: &Value| {
                (
                    of["start"]["character"].as_u64().expect("start"),
                    of["end"]["character"].as_u64().expect("end"),
                )
            };
            let own = range(&node["range"]);
            let selection = range(&node["selectionRange"]);
            assert!(
                own.0 <= selection.0 && selection.1 <= own.1,
                "selectionRange {selection:?} outside range {own:?}"
            );
            for child in node["children"].as_array().unwrap_or(&Vec::new()) {
                let kid = range(&child["range"]);
                assert!(
                    own.0 <= kid.0 && kid.1 <= own.1,
                    "child {kid:?} outside parent {own:?}"
                );
                check(child);
            }
        }
        for node in outline(PARTIES).as_array().expect("an array") {
            check(node);
        }
    }

    /// A group symbol *is* how its count field appears, so NoPartyIDs(453) must not
    /// also be a field row — it would show the tag twice.
    #[test]
    fn a_count_field_appears_once_as_its_group() {
        let shape = shape(&outline(PARTIES));
        assert_eq!(shape.matches("453").count(), 1, "{shape}");
        assert!(shape.contains("NoPartyIDs (453) = 2 [18]"), "{shape}");
    }

    #[test]
    fn depth_decides_how_far_the_tree_goes() {
        let (mut state, uri) = state_with(PARTIES);

        state.config.base.document_symbol.depth = SymbolDepth::Messages;
        super::super::testing::reconfigure(&mut state);
        let messages = answered(state.document_symbol_task(&request(&uri)));
        assert_eq!(shape(&messages).lines().count(), 1, "the message alone");

        state.config.base.document_symbol.depth = SymbolDepth::Fields;
        super::super::testing::reconfigure(&mut state);
        let fields = answered(state.document_symbol_task(&request(&uri)));
        let flat = shape(&fields);
        assert_eq!(
            flat.lines().count(),
            1 + PARTIES
                .replace(char::is_whitespace, "")
                .matches('=')
                .count(),
            "one row per field, groups flattened away: {flat}"
        );
        assert!(
            flat.lines().skip(1).all(|row| row.starts_with("  ")),
            "and none of them nested: {flat}"
        );
    }

    /// `Object` and `Struct` are outside the set a client understands without
    /// advertising one, so silence gets the legacy kinds.
    #[test]
    fn kinds_outside_the_legacy_set_are_answered_only_to_a_client_that_asked() {
        let (mut state, uri) = state_with(PARTIES);
        let legacy = shape(&answered(state.document_symbol_task(&request(&uri))));
        assert!(legacy.starts_with("# → NewOrderSingle (D) [5]"), "{legacy}");
        assert!(legacy.contains("[1] [18]"), "an entry as Array: {legacy}");

        state.client.extended_symbol_kinds = true;
        let extended = shape(&answered(state.document_symbol_task(&request(&uri))));
        assert!(
            extended.starts_with("# → NewOrderSingle (D) [19]"),
            "a message as Object: {extended}"
        );
        assert!(
            extended.contains("[1] [23]"),
            "an entry as Struct: {extended}"
        );
    }

    /// A jump lands on MsgType(35), which is what the message is about — or on its
    /// first field when the line carries no 35 at all.
    #[test]
    fn the_message_selects_its_type_or_its_first_field() {
        let with_type = outline("8=FIX.4.4|35=D|54=1");
        assert_eq!(with_type[0]["selectionRange"]["start"]["character"], 10);

        let without = outline("8=FIX.4.4|54=1");
        assert_eq!(
            without[0]["selectionRange"]["start"]["character"], 0,
            "the first field, not the whole line"
        );
        assert_eq!(without[0]["selectionRange"]["end"]["character"], 9);
    }

    /// The contract's fifth hand-written witness: a whole-document walk may
    /// not populate a line's cache. Gated on the cache existing — under
    /// `bench-no-line-cache` nothing can be populated and the assertion would hold
    /// without proving anything.
    #[cfg(not(feature = "bench-no-line-cache"))]
    #[test]
    fn walking_the_document_populates_nothing() {
        let (mut state, uri) = state_with(&heartbeats(5));
        answered(state.document_symbol_task(&request(&uri)));
        assert!(cached(&mut state, &uri).iter().all(|seen| !seen));
    }

    /// A heartbeat is eight fields, so nine symbols with its message row.
    const PER_HEARTBEAT: usize = 9;

    /// The answer and whatever the handler said while producing it.
    fn under_cap(text: &str, cap: usize) -> (Value, Vec<(lsp_types::MessageType, String)>) {
        let (mut state, uri) = state_with(text);
        state.config.base.limits.document_symbol_items = cap;
        super::super::testing::reconfigure(&mut state);
        answered_with(state.document_symbol_task(&request(&uri)))
    }

    fn rows(answer: &Value) -> usize {
        answer.as_array().expect("an array").len()
    }

    fn symbols(answer: &Value) -> usize {
        fn count(nodes: &[Value]) -> usize {
            nodes
                .iter()
                .map(|node| 1 + count(node["children"].as_array().unwrap_or(&Vec::new())))
                .sum()
        }
        count(answer.as_array().expect("an array"))
    }

    /// Roots are reserved before children are spent. Without that, the children
    /// of the first messages eat the cap and the answer is truncated from the
    /// middle of a document where every message alone would have fitted.
    #[test]
    fn every_message_is_answered_when_the_messages_alone_fit() {
        let (answer, said) = under_cap(&heartbeats(3), 3);
        assert_eq!(rows(&answer), 3, "three messages, no marker");
        assert_eq!(symbols(&answer), 3, "and no room left for a single field");
        assert_eq!(
            said.len(),
            1,
            "the children that were dropped are said once"
        );
    }

    /// All of a message's children or none: three of sixty fields would say
    /// something false about the line.
    #[test]
    fn children_are_dropped_per_message_and_never_by_half() {
        // Room for one message's children and one item over.
        let (answer, _) = under_cap(&heartbeats(2), 2 + PER_HEARTBEAT - 1 + 1);
        let rows = answer.as_array().expect("an array");
        assert_eq!(
            rows[0]["children"].as_array().map(Vec::len),
            Some(PER_HEARTBEAT - 1),
            "the first message keeps every field"
        );
        assert!(
            rows[1]["children"].is_null(),
            "and the second keeps none: {}",
            rows[1]
        );
    }

    #[test]
    fn a_complete_answer_says_nothing() {
        let (answer, said) = under_cap(&heartbeats(3), 3 * PER_HEARTBEAT);
        assert_eq!(symbols(&answer), 3 * PER_HEARTBEAT, "an exact fit");
        assert!(said.is_empty(), "nothing was left out: {said:?}");
    }

    #[test]
    fn an_empty_document_is_not_something_to_report() {
        let (answer, said) = under_cap("# just a comment\n", 50);
        assert_eq!(answer, json!([]));
        assert!(said.is_empty(), "{said:?}");
    }

    /// Past the cap the answer is a deterministic prefix plus one marker, and
    /// the marker counts — which is what keeps the total at the cap and not one
    /// over it.
    #[test]
    fn past_the_cap_a_prefix_and_a_marker_that_counts_itself() {
        let (answer, said) = under_cap(&heartbeats(3), 2);
        assert_eq!(symbols(&answer), 2, "one message and the marker");
        let rows = answer.as_array().expect("an array");
        assert_eq!(rows[1]["name"], "… 2 more messages");
        assert_eq!(
            rows[1]["range"]["start"]["line"], 1,
            "anchored on the first message it stands for"
        );
        assert_eq!(said.len(), 1);
    }

    /// A cap of one leaves no room for a root at all, so the marker stands for
    /// every message. The schema refuses zero for the same reason.
    #[test]
    fn a_cap_of_one_is_the_marker_alone() {
        let (answer, _) = under_cap(&heartbeats(3), 1);
        assert_eq!(rows(&answer), 1);
        assert_eq!(answer[0]["name"], "… 3 more messages");

        let (mut state, uri) = state_with(&heartbeats(3));
        let snapshot = state.snapshot_for(&uri).expect("ready");
        let zero = super::answer(
            &Wanted {
                depth: SymbolDepth::Full,
                kinds: Kinds::for_client(false),
                max_symbols: 0,
            },
            &snapshot,
            &mut Notices::collecting(),
        )
        .expect("defensive zero bound");
        assert_eq!(zero, answer, "a zero that arrives anyway is treated as one");
    }

    /// The classification pass counts messages, not lines: a comment between
    /// them must not spend a root.
    #[test]
    fn lines_that_are_not_messages_do_not_spend_the_budget() {
        let text = format!("{}\n# a comment\n{}", HEARTBEAT, HEARTBEAT);
        let (answer, said) = under_cap(&text, 2);
        assert_eq!(rows(&answer), 2, "two messages, no marker");
        assert!(
            answer.as_array().expect("an array")[1]["name"] != "… 1 more messages",
            "the comment was not counted as an omitted message"
        );
        assert_eq!(said.len(), 1, "only the children were dropped");
    }

    #[test]
    fn a_line_that_is_not_a_message_has_no_row() {
        assert_eq!(outline("# a comment\n\nplain prose"), json!([]));
    }

    #[test]
    fn the_outline_can_be_switched_off_without_withdrawing_the_provider() {
        let (mut state, uri) = state_with(PARTIES);
        state.config.base.document_symbol.enabled = false;
        super::super::testing::reconfigure(&mut state);
        assert_eq!(
            answered(state.document_symbol_task(&request(&uri))),
            json!([])
        );
    }
}
