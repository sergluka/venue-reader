//! The scoped message schema: every scope's members in one flat, pointer-free
//! table. Shared with `build.rs` via `#[path]`, so this file depends on nothing
//! but `std`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;

/// The reserved index: no group, no component row, no parent.
pub const NONE: u16 = u16::MAX;

/// A group's place in the schema's arena.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct GroupIdx(pub(crate) u16);

/// A message's place in the schema, in MsgType(35) order.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct MsgIdx(pub(crate) u16);

// Row constructors for the tables `build.rs` generates: short calls keep
// `dicts.rs` small.

pub const fn span(
    start: u32,
    len: u16,
    nested_start: u32,
    nested_len: u16,
    components_start: u32,
    components_len: u16,
) -> ScopeSpan {
    ScopeSpan {
        start,
        len,
        pad: 0,
        nested_start,
        nested_len,
        components_len,
        components_start,
    }
}

pub const fn member(position: u16, group: u16, component: u16, flags: u16) -> Member {
    Member {
        position,
        group,
        component,
        flags,
    }
}

pub const fn row(name: u16, parent: u16, flags: u16) -> ComponentRow {
    ComponentRow {
        name,
        parent,
        flags,
    }
}

/// Where one scope's rows sit in the shared arrays of a [`Schema`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ScopeSpan {
    /// Into `tags`, `members` and `order`.
    pub start: u32,
    pub len: u16,
    /// Keeps the 20-byte layout explicit.
    pub pad: u16,
    pub nested_start: u32,
    pub nested_len: u16,
    pub components_len: u16,
    pub components_start: u32,
}

/// One member of a scope, found by its tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Member {
    /// Declared position in the scope.
    pub position: u16,
    /// Group arena index, [`NONE`] for a plain field.
    pub group: u16,
    /// Row in this scope's component table, [`NONE`] when declared directly.
    pub component: u16,
    pub flags: u16,
}

impl Member {
    pub const REQUIRED: u16 = 1;
    /// Declared through more than one component, so it activates none of them.
    pub const AMBIGUOUS: u16 = 2;
    /// May appear more than once (`repeating="Y"`).
    pub const REPEATING: u16 = 4;

    pub fn group(self) -> Option<u16> {
        index(self.group)
    }

    pub fn required(self) -> bool {
        self.flags & Self::REQUIRED != 0
    }

    pub fn ambiguous(self) -> bool {
        self.flags & Self::AMBIGUOUS != 0
    }

    pub fn repeating(self) -> bool {
        self.flags & Self::REPEATING != 0
    }
}

/// One component a scope expands, in a per-scope table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComponentRow {
    /// Into [`Schema::names`].
    pub name: u16,
    /// An earlier row of the same scope, [`NONE`] at the top.
    pub parent: u16,
    pub flags: u16,
}

impl ComponentRow {
    pub const REQUIRED: u16 = 1;
}

const _: () = assert!(std::mem::size_of::<ScopeSpan>() == 20);
const _: () = assert!(std::mem::size_of::<Member>() == 8);
const _: () = assert!(std::mem::size_of::<ComponentRow>() == 6);

/// The packed schema; spans are `[header, trailer, messages.., groups..]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schema<'a> {
    pub spans: &'a [ScopeSpan],
    /// Strictly ascending inside each span.
    pub tags: &'a [u32],
    /// Parallel to `tags`.
    pub members: &'a [Member],
    /// Per span: declared position -> index into the span's sorted arrays.
    pub order: &'a [u16],
    /// Per span: the indices of its group members, in declared order.
    pub nested: &'a [u16],
    pub components: &'a [ComponentRow],
    /// Borrowed in the static tables, owned at runtime.
    pub names: &'a [Cow<'static, str>],
    pub messages: u16,
}

/// The owned form of a [`Schema`]; built only by [`pack`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaBuf {
    pub spans: Vec<ScopeSpan>,
    pub tags: Vec<u32>,
    pub members: Vec<Member>,
    pub order: Vec<u16>,
    pub nested: Vec<u16>,
    pub components: Vec<ComponentRow>,
    pub names: Vec<Cow<'static, str>>,
    pub messages: u16,
}

/// An empty header and trailer and nothing else, so every query is valid.
impl Default for SchemaBuf {
    fn default() -> Self {
        pack(&SchemaDefs::default()).expect("an empty schema packs")
    }
}

impl SchemaBuf {
    pub fn as_schema(&self) -> Schema<'_> {
        Schema {
            spans: &self.spans,
            tags: &self.tags,
            members: &self.members,
            order: &self.order,
            nested: &self.nested,
            components: &self.components,
            names: &self.names,
            messages: self.messages,
        }
    }
}

impl<'a> Schema<'a> {
    pub fn header(&self) -> Scope<'a> {
        self.scope(0)
    }

    pub fn trailer(&self) -> Scope<'a> {
        self.scope(1)
    }

    pub fn message(&self, message: u16) -> Scope<'a> {
        self.scope(2 + usize::from(message))
    }

    pub fn group(&self, group: u16) -> Scope<'a> {
        self.scope(self.groups_start() + usize::from(group))
    }

    pub fn to_buf(self) -> SchemaBuf {
        SchemaBuf {
            spans: self.spans.to_vec(),
            tags: self.tags.to_vec(),
            members: self.members.to_vec(),
            order: self.order.to_vec(),
            nested: self.nested.to_vec(),
            components: self.components.to_vec(),
            names: self.names.to_vec(),
            messages: self.messages,
        }
    }

    /// The input [`pack`] would rebuild this schema from.
    pub fn to_defs(self) -> SchemaDefs {
        SchemaDefs {
            header: self.header().to_def(),
            trailer: self.trailer().to_def(),
            messages: (0..self.messages)
                .map(|message| self.message(message).to_def())
                .collect(),
            groups: (self.groups_start()..self.spans.len())
                .map(|span| self.scope(span).to_def())
                .collect(),
        }
    }

    fn groups_start(&self) -> usize {
        2 + usize::from(self.messages)
    }

    fn scope(&self, span: usize) -> Scope<'a> {
        let span = self.spans[span];
        let start = span.start as usize;
        let end = start + usize::from(span.len);
        let nested = span.nested_start as usize;
        let components = span.components_start as usize;
        Scope {
            tags: &self.tags[start..end],
            members: &self.members[start..end],
            order: &self.order[start..end],
            nested: &self.nested[nested..nested + usize::from(span.nested_len)],
            components: &self.components[components..components + usize::from(span.components_len)],
            names: self.names,
        }
    }
}

/// One header, trailer, message body or group entry.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    tags: &'a [u32],
    members: &'a [Member],
    order: &'a [u16],
    nested: &'a [u16],
    components: &'a [ComponentRow],
    names: &'a [Cow<'static, str>],
}

impl<'a> Scope<'a> {
    pub fn find(&self, tag: u32) -> Option<Member> {
        let index = self.tags.binary_search(&tag).ok()?;
        Some(self.members[index])
    }

    /// Tag and member, in declared order.
    pub fn declared(&self) -> impl Iterator<Item = (u32, Member)> + 'a {
        let (tags, members) = (self.tags, self.members);
        self.order
            .iter()
            .map(move |&i| (tags[usize::from(i)], members[usize::from(i)]))
    }

    /// The members that open a group, in declared order.
    pub fn groups(&self) -> impl Iterator<Item = (u32, Member)> + 'a {
        let (tags, members) = (self.tags, self.members);
        self.nested
            .iter()
            .map(move |&i| (tags[usize::from(i)], members[usize::from(i)]))
    }

    /// The innermost component `member` came through, if any.
    pub fn component(&self, member: Member) -> Option<ComponentRef<'a>> {
        Some(ComponentRef {
            rows: self.components,
            names: self.names,
            row: index(member.component)?,
        })
    }

    /// Whether `tag` is a member here or in any group entered from here.
    pub fn reaches(&self, schema: &Schema<'_>, tag: u32) -> bool {
        // Terminates: `pack` puts a nested group before its parent in the arena.
        self.find(tag).is_some()
            || self
                .groups()
                .any(|(_, member)| schema.group(member.group).reaches(schema, tag))
    }

    fn to_def(self) -> ScopeDef {
        ScopeDef {
            members: self
                .declared()
                .map(|(tag, member)| MemberDef {
                    tag,
                    group: member.group(),
                    component: index(member.component),
                    required: member.required(),
                    ambiguous: member.ambiguous(),
                    repeating: member.repeating(),
                })
                .collect(),
            components: self
                .components
                .iter()
                .map(|row| ComponentDef {
                    name: self.names[usize::from(row.name)].to_string(),
                    parent: index(row.parent),
                    required: row.flags & ComponentRow::REQUIRED != 0,
                })
                .collect(),
        }
    }
}

/// A row of a scope's component table.
#[derive(Debug, Clone, Copy)]
pub struct ComponentRef<'a> {
    rows: &'a [ComponentRow],
    names: &'a [Cow<'static, str>],
    row: u16,
}

impl<'a> ComponentRef<'a> {
    pub fn name(&self) -> &'a str {
        &self.names[usize::from(self.rows[usize::from(self.row)].name)]
    }

    pub fn required(&self) -> bool {
        self.rows[usize::from(self.row)].flags & ComponentRow::REQUIRED != 0
    }

    /// The component this one is declared inside.
    pub fn parent(&self) -> Option<ComponentRef<'a>> {
        Some(ComponentRef {
            row: index(self.rows[usize::from(self.row)].parent)?,
            ..*self
        })
    }
}

fn index(value: u16) -> Option<u16> {
    (value != NONE).then_some(value)
}

/// One scope as a parser declares it; also the group arena's dedup key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct ScopeDef {
    /// Declared order, each tag once.
    pub members: Vec<MemberDef>,
    /// Enclosing before enclosed.
    pub components: Vec<ComponentDef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MemberDef {
    pub tag: u32,
    /// Index into [`SchemaDefs::groups`].
    pub group: Option<u16>,
    /// Index into the scope's `components`.
    pub component: Option<u16>,
    pub required: bool,
    pub ambiguous: bool,
    pub repeating: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ComponentDef {
    pub name: String,
    /// An earlier index into the scope's `components`.
    pub parent: Option<u16>,
    pub required: bool,
}

/// Everything [`pack`] needs; `groups` is the arena, nested groups first.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct SchemaDefs {
    pub header: ScopeDef,
    pub trailer: ScopeDef,
    pub messages: Vec<ScopeDef>,
    pub groups: Vec<ScopeDef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeId {
    Header,
    Trailer,
    Message(usize),
    Group(usize),
}

impl fmt::Display for ScopeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScopeId::Header => f.write_str("header"),
            ScopeId::Trailer => f.write_str("trailer"),
            ScopeId::Message(message) => write!(f, "message {message}"),
            ScopeId::Group(group) => write!(f, "group {group}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackError {
    /// A count or a start does not fit the width the tables store it in.
    Overflow {
        what: &'static str,
        max: usize,
        scope: Option<ScopeId>,
    },
    DoubledTag {
        scope: ScopeId,
        tag: u32,
    },
    DanglingGroup {
        scope: ScopeId,
        tag: u32,
        group: u16,
    },
    DanglingComponent {
        scope: ScopeId,
        tag: u32,
        row: u16,
    },
    ParentNotEarlier {
        scope: ScopeId,
        row: usize,
        parent: u16,
    },
    /// Would let `reaches` loop.
    NestedAfterParent {
        group: usize,
        tag: u32,
        nested: u16,
    },
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackError::Overflow { what, max, scope } => {
                if let Some(scope) = scope {
                    write!(f, "{scope}: ")?;
                }
                write!(f, "too many {what} (more than {max})")
            }
            PackError::DoubledTag { scope, tag } => {
                write!(f, "{scope}: tag {tag} is listed twice")
            }
            PackError::DanglingGroup { scope, tag, group } => {
                write!(
                    f,
                    "{scope}: tag {tag} opens group {group}, which does not exist"
                )
            }
            PackError::DanglingComponent { scope, tag, row } => write!(
                f,
                "{scope}: tag {tag} comes through component row {row}, which does not exist"
            ),
            PackError::ParentNotEarlier { scope, row, parent } => write!(
                f,
                "{scope}: component row {row} is inside row {parent}, which is not an earlier row"
            ),
            PackError::NestedAfterParent { group, tag, nested } => write!(
                f,
                "group {group}: tag {tag} opens group {nested}, which does not come before it"
            ),
        }
    }
}

impl std::error::Error for PackError {}

/// The only way to build a schema; checks every index and width it stores.
pub fn pack(defs: &SchemaDefs) -> Result<SchemaBuf, PackError> {
    let messages = narrow(defs.messages.len(), "messages", None)?;
    // An arena index must stay below NONE.
    if defs.groups.len() > usize::from(NONE) {
        return Err(overflow("groups", usize::from(NONE), None));
    }
    let mut buf = SchemaBuf {
        spans: Vec::new(),
        tags: Vec::new(),
        members: Vec::new(),
        order: Vec::new(),
        nested: Vec::new(),
        components: Vec::new(),
        names: Vec::new(),
        messages,
    };
    let mut interned = HashMap::new();
    let scopes = [
        (ScopeId::Header, &defs.header),
        (ScopeId::Trailer, &defs.trailer),
    ]
    .into_iter()
    .chain(
        defs.messages
            .iter()
            .enumerate()
            .map(|(message, def)| (ScopeId::Message(message), def)),
    )
    .chain(
        defs.groups
            .iter()
            .enumerate()
            .map(|(group, def)| (ScopeId::Group(group), def)),
    );
    for (id, def) in scopes {
        pack_scope(&mut buf, &mut interned, defs.groups.len(), id, def)?;
    }
    Ok(buf)
}

fn pack_scope<'d>(
    buf: &mut SchemaBuf,
    interned: &mut HashMap<&'d str, u16>,
    groups: usize,
    id: ScopeId,
    def: &'d ScopeDef,
) -> Result<(), PackError> {
    let len = narrow(def.members.len(), "members", Some(id))?;
    let components_len = narrow(def.components.len(), "component rows", Some(id))?;
    let members_start = start(buf.tags.len(), "scope members")?;
    let nested_start = start(buf.nested.len(), "nested groups")?;
    let components_start = start(buf.components.len(), "component rows")?;

    for (row, component) in def.components.iter().enumerate() {
        let parent = match component.parent {
            None => NONE,
            Some(parent) if usize::from(parent) < row => parent,
            Some(parent) => {
                return Err(PackError::ParentNotEarlier {
                    scope: id,
                    row,
                    parent,
                })
            }
        };
        let name = match interned.get(component.name.as_str()) {
            Some(&name) => name,
            None => {
                // A name index must stay below NONE.
                let name = u16::try_from(buf.names.len())
                    .ok()
                    .filter(|&name| name != NONE)
                    .ok_or(overflow("component names", usize::from(NONE), None))?;
                buf.names.push(Cow::Owned(component.name.clone()));
                interned.insert(&component.name, name);
                name
            }
        };
        let flags = if component.required {
            ComponentRow::REQUIRED
        } else {
            0
        };
        buf.components.push(ComponentRow {
            name,
            parent,
            flags,
        });
    }

    for member in &def.members {
        if let Some(group) = member.group {
            if usize::from(group) >= groups {
                return Err(PackError::DanglingGroup {
                    scope: id,
                    tag: member.tag,
                    group,
                });
            }
            if let ScopeId::Group(parent) = id {
                if usize::from(group) >= parent {
                    return Err(PackError::NestedAfterParent {
                        group: parent,
                        tag: member.tag,
                        nested: group,
                    });
                }
            }
        }
        if let Some(row) = member.component {
            if usize::from(row) >= def.components.len() {
                return Err(PackError::DanglingComponent {
                    scope: id,
                    tag: member.tag,
                    row,
                });
            }
        }
    }

    let mut sorted: Vec<u16> = (0..len).collect();
    sorted.sort_unstable_by_key(|&position| def.members[usize::from(position)].tag);
    if let Some(pair) = sorted
        .windows(2)
        .find(|pair| def.members[usize::from(pair[0])].tag == def.members[usize::from(pair[1])].tag)
    {
        let tag = def.members[usize::from(pair[0])].tag;
        return Err(PackError::DoubledTag { scope: id, tag });
    }

    let mut order = vec![0; usize::from(len)];
    for (index, &position) in (0..len).zip(&sorted) {
        let member = &def.members[usize::from(position)];
        order[usize::from(position)] = index;
        let flags = [
            (member.required, Member::REQUIRED),
            (member.ambiguous, Member::AMBIGUOUS),
            (member.repeating, Member::REPEATING),
        ]
        .into_iter()
        .filter(|&(set, _)| set)
        .fold(0, |flags, (_, flag)| flags | flag);
        buf.tags.push(member.tag);
        buf.members.push(Member {
            position,
            group: member.group.unwrap_or(NONE),
            component: member.component.unwrap_or(NONE),
            flags,
        });
    }
    let nested_before = buf.nested.len();
    buf.nested.extend(
        def.members
            .iter()
            .zip(&order)
            .filter(|(member, _)| member.group.is_some())
            .map(|(_, &index)| index),
    );
    buf.order.extend(order);

    buf.spans.push(ScopeSpan {
        start: members_start,
        len,
        pad: 0,
        nested_start,
        nested_len: narrow(buf.nested.len() - nested_before, "nested groups", Some(id))?,
        components_len,
        components_start,
    });
    Ok(())
}

/// Where a span starts in a shared array.
fn start(offset: usize, what: &'static str) -> Result<u32, PackError> {
    u32::try_from(offset).map_err(|_| overflow(what, u32::MAX as usize, None))
}

fn narrow(count: usize, what: &'static str, scope: Option<ScopeId>) -> Result<u16, PackError> {
    u16::try_from(count).map_err(|_| overflow(what, usize::from(u16::MAX), scope))
}

fn overflow(what: &'static str, max: usize, scope: Option<ScopeId>) -> PackError {
    PackError::Overflow { what, max, scope }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUIRED: u16 = Member::REQUIRED;

    fn field(tag: u32) -> MemberDef {
        MemberDef {
            tag,
            group: None,
            component: None,
            required: false,
            ambiguous: false,
            repeating: false,
        }
    }

    fn required(tag: u32) -> MemberDef {
        MemberDef {
            required: true,
            ..field(tag)
        }
    }

    fn component(name: &str, parent: Option<u16>, required: bool) -> ComponentDef {
        ComponentDef {
            name: name.to_string(),
            parent,
            required,
        }
    }

    fn scope(members: Vec<MemberDef>, components: Vec<ComponentDef>) -> ScopeDef {
        ScopeDef {
            members,
            components,
        }
    }

    fn spanned(start: u32, len: u16, nested: (u32, u16), components: (u32, u16)) -> ScopeSpan {
        span(start, len, nested.0, nested.1, components.0, components.1)
    }

    /// One of each shape `pack` must carry, a group two deep and each member flag among them.
    fn fixture() -> SchemaDefs {
        SchemaDefs {
            header: scope(
                vec![
                    required(8),  // BeginString(8)
                    required(9),  // BodyLength(9)
                    required(35), // MsgType(35)
                    // NoHops(627) through HopGrp
                    MemberDef {
                        group: Some(0),
                        component: Some(0),
                        ..field(627)
                    },
                    required(49), // SenderCompID(49)
                ],
                vec![component("HopGrp", None, false)],
            ),
            // CheckSum(10)
            trailer: scope(vec![required(10)], vec![]),
            messages: vec![
                // NewOrderSingle
                scope(
                    vec![
                        required(11), // ClOrdID(11)
                        // NoPartyIDs(453) through Parties
                        MemberDef {
                            group: Some(2),
                            component: Some(2),
                            ..field(453)
                        },
                        // Symbol(55) through Instrument
                        MemberDef {
                            required: true,
                            component: Some(0),
                            ..field(55)
                        },
                        // NoSecurityAltID(454) through Instrument / SecAltIDGrp
                        MemberDef {
                            group: Some(3),
                            component: Some(1),
                            ..field(454)
                        },
                        required(54), // Side(54)
                        MemberDef {
                            repeating: true,
                            ..field(46)
                        }, // RelatdSym(46)
                        // Text(58), declared through two components
                        MemberDef {
                            ambiguous: true,
                            component: Some(2),
                            ..field(58)
                        },
                    ],
                    vec![
                        component("Instrument", None, true),
                        component("SecAltIDGrp", Some(0), false),
                        component("Parties", None, false),
                    ],
                ),
                ScopeDef::default(),
            ],
            groups: vec![
                // NoHops(627): HopCompID(628), HopSendingTime(629)
                scope(vec![field(628), field(629)], vec![]),
                // NoPartySubIDs(802): PartySubID(523), PartySubIDType(803)
                scope(vec![field(523), field(803)], vec![]),
                // NoPartyIDs(453): PartyID(448), PartyIDSource(447), PartyRole(452),
                // NoPartySubIDs(802) through PtysSubGrp
                scope(
                    vec![
                        field(448),
                        field(447),
                        field(452),
                        MemberDef {
                            group: Some(1),
                            component: Some(0),
                            ..field(802)
                        },
                    ],
                    vec![component("PtysSubGrp", None, false)],
                ),
                // NoSecurityAltID(454): SecurityAltID(455), SecurityAltIDSource(456)
                scope(vec![field(455), field(456)], vec![]),
            ],
        }
    }

    fn packed() -> SchemaBuf {
        pack(&fixture()).expect("the fixture packs")
    }

    #[test]
    fn rows_have_the_sizes_the_tables_are_measured_with() {
        assert_eq!(std::mem::size_of::<ScopeSpan>(), 20);
        assert_eq!(std::mem::size_of::<Member>(), 8);
        assert_eq!(std::mem::size_of::<ComponentRow>(), 6);
    }

    #[test]
    fn pack_lays_out_the_fixture_as_written_by_hand() {
        let expected = SchemaBuf {
            spans: vec![
                spanned(0, 5, (0, 1), (0, 1)),  // header
                spanned(5, 1, (1, 0), (1, 0)),  // trailer
                spanned(6, 7, (1, 2), (1, 3)),  // NewOrderSingle
                spanned(13, 0, (3, 0), (4, 0)), // empty message
                spanned(13, 2, (3, 0), (4, 0)), // NoHops(627)
                spanned(15, 2, (3, 0), (4, 0)), // NoPartySubIDs(802)
                spanned(17, 4, (3, 1), (4, 1)), // NoPartyIDs(453)
                spanned(21, 2, (4, 0), (5, 0)), // NoSecurityAltID(454)
            ],
            tags: vec![
                8, 9, 35, 49, 627, //
                10,  //
                11, 46, 54, 55, 58, 453, 454, //
                628, 629, //
                523, 803, //
                447, 448, 452, 802, //
                455, 456,
            ],
            members: vec![
                member(0, NONE, NONE, REQUIRED),
                member(1, NONE, NONE, REQUIRED),
                member(2, NONE, NONE, REQUIRED),
                member(4, NONE, NONE, REQUIRED),
                member(3, 0, 0, 0),
                //
                member(0, NONE, NONE, REQUIRED),
                //
                member(0, NONE, NONE, REQUIRED),
                member(5, NONE, NONE, Member::REPEATING),
                member(4, NONE, NONE, REQUIRED),
                member(2, NONE, 0, REQUIRED),
                member(6, NONE, 2, Member::AMBIGUOUS),
                member(1, 2, 2, 0),
                member(3, 3, 1, 0),
                //
                member(0, NONE, NONE, 0),
                member(1, NONE, NONE, 0),
                //
                member(0, NONE, NONE, 0),
                member(1, NONE, NONE, 0),
                //
                member(1, NONE, NONE, 0),
                member(0, NONE, NONE, 0),
                member(2, NONE, NONE, 0),
                member(3, 1, 0, 0),
                //
                member(0, NONE, NONE, 0),
                member(1, NONE, NONE, 0),
            ],
            order: vec![
                0, 1, 2, 4, 3, //
                0, //
                0, 5, 3, 6, 2, 1, 4, //
                0, 1, //
                0, 1, //
                1, 0, 2, 3, //
                0, 1,
            ],
            nested: vec![4, 5, 6, 3],
            components: vec![
                row(0, NONE, 0),
                row(1, NONE, ComponentRow::REQUIRED),
                row(2, 0, 0),
                row(3, NONE, 0),
                row(4, NONE, 0),
            ],
            names: [
                "HopGrp",
                "Instrument",
                "SecAltIDGrp",
                "Parties",
                "PtysSubGrp",
            ]
            .map(|name| Cow::Owned(name.to_string()))
            .to_vec(),
            messages: 2,
        };
        assert_eq!(packed(), expected);
    }

    #[test]
    fn pack_round_trips_through_to_defs() {
        let buf = packed();
        assert_eq!(buf.as_schema().to_defs(), fixture());
        assert_eq!(buf.as_schema().to_buf(), buf);
    }

    #[test]
    fn find_hits_a_member_and_misses_an_absent_tag() {
        let buf = packed();
        let schema = buf.as_schema();
        let order = schema.message(0);
        // Symbol(55)
        assert_eq!(order.find(55), Some(member(2, NONE, 0, REQUIRED)));
        // NoSecurityAltID(454), the last tag of the span
        assert_eq!(order.find(454), Some(member(3, 3, 1, 0)));
        // Price(44)
        assert_eq!(order.find(44), None);
        // CheckSum(10) lives in the trailer, not the message
        assert_eq!(order.find(10), None);
        assert_eq!(
            schema.trailer().find(10),
            Some(member(0, NONE, NONE, REQUIRED))
        );
        assert_eq!(schema.message(1).find(11), None);
        // NoPartySubIDs(802) inside NoPartyIDs(453)
        assert_eq!(schema.group(2).find(802), Some(member(3, 1, 0, 0)));
        assert_eq!(schema.group(2).find(448), Some(member(0, NONE, NONE, 0)));
    }

    #[test]
    fn declared_follows_the_declaration_not_the_tag_order() {
        let buf = packed();
        let schema = buf.as_schema();
        let tags = |scope: Scope<'_>| scope.declared().map(|(tag, _)| tag).collect::<Vec<_>>();
        assert_eq!(tags(schema.header()), [8, 9, 35, 627, 49]);
        assert_eq!(tags(schema.message(0)), [11, 453, 55, 454, 54, 46, 58]);
        assert_eq!(tags(schema.group(2)), [448, 447, 452, 802]);
        assert_eq!(tags(schema.message(1)), Vec::<u32>::new());
        let positions = schema
            .message(0)
            .declared()
            .map(|(_, member)| member.position)
            .collect::<Vec<_>>();
        assert_eq!(positions, [0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn groups_lists_group_members_in_declared_order() {
        let buf = packed();
        let schema = buf.as_schema();
        let groups = |scope: Scope<'_>| {
            scope
                .groups()
                .map(|(tag, member)| (tag, member.group))
                .collect::<Vec<_>>()
        };
        assert_eq!(groups(schema.message(0)), [(453, 2), (454, 3)]);
        assert_eq!(groups(schema.header()), [(627, 0)]);
        assert_eq!(groups(schema.group(2)), [(802, 1)]);
        assert_eq!(groups(schema.trailer()), Vec::<(u32, u16)>::new());
    }

    #[test]
    fn component_walks_the_chain_to_the_top() {
        let buf = packed();
        let schema = buf.as_schema();
        let order = schema.message(0);

        // NoSecurityAltID(454): SecAltIDGrp inside Instrument
        let inner = order.component(order.find(454).unwrap()).unwrap();
        assert_eq!(inner.name(), "SecAltIDGrp");
        assert!(!inner.required());
        let outer = inner.parent().unwrap();
        assert_eq!(outer.name(), "Instrument");
        assert!(outer.required());
        assert!(outer.parent().is_none());

        // NoPartyIDs(453): Parties at the top
        let parties = order.component(order.find(453).unwrap()).unwrap();
        assert_eq!(parties.name(), "Parties");
        assert!(!parties.required());
        assert!(parties.parent().is_none());

        // ClOrdID(11) is declared directly
        assert!(order.component(order.find(11).unwrap()).is_none());

        // NoPartySubIDs(802): the chain restarts inside a group
        let group = schema.group(2);
        let sub = group.component(group.find(802).unwrap()).unwrap();
        assert_eq!(sub.name(), "PtysSubGrp");
        assert!(sub.parent().is_none());
    }

    #[test]
    fn reaches_enters_nested_groups() {
        let buf = packed();
        let schema = buf.as_schema();
        let order = schema.message(0);
        // PartySubID(523): NoPartyIDs(453) -> NoPartySubIDs(802) -> PartySubID(523)
        assert!(order.reaches(&schema, 523));
        assert!(order.reaches(&schema, 803));
        // SecurityAltIDSource(456) one group deep
        assert!(order.reaches(&schema, 456));
        // Side(54) directly
        assert!(order.reaches(&schema, 54));
        // HopCompID(628) is a header group's member
        assert!(!order.reaches(&schema, 628));
        assert!(schema.header().reaches(&schema, 628));
        assert!(!schema.header().reaches(&schema, 523));
        // Price(44) nowhere
        assert!(!order.reaches(&schema, 44));
        assert!(!schema.message(1).reaches(&schema, 11));
    }

    #[test]
    fn component_names_are_interned_once_across_scopes() {
        let defs = SchemaDefs {
            messages: vec![
                scope(
                    vec![MemberDef {
                        component: Some(0),
                        ..field(448)
                    }],
                    vec![component("Parties", None, false)],
                ),
                scope(
                    vec![MemberDef {
                        component: Some(1),
                        ..field(448)
                    }],
                    vec![
                        component("Instrument", None, true),
                        component("Parties", None, true),
                    ],
                ),
            ],
            ..SchemaDefs::default()
        };
        let buf = pack(&defs).unwrap();
        assert_eq!(
            buf.names,
            [Cow::<str>::Borrowed("Parties"), Cow::Borrowed("Instrument")]
        );
        assert_eq!(
            buf.components,
            [row(0, NONE, 0), row(1, NONE, 1), row(0, NONE, 1)]
        );
        let schema = buf.as_schema();
        let second = schema.message(1);
        let parties = second.component(second.find(448).unwrap()).unwrap();
        assert_eq!(parties.name(), "Parties");
        assert!(parties.required());
        assert_eq!(schema.to_defs(), defs);
    }

    fn overflow(what: &'static str, scope: Option<ScopeId>) -> PackError {
        PackError::Overflow {
            what,
            max: 65_535,
            scope,
        }
    }

    #[test]
    fn messages_cap_at_u16_max() {
        let at = SchemaDefs {
            messages: vec![ScopeDef::default(); 65_535],
            ..SchemaDefs::default()
        };
        let buf = pack(&at).unwrap();
        assert_eq!(buf.messages, 65_535);
        assert_eq!(buf.spans.len(), 65_537);

        let past = SchemaDefs {
            messages: vec![ScopeDef::default(); 65_536],
            ..SchemaDefs::default()
        };
        assert_eq!(pack(&past), Err(overflow("messages", None)));
    }

    #[test]
    fn groups_cap_below_none() {
        let reaching = |group: u16| SchemaDefs {
            header: scope(
                vec![MemberDef {
                    group: Some(group),
                    ..field(627)
                }],
                vec![],
            ),
            groups: vec![ScopeDef::default(); 65_535],
            ..SchemaDefs::default()
        };
        let buf = pack(&reaching(65_534)).unwrap();
        assert_eq!(buf.members[0].group, 65_534);
        assert_eq!(buf.as_schema().header().groups().count(), 1);
        assert_eq!(
            pack(&reaching(65_535)),
            Err(PackError::DanglingGroup {
                scope: ScopeId::Header,
                tag: 627,
                group: 65_535,
            })
        );

        let past = SchemaDefs {
            groups: vec![ScopeDef::default(); 65_536],
            ..SchemaDefs::default()
        };
        assert_eq!(pack(&past), Err(overflow("groups", None)));
    }

    #[test]
    fn a_scope_caps_at_u16_max_members() {
        let members = |count: u32| (1..=count).map(field).collect::<Vec<_>>();
        let at = SchemaDefs {
            messages: vec![scope(members(65_535), vec![])],
            ..SchemaDefs::default()
        };
        let buf = pack(&at).unwrap();
        assert_eq!(buf.spans[2].len, 65_535);
        assert_eq!(
            buf.as_schema().message(0).find(65_535),
            Some(member(65_534, NONE, NONE, 0))
        );

        let past = SchemaDefs {
            messages: vec![scope(members(65_536), vec![])],
            ..SchemaDefs::default()
        };
        assert_eq!(
            pack(&past),
            Err(overflow("members", Some(ScopeId::Message(0))))
        );
    }

    #[test]
    fn a_scope_caps_component_rows_below_none() {
        let rows = |count: usize| vec![component("Parties", None, false); count];
        let reaching = |row: u16| SchemaDefs {
            trailer: scope(
                vec![MemberDef {
                    component: Some(row),
                    ..field(93)
                }],
                rows(65_535),
            ),
            ..SchemaDefs::default()
        };
        // SignatureLength(93) through the last row
        let buf = pack(&reaching(65_534)).unwrap();
        assert_eq!(buf.spans[1].components_len, 65_535);
        assert_eq!(buf.names.len(), 1);
        assert_eq!(
            pack(&reaching(65_535)),
            Err(PackError::DanglingComponent {
                scope: ScopeId::Trailer,
                tag: 93,
                row: 65_535,
            })
        );

        let past = SchemaDefs {
            trailer: scope(vec![], rows(65_536)),
            ..SchemaDefs::default()
        };
        assert_eq!(
            pack(&past),
            Err(overflow("component rows", Some(ScopeId::Trailer)))
        );
    }

    #[test]
    fn component_names_cap_below_none() {
        let named = |count: usize, from: usize| {
            (from..from + count)
                .map(|i| component(&format!("C{i}"), None, false))
                .collect::<Vec<_>>()
        };
        let at = SchemaDefs {
            header: scope(vec![], named(65_535, 0)),
            ..SchemaDefs::default()
        };
        let buf = pack(&at).unwrap();
        assert_eq!(buf.names.len(), 65_535);
        assert_eq!(buf.components[65_534].name, 65_534);
        assert_eq!(buf.names[65_534], "C65534");

        let past = SchemaDefs {
            header: scope(vec![], named(65_535, 0)),
            trailer: scope(vec![], named(1, 65_535)),
            ..SchemaDefs::default()
        };
        assert_eq!(pack(&past), Err(overflow("component names", None)));
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn starts_cap_at_u32_max() {
        // Four billion members do not fit a test; the one conversion every start goes through does.
        assert_eq!(start(4_294_967_295, "scope members"), Ok(u32::MAX));
        assert_eq!(
            start(4_294_967_296, "scope members"),
            Err(PackError::Overflow {
                what: "scope members",
                max: 4_294_967_295,
                scope: None,
            })
        );
    }

    #[test]
    fn a_tag_twice_in_one_scope_is_refused() {
        let defs = SchemaDefs {
            // ClOrdID(11), Symbol(55), ClOrdID(11)
            messages: vec![scope(vec![field(11), field(55), field(11)], vec![])],
            ..SchemaDefs::default()
        };
        assert_eq!(
            pack(&defs),
            Err(PackError::DoubledTag {
                scope: ScopeId::Message(0),
                tag: 11,
            })
        );
    }

    #[test]
    fn a_tag_in_two_scopes_is_fine() {
        let defs = SchemaDefs {
            // ClOrdID(11) in two messages
            messages: vec![
                scope(vec![field(11)], vec![]),
                scope(vec![field(11)], vec![]),
            ],
            ..SchemaDefs::default()
        };
        assert!(pack(&defs).is_ok());
    }

    #[test]
    fn a_dangling_group_is_refused() {
        let defs = SchemaDefs {
            // NoPartyIDs(453) pointing past the one group
            messages: vec![scope(
                vec![MemberDef {
                    group: Some(1),
                    ..field(453)
                }],
                vec![],
            )],
            groups: vec![ScopeDef::default()],
            ..SchemaDefs::default()
        };
        assert_eq!(
            pack(&defs),
            Err(PackError::DanglingGroup {
                scope: ScopeId::Message(0),
                tag: 453,
                group: 1,
            })
        );
    }

    #[test]
    fn a_dangling_component_row_is_refused() {
        let defs = SchemaDefs {
            // Symbol(55) through a second row that does not exist
            messages: vec![scope(
                vec![MemberDef {
                    component: Some(1),
                    ..field(55)
                }],
                vec![component("Instrument", None, true)],
            )],
            ..SchemaDefs::default()
        };
        assert_eq!(
            pack(&defs),
            Err(PackError::DanglingComponent {
                scope: ScopeId::Message(0),
                tag: 55,
                row: 1,
            })
        );
    }

    #[test]
    fn a_parent_must_be_an_earlier_row() {
        let later = SchemaDefs {
            header: scope(
                vec![],
                vec![
                    component("SecAltIDGrp", Some(1), false),
                    component("Instrument", None, true),
                ],
            ),
            ..SchemaDefs::default()
        };
        assert_eq!(
            pack(&later),
            Err(PackError::ParentNotEarlier {
                scope: ScopeId::Header,
                row: 0,
                parent: 1,
            })
        );

        let itself = SchemaDefs {
            groups: vec![scope(vec![], vec![component("Instrument", Some(0), true)])],
            ..SchemaDefs::default()
        };
        assert_eq!(
            pack(&itself),
            Err(PackError::ParentNotEarlier {
                scope: ScopeId::Group(0),
                row: 0,
                parent: 0,
            })
        );
    }

    #[test]
    fn a_nested_group_must_come_before_its_parent() {
        let after = SchemaDefs {
            groups: vec![
                // NoPartyIDs(453) entries holding NoPartySubIDs(802), which comes later
                scope(
                    vec![MemberDef {
                        group: Some(1),
                        ..field(802)
                    }],
                    vec![],
                ),
                scope(vec![field(523)], vec![]),
            ],
            ..SchemaDefs::default()
        };
        assert_eq!(
            pack(&after),
            Err(PackError::NestedAfterParent {
                group: 0,
                tag: 802,
                nested: 1,
            })
        );

        let itself = SchemaDefs {
            groups: vec![scope(
                vec![MemberDef {
                    group: Some(0),
                    ..field(802)
                }],
                vec![],
            )],
            ..SchemaDefs::default()
        };
        assert_eq!(
            pack(&itself),
            Err(PackError::NestedAfterParent {
                group: 0,
                tag: 802,
                nested: 0,
            })
        );
    }

    #[test]
    fn errors_say_what_is_wrong() {
        assert_eq!(
            overflow("messages", None).to_string(),
            "too many messages (more than 65535)"
        );
        assert_eq!(
            overflow("members", Some(ScopeId::Message(3))).to_string(),
            "message 3: too many members (more than 65535)"
        );
        assert_eq!(
            PackError::DoubledTag {
                scope: ScopeId::Header,
                tag: 49,
            }
            .to_string(),
            "header: tag 49 is listed twice"
        );
        assert_eq!(
            PackError::NestedAfterParent {
                group: 4,
                tag: 802,
                nested: 7,
            }
            .to_string(),
            "group 4: tag 802 opens group 7, which does not come before it"
        );
    }
}
