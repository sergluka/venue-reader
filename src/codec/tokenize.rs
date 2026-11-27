//! Splitting a line into `tag=value` spans.
//!
//! Accepts the token language of the tree-sitter grammar the editor side uses,
//! but is line-anchored and resynchronising rather than whole-file: one
//! separator per line, a log prefix skipped when nothing parses from column 0,
//! and trailing junk recorded instead of rejecting the line.

use super::ByteOffset;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SkipReason {
    #[error("Skipped FIX analysis: line exceeds fix.limits.lineBytes ({0} bytes)")]
    LineBytes(u32),
    #[error("Skipped FIX analysis: line exceeds fix.limits.fieldsPerLine ({0} fields)")]
    FieldsPerLine(u32),
}

pub(super) fn check_line_bytes(length: usize, limit: u32) -> Result<(), SkipReason> {
    if u32::try_from(length).is_ok_and(|length| length <= limit) {
        Ok(())
    } else {
        Err(SkipReason::LineBytes(limit))
    }
}

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
    Skipped(SkipReason),
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
    tokenize_bounded(bytes, u32::MAX, u32::MAX)
}

pub fn tokenize_bounded(bytes: &[u8], line_bytes: u32, fields_per_line: u32) -> LineTokens {
    scan_bounded::<true>(bytes, line_bytes, fields_per_line)
}

pub(crate) fn line_kind(bytes: &[u8], line_bytes: u32, fields_per_line: u32) -> LineKind {
    scan_bounded::<false>(bytes, line_bytes, fields_per_line).kind
}

fn scan_bounded<const COLLECT_SPANS: bool>(
    bytes: &[u8],
    line_bytes: u32,
    fields_per_line: u32,
) -> LineTokens {
    check_line_bytes(bytes.len(), line_bytes)
        .and_then(|()| scan_line::<COLLECT_SPANS>(bytes, fields_per_line))
        .unwrap_or_else(|reason| empty(LineKind::Skipped(reason), None))
}

pub(crate) fn check_limits(
    bytes: &[u8],
    line_bytes: u32,
    fields_per_line: u32,
) -> Result<(), SkipReason> {
    check_line_bytes(bytes.len(), line_bytes)?;
    // An overflowing line needs at least `1=|` per admitted field, then `1=`.
    if (bytes.len() as u64) < u64::from(fields_per_line) * 3 + 2 {
        return Ok(());
    }
    scan_line::<false>(bytes, fields_per_line).map(|_| ())
}

fn scan_line<const COLLECT_SPANS: bool>(
    bytes: &[u8],
    fields_per_line: u32,
) -> Result<LineTokens, SkipReason> {
    let separator = sniff_separator(bytes);

    let cursor = skip_blanks(bytes, 0);
    if cursor == bytes.len() {
        return Ok(empty(LineKind::Empty, separator));
    }
    if bytes[cursor] == b'#' {
        return Ok(empty(LineKind::Comment, separator));
    }

    let mut tokens = empty(LineKind::Other, separator);
    let mut stopped_at = scan_fields::<COLLECT_SPANS>(bytes, 0, fields_per_line, &mut tokens)?;
    // A leading field takes precedence over a later anchored BeginString.
    if tokens.kind == LineKind::Other {
        if let Some(prefix) = resync_offset(bytes, separator) {
            tokens.prefix_len = offset(prefix);
            stopped_at = scan_fields::<COLLECT_SPANS>(bytes, prefix, fields_per_line, &mut tokens)?;
        }
    }
    tokens.trailing_junk_at = (stopped_at < bytes.len()).then_some(offset(stopped_at));
    Ok(tokens)
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

fn scan_fields<const COLLECT_SPANS: bool>(
    bytes: &[u8],
    from: usize,
    limit: u32,
    tokens: &mut LineTokens,
) -> Result<usize, SkipReason> {
    let mut count = 0;
    let mut cursor = from;
    loop {
        let start = skip_blanks(bytes, cursor);
        let Some(tag_end) = tag_end(bytes, start) else {
            return Ok(start);
        };
        if count == limit {
            return Err(SkipReason::FieldsPerLine(limit));
        }
        let value_start = tag_end + 1;
        let value_end = tokens
            .separator
            .and_then(|separator| {
                bytes[value_start..]
                    .iter()
                    .position(|&byte| byte == separator)
            })
            .map_or(bytes.len(), |relative| value_start + relative);
        if COLLECT_SPANS {
            if count == 0 {
                // A failed prefix probe must not allocate. One raw value may occupy the entire line.
                tokens.fields.reserve(
                    (bytes.len().saturating_sub(from) / 8)
                        .clamp(16, 256)
                        .min(limit as usize),
                );
            }
            tokens.fields.push(FieldSpan {
                tag: parse_tag(&bytes[start..tag_end]),
                tag_start: offset(start),
                tag_end: offset(tag_end),
                value_start: offset(value_start),
                value_end: offset(value_end),
            });
        }
        tokens.kind = LineKind::Message;
        count += 1;
        if value_end == bytes.len() {
            return Ok(value_end);
        }
        cursor = value_end + 1;
    }
}

fn tag_end(bytes: &[u8], start: usize) -> Option<usize> {
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
    Some(tag_end)
}

fn offset(at: usize) -> ByteOffset {
    ByteOffset::from_len(at).expect("the full line fits in a ByteOffset")
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

    #[test]
    fn byte_limit_counts_prefix_and_multibyte_values_before_reading_fields() {
        let bytes = "prefix 8=FIX.4.4|58=é|".as_bytes();
        let length = u32::try_from(bytes.len()).unwrap();
        for limit in [length, length + 1] {
            assert_eq!(tokenize_bounded(bytes, limit, 2).fields.len(), 2);
        }
        let over = tokenize_bounded(bytes, length - 1, 0);
        assert_eq!(
            over.kind,
            LineKind::Skipped(SkipReason::LineBytes(length - 1))
        );
        assert!(over.fields.is_empty());
        assert_eq!(over.separator, None);
        assert_eq!(tokenize_bounded(b"", 0, 0).kind, LineKind::Empty);
        assert_eq!(
            tokenize_bounded(b"#", 0, 0).kind,
            LineKind::Skipped(SkipReason::LineBytes(0))
        );
    }

    #[test]
    fn the_scanner_stops_at_the_first_excess_field_including_after_a_prefix() {
        for separator in [b'|', b'^', 1] {
            for prefix in ["", "[log] "] {
                let line = format!("{prefix}8=FIX.4.4|58=|58=value|junk")
                    .replace('|', &char::from(separator).to_string());
                let bytes = line.as_bytes();
                for limit in [3, 4] {
                    let tokens = tokenize_bounded(bytes, u32::MAX, limit);
                    assert_eq!(tokens.fields.len(), 3);
                    assert!(tokens.trailing_junk_at.is_some());
                }
                for limit in [0, 2] {
                    let tokens = tokenize_bounded(bytes, u32::MAX, limit);
                    assert_eq!(
                        tokens.kind,
                        LineKind::Skipped(SkipReason::FieldsPerLine(limit))
                    );
                    assert!(tokens.fields.is_empty());
                    assert_eq!(tokens.trailing_junk_at, None);
                    assert_eq!(tokens.prefix_len.index(), 0);
                }
            }
        }
        assert_eq!(
            tokenize_bounded(b"prose", u32::MAX, 0).kind,
            LineKind::Other
        );
        let mut long_value = b"58=".to_vec();
        long_value.resize(2 * 1024 * 1024, b'A');
        assert_eq!(tokenize_bounded(&long_value, u32::MAX, 1).fields.len(), 1);
    }

    #[test]
    fn limit_checks_match_tokenization_without_retaining_spans() {
        for bytes in [
            &b""[..],
            b"1=",
            b"1=|1=",
            b"1=|1=|1=",
            b"  1=|1=|junk|1=",
            b"  # 1=|1=|1=",
            b"log [S] 8=^1=^1=^",
            b"logx8=|1=|1=",
            b"1=|8=\x011=^|\x011=",
            b"21474836481=|1=",
            b"1=\xff|1=\x80",
        ] {
            let length = bytes.len() as u32;
            for line_limit in [0, length.saturating_sub(1), length, u32::MAX] {
                for field_limit in [0, 1, 2, 3, u32::MAX] {
                    let kind = tokenize_bounded(bytes, line_limit, field_limit).kind;
                    assert_eq!(line_kind(bytes, line_limit, field_limit), kind);
                    let expected = match kind {
                        LineKind::Skipped(reason) => Err(reason),
                        _ => Ok(()),
                    };
                    assert_eq!(
                        check_limits(bytes, line_limit, field_limit),
                        expected,
                        "{bytes:?}, bytes={line_limit}, fields={field_limit}"
                    );
                }
            }
            assert_eq!(
                scan_line::<false>(bytes, u32::MAX)
                    .unwrap()
                    .fields
                    .capacity(),
                0
            );
        }
    }

    #[test]
    fn line_bound_is_inclusive_and_also_bounds_standalone_offsets() {
        assert_eq!(check_line_bytes(u32::MAX as usize, u32::MAX), Ok(()));
        if let Some(too_long) = (u32::MAX as usize).checked_add(1) {
            assert_eq!(
                check_line_bytes(too_long, u32::MAX),
                Err(SkipReason::LineBytes(u32::MAX))
            );
        }
    }

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
