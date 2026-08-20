//! Owned dictionary model. Shared with `build.rs` via `#[path]`, so this file
//! depends on nothing but `std`.

/// What a value of a field looks like, normalised from the XML spelling once
/// so nothing has to compare thirty strings per token at request time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Datatype {
    Numeric,
    /// A price per unit, and the offsets read against one.
    Price,
    /// A number of units.
    Quantity,
    /// An amount rather than a level: a total (`Commission`, `NetMoney`) or a
    /// difference read against a price (`Spread`, forward points).
    Money,
    /// Distinct from `Numeric`: it is what marks a field as a group counter.
    NumInGroup,
    Timestamp,
    Boolean,
    Character,
    Text,
    Data,
    #[default]
    Unknown,
}

impl Datatype {
    /// The one place that knows the spellings. Case-insensitive: the bundle
    /// writes `PRICE`/`CHAR`, hand-written dictionaries often `Price`/`char`.
    pub fn of(spelling: &str) -> Datatype {
        use Datatype::*;
        // `Stirng` and `LocalMMktDate` are typos seen in FIX Repository files,
        // `date`/`time` its shorthand, and the hyphenated spellings QuickFIX's
        // FIX 4.0/4.1; all are data, not mistakes to fix.
        const SPELLINGS: &[(&str, Datatype)] = &[
            ("int", Numeric),
            ("float", Numeric),
            ("SeqNum", Numeric),
            ("Length", Numeric),
            ("Percentage", Numeric),
            ("DayOfMonth", Numeric),
            ("DAY-OF-MONTH", Numeric),
            ("Price", Price),
            ("Qty", Quantity),
            ("Amt", Money),
            ("PriceOffset", Money),
            ("NumInGroup", NumInGroup),
            ("UTCTimestamp", Timestamp),
            ("UTCDate", Timestamp),
            ("UTCDateOnly", Timestamp),
            ("UTCTimeOnly", Timestamp),
            ("TZTimestamp", Timestamp),
            ("TZTimeOnly", Timestamp),
            ("LocalMktDate", Timestamp),
            ("LocalMMktDate", Timestamp),
            ("MonthYear", Timestamp),
            ("MONTH-YEAR", Timestamp),
            ("date", Timestamp),
            ("time", Timestamp),
            ("Boolean", Boolean),
            ("char", Character),
            ("MultipleCharValue", Character),
            ("String", Text),
            ("Stirng", Text),
            ("MultipleStringValue", Text),
            ("MultipleValueString", Text),
            ("Currency", Text),
            ("Country", Text),
            ("Exchange", Text),
            ("Language", Text),
            ("data", Data),
            ("XMLData", Data),
        ];
        SPELLINGS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(spelling))
            .map_or(Unknown, |(_, class)| *class)
    }
}

/// A field definition with its enum values nested inside.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct FieldDef {
    pub tag: u32,
    pub name: String,
    pub datatype: String,
    /// The class of the version's own type. Usually `Datatype::of(datatype)`,
    /// but the bundle shows FIX Latest's spelling beside the version's class.
    pub datatype_class: Datatype,
    pub description: String,
    /// XML `added`/`deprecated` attributes; empty when absent (QuickFIX never has them).
    pub added: String,
    pub deprecated: String,
    /// The extension pack recorded against `added`, or 0 when none is — which
    /// is every parsed dictionary, since only Orchestra records one.
    pub added_ep: u16,
    /// Sorted by `value`.
    pub enums: Vec<EnumDef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct EnumDef {
    pub value: String,
    pub name: String,
    pub description: String,
    pub added: String,
    /// As `FieldDef::added_ep`.
    pub added_ep: u16,
}

/// A repeating group. `nested` holds arena indices into `ParsedDict::groups`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct GroupDef {
    pub name: String,
    pub count_tag: u32,
    pub delimiter_tag: u32,
    /// Direct member tags in definition order, nested groups by their count tag.
    pub members: Vec<u32>,
    pub nested: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct MessageDef {
    pub msg_type: String,
    pub name: String,
    pub category: String,
    pub description: String,
    /// Top-level groups of the message, as arena indices.
    pub groups: Vec<u16>,
}

/// One parsed dictionary. `fields` is sorted by tag, `messages` by `msg_type`,
/// `groups` is a structurally deduplicated arena.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedDict {
    /// The version the XML declares about itself, if any.
    pub version: Option<String>,
    pub fields: Vec<FieldDef>,
    /// Session-layer tags, sorted. Empty when the source declares no header.
    pub header: Vec<u32>,
    pub trailer: Vec<u32>,
    pub groups: Vec<GroupDef>,
    pub messages: Vec<MessageDef>,
}

impl ParsedDict {
    pub fn field_index(&self, tag: u32) -> Option<usize> {
        self.fields.binary_search_by_key(&tag, |f| f.tag).ok()
    }

    pub fn message_index(&self, msg_type: &str) -> Option<usize> {
        self.messages
            .binary_search_by(|m| m.msg_type.as_str().cmp(msg_type))
            .ok()
    }

    pub fn field(&self, tag: u32) -> Option<&FieldDef> {
        self.field_index(tag).map(|i| &self.fields[i])
    }

    pub fn message(&self, msg_type: &str) -> Option<&MessageDef> {
        self.message_index(msg_type).map(|i| &self.messages[i])
    }
}
