//! `textDocument/documentSymbol`: the outline of a log.
//!
//! Whole-document enumeration may read existing analyses, but never populates
//! the line cache or refreshes its eviction marks.

use lsp_server::{ErrorCode, ResponseError};
use lsp_types::{DocumentSymbolParams, SymbolKind};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::dispatch::{BackgroundEvent, BackgroundResult, BackgroundTask, Dispatched, Snapshot};
use super::ServerState;
use crate::codec::{tokenize, ByteOffset, GroupSpans, LineKind, MessageAnalysis};
use crate::config::SymbolDepth;
use crate::document::{Column, Columns, LineNo};
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
struct Wanted {
    depth: SymbolDepth,
    kinds: Kinds,
    /// `limits.documentSymbolItems`: the most symbols one answer may hold, the
    /// truncation marker counted among them.
    max_symbols: usize,
    #[cfg(test)]
    skip_line: Option<LineNo>,
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
    pub(super) fn document_symbol_task_prepared(
        &mut self,
        params: DocumentSymbolParams,
    ) -> Dispatched {
        let uri = &params.text_document.uri;
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
            #[cfg(test)]
            skip_line: None,
        };
        let Some(snapshot) = self.snapshot_for(uri) else {
            // Closed between the request and here.
            return Dispatched::Now(Ok(json!([])));
        };
        let partial = params.partial_result_params.partial_result_token;
        let streaming = partial.is_some();
        Dispatched::Background(BackgroundTask {
            method: super::method::DOCUMENT_SYMBOL,
            title: "Document symbols",
            uri: params.text_document.uri,
            snapshot,
            work_done: params.work_done_progress_params.work_done_token,
            partial,
            run: Box::new(move |snapshot, cancelled, emit| {
                enumerate(&wanted, snapshot, cancelled, streaming, emit)
            }),
        })
    }
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
    /// `cap` is at least one: the marker needs a slot.
    fn spend(cap: usize, total: usize) -> Budget {
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

    fn omitted(&self) -> usize {
        self.total - self.roots
    }
}

/// Bounds of one streamed chunk: its symbols, its bytes and its age.
const CHUNK_ITEMS: usize = 256;
const CHUNK_BYTES: usize = 64 * 1024;
const CHUNK_INTERVAL: Duration = Duration::from_millis(20);

const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// Classification reads the clock once per this many lines, not on every line.
const CLOCK_STRIDE: usize = 1024;

fn check_cancel(cancelled: &AtomicBool) -> Result<(), ResponseError> {
    if cancelled.load(Ordering::Relaxed) {
        Err(ResponseError {
            code: ErrorCode::RequestCanceled as i32,
            message: "document symbols request cancelled".into(),
            data: None,
        })
    } else {
        Ok(())
    }
}

/// Share of the bar that classification gets. It is cheap beside the build,
/// so an equal split would fill half the bar at once and then appear to stop.
const CLASSIFY_SHARE: u64 = 10;

/// Progress through one phase, as that phase's share of the whole bar.
struct Progress {
    phase: &'static str,
    /// Percentage at which the phase starts.
    base: u64,
    share: u64,
    done: u64,
    of: u64,
    reported: Instant,
}

impl Progress {
    fn classifying(lines: usize) -> Progress {
        Progress {
            phase: "Classifying",
            base: 0,
            share: CLASSIFY_SHARE,
            done: 0,
            of: lines as u64,
            reported: Instant::now(),
        }
    }

    fn build(&mut self, roots: usize) {
        self.phase = "Building symbols";
        self.base = CLASSIFY_SHARE;
        self.share = 100 - CLASSIFY_SHARE;
        self.done = 0;
        self.of = roots as u64;
    }

    fn advance(&mut self, emit: &mut dyn FnMut(BackgroundEvent)) {
        self.done += 1;
        self.report(emit);
    }

    fn report(&mut self, emit: &mut dyn FnMut(BackgroundEvent)) {
        if self.reported.elapsed() >= PROGRESS_INTERVAL {
            let within = self.done.saturating_mul(self.share) / self.of.max(1);
            emit(BackgroundEvent::Progress {
                // 100 is said once, when the answer is complete.
                percentage: (self.base + within).min(99) as u32,
                phase: self.phase,
            });
            self.reported = Instant::now();
        }
    }
}

struct Root {
    number: LineNo,
    items: usize,
    /// The root and its subtree, as JSON.
    encoded: Vec<u8>,
}

/// The answer as one open JSON array: flushed in chunks when streaming, kept whole otherwise.
struct Output {
    streaming: bool,
    array: Vec<u8>,
    /// Symbols in `array` since the last flush.
    items: usize,
    sent: bool,
    flushed: Instant,
}

impl Output {
    fn new(streaming: bool) -> Self {
        Self {
            streaming,
            array: vec![b'['],
            items: 0,
            sent: false,
            flushed: Instant::now(),
        }
    }

    fn flush(&mut self, emit: &mut dyn FnMut(BackgroundEvent)) {
        if self.items == 0 {
            return;
        }
        self.array.push(b']');
        emit(BackgroundEvent::Chunk(std::mem::replace(
            &mut self.array,
            vec![b'['],
        )));
        self.items = 0;
        self.sent = true;
        self.flushed = Instant::now();
    }

    fn push(
        &mut self,
        root: Root,
        cancelled: &AtomicBool,
        emit: &mut dyn FnMut(BackgroundEvent),
    ) -> Result<(), ResponseError> {
        check_cancel(cancelled)?;
        let separator = usize::from(self.items != 0);
        if self.streaming
            && self.items != 0
            && (self.items + root.items > CHUNK_ITEMS
                || self.array.len() + separator + root.encoded.len() + 1 > CHUNK_BYTES)
        {
            self.flush(emit);
        }
        if self.items != 0 {
            self.array.push(b',');
        }
        self.array.extend_from_slice(&root.encoded);
        self.items += root.items;
        if self.streaming
            && (!self.sent
                || self.items >= CHUNK_ITEMS
                || self.array.len() + 1 >= CHUNK_BYTES
                || self.flushed.elapsed() >= CHUNK_INTERVAL)
        {
            self.flush(emit);
        }
        Ok(())
    }

    /// The final result: empty when every root went out in a chunk.
    fn finish(mut self, emit: &mut dyn FnMut(BackgroundEvent)) -> Vec<u8> {
        if self.streaming {
            self.flush(emit);
            return b"[]".to_vec();
        }
        self.array.push(b']');
        self.array
    }
}

fn encode(value: &impl Serialize) -> Vec<u8> {
    serde_json::to_vec(value).expect("a symbol is strings and numbers")
}

fn affected(first: &mut Option<LineNo>, number: LineNo) {
    *first = Some(first.map_or(number, |previous| previous.min(number)));
}

fn enumerate(
    wanted: &Wanted,
    snapshot: &Snapshot,
    cancelled: &AtomicBool,
    streaming: bool,
    emit: &mut dyn FnMut(BackgroundEvent),
) -> BackgroundResult {
    let started = Instant::now();
    let mut cache_hits = 0;
    let mut cache_misses = 0;
    let mut build_time = Duration::ZERO;
    let mut serialization = Duration::ZERO;
    // The schema refuses zero, because a cap of zero cannot hold the marker
    // it would need; a client that sends one anyway gets the marker.
    let cap = wanted.max_symbols.max(1);
    let physical_lines = snapshot.content_lines();
    let mut progress = Progress::classifying(physical_lines);
    let mut lines = Vec::with_capacity(physical_lines.min(cap.saturating_add(1)));
    let mut total = 0;
    for number in 0..physical_lines {
        check_cancel(cancelled)?;
        let number = LineNo::new(number as u32);
        // Tokenized always: a cache lookup locks the line, and a hit saves little here.
        if snapshot
            .line(number)
            .is_some_and(|line| tokenize(&line.bytes).kind == LineKind::Message)
        {
            total += 1;
            if lines.len() <= cap {
                lines.push(number);
            }
        }
        progress.done += 1;
        if number.index().is_multiple_of(CLOCK_STRIDE) {
            progress.report(emit);
        }
    }
    let classification_time = started.elapsed();
    let budget = Budget::spend(cap, total);
    let mut omitted = budget.omitted();
    let mut first_affected = lines.get(budget.roots).copied().filter(|_| omitted != 0);
    let mut children_omitted = false;
    let mut room = budget.children;
    let mut children_fit = room != 0;
    let mut pending: Option<Root> = None;
    let mut used = 0;
    let mut output = Output::new(streaming);
    progress.build(budget.roots);
    for number in lines.into_iter().take(budget.roots) {
        check_cancel(cancelled)?;
        let building = Instant::now();
        let root = snapshot.line(number).and_then(|line| {
            #[cfg(test)]
            if wanted.skip_line == Some(number) {
                return None;
            }
            let analysis = match line.analysis_if_cached() {
                Some(analysis) => {
                    cache_hits += 1;
                    analysis
                }
                None => {
                    cache_misses += 1;
                    std::sync::Arc::new(crate::ctx::analyze(&line.bytes, &snapshot.ctx))
                }
            };
            if cancelled.load(Ordering::Relaxed) {
                return None;
            }
            let message = analysis.message()?;
            let line_end = ByteOffset::from_len(line.bytes.len())?;
            let has_children = wanted.depth != SymbolDepth::Messages && !message.fields.is_empty();
            let mut outline = tree(
                &line.bytes,
                message,
                wanted,
                &snapshot.render,
                line_end,
                children_fit,
            );
            if cancelled.load(Ordering::Relaxed) {
                return None;
            }
            if children_fit {
                let count = outline.count() - 1;
                if count <= room {
                    room -= count;
                } else {
                    children_fit = false;
                    outline.children.clear();
                }
            }
            if has_children && !children_fit {
                children_omitted = true;
                affected(&mut first_affected, number);
            }
            let items = outline.count();
            let mut offsets = Vec::new();
            outline.offsets(&mut offsets);
            let columns = Columns::of(line, offsets, snapshot.encoding);
            if cancelled.load(Ordering::Relaxed) {
                return None;
            }
            let encoding = Instant::now();
            let encoded = encode(&Node {
                symbol: &outline,
                line: number,
                columns: &columns,
            });
            serialization += encoding.elapsed();
            Some(Root {
                number,
                items,
                encoded,
            })
        });
        build_time += building.elapsed();
        check_cancel(cancelled)?;
        if let Some(root) = root {
            used += root.items;
            // Retain the last actually produced root, even if a later candidate cannot render.
            if let Some(previous) = pending.replace(root) {
                output.push(previous, cancelled, emit)?;
            }
        } else {
            omitted += 1;
            affected(&mut first_affected, number);
        }
        progress.advance(emit);
    }
    if omitted != 0 || children_omitted {
        if used == cap {
            let displaced = pending.take().expect("a full answer has a final root");
            used -= displaced.items;
            omitted += 1;
            affected(&mut first_affected, displaced.number);
        }
        if let Some(root) = pending.take() {
            output.push(root, cancelled, emit)?;
        }
        let number = first_affected.expect("omission identifies an affected line");
        debug_assert!(used < cap);
        output.push(
            Root {
                number,
                items: 1,
                encoded: encode(&marker(
                    omitted,
                    children_omitted,
                    number,
                    wanted.kinds.marker,
                )),
            },
            cancelled,
            emit,
        )?;
        used += 1;
    } else if let Some(root) = pending.take() {
        output.push(root, cancelled, emit)?;
    }
    check_cancel(cancelled)?;
    let result = output.finish(emit);
    emit(BackgroundEvent::Progress {
        percentage: 100,
        phase: "Symbols generated",
    });
    log::debug!(
        "document symbols phases: classification_ms={:.3} build_ms={:.3} serialization_ms={:.3} total_ms={:.3} cache_hits={} cache_misses={} output_items={}",
        classification_time.as_secs_f64() * 1000.0,
        build_time.as_secs_f64() * 1000.0,
        serialization.as_secs_f64() * 1000.0,
        started.elapsed().as_secs_f64() * 1000.0,
        cache_hits, cache_misses, used,
    );
    Ok(result)
}

fn marker(omitted: usize, children: bool, at: LineNo, kind: SymbolKind) -> Value {
    let empty = json!({ "line": at, "character": 0 });
    let range = json!({ "start": empty, "end": empty });
    json!({
        "name": symbol::truncated(omitted, children),
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
}

/// A symbol as the wire wants it, written without a `Value` tree between.
///
/// Keys go out sorted, as a `serde_json::Value` writes them.
struct Node<'a> {
    symbol: &'a Symbol,
    line: LineNo,
    columns: &'a Columns,
}

#[derive(serde::Serialize)]
struct Position {
    character: Column,
    line: LineNo,
}

#[derive(serde::Serialize)]
struct Span {
    end: Position,
    start: Position,
}

impl Node<'_> {
    fn span(&self, range: &Range<ByteOffset>) -> Span {
        let position = |byte| Position {
            character: self.columns.at(byte),
            line: self.line,
        };
        Span {
            end: position(range.end),
            start: position(range.start),
        }
    }
}

impl Serialize for Node<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let symbol = self.symbol;
        let mut map = serializer.serialize_map(None)?;
        if !symbol.children.is_empty() {
            map.serialize_entry("children", &Children(self))?;
        }
        map.serialize_entry("kind", &symbol.kind)?;
        map.serialize_entry("name", &symbol.name)?;
        map.serialize_entry("range", &self.span(&symbol.range))?;
        map.serialize_entry("selectionRange", &self.span(&symbol.selection))?;
        map.end()
    }
}

struct Children<'a>(&'a Node<'a>);

impl Serialize for Children<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let parent = self.0;
        serializer.collect_seq(parent.symbol.children.iter().map(|symbol| Node {
            symbol,
            line: parent.line,
            columns: parent.columns,
        }))
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
    use crate::server::testing::{answered, state_with, HEARTBEAT};

    /// Two parties, the first carrying a nested sub-ID group: every shape the
    /// outline can produce, on one line.
    const PARTIES: &str = "8=FIX.4.4|35=D|453=2|448=A|447=D|452=1|802=1|523=S1|803=1\
                           |448=B|447=D|452=2|54=1";

    fn request(uri: &lsp_types::Uri) -> DocumentSymbolParams {
        serde_json::from_value(json!({ "textDocument": { "uri": uri.as_str() } }))
            .expect("valid symbol parameters")
    }

    fn outline(text: &str) -> Value {
        let (mut state, uri) = state_with(text);
        answered(state.document_symbol_task_prepared(request(&uri)))
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
        let messages = answered(state.document_symbol_task_prepared(request(&uri)));
        assert_eq!(shape(&messages).lines().count(), 1, "the message alone");

        state.config.base.document_symbol.depth = SymbolDepth::Fields;
        super::super::testing::reconfigure(&mut state);
        let fields = answered(state.document_symbol_task_prepared(request(&uri)));
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
        let legacy = shape(&answered(
            state.document_symbol_task_prepared(request(&uri)),
        ));
        assert!(legacy.starts_with("# → NewOrderSingle (D) [5]"), "{legacy}");
        assert!(legacy.contains("[1] [18]"), "an entry as Array: {legacy}");

        state.client.extended_symbol_kinds = true;
        let extended = shape(&answered(
            state.document_symbol_task_prepared(request(&uri)),
        ));
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
        answered(state.document_symbol_task_prepared(request(&uri)));
        assert!(cached(&mut state, &uri).iter().all(|seen| !seen));
    }

    /// A heartbeat is eight fields, so nine symbols with its message row.
    const PER_HEARTBEAT: usize = 9;

    fn under_cap(text: &str, cap: usize) -> Value {
        let (mut state, uri) = state_with(text);
        state.config.base.limits.document_symbol_items = cap;
        super::super::testing::reconfigure(&mut state);
        answered(state.document_symbol_task_prepared(request(&uri)))
    }

    /// The whole answer, not streamed.
    fn enumerated(wanted: &Wanted, snapshot: &Snapshot) -> Result<Value, ResponseError> {
        enumerate(
            wanted,
            snapshot,
            &AtomicBool::new(false),
            false,
            &mut |_| {},
        )
        .map(|bytes| serde_json::from_slice(&bytes).expect("a JSON answer"))
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
    fn the_last_root_gives_its_slot_to_a_child_omission_marker() {
        let answer = under_cap(&heartbeats(3), 3);
        assert_eq!(rows(&answer), 3, "two messages and a marker");
        assert_eq!(symbols(&answer), 3, "and no room left for a single field");
        assert_eq!(
            answer[2]["name"],
            "… 1 more messages; some fields/groups omitted"
        );
    }

    /// All of a message's children or none: three of sixty fields would say
    /// something false about the line.
    #[test]
    fn children_are_dropped_per_message_and_never_by_half() {
        // Room for one message's children and one item over.
        let answer = under_cap(&heartbeats(2), 2 + PER_HEARTBEAT - 1 + 1);
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
        let answer = under_cap(&heartbeats(3), 3 * PER_HEARTBEAT);
        assert_eq!(symbols(&answer), 3 * PER_HEARTBEAT, "an exact fit");
    }

    #[test]
    fn an_empty_document_is_not_something_to_report() {
        let answer = under_cap("# just a comment\n", 50);
        assert_eq!(answer, json!([]));
    }

    /// Past the cap the answer is a deterministic prefix plus one marker, and
    /// the marker counts — which is what keeps the total at the cap and not one
    /// over it.
    #[test]
    fn past_the_cap_a_prefix_and_a_marker_that_counts_itself() {
        let answer = under_cap(&heartbeats(3), 2);
        assert_eq!(symbols(&answer), 2, "one message and the marker");
        let rows = answer.as_array().expect("an array");
        assert_eq!(
            rows[1]["name"],
            "… 2 more messages; some fields/groups omitted"
        );
        assert_eq!(
            rows[1]["range"]["start"]["line"], 0,
            "anchored on the earliest omitted children"
        );
    }

    /// A cap of one leaves no room for a root at all, so the marker stands for
    /// every message. The schema refuses zero for the same reason.
    #[test]
    fn a_cap_of_one_is_the_marker_alone() {
        let answer = under_cap(&heartbeats(3), 1);
        assert_eq!(rows(&answer), 1);
        assert_eq!(answer[0]["name"], "… 3 more messages");

        let (mut state, uri) = state_with(&heartbeats(3));
        let snapshot = state.snapshot_for(&uri).expect("ready");
        let zero = enumerated(
            &Wanted {
                depth: SymbolDepth::Full,
                kinds: Kinds::for_client(false),
                max_symbols: 0,
                skip_line: None,
            },
            &snapshot,
        )
        .expect("defensive zero bound");
        assert_eq!(zero, answer, "a zero that arrives anyway is treated as one");
    }

    /// The classification pass counts messages, not lines: a comment between
    /// them must not spend a root.
    #[test]
    fn lines_that_are_not_messages_do_not_spend_the_budget() {
        let text = format!("{}\n# a comment\n{}", HEARTBEAT, HEARTBEAT);
        let answer = under_cap(&text, 2);
        assert_eq!(rows(&answer), 2, "one message and a marker");
        assert!(
            answer.as_array().expect("an array")[1]["name"] != "… 1 more messages",
            "the comment was not counted as an omitted message"
        );
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
            answered(state.document_symbol_task_prepared(request(&uri))),
            json!([])
        );
    }

    #[cfg(not(feature = "bench-no-line-cache"))]
    #[test]
    fn warmed_analyses_are_reused_without_populating_or_refreshing_marks() {
        let (mut state, uri) = state_with(&heartbeats(3));
        let snapshot = state.snapshot_for(&uri).expect("ready");
        let wanted = Wanted {
            depth: SymbolDepth::Full,
            kinds: Kinds::for_client(false),
            max_symbols: 100,
            skip_line: None,
        };
        let cold = enumerated(&wanted, &snapshot).unwrap();
        let warmed = snapshot.analysis(snapshot.line(LineNo::new(0)).unwrap());
        let document = state.docs.get(&uri).unwrap();
        assert_eq!(
            document.evict_over(0),
            0,
            "first sweep clears the existing use mark"
        );
        let warm = enumerated(&wanted, &snapshot).unwrap();
        assert_eq!(cold, warm);
        assert_eq!(cached(&mut state, &uri), vec![true, false, false]);
        assert!(std::sync::Arc::ptr_eq(
            &warmed,
            &snapshot
                .line(LineNo::new(0))
                .unwrap()
                .analysis_if_cached()
                .unwrap()
        ));
        assert_eq!(
            state.docs.get(&uri).unwrap().evict_over(0),
            1,
            "enumeration did not refresh the use mark"
        );
        assert!(cached(&mut state, &uri).iter().all(|present| !present));
        let old_analysis = snapshot.analysis(snapshot.line(LineNo::new(0)).unwrap());
        state
            .docs
            .get_mut(&uri)
            .unwrap()
            .set_ctx(crate::ctx::Ctx::for_test("FIX.4.2"));
        assert!(state.docs.get(&uri).unwrap().lines[0]
            .analysis_if_cached()
            .is_none());
        let old_view = enumerated(&wanted, &snapshot).unwrap();
        assert_eq!(
            old_view, cold,
            "old snapshot retains its own lines and context"
        );
        assert!(std::sync::Arc::ptr_eq(
            &old_analysis,
            &snapshot
                .line(LineNo::new(0))
                .unwrap()
                .analysis_if_cached()
                .unwrap()
        ));
    }

    /// The view writes keys in the order a `Value` does, so no client sees new bytes.
    #[test]
    fn the_encoded_answer_matches_a_value_byte_for_byte() {
        let (mut state, uri) = state_with(PARTIES);
        let snapshot = state.snapshot_for(&uri).expect("ready");
        let wanted = Wanted {
            depth: SymbolDepth::Full,
            kinds: Kinds::for_client(true),
            max_symbols: 100,
            skip_line: None,
        };
        let encoded = enumerate(
            &wanted,
            &snapshot,
            &AtomicBool::new(false),
            false,
            &mut |_| {},
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&encoded).unwrap();
        let fields = value[0]["children"].as_array().expect("fields");
        assert!(
            fields.iter().any(|field| field["children"].is_array()),
            "nested"
        );
        assert_eq!(encoded, serde_json::to_vec(&value).unwrap());
    }

    #[test]
    fn an_unrenderable_final_candidate_frees_its_slot_without_retraction() {
        let (mut state, uri) = state_with(&heartbeats(3));
        let snapshot = state.snapshot_for(&uri).expect("ready");
        let wanted = Wanted {
            depth: SymbolDepth::Messages,
            kinds: Kinds::for_client(false),
            max_symbols: 3,
            skip_line: Some(LineNo::new(2)),
        };
        let mut chunks = Vec::new();
        let final_result = enumerate(
            &wanted,
            &snapshot,
            &AtomicBool::new(false),
            true,
            &mut |event| {
                if let BackgroundEvent::Chunk(bytes) = event {
                    chunks.extend(serde_json::from_slice::<Vec<Value>>(&bytes).unwrap());
                }
            },
        )
        .unwrap();
        assert_eq!(final_result, b"[]");
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0]["range"]["start"]["line"], 0);
        assert_eq!(chunks[1]["range"]["start"]["line"], 1);
        assert_eq!(chunks[2]["name"], "… 1 more messages");
        assert_eq!(chunks[2]["range"]["start"]["line"], 2);
    }
}
