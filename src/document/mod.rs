//! Document model and incremental synchronisation.
//!
//! A document is `Vec<Arc<Line>>`; the line's bytes are the source of truth
//! (terminators `\n`/`\r\n` are not part of a line). A `didChange` applies its
//! content changes sequentially, then takes the notification's `version` (which
//! may be non-consecutive); a stale version or a malformed range rejects the
//! whole notification without mutating anything.
//!
//! Changes arrive already decoded to bytes: recovering the transport's byte
//! escaping is the protocol boundary's job, so nothing here knows the wire format.

pub mod position;

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lsp_types::Range;
use xxhash_rust::xxh3::xxh3_64;

use crate::codec::LineAnalysis;
use crate::ctx::{analyze, Ctx};

pub use position::{
    byte_to_position, position_to_byte, Column, Columns, Offsets, PositionEncoding,
};

#[cfg(feature = "cache-stats")]
#[doc(hidden)]
pub mod stats;

/// A line's index in a document, zero-based as LSP counts.
///
/// Like [`Column`] and [`crate::codec::ByteOffset`] it distinguishes a *unit*
/// and not an identity: nothing here ties it to the document it indexes. What
/// it stops is a line number and a column meeting as two `u32`s in one
/// signature, which is how `field_hint` used to take them.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize)]
pub struct LineNo(u32);

impl LineNo {
    pub fn new(line: u32) -> LineNo {
        LineNo(line)
    }

    pub fn raw(self) -> u32 {
        self.0
    }

    /// For indexing a line vector.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl TryFrom<u64> for LineNo {
    type Error = std::num::TryFromIntError;

    /// A client handing one back in a resolve payload.
    fn try_from(line: u64) -> Result<LineNo, Self::Error> {
        u32::try_from(line).map(LineNo)
    }
}

/// A half-open span of lines, which is the shape both range handlers build.
///
/// **A container, not a normaliser.** How a client's `Range` becomes a span of
/// lines differs by request and must keep differing: a cursor at `(n,0)..(n,0)`
/// *includes* line `n` for a code action, because the action is about the line
/// the cursor is on, and *excludes* it for an inlay hint, because a hint is a
/// point and the end is exclusive. Saturation and clamping stay with the
/// handler that owns them too. Moving any of it in here is how this type would
/// silently suppress a cursor quickfix.
///
/// It exists because `Range<LineNo>` cannot be iterated on stable — `Step` is
/// unstable — so it maps onto an integer range internally.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct LineRange {
    start: LineNo,
    end: LineNo,
}

impl LineRange {
    /// Reversed bounds are empty rather than a panic or a wrap: a caller that
    /// clamps both ends independently can produce them.
    pub fn new(start: LineNo, end: LineNo) -> LineRange {
        LineRange { start, end }
    }

    pub fn lines(self) -> impl Iterator<Item = LineNo> {
        (self.start.0..self.end.0).map(LineNo)
    }

    pub fn len(self) -> usize {
        self.end.0.saturating_sub(self.start.0) as usize
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
}

/// One line of the document. `bytes` excludes the terminator.
///
/// The bytes sit behind an `Arc` so adopting a new context can rebuild the line
/// vector without copying them; slicing is unchanged because `Arc<[u8]>` derefs
/// to a slice.
pub struct Line {
    pub bytes: Arc<[u8]>,
    pub hash: u64,
    pub is_ascii: bool,
    /// Read since the last sweep. Set by every hit and every population, so a
    /// line has to go unread across two sweeps before it is evicted.
    used: AtomicBool,
    /// A `Mutex` rather than a `OnceLock` because the sweep clears it; held
    /// only long enough to clone or store an `Arc`, or to analyse once.
    analysis: Mutex<Option<Arc<LineAnalysis>>>,
}

// Measured for the 4c-3 memory budget; a field added here is paid per line.
#[cfg(target_os = "linux")]
const _: () = assert!(std::mem::size_of::<Line>() == 48);

impl Line {
    /// Takes whatever the caller already holds: splitting a document has a
    /// slice, an edit has the `Vec` it built. Going through a `Vec` when there
    /// is only a slice would allocate and copy twice, because an `Arc<[u8]>`
    /// cannot reuse a `Vec`'s allocation — its header precedes the data.
    fn new(bytes: impl Into<Arc<[u8]>>) -> Arc<Line> {
        let bytes: Arc<[u8]> = bytes.into();
        let hash = xxh3_64(&bytes);
        let is_ascii = bytes.is_ascii();
        Arc::new(Line {
            bytes,
            hash,
            is_ascii,
            used: AtomicBool::new(false),
            analysis: Mutex::new(None),
        })
    }

    /// The same line under a fresh, empty cache. Shares the bytes.
    fn recycled(&self) -> Arc<Line> {
        Arc::new(Line {
            bytes: self.bytes.clone(),
            hash: self.hash,
            is_ascii: self.is_ascii,
            used: AtomicBool::new(false),
            analysis: Mutex::new(None),
        })
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Arc<LineAnalysis>>> {
        // A handler that panicked mid-analysis must not make the line
        // unreadable for the rest of the session; the slot is `None` or a
        // complete analysis either way.
        self.analysis.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Second chance: a line read since the last sweep keeps its analysis and
    /// loses its mark; one not read since loses the analysis.
    fn sweep(&self) -> Swept {
        let mut slot = self.slot();
        if slot.is_none() {
            Swept::Empty
        } else if self.used.swap(false, Ordering::Relaxed) {
            Swept::Kept
        } else {
            *slot = None;
            Swept::Evicted
        }
    }

    /// What is cached, without caching anything.
    ///
    /// The cache-population contract is a rule about *who calls what*, so the
    /// only way to test it is to look without touching: `cached_analysis` would
    /// populate the slot and make every caller look compliant.
    pub fn analysis_if_cached(&self) -> Option<Arc<LineAnalysis>> {
        self.slot().clone()
    }

    /// Cached analysis, computed on first use. **The only way to populate the
    /// cache**, which is why the name says so: [`analyze`] is the pure function
    /// and the safe default, and it would be the wrong one to label.
    ///
    /// Only viewport-bounded requests may call this. Anything walking a whole
    /// document calls [`analyze`] and drops the result: a fully populated cache
    /// costs several times the file size. That policy bounds *who* may cache;
    /// `Document::evict_over` bounds how much accumulates.
    ///
    /// The caller guarantees `ctx` is the one the owning document adopted and
    /// `population` is that document's, which is what `Snapshot::analysis`
    /// pairs for it.
    ///
    /// Returns an owned `Arc` rather than a reference into the slot, so that
    /// the `no-line-cache` build — which has no slot to borrow from — is the
    /// same function rather than a different one.
    pub fn cached_analysis(&self, ctx: &Ctx, population: &Population) -> Arc<LineAnalysis> {
        #[cfg(feature = "no-line-cache")]
        {
            let _ = population;
            // Recorded too, and always a miss — which is what "no cache" means,
            // and keeps the two builds' counters comparable.
            #[cfg(feature = "cache-stats")]
            stats::record(false);
            Arc::new(analyze(&self.bytes, ctx))
        }
        #[cfg(not(feature = "no-line-cache"))]
        {
            let mut slot = self.slot();
            let hit = slot.is_some();
            #[cfg(feature = "cache-stats")]
            stats::record(hit);
            let analysis = slot
                .get_or_insert_with(|| Arc::new(analyze(&self.bytes, ctx)))
                .clone();
            if !hit {
                population.0.fetch_add(1, Ordering::Relaxed);
            }
            self.used.store(true, Ordering::Relaxed);
            analysis
        }
    }
}

enum Swept {
    Empty,
    Kept,
    Evicted,
}

/// How many lines of a document hold an analysis — the number the sweep
/// compares against the bound.
///
/// Shared into every snapshot of the document, so a task on another thread
/// counts what it populates; only the main loop evicts.
///
/// **Never exact, and deliberately wrong in one direction.** An edit replaces
/// lines without telling it, and once tasks run on a pool one can populate a
/// line an edit has already dropped, so exactness was never on offer. Both
/// residues are arranged to read high rather than low: an edit leaves lines
/// counted that no longer exist, and a sweep counts a line populated ahead of
/// its walk twice. A count that read low would delay the next sweep and
/// overshoot the bound; one that reads high only brings it forward.
#[derive(Default)]
pub struct Population(AtomicUsize);

impl Population {
    pub fn count(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

/// A single content change: a full-document replacement or a ranged edit.
pub enum Change {
    Full(Vec<u8>),
    Ranged { range: Range, text: Vec<u8> },
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChangeError {
    StaleVersion { got: i32, have: i32 },
    MalformedRange,
}

impl fmt::Display for ChangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChangeError::StaleVersion { got, have } => {
                write!(f, "stale version {got} (document at {have})")
            }
            ChangeError::MalformedRange => write!(f, "malformed change range"),
        }
    }
}

pub struct Document {
    pub version: i32,
    pub population: Arc<Population>,
    /// Behind an `Arc` so a snapshot is one refcount bump rather than a copy of
    /// the whole pointer vector — on a 100k-line log that was 100k atomic
    /// increments and 780 KB per request, paid on every hover and every hint.
    /// The cost moves to editing while a task holds the old view, where
    /// `Arc::make_mut` copies once; edits are rarer than requests and the
    /// multi-change arm below already copies unconditionally.
    ///
    /// Reconsider in Stage 4, when there are real snapshot consumers: a
    /// range-scoped snapshot — handing a viewport-bounded task the slice it
    /// actually reads instead of the whole document — may be the better answer,
    /// and it is only measurable once something other than a hover stub asks.
    pub lines: Arc<Vec<Arc<Line>>>,
    /// The context every cached analysis in `lines` was produced under.
    pub ctx: Arc<Ctx>,
}

/// Lines not counting the empty one a trailing newline leaves behind.
pub fn content_lines(lines: &[Arc<Line>]) -> usize {
    lines.len() - usize::from(lines.last().is_some_and(|line| line.bytes.is_empty()))
}

impl Document {
    pub fn content_lines(&self) -> usize {
        content_lines(&self.lines)
    }

    pub fn open(bytes: &[u8], version: i32, ctx: Arc<Ctx>) -> Document {
        Document {
            version,
            population: Arc::default(),
            lines: Arc::new(split_lines(bytes)),
            ctx,
        }
    }

    /// Evict what has gone unread, once more lines than `bound` hold an
    /// analysis. Nothing happens below the bound, so calling it after every
    /// request costs one load.
    ///
    /// Second chance, not LRU: a hit marks the line, the sweep clears the
    /// unmarked and resets the marks. The access pattern is a screen read as a
    /// block and left as a block, so exact recency would buy nothing for a
    /// shared structure on the read path. Returns how many were evicted.
    ///
    /// **It reaches this document's current lines and nothing else.** An
    /// analysis belonging to a line an edit replaced lives on while a snapshot
    /// taken before that edit is still in flight — the diagnostics worker holds
    /// one across a whole walk — and is freed when that snapshot drops rather
    /// than by any bound.
    pub fn evict_over(&self, bound: usize) -> usize {
        let before = self.population.count();
        if before <= bound {
            return 0;
        }
        let mut kept = 0;
        let mut evicted = 0;
        for line in self.lines.iter() {
            match line.sweep() {
                Swept::Empty => {}
                Swept::Kept => kept += 1,
                Swept::Evicted => evicted += 1,
            }
        }
        // Adjusted, not stored: a task that populated a line this walk had
        // already passed must not have its increment overwritten, or the count
        // reads low and the next sweep comes late. The residue points the other
        // way instead — a line populated ahead of the walk is counted twice,
        // once as a survivor and once by its own increment — and an overcount
        // only brings the next sweep forward.
        self.population
            .0
            .fetch_sub(before.saturating_sub(kept), Ordering::Relaxed);
        evicted
    }

    /// Take `ctx`, dropping every cached analysis when it is a different one.
    /// Byte buffers are shared, so a rebuild is `n` refcount bumps, never a copy.
    ///
    /// Identity is the `Arc`, not the fingerprint: `ServerState` keeps its
    /// context unless the interpretation moved, so an equivalent-but-distinct
    /// one never reaches a document on the server path. A caller that builds
    /// its own gets a rebuild, which is conservative rather than wrong.
    pub fn set_ctx(&mut self, ctx: Arc<Ctx>) {
        if !Arc::ptr_eq(&self.ctx, &ctx) {
            self.lines = Arc::new(self.lines.iter().map(|line| line.recycled()).collect());
            self.population.0.store(0, Ordering::Relaxed);
            self.ctx = ctx;
        }
    }

    /// Apply a `didChange`. Either every change lands or none does.
    ///
    /// The report is a **return value** rather than something the document
    /// keeps: a batch is applied to a copy and published only if every change
    /// lands, so a rejected batch must say nothing at all.
    pub fn apply_changes(
        &mut self,
        changes: &[Change],
        new_version: i32,
        encoding: PositionEncoding,
    ) -> Result<Edited, ChangeError> {
        if new_version <= self.version {
            return Err(ChangeError::StaleVersion {
                got: new_version,
                have: self.version,
            });
        }
        let edited = match changes {
            // The common shape — one edit. `apply_change` validates before it
            // mutates, so all-or-nothing holds without copying the line vector.
            [change] => apply_change(Arc::make_mut(&mut self.lines), change, encoding)?,
            [] => Edited::Nothing,
            // Several changes must land together. Each one is atomic on its
            // own, but the second failing after the first applied would leave a
            // half-updated document, so mutate a copy of the pointer vector and
            // publish it only once they all land.
            //
            // The copy is O(document), not O(edit): a vector allocation plus one
            // atomic increment per line, measured at ~5 ns a line — 0.5 ms and
            // 780 KB on a 100k-line log. Two reasons that is left alone. The
            // single-change arm above, which is what an editor sends for
            // ordinary typing, never pays it; this arm is for multi-cursor
            // edits. And the asymptotically right fix — journal each change's
            // replaced range and roll back on failure, O(lines touched) — is
            // machinery for an error path only a client that violates the
            // protocol can reach. Pre-validating instead is not possible: a
            // later change's ranges are indices into the document the earlier
            // ones produced.
            _ => {
                let mut lines = (*self.lines).clone();
                let mut edited = Edited::Nothing;
                for change in changes {
                    edited = edited.then(apply_change(&mut lines, change, encoding)?);
                }
                self.lines = Arc::new(lines);
                edited
            }
        };
        self.version = new_version;
        Ok(edited)
    }
}

/// What an edit did to a document's line numbering, in **new**-document
/// coordinates.
///
/// It exists so a consumer that keeps something per line — the semantic-token
/// digest — can splice rather than rebuild. What it promises is line identity
/// either side of the span: **before `first`, new line `i` is old line `i`;
/// after `last`, new line `i` is old line `i - line_delta`.** Inside the span
/// there is no identity at all, which is the point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edited {
    Nothing,
    Lines {
        first: u32,
        /// The last changed line **as the document now numbers it**.
        last: u32,
        /// New line count minus old, so it is negative for a deletion.
        line_delta: i64,
    },
    /// The text was replaced wholesale; nothing survives by number.
    Everything,
}

impl Edited {
    fn one(line: usize) -> Edited {
        Edited::Lines {
            first: line as u32,
            last: line as u32,
            line_delta: 0,
        }
    }

    /// One batch is one report. Each change addressed the document the change
    /// before it left, so the earlier bounds are shifted into the later
    /// change's coordinates before the two are merged.
    ///
    /// Deliberately conservative: the merged span may be wider than the lines
    /// that actually changed, which costs re-encoding and never correctness.
    pub fn then(self, later: Edited) -> Edited {
        match (self, later) {
            (Edited::Everything, _) | (_, Edited::Everything) => Edited::Everything,
            (Edited::Nothing, other) | (other, Edited::Nothing) => other,
            (
                Edited::Lines {
                    first,
                    last,
                    line_delta,
                },
                Edited::Lines {
                    first: later_first,
                    last: later_last,
                    line_delta: later_delta,
                },
            ) => {
                // The later change replaced lines up to this one, counted the
                // way the document was numbered when it ran.
                let replaced_last = i64::from(later_last) - later_delta;
                let shifted = if i64::from(last) > replaced_last {
                    i64::from(last) + later_delta
                } else {
                    i64::from(later_last)
                };
                Edited::Lines {
                    first: first.min(later_first),
                    last: shifted.max(i64::from(later_last)).max(0) as u32,
                    line_delta: line_delta + later_delta,
                }
            }
        }
    }
}

fn apply_change(
    lines: &mut Vec<Arc<Line>>,
    change: &Change,
    encoding: PositionEncoding,
) -> Result<Edited, ChangeError> {
    match change {
        Change::Full(bytes) => {
            *lines = split_lines(bytes);
            Ok(Edited::Everything)
        }
        Change::Ranged { range, text } => apply_ranged(lines, range, text, encoding),
    }
}

fn apply_ranged(
    lines: &mut Vec<Arc<Line>>,
    range: &Range,
    text: &[u8],
    encoding: PositionEncoding,
) -> Result<Edited, ChangeError> {
    let first = range.start.line as usize;
    let last = range.end.line as usize;
    if first > last || last >= lines.len() {
        return Err(ChangeError::MalformedRange);
    }
    let start_byte = position_to_byte(&lines[first], Column::new(range.start.character), encoding);
    let end_byte = position_to_byte(&lines[last], Column::new(range.end.character), encoding);
    if first == last && start_byte > end_byte {
        return Err(ChangeError::MalformedRange);
    }

    let head = &lines[first].bytes[..start_byte];
    let tail = &lines[last].bytes[end_byte..];
    let mut region = Vec::with_capacity(head.len() + text.len() + tail.len());
    region.extend_from_slice(head);
    region.extend_from_slice(text);
    region.extend_from_slice(tail);

    // An edit that neither spans nor introduces a line break rewrites one line;
    // moving the region into it skips the intermediate vector and its copies.
    if first == last && !region.contains(&b'\n') {
        // Trimmed the way `split_lines` trims, or an edit that ends a line with
        // a carriage return would leave bytes `didOpen` of the same text never
        // produces — a false checksum mismatch, and a stamp that cannot match.
        let trimmed = region.strip_suffix(b"\r").map_or(region.len(), <[u8]>::len);
        let mut region = region;
        region.truncate(trimmed);
        lines[first] = Line::new(region);
        return Ok(Edited::one(first));
    }
    let replacement = split_lines(&region);
    let grown = replacement.len() as i64 - (last - first + 1) as i64;
    let new_last = first + replacement.len() - 1;
    lines.splice(first..=last, replacement);
    Ok(Edited::Lines {
        first: first as u32,
        last: new_last as u32,
        line_delta: grown,
    })
}

/// Split raw bytes into lines on `\n`, stripping a trailing `\r`. Always yields at
/// least one line (empty input → one empty line; a trailing `\n` → empty last line).
fn split_lines(bytes: &[u8]) -> Vec<Arc<Line>> {
    bytes
        .split(|&b| b == b'\n')
        .map(|line| Line::new(line.strip_suffix(b"\r").unwrap_or(line)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Position;
    use proptest::prelude::*;

    const UTF8: PositionEncoding = PositionEncoding::Utf8;

    fn range(start_line: u32, start_ch: u32, end_line: u32, end_ch: u32) -> Range {
        Range {
            start: Position {
                line: start_line,
                character: start_ch,
            },
            end: Position {
                line: end_line,
                character: end_ch,
            },
        }
    }

    fn line_bytes(doc: &Document) -> Vec<Vec<u8>> {
        doc.lines.iter().map(|line| line.bytes.to_vec()).collect()
    }

    fn ctx() -> Arc<Ctx> {
        Ctx::for_test("FIX.4.4")
    }

    /// The version a line resolved to, `None` when it is not a message.
    fn version_of(doc: &Document, line: usize) -> Option<String> {
        doc.lines[line]
            .cached_analysis(&doc.ctx, &doc.population)
            .message()
            .map(|message| message.version.to_string())
    }

    fn open(bytes: &[u8], version: i32) -> Document {
        Document::open(bytes, version, ctx())
    }

    /// The point of the `Arc` around the line vector: taking a view of the
    /// document must not walk it. Comparing the `Arc`s is how that stays true —
    /// a `Vec` clone would pass every other test in this file.
    #[test]
    fn a_view_of_the_lines_costs_one_refcount() {
        let doc = open(b"8=FIX.4.4|35=0\n8=FIX.4.4|35=1", 1);
        let view = doc.lines.clone();
        assert!(Arc::ptr_eq(&view, &doc.lines));
    }

    /// And the price of that: an edit while a view is alive has to copy, or the
    /// holder would see the document change underneath it.
    #[test]
    fn an_edit_leaves_an_outstanding_view_untouched() {
        let mut doc = open(b"8=FIX.4.4|35=0\n8=FIX.4.4|35=1", 1);
        let view = doc.lines.clone();
        doc.apply_changes(&[ranged(range(0, 13, 0, 14), "5")], 2, UTF8)
            .unwrap();
        assert!(
            !Arc::ptr_eq(&view, &doc.lines),
            "the edit forked the vector"
        );
        assert_eq!(
            &*view[0].bytes, b"8=FIX.4.4|35=0",
            "the view kept its lines"
        );
        assert_eq!(&*doc.lines[0].bytes, b"8=FIX.4.4|35=5");
        assert!(
            Arc::ptr_eq(&view[1], &doc.lines[1]),
            "an untouched line is still shared, not duplicated"
        );
    }

    fn ranged(r: Range, text: &str) -> Change {
        Change::Ranged {
            range: r,
            text: text.as_bytes().to_vec(),
        }
    }

    #[test]
    fn open_splitting_convention() {
        assert_eq!(line_bytes(&open(b"", 1)).len(), 1);
        assert_eq!(
            line_bytes(&open(b"a\nb", 1)),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
        // Trailing newline yields an empty final line.
        assert_eq!(
            line_bytes(&open(b"a\n", 1)),
            vec![b"a".to_vec(), b"".to_vec()]
        );
    }

    #[test]
    fn crlf_is_stripped() {
        assert_eq!(
            line_bytes(&open(b"a\r\nb", 1)),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
    }

    #[test]
    fn non_utf8_bytes_are_kept_verbatim() {
        let doc = open(&[b'A', 0x80, b'B'], 1);
        assert!(!doc.lines[0].is_ascii);
        assert_eq!(&*doc.lines[0].bytes, vec![b'A', 0x80, b'B']);
    }

    #[test]
    fn single_line_edit_preserves_other_arcs() {
        let mut doc = open(b"aaa\nbbb\nccc", 1);
        let first = Arc::as_ptr(&doc.lines[0]);
        let third = Arc::as_ptr(&doc.lines[2]);
        doc.apply_changes(&[ranged(range(1, 0, 1, 3), "XY")], 2, UTF8)
            .unwrap();
        assert_eq!(&*doc.lines[1].bytes, b"XY");
        assert_eq!(Arc::as_ptr(&doc.lines[0]), first, "line 0 keeps its Arc");
        assert_eq!(Arc::as_ptr(&doc.lines[2]), third, "line 2 keeps its Arc");
        assert_eq!(doc.version, 2);
    }

    #[test]
    fn multi_change_applied_sequentially() {
        let mut doc = open(b"abc", 1);
        doc.apply_changes(
            &[
                ranged(range(0, 3, 0, 3), "d"), // abc -> abcd
                ranged(range(0, 0, 0, 1), "Z"), // abcd -> Zbcd
            ],
            5, // non-consecutive version jump
            UTF8,
        )
        .unwrap();
        assert_eq!(&*doc.lines[0].bytes, b"Zbcd");
        assert_eq!(doc.version, 5);
    }

    #[test]
    fn newline_insertion_splits_line() {
        let mut doc = open(b"ab", 1);
        doc.apply_changes(&[ranged(range(0, 1, 0, 1), "\n")], 2, UTF8)
            .unwrap();
        assert_eq!(line_bytes(&doc), vec![b"a".to_vec(), b"b".to_vec()]);
    }

    #[test]
    fn stale_version_rejected() {
        let mut doc = open(b"a", 5);
        let err = doc
            .apply_changes(&[Change::Full(b"b".to_vec())], 5, UTF8)
            .unwrap_err();
        assert_eq!(err, ChangeError::StaleVersion { got: 5, have: 5 });
        assert_eq!(&*doc.lines[0].bytes, b"a", "unchanged after rejection");
    }

    #[test]
    fn malformed_range_rejected_wholesale() {
        let mut doc = open(b"abc\ndef", 1);
        // start after end on the same line
        let err = doc
            .apply_changes(&[ranged(range(0, 3, 0, 1), "X")], 2, UTF8)
            .unwrap_err();
        assert_eq!(err, ChangeError::MalformedRange);
        assert_eq!(line_bytes(&doc), vec![b"abc".to_vec(), b"def".to_vec()]);
        assert_eq!(doc.version, 1, "version unchanged after rejection");
    }

    #[test]
    fn a_later_malformed_change_discards_the_earlier_one() {
        let mut doc = open(b"abc", 1);
        let err = doc
            .apply_changes(
                &[
                    ranged(range(0, 0, 0, 1), "Z"), // would apply
                    ranged(range(9, 0, 9, 0), "Y"), // past EOF
                ],
                2,
                UTF8,
            )
            .unwrap_err();
        assert_eq!(err, ChangeError::MalformedRange);
        assert_eq!(&*doc.lines[0].bytes, b"abc", "the batch is all-or-nothing");
    }

    #[test]
    fn line_index_past_eof_rejected() {
        let mut doc = open(b"a", 1);
        let err = doc
            .apply_changes(&[ranged(range(5, 0, 5, 0), "x")], 2, UTF8)
            .unwrap_err();
        assert_eq!(err, ChangeError::MalformedRange);
    }

    #[test]
    fn utf16_offset_past_non_bmp_scalar() {
        // "𝔽" is U+1D53D — 2 UTF-16 code units, 4 UTF-8 bytes. Character 2 sits
        // just after it, so the byte offset must be 4, not 2.
        let doc = open("𝔽x".as_bytes(), 1);
        let line = &doc.lines[0];
        assert!(!line.is_ascii);
        assert_eq!(
            position_to_byte(line, Column::new(2), PositionEncoding::Utf16),
            4
        );
        assert_eq!(
            byte_to_position(
                line,
                crate::codec::ByteOffset::new(4),
                PositionEncoding::Utf16
            ),
            Column::new(2)
        );
    }

    /// Independent flat-buffer model over ASCII lines.
    fn flat_offset(lines: &[Vec<u8>], line: usize, ch: usize) -> usize {
        let mut offset = 0;
        for l in &lines[..line] {
            offset += l.len() + 1; // + '\n'
        }
        offset + ch.min(lines[line].len())
    }

    proptest! {
        #[test]
        fn incremental_equals_fresh_split(
            init in prop::collection::vec("[a-z]{0,4}", 1..5),
            edits in prop::collection::vec(
                (0usize..4, 0usize..5, 0usize..4, 0usize..5, "[A-Z]{0,3}", any::<bool>()),
                0..8,
            ),
            crlf in any::<bool>(),
            utf16 in any::<bool>(),
        ) {
            let sep: &[u8] = if crlf { b"\r\n" } else { b"\n" };
            let encoding = if utf16 { PositionEncoding::Utf16 } else { UTF8 };
            let init_bytes = init.iter().map(|s| s.as_bytes().to_vec())
                .collect::<Vec<_>>().join(sep);

            let mut doc = open(&init_bytes, 1);
            let mut model: Vec<Vec<u8>> = init.iter().map(|s| s.clone().into_bytes()).collect();
            let mut version = 1i32;

            for (start_line, start_ch, end_line, end_ch, insert, insert_newline) in edits {
                // Clamp to a valid in-range change against the current model.
                let count = model.len();
                let start_line = start_line.min(count - 1);
                let end_line = end_line.min(count - 1).max(start_line);
                let start_ch = start_ch.min(model[start_line].len());
                let end_ch = if end_line == start_line {
                    end_ch.min(model[end_line].len()).max(start_ch)
                } else {
                    end_ch.min(model[end_line].len())
                };

                let mut text = insert.into_bytes();
                if insert_newline { text.push(b'\n'); }

                // Apply to the model via a flat splice.
                let from = flat_offset(&model, start_line, start_ch);
                let to = flat_offset(&model, end_line, end_ch);
                let mut flat = model.join(&b'\n');
                flat.splice(from..to, text.iter().copied());
                model = split_lines(&flat).iter().map(|line| line.bytes.to_vec()).collect();

                // Apply to the document.
                version += 1 + (version as usize % 3) as i32; // version jumps
                let change = Change::Ranged {
                    range: range(start_line as u32, start_ch as u32,
                                 end_line as u32, end_ch as u32),
                    text,
                };
                doc.apply_changes(&[change], version, encoding).unwrap();
            }

            prop_assert_eq!(line_bytes(&doc), model);
        }

        /// The whole promise of `Edited`: outside the span a line keeps its
        /// identity, shifted by the delta on the far side and by nothing on the
        /// near one. Asserted against the bytes, which is the only thing that
        /// can tell one line from another.
        #[test]
        fn a_report_says_which_lines_kept_their_identity(
            init in prop::collection::vec("[a-z]{0,4}", 1..8),
            start_line in 0usize..8,
            start_ch in 0usize..5,
            end_line in 0usize..8,
            end_ch in 0usize..5,
            insert in "[A-Z\n]{0,8}",
            batch in 1usize..4,
        ) {
            let init_bytes = init.join("\n").into_bytes();
            let mut doc = open(&init_bytes, 1);
            let before: Vec<Vec<u8>> = line_bytes(&doc);

            let count = before.len();
            let start_line = start_line.min(count - 1);
            let end_line = end_line.min(count - 1).max(start_line);
            let start_ch = start_ch.min(before[start_line].len());
            let end_ch = if end_line == start_line {
                end_ch.min(before[end_line].len()).max(start_ch)
            } else {
                end_ch.min(before[end_line].len())
            };
            // Each change addresses the document the one before it left, which
            // is what `then` folds — so they must differ, or the fold compares
            // one bound against itself and the arithmetic never runs. Line 0 is
            // always in range whatever the earlier changes did to the rest.
            let changes: Vec<Change> = (0..batch)
                .map(|at| Change::Ranged {
                    range: if at == 0 {
                        range(start_line as u32, start_ch as u32,
                              end_line as u32, end_ch as u32)
                    } else {
                        range(0, 0, 0, 0)
                    },
                    text: insert.clone().into_bytes(),
                })
                .collect();
            let Ok(edited) = doc.apply_changes(&changes, 2, UTF8) else {
                return Ok(());
            };
            let after: Vec<Vec<u8>> = line_bytes(&doc);

            let Edited::Lines { first, last, line_delta } = edited else {
                return Ok(());
            };
            for at in 0..first as usize {
                prop_assert_eq!(&after[at], &before[at], "line {} is before the span", at);
            }
            for (at, line) in after.iter().enumerate().skip(last as usize + 1) {
                let was = at as i64 - line_delta;
                prop_assert!(was >= 0 && (was as usize) < before.len(), "old line {was} exists");
                prop_assert_eq!(line, &before[was as usize],
                                "line {} is old line {}", at, was);
            }
        }
    }

    /// The single-line fast path once kept a trailing carriage return that
    /// every other path strips, so the same text reached by an edit and by
    /// opening it were different bytes — a false checksum mismatch, and a
    /// stamp that could not match.
    #[test]
    fn an_edit_that_ends_a_line_with_a_carriage_return_trims_it() {
        let mut doc = open(b"8=FIX.4.4|10=171", 1);
        doc.apply_changes(&[ranged(range(0, 13, 0, 16), "171\r")], 2, UTF8)
            .unwrap();
        assert_eq!(
            line_bytes(&doc),
            line_bytes(&open(b"8=FIX.4.4|10=171\r", 1))
        );
        assert_eq!(line_bytes(&doc), vec![b"8=FIX.4.4|10=171".to_vec()]);
    }

    #[test]
    #[cfg(not(feature = "no-line-cache"))] // asserts a cache exists
    fn analysis_is_computed_once_per_line() {
        let doc = open(b"8=FIX.4.4|35=0", 1);
        let first = doc.lines[0]
            .cached_analysis(&doc.ctx, &doc.population)
            .clone();
        assert!(
            Arc::ptr_eq(
                &doc.lines[0].cached_analysis(&doc.ctx, &doc.population),
                &first
            ),
            "the second call is a cache hit"
        );
    }

    #[test]
    #[cfg(not(feature = "no-line-cache"))] // asserts a cache exists
    fn an_edit_invalidates_only_the_touched_line() {
        let mut doc = open(b"8=FIX.4.4|35=0\n8=FIX.4.4|35=1\n8=FIX.4.4|35=2", 1);
        let keep = doc.lines[2]
            .cached_analysis(&doc.ctx, &doc.population)
            .clone();
        doc.lines[1].cached_analysis(&doc.ctx, &doc.population);
        doc.apply_changes(&[ranged(range(1, 13, 1, 14), "5")], 2, UTF8)
            .unwrap();
        assert!(
            Arc::ptr_eq(
                &doc.lines[2].cached_analysis(&doc.ctx, &doc.population),
                &keep
            ),
            "an untouched line keeps its analysis"
        );
        let edited = doc.lines[1].cached_analysis(&doc.ctx, &doc.population);
        let message = edited.message().expect("a message");
        assert_eq!(message.first(35).unwrap().value(&doc.lines[1].bytes), b"5");
    }

    #[test]
    fn each_line_resolves_its_own_dictionary() {
        // The version is a per-line fact, so one cached document holds analyses
        // decoded against different dictionaries.
        let doc = open(b"8=FIX.4.2|35=0\n8=FIX.4.4|35=0\n35=0", 1);
        assert_eq!(version_of(&doc, 0).as_deref(), Some("FIX.4.2"));
        assert_eq!(version_of(&doc, 1).as_deref(), Some("FIX.4.4"));
        assert_eq!(
            version_of(&doc, 2).as_deref(),
            Some("FIX.4.4"),
            "the default"
        );
    }

    #[test]
    fn setting_a_different_context_drops_the_analyses_but_shares_the_bytes() {
        let mut doc = open(b"8=FIX.4.4|35=0", 1);
        let before = doc.lines[0]
            .cached_analysis(&doc.ctx, &doc.population)
            .clone();
        let bytes = doc.lines[0].bytes.as_ptr();
        doc.set_ctx(Ctx::for_test("FIX.4.2"));
        assert_eq!(doc.lines[0].bytes.as_ptr(), bytes, "no byte copy");
        assert!(
            !Arc::ptr_eq(
                &doc.lines[0].cached_analysis(&doc.ctx, &doc.population),
                &before
            ),
            "analysis recomputed"
        );
        assert_eq!(
            version_of(&doc, 0).as_deref(),
            Some("FIX.4.4"),
            "BeginString(8) still wins over the default"
        );
    }

    #[test]
    #[cfg(not(feature = "no-line-cache"))] // asserts a cache exists
    fn setting_the_same_context_keeps_the_analyses() {
        let mut doc = open(b"8=FIX.4.4|35=0", 1);
        let before = doc.lines[0]
            .cached_analysis(&doc.ctx, &doc.population)
            .clone();
        let same = doc.ctx.clone();
        doc.set_ctx(same);
        assert!(Arc::ptr_eq(
            &doc.lines[0].cached_analysis(&doc.ctx, &doc.population),
            &before
        ));
    }

    /// The contract stated rather than implied: identity is the `Arc`, not the
    /// fingerprint. An equivalent-but-distinct context never reaches a document
    /// on the server path, which is what
    /// `a_template_only_change_keeps_the_context_and_reaches_the_renderer`
    /// checks; here it costs a rebuild, which is conservative, not wrong.
    #[test]
    fn setting_an_equivalent_but_distinct_context_still_drops_the_analyses() {
        let mut doc = open(b"8=FIX.4.4|35=0", 1);
        let before = doc.lines[0]
            .cached_analysis(&doc.ctx, &doc.population)
            .clone();
        let twin = Ctx::for_test("FIX.4.4");
        assert_eq!(twin.fingerprint(), doc.ctx.fingerprint(), "equivalent");
        doc.set_ctx(twin);
        assert!(!Arc::ptr_eq(
            &doc.lines[0].cached_analysis(&doc.ctx, &doc.population),
            &before
        ));
    }

    #[cfg(not(feature = "no-line-cache"))] // every assertion on it needs one
    fn cached(doc: &Document) -> usize {
        doc.lines
            .iter()
            .filter(|line| line.analysis_if_cached().is_some())
            .count()
    }

    fn populate(doc: &Document, lines: std::ops::Range<usize>) {
        for line in &doc.lines[lines] {
            line.cached_analysis(&doc.ctx, &doc.population);
        }
    }

    #[test]
    #[cfg(not(feature = "no-line-cache"))] // asserts a cache exists
    fn below_the_bound_a_sweep_touches_nothing() {
        let doc = open(b"35=0\n35=1\n35=2\n35=3", 1);
        populate(&doc, 0..4);
        let before = doc.lines[0].analysis_if_cached().unwrap();
        assert_eq!(doc.evict_over(4), 0, "at the bound is not over it");
        assert!(Arc::ptr_eq(
            &doc.lines[0].analysis_if_cached().unwrap(),
            &before
        ));
        assert_eq!(doc.population.count(), 4);
    }

    /// Second chance: the first sweep over a line only clears its mark, the
    /// next one evicts it — unless it was read in between.
    #[test]
    #[cfg(not(feature = "no-line-cache"))] // asserts a cache exists
    fn a_line_is_evicted_by_the_second_sweep_it_goes_unread_across() {
        let doc = open(b"35=0\n35=1\n35=2\n35=3\n35=4\n35=5", 1);
        populate(&doc, 0..6);
        assert_eq!(doc.evict_over(4), 0, "everything was populated since");
        assert_eq!(cached(&doc), 6);
        assert_eq!(doc.population.count(), 6, "recounted, not reset");

        let kept = doc.lines[2].cached_analysis(&doc.ctx, &doc.population);
        assert_eq!(doc.evict_over(4), 5, "all but the one read");
        assert_eq!(cached(&doc), 1);
        assert!(Arc::ptr_eq(
            &doc.lines[2].analysis_if_cached().unwrap(),
            &kept
        ));
        assert_eq!(doc.population.count(), 1);
    }

    /// An edit drops analyses without telling the count; the next sweep
    /// recounts. In between the count is only ever too high.
    #[test]
    #[cfg(not(feature = "no-line-cache"))] // asserts a cache exists
    fn the_count_is_an_overcount_after_edits_until_a_sweep_recounts() {
        let mut doc = open(b"35=0\n35=1\n35=2\n35=3", 1);
        populate(&doc, 0..4);
        // One line rewritten in place, two more spliced into one; every
        // line an edit touches is a new object, and the last keeps its identity.
        doc.apply_changes(&[ranged(range(0, 0, 0, 1), "x")], 2, UTF8)
            .unwrap();
        doc.apply_changes(&[ranged(range(1, 0, 2, 4), "")], 3, UTF8)
            .unwrap();
        assert_eq!(
            line_bytes(&doc),
            [b"x5=0".to_vec(), vec![], b"35=3".to_vec()]
        );
        assert_eq!(cached(&doc), 1);
        assert_eq!(doc.population.count(), 4, "not told");

        assert_eq!(doc.evict_over(1), 0, "the survivor was read since");
        assert_eq!(doc.population.count(), 1, "and now it is exact");

        doc.apply_changes(&[Change::Full(b"35=9".to_vec())], 4, UTF8)
            .unwrap();
        assert_eq!(cached(&doc), 0);
        assert_eq!(doc.population.count(), 1);
        assert_eq!(doc.evict_over(0), 0, "nothing left to evict");
        assert_eq!(doc.population.count(), 0);
    }

    /// A population the sweep walked past must survive it, or the count reads
    /// low, the next sweep comes late and the bound is overshot.
    ///
    /// Deterministic rather than a stress test: the sweep takes each line's own
    /// lock, so holding one blocks the walk at a known line, and the population
    /// below is provably behind it. Storing the survivor count instead of
    /// adjusting by it fails here.
    #[test]
    #[cfg(not(feature = "no-line-cache"))] // asserts a cache exists
    fn a_population_the_sweep_walked_past_survives_it() {
        let doc = Arc::new(open(["8=FIX.4.4|35=0|34=1|"; 10].join("\n").as_bytes(), 1));
        populate(&doc, 0..10);
        assert_eq!(
            doc.evict_over(0),
            0,
            "the first sweep only clears the marks"
        );
        assert_eq!(doc.population.count(), 10);

        // Taken before the sweep starts, so the walk blocks at line 5 having
        // finished 0..=4.
        let held = doc.lines[5].slot();
        let sweeping = std::thread::spawn({
            let doc = doc.clone();
            move || doc.evict_over(0)
        });
        while doc.lines[0].analysis_if_cached().is_some() {
            std::thread::yield_now();
        }

        // Behind the walk by construction: line 0 has already been swept.
        doc.lines[0].cached_analysis(&doc.ctx, &doc.population);
        drop(held);
        assert_eq!(sweeping.join().expect("the sweep"), 10, "all ten unread");

        assert_eq!(cached(&doc), 1, "only the one repopulated since");
        assert_eq!(doc.population.count(), 1, "and the count says so");
    }

    #[test]
    #[cfg(not(feature = "no-line-cache"))] // asserts a cache exists
    fn a_new_context_resets_the_count_with_the_analyses() {
        let mut doc = open(b"35=0\n35=1", 1);
        populate(&doc, 0..2);
        doc.set_ctx(Ctx::for_test("FIX.4.2"));
        assert_eq!(cached(&doc), 0);
        assert_eq!(doc.population.count(), 0);
    }

    #[test]
    #[cfg(feature = "no-line-cache")]
    fn without_a_cache_the_count_never_moves() {
        let doc = open(b"35=0\n35=1", 1);
        populate(&doc, 0..2);
        assert_eq!(doc.population.count(), 0);
        assert_eq!(doc.evict_over(0), 0);
    }
}

#[cfg(test)]
mod line_range_tests {
    use super::{LineNo, LineRange};

    fn range(start: u32, end: u32) -> LineRange {
        LineRange::new(LineNo::new(start), LineNo::new(end))
    }

    /// Reversed bounds are empty, not a panic and not a wrap. A handler that
    /// clamps its two ends independently can produce them, and `len()` is read
    /// against a cap before anything is iterated.
    #[test]
    fn reversed_bounds_are_empty() {
        let backwards = range(9, 4);
        assert_eq!(backwards.len(), 0);
        assert!(backwards.is_empty());
        assert_eq!(backwards.lines().count(), 0);
    }

    #[test]
    fn a_span_yields_its_lines_and_nothing_past_them() {
        let lines: Vec<u32> = range(4, 7).lines().map(LineNo::raw).collect();
        assert_eq!(lines, vec![4, 5, 6], "half-open");
        assert_eq!(range(4, 7).len(), 3);
        assert!(range(4, 4).is_empty(), "empty is not one line");
    }
}
