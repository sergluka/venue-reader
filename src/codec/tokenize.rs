//! Splitting a line into `tag=value` spans.
//!
//! Accepts the token language of the tree-sitter grammar the editor side uses,
//! but is line-anchored and resynchronising rather than whole-file: one
//! separator per line, a log prefix skipped when nothing parses from column 0,
//! and trailing junk recorded instead of rejecting the line.

use super::ByteOffset;

/// What a line is, decided without a dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Empty or whitespace-only.
    Empty,
    /// First non-blank byte is `#`; a `fix:` modeline is one of these.
    Comment,
    /// Non-blank, but no `tag=value` pair could be read.
    Other,
    Message,
}

/// What one line tokenizes to: how it classifies, and the `tag=value` spans it
/// yielded. The hand-off from [`tokenize`] to the decoder, never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineTokens {
    pub kind: LineKind,
    /// Bytes skipped before the first field; `0` unless a log prefix was cut.
    pub prefix_len: ByteOffset,
    pub fields: Vec<FieldSpan>,
    /// The one separator byte this line uses. `None` when it contains none.
    pub separator: Option<u8>,
    /// Where parsing stopped short of the line end, if it did.
    pub trailing_junk_at: Option<ByteOffset>,
}

/// One `tag=value` pair. Offsets are 0-based, half-open byte offsets into the
/// **whole line**, so a skipped prefix never shifts them.
///
/// `tag_end` is both the end of the digit run and the index of `=`, because no
/// whitespace is allowed around it; `value_start` is therefore always
/// `tag_end + 1`, which is what lets a decoded field drop the offset. `value_end`
/// is the index of the separator that ends the value, or the line length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSpan {
    /// `u32::MAX` when the digit run does not fit a `u32`; the span is still
    /// recorded so a later stage can diagnose it.
    pub tag: u32,
    pub tag_start: ByteOffset,
    pub tag_end: ByteOffset,
    pub value_start: ByteOffset,
    pub value_end: ByteOffset,
}

/// No FIX tag has 256 digits, so a longer run is junk rather than a tag. The
/// rule is what lets a decoded field store the tag length in a byte.
const MAX_TAG_DIGITS: usize = 255;

const SOH: u8 = 0x01;

pub fn tokenize(bytes: &[u8]) -> LineTokens {
    let separator = sniff_separator(bytes);

    let cursor = skip_blanks(bytes, 0);
    if cursor == bytes.len() {
        return empty(LineKind::Empty, separator);
    }
    if bytes[cursor] == b'#' {
        return empty(LineKind::Comment, separator);
    }

    let (fields, stopped_at) = scan_fields(bytes, 0, separator);
    // Nothing parsed from column 0: the line may be a log entry whose FIX
    // payload starts after a timestamp. Resynchronising only when column 0
    // yields nothing is what keeps a leading field from being eaten as prefix.
    let (fields, stopped_at, prefix_len) = if fields.is_empty() {
        match resync_offset(bytes, separator) {
            Some(offset) => {
                let (fields, stopped_at) = scan_fields(bytes, offset, separator);
                (fields, stopped_at, offset)
            }
            None => (fields, stopped_at, 0),
        }
    } else {
        (fields, stopped_at, 0)
    };

    let kind = if fields.is_empty() {
        LineKind::Other
    } else {
        LineKind::Message
    };
    LineTokens {
        kind,
        prefix_len: offset(prefix_len),
        fields,
        separator,
        trailing_junk_at: (stopped_at < bytes.len()).then_some(offset(stopped_at)),
    }
}

fn empty(kind: LineKind, separator: Option<u8>) -> LineTokens {
    LineTokens {
        kind,
        prefix_len: ByteOffset::new(0),
        fields: Vec::new(),
        separator,
        trailing_junk_at: None,
    }
}

/// The grammar allows `\x01`, `|` and `^` interchangeably within one message,
/// which mis-splits a `|` inside free text on a SOH-separated line. Pick one
/// per line instead, most-specific first.
fn sniff_separator(bytes: &[u8]) -> Option<u8> {
    // `contains` on a byte slice is memchr — vectorised, and it stops at the
    // first hit. A hand-rolled single pass over all three candidates reads the
    // whole line every time and is the slower answer, not the faster one.
    [SOH, b'|', b'^']
        .into_iter()
        .find(|candidate| bytes.contains(candidate))
}

/// First `8=` that starts the line or follows a byte a log prefix plausibly
/// ends with.
fn resync_offset(bytes: &[u8], separator: Option<u8>) -> Option<usize> {
    let at = bytes.windows(2).position(|pair| pair == b"8=")?;
    if at == 0 {
        return Some(at);
    }
    let before = bytes[at - 1];
    let anchored = matches!(before, b' ' | b'\t' | b'[' | b']' | b'>') || Some(before) == separator;
    anchored.then_some(at)
}

fn skip_blanks(bytes: &[u8], from: usize) -> usize {
    let mut cursor = from;
    while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
        cursor += 1;
    }
    cursor
}

/// Returns the fields read and the offset parsing stopped at.
fn scan_fields(bytes: &[u8], from: usize, separator: Option<u8>) -> (Vec<FieldSpan>, usize) {
    let mut fields = Vec::new();
    let mut cursor = from;
    loop {
        let field_start = skip_blanks(bytes, cursor);
        let Some(span) = read_field(bytes, field_start, separator) else {
            return (fields, field_start);
        };
        if fields.is_empty() {
            // Sized on the first hit rather than up front: `tokenize` probes
            // from offset 0 before resyncing past a log prefix, and that probe
            // must not allocate. Clamped because line length bounds the field
            // count poorly at both ends — one RawData value can be megabytes.
            fields.reserve((bytes.len().saturating_sub(from) / 8).clamp(16, 256));
        }
        cursor = span.value_end.index();
        fields.push(span);
        if cursor < bytes.len() && Some(bytes[cursor]) == separator {
            cursor += 1;
        } else {
            return (fields, cursor);
        }
    }
}

fn read_field(bytes: &[u8], start: usize, separator: Option<u8>) -> Option<FieldSpan> {
    let mut cursor = start;
    while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
        cursor += 1;
    }
    let tag_end = cursor;
    if tag_end == start || tag_end - start > MAX_TAG_DIGITS {
        return None;
    }

    // No whitespace around `=`. The grammar accepts `35 = D` through its
    // `extras`, but that is a side effect of applying extras between all tokens
    // rather than a designed allowance — the wire format is `tag=value<SOH>`,
    // and the large generated synthetic FIX corpus contains no such spacing.
    // The payoff is `value_start == tag_end + 1`, which a decoded field relies
    // on.
    if cursor >= bytes.len() || bytes[cursor] != b'=' {
        return None;
    }
    let value_start = cursor + 1;

    let mut value_end = value_start;
    while value_end < bytes.len() && Some(bytes[value_end]) != separator {
        value_end += 1;
    }

    Some(FieldSpan {
        tag: parse_tag(&bytes[start..tag_end]),
        tag_start: offset(start),
        tag_end: offset(tag_end),
        value_start: offset(value_start),
        value_end: offset(value_end),
    })
}

/// A position inside the line the tokenizer is walking, which scans with
/// `usize`.
///
/// **This truncates**, and nothing upstream stops it: no line-length limit
/// exists anywhere in the crate, so a line past `u32::MAX` would have every
/// offset on it wrapped. Said plainly rather than excused — an earlier comment
/// here claimed the codec's packing refused such a line, and it does not.
/// Work-order item 20 is where that gets decided; until then this is the
/// crate's oldest unstated premise, not a new one.
fn offset(at: usize) -> ByteOffset {
    ByteOffset::new(at as u32)
}

/// `u32::MAX` marks a run that does not fit; the span still stands.
fn parse_tag(digits: &[u8]) -> u32 {
    let mut tag: u32 = 0;
    for &digit in digits {
        match tag
            .checked_mul(10)
            .and_then(|shifted| shifted.checked_add(u32::from(digit - b'0')))
        {
            Some(next) => tag = next,
            None => return u32::MAX,
        }
    }
    tag
}

#[cfg(test)]
mod tests {
    use crate::codec::byte;

    use super::*;
    use proptest::prelude::*;

    fn spans(line: &str) -> Vec<(u32, u32, u32, u32, u32)> {
        tokenize(line.as_bytes())
            .fields
            .iter()
            .map(|field| {
                (
                    field.tag,
                    field.tag_start.raw(),
                    field.tag_end.raw(),
                    field.value_start.raw(),
                    field.value_end.raw(),
                )
            })
            .collect()
    }

    #[test]
    fn a_span_runs_from_the_tag_to_the_end_of_its_value() {
        assert_eq!(
            spans("8=FIX.4.4|9=123|35=W"),
            vec![(8, 0, 1, 2, 9), (9, 10, 11, 12, 15), (35, 16, 18, 19, 20)]
        );
    }

    #[test]
    fn each_separator_is_recognised_on_its_own() {
        for separator in ["\u{1}", "|", "^"] {
            let line = format!("8=FIX.4.4{separator}35=0{separator}");
            let tokenized = tokenize(line.as_bytes());
            assert_eq!(tokenized.kind, LineKind::Message, "sep {separator:?}");
            assert_eq!(tokenized.fields.len(), 2, "sep {separator:?}");
            assert_eq!(tokenized.separator, Some(separator.as_bytes()[0]));
        }
    }

    #[test]
    fn one_separator_per_line_protects_free_text() {
        // SOH wins, so the pipe inside Text(58) stays part of the value.
        let tokenized = tokenize("8=FIX.4.4\u{1}58=a|b\u{1}".as_bytes());
        assert_eq!(tokenized.separator, Some(SOH));
        assert_eq!(tokenized.fields.len(), 2);
        let value = &tokenized.fields[1];
        assert_eq!(value.value_end - value.value_start, 3, "\"a|b\"");

        // With no SOH present the pipe separates and `^` is plain text.
        let tokenized = tokenize(b"8=A|58=x^y|");
        assert_eq!(tokenized.separator, Some(b'|'));
        assert_eq!(tokenized.fields.len(), 2);
    }

    #[test]
    fn trailing_separator_is_optional() {
        assert_eq!(spans("35=0"), vec![(35, 0, 2, 3, 4)]);
        assert_eq!(spans("35=0|"), vec![(35, 0, 2, 3, 4)]);
    }

    #[test]
    fn empty_value_and_equals_inside_value() {
        assert_eq!(spans("35=|34=1"), vec![(35, 0, 2, 3, 3), (34, 4, 6, 7, 8)]);
        assert_eq!(spans("58=a=b"), vec![(58, 0, 2, 3, 6)]);
    }

    #[test]
    fn whitespace_before_a_tag_is_skipped() {
        // Pasting FIX into a ticket or a chat indents it; that must still parse.
        assert_eq!(spans("  8=FIX.4.4"), vec![(8, 2, 3, 4, 11)]);
    }

    #[test]
    fn whitespace_around_equals_is_not_a_field() {
        // Deliberate deviation from the grammar's `extras`, and the invariant a
        // decoded field's packing rests on.
        assert_eq!(tokenize(b" 8 = FIX.4.4").kind, LineKind::Other);
        assert_eq!(tokenize(b"8 =FIX.4.4").kind, LineKind::Other);
        assert_eq!(
            tokenize(b"8= FIX.4.4").fields[0].value_start,
            byte(2),
            "a space after `=` is value"
        );
    }

    #[test]
    fn line_kinds() {
        assert_eq!(tokenize(b"").kind, LineKind::Empty);
        assert_eq!(tokenize(b"   \t ").kind, LineKind::Empty);
        assert_eq!(
            tokenize(b"# fix: hover.enabled=false").kind,
            LineKind::Comment
        );
        assert_eq!(tokenize(b"  # indented").kind, LineKind::Comment);
        assert_eq!(tokenize(b"hello world").kind, LineKind::Other);
    }

    #[test]
    fn a_log_prefix_is_skipped_and_spans_stay_absolute() {
        let line = "2026-06-30 07:55:12 INFO [S1] 8=FIX.4.4|35=0|";
        let tokenized = tokenize(line.as_bytes());
        assert_eq!(tokenized.kind, LineKind::Message);
        assert_eq!(tokenized.prefix_len, byte(30));
        assert_eq!(
            tokenized.fields[0].tag_start,
            byte(30),
            "offsets stay absolute"
        );
        assert_eq!(tokenized.fields[0].tag, 8);
        assert_eq!(tokenized.fields.len(), 2);
    }

    #[test]
    fn a_leading_field_is_never_eaten_as_prefix() {
        // Resynchronising only when column 0 yields nothing: here it does, so
        // the anchored `8=` further along must not truncate the line.
        let tokenized = tokenize(b"9=57|8=FIX.4.4|35=0");
        assert_eq!(tokenized.prefix_len, byte(0));
        assert_eq!(tokenized.fields.len(), 3);
        assert_eq!(tokenized.fields[0].tag, 9);
    }

    #[test]
    fn without_an_anchored_tag_8_parsing_starts_at_zero() {
        // validation/fragment line 0 — a bare repeating-group block.
        assert_eq!(tokenize(b"268=2|269=0|270=100.25").fields.len(), 3);
        // malformed-missing-tag8.
        assert_eq!(tokenize(b"35=0|49=CLIENT1|").fields.len(), 2);
        // An `8=` that is not anchored does not trigger a skip.
        assert_eq!(tokenize(b"x8=FIX.4.4").kind, LineKind::Other);
    }

    #[test]
    fn trailing_junk_keeps_the_fields_before_it() {
        let tokenized = tokenize(b"8=FIX.4.4|and then prose");
        assert_eq!(tokenized.fields.len(), 1);
        assert_eq!(tokenized.trailing_junk_at, Some(byte(10)));
    }

    #[test]
    fn duplicate_tags_are_kept_in_order() {
        let parsed = spans("55=A|55=B");
        assert_eq!(parsed.len(), 2);
        assert_eq!((parsed[0].0, parsed[1].0), (55, 55));
    }

    #[test]
    fn an_overflowing_tag_keeps_its_span() {
        let tokenized = tokenize(b"8=FIX.4.4|99999999999=x|34=1");
        assert_eq!(tokenized.fields.len(), 3);
        assert_eq!(tokenized.fields[1].tag, u32::MAX);
        assert_eq!(tokenized.fields[2].tag, 34, "parsing continues past it");
    }

    #[test]
    fn a_digit_run_longer_than_255_is_not_a_tag() {
        // `[0-9]+` is unbounded in the grammar; no FIX tag has 256 digits, so
        // such a run is junk. A proptest over 300 random bytes would
        // essentially never generate this, so it is explicit.
        let line = format!("{}=x", "9".repeat(300));
        let tokenized = tokenize(line.as_bytes());
        assert_eq!(tokenized.kind, LineKind::Other);
        assert_eq!(tokenized.trailing_junk_at, Some(byte(0)));

        // 255 digits is still a (nonsensical, overflowing) tag.
        let line = format!("{}=x", "9".repeat(255));
        let tokenized = tokenize(line.as_bytes());
        assert_eq!(tokenized.fields.len(), 1);
        assert_eq!(tokenized.fields[0].tag, u32::MAX);
        assert_eq!(tokenized.fields[0].tag_end, byte(255));
    }

    #[test]
    fn non_utf8_bytes_are_tolerated() {
        let tokenized = tokenize(b"58=\xff\x80|34=1");
        assert_eq!(tokenized.fields.len(), 2);
        assert_eq!(
            tokenized.fields[0].value_end,
            byte(5),
            "the separator is not part of the value"
        );
    }

    proptest! {
        #[test]
        fn spans_are_well_formed(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
            let tokenized = tokenize(&bytes);
            let len = bytes.len();
            let mut previous_end = tokenized.prefix_len;
            for field in &tokenized.fields {
                prop_assert!(field.tag_start >= previous_end);
                prop_assert!(field.tag_start < field.tag_end);
                prop_assert!(field.tag_end - field.tag_start <= MAX_TAG_DIGITS as u32);
                prop_assert_eq!(field.value_start, field.tag_end + 1, "no whitespace around `=`");
                prop_assert_eq!(bytes[field.tag_end.index()], b'=');
                prop_assert!(field.value_start <= field.value_end);
                prop_assert!(field.value_end.index() <= len);
                previous_end = field.value_end;
            }
            prop_assert_eq!(tokenized.fields.is_empty(), tokenized.kind != LineKind::Message);
            if let Some(junk) = tokenized.trailing_junk_at {
                prop_assert!(junk.index() < len);
            }
        }
    }
}
