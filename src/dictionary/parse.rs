//! The QuickFIX XML dictionary parser. Shared with `build.rs` via `#[path]`,
//! so this file may depend only on `std`, `roxmltree`, `super::model` and
//! `super::schema`.
//!
//! The core entry point takes bytes and never touches the filesystem; the thin
//! `parse_quickfix` wrapper reads a file for tests.
//! Bounded, validated reads for user-supplied dictionaries live in the registry.

use std::cell::{Cell, RefCell};
use std::collections::{btree_map, BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use roxmltree::{Document, Node};

use super::model::{Datatype, EnumDef, Fault, FieldDef, GroupDef, MessageDef, ParsedDict};
use super::schema::{pack, ComponentDef, MemberDef, PackError, SchemaDefs, ScopeDef, NONE};

/// Component/group nesting deeper than this is an error, not a deeper recursion.
pub const MAX_DEPTH: usize = 32;

/// A component chain can expand exponentially; the depth cap bounds depth only.
pub const MAX_DECLARATIONS: usize = 1 << 20;

/// The most entries a dictionary collection may hold. Consumers store an index
/// in 16 bits with the top value reserved (`codec::Packed16`), so exactly this
/// many positions can be addressed; a larger collection is rejected here, where
/// the user gets a diagnostic naming the file, rather than resolving to
/// "unknown" at lookup. A *count*, not an index: the largest index it permits
/// is `MAX_ITEMS - 1`, which is the last packable value.
pub const MAX_ITEMS: usize = u16::MAX as usize;

#[derive(Debug)]
pub enum ParseError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Not well-formed, not UTF-8, or carries a DTD (rejected by default); the
    /// message includes roxmltree's position.
    Xml {
        path: PathBuf,
        message: String,
    },
    BadTag {
        path: PathBuf,
        text: String,
    },
    TagOutOfRange {
        path: PathBuf,
        name: String,
        tag: u32,
    },
    MissingRoot {
        path: PathBuf,
        expected: &'static str,
    },
    TooDeep {
        path: PathBuf,
    },
    /// Expanding the components would produce more than [`MAX_DECLARATIONS`] members.
    TooManyDeclarations {
        path: PathBuf,
    },
    /// More entries in a collection than its index can address.
    IndexOverflow {
        path: PathBuf,
        what: &'static str,
        max: usize,
    },
    /// One field carries more enum values than an enum index can address.
    EnumOverflow {
        path: PathBuf,
        tag: u32,
    },
    /// A reference to a name the file does not define. `within` says where the
    /// reference was made, so the user can find it.
    UndefinedReference {
        path: PathBuf,
        kind: &'static str,
        name: String,
        within: String,
    },
    /// `cycle` runs from the component back to itself.
    ComponentCycle {
        path: PathBuf,
        name: String,
        cycle: Vec<String>,
    },
    MessageWithoutType {
        path: PathBuf,
        name: String,
    },
    MessageWithoutName {
        path: PathBuf,
        msg_type: String,
    },
    /// No `<messages>` section, or one without a `<message>`.
    NoMessages {
        path: PathBuf,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            ParseError::Xml { path, message } => write!(f, "{}: {message}", path.display()),
            ParseError::BadTag { path, text } => {
                write!(f, "{}: tag is not a number: {text:?}", path.display())
            }
            ParseError::TagOutOfRange { path, name, tag } => write!(
                f,
                "{}: field {name} has number {tag}, outside the tag range 1 to {}",
                path.display(),
                u32::MAX - 1
            ),
            ParseError::MissingRoot { path, expected } => {
                write!(f, "{}: expected <{expected}> root element", path.display())
            }
            ParseError::TooDeep { path } => write!(
                f,
                "{}: component/group nesting deeper than {MAX_DEPTH}",
                path.display()
            ),
            ParseError::TooManyDeclarations { path } => write!(
                f,
                "{}: expands to more than {MAX_DECLARATIONS} member declarations",
                path.display()
            ),
            ParseError::IndexOverflow { path, what, max } => {
                write!(f, "{}: too many {what} (more than {max})", path.display())
            }
            ParseError::EnumOverflow { path, tag } => {
                write!(
                    f,
                    "{}: field {tag} has too many enum values (more than {MAX_ITEMS})",
                    path.display()
                )
            }
            ParseError::UndefinedReference {
                path,
                kind,
                name,
                within,
            } => write!(
                f,
                "{}: {within} refers to {kind} {name:?}, which the file does not define",
                path.display()
            ),
            ParseError::ComponentCycle { path, name, cycle } => write!(
                f,
                "{}: component {name} contains itself ({})",
                path.display(),
                cycle.join(" → ")
            ),
            ParseError::MessageWithoutType { path, name } if name.is_empty() => write!(
                f,
                "{}: a message has neither a name nor a msgtype",
                path.display()
            ),
            ParseError::MessageWithoutType { path, name } => {
                write!(f, "{}: message {name} has no msgtype", path.display())
            }
            ParseError::MessageWithoutName { path, msg_type } => write!(
                f,
                "{}: the message with MsgType(35) = {msg_type} has no name",
                path.display()
            ),
            ParseError::NoMessages { path } => {
                write!(f, "{}: declares no messages", path.display())
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// One XML file in memory; `path` is only used in error messages.
#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

impl Source {
    pub fn read(path: PathBuf) -> Result<Source, ParseError> {
        let bytes = fs::read(&path).map_err(|source| ParseError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(Source { path, bytes })
    }
}

type FieldsByTag = BTreeMap<u32, FieldDef>;

// ---------------------------------------------------------------------------
// Scopes: what one header, trailer, message or group entry lists

/// One reference in a scope, as the file lists it.
#[derive(Debug, Clone, Copy)]
struct Declaration {
    tag: u32,
    group: Option<u16>,
    component: Option<u16>,
    required: bool,
    repeating: bool,
}

/// A scope before a tag it lists twice is merged.
#[derive(Debug, Default)]
struct Draft {
    members: Vec<Declaration>,
    components: Vec<ComponentDef>,
}

impl Draft {
    fn push_component(&mut self, component: ComponentDef, path: &Path) -> Result<u16, ParseError> {
        let row = index16(self.components.len(), path, "component rows")?;
        self.components.push(component);
        Ok(row)
    }

    /// Adds another definition of the same message after this one.
    fn append(&mut self, other: Draft, path: &Path) -> Result<(), ParseError> {
        let offset = self.components.len();
        let shift = |row: u16| index16(offset + usize::from(row), path, "component rows");
        for mut component in other.components {
            component.parent = component.parent.map(shift).transpose()?;
            self.push_component(component, path)?;
        }
        for mut declaration in other.members {
            declaration.component = declaration.component.map(shift).transpose()?;
            self.members.push(declaration);
        }
        Ok(())
    }

    /// What makes two declarations of a tag the same: shape, requiredness and
    /// chain, components compared by name and requiredness.
    fn identity(&self, declaration: &Declaration) -> (Option<u16>, bool, bool, Vec<(&str, bool)>) {
        let mut chain = Vec::new();
        let mut at = declaration.component;
        while let Some(row) = at {
            let component = &self.components[usize::from(row)];
            chain.push((component.name.as_str(), component.required));
            at = component.parent;
        }
        (
            declaration.group,
            declaration.required,
            declaration.repeating,
            chain,
        )
    }

    /// The component names from `row` out to the scope.
    fn chain(&self, row: Option<u16>) -> Vec<&str> {
        let mut names = Vec::new();
        let mut at = row;
        while let Some(row) = at {
            let component = &self.components[usize::from(row)];
            names.push(component.name.as_str());
            at = component.parent;
        }
        names
    }

    /// Lower wins a merge: any group before any plain field, then by
    /// strictness.
    fn rank(&self, declaration: &Declaration) -> (bool, u8) {
        (declaration.group.is_none(), self.strictness(declaration))
    }

    /// 0 is the strictest: required with no optional component on its chain,
    /// then required inside an optional component, then optional.
    fn strictness(&self, declaration: &Declaration) -> u8 {
        if !declaration.required {
            return 2;
        }
        let mut at = declaration.component;
        while let Some(row) = at {
            let component = &self.components[usize::from(row)];
            if !component.required {
                return 1;
            }
            at = component.parent;
        }
        0
    }

    /// One member per tag, at its first position. A tag listed twice takes the
    /// strictest declaration whole — group, requiredness and chain — keeps the
    /// first on a tie, and is reported once; a group always beats a plain
    /// field, which a field carries no shape to argue with. Through different
    /// components it can activate none of them, so it is marked ambiguous.
    fn finish(
        self,
        label: impl Fn() -> String,
        fields: &FieldsByTag,
        faults: &mut Vec<Fault>,
    ) -> ScopeDef {
        struct Merge {
            first: usize,
            winner: usize,
            declared: usize,
            all_repeating: bool,
            ambiguous: bool,
            differs: bool,
            fields: bool,
            groups: bool,
        }
        let mut at: HashMap<u32, usize> = HashMap::new();
        let mut merges: Vec<Merge> = Vec::with_capacity(self.members.len());
        for (i, declaration) in self.members.iter().enumerate() {
            let Some(&merged) = at.get(&declaration.tag) else {
                at.insert(declaration.tag, merges.len());
                merges.push(Merge {
                    first: i,
                    winner: i,
                    declared: 1,
                    all_repeating: declaration.repeating,
                    ambiguous: false,
                    differs: false,
                    fields: declaration.group.is_none(),
                    groups: declaration.group.is_some(),
                });
                continue;
            };
            let merge = &mut merges[merged];
            merge.declared += 1;
            merge.all_repeating &= declaration.repeating;
            merge.fields |= declaration.group.is_none();
            merge.groups |= declaration.group.is_some();
            let first = &self.members[merge.first];
            merge.ambiguous |= self.chain(declaration.component) != self.chain(first.component);
            merge.differs |= self.identity(declaration) != self.identity(first);
            if self.rank(declaration) < self.rank(&self.members[merge.winner]) {
                merge.winner = i;
            }
        }

        let mut label_text = None;
        let members = merges
            .iter()
            .map(|merge| {
                let winner = &self.members[merge.winner];
                let mut within = || label_text.get_or_insert_with(&label).clone();
                let name = || {
                    fields
                        .get(&winner.tag)
                        .map_or_else(String::new, |field| field.name.clone())
                };
                if merge.fields && merge.groups {
                    faults.push(Fault::FieldAndGroup {
                        within: within(),
                        tag: winner.tag,
                        name: name(),
                    });
                // A field that may repeat is allowed to be listed again, as
                // the same declaration.
                } else if merge.declared > 1 && !(merge.all_repeating && !merge.differs) {
                    faults.push(Fault::DoubledTag {
                        within: within(),
                        tag: winner.tag,
                        name: name(),
                        differs: merge.differs,
                    });
                }
                MemberDef {
                    tag: winner.tag,
                    group: winner.group,
                    component: winner.component,
                    required: winner.required,
                    ambiguous: merge.ambiguous,
                    repeating: winner.repeating,
                }
            })
            .collect();
        ScopeDef {
            members,
            components: self.components,
        }
    }
}

/// Structurally deduplicating group arena: the same group reached from many
/// scopes, with the same entries, is one group. Nested groups come first.
#[derive(Default)]
struct Arena {
    groups: Vec<GroupDef>,
    scopes: Vec<ScopeDef>,
    index: HashMap<(GroupDef, ScopeDef), u16>,
}

impl Arena {
    fn intern(
        &mut self,
        name: &str,
        count_tag: u32,
        scope: ScopeDef,
        path: &Path,
    ) -> Result<u16, ParseError> {
        let def = GroupDef {
            name: name.to_owned(),
            count_tag,
            delimiter_tag: scope.members.first().map_or(0, |member| member.tag),
        };
        let key = (def, scope);
        if let Some(&group) = self.index.get(&key) {
            return Ok(group);
        }
        let group = index16(self.groups.len(), path, "groups")?;
        self.groups.push(key.0.clone());
        self.scopes.push(key.1.clone());
        self.index.insert(key, group);
        Ok(group)
    }
}

// ---------------------------------------------------------------------------
// QuickFIX format: one <fix> document with <fields>, <messages>, <components>

/// Convenience wrapper with a plain unbounded read — for tests only.
pub fn parse_quickfix(path: &Path) -> Result<ParsedDict, ParseError> {
    parse_quickfix_bytes(&Source::read(path.to_path_buf())?)
}

pub fn parse_quickfix_bytes(src: &Source) -> Result<ParsedDict, ParseError> {
    let doc = parse_doc(src)?;
    let root = doc.root_element();
    if !root.tag_name().name().eq_ignore_ascii_case("fix") {
        return Err(ParseError::MissingRoot {
            path: src.path.clone(),
            expected: "fix",
        });
    }
    let Some(fields_root) = section(root, "fields") else {
        return Err(ParseError::MissingRoot {
            path: src.path.clone(),
            expected: "fields",
        });
    };

    // Duplicate tags: last definition wins. Attribute spellings vary between
    // generators (`number`/`tag`, `enum`/`value`, `description`/`name`).
    let mut fields: BTreeMap<u32, FieldDef> = BTreeMap::new();
    // Built from every element, so both names of a doubled tag resolve. A
    // doubled name refers to its last number, like a doubled tag.
    let mut by_name: HashMap<String, u32> = HashMap::new();
    let mut renumbered: BTreeMap<String, u32> = BTreeMap::new();
    for node in elements_ci(fields_root, "field") {
        let tag_text = attr_any(node, &["number", "tag"]);
        let tag = parse_tag(&tag_text, &src.path)?;
        // The codec marks an overflowed tag as `u32::MAX`.
        if tag == 0 || tag == u32::MAX {
            return Err(ParseError::TagOutOfRange {
                path: src.path.clone(),
                name: attr(node, "name"),
                tag,
            });
        }
        let datatype = attr(node, "type");
        let mut def = FieldDef {
            tag,
            name: attr(node, "name"),
            datatype_class: Datatype::of(&datatype),
            datatype,
            description: attr(node, "description"),
            allows_other_values: attr(node, "allowOtherValues").eq_ignore_ascii_case("true"),
            ..FieldDef::default()
        };
        for value in elements_ci(node, "value") {
            let enum_value = attr_any(value, &["enum", "value"]);
            if enum_value.is_empty() {
                continue;
            }
            let label = attr_any(value, &["description", "name"]);
            upsert_enum(
                &mut def.enums,
                EnumDef {
                    value: enum_value,
                    name: label.clone(),
                    description: label,
                    added: String::new(),
                    added_ep: 0,
                },
            );
        }
        if !def.name.is_empty() {
            if let Some(earlier) = by_name.insert(def.name.clone(), tag) {
                if earlier != tag {
                    renumbered.entry(def.name.clone()).or_insert(earlier);
                }
            }
        }
        fields.insert(tag, def);
    }

    let component_nodes: Vec<Node<'_, '_>> = section(root, "components")
        .into_iter()
        .flat_map(|c| elements_ci(c, "component"))
        .collect();
    let components: HashMap<String, Node<'_, '_>> = component_nodes
        .iter()
        .map(|&c| (attr(c, "name"), c))
        .collect();
    let expander = QuickfixExpander {
        by_name: &by_name,
        components: &components,
        fields: &fields,
        path: &src.path,
        expanded: RefCell::new(HashSet::new()),
        faults: RefCell::new(Vec::new()),
        arena: RefCell::new(Arena::default()),
        declarations: Cell::new(0),
    };

    // Every definition of a type, concatenated in document order.
    struct Definition {
        name: String,
        category: String,
        draft: Draft,
        definitions: usize,
        /// Other distinct names, in file order.
        others: Vec<String>,
    }
    let mut definitions: BTreeMap<String, Definition> = BTreeMap::new();
    expander
        .faults
        .borrow_mut()
        .extend(renumbered.into_iter().filter_map(|(name, first)| {
            let second = by_name[&name];
            // Back on its first number after a detour: still one name, one tag.
            (first != second).then_some(Fault::FieldNameTwice {
                name,
                first,
                second,
            })
        }));
    for node in section(root, "messages")
        .into_iter()
        .flat_map(|m| elements_ci(m, "message"))
    {
        let msg_type = attr(node, "msgtype");
        let name = attr(node, "name");
        if msg_type.is_empty() {
            return Err(ParseError::MessageWithoutType {
                path: src.path.clone(),
                name,
            });
        }
        if name.is_empty() {
            return Err(ParseError::MessageWithoutName {
                path: src.path.clone(),
                msg_type,
            });
        }
        let mut draft = Draft::default();
        let within = Within::Message {
            name: &name,
            msg_type: &msg_type,
        };
        expander.expand(node, &mut Vec::new(), 0, &within, &mut draft, None)?;
        match definitions.entry(msg_type) {
            btree_map::Entry::Vacant(slot) => {
                slot.insert(Definition {
                    name,
                    category: attr(node, "msgcat"),
                    draft,
                    definitions: 1,
                    others: Vec::new(),
                });
            }
            btree_map::Entry::Occupied(mut slot) => {
                let definition = slot.get_mut();
                definition.definitions += 1;
                if name != definition.name && !definition.others.contains(&name) {
                    definition.others.push(name);
                }
                definition.draft.append(draft, &src.path)?;
            }
        }
    }
    if definitions.is_empty() {
        return Err(ParseError::NoMessages {
            path: src.path.clone(),
        });
    }

    let mut messages = Vec::with_capacity(definitions.len());
    let mut message_scopes = Vec::with_capacity(definitions.len());
    for (msg_type, definition) in definitions {
        if definition.definitions > 1 {
            expander.faults.borrow_mut().push(Fault::DoubledMessage {
                msg_type: msg_type.clone(),
                name: definition.name.clone(),
                definitions: definition.definitions,
                others: definition.others,
            });
        }
        let within = Within::Message {
            name: &definition.name,
            msg_type: &msg_type,
        };
        let scope = definition.draft.finish(
            || within.label(),
            &fields,
            &mut expander.faults.borrow_mut(),
        );
        message_scopes.push(scope);
        messages.push(MessageDef {
            msg_type,
            name: definition.name,
            category: definition.category,
            description: String::new(),
        });
    }
    check_u16(messages.len(), &src.path, "messages")?;

    // An absent or empty section is an empty scope.
    let session_scope = |name: &str, within: Within<'_>| -> Result<ScopeDef, ParseError> {
        let mut draft = Draft::default();
        if let Some(node) = section(root, name) {
            expander.expand(node, &mut Vec::new(), 0, &within, &mut draft, None)?;
        }
        Ok(draft.finish(
            || within.label(),
            &fields,
            &mut expander.faults.borrow_mut(),
        ))
    };
    let header = session_scope("header", Within::Header)?;
    let trailer = session_scope("trailer", Within::Trailer)?;

    // A component no message reaches is still the file's word; a bad
    // reference in it rejects the file all the same. It is no scope, so it
    // interns no group and merges nothing.
    let arena = expander.arena.take();
    let reported = expander.faults.borrow().len();
    for node in component_nodes {
        let name = attr(node, "name");
        if expander.expanded.borrow().contains(&name) {
            continue;
        }
        expander.expanded.borrow_mut().insert(name.clone());
        let within = Within::Component(&name);
        let mut draft = Draft::default();
        expander.expand(node, &mut vec![name.clone()], 1, &within, &mut draft, None)?;
    }
    let mut faults = expander.faults.into_inner();
    let unreached = faults.split_off(reported);
    faults.extend(
        unreached
            .into_iter()
            .filter(|fault| matches!(fault, Fault::UndefinedField { .. })),
    );
    faults.sort();
    faults.dedup();

    // QuickFIX carries no message descriptions; the MsgType enum is derived.
    if let Some(field) = fields.get_mut(&35) {
        for message in &messages {
            upsert_enum(
                &mut field.enums,
                EnumDef {
                    value: message.msg_type.clone(),
                    name: message.name.clone(),
                    description: message.name.clone(),
                    added: String::new(),
                    added_ep: 0,
                },
            );
        }
    }
    check_index_space(fields.len(), &src.path, "fields")?;
    check_enum_space(&fields, &src.path)?;

    let defs = SchemaDefs {
        header,
        trailer,
        messages: message_scopes,
        groups: arena.scopes,
    };
    let schema = pack(&defs).map_err(|e| pack_error(e, &src.path))?;
    Ok(ParsedDict {
        version: quickfix_version(root),
        fields: fields.into_values().collect(),
        groups: arena.groups,
        messages,
        schema,
        faults,
    })
}

/// Only a count can overrun: the parser merges doubled tags and makes every
/// index itself, so any other refusal is a bug here.
fn pack_error(e: PackError, path: &Path) -> ParseError {
    match e {
        PackError::Overflow { what, max, .. } => ParseError::IndexOverflow {
            path: path.to_path_buf(),
            what,
            max,
        },
        e => panic!(
            "{}: the parser built an invalid schema: {e}",
            path.display()
        ),
    }
}

/// Where a reference was made, rendered only when it fails to resolve.
#[derive(Clone, Copy)]
enum Within<'a> {
    Message { name: &'a str, msg_type: &'a str },
    Header,
    Trailer,
    Component(&'a str),
    Group(&'a str, &'a Within<'a>),
}

impl Within<'_> {
    /// The scope as a doubled-tag fault names it: a message with its MsgType(35).
    fn label(&self) -> String {
        match self {
            Within::Message { name, msg_type } => {
                format!("message {name}, MsgType(35) = {msg_type}")
            }
            Within::Group(name, parent) => format!("group {name} in {}", parent.label()),
            _ => self.to_string(),
        }
    }
}

impl fmt::Display for Within<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Within::Message { name, .. } => write!(f, "message {name}"),
            Within::Header => f.write_str("the header"),
            Within::Trailer => f.write_str("the trailer"),
            Within::Component(name) => write!(f, "component {name}"),
            Within::Group(name, parent) => write!(f, "group {name} in {parent}"),
        }
    }
}

struct QuickfixExpander<'a, 'i> {
    by_name: &'a HashMap<String, u32>,
    components: &'a HashMap<String, Node<'a, 'i>>,
    /// For the names a fault shows.
    fields: &'a FieldsByTag,
    path: &'a Path,
    /// Components walked so far, so the unreached ones can be checked after.
    expanded: RefCell<HashSet<String>>,
    /// Undefined field references, skipped rather than refused, and merges.
    faults: RefCell<Vec<Fault>>,
    arena: RefCell<Arena>,
    /// Members produced so far, bounded by [`MAX_DECLARATIONS`].
    declarations: Cell<usize>,
}

impl QuickfixExpander<'_, '_> {
    /// Every member a scope gets passes through here, so the budget counts them all.
    fn declare(&self, draft: &mut Draft, declaration: Declaration) -> Result<(), ParseError> {
        let count = self.declarations.get() + 1;
        if count > MAX_DECLARATIONS {
            return Err(ParseError::TooManyDeclarations {
                path: self.path.to_path_buf(),
            });
        }
        self.declarations.set(count);
        draft.members.push(declaration);
        Ok(())
    }

    /// Adds the children of a message/group/component node to `draft`, with
    /// `<component>` references expanded under row `current`. `chain` holds the
    /// components being expanded, groups included, so a component inside
    /// itself is caught however it got there; a `<group>` is a scope of its
    /// own, with its own component rows. An undefined field is left out and
    /// recorded as a fault.
    fn expand(
        &self,
        node: Node<'_, '_>,
        chain: &mut Vec<String>,
        depth: usize,
        within: &Within<'_>,
        draft: &mut Draft,
        current: Option<u16>,
    ) -> Result<(), ParseError> {
        if depth > MAX_DEPTH {
            return Err(ParseError::TooDeep {
                path: self.path.to_path_buf(),
            });
        }
        for child in node.children().filter(|n| n.is_element()) {
            let name = attr(child, "name");
            // An absent `required` means optional.
            let required = child.attribute("required") == Some("Y");
            let kind = child.tag_name().name().to_ascii_lowercase();
            match kind.as_str() {
                "field" => match self.by_name.get(&name) {
                    Some(&tag) => self.declare(
                        draft,
                        Declaration {
                            tag,
                            group: None,
                            component: current,
                            required,
                            repeating: child.attribute("repeating") == Some("Y"),
                        },
                    )?,
                    None => self.faults.borrow_mut().push(Fault::UndefinedField {
                        within: within.to_string(),
                        name,
                    }),
                },
                "group" => {
                    let Some(&count_tag) = self.by_name.get(&name) else {
                        return Err(self.undefined("group counter", name, within));
                    };
                    let within = Within::Group(&name, within);
                    let mut entries = Draft::default();
                    self.expand(child, chain, depth + 1, &within, &mut entries, None)?;
                    let scope = entries.finish(
                        || within.label(),
                        self.fields,
                        &mut self.faults.borrow_mut(),
                    );
                    let group = self
                        .arena
                        .borrow_mut()
                        .intern(&name, count_tag, scope, self.path)?;
                    self.declare(
                        draft,
                        Declaration {
                            tag: count_tag,
                            group: Some(group),
                            component: current,
                            required,
                            repeating: false,
                        },
                    )?;
                }
                "component" => {
                    let Some(&component) = self.components.get(&name) else {
                        return Err(self.undefined("component", name, within));
                    };
                    if let Some(start) = chain.iter().position(|seen| *seen == name) {
                        let mut cycle = chain[start..].to_vec();
                        cycle.push(name.clone());
                        return Err(ParseError::ComponentCycle {
                            path: self.path.to_path_buf(),
                            name,
                            cycle,
                        });
                    }
                    self.expanded.borrow_mut().insert(name.clone());
                    let row = draft.push_component(
                        ComponentDef {
                            name: name.clone(),
                            parent: current,
                            required,
                        },
                        self.path,
                    )?;
                    chain.push(name.clone());
                    let expanded = self.expand(
                        component,
                        chain,
                        depth + 1,
                        &Within::Component(&name),
                        draft,
                        Some(row),
                    );
                    chain.pop();
                    expanded?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn undefined(&self, kind: &'static str, name: String, within: &Within<'_>) -> ParseError {
        ParseError::UndefinedReference {
            path: self.path.to_path_buf(),
            kind,
            name,
            within: within.to_string(),
        }
    }
}

/// The XML header is the sole source of a runtime dictionary's version.
fn quickfix_version(root: Node<'_, '_>) -> Option<String> {
    if let Some(version) = root.attribute("version") {
        return Some(version.to_owned());
    }
    if let (Some(major), Some(minor)) = (root.attribute("major"), root.attribute("minor")) {
        let typ = root.attribute("type").unwrap_or("FIX");
        let mut version = format!("{typ}.{major}.{minor}");
        let service_pack = attr_any(root, &["servicepack", "servicePack"]);
        if !service_pack.is_empty() {
            let sp = service_pack.parse::<u32>().ok()?;
            if sp > 0 {
                version.push_str(&format!("SP{sp}"));
            }
        }
        return Some(version);
    }
    None
}

// ---------------------------------------------------------------------------
// XML helpers

/// First child element named `name`, matched case-insensitively (QuickFIX files
/// are seen with both `<fields>` and `<Fields>`).
fn section<'a, 'i: 'a>(node: Node<'a, 'i>, name: &'a str) -> Option<Node<'a, 'i>> {
    elements_ci(node, name).next()
}

fn elements_ci<'a, 'i: 'a>(
    node: Node<'a, 'i>,
    name: &'a str,
) -> impl Iterator<Item = Node<'a, 'i>> {
    node.children()
        .filter(move |n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case(name))
}

/// First present attribute among `names`, or empty.
fn attr_any(node: Node<'_, '_>, names: &[&str]) -> String {
    names
        .iter()
        .find_map(|n| node.attribute(*n))
        .unwrap_or_default()
        .to_owned()
}

pub(super) fn parse_doc(src: &Source) -> Result<Document<'_>, ParseError> {
    let text = std::str::from_utf8(&src.bytes).map_err(|e| ParseError::Xml {
        path: src.path.clone(),
        message: format!("invalid UTF-8: {e}"),
    })?;
    // Default options reject DTDs — required: no entity expansion from user files.
    Document::parse(text).map_err(|e| ParseError::Xml {
        path: src.path.clone(),
        message: e.to_string(),
    })
}

pub(super) fn attr(node: Node<'_, '_>, name: &str) -> String {
    node.attribute(name).unwrap_or_default().to_owned()
}

/// A different fact from [`check_u16`], even though the two numbers coincide:
/// a field index is packed with one value reserved, so its capacity is
/// [`MAX_ITEMS`], not the full `u16` range.
fn check_index_space(len: usize, path: &Path, what: &'static str) -> Result<(), ParseError> {
    if len > MAX_ITEMS {
        return Err(ParseError::IndexOverflow {
            path: path.to_path_buf(),
            what,
            max: MAX_ITEMS,
        });
    }
    Ok(())
}

/// Enum positions are packed like field indices. Checked over the assembled
/// fields, because MsgType(35) also gains a value from every `<message>`.
fn check_enum_space(fields: &FieldsByTag, path: &Path) -> Result<(), ParseError> {
    for field in fields.values() {
        if field.enums.len() > MAX_ITEMS {
            return Err(ParseError::EnumOverflow {
                path: path.to_path_buf(),
                tag: field.tag,
            });
        }
    }
    Ok(())
}

/// Messages and groups are stored as a plain `u16` in the model and reserve
/// nothing, so the fact here is the `u16` range rather than a packing decision.
fn check_u16(len: usize, path: &Path, what: &'static str) -> Result<(), ParseError> {
    if len > usize::from(u16::MAX) {
        return Err(ParseError::IndexOverflow {
            path: path.to_path_buf(),
            what,
            max: u16::MAX as usize,
        });
    }
    Ok(())
}

/// An index stored in 16 bits with `NONE` reserved, made where it is created
/// so it can never wrap or become `NONE` before `pack` sees it.
fn index16(len: usize, path: &Path, what: &'static str) -> Result<u16, ParseError> {
    u16::try_from(len)
        .ok()
        .filter(|&index| index != NONE)
        .ok_or_else(|| ParseError::IndexOverflow {
            path: path.to_path_buf(),
            what,
            max: usize::from(NONE),
        })
}

fn parse_tag(text: &str, path: &Path) -> Result<u32, ParseError> {
    text.trim().parse().map_err(|_| ParseError::BadTag {
        path: path.to_path_buf(),
        text: text.to_owned(),
    })
}

/// Insert or replace by value, keeping `enums` sorted (duplicate values: last wins).
fn upsert_enum(enums: &mut Vec<EnumDef>, def: EnumDef) {
    match enums.binary_search_by(|e| e.value.cmp(&def.value)) {
        Ok(i) => enums[i] = def,
        Err(i) => enums.insert(i, def),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictionary::schema::Scope;
    use std::fmt::Write as _;

    fn src(name: &str, xml: &str) -> Source {
        Source {
            path: PathBuf::from(name),
            bytes: xml.as_bytes().to_vec(),
        }
    }

    const CUSTOM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/xml/custom");

    fn quickfix(xml: &str) -> Result<ParsedDict, ParseError> {
        parse_quickfix_bytes(&src("dict.xml", xml))
    }

    fn message_of<'a>(dict: &'a ParsedDict, msg_type: &str) -> Scope<'a> {
        let index = u16::try_from(dict.message_index(msg_type).unwrap()).unwrap();
        dict.schema.as_schema().message(index)
    }

    /// The groups a scope opens, as arena indices, in declared order.
    fn opened(scope: Scope<'_>) -> Vec<u16> {
        scope
            .groups()
            .filter_map(|(_, member)| member.group())
            .collect()
    }

    /// Direct member tags in declared order, counters of nested groups included.
    fn tags(scope: Scope<'_>) -> Vec<u32> {
        scope.declared().map(|(tag, _)| tag).collect()
    }

    /// Every tag `scope` reaches, nested groups included, sorted.
    fn reached(dict: &ParsedDict, scope: Scope<'_>) -> Vec<u32> {
        let schema = dict.schema.as_schema();
        let mut tags = schema.tags.to_vec();
        tags.sort_unstable();
        tags.dedup();
        tags.retain(|&tag| scope.reaches(&schema, tag));
        tags
    }

    fn header(dict: &ParsedDict) -> Vec<u32> {
        reached(dict, dict.schema.as_schema().header())
    }

    fn trailer(dict: &ParsedDict) -> Vec<u32> {
        reached(dict, dict.schema.as_schema().trailer())
    }

    /// A top-level group of a message, with its entry scope.
    fn group_of<'a>(
        dict: &'a ParsedDict,
        msg_type: &str,
        count_tag: u32,
    ) -> Option<(&'a GroupDef, Scope<'a>)> {
        let (_, member) = message_of(dict, msg_type)
            .groups()
            .find(|&(tag, _)| tag == count_tag)?;
        let group = member.group()?;
        Some((
            &dict.groups[usize::from(group)],
            dict.schema.as_schema().group(group),
        ))
    }

    #[test]
    fn synthetic_oe_fix44_parses_with_messages_and_groups() {
        let dict = parse_quickfix(&Path::new(CUSTOM).join("synthetic/oe-fix44.xml")).unwrap();
        assert_eq!(dict.version.as_deref(), Some("FIX.4.4"));
        assert_eq!(dict.field(50001).unwrap().name, "SyntheticMode");
        let d = dict.message("D").unwrap();
        assert_eq!(d.name, "NewOrderSingle");
        assert_eq!(d.category, "app");
        // Group declared directly in the message, with a nested group.
        let (parties, entry) = group_of(&dict, "D", 453).expect("NoPartyIDs");
        assert_eq!(parties.delimiter_tag, 448);
        assert_eq!(tags(entry), vec![448, 452, 802]);
        let subs = &dict.groups[usize::from(opened(entry)[0])];
        assert_eq!((subs.count_tag, subs.delimiter_tag), (802, 523));
        // The Instrument component is spliced inline: not a group.
        assert_eq!(opened(message_of(&dict, "D")).len(), 1);
        assert_eq!(opened(message_of(&dict, "8")).len(), 0);
    }

    #[test]
    fn fields_are_sorted_and_unique_by_tag() {
        let dict = parse_quickfix(&Path::new(CUSTOM).join("synthetic/oe-fix44.xml")).unwrap();
        assert!(dict.fields.windows(2).all(|w| w[0].tag < w[1].tag));
        assert!(dict
            .messages
            .windows(2)
            .all(|w| w[0].msg_type < w[1].msg_type));
        assert!(!dict.groups.is_empty());
    }

    #[test]
    fn dtd_is_rejected() {
        let xml = "<!DOCTYPE fix [<!ENTITY x \"y\">]><fix major='4' minor='4'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields/></fix>";
        let err = quickfix(xml).unwrap_err();
        assert!(matches!(err, ParseError::Xml { .. }), "{err}");
    }

    /// One number with two names: the last definition wins, and a reference
    /// by either name still resolves.
    #[test]
    fn duplicate_tag_last_wins() {
        let dict = quickfix(
            "<fix major='4' minor='4'>\
             <header><field name='Old'/></header>\
             <messages><message name='NewOrderSingle' msgtype='D' msgcat='app'>\
             <field name='BeginString'/></message></messages><fields>\
             <field number='8' name='Old' type='CHAR'/>\
             <field number='8' name='BeginString' type='STRING'/>\
             </fields></fix>",
        )
        .unwrap();
        assert_eq!(dict.fields.len(), 1);
        assert_eq!(dict.field(8).unwrap().name, "BeginString");
        assert_eq!(header(&dict), vec![8]);
        assert!(dict.faults.is_empty());
    }

    #[test]
    fn allow_other_values_is_read() {
        let dict = quickfix(
            "<fix major='4' minor='4'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields>\
             <field number='65' name='SymbolSfx' type='STRING' allowOtherValues='true'>\
             <value enum='WI' description='WHEN_ISSUED'/></field>\
             <field number='54' name='Side' type='CHAR'/>\
             </fields></fix>",
        )
        .unwrap();
        assert!(dict.field(65).unwrap().allows_other_values);
        assert!(!dict.field(54).unwrap().allows_other_values);
    }

    #[test]
    fn duplicate_enum_values_last_win_and_values_sort() {
        let dict = quickfix(
            "<fix major='4' minor='4'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields>\
             <field number='54' name='Side' type='CHAR'>\
             <value enum='1' description='OLD'/>\
             <value enum='1' description='BUY'/>\
             <value enum='0' description='ZERO'/>\
             </field></fields></fix>",
        )
        .unwrap();
        let side = dict.field(54).unwrap();
        assert_eq!(side.enums.len(), 2);
        assert_eq!(side.enums[0].value, "0");
        assert_eq!(side.enums[1].name, "BUY");
    }

    #[test]
    fn md_fix50_group_inside_component_is_expanded() {
        // Message `y` reaches NoRelatedSym only through <component name='SecListGrp'/>;
        // the group's own first member comes from a nested <component name='Instrument'/>.
        let dict = parse_quickfix(&Path::new(CUSTOM).join("synthetic/md-fix50.xml")).unwrap();
        assert_eq!(dict.version.as_deref(), Some("FIX.5.0"));
        let (related, entry) = group_of(&dict, "y", 146).expect("NoRelatedSym via SecListGrp");
        assert_eq!(related.name, "NoRelatedSym");
        assert_eq!(related.delimiter_tag, 55);
        assert_eq!(tags(entry), vec![55, 167, 562]);
        // Message V declares NoRelatedSym directly — present as well, but not the point.
        assert_eq!(tags(group_of(&dict, "V", 146).unwrap().1), vec![55]);
        // W reaches NoMDEntries through MDFullGrp; MDReqGrp is defined but unused.
        assert!(group_of(&dict, "W", 268).is_some());
        assert!(dict.groups.iter().all(|g| g.count_tag != 267));
    }

    #[test]
    fn quickfix_enum_name_comes_from_description_attr() {
        let dict = quickfix(
            "<fix major='4' minor='4'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields>\
             <field number='54' name='Side' type='CHAR'><value enum='1' description='BUY'/></field>\
             </fields></fix>",
        )
        .unwrap();
        let e = &dict.field(54).unwrap().enums[0];
        assert_eq!(
            (e.value.as_str(), e.name.as_str(), e.description.as_str()),
            ("1", "BUY", "BUY")
        );
        assert_eq!(e.added, "");
    }

    #[test]
    fn msgtype_enum_is_synthesised_on_field_35() {
        let dict = quickfix(
            "<fix major='4' minor='4'>\
             <messages><message name='Heartbeat' msgtype='0' msgcat='admin'/></messages>\
             <fields><field number='35' name='MsgType' type='STRING'/></fields></fix>",
        )
        .unwrap();
        let e = &dict.field(35).unwrap().enums[0];
        assert_eq!((e.value.as_str(), e.name.as_str()), ("0", "Heartbeat"));
        let hb = dict.message("0").unwrap();
        assert_eq!(
            (
                hb.name.as_str(),
                hb.category.as_str(),
                hb.description.as_str()
            ),
            ("Heartbeat", "admin", "")
        );
    }

    #[test]
    fn attr_fallbacks_are_ported() {
        let dict = quickfix(
            "<FIX major='4' minor='2'><Messages><Message name='NewOrderSingle' msgtype='D' msgcat='app'/></Messages><Fields>\
             <Field tag='54' name='Side' type='CHAR'><Value value='2' name='SELL'/></Field>\
             </Fields></FIX>",
        )
        .unwrap();
        let side = dict.field(54).unwrap();
        assert_eq!(side.enums[0].value, "2");
        assert_eq!(side.enums[0].name, "SELL");
    }

    #[test]
    fn quickfix_missing_root_or_fields_is_reported() {
        let err = quickfix("<nope/>").unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::MissingRoot {
                    expected: "fix",
                    ..
                }
            ),
            "{err}"
        );
        let err = quickfix("<fix major='4' minor='4'><messages/></fix>").unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::MissingRoot {
                    expected: "fields",
                    ..
                }
            ),
            "{err}"
        );
        let err = quickfix("<fix><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields><field number='x' name='A'/></fields></fix>").unwrap_err();
        assert!(matches!(err, ParseError::BadTag { .. }), "{err}");
    }

    /// The codec marks a tag whose digits overflow a `u32` as `u32::MAX`, so a
    /// field with that number would name every such tag; 0 is no FIX tag.
    #[test]
    fn a_field_number_outside_the_tag_range_is_refused() {
        let file = |number: &str| {
            format!(
                "<fix><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages>\
                 <fields><field number='{number}' name='A' type='STRING'/></fields></fix>"
            )
        };
        for (number, text) in [
            (
                "0",
                "dict.xml: field A has number 0, outside the tag range 1 to 4294967294",
            ),
            (
                "4294967295",
                "dict.xml: field A has number 4294967295, outside the tag range 1 to 4294967294",
            ),
        ] {
            let err = quickfix(&file(number)).unwrap_err();
            assert!(matches!(err, ParseError::TagOutOfRange { .. }), "{err}");
            assert_eq!(err.to_string(), text);
        }
        let dict = quickfix(&file("4294967294")).unwrap();
        assert_eq!(dict.fields[0].tag, 4_294_967_294);
    }

    /// A QuickFIX file declares its own session layer, groups included; a
    /// dictionary that declares none gets empty sets rather than a guess.
    #[test]
    fn quickfix_header_and_trailer_are_read_from_the_file() {
        let dict = quickfix(
            "<fix major='4' minor='4'>\
             <header><field name='BeginString'/><field name='MsgType'/>\
             <group name='NoHops'><field name='HopCompID'/></group></header>\
             <trailer><field name='CheckSum'/></trailer>\
             <messages><message name='X' msgtype='X'><field name='Symbol'/></message></messages>\
             <fields>\
             <field number='8' name='BeginString' type='STRING'/>\
             <field number='35' name='MsgType' type='STRING'/>\
             <field number='55' name='Symbol' type='STRING'/>\
             <field number='10' name='CheckSum' type='STRING'/>\
             <field number='627' name='NoHops' type='NUMINGROUP'/>\
             <field number='628' name='HopCompID' type='STRING'/>\
             </fields></fix>",
        )
        .unwrap();
        assert_eq!(header(&dict), vec![8, 35, 627, 628]);
        assert_eq!(trailer(&dict), vec![10]);

        let none = quickfix(
            "<fix major='4' minor='4'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields><field number='1' name='A' type='STRING'/></fields></fix>",
        )
        .unwrap();
        assert!(header(&none).is_empty() && trailer(&none).is_empty());
    }

    /// The counter is typed `int` in older files, so the name still decides.
    #[test]
    fn a_datatype_class_comes_from_the_spelling_whatever_its_case() {
        let dict = quickfix(
            "<fix major='4' minor='4'>\
             <messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages>\
             <fields><field number='44' name='Price' type='PRICE'/>\
             <field number='60' name='TransactTime' type='utctimestamp'/>\
             <field number='9999' name='Odd' type='Whatever'/></fields></fix>",
        )
        .unwrap();
        assert_eq!(dict.field(44).unwrap().datatype_class, Datatype::Price);
        assert_eq!(dict.field(60).unwrap().datatype_class, Datatype::Timestamp);
        assert_eq!(dict.field(9999).unwrap().datatype_class, Datatype::Unknown);
        // The string is untouched: hover shows what the file said.
        assert_eq!(dict.field(44).unwrap().datatype, "PRICE");
    }

    // --- Scopes -----------------------------------------------------------
    // Expected values are written out by hand from the XML in each test.

    const SCHEMA_FIELDS: &str = "\
        <field number='8' name='BeginString' type='STRING'/>\
        <field number='10' name='CheckSum' type='STRING'/>\
        <field number='11' name='ClOrdID' type='STRING'/>\
        <field number='35' name='MsgType' type='STRING'/>\
        <field number='48' name='SecurityID' type='STRING'/>\
        <field number='54' name='Side' type='CHAR'/>\
        <field number='55' name='Symbol' type='STRING'/>\
        <field number='58' name='Text' type='STRING'/>\
        <field number='448' name='PartyID' type='STRING'/>\
        <field number='452' name='PartyRole' type='INT'/>\
        <field number='453' name='NoPartyIDs' type='NUMINGROUP'/>\
        <field number='454' name='NoSecurityAltID' type='NUMINGROUP'/>\
        <field number='455' name='SecurityAltID' type='STRING'/>\
        <field number='517' name='OwnershipType' type='CHAR'/>\
        <field number='627' name='NoHops' type='NUMINGROUP'/>\
        <field number='628' name='HopCompID' type='STRING'/>\
        <field number='629' name='HopSendingTime' type='UTCTIMESTAMP'/>\
        <field number='1499' name='NoAsgnReqs' type='NUMINGROUP'/>";

    fn schema_dict(body: &str) -> ParsedDict {
        quickfix(&format!(
            "<fix major='4' minor='4'>{body}<fields>{SCHEMA_FIELDS}</fields></fix>"
        ))
        .unwrap()
    }

    fn defs(dict: &ParsedDict) -> SchemaDefs {
        dict.schema.as_schema().to_defs()
    }

    fn message_scope(dict: &ParsedDict, msg_type: &str) -> ScopeDef {
        defs(dict).messages[dict.message_index(msg_type).unwrap()].clone()
    }

    fn plain(tag: u32) -> MemberDef {
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
            ..plain(tag)
        }
    }

    fn component(name: &str, parent: Option<u16>, required: bool) -> ComponentDef {
        ComponentDef {
            name: name.into(),
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

    fn group(name: &str, count_tag: u32, delimiter_tag: u32) -> GroupDef {
        GroupDef {
            name: name.into(),
            count_tag,
            delimiter_tag,
        }
    }

    fn doubled(within: &str, tag: u32, name: &str, differs: bool) -> Fault {
        Fault::DoubledTag {
            within: within.into(),
            tag,
            name: name.into(),
            differs,
        }
    }

    #[test]
    fn a_required_and_an_optional_component_each_get_a_row() {
        let dict = schema_dict(
            "<messages><message name='NewOrderSingle' msgtype='D'>\
               <field name='ClOrdID' required='Y'/>\
               <component name='Instrument' required='Y'/>\
               <component name='Parties' required='N'/>\
               <field name='Side' required='Y'/>\
             </message></messages>\
             <components>\
               <component name='Instrument'>\
                 <field name='Symbol' required='Y'/><field name='SecurityID' required='N'/>\
               </component>\
               <component name='Parties'><field name='PartyID' required='Y'/></component>\
             </components>",
        );
        assert_eq!(
            message_scope(&dict, "D"),
            scope(
                vec![
                    required(11),
                    MemberDef {
                        component: Some(0),
                        ..required(55)
                    },
                    MemberDef {
                        component: Some(0),
                        ..plain(48)
                    },
                    MemberDef {
                        component: Some(1),
                        ..required(448)
                    },
                    required(54),
                ],
                vec![
                    component("Instrument", None, true),
                    component("Parties", None, false),
                ],
            )
        );
        assert!(dict.faults.is_empty());
    }

    /// Requiredness sits on the reference, and an absent `required` is optional.
    #[test]
    fn one_component_is_required_in_one_message_and_optional_in_another() {
        let dict = schema_dict(
            "<messages>\
               <message name='NewOrderSingle' msgtype='D'><component name='Instrument' required='Y'/></message>\
               <message name='OrderCancelRequest' msgtype='F'>\
                 <field name='ClOrdID' required='Y'/><component name='Instrument'/>\
               </message>\
             </messages>\
             <components><component name='Instrument'><field name='Symbol' required='Y'/></component></components>",
        );
        let symbol = MemberDef {
            component: Some(0),
            ..required(55)
        };
        assert_eq!(
            message_scope(&dict, "D"),
            scope(vec![symbol], vec![component("Instrument", None, true)])
        );
        assert_eq!(
            message_scope(&dict, "F"),
            scope(
                vec![required(11), symbol],
                vec![component("Instrument", None, false)]
            )
        );
    }

    #[test]
    fn a_required_component_inside_an_optional_one_hangs_off_it() {
        let dict = schema_dict(
            "<messages><message name='NewOrderSingle' msgtype='D'>\
               <component name='Instrument' required='N'/>\
             </message></messages>\
             <components>\
               <component name='Instrument'>\
                 <field name='Symbol' required='Y'/><component name='SecAltIDGrp' required='Y'/>\
               </component>\
               <component name='SecAltIDGrp'><field name='SecurityID' required='Y'/></component>\
             </components>",
        );
        assert_eq!(
            message_scope(&dict, "D"),
            scope(
                vec![
                    MemberDef {
                        component: Some(0),
                        ..required(55)
                    },
                    MemberDef {
                        component: Some(1),
                        ..required(48)
                    },
                ],
                vec![
                    component("Instrument", None, false),
                    component("SecAltIDGrp", Some(0), true),
                ],
            )
        );
    }

    /// The counter belongs to the scope the component is expanded into; the
    /// group's own entries start a fresh chain.
    #[test]
    fn a_group_counter_reached_through_a_component_carries_it() {
        let dict = schema_dict(
            "<messages><message name='NewOrderSingle' msgtype='D'>\
               <field name='ClOrdID' required='Y'/><component name='Instrument' required='Y'/>\
             </message></messages>\
             <components>\
               <component name='Instrument'>\
                 <field name='Symbol' required='Y'/><component name='SecAltIDGrp' required='N'/>\
               </component>\
               <component name='SecAltIDGrp'>\
                 <group name='NoSecurityAltID' required='N'><field name='SecurityAltID' required='Y'/></group>\
               </component>\
             </components>",
        );
        assert_eq!(
            message_scope(&dict, "D"),
            scope(
                vec![
                    required(11),
                    MemberDef {
                        component: Some(0),
                        ..required(55)
                    },
                    MemberDef {
                        group: Some(0),
                        component: Some(1),
                        ..plain(454)
                    },
                ],
                vec![
                    component("Instrument", None, true),
                    component("SecAltIDGrp", Some(0), false),
                ],
            )
        );
        assert_eq!(defs(&dict).groups, [scope(vec![required(455)], vec![])]);
        assert_eq!(dict.groups, [group("NoSecurityAltID", 454, 455)]);
    }

    /// An empty nested group still delimits the entry around it.
    #[test]
    fn an_empty_nested_group_as_the_first_member_is_the_delimiter() {
        let dict = quickfix(&format!(
            "<fix major='4' minor='4'><messages><message name='NewOrderSingle' msgtype='D'>\
               <group name='NoPartyIDs'><group name='NoPartySubIDs'/><field name='PartyID'/></group>\
             </message></messages>\
             <fields>{SCHEMA_FIELDS}<field number='802' name='NoPartySubIDs' type='NUMINGROUP'/></fields></fix>"
        ))
        .unwrap();
        assert_eq!(
            defs(&dict).groups,
            [
                scope(vec![], vec![]),
                scope(
                    vec![
                        MemberDef {
                            group: Some(0),
                            ..plain(802)
                        },
                        plain(448),
                    ],
                    vec![],
                ),
            ]
        );
        assert_eq!(
            dict.groups,
            [
                group("NoPartySubIDs", 802, 0),
                group("NoPartyIDs", 453, 802),
            ]
        );
    }

    /// As FIX.5.0SP2's NoAsgnReqs(1499), whose first member is NoPartyIDs(453).
    #[test]
    fn a_nested_counter_can_be_the_delimiter() {
        let dict = schema_dict(
            "<messages><message name='StreamAssignmentReport' msgtype='CD'>\
               <group name='NoAsgnReqs' required='N'>\
                 <component name='Parties' required='N'/><field name='Symbol'/>\
               </group>\
             </message></messages>\
             <components><component name='Parties'>\
               <group name='NoPartyIDs' required='N'><field name='PartyID'/><field name='PartyRole'/></group>\
             </component></components>",
        );
        assert_eq!(
            defs(&dict).groups,
            [
                scope(vec![plain(448), plain(452)], vec![]),
                scope(
                    vec![
                        MemberDef {
                            group: Some(0),
                            component: Some(0),
                            ..plain(453)
                        },
                        plain(55),
                    ],
                    vec![component("Parties", None, false)],
                ),
            ]
        );
        assert_eq!(
            dict.groups,
            [
                group("NoPartyIDs", 453, 448),
                group("NoAsgnReqs", 1499, 453),
            ]
        );
        assert_eq!(
            message_scope(&dict, "CD"),
            scope(
                vec![MemberDef {
                    group: Some(1),
                    ..plain(1499)
                }],
                vec![]
            )
        );
    }

    #[test]
    fn header_no_hops_is_a_group_direct_or_through_a_component() {
        let order = "<messages><message name='NewOrderSingle' msgtype='D'><field name='Symbol'/></message></messages>";
        let hops = "<group name='NoHops' required='N'>\
                      <field name='HopCompID'/><field name='HopSendingTime'/>\
                    </group>";
        let entries = [scope(vec![plain(628), plain(629)], vec![])];
        let no_hops = [group("NoHops", 627, 628)];

        let direct = schema_dict(&format!(
            "<header><field name='BeginString' required='Y'/>{hops}</header>\
             <trailer><field name='CheckSum' required='Y'/></trailer>{order}"
        ));
        let schema = defs(&direct);
        assert_eq!(
            schema.header,
            scope(
                vec![
                    required(8),
                    MemberDef {
                        group: Some(0),
                        ..plain(627)
                    },
                ],
                vec![]
            )
        );
        assert_eq!(schema.trailer, scope(vec![required(10)], vec![]));
        assert_eq!(schema.groups, entries);
        assert_eq!(direct.groups, no_hops);
        assert_eq!(header(&direct), [8, 627, 628, 629]);

        let through = schema_dict(&format!(
            "<header><field name='BeginString' required='Y'/><component name='HopGrp' required='N'/></header>\
             {order}<components><component name='HopGrp'>{hops}</component></components>"
        ));
        let schema = defs(&through);
        assert_eq!(
            schema.header,
            scope(
                vec![
                    required(8),
                    MemberDef {
                        group: Some(0),
                        component: Some(0),
                        ..plain(627)
                    },
                ],
                vec![component("HopGrp", None, false)]
            )
        );
        assert_eq!(schema.groups, entries);
        assert_eq!(through.groups, no_hops);
    }

    #[test]
    fn a_tag_listed_twice_is_one_member_at_its_first_position() {
        let dict = schema_dict(
            "<messages>\
               <message name='RegistrationInstructions' msgtype='o'>\
                 <field name='OwnershipType' required='N'/>\
                 <field name='ClOrdID' required='Y'/>\
                 <field name='OwnershipType' required='Y'/>\
                 <field name='OwnershipType' required='N'/>\
               </message>\
               <message name='NewOrderSingle' msgtype='D'>\
                 <group name='NoPartyIDs' required='N'>\
                   <field name='PartyID'/><field name='PartyRole'/><field name='PartyID'/>\
                 </group>\
               </message>\
             </messages>",
        );
        assert_eq!(
            message_scope(&dict, "o"),
            scope(vec![required(517), required(11)], vec![])
        );
        assert_eq!(
            defs(&dict).groups,
            [scope(vec![plain(448), plain(452)], vec![])]
        );
        assert_eq!(
            dict.faults,
            [
                doubled(
                    "group NoPartyIDs in message NewOrderSingle, MsgType(35) = D",
                    448,
                    "PartyID",
                    false
                ),
                doubled(
                    "message RegistrationInstructions, MsgType(35) = o",
                    517,
                    "OwnershipType",
                    true
                ),
            ],
            "three declarations, one fault"
        );
        let text: Vec<String> = dict.faults.iter().map(ToString::to_string).collect();
        assert_eq!(
            text,
            [
                "group NoPartyIDs in message NewOrderSingle, MsgType(35) = D, lists PartyID(448) \
                 twice; merged into one",
                "message RegistrationInstructions, MsgType(35) = o, lists OwnershipType(517) \
                 twice; merged into one",
            ]
        );
        let header = schema_dict(
            "<header><field name='BeginString' required='Y'/><field name='BeginString'/></header>\
             <messages><message name='NewOrderSingle' msgtype='D'/></messages>",
        );
        assert_eq!(
            header.faults[0].to_string(),
            "the header lists BeginString(8) twice; merged into one"
        );
    }

    /// Required outside any optional component beats required inside one,
    /// which beats optional, whichever comes first.
    #[test]
    fn the_strictest_declaration_of_a_doubled_tag_wins() {
        let dict = schema_dict(
            "<messages>\
               <message name='NewOrderSingle' msgtype='D'>\
                 <component name='Instrument' required='N'/><field name='Symbol' required='Y'/>\
               </message>\
               <message name='OrderCancelRequest' msgtype='F'>\
                 <field name='Symbol'/><component name='Instrument' required='N'/>\
               </message>\
             </messages>\
             <components><component name='Instrument'><field name='Symbol' required='Y'/></component></components>",
        );
        let instrument = vec![component("Instrument", None, false)];
        assert_eq!(
            message_scope(&dict, "D"),
            scope(
                vec![MemberDef {
                    ambiguous: true,
                    ..required(55)
                }],
                instrument.clone()
            )
        );
        assert_eq!(
            message_scope(&dict, "F"),
            scope(
                vec![MemberDef {
                    component: Some(0),
                    ambiguous: true,
                    ..required(55)
                }],
                instrument
            )
        );
        assert_eq!(
            dict.faults,
            [
                doubled(
                    "message NewOrderSingle, MsgType(35) = D",
                    55,
                    "Symbol",
                    true
                ),
                doubled(
                    "message OrderCancelRequest, MsgType(35) = F",
                    55,
                    "Symbol",
                    true
                ),
            ]
        );
    }

    /// Ambiguous when the declarations came through different components; the
    /// same component listed twice is not.
    #[test]
    fn a_tag_shared_by_two_optional_components_is_ambiguous() {
        let dict = schema_dict(
            "<messages>\
               <message name='NewOrderSingle' msgtype='D'>\
                 <component name='CommissionData' required='N'/><component name='Stipulations' required='N'/>\
               </message>\
               <message name='OrderCancelRequest' msgtype='F'>\
                 <component name='CommissionData' required='N'/><component name='CommissionData' required='N'/>\
               </message>\
             </messages>\
             <components>\
               <component name='CommissionData'><field name='Text'/></component>\
               <component name='Stipulations'><field name='Text'/></component>\
             </components>",
        );
        assert_eq!(
            message_scope(&dict, "D"),
            scope(
                vec![MemberDef {
                    component: Some(0),
                    ambiguous: true,
                    ..plain(58)
                }],
                vec![
                    component("CommissionData", None, false),
                    component("Stipulations", None, false),
                ]
            )
        );
        assert_eq!(
            message_scope(&dict, "F"),
            scope(
                vec![MemberDef {
                    component: Some(0),
                    ..plain(58)
                }],
                vec![
                    component("CommissionData", None, false),
                    component("CommissionData", None, false),
                ]
            )
        );
        assert_eq!(
            dict.faults,
            [
                doubled("message NewOrderSingle, MsgType(35) = D", 58, "Text", true),
                doubled(
                    "message OrderCancelRequest, MsgType(35) = F",
                    58,
                    "Text",
                    false
                ),
            ]
        );
    }

    /// The shape, requiredness and chain come from one declaration together.
    #[test]
    fn a_counter_listed_twice_takes_the_strictest_shape_whole() {
        let dict = schema_dict(
            "<messages><message name='NewOrderSingle' msgtype='D'>\
               <group name='NoPartyIDs' required='N'><field name='PartyID'/></group>\
               <field name='Symbol'/>\
               <component name='Parties' required='Y'/>\
             </message></messages>\
             <components><component name='Parties'>\
               <group name='NoPartyIDs' required='Y'>\
                 <field name='PartyID' required='Y'/><field name='PartyRole'/>\
               </group>\
             </component></components>",
        );
        assert_eq!(
            message_scope(&dict, "D"),
            scope(
                vec![
                    MemberDef {
                        group: Some(1),
                        component: Some(0),
                        ambiguous: true,
                        ..required(453)
                    },
                    plain(55),
                ],
                vec![component("Parties", None, true)]
            )
        );
        assert_eq!(
            defs(&dict).groups,
            [
                scope(vec![plain(448)], vec![]),
                scope(vec![required(448), plain(452)], vec![]),
            ]
        );
        assert_eq!(opened(message_of(&dict, "D")), [1]);
        assert_eq!(
            dict.faults,
            [doubled(
                "message NewOrderSingle, MsgType(35) = D",
                453,
                "NoPartyIDs",
                true
            )]
        );
    }

    fn field_and_group(within: &str, tag: u32, name: &str) -> Fault {
        Fault::FieldAndGroup {
            within: within.into(),
            tag,
            name: name.into(),
        }
    }

    /// A group declaration of a counter beats a plain field of the same tag
    /// whole, whatever the order or requiredness, with its own fault.
    #[test]
    fn a_group_beats_a_plain_field_of_the_same_counter() {
        let parties = "<components><component name='Parties'>\
             <group name='NoPartyIDs' required='N'><field name='PartyID'/><field name='PartyRole'/></group>\
             </component></components>";
        let field_first = schema_dict(&format!(
            "<messages><message name='ExecutionReport' msgtype='8'>\
               <field name='NoPartyIDs' required='Y'/><field name='Symbol'/>\
               <component name='Parties' required='N'/>\
             </message></messages>{parties}"
        ));
        assert_eq!(
            message_scope(&field_first, "8"),
            scope(
                vec![
                    MemberDef {
                        group: Some(0),
                        component: Some(0),
                        ambiguous: true,
                        ..plain(453)
                    },
                    plain(55),
                ],
                vec![component("Parties", None, false)]
            )
        );
        assert_eq!(opened(message_of(&field_first, "8")), [0]);
        assert_eq!(
            field_first.faults,
            [field_and_group(
                "message ExecutionReport, MsgType(35) = 8",
                453,
                "NoPartyIDs"
            )]
        );
        assert_eq!(
            field_first.faults[0].to_string(),
            "message ExecutionReport, MsgType(35) = 8, lists NoPartyIDs(453) as a plain field \
             and as a group; the group is kept"
        );

        let group_first = schema_dict(&format!(
            "<messages><message name='ExecutionReport' msgtype='8'>\
               <component name='Parties' required='N'/><field name='Symbol'/>\
               <field name='NoPartyIDs' required='Y'/><field name='NoPartyIDs'/>\
             </message></messages>{parties}"
        ));
        assert_eq!(
            message_scope(&group_first, "8"),
            scope(
                vec![
                    MemberDef {
                        group: Some(0),
                        component: Some(0),
                        ambiguous: true,
                        ..plain(453)
                    },
                    plain(55),
                ],
                vec![component("Parties", None, false)]
            )
        );
        assert_eq!(
            group_first.faults,
            [field_and_group(
                "message ExecutionReport, MsgType(35) = 8",
                453,
                "NoPartyIDs"
            )],
            "one fault, not a doubled-tag one besides"
        );
    }

    /// Nothing the file says is lost when every declaration is the same; a
    /// choice between different ones is an error.
    #[test]
    fn a_doubled_tag_is_a_warning_when_identical_and_an_error_when_not() {
        let dict = schema_dict(
            "<messages>\
               <message name='NewOrderSingle' msgtype='D'>\
                 <field name='Symbol'/><field name='Side' required='Y'/><field name='Symbol'/>\
                 <component name='CommissionData' required='N'/><component name='CommissionData' required='N'/>\
               </message>\
               <message name='OrderCancelRequest' msgtype='F'>\
                 <field name='Symbol'/><field name='Symbol' required='Y'/>\
                 <component name='CommissionData' required='N'/><component name='CommissionData' required='Y'/>\
               </message>\
             </messages>\
             <components><component name='CommissionData'><field name='Text'/></component></components>",
        );
        let order = "message NewOrderSingle, MsgType(35) = D";
        let cancel = "message OrderCancelRequest, MsgType(35) = F";
        assert_eq!(
            dict.faults,
            [
                doubled(order, 55, "Symbol", false),
                doubled(order, 58, "Text", false),
                doubled(cancel, 55, "Symbol", true),
                doubled(cancel, 58, "Text", true),
            ]
        );
        let errors: Vec<bool> = dict.faults.iter().map(Fault::is_error).collect();
        assert_eq!(errors, [false, false, true, true]);
    }

    fn doubled_message(msg_type: &str, name: &str, definitions: usize, others: &[&str]) -> Fault {
        Fault::DoubledMessage {
            msg_type: msg_type.into(),
            name: name.into(),
            definitions,
            others: others.iter().map(|&other| other.to_owned()).collect(),
        }
    }

    /// The other names are what makes it an error, so the fault says them:
    /// each distinct one once, in file order.
    #[test]
    fn a_doubled_message_names_its_other_names() {
        let dict = schema_dict(
            "<messages>\
               <message name='NewOrderSingle' msgtype='D'/>\
               <message name='OrderSingle' msgtype='D'/>\
               <message name='OrderCancelRequest' msgtype='F'/>\
               <message name='CancelOrder' msgtype='F'/>\
               <message name='OrderCancelRequest' msgtype='F'/>\
               <message name='CancelOrder' msgtype='F'/>\
               <message name='Heartbeat' msgtype='0'/>\
               <message name='Heartbeat' msgtype='0'/>\
               <message name='Heartbeat' msgtype='0'/>\
               <message name='ExecutionReport' msgtype='8'/>\
               <message name='OrderSingle' msgtype='8'/>\
               <message name='NOS' msgtype='8'/>\
             </messages>",
        );
        assert_eq!(
            dict.faults,
            [
                doubled_message("0", "Heartbeat", 3, &[]),
                doubled_message("8", "ExecutionReport", 3, &["OrderSingle", "NOS"]),
                doubled_message("D", "NewOrderSingle", 2, &["OrderSingle"]),
                doubled_message("F", "OrderCancelRequest", 4, &["CancelOrder"]),
            ]
        );
        let errors: Vec<bool> = dict.faults.iter().map(Fault::is_error).collect();
        assert_eq!(errors, [false, true, true, true]);
        let text: Vec<String> = dict.faults.iter().map(ToString::to_string).collect();
        assert_eq!(
            text,
            [
                "message Heartbeat, MsgType(35) = 0, is defined 3 times; merged into one",
                "message ExecutionReport, MsgType(35) = 8, is defined 3 times, also as \
                 OrderSingle, NOS; merged into one under ExecutionReport",
                "message NewOrderSingle, MsgType(35) = D, is defined twice, also as \
                 OrderSingle; merged into one under NewOrderSingle",
                "message OrderCancelRequest, MsgType(35) = F, is defined 4 times, also as \
                 CancelOrder; merged into one under OrderCancelRequest",
            ]
        );
    }

    #[test]
    fn a_field_and_a_group_is_a_warning_and_a_skipped_or_renumbered_field_an_error() {
        let within = || "message ExecutionReport, MsgType(35) = 8".to_owned();
        assert!(!Fault::FieldAndGroup {
            within: within(),
            tag: 453,
            name: "NoPartyIDs".into(),
        }
        .is_error());
        assert!(Fault::UndefinedField {
            within: within(),
            name: "Symbl".into(),
        }
        .is_error());
        assert!(Fault::FieldNameTwice {
            name: "TargetStrategy".into(),
            first: 847,
            second: 50001,
        }
        .is_error());
    }

    /// `repeating="Y"` allows a tag again only as the same declaration.
    #[test]
    fn a_repeating_tag_listed_again_with_another_requiredness_is_a_fault() {
        let dict = schema_dict(
            "<messages><message name='NewOrderSingle' msgtype='D'>\
               <field name='Symbol' repeating='Y' required='Y'/>\
               <field name='ClOrdID'/>\
               <field name='Symbol' repeating='Y'/>\
             </message></messages>",
        );
        assert_eq!(
            message_scope(&dict, "D"),
            scope(
                vec![
                    MemberDef {
                        repeating: true,
                        ..required(55)
                    },
                    plain(11),
                ],
                vec![]
            )
        );
        assert_eq!(
            dict.faults,
            [doubled(
                "message NewOrderSingle, MsgType(35) = D",
                55,
                "Symbol",
                true
            )]
        );
    }

    /// Both definitions' members in document order, each with its own
    /// component rows, then merged like any other scope.
    #[test]
    fn a_doubled_message_type_unions_its_scopes() {
        let dict = schema_dict(
            "<messages>\
               <message name='NewOrderSingle' msgtype='D'>\
                 <field name='ClOrdID' required='Y'/><component name='Parties' required='N'/>\
               </message>\
               <message name='OrderSingle' msgtype='D'>\
                 <field name='Side' required='Y'/><component name='Instrument' required='Y'/>\
               </message>\
             </messages>\
             <components>\
               <component name='Parties'><group name='NoPartyIDs' required='N'><field name='PartyID'/></group></component>\
               <component name='Instrument'><field name='Symbol' required='Y'/></component>\
             </components>",
        );
        assert_eq!(
            message_scope(&dict, "D"),
            scope(
                vec![
                    required(11),
                    MemberDef {
                        group: Some(0),
                        component: Some(0),
                        ..plain(453)
                    },
                    required(54),
                    MemberDef {
                        component: Some(1),
                        ..required(55)
                    },
                ],
                vec![
                    component("Parties", None, false),
                    component("Instrument", None, true),
                ]
            )
        );
        assert_eq!(
            dict.faults,
            [doubled_message("D", "NewOrderSingle", 2, &["OrderSingle"])]
        );
    }

    /// The second definition's component rows come after the first's, so a
    /// nested row's parent and a member's component point at the shifted row.
    #[test]
    fn a_doubled_message_type_shifts_the_second_definitions_component_rows() {
        let dict = schema_dict(
            "<messages>\
               <message name='NewOrderSingle' msgtype='D'>\
                 <field name='ClOrdID' required='Y'/><component name='Parties' required='N'/>\
               </message>\
               <message name='OrderSingle' msgtype='D'>\
                 <component name='Instrument' required='Y'/>\
               </message>\
             </messages>\
             <components>\
               <component name='Parties'><group name='NoPartyIDs' required='N'><field name='PartyID'/></group></component>\
               <component name='Instrument'>\
                 <field name='Symbol' required='Y'/><component name='SecAltIDGrp' required='N'/>\
               </component>\
               <component name='SecAltIDGrp'>\
                 <group name='NoSecurityAltID' required='N'><field name='SecurityAltID' required='Y'/></group>\
               </component>\
             </components>",
        );
        assert_eq!(
            message_scope(&dict, "D"),
            scope(
                vec![
                    required(11),
                    MemberDef {
                        group: Some(0),
                        component: Some(0),
                        ..plain(453)
                    },
                    MemberDef {
                        component: Some(1),
                        ..required(55)
                    },
                    MemberDef {
                        group: Some(1),
                        component: Some(2),
                        ..plain(454)
                    },
                ],
                vec![
                    component("Parties", None, false),
                    component("Instrument", None, true),
                    component("SecAltIDGrp", Some(1), false),
                ]
            )
        );
        assert_eq!(
            dict.groups,
            [
                group("NoPartyIDs", 453, 448),
                group("NoSecurityAltID", 454, 455),
            ]
        );
        assert_eq!(
            dict.faults,
            [doubled_message("D", "NewOrderSingle", 2, &["OrderSingle"])]
        );
    }

    /// A message with no members allows none; an empty group is kept with
    /// delimiter 0 and its counter stays a member.
    #[test]
    fn an_empty_group_and_an_empty_message_are_empty_scopes() {
        let dict = schema_dict(
            "<header><field name='BeginString' required='Y'/><group name='NoHops' required='N'/></header>\
             <messages>\
               <message name='Heartbeat' msgtype='0'/>\
               <message name='NewOrderSingle' msgtype='D'>\
                 <field name='Symbol'/><group name='NoPartyIDs' required='Y'/>\
               </message>\
             </messages>",
        );
        let schema = defs(&dict);
        assert_eq!(message_scope(&dict, "0"), ScopeDef::default());
        assert_eq!(
            message_scope(&dict, "D"),
            scope(
                vec![
                    plain(55),
                    MemberDef {
                        group: Some(0),
                        ..required(453)
                    },
                ],
                vec![]
            )
        );
        assert_eq!(
            schema.header,
            scope(
                vec![
                    required(8),
                    MemberDef {
                        group: Some(1),
                        ..plain(627)
                    },
                ],
                vec![]
            )
        );
        assert_eq!(schema.groups, [ScopeDef::default(), ScopeDef::default()]);
        assert_eq!(
            dict.groups,
            [group("NoPartyIDs", 453, 0), group("NoHops", 627, 0),]
        );
        assert!(dict.faults.is_empty());
    }

    /// FIX.4.0's RelatdSym(46): a field that may repeat, listed again, is no
    /// defect.
    #[test]
    fn a_repeating_field_is_marked_and_may_be_listed_again() {
        let dict = schema_dict(
            "<messages><message name='News' msgtype='B'>\
               <field name='Symbol' required='N' repeating='Y'/>\
               <field name='Text'/>\
               <field name='Symbol' repeating='Y'/>\
             </message></messages>",
        );
        assert_eq!(
            message_scope(&dict, "B"),
            scope(
                vec![
                    MemberDef {
                        repeating: true,
                        ..plain(55)
                    },
                    plain(58),
                ],
                vec![]
            )
        );
        assert!(dict.faults.is_empty());
    }

    /// Around `body`, the fields every rejection test refers to by name.
    fn with_fields(body: &str) -> String {
        format!(
            "<fix major='4' minor='4'>{body}<fields>\
             <field number='35' name='MsgType' type='STRING'/>\
             <field number='55' name='Symbol' type='STRING'/>\
             <field number='78' name='NoAllocs' type='NUMINGROUP'/>\
             <field number='79' name='AllocAccount' type='STRING'/>\
             <field number='448' name='PartyID' type='STRING'/>\
             <field number='453' name='NoPartyIDs' type='NUMINGROUP'/>\
             </fields></fix>"
        )
    }

    fn refused(body: &str) -> ParseError {
        quickfix(&with_fields(body)).unwrap_err()
    }

    const ORDER: &str = "<message name='NewOrderSingle' msgtype='D'>";

    /// The member is left out, the rest of the scope and the file load, and
    /// the fault names where the reference was made.
    #[test]
    fn an_undefined_field_is_skipped_with_a_fault_naming_where_it_was_referenced() {
        let parties = "<group name='NoPartyIDs'><field name='PartyID'/></group>";
        let has_parties = |dict: &ParsedDict| {
            group_of(dict, "D", 453)
                .is_some_and(|(group, entry)| tags(entry) == [448] && group.delimiter_tag == 448)
        };
        type Check = fn(&ParsedDict) -> bool;
        let cases: [(String, &str, Check); 6] = [
            (
                format!("<messages>{ORDER}<field name='Symbl'/>{parties}</message></messages>"),
                "message NewOrderSingle",
                has_parties,
            ),
            (
                format!(
                    "<messages>{ORDER}<component name='Instrument'/></message></messages>\
                     <components><component name='Instrument'><field name='Symbl'/>{parties}</component></components>"
                ),
                "component Instrument",
                has_parties,
            ),
            (
                format!(
                    "<messages>{ORDER}<group name='NoPartyIDs'><field name='Symbl'/><field name='PartyID'/></group></message></messages>"
                ),
                "group NoPartyIDs in message NewOrderSingle",
                has_parties,
            ),
            (
                format!(
                    "<header><field name='Symbl'/><field name='Symbol'/></header>\
                     <messages>{ORDER}</message></messages>"
                ),
                "the header",
                |dict| header(dict) == [55],
            ),
            (
                format!(
                    "<messages>{ORDER}</message></messages>\
                     <trailer><field name='Symbl'/><field name='Symbol'/></trailer>"
                ),
                "the trailer",
                |dict| trailer(dict) == [55],
            ),
            // No message reaches it, and it is reported all the same.
            (
                format!(
                    "<messages>{ORDER}{parties}</message></messages>\
                     <components><component name='Unused'><field name='Symbl'/></component></components>"
                ),
                "component Unused",
                has_parties,
            ),
        ];
        for (body, within, rest_is_there) in cases {
            let dict = quickfix(&with_fields(&body)).unwrap();
            assert_eq!(
                dict.faults,
                [Fault::UndefinedField {
                    within: within.into(),
                    name: "Symbl".into()
                }],
                "{within}"
            );
            assert_eq!(
                dict.faults[0].to_string(),
                format!(
                    "{within} refers to field \"Symbl\", which the file does not define; skipped"
                )
            );
            assert_eq!(dict.message("D").unwrap().name, "NewOrderSingle");
            assert!(rest_is_there(&dict), "{within}: {dict:?}");
        }
    }

    #[test]
    fn one_undefined_field_reached_from_two_messages_is_one_fault() {
        let dict = quickfix(&with_fields(
            "<messages>\
             <message name='NewOrderSingle' msgtype='D'><component name='Instrument'/></message>\
             <message name='OrderCancelRequest' msgtype='F'><component name='Instrument'/></message>\
             </messages><components><component name='Instrument'><field name='Symbl'/></component></components>",
        ))
        .unwrap();
        assert_eq!(
            dict.faults,
            [Fault::UndefinedField {
                within: "component Instrument".into(),
                name: "Symbl".into()
            }]
        );
    }

    #[test]
    fn an_undefined_group_counter_is_refused() {
        let err = refused(
            "<messages><message name='NewOrderSingle' msgtype='D'><component name='AttrbGrp'/></message></messages>\
             <components><component name='AttrbGrp'>\
               <group name='NoInstrAttrib'><field name='Symbol'/></group>\
             </component></components>",
        );
        assert!(
            matches!(
                &err,
                ParseError::UndefinedReference {
                    kind: "group counter",
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "dict.xml: component AttrbGrp refers to group counter \"NoInstrAttrib\", \
             which the file does not define"
        );
    }

    #[test]
    fn an_unknown_component_is_refused() {
        let err = refused(&format!(
            "<messages>{ORDER}<component name='Instrumnt'/></message></messages>"
        ));
        assert_eq!(
            err.to_string(),
            "dict.xml: message NewOrderSingle refers to component \"Instrumnt\", \
             which the file does not define"
        );
    }

    #[test]
    fn a_component_inside_itself_is_refused_and_depth_is_capped() {
        let cases = [
            ("<component name='A'><component name='A'/></component>", "A → A"),
            (
                "<component name='A'><field name='Symbol'/><component name='B'/></component>\
                 <component name='B'><component name='A'/></component>",
                "A → B → A",
            ),
            // The chain spans groups: a group starts no fresh expansion.
            (
                "<component name='A'><field name='Symbol'/><component name='B'/></component>\
                 <component name='B'><group name='NoPartyIDs'><field name='PartyID'/><component name='A'/></group></component>",
                "A → B → A",
            ),
        ];
        for (components, cycle) in cases {
            let err = refused(&format!(
                "<messages>{ORDER}<component name='A'/></message></messages>\
                 <components>{components}</components>"
            ));
            assert!(
                matches!(&err, ParseError::ComponentCycle { name, .. } if name == "A"),
                "{err:?}"
            );
            assert_eq!(
                err.to_string(),
                format!("dict.xml: component A contains itself ({cycle})")
            );
        }

        let mut xml = String::from("<fix major='4' minor='4'><messages><message name='X' msgtype='X'><component name='C0'/></message></messages><components>");
        for i in 0..(MAX_DEPTH + 5) {
            xml.push_str(&format!(
                "<component name='C{i}'><component name='C{}'/></component>",
                i + 1
            ));
        }
        xml.push_str("</components><fields><field number='1' name='A'/></fields></fix>");
        assert!(matches!(
            quickfix(&xml).unwrap_err(),
            ParseError::TooDeep { .. }
        ));
    }

    /// The leaf lists 16 fields and each component above it lists the one
    /// before inside 16 groups, so a level has 16 counters plus 16 times the
    /// level below: 16, 272, 4 368, 69 904, then 1 118 480 at the top.
    #[test]
    fn a_dictionary_that_expands_past_the_declaration_budget_is_refused() {
        const LEAF: [&str; 16] = [
            "BeginString",
            "CheckSum",
            "ClOrdID",
            "SecurityID",
            "Side",
            "Symbol",
            "Text",
            "PartyID",
            "PartyRole",
            "NoSecurityAltID",
            "SecurityAltID",
            "OwnershipType",
            "NoHops",
            "HopCompID",
            "HopSendingTime",
            "NoAsgnReqs",
        ];
        let leaf: String = LEAF
            .iter()
            .map(|name| format!("<field name='{name}'/>"))
            .collect();
        let mut components = format!("<component name='C0'>{leaf}</component>");
        for i in 1..=4 {
            let groups = format!(
                "<group name='NoPartyIDs'><component name='C{}'/></group>",
                i - 1
            )
            .repeat(16);
            components.push_str(&format!("<component name='C{i}'>{groups}</component>"));
        }
        let xml = format!(
            "<fix major='4' minor='4'><messages><message name='X' msgtype='X'>\
             <component name='C4'/></message></messages>\
             <components>{components}</components><fields>{SCHEMA_FIELDS}</fields></fix>"
        );
        let err = quickfix(&xml).unwrap_err();
        assert!(
            matches!(err, ParseError::TooManyDeclarations { .. }),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "dict.xml: expands to more than 1048576 member declarations"
        );
    }

    #[test]
    fn a_message_without_a_type_or_a_name_is_refused() {
        let err = refused("<messages><message name='NewOrderSingle'/></messages>");
        assert!(
            matches!(err, ParseError::MessageWithoutType { .. }),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "dict.xml: message NewOrderSingle has no msgtype"
        );
        let err = refused("<messages><message msgcat='app'/></messages>");
        assert_eq!(
            err.to_string(),
            "dict.xml: a message has neither a name nor a msgtype"
        );
        let err = refused("<messages><message msgtype='D'/></messages>");
        assert!(
            matches!(err, ParseError::MessageWithoutName { .. }),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "dict.xml: the message with MsgType(35) = D has no name"
        );
    }

    /// Both tags stay defined; the name refers to the last one.
    #[test]
    fn a_field_name_with_two_numbers_loads_with_a_fault() {
        let dict = quickfix(
            "<fix major='4' minor='4'>\
             <header><field name='TargetStrategy'/></header>\
             <messages><message name='NewOrderSingle' msgtype='D'/></messages>\
             <fields><field number='847' name='TargetStrategy' type='INT'/>\
             <field number='50001' name='TargetStrategy' type='STRING'/></fields></fix>",
        )
        .unwrap();
        assert_eq!(dict.field(847).unwrap().name, "TargetStrategy");
        assert_eq!(dict.field(50001).unwrap().name, "TargetStrategy");
        assert_eq!(header(&dict), [50001]);
        assert_eq!(
            dict.faults,
            [Fault::FieldNameTwice {
                name: "TargetStrategy".into(),
                first: 847,
                second: 50001
            }]
        );
        assert_eq!(
            dict.faults[0].to_string(),
            "field TargetStrategy is defined with two numbers, TargetStrategy(847) and \
             TargetStrategy(50001); the name refers to TargetStrategy(50001)"
        );
    }

    #[test]
    fn a_file_that_declares_no_messages_is_refused() {
        for body in ["", "<messages/>", "<messages><header/></messages>"] {
            let err = refused(body);
            assert!(
                matches!(err, ParseError::NoMessages { .. }),
                "{body}: {err:?}"
            );
            assert_eq!(err.to_string(), "dict.xml: declares no messages");
        }
    }

    #[test]
    fn a_doubled_message_type_is_the_union_of_its_definitions_with_one_fault() {
        let dict = quickfix(&with_fields(
            "<messages>\
             <message name='NewOrderSingle' msgtype='D' msgcat='app'>\
               <group name='NoPartyIDs'><field name='PartyID'/></group></message>\
             <message name='OrderSingle' msgtype='D' msgcat='admin'>\
               <group name='NoAllocs'><field name='AllocAccount'/></group></message>\
             <message name='Order' msgtype='D'><field name='Symbol'/></message>\
             </messages>",
        ))
        .unwrap();
        let order = dict.message("D").unwrap();
        assert_eq!(
            (order.name.as_str(), order.category.as_str()),
            ("NewOrderSingle", "app"),
            "the first definition names it"
        );
        let counters: Vec<u32> = opened(message_of(&dict, "D"))
            .into_iter()
            .map(|group| dict.groups[usize::from(group)].count_tag)
            .collect();
        assert_eq!(counters, vec![453, 78]);
        let label = &dict.field(35).unwrap().enums[0];
        assert_eq!(
            (label.value.as_str(), label.name.as_str()),
            ("D", "NewOrderSingle")
        );
        assert_eq!(
            dict.faults,
            vec![doubled_message(
                "D",
                "NewOrderSingle",
                3,
                &["OrderSingle", "Order"]
            )],
            "three definitions, one fault"
        );
        assert_eq!(
            dict.faults[0].to_string(),
            "message NewOrderSingle, MsgType(35) = D, is defined 3 times, also as OrderSingle, \
             Order; merged into one under NewOrderSingle"
        );
    }

    #[test]
    fn faults_are_sorted() {
        let dict = quickfix(&with_fields(
            "<messages>\
             <message name='OrderCancelRequest' msgtype='F'/>\
             <message name='NewOrderSingle' msgtype='D'/>\
             <message name='OrderCancelRequest' msgtype='F'/>\
             <message name='NewOrderSingle' msgtype='D'/>\
             </messages>",
        ))
        .unwrap();
        assert_eq!(
            dict.faults,
            [
                doubled_message("D", "NewOrderSingle", 2, &[]),
                doubled_message("F", "OrderCancelRequest", 2, &[])
            ]
        );
    }

    #[test]
    fn version_from_header_attrs() {
        let v = |xml: &str| quickfix(xml).unwrap().version;
        assert_eq!(
            v("<fix type='FIXT' major='1' minor='1' servicepack='0'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields/></fix>").as_deref(),
            Some("FIXT.1.1")
        );
        assert_eq!(
            v("<fix major='5' minor='0' servicepack='2'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields/></fix>").as_deref(),
            Some("FIX.5.0SP2")
        );
        assert_eq!(
            v("<fix major='4' minor='4' servicePack='0'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields/></fix>").as_deref(),
            Some("FIX.4.4")
        );
        assert_eq!(
            v("<fix version='FIX.9.9'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields/></fix>").as_deref(),
            Some("FIX.9.9")
        );
        // Runtime loading rejects absent or invalid XML version metadata.
        assert_eq!(v("<fix><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields/></fix>"), None);
        assert_eq!(
            v("<fix major='4' minor='4' servicepack='bad'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields/></fix>"),
            None
        );
    }

    fn quickfix_with_fields(count: u32) -> String {
        let mut doc = String::with_capacity(count as usize * 48 + 64);
        doc.push_str("<fix major='4' minor='4'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields>");
        for tag in 1..=count {
            doc.push_str(&format!(
                "<field number='{tag}' name='F{tag}' type='STRING'/>"
            ));
        }
        doc.push_str("</fields></fix>");
        doc
    }

    #[test]
    fn a_dictionary_that_overruns_the_field_index_space_is_rejected() {
        // A field index is packed into a `u16` whose `u16::MAX` means "unknown
        // tag", leaving MAX_ITEMS addressable positions.
        let doc = quickfix_with_fields(MAX_ITEMS as u32 + 1);
        let err = quickfix(&doc).unwrap_err();
        assert!(
            matches!(&err, ParseError::IndexOverflow { what, .. } if *what == "fields"),
            "{err:?}"
        );
    }

    #[test]
    fn a_dictionary_that_exactly_fills_the_field_index_space_still_parses() {
        let doc = quickfix_with_fields(MAX_ITEMS as u32);
        let parsed = quickfix(&doc).unwrap();
        assert_eq!(parsed.fields.len(), MAX_ITEMS);
    }

    #[test]
    fn the_field_count_may_fill_the_index_space_and_no_more() {
        let path = Path::new("x.xml");
        assert!(check_index_space(MAX_ITEMS, path, "fields").is_ok());
        assert!(check_index_space(MAX_ITEMS + 1, path, "fields").is_err());
    }

    #[test]
    fn an_overflow_names_the_limit_that_was_exceeded() {
        // The message used to hardcode `u16::MAX` for both checks while only
        // one of them capped there. They agree again now; the `max` field is
        // what keeps them agreeing if either moves.
        let path = Path::new("x.xml");
        let fields = check_index_space(MAX_ITEMS + 1, path, "fields").unwrap_err();
        assert!(
            fields.to_string().contains(&MAX_ITEMS.to_string()),
            "{fields}"
        );
        let messages = check_u16(u16::MAX as usize + 1, path, "messages").unwrap_err();
        assert!(
            messages.to_string().contains(&u16::MAX.to_string()),
            "{messages}"
        );
    }

    #[test]
    fn a_field_may_not_carry_more_enum_values_than_can_be_indexed() {
        let mut fields = FieldsByTag::new();
        let mut side = FieldDef {
            tag: 54,
            name: "Side".into(),
            ..FieldDef::default()
        };
        side.enums = vec![EnumDef::default(); MAX_ITEMS];
        fields.insert(54, side.clone());
        assert!(check_enum_space(&fields, Path::new("Enums.xml")).is_ok());

        side.enums.push(EnumDef::default());
        fields.insert(54, side);
        let error = check_enum_space(&fields, Path::new("Enums.xml")).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("Enums.xml"), "{text}");
        assert!(
            text.contains("54"),
            "names the tag, not just the file: {text}"
        );
    }

    /// Exists because the test above cannot tell whether the parser calls the
    /// helper. Generated rather than checked in: the point is the count, and a
    /// 3 MB fixture in the repository would be worse.
    #[test]
    fn a_quickfix_field_with_too_many_values_is_refused() {
        let mut values = String::with_capacity(MAX_ITEMS * 40);
        for i in 0..=MAX_ITEMS {
            let _ = write!(values, "<value enum='V{i}' description='D{i}'/>");
        }
        let doc = format!(
            "<fix major='4' minor='4'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields>\
             <field number='54' name='Side' type='CHAR'>{values}</field>\
             </fields></fix>"
        );
        let err = quickfix(&doc).unwrap_err();
        assert!(
            matches!(&err, ParseError::EnumOverflow { tag, .. } if *tag == 54),
            "{err:?}"
        );
    }
}

/// Dictionaries built from a string, for tests across the crate.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::{parse_quickfix_bytes, Source};
    use crate::dictionary::ParsedDict;

    pub fn source(path: &str, xml: String) -> Source {
        Source {
            path: path.into(),
            bytes: xml.into_bytes(),
        }
    }

    /// A QuickFIX dictionary for `version` declaring `fields`, and the one
    /// application message a file must declare to load.
    pub fn quickfix(version: &str, fields: &str) -> ParsedDict {
        let xml = format!("<fix version='{version}'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields>{fields}</fields></fix>");
        parse_quickfix_bytes(&source("custom.xml", xml)).unwrap()
    }

    /// One `<field>` declaration for `quickfix`.
    pub fn field(tag: u32, name: &str) -> String {
        format!("<field number='{tag}' name='{name}' type='STRING'/>")
    }
}
