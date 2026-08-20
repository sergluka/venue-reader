//! XML dictionary parsers. Shared with `build.rs` via `#[path]`, so this file may
//! depend only on `std`, `roxmltree`, and `super::model`.
//!
//! The core entry points take bytes and never touch the filesystem; the thin
//! `parse_repository`/`parse_quickfix` wrappers read files for build.rs and tests.
//! Bounded, validated reads for user-supplied dictionaries live in the registry.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use roxmltree::{Document, Node};

use super::model::{Datatype, EnumDef, FieldDef, GroupDef, MessageDef, ParsedDict};

/// Component/group nesting deeper than this is an error, not a deeper recursion.
pub const MAX_DEPTH: usize = 32;

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
    MissingRoot {
        path: PathBuf,
        expected: &'static str,
    },
    TooDeep {
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
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            ParseError::Xml { path, message } => write!(f, "{}: {message}", path.display()),
            ParseError::BadTag { path, text } => {
                write!(f, "{}: tag is not a number: {text:?}", path.display())
            }
            ParseError::MissingRoot { path, expected } => {
                write!(f, "{}: expected <{expected}> root element", path.display())
            }
            ParseError::TooDeep { path } => write!(
                f,
                "{}: component/group nesting deeper than {MAX_DEPTH}",
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

/// The files of one FIX Repository version directory. The three structure files
/// are optional as a set: groups and messages are built only when all are present.
#[derive(Debug, Clone)]
pub struct RepoSources {
    pub fields: Source,
    pub enums: Source,
    pub messages: Option<Source>,
    pub msg_contents: Option<Source>,
    pub components: Option<Source>,
}

impl RepoSources {
    pub const REQUIRED: [&'static str; 2] = ["Fields.xml", "Enums.xml"];
    pub const OPTIONAL: [&'static str; 3] = ["Messages.xml", "MsgContents.xml", "Components.xml"];

    /// Assemble the file set of `dir` through `read`, which decides how a file
    /// is read (and returns `Ok(None)` for an absent optional one). The only
    /// place that knows the repository layout.
    pub fn read_with<E>(
        dir: &Path,
        mut read: impl FnMut(&Path, bool) -> Result<Option<Source>, E>,
    ) -> Result<RepoSources, E> {
        let mut required = |name: &str| read(&dir.join(name), true).map(|s| s.expect("required"));
        let fields = required("Fields.xml")?;
        let enums = required("Enums.xml")?;
        let messages = read(&dir.join("Messages.xml"), false)?;
        let msg_contents = read(&dir.join("MsgContents.xml"), false)?;
        let components = read(&dir.join("Components.xml"), false)?;
        Ok(RepoSources {
            fields,
            enums,
            messages,
            msg_contents,
            components,
        })
    }
}

/// Convenience wrapper with plain unbounded reads — for build.rs and tests only.
pub fn parse_repository(dir: &Path) -> Result<ParsedDict, ParseError> {
    let src = RepoSources::read_with(dir, |path, required| {
        if required || path.is_file() {
            Source::read(path.to_path_buf()).map(Some)
        } else {
            Ok(None)
        }
    })?;
    parse_repository_bytes(&src)
}

pub fn parse_repository_bytes(src: &RepoSources) -> Result<ParsedDict, ParseError> {
    // Scoped so the Fields/Enums DOMs are freed before MsgContents (the largest
    // file) is parsed: peak memory is one big document, not all five.
    let (version, mut fields) = parse_fields_and_enums(src)?;

    // A repository missing its optional structure files declares no header.
    let structured = match (&src.messages, &src.msg_contents, &src.components) {
        (Some(messages), Some(contents), Some(components)) => {
            repository_structure(messages, contents, components, &fields)?
        }
        _ => Structured::default(),
    };
    fields.values_mut().for_each(|f| f.enums.shrink_to_fit());
    check_index_space(fields.len(), &src.fields.path, "fields")?;
    check_enum_space(&fields, &src.enums.path)?;

    Ok(ParsedDict {
        version,
        fields: fields.into_values().collect(),
        header: structured.header,
        trailer: structured.trailer,
        groups: structured.groups,
        messages: structured.messages,
    })
}

type FieldsByTag = BTreeMap<u32, FieldDef>;

fn parse_fields_and_enums(src: &RepoSources) -> Result<(Option<String>, FieldsByTag), ParseError> {
    let fields_doc = parse_doc(&src.fields)?;
    let fields_root = expect_root(&fields_doc, "Fields", &src.fields.path)?;
    let version = fields_root.attribute("version").map(str::to_owned);

    // Duplicate tags: last definition wins.
    let mut fields: BTreeMap<u32, FieldDef> = BTreeMap::new();
    for node in elements(fields_root, "Field") {
        let tag = parse_tag(&child_text(node, "Tag"), &src.fields.path)?;
        fields.insert(tag, {
            let datatype = child_text(node, "Type");
            FieldDef {
                tag,
                name: child_text(node, "Name"),
                datatype_class: Datatype::of(&datatype),
                datatype,
                description: child_text(node, "Description"),
                added: attr(node, "added"),
                deprecated: attr(node, "deprecated"),
                added_ep: 0,
                enums: Vec::new(),
            }
        });
    }

    let enums_doc = parse_doc(&src.enums)?;
    let enums_root = expect_root(&enums_doc, "Enums", &src.enums.path)?;
    for node in elements(enums_root, "Enum") {
        let tag = parse_tag(&child_text(node, "Tag"), &src.enums.path)?;
        // An enum whose tag has no field definition has nowhere to live: dropped.
        let Some(field) = fields.get_mut(&tag) else {
            continue;
        };
        upsert_enum(
            &mut field.enums,
            EnumDef {
                value: child_text(node, "Value"),
                name: child_text(node, "SymbolicName"),
                description: child_text(node, "Description"),
                added: attr(node, "added"),
                added_ep: 0,
            },
        );
    }
    Ok((version, fields))
}

// ---------------------------------------------------------------------------
// Repository message structure: Messages.xml + MsgContents.xml + Components.xml

/// A message's content before arena interning.
#[derive(Debug, Clone)]
enum Child {
    Field { tag: u32, indent: i64 },
    Group { group: GroupTree, indent: i64 },
}

impl Child {
    fn indent(&self) -> i64 {
        match self {
            Child::Field { indent, .. } | Child::Group { indent, .. } => *indent,
        }
    }

    /// The tag this child contributes to its parent's member list.
    fn member_tag(&self) -> u32 {
        match self {
            Child::Field { tag, .. } => *tag,
            Child::Group { group, .. } => group.count_tag,
        }
    }
}

#[derive(Debug, Clone)]
struct GroupTree {
    name: String,
    count_tag: u32,
    delimiter_tag: u32,
    members: Vec<u32>,
    nested: Vec<GroupTree>,
}

/// Build a group from its children; `None` when there are none (no delimiter).
fn new_group(name: &str, count_tag: u32, children: Vec<Child>) -> Option<GroupTree> {
    let delimiter_tag = children.first()?.member_tag();
    let mut members = Vec::new();
    for child in &children {
        let tag = child.member_tag();
        if !members.contains(&tag) {
            members.push(tag);
        }
    }
    let nested = children
        .into_iter()
        .filter_map(|c| match c {
            Child::Group { group, .. } => Some(group),
            Child::Field { .. } => None,
        })
        .collect();
    Some(GroupTree {
        name: name.to_owned(),
        count_tag,
        delimiter_tag,
        members,
        nested,
    })
}

/// Every tag a child list reaches: fields, group counters, and the members of
/// nested groups. `Child::member_tag` alone would stop at a group's counter.
///
/// **Sorted**, and `Dict::is_header` binary-searches it.
fn reached_tags(children: &[Child]) -> Vec<u32> {
    fn walk(children: &[Child], out: &mut Vec<u32>) {
        for child in children {
            match child {
                Child::Field { tag, .. } => out.push(*tag),
                Child::Group { group, .. } => group_tags(group, out),
            }
        }
    }
    fn group_tags(group: &GroupTree, out: &mut Vec<u32>) {
        out.push(group.count_tag);
        out.extend(&group.members);
        for nested in &group.nested {
            group_tags(nested, out);
        }
    }
    let mut out = Vec::new();
    walk(children, &mut out);
    out.sort_unstable();
    out.dedup();
    out
}

/// Old versions type group counters as plain `int`, so the `No…` name prefix is
/// part of the rule, not just `NumInGroup`.
fn can_count_group(field: Option<&FieldDef>) -> bool {
    field.is_some_and(|f| f.datatype_class == Datatype::NumInGroup || f.name.starts_with("No"))
}

/// Re-nest a flat, indent-annotated child list: a counter field followed by
/// deeper-indented children becomes a group; a counter with no children stays a field.
fn grouped_by_indent(
    children: &[Child],
    start: usize,
    parent_indent: i64,
    fields: &BTreeMap<u32, FieldDef>,
) -> (Vec<Child>, usize) {
    let mut grouped = Vec::new();
    let mut index = start;
    while index < children.len() {
        let child = &children[index];
        let indent = child.indent();
        if indent <= parent_indent {
            break;
        }
        if let Child::Field { tag, .. } = child {
            if can_count_group(fields.get(tag)) {
                let (nested, next) = grouped_by_indent(children, index + 1, indent, fields);
                if !nested.is_empty() {
                    if let Some(group) = new_group(&fields[tag].name, *tag, nested) {
                        grouped.push(Child::Group { group, indent });
                    }
                    index = next;
                    continue;
                }
            }
        }
        grouped.push(child.clone());
        index += 1;
    }
    (grouped, index)
}

struct ComponentInfo {
    id: i64,
    component_type: String,
}

struct Content {
    tag_text: String,
    indent: i64,
    /// Fractional in the data (`13.1` sits between 13 and 14).
    position: f64,
}

struct Structure<'a> {
    components_by_name: HashMap<String, ComponentInfo>,
    contents_by_component: HashMap<i64, Vec<Content>>,
    fields: &'a BTreeMap<u32, FieldDef>,
    path: &'a Path,
}

impl Structure<'_> {
    /// Expand a component's contents into children; cycle-guarded (a component may
    /// appear on several branches, but never inside itself) and depth-capped.
    fn component_children(
        &self,
        component_id: i64,
        seen: &mut HashSet<i64>,
        depth: usize,
        base_indent: i64,
    ) -> Result<Vec<Child>, ParseError> {
        if depth > MAX_DEPTH {
            return Err(ParseError::TooDeep {
                path: self.path.to_path_buf(),
            });
        }
        if !seen.insert(component_id) {
            return Ok(Vec::new());
        }
        let mut children = Vec::new();
        for content in self
            .contents_by_component
            .get(&component_id)
            .into_iter()
            .flatten()
        {
            let indent = base_indent + content.indent;
            if let Ok(tag) = content.tag_text.trim().parse::<u32>() {
                children.push(Child::Field { tag, indent });
                continue;
            }
            let Some(component) = self.components_by_name.get(&content.tag_text) else {
                continue;
            };
            let mut nested = self.component_children(component.id, seen, depth + 1, indent)?;
            if component.component_type.contains("Repeating") {
                // The first entry of a repeating component is its counter.
                if nested.is_empty() {
                    continue;
                }
                let count = nested.remove(0).member_tag();
                if let Some(field) = self.fields.get(&count) {
                    if let Some(group) = new_group(&field.name, count, nested) {
                        children.push(Child::Group { group, indent });
                    }
                }
            } else {
                children.append(&mut nested);
            }
        }
        seen.remove(&component_id);
        Ok(children)
    }
}

/// What a repository's message section yields, beyond the fields.
#[derive(Default)]
struct Structured {
    groups: Vec<GroupDef>,
    messages: Vec<MessageDef>,
    header: Vec<u32>,
    trailer: Vec<u32>,
}

fn repository_structure(
    messages: &Source,
    contents: &Source,
    components: &Source,
    fields: &BTreeMap<u32, FieldDef>,
) -> Result<Structured, ParseError> {
    let components_doc = parse_doc(components)?;
    let components_root = expect_root(&components_doc, "Components", &components.path)?;
    let mut components_by_name = HashMap::new();
    for node in elements(components_root, "Component") {
        let Ok(id) = child_text(node, "ComponentID").trim().parse::<i64>() else {
            continue;
        };
        let name = child_text(node, "Name");
        if name.is_empty() {
            continue;
        }
        components_by_name.insert(
            name,
            ComponentInfo {
                id,
                component_type: child_text(node, "ComponentType"),
            },
        );
    }

    let contents_doc = parse_doc(contents)?;
    let contents_root = expect_root(&contents_doc, "MsgContents", &contents.path)?;
    let mut contents_by_component: HashMap<i64, Vec<Content>> = HashMap::new();
    for node in elements(contents_root, "MsgContent") {
        let Ok(component_id) = child_text(node, "ComponentID").trim().parse::<i64>() else {
            continue;
        };
        let tag_text = child_text(node, "TagText");
        if tag_text.is_empty() {
            continue;
        }
        contents_by_component
            .entry(component_id)
            .or_default()
            .push(Content {
                tag_text,
                indent: child_text(node, "Indent").trim().parse().unwrap_or(0),
                position: child_text(node, "Position").trim().parse().unwrap_or(0.0),
            });
    }
    for list in contents_by_component.values_mut() {
        list.sort_by(|a, b| a.position.total_cmp(&b.position));
    }

    let structure = Structure {
        components_by_name,
        contents_by_component,
        fields,
        path: &contents.path,
    };

    let messages_doc = parse_doc(messages)?;
    let messages_root = expect_root(&messages_doc, "Messages", &messages.path)?;
    let mut arena = GroupArena::new(&messages.path);
    // Duplicate message types: last definition wins.
    let mut defs: BTreeMap<String, MessageDef> = BTreeMap::new();
    for node in elements(messages_root, "Message") {
        let msg_type = child_text(node, "MsgType");
        let name = child_text(node, "Name");
        if msg_type.is_empty() || name.is_empty() {
            continue;
        }
        let mut groups = Vec::new();
        if let Ok(component_id) = child_text(node, "ComponentID").trim().parse::<i64>() {
            let children = structure.component_children(component_id, &mut HashSet::new(), 0, 0)?;
            let (grouped, _) = grouped_by_indent(&children, 0, -1, fields);
            for child in grouped {
                if let Child::Group { group, .. } = child {
                    groups.push(arena.intern(&group)?);
                }
            }
        }
        defs.insert(
            msg_type.clone(),
            MessageDef {
                msg_type,
                name,
                category: child_text(node, "CategoryID"),
                description: child_text(node, "Description"),
                groups,
            },
        );
    }
    check_u16(defs.len(), &messages.path, "messages")?;

    let session_block = |name: &str| match structure.components_by_name.get(name) {
        Some(component) => structure
            .component_children(component.id, &mut HashSet::new(), 0, 0)
            .map(|children| reached_tags(&children)),
        None => Ok(Vec::new()),
    };
    Ok(Structured {
        groups: arena.groups,
        messages: defs.into_values().collect(),
        header: session_block("StandardHeader")?,
        trailer: session_block("StandardTrailer")?,
    })
}

/// Structurally deduplicating arena: the same component referenced from many
/// messages becomes one `GroupDef`.
struct GroupArena<'a> {
    groups: Vec<GroupDef>,
    index: HashMap<GroupDef, u16>,
    path: &'a Path,
}

impl<'a> GroupArena<'a> {
    fn new(path: &'a Path) -> Self {
        GroupArena {
            groups: Vec::new(),
            index: HashMap::new(),
            path,
        }
    }

    fn intern(&mut self, tree: &GroupTree) -> Result<u16, ParseError> {
        let mut nested = Vec::with_capacity(tree.nested.len());
        for child in &tree.nested {
            nested.push(self.intern(child)?);
        }
        self.push(GroupDef {
            name: tree.name.clone(),
            count_tag: tree.count_tag,
            delimiter_tag: tree.delimiter_tag,
            members: tree.members.clone(),
            nested,
        })
    }

    fn push(&mut self, def: GroupDef) -> Result<u16, ParseError> {
        if let Some(&idx) = self.index.get(&def) {
            return Ok(idx);
        }
        check_u16(self.groups.len(), self.path, "groups")?;
        let idx = self.groups.len() as u16;
        self.groups.push(def.clone());
        self.index.insert(def, idx);
        Ok(idx)
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
    for node in elements_ci(fields_root, "field") {
        let tag_text = attr_any(node, &["number", "tag"]);
        let tag = parse_tag(&tag_text, &src.path)?;
        let datatype = attr(node, "type");
        let mut def = FieldDef {
            tag,
            name: attr(node, "name"),
            datatype_class: Datatype::of(&datatype),
            datatype,
            description: attr(node, "description"),
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
        fields.insert(tag, def);
    }

    let by_name: HashMap<String, u32> = fields.values().map(|f| (f.name.clone(), f.tag)).collect();
    let components: HashMap<String, Node<'_, '_>> = section(root, "components")
        .into_iter()
        .flat_map(|c| elements_ci(c, "component"))
        .map(|c| (attr(c, "name"), c))
        .collect();
    let expander = QuickfixExpander {
        by_name: &by_name,
        components: &components,
        path: &src.path,
    };

    let mut arena = GroupArena::new(&src.path);
    let mut messages: BTreeMap<String, MessageDef> = BTreeMap::new();
    for node in section(root, "messages")
        .into_iter()
        .flat_map(|m| elements_ci(m, "message"))
    {
        let msg_type = attr(node, "msgtype");
        let name = attr(node, "name");
        if msg_type.is_empty() || name.is_empty() {
            continue;
        }
        // QuickFIX carries no message descriptions; the MsgType enum is derived.
        if let Some(field) = fields.get_mut(&35) {
            upsert_enum(
                &mut field.enums,
                EnumDef {
                    value: msg_type.clone(),
                    name: name.clone(),
                    description: name.clone(),
                    added: String::new(),
                    added_ep: 0,
                },
            );
        }
        let mut groups = Vec::new();
        for child in expander.children(node, &mut HashSet::new(), 0)? {
            if let Child::Group { group, .. } = child {
                groups.push(arena.intern(&group)?);
            }
        }
        messages.insert(
            msg_type.clone(),
            MessageDef {
                msg_type,
                name,
                category: attr(node, "msgcat"),
                description: String::new(),
                groups,
            },
        );
    }
    check_u16(messages.len(), &src.path, "messages")?;
    check_index_space(fields.len(), &src.path, "fields")?;
    check_enum_space(&fields, &src.path)?;

    let session_block = |name: &str| match section(root, name) {
        Some(node) => expander
            .children(node, &mut HashSet::new(), 0)
            .map(|children| reached_tags(&children)),
        None => Ok(Vec::new()),
    };

    Ok(ParsedDict {
        version: quickfix_version(root),
        fields: fields.into_values().collect(),
        header: session_block("header")?,
        trailer: session_block("trailer")?,
        groups: arena.groups,
        messages: messages.into_values().collect(),
    })
}

struct QuickfixExpander<'a, 'i> {
    by_name: &'a HashMap<String, u32>,
    components: &'a HashMap<String, Node<'a, 'i>>,
    path: &'a Path,
}

impl QuickfixExpander<'_, '_> {
    /// Children of a message/group/component node with `<component>` references
    /// spliced in (cycle-guarded, depth-capped). Unknown field/group names and
    /// groups left without children are dropped.
    fn children(
        &self,
        node: Node<'_, '_>,
        seen: &mut HashSet<String>,
        depth: usize,
    ) -> Result<Vec<Child>, ParseError> {
        if depth > MAX_DEPTH {
            return Err(ParseError::TooDeep {
                path: self.path.to_path_buf(),
            });
        }
        let mut out = Vec::new();
        for child in node.children().filter(|n| n.is_element()) {
            let name = attr(child, "name");
            let kind = child.tag_name().name().to_ascii_lowercase();
            match kind.as_str() {
                "field" => {
                    if let Some(&tag) = self.by_name.get(&name) {
                        out.push(Child::Field { tag, indent: 0 });
                    }
                }
                "group" => {
                    let Some(&count_tag) = self.by_name.get(&name) else {
                        continue;
                    };
                    let members = self.children(child, seen, depth + 1)?;
                    if let Some(group) = new_group(&name, count_tag, members) {
                        out.push(Child::Group { group, indent: 0 });
                    }
                }
                "component" => {
                    let Some(&component) = self.components.get(&name) else {
                        continue;
                    };
                    if !seen.insert(name.clone()) {
                        continue;
                    }
                    let mut nested = self.children(component, seen, depth + 1)?;
                    seen.remove(&name);
                    out.append(&mut nested);
                }
                _ => {}
            }
        }
        Ok(out)
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

fn expect_root<'a, 'i>(
    doc: &'a Document<'i>,
    expected: &'static str,
    path: &Path,
) -> Result<Node<'a, 'i>, ParseError> {
    let root = doc.root_element();
    if root.has_tag_name(expected) {
        Ok(root)
    } else {
        Err(ParseError::MissingRoot {
            path: path.to_path_buf(),
            expected,
        })
    }
}

fn elements<'a, 'i: 'a>(node: Node<'a, 'i>, name: &'a str) -> impl Iterator<Item = Node<'a, 'i>> {
    node.children()
        .filter(move |n| n.is_element() && n.has_tag_name(name))
}

/// Text of the first child element `name`, or empty.
fn child_text<'a>(node: Node<'a, '_>, name: &'a str) -> String {
    elements(node, name)
        .next()
        .map(|n| n.children().filter_map(|c| c.text()).collect())
        .unwrap_or_default()
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
/// fields rather than at either source, because the repository parser merges
/// values from a second file into a field defined in the first.
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
    use std::fmt::Write as _;

    const FIX44: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/xml/custom/synthetic/repository/FIX.4.4/Base"
    );

    fn src(name: &str, xml: &str) -> Source {
        Source {
            path: PathBuf::from(name),
            bytes: xml.as_bytes().to_vec(),
        }
    }

    fn fields_enums(fields: &str, enums: &str) -> RepoSources {
        RepoSources {
            fields: src("Fields.xml", fields),
            enums: src("Enums.xml", enums),
            messages: None,
            msg_contents: None,
            components: None,
        }
    }

    fn field_xml(tag: u32, name: &str, datatype: &str) -> String {
        format!("<Field added=\"FIX.4.0\"><Tag>{tag}</Tag><Name>{name}</Name><Type>{datatype}</Type><Description>d{tag}</Description></Field>")
    }

    #[test]
    fn fix44_field_8_is_begin_string() {
        let dict = parse_repository(Path::new(FIX44)).unwrap();
        let f = dict.field(8).unwrap();
        assert_eq!(f.name, "BeginString");
        assert_eq!(f.datatype, "String");
        assert_eq!(f.description, "Synthetic description of BeginString.");
        assert_eq!(f.added, "FIX.4.0");
        assert!(f.deprecated.is_empty());
    }

    #[test]
    fn a_repository_header_reaches_into_repeating_components() {
        let dict = parse_repository(Path::new(FIX44)).unwrap();
        assert_eq!(dict.header, vec![8, 9, 34, 35, 49, 52, 56, 627, 628]);
        assert_eq!(dict.trailer, vec![10]);
    }

    #[test]
    fn fix44_enum_54_1_is_buy() {
        let dict = parse_repository(Path::new(FIX44)).unwrap();
        let side = dict.field(54).unwrap();
        let buy = side.enums.iter().find(|e| e.value == "1").unwrap();
        assert_eq!(buy.name, "Buy");
    }

    #[test]
    fn fix44_message_d_is_new_order_single() {
        let dict = parse_repository(Path::new(FIX44)).unwrap();
        let d = dict.message("D").unwrap();
        assert!(d.name.contains("OrderSingle"), "{}", d.name);
        assert!(!d.category.is_empty());
        assert!(!d.description.is_empty());
        assert!(dict.message("ZZZZ").is_none());
    }

    #[test]
    fn fix44_market_data_has_no_md_entries_group() {
        let dict = parse_repository(Path::new(FIX44)).unwrap();
        let w = dict.message("W").unwrap();
        let md = w
            .groups
            .iter()
            .map(|&i| &dict.groups[usize::from(i)])
            .find(|g| g.count_tag == 268)
            .expect("NoMDEntries group");
        assert_eq!(md.name, "NoMDEntries");
        assert_eq!(md.delimiter_tag, 269);
        assert!(md.members.contains(&270));
        assert!(!md.members.contains(&268));
    }

    #[test]
    fn fields_are_sorted_and_unique_by_tag() {
        let dict = parse_repository(Path::new(FIX44)).unwrap();
        assert!(dict.fields.windows(2).all(|w| w[0].tag < w[1].tag));
        assert!(dict
            .messages
            .windows(2)
            .all(|w| w[0].msg_type < w[1].msg_type));
        assert!(!dict.groups.is_empty());
    }

    #[test]
    fn missing_msg_contents_degrades_to_no_groups() {
        let full = parse_repository(Path::new(FIX44)).unwrap();
        let src = RepoSources {
            fields: Source::read(Path::new(FIX44).join("Fields.xml")).unwrap(),
            enums: Source::read(Path::new(FIX44).join("Enums.xml")).unwrap(),
            messages: Some(Source::read(Path::new(FIX44).join("Messages.xml")).unwrap()),
            msg_contents: None,
            components: Some(Source::read(Path::new(FIX44).join("Components.xml")).unwrap()),
        };
        let dict = parse_repository_bytes(&src).unwrap();
        assert!(dict.groups.is_empty());
        assert!(dict.messages.is_empty());
        assert_eq!(dict.fields, full.fields);
    }

    #[test]
    fn version_attribute_is_captured() {
        let dict = parse_repository(Path::new(FIX44)).unwrap();
        assert_eq!(dict.version.as_deref(), Some("FIX.4.4"));
        let none = parse_repository_bytes(&fields_enums("<Fields></Fields>", "<Enums/>")).unwrap();
        assert_eq!(none.version, None);
    }

    #[test]
    fn dtd_is_rejected() {
        let xml = "<!DOCTYPE Fields [<!ENTITY x \"y\">]><Fields version=\"FIX.4.4\"></Fields>";
        let err = parse_repository_bytes(&fields_enums(xml, "<Enums/>")).unwrap_err();
        assert!(matches!(err, ParseError::Xml { .. }), "{err}");
    }

    #[test]
    fn missing_root_is_reported() {
        let err = parse_repository_bytes(&fields_enums("<Nope/>", "<Enums/>")).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::MissingRoot {
                    expected: "Fields",
                    ..
                }
            ),
            "{err}"
        );
        let err = parse_repository_bytes(&fields_enums("<Fields/>", "<Nope/>")).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::MissingRoot {
                    expected: "Enums",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn non_numeric_tag_is_bad_tag() {
        let xml = "<Fields><Field><Tag>abc</Tag><Name>X</Name></Field></Fields>";
        let err = parse_repository_bytes(&fields_enums(xml, "<Enums/>")).unwrap_err();
        assert!(
            matches!(err, ParseError::BadTag { ref text, .. } if text == "abc"),
            "{err}"
        );
    }

    #[test]
    fn duplicate_tag_last_wins() {
        let xml = format!(
            "<Fields>{}{}</Fields>",
            field_xml(8, "Old", "char"),
            field_xml(8, "BeginString", "String")
        );
        let dict = parse_repository_bytes(&fields_enums(&xml, "<Enums/>")).unwrap();
        assert_eq!(dict.fields.len(), 1);
        assert_eq!(dict.field(8).unwrap().name, "BeginString");
    }

    #[test]
    fn orphan_enum_is_dropped_and_duplicates_last_win() {
        let fields = format!("<Fields>{}</Fields>", field_xml(54, "Side", "char"));
        let enums = "<Enums>\
            <Enum added=\"FIX.2.7\"><Tag>54</Tag><Value>1</Value><SymbolicName>Old</SymbolicName><Description>o</Description></Enum>\
            <Enum><Tag>54</Tag><Value>1</Value><SymbolicName>Buy</SymbolicName><Description>Buy</Description></Enum>\
            <Enum><Tag>54</Tag><Value>0</Value><SymbolicName>Zero</SymbolicName><Description>z</Description></Enum>\
            <Enum><Tag>999</Tag><Value>1</Value><SymbolicName>Orphan</SymbolicName><Description>x</Description></Enum>\
            </Enums>";
        let dict = parse_repository_bytes(&fields_enums(&fields, enums)).unwrap();
        let side = dict.field(54).unwrap();
        assert_eq!(side.enums.len(), 2);
        assert_eq!(side.enums[0].value, "0"); // sorted by value
        assert_eq!(side.enums[1].name, "Buy");
        assert_eq!(side.enums[1].added, "");
        assert!(dict.field(999).is_none());
    }

    /// A chain of N non-repeating components, each containing the next.
    fn component_chain(n: usize) -> RepoSources {
        let fields = format!("<Fields>{}</Fields>", field_xml(1, "Account", "String"));
        let mut components = String::from("<Components>");
        let mut contents = String::from("<MsgContents>");
        for i in 1..=n {
            components.push_str(&format!("<Component><ComponentID>{i}</ComponentID><ComponentType>Block</ComponentType><Name>C{i}</Name></Component>"));
            let child = if i < n {
                format!("C{}", i + 1)
            } else {
                "1".to_owned()
            };
            contents.push_str(&format!("<MsgContent><ComponentID>{i}</ComponentID><TagText>{child}</TagText><Indent>0</Indent><Position>1</Position></MsgContent>"));
        }
        components.push_str("</Components>");
        contents.push_str("</MsgContents>");
        let messages = "<Messages><Message><ComponentID>1</ComponentID><MsgType>X</MsgType><Name>Chain</Name><CategoryID>T</CategoryID></Message></Messages>";
        RepoSources {
            fields: src("Fields.xml", &fields),
            enums: src("Enums.xml", "<Enums/>"),
            messages: Some(src("Messages.xml", messages)),
            msg_contents: Some(src("MsgContents.xml", &contents)),
            components: Some(src("Components.xml", &components)),
        }
    }

    #[test]
    fn deep_component_nesting_errors_not_overflows() {
        let err = parse_repository_bytes(&component_chain(MAX_DEPTH + 5)).unwrap_err();
        assert!(matches!(err, ParseError::TooDeep { .. }), "{err}");
        let ok = parse_repository_bytes(&component_chain(5)).unwrap();
        assert_eq!(ok.message("X").unwrap().name, "Chain");
    }

    #[test]
    fn component_cycle_is_cut_not_infinite() {
        let fields = format!(
            "<Fields>{}{}</Fields>",
            field_xml(1, "Account", "String"),
            field_xml(2, "AdvId", "String")
        );
        let components = "<Components>\
            <Component><ComponentID>1</ComponentID><ComponentType>Block</ComponentType><Name>A</Name></Component>\
            <Component><ComponentID>2</ComponentID><ComponentType>Block</ComponentType><Name>B</Name></Component>\
            </Components>";
        let contents = "<MsgContents>\
            <MsgContent><ComponentID>1</ComponentID><TagText>1</TagText><Indent>0</Indent><Position>1</Position></MsgContent>\
            <MsgContent><ComponentID>1</ComponentID><TagText>B</TagText><Indent>0</Indent><Position>2</Position></MsgContent>\
            <MsgContent><ComponentID>2</ComponentID><TagText>2</TagText><Indent>0</Indent><Position>1</Position></MsgContent>\
            <MsgContent><ComponentID>2</ComponentID><TagText>A</TagText><Indent>0</Indent><Position>2</Position></MsgContent>\
            </MsgContents>";
        let messages = "<Messages><Message><ComponentID>1</ComponentID><MsgType>X</MsgType><Name>Cyc</Name></Message></Messages>";
        let src = RepoSources {
            fields: src("Fields.xml", &fields),
            enums: src("Enums.xml", "<Enums/>"),
            messages: Some(src("Messages.xml", messages)),
            msg_contents: Some(src("MsgContents.xml", contents)),
            components: Some(src("Components.xml", components)),
        };
        let dict = parse_repository_bytes(&src).unwrap();
        assert_eq!(dict.message("X").unwrap().name, "Cyc");
    }

    #[test]
    fn repeating_component_becomes_group_with_nested_group() {
        // Message M → NoSides group (552) containing 54 and a nested NoPartyIDs group (453) with 448.
        let fields = format!(
            "<Fields>{}{}{}{}</Fields>",
            field_xml(552, "NoSides", "NumInGroup"),
            field_xml(54, "Side", "char"),
            field_xml(453, "NoPartyIDs", "int"),
            field_xml(448, "PartyID", "String")
        );
        let components = "<Components>\
            <Component><ComponentID>10</ComponentID><ComponentType>ImplicitBlockRepeating</ComponentType><Name>SideGrp</Name></Component>\
            <Component><ComponentID>20</ComponentID><ComponentType>BlockRepeating</ComponentType><Name>Parties</Name></Component>\
            </Components>";
        let contents = "<MsgContents>\
            <MsgContent><ComponentID>1</ComponentID><TagText>SideGrp</TagText><Indent>0</Indent><Position>1</Position></MsgContent>\
            <MsgContent><ComponentID>10</ComponentID><TagText>552</TagText><Indent>0</Indent><Position>1</Position></MsgContent>\
            <MsgContent><ComponentID>10</ComponentID><TagText>54</TagText><Indent>1</Indent><Position>2</Position></MsgContent>\
            <MsgContent><ComponentID>10</ComponentID><TagText>Parties</TagText><Indent>1</Indent><Position>3</Position></MsgContent>\
            <MsgContent><ComponentID>20</ComponentID><TagText>453</TagText><Indent>0</Indent><Position>1</Position></MsgContent>\
            <MsgContent><ComponentID>20</ComponentID><TagText>448</TagText><Indent>1</Indent><Position>2</Position></MsgContent>\
            </MsgContents>";
        let messages = "<Messages>\
            <Message><ComponentID>1</ComponentID><MsgType>M</MsgType><Name>Multi</Name></Message>\
            <Message><ComponentID>1</ComponentID><MsgType>N</MsgType><Name>Same</Name></Message>\
            </Messages>";
        let src = RepoSources {
            fields: src("Fields.xml", &fields),
            enums: src("Enums.xml", "<Enums/>"),
            messages: Some(src("Messages.xml", messages)),
            msg_contents: Some(src("MsgContents.xml", contents)),
            components: Some(src("Components.xml", components)),
        };
        let dict = parse_repository_bytes(&src).unwrap();
        let m = dict.message("M").unwrap();
        assert_eq!(m.groups.len(), 1);
        let sides = &dict.groups[usize::from(m.groups[0])];
        assert_eq!(
            (sides.name.as_str(), sides.count_tag, sides.delimiter_tag),
            ("NoSides", 552, 54)
        );
        assert_eq!(sides.members, vec![54, 453]);
        assert_eq!(sides.nested.len(), 1);
        let parties = &dict.groups[usize::from(sides.nested[0])];
        assert_eq!((parties.count_tag, parties.delimiter_tag), (453, 448));
        assert_eq!(parties.members, vec![448]);
        // Same component from two messages → one arena entry each.
        assert_eq!(dict.groups.len(), 2);
        assert_eq!(dict.message("N").unwrap().groups, m.groups);
    }

    // --- QuickFIX -----------------------------------------------------------

    const CUSTOM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/xml/custom");

    fn quickfix(xml: &str) -> Result<ParsedDict, ParseError> {
        parse_quickfix_bytes(&src("dict.xml", xml))
    }

    fn group_of<'a>(dict: &'a ParsedDict, msg_type: &str, count_tag: u32) -> Option<&'a GroupDef> {
        dict.message(msg_type)?
            .groups
            .iter()
            .map(|&i| &dict.groups[usize::from(i)])
            .find(|g| g.count_tag == count_tag)
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
        let parties = group_of(&dict, "D", 453).expect("NoPartyIDs");
        assert_eq!(parties.delimiter_tag, 448);
        assert_eq!(parties.members, vec![448, 452, 802]);
        let subs = &dict.groups[usize::from(parties.nested[0])];
        assert_eq!((subs.count_tag, subs.delimiter_tag), (802, 523));
        // The Instrument component is spliced inline: not a group.
        assert_eq!(d.groups.len(), 1);
        assert_eq!(dict.message("8").unwrap().groups.len(), 0);
    }

    #[test]
    fn md_fix50_group_inside_component_is_expanded() {
        // Message `y` reaches NoRelatedSym only through <component name='SecListGrp'/>;
        // the group's own first member comes from a nested <component name='Instrument'/>.
        let dict = parse_quickfix(&Path::new(CUSTOM).join("synthetic/md-fix50.xml")).unwrap();
        assert_eq!(dict.version.as_deref(), Some("FIX.5.0"));
        let related = group_of(&dict, "y", 146).expect("NoRelatedSym via SecListGrp");
        assert_eq!(related.name, "NoRelatedSym");
        assert_eq!(related.delimiter_tag, 55);
        assert_eq!(related.members, vec![55, 167, 562]);
        // Message V declares NoRelatedSym directly — present as well, but not the point.
        assert_eq!(group_of(&dict, "V", 146).unwrap().members, vec![55]);
        // W reaches NoMDEntries through MDFullGrp; MDReqGrp is defined but unused.
        assert!(group_of(&dict, "W", 268).is_some());
        assert!(dict.groups.iter().all(|g| g.count_tag != 267));
    }

    #[test]
    fn quickfix_enum_name_comes_from_description_attr() {
        let dict = quickfix(
            "<fix major='4' minor='4'><fields>\
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
            "<FIX major='4' minor='2'><Fields>\
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
        let err = quickfix("<fix><fields><field number='x' name='A'/></fields></fix>").unwrap_err();
        assert!(matches!(err, ParseError::BadTag { .. }), "{err}");
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
        assert_eq!(dict.header, vec![8, 35, 627, 628]);
        assert_eq!(dict.trailer, vec![10]);

        let none = quickfix(
            "<fix major='4' minor='4'><fields><field number='1' name='A' type='STRING'/></fields></fix>",
        )
        .unwrap();
        assert!(none.header.is_empty() && none.trailer.is_empty());
    }

    /// The counter is typed `int` in older files, so the name still decides.
    #[test]
    fn a_datatype_class_comes_from_the_spelling_whatever_its_case() {
        let dict = quickfix(
            "<fix major='4' minor='4'>\
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

    #[test]
    fn empty_group_is_dropped() {
        let dict = quickfix(
            "<fix major='4' minor='4'>\
             <messages><message name='X' msgtype='X'><group name='NoX' required='N'/><group name='Unknown'><field name='A'/></group></message></messages>\
             <fields><field number='1' name='A' type='STRING'/><field number='2' name='NoX' type='NUMINGROUP'/></fields></fix>",
        )
        .unwrap();
        assert!(dict.message("X").unwrap().groups.is_empty());
        assert!(dict.groups.is_empty());
    }

    #[test]
    fn quickfix_component_cycle_is_cut_and_depth_capped() {
        let dict = quickfix(
            "<fix major='4' minor='4'>\
             <messages><message name='X' msgtype='X'><component name='A'/></message></messages>\
             <components>\
               <component name='A'><field name='F1'/><component name='B'/></component>\
               <component name='B'><group name='NoG'><field name='F2'/><component name='A'/></group></component>\
             </components>\
             <fields><field number='1' name='F1'/><field number='2' name='F2'/><field number='3' name='NoG'/></fields></fix>",
        )
        .unwrap();
        let g = group_of(&dict, "X", 3).unwrap();
        assert_eq!(g.delimiter_tag, 2);
        assert_eq!(g.members, vec![2]);

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

    #[test]
    fn version_from_header_attrs() {
        let v = |xml: &str| quickfix(xml).unwrap().version;
        assert_eq!(
            v("<fix type='FIXT' major='1' minor='1' servicepack='0'><fields/></fix>").as_deref(),
            Some("FIXT.1.1")
        );
        assert_eq!(
            v("<fix major='5' minor='0' servicepack='2'><fields/></fix>").as_deref(),
            Some("FIX.5.0SP2")
        );
        assert_eq!(
            v("<fix major='4' minor='4' servicePack='0'><fields/></fix>").as_deref(),
            Some("FIX.4.4")
        );
        assert_eq!(
            v("<fix version='FIX.9.9'><fields/></fix>").as_deref(),
            Some("FIX.9.9")
        );
        // Runtime loading rejects absent or invalid XML version metadata.
        assert_eq!(v("<fix><fields/></fix>"), None);
        assert_eq!(
            v("<fix major='4' minor='4' servicepack='bad'><fields/></fix>"),
            None
        );
    }

    fn quickfix_with_fields(count: u32) -> String {
        let mut doc = String::with_capacity(count as usize * 48 + 64);
        doc.push_str("<fix major='4' minor='4'><fields>");
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

    /// The two below exist because the test above cannot tell whether either
    /// parser calls the helper. Generated rather than checked in: the point is
    /// the count, and a 3 MB fixture in the repository would be worse.
    #[test]
    fn a_quickfix_field_with_too_many_values_is_refused() {
        let mut values = String::with_capacity(MAX_ITEMS * 40);
        for i in 0..=MAX_ITEMS {
            let _ = write!(values, "<value enum='V{i}' description='D{i}'/>");
        }
        let doc = format!(
            "<fix major='4' minor='4'><fields>\
             <field number='54' name='Side' type='CHAR'>{values}</field>\
             </fields></fix>"
        );
        let err = quickfix(&doc).unwrap_err();
        assert!(
            matches!(&err, ParseError::EnumOverflow { tag, .. } if *tag == 54),
            "{err:?}"
        );
    }

    #[test]
    fn a_repository_field_with_too_many_values_is_refused() {
        let mut enums = String::with_capacity(MAX_ITEMS * 70);
        for i in 0..=MAX_ITEMS {
            let _ = write!(
                enums,
                "<Enum><Tag>54</Tag><Value>V{i}</Value><SymbolicName>S{i}</SymbolicName></Enum>"
            );
        }
        let src = fields_enums(
            "<Fields><Field><Tag>54</Tag><Name>Side</Name><Type>char</Type></Field></Fields>",
            &format!("<Enums>{enums}</Enums>"),
        );
        let err = parse_repository_bytes(&src).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("54"), "{text}");
        assert!(
            text.contains("Enums"),
            "points at the file the values are in: {text}"
        );
    }
}
