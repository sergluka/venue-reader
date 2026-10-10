//! Owned dictionary model. Shared with `build.rs` via `#[path]`, so this file
//! depends on nothing but `std`.

use std::fmt;

use super::schema::SchemaBuf;

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
    /// A space-separated list of characters (MultipleCharValue).
    MultipleChar,
    Text,
    /// A space-separated list of strings (MultipleStringValue).
    MultipleString,
    Data,
    #[default]
    Unknown,
}

impl Datatype {
    /// The one place that knows the spellings. Case-insensitive: the bundle
    /// writes `PRICE`/`CHAR`, hand-written dictionaries often `Price`/`char`.
    pub fn of(spelling: &str) -> Datatype {
        use Datatype::*;
        // `Stirng` and `LocalMMktDate` are typos seen in FIX Repository files
        // and carried into dictionaries generated from them,
        // `date`/`time` their shorthand, and the hyphenated spellings QuickFIX's
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
            ("MultipleCharValue", MultipleChar),
            ("String", Text),
            ("Stirng", Text),
            ("MultipleStringValue", MultipleString),
            ("MultipleValueString", MultipleString),
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
    /// Dates `bundle::enrich` takes from Orchestra for the bundled tables;
    /// QuickFIX XML has none, so a parsed dictionary leaves them empty.
    pub added: String,
    pub deprecated: String,
    /// The extension pack recorded against `added`, or 0 when none is.
    pub added_ep: u16,
    /// QuickFIX's `allowOtherValues`: the listed values are not the only ones.
    pub allows_other_values: bool,
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

/// A repeating group, parallel to its scope in `ParsedDict::schema`, which
/// holds its members.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct GroupDef {
    pub name: String,
    pub count_tag: u32,
    /// The first member; 0 for a group with none.
    pub delimiter_tag: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct MessageDef {
    pub msg_type: String,
    pub name: String,
    pub category: String,
    pub description: String,
}

/// A defect a custom dictionary still loads with. An error when the server
/// dropped or chose something the file says, a warning when nothing is lost.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Fault {
    /// Two or more `<message>` elements share a MsgType(35); their members
    /// are unioned under the first `name`. `others` holds the other distinct
    /// names, in file order.
    DoubledMessage {
        msg_type: String,
        name: String,
        definitions: usize,
        others: Vec<String>,
    },
    /// One scope lists a tag twice; the declarations are merged into one
    /// member. `within` is the scope, already rendered; `differs` when the
    /// declarations differ in shape, requiredness or chain, so one was chosen.
    DoubledTag {
        within: String,
        tag: u32,
        name: String,
        differs: bool,
    },
    /// One scope lists a counter both as a plain field and as a group; the
    /// group is kept. `within` as in `DoubledTag`.
    FieldAndGroup {
        within: String,
        tag: u32,
        name: String,
    },
    /// One name on two tag numbers; both tags stay defined, the name refers
    /// to `second`, the last one defined.
    FieldNameTwice {
        name: String,
        first: u32,
        second: u32,
    },
    /// A reference to a field name the file does not define; the member is
    /// left out. `within` is where the reference was made, already rendered.
    UndefinedField { within: String, name: String },
}

impl Fault {
    pub fn is_error(&self) -> bool {
        match self {
            Fault::DoubledMessage { others, .. } => !others.is_empty(),
            Fault::DoubledTag { differs, .. } => *differs,
            // The plain listing carries no structure the group lacks.
            Fault::FieldAndGroup { .. } => false,
            Fault::FieldNameTwice { .. } | Fault::UndefinedField { .. } => true,
        }
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::DoubledMessage {
                msg_type,
                name,
                definitions,
                others,
            } => {
                write!(f, "message {name}, MsgType(35) = {msg_type}, is defined ")?;
                match definitions {
                    2 => f.write_str("twice")?,
                    count => write!(f, "{count} times")?,
                }
                if others.is_empty() {
                    f.write_str("; merged into one")
                } else {
                    write!(
                        f,
                        ", also as {}; merged into one under {name}",
                        others.join(", ")
                    )
                }
            }
            Fault::DoubledTag {
                within, tag, name, ..
            } => write!(
                f,
                "{within}{} lists {name}({tag}) twice; merged into one",
                appositive_comma(within)
            ),
            Fault::FieldAndGroup { within, tag, name } => write!(
                f,
                "{within}{} lists {name}({tag}) as a plain field and as a group; the group is kept",
                appositive_comma(within)
            ),
            Fault::FieldNameTwice {
                name,
                first,
                second,
            } => write!(
                f,
                "field {name} is defined with two numbers, {name}({first}) and {name}({second}); \
                 the name refers to {name}({second})"
            ),
            Fault::UndefinedField { within, name } => write!(
                f,
                "{within} refers to field {name:?}, which the file does not define; skipped"
            ),
        }
    }
}

/// A message's label ends in its MsgType(35), set off by commas.
fn appositive_comma(within: &str) -> &'static str {
    if within.contains(", MsgType(35) = ") {
        ","
    } else {
        ""
    }
}

/// One parsed dictionary. `fields` is sorted by tag, `messages` by `msg_type`,
/// `groups` is a structurally deduplicated arena; `messages` and `groups` are
/// parallel to their scopes in `schema`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedDict {
    /// The version the XML declares about itself, if any.
    pub version: Option<String>,
    pub fields: Vec<FieldDef>,
    pub groups: Vec<GroupDef>,
    pub messages: Vec<MessageDef>,
    pub schema: SchemaBuf,
    /// Sorted and deduplicated, so a cached dictionary replays them as they were.
    pub faults: Vec<Fault>,
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

#[cfg(test)]
mod tests {
    use super::Datatype;

    #[test]
    fn multiple_value_spellings_keep_their_own_class() {
        for spelling in ["MultipleCharValue", "MULTIPLECHARVALUE"] {
            assert_eq!(Datatype::of(spelling), Datatype::MultipleChar, "{spelling}");
        }
        for spelling in [
            "MultipleStringValue",
            "MultipleValueString",
            "MULTIPLEVALUESTRING",
        ] {
            assert_eq!(
                Datatype::of(spelling),
                Datatype::MultipleString,
                "{spelling}"
            );
        }
    }
}
