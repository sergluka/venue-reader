//! What colour a byte range is, and the legend a client resolves it against.
//!
//! The legend's order is on the wire — a token names its type by index, not by
//! name — so each enum lists its variants once in `ALL` and spells them once in
//! `name`, and a `const` block builds the legend from the two while asserting
//! that the list is in discriminant order. Everything here is byte offsets; the
//! encoder converts.

use std::ops::Range;

use crate::codec::{ByteOffset, Field, LineAnalysis, MessageAnalysis};
use crate::dictionary::{Datatype, GroupIdx};

/// The names of an enum's `ALL`, and the proof that it is in discriminant order
/// — which is what a token's index on the wire means. A variant listed out of
/// place fails the build rather than a test. A `const fn` cannot take `name` as
/// an argument, hence a macro.
macro_rules! legend {
    ($of:ty) => {{
        let mut names = [""; <$of>::ALL.len()];
        let mut at = 0;
        while at < <$of>::ALL.len() {
            assert!(<$of>::ALL[at] as usize == at, "the legend is out of order");
            names[at] = <$of>::ALL[at].name();
            at += 1;
        }
        names
    }};
}

/// A token type, by its index in [`TYPES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Kind {
    Property,
    EnumMember,
    Number,
    /// Custom: a price per unit, worth telling apart from any other number.
    Price,
    /// Custom: a number of units.
    Quantity,
    /// Custom: an amount rather than a level.
    Money,
    String,
    Keyword,
    Namespace,
    Comment,
    /// Custom: no standard type describes a date or a time.
    Timestamp,
    /// Custom: the overlay covering a whole group entry.
    Group,
}

impl Kind {
    /// In discriminant order, which `TYPES` asserts.
    const ALL: [Kind; 12] = [
        Kind::Property,
        Kind::EnumMember,
        Kind::Number,
        Kind::Price,
        Kind::Quantity,
        Kind::Money,
        Kind::String,
        Kind::Keyword,
        Kind::Namespace,
        Kind::Comment,
        Kind::Timestamp,
        Kind::Group,
    ];

    /// The name a client maps to a colour. Exhaustive, so a new variant does
    /// not compile until it has one.
    pub const fn name(self) -> &'static str {
        match self {
            Kind::Property => "property",
            Kind::EnumMember => "enumMember",
            Kind::Number => "number",
            Kind::Price => "price",
            Kind::Quantity => "quantity",
            Kind::Money => "money",
            Kind::String => "string",
            Kind::Keyword => "keyword",
            Kind::Namespace => "namespace",
            Kind::Comment => "comment",
            Kind::Timestamp => "timestamp",
            Kind::Group => "group",
        }
    }
}

/// The legend's `tokenTypes`, in wire order.
pub const TYPES: [&str; Kind::ALL.len()] = legend!(Kind);

/// A token modifier, by its bit position in a token's modifier set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Mod {
    Deprecated,
    Header,
    Trailer,
    Session,
    PossDup,
    /// The tag is not in this line's dictionary.
    Unknown,
    /// The field counts the entries of a repeating group.
    Count,
    /// Depth, then alternating by entry, so adjacent entries differ.
    Group1a,
    Group1b,
    Group2a,
    Group2b,
    Group3a,
    Group3b,
}

impl Mod {
    /// In discriminant order, which `MODIFIERS` asserts.
    const ALL: [Mod; 13] = [
        Mod::Deprecated,
        Mod::Header,
        Mod::Trailer,
        Mod::Session,
        Mod::PossDup,
        Mod::Unknown,
        Mod::Count,
        Mod::Group1a,
        Mod::Group1b,
        Mod::Group2a,
        Mod::Group2b,
        Mod::Group3a,
        Mod::Group3b,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Mod::Deprecated => "deprecated",
            Mod::Header => "header",
            Mod::Trailer => "trailer",
            Mod::Session => "session",
            Mod::PossDup => "possDup",
            Mod::Unknown => "unknown",
            Mod::Count => "count",
            Mod::Group1a => "group1a",
            Mod::Group1b => "group1b",
            Mod::Group2a => "group2a",
            Mod::Group2b => "group2b",
            Mod::Group3a => "group3a",
            Mod::Group3b => "group3b",
        }
    }

    pub fn bit(self) -> u32 {
        1 << self as u32
    }

    /// The pair for one entry: depth chooses it, the entry number alternates
    /// within it, so two adjacent entries of one group never look the same.
    fn of_entry(depth: u16, entry: u32) -> Mod {
        const BY_DEPTH: [[Mod; 2]; DEEPEST_MARKED as usize] = [
            [Mod::Group1a, Mod::Group1b],
            [Mod::Group2a, Mod::Group2b],
            [Mod::Group3a, Mod::Group3b],
        ];
        // `entry` is 1-based, so an odd one is the first of its pair.
        BY_DEPTH[usize::from(depth.clamp(1, DEEPEST_MARKED) - 1)]
            [usize::from(entry.is_multiple_of(2))]
    }
}

/// The legend's `tokenModifiers`, in bit order.
pub const MODIFIERS: [&str; Mod::ALL.len()] = legend!(Mod);

/// Groups deeper than this share the deepest pair of modifiers; a client has to
/// map every name it colours, and the nesting a FIX log shows in practice is two.
const DEEPEST_MARKED: u16 = 3;

/// One coloured range. Never spans a line: a FIX message is one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub range: Range<ByteOffset>,
    pub kind: Kind,
    /// Bit set of [`Mod`].
    pub mods: u32,
}

/// Tokens of one line, appended in wire order.
///
/// `overlays` adds one token per group entry covering the whole entry, which is
/// only useful to a client that declared it can render overlapping tokens.
pub fn classify(line: &[u8], analysis: &LineAnalysis, overlays: bool, out: &mut Vec<Token>) {
    match analysis {
        LineAnalysis::Message(message) => message_tokens(line, message, overlays, out),
        LineAnalysis::Comment => out.push(Token {
            range: ByteOffset::new(0)..end_of(line),
            kind: Kind::Comment,
            mods: 0,
        }),
        LineAnalysis::Empty | LineAnalysis::Other => {}
    }
}

fn end_of(line: &[u8]) -> ByteOffset {
    ByteOffset::from_len(line.len()).unwrap_or(ByteOffset::new(u32::MAX))
}

fn message_tokens(line: &[u8], message: &MessageAnalysis, overlays: bool, out: &mut Vec<Token>) {
    if message.prefix_len > ByteOffset::new(0) {
        out.push(Token {
            range: ByteOffset::new(0)..message.prefix_len,
            kind: Kind::Comment,
            mods: 0,
        });
    }

    // Two facts about the whole line, so they are read once and OR'd into
    // everything rather than asked per field.
    let mut line_mods = 0;
    if message
        .msg_type
        .is_some_and(|idx| message.dict.message(idx).is_session())
    {
        line_mods |= Mod::Session.bit();
    }
    if message.is_poss_dup(line) {
        line_mods |= Mod::PossDup.bit();
    }

    // Only for the overlay, and only when one is wanted: the spans cost a
    // vector and a map per line, while "which field opens a group" is a cheaper
    // question asked of the instances directly.
    let spans = overlays.then(|| message.group_spans());
    let counters = counter_tags(message);
    // An entry's first field is the one that raises the highest instance index
    // seen so far: instances are numbered as they are opened, so returning to a
    // parent after a nested group never looks like a new entry.
    let mut deepest_seen: Option<u32> = None;

    for field in &message.fields {
        let mut mods = line_mods;
        match field.def() {
            Some(def) => {
                // `deprecated()` and not `lifecycle()`: the latter resolves two
                // more attributes and walks the presence masks for a third, all
                // to answer a question about one of them.
                if !message.dict.field(def).deprecated().is_empty() {
                    mods |= Mod::Deprecated.bit();
                }
            }
            None => mods |= Mod::Unknown.bit(),
        }
        if message.dict.is_header(field.tag) {
            mods |= Mod::Header.bit();
        } else if message.dict.is_trailer(field.tag) {
            mods |= Mod::Trailer.bit();
        }
        if counters.binary_search(&field.tag).is_ok() {
            mods |= Mod::Count.bit();
        }
        let instance = field.group_idx();
        if let Some(entry) = instance.map(|idx| &message.groups[idx as usize]) {
            mods |= Mod::of_entry(entry.depth, entry.entry).bit();
        }

        out.push(Token {
            range: field.tag_start..field.tag_end(),
            kind: Kind::Property,
            mods,
        });

        // After the tag, never before it: a client that declared overlap and
        // does not render it then drops the overlay rather than the field.
        if overlays {
            if let Some(idx) = instance.filter(|idx| deepest_seen < Some(*idx)) {
                deepest_seen = Some(idx);
                if let Some(range) = spans.as_ref().and_then(|spans| spans.instance(idx)) {
                    out.push(Token {
                        range,
                        kind: Kind::Group,
                        mods,
                    });
                }
            }
        }

        if field.value_start() < field.value_end() {
            out.push(Token {
                range: field.value_start()..field.value_end(),
                kind: value_kind(message, field),
                mods,
            });
        }
    }
}

/// Which tags open a group in this message, sorted.
///
/// From the **dictionary**, not from what the line instantiated: `268=0` opens
/// no entries and is still the field that counts them. A message type the
/// dictionary does not know declares nothing, which is what it can honestly
/// say.
fn counter_tags(message: &MessageAnalysis) -> Vec<u32> {
    let Some(msg) = message.msg_type else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut stack: Vec<GroupIdx> = message.dict.message(msg).groups().collect();
    while let Some(idx) = stack.pop() {
        let group = message.dict.group(idx);
        out.push(group.count_tag());
        stack.extend(group.nested());
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The special tags come first on purpose: a QuickFIX dictionary synthesises an
/// enum for every message type, so resolving enums first would leave nothing
/// spelled as a keyword.
fn value_kind(message: &MessageAnalysis, field: &Field) -> Kind {
    match field.tag {
        35 => return Kind::Keyword,
        8 => return Kind::Namespace,
        _ => {}
    }
    if field.enum_value().is_some() {
        return Kind::EnumMember;
    }
    match field.def() {
        Some(def) => of_datatype(message.dict.field(def).datatype_class()),
        None => Kind::String,
    }
}

/// The second half of the mapping the dictionary starts: an XML datatype has a
/// class, and a class has a token type.
fn of_datatype(class: Datatype) -> Kind {
    match class {
        Datatype::Numeric | Datatype::NumInGroup => Kind::Number,
        Datatype::Price => Kind::Price,
        Datatype::Quantity => Kind::Quantity,
        Datatype::Money => Kind::Money,
        Datatype::Timestamp => Kind::Timestamp,
        Datatype::Boolean
        | Datatype::Character
        | Datatype::Text
        | Datatype::Data
        | Datatype::Unknown => Kind::String,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::{analyze, Ctx};

    /// The wire says "type 4", not "keyword", so a reordering here silently
    /// recolours every client. Written out rather than derived from the enums.
    #[test]
    fn the_legend_is_the_order_on_the_wire() {
        assert_eq!(
            TYPES,
            [
                "property",
                "enumMember",
                "number",
                "price",
                "quantity",
                "money",
                "string",
                "keyword",
                "namespace",
                "comment",
                "timestamp",
                "group",
            ]
        );
        assert_eq!(
            MODIFIERS,
            [
                "deprecated",
                "header",
                "trailer",
                "session",
                "possDup",
                "unknown",
                "count",
                "group1a",
                "group1b",
                "group2a",
                "group2b",
                "group3a",
                "group3b",
            ]
        );
        assert_eq!(Kind::Property as u32, 0);
        assert_eq!(Kind::Group as u32, TYPES.len() as u32 - 1);
        assert_eq!(TYPES[Kind::Price as usize], "price");
        assert_eq!(Mod::Deprecated.bit(), 1);
        assert_eq!(Mod::Group3b.bit(), 1 << (MODIFIERS.len() - 1));
    }

    fn tokens(line: &str, overlays: bool) -> (Vec<u8>, Vec<Token>) {
        let bytes = line.as_bytes().to_vec();
        let analysis = analyze(&bytes, &Ctx::for_test("FIX.4.4"));
        let mut out = Vec::new();
        classify(&bytes, &analysis, overlays, &mut out);
        (bytes, out)
    }

    /// Every token as `text:type:modifiers`, which is what a reader of a failing
    /// assertion actually wants to see.
    fn spelled(line: &str, overlays: bool) -> Vec<String> {
        let (bytes, out) = tokens(line, overlays);
        out.iter()
            .map(|token| {
                let text = String::from_utf8_lossy(
                    &bytes[token.range.start.index()..token.range.end.index()],
                );
                let mods: Vec<&str> = MODIFIERS
                    .iter()
                    .enumerate()
                    .filter(|(bit, _)| token.mods & (1 << bit) != 0)
                    .map(|(_, name)| *name)
                    .collect();
                format!("{text}:{}:{}", TYPES[token.kind as usize], mods.join("+"))
            })
            .collect()
    }

    #[test]
    fn a_tag_is_a_property_and_its_value_takes_the_datatype() {
        assert_eq!(
            spelled("8=FIX.4.4|44=1.5|60=20251026-09:02:00|55=AAPL", false),
            [
                "8:property:header",
                "FIX.4.4:namespace:header",
                "44:property:",
                "1.5:price:",
                "60:property:",
                "20251026-09:02:00:timestamp:",
                "55:property:",
                "AAPL:string:",
            ]
        );
    }

    /// A price, a quantity and a money amount are three quantities that share a
    /// numeric shape; a sequence number is the plain one they are told from.
    #[test]
    fn money_and_quantity_are_not_the_same_number_as_a_price() {
        assert_eq!(
            spelled("8=FIX.4.4|34=7|31=1.5|32=100|12=2.75|451=-1.25", false),
            [
                "8:property:header",
                "FIX.4.4:namespace:header",
                "34:property:header",
                "7:number:header",
                "31:property:",
                "1.5:price:",
                "32:property:",
                "100:quantity:",
                "12:property:",
                "2.75:money:",
                "451:property:",
                "-1.25:money:",
            ]
        );
    }

    /// A QuickFIX dictionary synthesises an enum for every message type, so
    /// enum-first would leave nothing spelled as a keyword.
    #[test]
    fn the_message_type_is_a_keyword_and_not_an_enum_member() {
        let out = spelled("8=FIX.4.4|35=D|54=1", false);
        assert!(out.contains(&"D:keyword:header".to_owned()), "{out:?}");
        assert!(out.contains(&"1:enumMember:".to_owned()), "{out:?}");
    }

    #[test]
    fn a_session_message_marks_every_token_and_a_poss_dup_too() {
        let out = spelled("8=FIX.4.4|35=0|43=Y", false);
        for token in &out {
            assert!(token.contains("session"), "{token}");
            assert!(token.contains("possDup"), "{token}");
        }
    }

    #[test]
    fn the_session_layer_marks_the_tag_it_belongs_to() {
        let out = spelled("8=FIX.4.4|34=1|55=AAPL|10=171", false);
        assert!(out.contains(&"34:property:header".to_owned()), "{out:?}");
        assert!(out.contains(&"55:property:".to_owned()), "{out:?}");
        assert!(out.contains(&"10:property:trailer".to_owned()), "{out:?}");
    }

    /// The counter is marked, and adjacent entries of one group alternate so a
    /// reader can see where one ends and the next begins.
    #[test]
    fn entries_of_one_group_alternate_and_the_counter_says_so() {
        let out = spelled("8=FIX.4.4|35=W|268=2|269=0|270=1.5|269=1|270=1.6", false);
        assert!(out.contains(&"268:property:count".to_owned()), "{out:?}");
        assert!(out.contains(&"269:property:group1a".to_owned()), "{out:?}");
        assert!(out.contains(&"270:property:group1a".to_owned()), "{out:?}");
        assert!(out.contains(&"269:property:group1b".to_owned()), "{out:?}");
        assert!(out.contains(&"270:property:group1b".to_owned()), "{out:?}");
    }

    /// A counter that opened no entries is still the field that counts them,
    /// and a count larger than the entries present is still a count.
    #[test]
    fn a_group_counter_is_marked_whatever_its_value_says() {
        let empty = spelled("8=FIX.4.4|35=W|268=0|55=X", false);
        assert!(
            empty.contains(&"268:property:count".to_owned()),
            "{empty:?}"
        );
        assert!(empty.contains(&"0:number:count".to_owned()), "{empty:?}");
        assert!(empty.contains(&"55:property:".to_owned()), "{empty:?}");

        let short = spelled("8=FIX.4.4|35=W|268=9|269=0|270=1.5", false);
        assert!(
            short.contains(&"268:property:count".to_owned()),
            "{short:?}"
        );
    }

    /// The overlay follows the entry's first tag, so a client that declared
    /// overlap and does not render it drops the overlay, not the field.
    #[test]
    fn an_overlay_covers_a_whole_entry_and_never_precedes_it() {
        let line = "8=FIX.4.4|35=W|268=2|269=0|270=1.5|269=1|270=1.6";
        let plain = spelled(line, false);
        let with = spelled(line, true);
        assert_eq!(with.len(), plain.len() + 2, "one overlay per entry");
        let overlays: Vec<&String> = with.iter().filter(|t| t.contains(":group:")).collect();
        assert_eq!(
            overlays,
            [
                &"269=0|270=1.5:group:group1a".to_owned(),
                &"269=1|270=1.6:group:group1b".to_owned(),
            ]
        );
        let first = with
            .iter()
            .position(|t| t.contains(":group:"))
            .expect("an overlay");
        assert_eq!(with[first - 1], "269:property:group1a");
    }

    #[test]
    fn an_unknown_tag_is_marked_and_its_value_stays_text() {
        let out = spelled("8=FIX.4.4|9999=x", false);
        assert!(out.contains(&"9999:property:unknown".to_owned()), "{out:?}");
        assert!(out.contains(&"x:string:unknown".to_owned()), "{out:?}");
    }

    #[test]
    fn a_log_prefix_and_a_comment_line_are_both_comments() {
        let out = spelled("09:02:00.123 8=FIX.4.4|35=0", false);
        assert_eq!(out[0], "09:02:00.123 :comment:");
        assert_eq!(spelled("# a note", false), ["# a note:comment:"]);
        assert!(spelled("", false).is_empty());
    }

    #[test]
    fn an_empty_value_emits_no_token() {
        let out = spelled("8=FIX.4.4|55=|56=X", false);
        assert!(out.contains(&"55:property:".to_owned()), "{out:?}");
        assert!(!out.iter().any(|t| t.starts_with(":string")), "{out:?}");
    }

    #[test]
    fn depth_beyond_the_marked_pairs_reuses_the_deepest() {
        assert_eq!(Mod::of_entry(1, 1), Mod::Group1a);
        assert_eq!(Mod::of_entry(1, 2), Mod::Group1b);
        assert_eq!(Mod::of_entry(3, 1), Mod::Group3a);
        assert_eq!(Mod::of_entry(9, 2), Mod::Group3b);
    }
}
