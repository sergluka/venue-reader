//! Byte-level FIX decoding: tokenizer, decoder, integrity facts.
//!
//! Config-free by design — everything here takes bytes and a `Dict`. The glue
//! that picks the dictionary for a line lives in [`crate::ctx`].

pub mod decode;
pub mod integrity;
mod packed;
pub mod tokenize;

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

pub use decode::{decode, decode_non_message, decode_tokenized};
pub use integrity::Integrity;

pub use tokenize::{tokenize, FieldSpan, LineKind, LineTokens};

use crate::dictionary::{Dict, EnumIdx, FieldIdx, GroupIdx, MsgIdx};
use packed::Packed16;

/// `GroupInstance::parent` is wider than `Field::group` only because it never
/// shares the struct's byte budget; the "none" case is the same idea.
pub(crate) const NO_GROUP: u32 = u32::MAX;

/// A byte offset within one line, measured from the first byte of that line.
///
/// **It includes any log prefix** the tokenizer resynchronised past, because it
/// counts the line's own bytes rather than the FIX message's.
///
/// This distinguishes a *unit*, not an identity: nothing here stops an offset
/// taken on one line from being used against another, and nothing marks it as
/// belonging to a particular document. It exists so that the three quantities
/// that are all `u32` — this, a line number and a column in the negotiated
/// encoding — stop being interchangeable in a signature.
///
/// `#[repr(transparent)]` because `Field` is asserted to 20 bytes below; the
/// `repr` guarantees this wrapper lays out as its `u32`, and the assertion is
/// what guarantees `Field`.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize)]
pub struct ByteOffset(u32);

impl ByteOffset {
    pub fn new(byte: u32) -> ByteOffset {
        ByteOffset(byte)
    }

    pub fn raw(self) -> u32 {
        self.0
    }

    /// For slicing. The one place a byte offset becomes a `usize`.
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// From a length or an offset the platform sizes — `None` when it does not
    /// fit, which callers answer rather than assert.
    ///
    /// The one *fallible* narrowing — `tokenize::byte` truncates, and says so.
    /// This one exists because the editing path in `document` keeps `usize` end
    /// to end: a wrapped offset there would apply an edit at the wrong place,
    /// and a `debug_assert` would make that a release-only bug. Where it is
    /// reached — hover, on a line longer than 4 GiB — saying nothing beats
    /// pointing somewhere arbitrary.
    pub fn from_len(len: usize) -> Option<ByteOffset> {
        u32::try_from(len).ok().map(ByteOffset)
    }
}

/// A byte offset, short. Tests about spans are full of them, and five copies of
/// this had accumulated across four modules before it was hoisted here.
#[cfg(test)]
pub(crate) fn byte(at: u32) -> ByteOffset {
    ByteOffset::new(at)
}

impl From<ByteOffset> for u64 {
    fn from(byte: ByteOffset) -> u64 {
        u64::from(byte.0)
    }
}

/// An offset plus a length is an offset. The length stays a bare `u32`: a byte
/// count is not a position, and giving it a type of its own would buy a second
/// arithmetic surface for nothing.
impl std::ops::Add<u32> for ByteOffset {
    type Output = ByteOffset;

    fn add(self, len: u32) -> ByteOffset {
        ByteOffset(self.0 + len)
    }
}

/// And the difference of two offsets is a length, which is why this does not
/// return `Self`.
impl std::ops::Sub for ByteOffset {
    type Output = u32;

    fn sub(self, from: ByteOffset) -> u32 {
        self.0 - from.0
    }
}

/// One decoded field, packed to 20 bytes.
///
/// Only `tag_start` is stored absolutely, because only it is searched; the
/// other offsets are derived, which is sound because the tokenizer caps a digit
/// run at 255 and puts the value immediately after `=`.
///
/// Named `Field` without a qualifier because in this domain a `tag=value` pair
/// on a line *is* the field — `FieldDef`/`FieldView`/`FieldIdx` all describe a
/// field rather than being one. Modules disambiguate: `codec::Field` against
/// `dictionary::FieldView`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub tag_start: ByteOffset,
    /// `u32::MAX` when the digit run overflowed.
    pub tag: u32,
    /// Never clamped: `integrity` derives BodyLength and CheckSum from it, and
    /// a truncated length would make a repair rewrite the log with a wrong one.
    value_len: u32,
    def: Packed16,
    group: Packed16,
    enum_idx: Packed16,
    tag_len: u8,
    // One byte of tail padding is left over. Deliberate: a genuine flag can be
    // added later for free, and nothing has to be repacked to make room.
}

const _: () = assert!(std::mem::size_of::<Field>() == 20);

// There is no `flags` byte, and that is a decision rather than an omission.
// A "tag overflowed" flag would restate what `tag == u32::MAX` already says, and
// two encodings of one fact means half the callers check the wrong one. A "value
// is not UTF-8" flag is cheaper to recompute than to store: "no enum resolved"
// is already `enum_value() == None`, and the consumers that need "these bytes
// are not text" hold the line bytes anyway.
//
// The only remaining lever down to 16 bytes is dropping `tag` and re-reading the
// digits from the line. Not done: `tag` is the most-read member, a `Field` that
// cannot answer "which tag?" without the line is a poor abstraction, and the
// memory question is settled by cache policy rather than by four more bytes.

impl Field {
    fn new(span: &FieldSpan) -> Field {
        // The narrow members are only sound because of two tokenizer rules;
        // keep the pair checkable so a change to either fails loudly here.
        debug_assert!(span.tag_end - span.tag_start <= 255, "digit run over 255");
        debug_assert_eq!(span.value_start, span.tag_end + 1, "whitespace around `=`");
        Field {
            tag_start: span.tag_start,
            tag: span.tag,
            value_len: span.value_end - span.value_start,
            def: Packed16::NONE,
            group: Packed16::NONE,
            enum_idx: Packed16::NONE,
            tag_len: (span.tag_end - span.tag_start) as u8,
        }
    }

    fn set_def(&mut self, def: FieldIdx) {
        self.def = Packed16::pack(def.raw());
    }

    fn set_enum(&mut self, at: EnumIdx) {
        self.enum_idx = Packed16::pack(u32::from(at.raw()));
    }

    fn set_group(&mut self, instance: u32) {
        self.group = Packed16::pack(instance);
    }

    pub fn tag_end(&self) -> ByteOffset {
        self.tag_start + u32::from(self.tag_len)
    }

    pub fn value_start(&self) -> ByteOffset {
        self.tag_end() + 1
    }

    pub fn value_end(&self) -> ByteOffset {
        self.value_start() + self.value_len
    }

    /// The whole field, `tag=value`, in bytes. What "this field" means to a
    /// cursor, to a highlight range, and to the checksum arithmetic — one
    /// definition rather than `tag_start..value_end()` written out at each.
    pub fn span(&self) -> Range<ByteOffset> {
        self.tag_start..self.value_end()
    }

    /// `None` when the tag is not defined in this line's dictionary.
    pub fn def(&self) -> Option<FieldIdx> {
        self.def.get().map(FieldIdx::from_raw)
    }

    /// `None` when the value matches no enum of that field.
    pub fn enum_value(&self) -> Option<EnumIdx> {
        self.enum_idx.get().map(|at| EnumIdx::from_raw(at as u16))
    }

    /// Index into `MessageAnalysis.groups` of the innermost instance containing
    /// this field. A group's count field is not inside its own group — it
    /// carries the parent's index, if any.
    pub fn group_idx(&self) -> Option<u32> {
        self.group.get()
    }

    /// The tag as written. `tag` is the parsed number and reads `4294967295`
    /// when the digit run overflowed; these are the bytes either way.
    pub fn tag_digits<'a>(&self, line: &'a [u8]) -> &'a [u8] {
        &line[self.tag_start.index()..self.tag_end().index()]
    }

    pub fn value<'a>(&self, line: &'a [u8]) -> &'a [u8] {
        &line[self.value_start().index()..self.value_end().index()]
    }

    /// The value as text, `None` when it is not UTF-8 — a FIX line may carry
    /// raw bytes. One place decides what "the value as text" means.
    pub fn text<'a>(&self, line: &'a [u8]) -> Option<&'a str> {
        std::str::from_utf8(self.value(line)).ok()
    }
}

/// One entry of one repeating group. The ancestor chain is walked through
/// `parent`, so a field stores a single index instead of a per-field list.
///
/// **Instances are pushed parent-before-child**, so an instance's `parent` is
/// always a lower index than its own. [`MessageAnalysis::group_spans`] rests
/// on that: it merges children into parents in one reverse pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupInstance {
    pub group: GroupIdx,
    /// 1-based entry number within the group.
    pub entry: u32,
    /// Index into `MessageAnalysis.fields` of the count field.
    pub count_field_idx: u32,
    /// Index into `MessageAnalysis.groups`, or [`NO_GROUP`].
    parent_idx: u32,
    /// 1 for a top-level group.
    pub depth: u16,
}

impl GroupInstance {
    /// Index of the instance this one sits inside, always lower than its own.
    pub fn parent_idx(&self) -> Option<u32> {
        (self.parent_idx != NO_GROUP).then_some(self.parent_idx)
    }
}

/// Byte spans of one message's repeating groups — see
/// [`MessageAnalysis::group_spans`], which is the only thing that builds one.
pub struct GroupSpans {
    /// Parallel to `MessageAnalysis.groups`. `None` only for an instance no
    /// field was marked with, which the annotator does not produce.
    instances: Vec<Option<Range<ByteOffset>>>,
    /// Keyed by count field: its span, and the definition it opens. The
    /// definition is kept here because it is in hand while the table is built,
    /// and a consumer that looks it up by scanning the instances turns a
    /// bounded pass into a quadratic one.
    groups: BTreeMap<u32, (Range<ByteOffset>, GroupIdx)>,
}

impl GroupSpans {
    /// What one entry covers, count fields of nested groups included.
    pub fn instance(&self, index: u32) -> Option<Range<ByteOffset>> {
        self.instances.get(index as usize)?.clone()
    }

    /// What every entry of one group occurrence covers, its count tag included.
    ///
    /// Addressed by the count field's index, not by `GroupIdx`. `None` when
    /// that field opened no entries — a zero, negative or unparseable count.
    pub fn group(&self, count_field_idx: u32) -> Option<Range<ByteOffset>> {
        self.groups
            .get(&count_field_idx)
            .map(|(span, _)| span.clone())
    }

    /// The definition a count field opens.
    pub fn group_def(&self, count_field_idx: u32) -> Option<GroupIdx> {
        self.groups.get(&count_field_idx).map(|(_, of)| *of)
    }
}

fn widen(slot: &mut Option<Range<ByteOffset>>, span: Range<ByteOffset>) {
    match slot {
        Some(have) => widen_range(have, span),
        None => *slot = Some(span),
    }
}

fn widen_range(have: &mut Range<ByteOffset>, span: Range<ByteOffset>) {
    have.start = have.start.min(span.start);
    have.end = have.end.max(span.end);
}

/// What one line turned into.
///
/// An enum rather than a struct with a `kind` tag, so that "a comment has no
/// fields" and "a message always has a dictionary" are unrepresentable rather
/// than upheld by convention. Not for the memory: the enum is sized by its
/// largest variant either way (120 bytes, down from 128 only because the tag
/// and three `Option` discriminants went). What a line that is not a message
/// stops paying is the refcount traffic — no `Dict` and no version `Arc` to
/// clone and later drop. Cached per line in `document::Line`.
pub enum LineAnalysis {
    /// Empty or whitespace-only.
    Empty,
    /// First non-blank byte is `#`; a `fix:` modeline is one of these.
    Comment,
    /// Non-blank, but no `tag=value` pair could be read.
    Other,
    Message(MessageAnalysis),
}

/// A decoded FIX line: its fields with their dictionary indices, its
/// repeating-group instances, and its integrity facts.
pub struct MessageAnalysis {
    /// Bytes of log prefix skipped before the first field. Spans are absolute,
    /// so this is informational.
    pub prefix_len: ByteOffset,
    /// The dictionary this line was decoded with, and the version string that
    /// selected it — `FIXT.1.1` resolves to the `FIX.5.0SP2` tables but the
    /// line's version is still `FIXT.1.1`.
    pub dict: Dict,
    pub version: Arc<str>,
    pub fields: Vec<Field>,
    pub groups: Vec<GroupInstance>,
    /// Resolved from the first MsgType(35). `None` when the line carries none — a
    /// bare repeating-group block is still a message line — or when the
    /// dictionary does not know the type.
    pub msg_type: Option<MsgIdx>,
}

// `Line.analysis` is an `OnceLock<Arc<LineAnalysis>>` inside an `Arc<Line>` shared
// with workers; make a future `Rc`/`Cell` a compile error, not a later surprise.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<LineAnalysis>();
};

impl LineAnalysis {
    /// The decoded payload, `None` for a line that is not a message.
    pub fn message(&self) -> Option<&MessageAnalysis> {
        match self {
            LineAnalysis::Message(message) => Some(message),
            _ => None,
        }
    }

    pub fn kind(&self) -> LineKind {
        match self {
            LineAnalysis::Empty => LineKind::Empty,
            LineAnalysis::Comment => LineKind::Comment,
            LineAnalysis::Other => LineKind::Other,
            LineAnalysis::Message(_) => LineKind::Message,
        }
    }

    /// Bytes owned on the heap, for the bench's retained-memory model.
    pub fn heap_size(&self) -> usize {
        match self {
            LineAnalysis::Message(message) => message.heap_size(),
            _ => 0,
        }
    }
}

impl MessageAnalysis {
    /// First field with this tag, in wire order — what the summary and hover use
    /// for a repeated tag.
    pub fn first(&self, tag: u32) -> Option<&Field> {
        self.fields.iter().find(|field| field.tag == tag)
    }

    /// Field containing this byte offset, spanning the `=` deliberately. This,
    /// not `first`, is how hover addresses a repeated tag.
    pub fn field_at(&self, byte: ByteOffset) -> Option<&Field> {
        Some(&self.fields[self.field_idx_at(byte)?])
    }

    /// The same lookup, for a caller that needs the *index* — because a group
    /// occurrence is addressed by its count field's index, so "which field is
    /// this?" and "what does it open?" are one question.
    pub fn field_idx_at(&self, byte: ByteOffset) -> Option<usize> {
        self.fields
            .iter()
            .position(|field| field.span().contains(&byte))
    }

    /// PossDupFlag(43) is `Y`: the line repeats a message that was sent before.
    pub fn is_poss_dup(&self, line: &[u8]) -> bool {
        self.first(43).and_then(|field| field.text(line)) == Some("Y")
    }

    /// Byte spans of this message's repeating groups.
    ///
    /// Derived on request rather than stored on `GroupInstance`: three
    /// request-time consumers want it — the symbol tree, expand-selection and
    /// semantic tokens — and none is worth widening what a cached
    /// `LineAnalysis` retains. Item 23; decision record `group-span-home.md`.
    ///
    /// One forward pass over `fields`, because `Annotator` marks every field
    /// with its innermost instance and an entry's fields are contiguous; then
    /// one reverse pass over `groups`, because instances are pushed
    /// parent-before-child.
    pub fn group_spans(&self) -> GroupSpans {
        let mut instances: Vec<Option<Range<ByteOffset>>> = vec![None; self.groups.len()];
        for field in &self.fields {
            if let Some(at) = field.group_idx() {
                widen(&mut instances[at as usize], field.span());
            }
        }
        for at in (0..self.groups.len()).rev() {
            if let (Some(span), Some(parent)) =
                (instances[at].clone(), self.groups[at].parent_idx())
            {
                widen(&mut instances[parent as usize], span);
            }
        }

        // Keyed by the count field rather than by `GroupIdx`: a `GroupIdx` is a
        // *definition*, and one message may use the same definition twice.
        // The count field is deliberately outside its own group, so a group's
        // span is its instances *plus* the count field's own span — a user
        // pointing at `453=3` means the group.
        let mut groups: BTreeMap<u32, (Range<ByteOffset>, GroupIdx)> = BTreeMap::new();
        for (at, instance) in self.groups.iter().enumerate() {
            let Some(span) = instances[at].clone() else {
                continue;
            };
            let count = instance.count_field_idx;
            let slot = groups
                .entry(count)
                .or_insert_with(|| (self.fields[count as usize].span(), instance.group));
            widen_range(&mut slot.0, span);
        }
        GroupSpans { instances, groups }
    }

    /// Ancestor instances of a field, outermost first. Allocates; render-time
    /// only.
    pub fn ancestors(&self, field: &Field) -> Vec<&GroupInstance> {
        let mut chain = Vec::new();
        let mut at = field.group_idx();
        while let Some(index) = at {
            let instance = &self.groups[index as usize];
            chain.push(instance);
            at = instance.parent_idx();
        }
        chain.reverse();
        chain
    }

    /// Dictionary name of a field's tag, `None` when the tag is not defined in
    /// this line's version. Resolving through the `Dict` the line was decoded
    /// with is the only correct way: an index means nothing against another.
    pub fn field_name(&self, field: &Field) -> Option<&str> {
        Some(self.dict.field(field.def()?).name())
    }

    /// Enum label for a field's value, `None` when it matches none.
    pub fn enum_label(&self, field: &Field) -> Option<&str> {
        Some(self.dict.enum_at(field.def()?, field.enum_value()?).name())
    }

    /// Enum label of a tag's first occurrence — what a renderer wants when it
    /// names a tag rather than holding a field.
    pub fn label(&self, tag: u32) -> Option<&str> {
        self.enum_label(self.first(tag)?)
    }

    /// `NoMDEntries/2/MDEntryPx`; `None` outside a group. The leaf falls back to
    /// the numeric tag when the field is unknown.
    /// `line` is only read for the leaf of an undefined tag, and only so the
    /// digits are the ones on the line: `field.tag` is `u32::MAX` for a run
    /// that overflowed, which would put `4294967295` in a path beside the
    /// digits themselves wherever both are rendered.
    pub fn group_path(&self, field: &Field, line: &[u8]) -> Option<String> {
        let dict = &self.dict;
        let chain = self.ancestors(field);
        if chain.is_empty() {
            return None;
        }
        let mut path = String::new();
        for instance in chain {
            path.push_str(dict.group(instance.group).name());
            path.push('/');
            path.push_str(&instance.entry.to_string());
            path.push('/');
        }
        match self.field_name(field) {
            Some(name) => path.push_str(name),
            None => path.push_str(&String::from_utf8_lossy(field.tag_digits(line))),
        }
        Some(path)
    }

    /// BodyLength and CheckSum as they should be, recomputed from `line`.
    ///
    /// Not stored: it costs 13% of decoding and 32 bytes a line, and the only
    /// consumers are the diagnostic rules and their repairs. The diagnostics
    /// walk does not read cached analyses anyway — it calls `analyze` and drops
    /// the result — so nothing recomputes that would not have computed, while
    /// hover, inlay hints and semantic tokens stop paying for a fact they never
    /// look at.
    pub fn integrity(&self, line: &[u8]) -> Integrity {
        integrity::compute(line, &self.fields)
    }

    fn heap_size(&self) -> usize {
        self.fields.capacity() * std::mem::size_of::<Field>()
            + self.groups.capacity() * std::mem::size_of::<GroupInstance>()
    }
}

#[cfg(test)]
mod byte_offset_tests {
    use super::ByteOffset;

    /// The one narrowing in the crate, at its boundary. Asserted rather than
    /// argued because the failure it prevents — a wrapped offset applying an
    /// edit somewhere arbitrary — is invisible in a release build, which is
    /// exactly where wrapping happens.
    #[test]
    fn the_narrowing_answers_rather_than_wraps() {
        assert_eq!(
            ByteOffset::from_len(u32::MAX as usize),
            Some(ByteOffset::new(u32::MAX))
        );
        assert_eq!(ByteOffset::from_len(0), Some(ByteOffset::new(0)));
        // One past, which only exists on a 64-bit target. `None`, not a
        // truncation, and not a debug-only panic.
        if usize::BITS > 32 {
            assert_eq!(ByteOffset::from_len(u32::MAX as usize + 1), None);
            assert_eq!(ByteOffset::from_len(usize::MAX), None);
        }
    }
}
