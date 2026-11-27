//! `Position` ↔ byte-offset conversion under the negotiated encoding.
//!
//! Policy: `Utf8` ⇒ `character` is a byte offset — the only encoding under which
//! invalid-byte lines are addressed at all. `Utf16` ⇒ defined over valid UTF-8
//! lines only, which is what a client negotiating it sends; an invalid line
//! falls back to byte addressing. ASCII lines convert as identity.

use std::sync::OnceLock;

use super::Line;
use crate::codec::ByteOffset;

/// A column in the encoding *this snapshot* negotiated — the `character` half
/// of an LSP `Position`.
///
/// Like [`ByteOffset`], it distinguishes a *unit* and not an identity: nothing
/// stops a column measured under `Utf8` from being compared against one
/// measured under `Utf16`. What it does stop is a byte offset being handed to a
/// client as a column, which is the mistake this crate has actually made.
///
/// The only arithmetic is subtraction, and it yields a plain `u32` rather than
/// a column: a distance between two columns is a length, which is what the
/// protocol asks for in a token and in a range. Nothing adds to a column —
/// a token is a start and a length, so a second column is always converted
/// rather than computed.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Column(u32);

impl Column {
    pub fn new(character: u32) -> Column {
        Column(character)
    }

    pub fn fits_lsp(self) -> bool {
        self.0 <= i32::MAX as u32
    }

    pub fn wire_value(self) -> u32 {
        self.0.min(i32::MAX as u32)
    }
}

impl serde::Serialize for Column {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.wire_value())
    }
}

/// Saturating, because a caller that subtracts the wrong way round wants a
/// zero-length token rather than a panic in a request handler.
impl std::ops::Sub for Column {
    type Output = u32;

    fn sub(self, earlier: Column) -> u32 {
        self.0.saturating_sub(earlier.0)
    }
}

/// Both variants exist because the encoding is *negotiated*, not chosen by us:
/// `utf-16` is the protocol's historical default and the only encoding some
/// clients speak, so it cannot be dropped; `utf-8` is what we prefer — line
/// bytes are our source of truth, so the conversion becomes the identity, and
/// on a line holding invalid UTF-8 it is the only addressing that is well
/// defined at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
}

#[cfg(test)]
pub(crate) mod work {
    use std::cell::Cell;
    thread_local! {
        pub static PREFIX_BYTES: Cell<usize> = const { Cell::new(0) };
        pub static DECODED: Cell<usize> = const { Cell::new(0) };
        pub static VALIDATED: Cell<usize> = const { Cell::new(0) };
    }
}

pub(super) const INDEX_MIN_BYTES: usize = 16 * 1024;
const CHECKPOINT_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Default)]
struct Checkpoint {
    byte: usize,
    units: usize,
}

#[derive(Clone, Copy)]
enum FromUnit {
    Byte,
    Utf16,
}

impl FromUnit {
    fn source(self, point: Checkpoint) -> usize {
        match self {
            Self::Byte => point.byte,
            Self::Utf16 => point.units,
        }
    }

    fn result(self, point: Checkpoint) -> usize {
        match self {
            Self::Byte => point.units,
            Self::Utf16 => point.byte,
        }
    }
}

/// A short lookup can validate the text without building the index used for distant positions.
#[derive(Default)]
pub(super) struct Utf16Index {
    prefix_valid: OnceLock<bool>,
    points: OnceLock<Option<Box<[Checkpoint]>>>,
}

fn checked_text(bytes: &[u8]) -> Option<&str> {
    #[cfg(test)]
    work::VALIDATED.set(work::VALIDATED.get() + bytes.len());
    std::str::from_utf8(bytes).ok()
}

fn direct(line: &Line, encoding: PositionEncoding, last: usize) -> bool {
    // Byte and UTF-16 coordinates coincide throughout an ASCII prefix, even if its tail is invalid.
    if encoding == PositionEncoding::Utf8 || line.is_ascii {
        return true;
    }
    if last > CHECKPOINT_BYTES {
        return false;
    }
    let prefix = &line.bytes[..last.min(line.bytes.len())];
    #[cfg(test)]
    work::PREFIX_BYTES.set(work::PREFIX_BYTES.get() + prefix.len());
    prefix.is_ascii()
}

impl Utf16Index {
    fn checkpoints(bytes: &[u8]) -> Option<Box<[Checkpoint]>> {
        let text = checked_text(bytes)?;
        let mut points = Vec::with_capacity(text.len() / CHECKPOINT_BYTES + 2);
        let mut last_byte = 0;
        let mut units = 0;
        points.push(Checkpoint::default());
        for (byte, ch) in text.char_indices() {
            #[cfg(test)]
            work::DECODED.set(work::DECODED.get() + 1);
            if byte - last_byte >= CHECKPOINT_BYTES {
                points.push(Checkpoint { byte, units });
                last_byte = byte;
            }
            units += ch.len_utf16();
        }
        points.push(Checkpoint {
            byte: bytes.len(),
            units,
        });
        Some(points.into_boxed_slice())
    }

    fn convert(
        &self,
        bytes: &[u8],
        input: impl DoubleEndedIterator<Item = usize> + Clone,
        from: FromUnit,
        mut emit: impl FnMut(usize),
    ) {
        let Some(last) = input.clone().next_back() else {
            return;
        };
        if last <= CHECKPOINT_BYTES && self.points.get().is_none() {
            // A bad byte anywhere in the line switches *every* coordinate to byte addressing.
            if !self
                .prefix_valid
                .get_or_init(|| checked_text(bytes).is_some())
            {
                input.for_each(|value| emit(value.min(bytes.len())));
                return;
            }
            // Three UTF-8 bytes per UTF-16 unit also cover surrogate pairs (four bytes for two).
            let mut end = bytes.len().min(last * 3 + 3);
            while end < bytes.len() && bytes[end] & 0xc0 == 0x80 {
                end -= 1;
            }
            walk_chunk(&bytes[..end], Checkpoint::default(), input, from, &mut emit);
            return;
        }

        let Some(points) = self.points.get_or_init(|| Self::checkpoints(bytes)) else {
            input.for_each(|value| emit(value.min(bytes.len())));
            return;
        };
        let mut wanted = input.peekable();
        while let Some(&first) = wanted.peek() {
            let index = points.partition_point(|point| from.source(*point) <= first) - 1;
            let position = points[index];
            let Some(end) = points.get(index + 1) else {
                wanted.for_each(|_| emit(from.result(position)));
                break;
            };
            let in_chunk =
                std::iter::from_fn(|| wanted.next_if(|value| *value <= from.source(*end)));
            walk_chunk(
                &bytes[position.byte..end.byte],
                position,
                in_chunk,
                from,
                &mut emit,
            );
        }
    }
}

fn walk_chunk(
    bytes: &[u8],
    mut position: Checkpoint,
    input: impl Iterator<Item = usize>,
    from: FromUnit,
    emit: &mut impl FnMut(usize),
) {
    let mut chars = checked_text(bytes).expect("validated UTF-8").chars();
    for value in input {
        while from.source(position) < value {
            let Some(ch) = chars.next() else { break };
            #[cfg(test)]
            work::DECODED.set(work::DECODED.get() + 1);
            position.byte += ch.len_utf8();
            position.units += ch.len_utf16();
        }
        emit(from.result(position));
    }
}

/// Byte offset within `line` for the given `character`, clamped to the line end.
///
/// Returns `usize`, deliberately. `apply_ranged` slices with this result, so
/// narrowing it to a [`ByteOffset`] would put a wrapped offset — an edit
/// applied in the wrong place — on the editing path. A caller that needs the
/// narrow type asks for it explicitly through [`ByteOffset::from_len`] and
/// handles the `None`.
pub fn position_to_byte(line: &Line, character: Column, enc: PositionEncoding) -> usize {
    let ch = character.0 as usize;
    if direct(line, enc, ch) {
        return ch.min(line.bytes.len());
    }
    if let Some(index) = &line.utf16_index {
        let mut byte = 0;
        index.convert(&line.bytes, std::iter::once(ch), FromUnit::Utf16, |value| {
            byte = value
        });
        return byte;
    }
    let Some(s) = utf16_addressable(line) else {
        return ch.min(line.bytes.len());
    };
    let mut units = 0usize;
    for (byte_idx, c) in s.char_indices() {
        if units >= ch {
            return byte_idx;
        }
        units += c.len_utf16();
    }
    line.bytes.len()
}

/// The `character` (in the given encoding) for a byte offset within `line`.
pub fn byte_to_position(line: &Line, byte: ByteOffset, enc: PositionEncoding) -> Column {
    let byte = byte.index().min(line.bytes.len());
    if direct(line, enc, byte) {
        return Column(byte as u32);
    }
    if let Some(index) = &line.utf16_index {
        let mut column = 0;
        index.convert(
            &line.bytes,
            std::iter::once(byte),
            FromUnit::Byte,
            |value| column = value as u32,
        );
        return Column(column);
    }
    let Some(s) = utf16_addressable(line) else {
        return Column(byte as u32);
    };
    let mut units = 0u32;
    for (byte_idx, c) in s.char_indices() {
        if byte_idx >= byte {
            return Column(units);
        }
        units += c.len_utf16() as u32;
    }
    Column(units)
}

/// One line's byte offsets, converted together and then asked about by offset.
///
/// The conversion is O(line) per offset, so a caller with several offsets on one
/// line — a line's diagnostics, all of its inlay hints — must convert them in a
/// single ascending walk. That is easy to write and easy to write *wrongly*:
/// the offsets a caller has are rarely sorted (a hint's tag and value ends
/// interleave, two diagnostics on a line can nest) and are often duplicated.
/// So the sorting, the deduplication and the walk live here, once, rather than
/// being re-established at each call site.
pub struct Columns {
    offsets: Vec<ByteOffset>,
    columns: Vec<Column>,
}

impl Columns {
    /// One walk, whatever order the offsets arrive in.
    pub fn of(
        line: &Line,
        offsets: impl IntoIterator<Item = ByteOffset>,
        enc: PositionEncoding,
    ) -> Columns {
        let mut offsets: Vec<ByteOffset> = offsets.into_iter().collect();
        offsets.sort_unstable();
        offsets.dedup();
        let columns = byte_to_positions(line, &offsets, enc);
        Columns { offsets, columns }
    }

    /// The column for an offset this was built with.
    pub fn at(&self, byte: ByteOffset) -> Column {
        let at = self
            .offsets
            .binary_search(&byte)
            .expect("an offset this was built with");
        self.columns[at]
    }
}

/// One line's columns, converted together and then asked about by column —
/// [`Columns`] the other way round.
///
/// The mirror exists for the same reason: [`position_to_byte`] scans from the
/// start of the line per call, so a caller with several cursors on one line
/// pays O(line x cursors) unless the sorting, the deduplication and the walk
/// live here. `selectionRange` is that caller — LSP bounds neither the length
/// nor the spread of its `positions`.
pub struct Offsets {
    columns: Vec<Column>,
    bytes: Vec<usize>,
}

impl Offsets {
    /// One walk, whatever order the columns arrive in.
    pub fn of(
        line: &Line,
        columns: impl IntoIterator<Item = Column>,
        enc: PositionEncoding,
    ) -> Offsets {
        let mut columns: Vec<Column> = columns.into_iter().collect();
        columns.sort_unstable();
        columns.dedup();
        let bytes = positions_to_bytes(line, &columns, enc);
        Offsets { columns, bytes }
    }

    /// The byte offset for a column this was built with.
    ///
    /// `usize` for the reason [`position_to_byte`] returns one: a caller that
    /// wants the narrow type asks through [`ByteOffset::from_len`].
    pub fn at(&self, column: Column) -> usize {
        let at = self
            .columns
            .binary_search(&column)
            .expect("a column this was built with");
        self.bytes[at]
    }
}

/// Ascending columns in one walk — the inverse of [`byte_to_positions`], and
/// private for the same reason: [`Offsets`] owns the contract it needs.
fn positions_to_bytes(line: &Line, columns: &[Column], enc: PositionEncoding) -> Vec<usize> {
    debug_assert!(columns.is_sorted(), "columns must ascend: {columns:?}");
    let Some(last) = columns.last() else {
        return Vec::new();
    };
    let clamped = |column: &Column| (column.0 as usize).min(line.bytes.len());
    if direct(line, enc, last.0 as usize) {
        return columns.iter().map(clamped).collect();
    }
    if let Some(index) = &line.utf16_index {
        let mut result = Vec::with_capacity(columns.len());
        index.convert(
            &line.bytes,
            columns.iter().map(|column| column.0 as usize),
            FromUnit::Utf16,
            |byte| result.push(byte),
        );
        return result;
    }
    let Some(s) = utf16_addressable(line) else {
        return columns.iter().map(clamped).collect();
    };

    let mut out = Vec::with_capacity(columns.len());
    let mut wanted = columns.iter().map(|column| column.0 as usize).peekable();
    let mut units = 0usize;
    for (byte_idx, c) in s.char_indices() {
        // A `while`, not an `if`: several columns can land on one character
        // (a surrogate pair is two units), and each answers with its start.
        while wanted.peek().is_some_and(|&ch| units >= ch) {
            wanted.next();
            out.push(byte_idx);
        }
        if wanted.peek().is_none() {
            return out;
        }
        units += c.len_utf16();
    }
    // Whatever is left addresses past the end of the line.
    out.extend(wanted.map(|_| line.bytes.len()));
    out
}

/// Ascending byte offsets in one walk.
///
/// Private: [`Columns`] is the way in, because it owns the sorting and
/// deduplication this requires. A caller that reached past it would have to
/// re-establish that contract, which is what `code_action` used to do.
///
/// [`byte_to_position`] scans from the start of the line for every call, so a
/// caller with several offsets on one line — a line's diagnostics, or its inlay
/// hints — otherwise pays O(line x offsets). Offsets must be sorted; that is
/// asserted in debug rather than silently answered with the wrong columns.
fn byte_to_positions(line: &Line, bytes: &[ByteOffset], enc: PositionEncoding) -> Vec<Column> {
    debug_assert!(bytes.is_sorted(), "offsets must ascend: {bytes:?}");
    let Some(last) = bytes.last() else {
        return Vec::new();
    };
    if direct(line, enc, last.index()) {
        return bytes
            .iter()
            .map(|byte| Column(byte.index().min(line.bytes.len()) as u32))
            .collect();
    }
    if let Some(index) = &line.utf16_index {
        let mut result = Vec::with_capacity(bytes.len());
        index.convert(
            &line.bytes,
            bytes.iter().map(|byte| byte.index()),
            FromUnit::Byte,
            |units| result.push(Column(units as u32)),
        );
        return result;
    }
    let Some(s) = utf16_addressable(line) else {
        return bytes
            .iter()
            .map(|byte| Column(byte.index().min(line.bytes.len()) as u32))
            .collect();
    };

    let mut out = Vec::with_capacity(bytes.len());
    let mut wanted = bytes
        .iter()
        .map(|byte| byte.index().min(line.bytes.len()))
        .peekable();
    let mut units = 0u32;
    for (byte_idx, c) in s.char_indices() {
        // A `while`, not an `if`: several offsets can fall inside one character,
        // and each answers with the position of the character containing it.
        while wanted.peek().is_some_and(|&byte| byte <= byte_idx) {
            wanted.next();
            out.push(Column(units));
        }
        if wanted.peek().is_none() {
            return out;
        }
        units += c.len_utf16() as u32;
    }
    // Whatever is left addresses the end of the line.
    out.extend(wanted.map(|_| Column(units)));
    out
}

/// UTF-16 offsets are only defined over a valid UTF-8 line; a line holding raw
/// bytes falls back to byte addressing (such lines only reach us from clients
/// that negotiated utf-8 in the first place).
fn utf16_addressable(line: &Line) -> Option<&str> {
    std::str::from_utf8(&line.bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::document::{Ending, Line};

    fn line(text: &str) -> std::sync::Arc<Line> {
        Line::new(text.as_bytes(), Ending::None)
    }

    use crate::codec::byte;

    /// A column, short — the byte-offset half is `crate::codec::byte`.
    fn column(at: u32) -> Column {
        Column::new(at)
    }

    #[test]
    fn early_utf16_edits_touch_only_the_ascii_prefix_on_each_version() {
        use crate::ctx::Ctx;
        use crate::document::{Change, Document};
        use lsp_types::{Position, Range};

        let text = format!("{}é", "x".repeat(4 * INDEX_MIN_BYTES));
        let mut document = Document::open(text.as_bytes(), 1, Ctx::for_test("FIX.4.4"));
        let encoding = PositionEncoding::Utf16;
        for version in 2..8 {
            work::DECODED.set(0);
            work::VALIDATED.set(0);
            work::PREFIX_BYTES.set(0);
            let range = Range::new(Position::new(0, 12), Position::new(0, 13));
            let edited = Change::Ranged {
                range,
                text: vec![b'A' + version as u8],
            };
            document
                .apply_changes(&[edited], version, encoding)
                .unwrap();
            let line = &document.lines[0];
            assert_eq!(line.bytes[12], b'A' + version as u8);
            assert_eq!(byte_to_position(line, byte(80), encoding), column(80));
            assert_eq!(
                Columns::of(line, [byte(12), byte(80)], encoding).at(byte(12)),
                column(12)
            );
            assert_eq!(
                Offsets::of(line, [column(12), column(80)], encoding).at(column(80)),
                80
            );
            assert_eq!(work::PREFIX_BYTES.get(), 12 + 13 + 80 * 3);
            assert_eq!((work::VALIDATED.get(), work::DECODED.get()), (0, 0));
            assert!(line.utf16_index.as_ref().unwrap().points.get().is_none());
        }
        let line = &document.lines[0];
        let far = byte(text.len() as u32);
        assert_eq!(
            byte_to_position(line, far, encoding),
            column(text.len() as u32 - 1)
        );
        assert!(line.utf16_index.as_ref().unwrap().points.get().is_some());
        for distance in 4..10 {
            work::VALIDATED.set(0);
            work::DECODED.set(0);
            let offset = text.len() as u32 - distance;
            assert_eq!(
                byte_to_position(line, byte(offset), encoding),
                column(offset)
            );
            assert!(work::VALIDATED.get() <= CHECKPOINT_BYTES + 3);
            assert!(work::DECODED.get() <= CHECKPOINT_BYTES + 3);
        }
    }

    #[test]
    fn a_mixed_prefix_obeys_line_wide_utf8_validity_without_building_checkpoints() {
        for valid in [true, false] {
            let mut bytes = "🚀".as_bytes().to_vec();
            bytes.resize(INDEX_MIN_BYTES, b'x');
            if !valid {
                bytes.push(0xff);
            }
            let line = Line::new(bytes, Ending::None);
            let encoding = PositionEncoding::Utf16;
            assert_eq!(
                position_to_byte(&line, column(2), encoding),
                if valid { 4 } else { 2 }
            );
            work::VALIDATED.set(0);
            assert_eq!(
                byte_to_position(&line, byte(4), encoding),
                column(if valid { 2 } else { 4 })
            );
            assert!(work::VALIDATED.get() < 32);
            assert!(line.utf16_index.as_ref().unwrap().points.get().is_none());
        }
    }

    #[test]
    fn indexed_utf16_matches_a_linear_walk_across_checkpoints_and_invalid_text() {
        let text = "x🚀甲".repeat(INDEX_MIN_BYTES);
        for suffix in [b"".as_slice(), &[0xff]] {
            let bytes = [text.as_bytes(), suffix].concat();
            let indexed = Line::new(bytes.as_slice(), Ending::None);
            let mut reference = Line::new(bytes.as_slice(), Ending::None);
            std::sync::Arc::get_mut(&mut reference).unwrap().utf16_index = None;
            let positions = [
                0,
                1,
                2,
                4095,
                4096,
                4097,
                8191,
                8192,
                8193,
                65535,
                65536,
                u32::MAX,
            ];
            let columns = positions.map(Column::new);
            let offsets = positions.map(ByteOffset::new);
            let encoding = PositionEncoding::Utf16;
            assert_eq!(
                positions_to_bytes(&indexed, &columns, encoding),
                positions_to_bytes(&reference, &columns, encoding)
            );
            assert_eq!(
                byte_to_positions(&indexed, &offsets, encoding),
                byte_to_positions(&reference, &offsets, encoding)
            );
        }
    }

    /// The contract `Columns` exists for: whatever order and however many times
    /// an offset arrives, it is converted once. Unsorted input is the normal
    /// case — a hint's tag and value ends interleave — and the underlying walk
    /// asserts ascending order in debug, so getting this wrong is a panic in
    /// tests and silently wrong columns in release.
    #[test]
    fn columns_take_offsets_in_any_order_and_answer_by_offset() {
        let line = line("— 8=FIX.4.4|35=0");
        let columns = Columns::of(
            &line,
            [byte(9), byte(0), byte(12), byte(9), byte(3)],
            PositionEncoding::Utf16,
        );
        // "—" is three bytes and one utf-16 unit, so everything after it is two
        // columns behind its offset.
        assert_eq!(columns.at(byte(0)), column(0));
        assert_eq!(columns.at(byte(3)), column(1));
        assert_eq!(columns.at(byte(9)), column(7));
        assert_eq!(columns.at(byte(12)), column(10));
    }

    #[test]
    fn columns_agree_with_the_single_offset_conversion() {
        let line = line("— 8=FIX.4.4|35=0");
        let columns = Columns::of(&line, [byte(12), byte(3)], PositionEncoding::Utf16);
        for byte in [byte(3), byte(12)] {
            assert_eq!(
                columns.at(byte),
                byte_to_position(&line, byte, PositionEncoding::Utf16),
                "offset {byte:?}"
            );
        }
    }

    /// The batch walk must be the single-offset one, not a second definition of
    /// what a column is.
    #[test]
    fn a_batch_agrees_with_one_at_a_time() {
        for text in [
            "8=FIX.4.4|58=hello",
            "8=FIX.4.4|58=привет|10=001",
            "\u{1F600}=1",
        ] {
            let line = line(text);
            let offsets: Vec<ByteOffset> = (0..=text.len() as u32).map(byte).collect();
            for enc in [PositionEncoding::Utf8, PositionEncoding::Utf16] {
                let one_by_one: Vec<Column> = offsets
                    .iter()
                    .map(|&byte| byte_to_position(&line, byte, enc))
                    .collect();
                assert_eq!(
                    byte_to_positions(&line, &offsets, enc),
                    one_by_one,
                    "{text:?} under {enc:?}"
                );
            }
        }
    }

    /// The same contract the other way round: `selectionRange` receives its
    /// cursors in whatever order and with whatever repetition the client sends.
    #[test]
    fn offsets_take_columns_in_any_order_and_answer_by_column() {
        let line = line("— 8=FIX.4.4|35=0");
        let offsets = Offsets::of(
            &line,
            [column(7), column(0), column(10), column(7), column(1)],
            PositionEncoding::Utf16,
        );
        assert_eq!(offsets.at(column(0)), 0);
        assert_eq!(offsets.at(column(1)), 3);
        assert_eq!(offsets.at(column(7)), 9);
        assert_eq!(offsets.at(column(10)), 12);
    }

    /// The batch walk must be `position_to_byte`, not a second definition of
    /// what a cursor addresses.
    #[test]
    fn a_column_batch_agrees_with_one_at_a_time() {
        for text in [
            "8=FIX.4.4|58=hello",
            "8=FIX.4.4|58=привет|10=001",
            "\u{1F600}=1",
        ] {
            let line = line(text);
            let columns: Vec<Column> = (0..=text.len() as u32 + 2).map(column).collect();
            for enc in [PositionEncoding::Utf8, PositionEncoding::Utf16] {
                let one_by_one: Vec<usize> = columns
                    .iter()
                    .map(|&column| position_to_byte(&line, column, enc))
                    .collect();
                assert_eq!(
                    positions_to_bytes(&line, &columns, enc),
                    one_by_one,
                    "{text:?} under {enc:?}"
                );
            }
        }
    }

    #[test]
    fn offsets_past_the_end_clamp_like_the_single_walk() {
        let line = line("58=привет");
        let past = byte(line.bytes.len() as u32 + 10);
        assert_eq!(
            byte_to_positions(&line, &[past], PositionEncoding::Utf16),
            vec![byte_to_position(&line, past, PositionEncoding::Utf16)]
        );
    }

    #[test]
    fn an_ascii_line_needs_no_walk_at_all() {
        let line = line("8=FIX.4.4|10=001");
        assert!(line.is_ascii);
        assert_eq!(
            byte_to_positions(&line, &[byte(0), byte(4), byte(9)], PositionEncoding::Utf16),
            vec![column(0), column(4), column(9)]
        );
    }

    #[test]
    fn nothing_to_convert_allocates_nothing() {
        let line = line("58=привет");
        assert!(byte_to_positions(&line, &[], PositionEncoding::Utf16).is_empty());
    }
}
