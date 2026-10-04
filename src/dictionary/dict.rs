//! `Version`, the bundled lookup, and `Dict` — one lookup API over bundled
//! static tables and runtime-parsed custom dictionaries. Lookups return `Copy`
//! indices; strings are fetched through views at response time, so cached
//! analysis never borrows from an `Owned` dictionary.

use std::sync::Arc;

use super::bundle::release_of;
use super::model::{Datatype, EnumDef, FieldDef, GroupDef, MessageDef, ParsedDict};
use super::statics::{
    EnumOverride, EnumRecord, FieldOverride, StaticDict, StaticGroup, StaticMessage, TagRecord,
    VersionMask, BUNDLED as BUNDLED_TABLES, TAGS,
};

/// The version used when nothing else resolves (config default and registry fallback).
pub const DEFAULT_VERSION: &str = "FIX.4.4";

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Version {
    Fix40,
    Fix41,
    Fix42,
    Fix43,
    Fix44,
    Fix50,
    Fix50Sp1,
    Fix50Sp2,
    /// Session-layer version; resolves to FIX.5.0SP2 for bundled lookups.
    Fixt11,
}

impl Version {
    /// Versions with their own bundled tables.
    pub const BUNDLED: [Version; 8] = [
        Version::Fix40,
        Version::Fix41,
        Version::Fix42,
        Version::Fix43,
        Version::Fix44,
        Version::Fix50,
        Version::Fix50Sp1,
        Version::Fix50Sp2,
    ];

    pub const ALL: [Version; 9] = [
        Version::Fix40,
        Version::Fix41,
        Version::Fix42,
        Version::Fix43,
        Version::Fix44,
        Version::Fix50,
        Version::Fix50Sp1,
        Version::Fix50Sp2,
        Version::Fixt11,
    ];

    /// BeginStrings the server recognises although no dictionary ships for them.
    pub const KNOWN_UNBUNDLED: [&'static str; 2] = ["FIX.2.7", "FIX.3.0"];

    pub fn parse(s: &str) -> Option<Version> {
        Version::ALL.into_iter().find(|v| v.as_str() == s)
    }

    /// A string that names a FIX version (bundled, aliased, or known unbundled).
    pub fn is_version_identifier(s: &str) -> bool {
        Version::parse(s).is_some() || Version::KNOWN_UNBUNDLED.contains(&s)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Version::Fix40 => "FIX.4.0",
            Version::Fix41 => "FIX.4.1",
            Version::Fix42 => "FIX.4.2",
            Version::Fix43 => "FIX.4.3",
            Version::Fix44 => "FIX.4.4",
            Version::Fix50 => "FIX.5.0",
            Version::Fix50Sp1 => "FIX.5.0SP1",
            Version::Fix50Sp2 => "FIX.5.0SP2",
            Version::Fixt11 => "FIXT.1.1",
        }
    }

    /// The single alias: `FIXT.1.1 → FIX.5.0SP2`.
    pub fn resolve_alias(self) -> Version {
        match self {
            Version::Fixt11 => Version::Fix50Sp2,
            other => other,
        }
    }

    /// Alias-aware string form, for callers holding arbitrary version strings.
    pub fn resolve_alias_str(s: &str) -> &str {
        match Version::parse(s) {
            Some(v) => v.resolve_alias().as_str(),
            None => s,
        }
    }
}

/// The newest bundled version whose tables define `tag`.
pub fn newest_defining(tag: u32) -> Option<Version> {
    Version::BUNDLED
        .into_iter()
        .rev()
        .find(|version| Dict::Static(bundled(*version)).field_idx(tag).is_some())
}

/// The bundled dictionary for a version (total: the alias resolves `FIXT.1.1`).
/// Looked up by name in the generated table, so the mask ordinal has exactly
/// one source — `build.rs`.
pub fn bundled(version: Version) -> &'static StaticDict {
    let name = version.resolve_alias().as_str();
    BUNDLED_TABLES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, dict)| *dict)
        .expect("every bundled Version has generated tables")
}

/// What the dictionaries say about a tag's life across the bundled versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Lifecycle<'a> {
    pub added: Option<&'a str>,
    pub deprecated: Option<&'a str>,
    /// The first bundled version that does not define the tag — and only when
    /// *no* later version defines it either, which is what makes "removed" the
    /// honest word rather than "missing from this one".
    ///
    /// The generated masks may have gaps, and a tag that returns in a later
    /// edition was never removed. Eight versions, so scanning to the end is
    /// free. Scoped to the bundled dictionaries: an owned one has no later
    /// version to speak of and gets `None`.
    pub removed_in: Option<&'a str>,
}

/// Position of a tag's merged row in `TAGS`, the one search every lookup shares.
pub(super) fn tag_index(tag: u32) -> Option<usize> {
    TAGS.binary_search_by_key(&tag, |record| record.tag).ok()
}

pub(super) fn tag_record(tag: u32) -> Option<&'static TagRecord> {
    tag_index(tag).map(|i| &TAGS[i])
}

/// The bundled versions in protocol order, by mask ordinal. `FIXT.1.1` is not
/// among them — it is an alias for `FIX.5.0SP2`'s tables and shares its bit, so
/// counting it would visit the same edition twice.
fn bundled_order() -> impl Iterator<Item = (u8, &'static str)> {
    Version::ALL
        .into_iter()
        .filter(|version| version.resolve_alias() == *version)
        .map(|version| (bundled(version).mask_bit, version.as_str()))
}

fn removed_in(presence: VersionMask, bit: u8) -> Option<&'static str> {
    let mut later = bundled_order()
        .skip_while(|(ordinal, _)| *ordinal != bit)
        .skip(1);
    let (_, first_gap) = later.find(|(ordinal, _)| !presence.contains(*ordinal))?;
    // A gap only means removal when nothing comes back after it.
    later
        .all(|(ordinal, _)| !presence.contains(ordinal))
        .then_some(first_gap)
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FieldIdx(u32);
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct GroupIdx(u16);
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct MsgIdx(u16);

/// Position of an enum value within its field's enum list.
///
/// Indexes the *unfiltered* list, so the number is the same whichever version's
/// view produced it. Note the asymmetry with [`FieldView::enums`], which yields
/// only the values visible in that version and hands out no indices: "the i-th
/// visible enum" is a different number, and [`Dict::enum_at`] is the only way
/// back from an `EnumIdx`.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct EnumIdx(u16);

// Raw access so the codec can pack indices into sentinel-encoded fields.
impl FieldIdx {
    pub(crate) fn raw(self) -> u32 {
        self.0
    }
    pub(crate) fn from_raw(value: u32) -> FieldIdx {
        FieldIdx(value)
    }
}

impl EnumIdx {
    pub(crate) fn raw(self) -> u16 {
        self.0
    }
    pub(crate) fn from_raw(value: u16) -> EnumIdx {
        EnumIdx(value)
    }
}

/// Indices are only meaningful against the `Dict` that produced them; the
/// analysis cache pins dictionary identity through its fingerprint.
#[derive(Clone)]
pub enum Dict {
    Static(&'static StaticDict),
    Owned(Arc<ParsedDict>),
}

impl Dict {
    pub fn version(&self) -> Option<&str> {
        match self {
            Dict::Static(d) => Some(d.version),
            Dict::Owned(d) => d.version.as_deref(),
        }
    }

    pub fn is_static(&self) -> bool {
        matches!(self, Dict::Static(_))
    }

    /// The very same dictionary, not merely an equal one — used to tell a
    /// rebuild that reloaded nothing from one that did.
    pub fn ptr_eq(&self, other: &Dict) -> bool {
        match (self, other) {
            (Dict::Static(a), Dict::Static(b)) => std::ptr::eq(*a, *b),
            (Dict::Owned(a), Dict::Owned(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    pub fn field_idx(&self, tag: u32) -> Option<FieldIdx> {
        let i = match self {
            Dict::Static(d) => {
                let i = tag_index(tag)?;
                TAGS[i].presence.contains(d.mask_bit).then_some(i)?
            }
            Dict::Owned(d) => d.field_index(tag)?,
        };
        Some(FieldIdx(i as u32))
    }

    /// The merged row for a tag, whether or not this version has it — the one
    /// lookup that ignores the presence mask, so a caller can say what *other*
    /// versions define. `None` for an owned dictionary, which spans no versions.
    pub fn record(&self, tag: u32) -> Option<&'static TagRecord> {
        match self {
            Dict::Static(_) => tag_record(tag),
            Dict::Owned(_) => None,
        }
    }

    /// As `record`, for one value of a tag.
    pub fn enum_record(&self, tag: u32, value: &str) -> Option<&'static EnumRecord> {
        let rec = self.record(tag)?;
        let i = rec.enums.binary_search_by(|e| e.value.cmp(value)).ok()?;
        Some(&rec.enums[i])
    }

    pub fn field(&self, idx: FieldIdx) -> FieldView<'_> {
        match self {
            Dict::Static(d) => FieldView::Static {
                rec: &TAGS[idx.0 as usize],
                bit: d.mask_bit,
            },
            Dict::Owned(d) => FieldView::Owned(&d.fields[idx.0 as usize]),
        }
    }

    pub fn enum_label(&self, idx: FieldIdx, value: &str) -> Option<EnumView<'_>> {
        self.field(idx).enum_by_value(value)
    }

    /// `None` when the value matches nothing or the enum is absent from this
    /// version. The parsers cap a field's enum list at `MAX_ITEMS`, so the
    /// conversion cannot fail.
    pub fn enum_idx(&self, idx: FieldIdx, value: &str) -> Option<EnumIdx> {
        let at = self.field(idx).enum_position(value)?;
        u16::try_from(at).ok().map(EnumIdx)
    }

    pub fn enum_at(&self, idx: FieldIdx, at: EnumIdx) -> EnumView<'_> {
        self.field(idx).enum_at(usize::from(at.0))
    }

    pub fn message_idx(&self, msg_type: &str) -> Option<MsgIdx> {
        let i = match self {
            Dict::Static(d) => d
                .messages
                .binary_search_by(|m| m.msg_type.cmp(msg_type))
                .ok()?,
            Dict::Owned(d) => d.message_index(msg_type)?,
        };
        // The parsers reject more than u16::MAX messages (`IndexOverflow`).
        Some(MsgIdx(i as u16))
    }

    pub fn message(&self, idx: MsgIdx) -> MessageView<'_> {
        match self {
            Dict::Static(d) => MessageView::Static(&d.messages[usize::from(idx.0)]),
            Dict::Owned(d) => MessageView::Owned(&d.messages[usize::from(idx.0)]),
        }
    }

    pub fn group(&self, idx: GroupIdx) -> GroupView<'_> {
        match self {
            Dict::Static(d) => GroupView::Static(&d.groups[usize::from(idx.0)]),
            Dict::Owned(d) => GroupView::Owned(&d.groups[usize::from(idx.0)]),
        }
    }

    /// Depth-first search over a message's groups and their nested groups.
    pub fn group_by_count_tag(&self, msg: MsgIdx, count_tag: u32) -> Option<GroupIdx> {
        let mut stack: Vec<GroupIdx> = self.message(msg).groups().collect();
        while let Some(idx) = stack.pop() {
            let group = self.group(idx);
            if group.count_tag() == count_tag {
                return Some(idx);
            }
            stack.extend(group.nested());
        }
        None
    }

    /// A session-layer tag, as the dictionary declares its own header.
    /// A binary search, not a scan: the parser sorts these, and the classifier
    /// asks once per field per line. `contains` over a forty-tag header is
    /// millions of comparisons across a whole-document answer.
    pub fn is_header(&self, tag: u32) -> bool {
        self.session_tags(|d| d.header, |d| &d.header)
            .binary_search(&tag)
            .is_ok()
    }

    pub fn is_trailer(&self, tag: u32) -> bool {
        self.session_tags(|d| d.trailer, |d| &d.trailer)
            .binary_search(&tag)
            .is_ok()
    }

    fn session_tags(
        &self,
        stat: fn(&'static StaticDict) -> &'static [u32],
        owned: fn(&ParsedDict) -> &[u32],
    ) -> &[u32] {
        match self {
            Dict::Static(d) => stat(d),
            Dict::Owned(d) => owned(d),
        }
    }

    /// The exact per-version view as an owned dictionary (tests, parity).
    pub fn to_parsed(&self) -> ParsedDict {
        match self {
            Dict::Owned(d) => (**d).clone(),
            Dict::Static(d) => ParsedDict {
                version: Some(d.version.to_owned()),
                header: d.header.to_vec(),
                trailer: d.trailer.to_vec(),
                fields: TAGS
                    .iter()
                    .filter(|r| r.presence.contains(d.mask_bit))
                    .map(|rec| {
                        FieldView::Static {
                            rec,
                            bit: d.mask_bit,
                        }
                        .to_owned_def()
                    })
                    .collect(),
                groups: d
                    .groups
                    .iter()
                    .map(|g| GroupView::Static(g).to_owned_def())
                    .collect(),
                messages: d
                    .messages
                    .iter()
                    .map(|m| MessageView::Static(m).to_owned_def())
                    .collect(),
            },
        }
    }
}

#[derive(Clone, Copy)]
pub enum FieldView<'a> {
    Static { rec: &'static TagRecord, bit: u8 },
    Owned(&'a FieldDef),
}

impl<'a> FieldView<'a> {
    /// One attribute, resolved through the version's override for static data.
    fn resolved(
        &self,
        pick: fn(&FieldOverride) -> Option<&'static str>,
        base: fn(&TagRecord) -> &'static str,
        owned: fn(&FieldDef) -> &str,
    ) -> &'a str {
        match self {
            FieldView::Static { rec, bit } => {
                rec.override_for(*bit).and_then(pick).unwrap_or(base(rec))
            }
            FieldView::Owned(f) => owned(f),
        }
    }

    pub fn tag(&self) -> u32 {
        match self {
            FieldView::Static { rec, .. } => rec.tag,
            FieldView::Owned(f) => f.tag,
        }
    }

    pub fn name(&self) -> &'a str {
        self.resolved(|o| o.name, |r| r.name, |f| &f.name)
    }

    pub fn datatype(&self) -> &'a str {
        self.resolved(|o| o.datatype, |r| r.datatype, |f| &f.datatype)
    }

    /// `datatype()` normalised. One integer compare per token instead of thirty
    /// string compares, which is why it is stored rather than derived here.
    pub fn datatype_class(&self) -> Datatype {
        match self {
            FieldView::Static { rec, bit } => rec
                .override_for(*bit)
                .and_then(|o| o.datatype_class)
                .unwrap_or(rec.datatype_class),
            FieldView::Owned(f) => f.datatype_class,
        }
    }

    pub fn description(&self) -> &'a str {
        self.resolved(|o| o.description, |r| r.description, |f| &f.description)
    }

    pub fn added(&self) -> &'a str {
        self.resolved(|o| o.added, |r| r.added, |f| &f.added)
    }

    pub fn deprecated(&self) -> &'a str {
        self.resolved(|o| o.deprecated, |r| r.deprecated, |f| &f.deprecated)
    }

    /// The extension pack recorded against `added()`, 0 when none is.
    pub fn added_ep(&self) -> u16 {
        match self {
            FieldView::Static { rec, .. } => rec.added_ep,
            FieldView::Owned(f) => f.added_ep,
        }
    }

    /// When the tag entered the protocol, when it was deprecated, and the first
    /// bundled version that drops it for good.
    ///
    /// The renderer phrases this; the mask stays here. A `VersionMask` is a
    /// fact about how the tables were generated, and `render/` reading one
    /// would tie presentation to that layout.
    ///
    /// `added` is the release, not the pack's base: `FIX.5.0 EP55` entered the
    /// protocol in FIX.5.0SP1, which is the file that first has the tag. A date
    /// the files contradict is hover's to drop, not this method's.
    pub fn lifecycle(&self) -> Lifecycle<'a> {
        let some = |text: &'a str| (!text.is_empty()).then_some(text);
        Lifecycle {
            added: release_of(self.added(), self.added_ep()),
            deprecated: some(self.deprecated()),
            removed_in: match self {
                FieldView::Static { rec, bit } => removed_in(rec.presence, *bit),
                // An owned dictionary is one version; it has no "later" to
                // speak of, and claiming a boundary from a single edition
                // would be inventing one.
                FieldView::Owned(_) => None,
            },
        }
    }

    pub fn enums(&self) -> EnumIter<'a> {
        match self {
            FieldView::Static { rec, bit } => EnumIter::Static {
                iter: rec.enums.iter(),
                bit: *bit,
            },
            FieldView::Owned(f) => EnumIter::Owned(f.enums.iter()),
        }
    }

    pub fn enum_by_value(&self, value: &str) -> Option<EnumView<'a>> {
        self.enum_position(value).map(|at| self.enum_at(at))
    }

    /// Position within the unfiltered enum list, honouring per-version presence.
    fn enum_position(&self, value: &str) -> Option<usize> {
        match self {
            FieldView::Static { rec, bit } => {
                let at = rec.enums.binary_search_by(|e| e.value.cmp(value)).ok()?;
                rec.enums[at].presence.contains(*bit).then_some(at)
            }
            FieldView::Owned(f) => f
                .enums
                .binary_search_by(|e| e.value.as_str().cmp(value))
                .ok(),
        }
    }

    fn enum_at(&self, at: usize) -> EnumView<'a> {
        match self {
            FieldView::Static { rec, bit } => EnumView::Static {
                rec: &rec.enums[at],
                bit: *bit,
            },
            FieldView::Owned(f) => EnumView::Owned(&f.enums[at]),
        }
    }

    pub fn to_owned_def(&self) -> FieldDef {
        FieldDef {
            tag: self.tag(),
            name: self.name().to_owned(),
            datatype: self.datatype().to_owned(),
            datatype_class: self.datatype_class(),
            description: self.description().to_owned(),
            added: self.added().to_owned(),
            deprecated: self.deprecated().to_owned(),
            added_ep: self.added_ep(),
            enums: self.enums().map(|e| e.to_owned_def()).collect(),
        }
    }
}

/// The enum values visible through one field view.
pub enum EnumIter<'a> {
    Static {
        iter: std::slice::Iter<'static, EnumRecord>,
        bit: u8,
    },
    Owned(std::slice::Iter<'a, EnumDef>),
}

impl<'a> Iterator for EnumIter<'a> {
    type Item = EnumView<'a>;

    fn next(&mut self) -> Option<EnumView<'a>> {
        match self {
            EnumIter::Static { iter, bit } => {
                let rec = iter.find(|e| e.presence.contains(*bit))?;
                Some(EnumView::Static { rec, bit: *bit })
            }
            EnumIter::Owned(iter) => iter.next().map(EnumView::Owned),
        }
    }
}

#[derive(Clone, Copy)]
pub enum EnumView<'a> {
    Static { rec: &'static EnumRecord, bit: u8 },
    Owned(&'a EnumDef),
}

impl<'a> EnumView<'a> {
    fn resolved(
        &self,
        pick: fn(&EnumOverride) -> Option<&'static str>,
        base: fn(&EnumRecord) -> &'static str,
        owned: fn(&EnumDef) -> &str,
    ) -> &'a str {
        match self {
            EnumView::Static { rec, bit } => {
                rec.override_for(*bit).and_then(pick).unwrap_or(base(rec))
            }
            EnumView::Owned(e) => owned(e),
        }
    }

    pub fn value(&self) -> &'a str {
        match self {
            EnumView::Static { rec, .. } => rec.value,
            EnumView::Owned(e) => &e.value,
        }
    }

    pub fn name(&self) -> &'a str {
        self.resolved(|o| o.name, |r| r.name, |e| &e.name)
    }

    pub fn description(&self) -> &'a str {
        self.resolved(|o| o.description, |r| r.description, |e| &e.description)
    }

    pub fn added(&self) -> &'a str {
        self.resolved(|o| o.added, |r| r.added, |e| &e.added)
    }

    /// As `FieldView::added_ep`.
    pub fn added_ep(&self) -> u16 {
        match self {
            EnumView::Static { rec, .. } => rec.added_ep,
            EnumView::Owned(e) => e.added_ep,
        }
    }

    pub fn to_owned_def(&self) -> EnumDef {
        EnumDef {
            value: self.value().to_owned(),
            name: self.name().to_owned(),
            description: self.description().to_owned(),
            added: self.added().to_owned(),
            added_ep: self.added_ep(),
        }
    }
}

#[derive(Clone, Copy)]
pub enum MessageView<'a> {
    Static(&'static StaticMessage),
    Owned(&'a MessageDef),
}

impl<'a> MessageView<'a> {
    pub fn msg_type(&self) -> &'a str {
        match self {
            MessageView::Static(m) => m.msg_type,
            MessageView::Owned(m) => &m.msg_type,
        }
    }

    pub fn name(&self) -> &'a str {
        match self {
            MessageView::Static(m) => m.name,
            MessageView::Owned(m) => &m.name,
        }
    }

    pub fn category(&self) -> &'a str {
        match self {
            MessageView::Static(m) => m.category,
            MessageView::Owned(m) => &m.category,
        }
    }

    pub fn description(&self) -> &'a str {
        match self {
            MessageView::Static(m) => m.description,
            MessageView::Owned(m) => &m.description,
        }
    }

    /// Administrative rather than application traffic: `Session` in a
    /// repository, `admin` in a QuickFIX file.
    pub fn is_session(&self) -> bool {
        let category = self.category();
        category.eq_ignore_ascii_case("session") || category.eq_ignore_ascii_case("admin")
    }

    pub fn groups(&self) -> impl Iterator<Item = GroupIdx> + 'a {
        let slice: &'a [u16] = match self {
            MessageView::Static(m) => m.groups,
            MessageView::Owned(m) => &m.groups,
        };
        slice.iter().copied().map(GroupIdx)
    }

    pub fn to_owned_def(&self) -> MessageDef {
        MessageDef {
            msg_type: self.msg_type().to_owned(),
            name: self.name().to_owned(),
            category: self.category().to_owned(),
            description: self.description().to_owned(),
            groups: self.groups().map(|g| g.0).collect(),
        }
    }
}

#[derive(Clone, Copy)]
pub enum GroupView<'a> {
    Static(&'static StaticGroup),
    Owned(&'a GroupDef),
}

impl<'a> GroupView<'a> {
    pub fn name(&self) -> &'a str {
        match self {
            GroupView::Static(g) => g.name,
            GroupView::Owned(g) => &g.name,
        }
    }

    pub fn count_tag(&self) -> u32 {
        match self {
            GroupView::Static(g) => g.count_tag,
            GroupView::Owned(g) => g.count_tag,
        }
    }

    pub fn delimiter_tag(&self) -> u32 {
        match self {
            GroupView::Static(g) => g.delimiter_tag,
            GroupView::Owned(g) => g.delimiter_tag,
        }
    }

    pub fn members(&self) -> &'a [u32] {
        match self {
            GroupView::Static(g) => g.members,
            GroupView::Owned(g) => &g.members,
        }
    }

    pub fn nested(&self) -> impl Iterator<Item = GroupIdx> + 'a {
        let slice: &'a [u16] = match self {
            GroupView::Static(g) => g.nested,
            GroupView::Owned(g) => &g.nested,
        };
        slice.iter().copied().map(GroupIdx)
    }

    pub fn to_owned_def(&self) -> GroupDef {
        GroupDef {
            name: self.name().to_owned(),
            count_tag: self.count_tag(),
            delimiter_tag: self.delimiter_tag(),
            members: self.members().to_vec(),
            nested: self.nested().map(|g| g.0).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictionary::bundle::{enrich, parse_metadata};
    use crate::dictionary::parse::{parse_quickfix, Source};
    use std::path::Path;
    use std::sync::LazyLock;

    const XML: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/xml");

    /// FIX.4.4 assembled the way `build.rs` does it, once for all tests here.
    static FIX44_DIRECT: LazyLock<ParsedDict> = LazyLock::new(|| {
        let standard = Path::new(XML).join("standard");
        let meta = Source::read(standard.join("FIXLatest-metadata.xml")).unwrap();
        let mut dict = parse_quickfix(&standard.join("FIX44.xml")).unwrap();
        enrich(&mut dict, &parse_metadata(&meta).unwrap(), "FIX.4.4");
        dict
    });

    fn stat(v: Version) -> Dict {
        Dict::Static(bundled(v))
    }

    /// The `Dict` stays with the caller: a `FieldView` borrows from it, and for
    /// the static case the data behind it is `'static` but the view is not.
    fn lifecycle_of<'a>(dict: &'a Dict, tag: u32) -> Option<Lifecycle<'a>> {
        let idx = dict.field_idx(tag)?;
        Some(dict.field(idx).lifecycle())
    }

    /// Read back out of the generated tables, so a version's type spelling
    /// without a class fails here rather than colouring as `Unknown` at
    /// request time.
    #[test]
    fn every_datatype_the_bundle_spells_has_a_class() {
        let mut unclassified: Vec<(u32, &str)> = Vec::new();
        for rec in TAGS {
            let classes = std::iter::once(rec.datatype_class)
                .chain(rec.overrides.iter().filter_map(|o| o.datatype_class));
            if classes.into_iter().any(|c| c == Datatype::Unknown) {
                unclassified.push((rec.tag, rec.datatype));
            }
        }
        unclassified.sort_unstable();
        unclassified.dedup();
        assert!(unclassified.is_empty(), "unclassified: {unclassified:?}");
    }

    /// AvgPx(6) is `FLOAT` in 4.0 and `PRICE` in 4.4. The shown spelling is FIX
    /// Latest's in both, so the class has to travel the override path on its
    /// own rather than be derived from the spelling.
    #[test]
    fn a_class_follows_the_version_that_changed_the_type() {
        let of = |version, tag| {
            let dict = stat(version);
            let idx = dict.field_idx(tag).expect("present");
            (
                dict.field(idx).datatype().to_owned(),
                dict.field(idx).datatype_class(),
            )
        };
        assert_eq!(
            of(Version::Fix40, 6),
            ("Price".to_owned(), Datatype::Numeric)
        );
        assert_eq!(of(Version::Fix44, 6), ("Price".to_owned(), Datatype::Price));
    }

    /// A group counter is its own class: the parser tells a group apart by it.
    #[test]
    fn a_group_counter_is_not_plain_numeric() {
        let dict = stat(Version::Fix44);
        let idx = dict.field_idx(268).expect("NoMDEntries");
        assert_eq!(dict.field(idx).datatype_class(), Datatype::NumInGroup);
        let idx = dict.field_idx(34).expect("MsgSeqNum");
        assert_eq!(dict.field(idx).datatype_class(), Datatype::Numeric);
    }

    /// The alias resolves to 5.0SP2, whose header gained 1128 and 1129 — so the
    /// same six tags plus more, and the trailer unchanged.
    #[test]
    fn a_session_layer_is_what_the_dictionary_declares() {
        for version in [Version::Fix44, Version::Fixt11] {
            let dict = stat(version.resolve_alias());
            for tag in [8, 9, 35, 34, 49, 56] {
                assert!(dict.is_header(tag), "{version:?}: {tag} is header");
                assert!(!dict.is_trailer(tag), "{version:?}: {tag} is not trailer");
            }
            assert!(dict.is_trailer(10), "{version:?}: CheckSum");
            assert!(!dict.is_header(10), "{version:?}: CheckSum is not header");
            assert!(!dict.is_header(55), "{version:?}: Symbol is neither");
        }
    }

    /// `NoHops` sits inside 4.4's header as a repeating component; its members
    /// are session tags too, which `Child::member_tag` alone would not reach.
    #[test]
    fn a_group_inside_the_header_contributes_its_members() {
        let dict = stat(Version::Fix44);
        assert!(dict.is_header(627), "NoHops");
        assert!(dict.is_header(628), "HopCompID, a member of it");
    }

    #[test]
    fn a_heartbeat_is_session_traffic_and_an_order_is_not() {
        let dict = stat(Version::Fix44);
        let msg = |msg_type| dict.message(dict.message_idx(msg_type).expect(msg_type));
        assert!(msg("0").is_session(), "Heartbeat");
        assert!(msg("A").is_session(), "Logon");
        assert!(!msg("D").is_session(), "NewOrderSingle");
    }

    /// Rule80A is defined up to 4.3 and gone from 4.4 on. Under 4.2 that is a
    /// boundary to report; under 4.4 there is no field to report it on, which
    /// is a different answer and not a different sentence. FIX Latest has no
    /// such tag, so nothing says when it was added.
    #[test]
    fn a_tag_the_protocol_dropped_names_the_version_that_dropped_it() {
        let fix42 = stat(Version::Fix42);
        let under_42 = lifecycle_of(&fix42, 47).expect("47 is in 4.2");
        assert_eq!(
            (under_42.added, under_42.deprecated, under_42.removed_in),
            (None, None, Some("FIX.4.4"))
        );

        let fix44 = stat(Version::Fix44);
        assert!(
            lifecycle_of(&fix44, 47).is_none(),
            "4.4 has no such field at all"
        );
    }

    /// RepurchaseTerm is deprecated in 4.4 by FIX Latest's account, which is a
    /// fact about 4.4 onward and not about the 4.3 that still defines it.
    #[test]
    fn a_deprecation_holds_from_its_own_version_on() {
        let fix43 = stat(Version::Fix43);
        let under_43 = lifecycle_of(&fix43, 226).expect("226 is in 4.3");
        assert_eq!(
            (under_43.added, under_43.deprecated, under_43.removed_in),
            (Some("FIX.4.2"), None, None)
        );
        let fix44 = stat(Version::Fix44);
        let under_44 = lifecycle_of(&fix44, 226).expect("226 is in 4.4");
        assert_eq!(
            (under_44.added, under_44.deprecated, under_44.removed_in),
            (Some("FIX.4.2"), Some("FIX.4.4"), None)
        );
    }

    /// An extension pack is applied on top of its date and ships in the next
    /// release, so the pack's base is a release that does not have the tag:
    /// RejectText(1328) is `FIX.5.0 EP55` and arrived in FIX.5.0SP1, which is
    /// also the first file that declares it. The standing note says the same
    /// release, and one tag may not be given two dates.
    #[test]
    fn a_pack_dated_tag_arrived_in_the_release_that_shipped_the_pack() {
        for version in [Version::Fix50Sp1, Version::Fix50Sp2] {
            let dict = stat(version);
            let reject_text = lifecycle_of(&dict, 1328).expect("1328 is in 5.0SP1 on");
            assert_eq!(reject_text.added, Some("FIX.5.0SP1"), "{version:?}");
        }
        // NoStrategyParameters(957) is FIX.4.4 EP2, so FIX.5.0 — the first file
        // that has it.
        let fix50 = stat(Version::Fix50);
        let parameters = lifecycle_of(&fix50, 957).expect("957 is in 5.0");
        assert_eq!(parameters.added, Some("FIX.5.0"));
        // MsgType(35) has no pack: the date stands as Orchestra wrote it, older
        // than any bundled file.
        let msg_type = lifecycle_of(&fix50, 35).expect("35 is everywhere");
        assert_eq!(msg_type.added, Some("FIX.2.7"));
    }

    /// A tag every version defines has no boundary to report.
    #[test]
    fn a_tag_that_never_leaves_has_no_boundary() {
        let fix42 = stat(Version::Fix42);
        let msg_type = lifecycle_of(&fix42, 35).expect("35 is everywhere");
        assert_eq!(msg_type.removed_in, None);
    }

    /// A tag missing from one edition and back in a later one was never
    /// removed, and saying it was would be a false claim about every version
    /// after the gap — which is why the answer is "removed" only when nothing
    /// comes back.
    #[test]
    fn a_gap_that_closes_again_is_not_a_removal() {
        let with_a_gap = TagRecord {
            tag: 1,
            name: "T",
            datatype: "String",
            datatype_class: Datatype::Text,
            description: "",
            added: "",
            deprecated: "",
            added_ep: 0,
            // Bits 0 and 2 set, 1 clear: present, absent, present.
            presence: VersionMask(0b101),
            overrides: &[],
            enums: &[],
        };
        let order: Vec<u8> = bundled_order().map(|(bit, _)| bit).collect();
        assert!(order.len() >= 3, "the fixture needs three versions");
        assert_eq!(
            removed_in(with_a_gap.presence, order[0]),
            None,
            "it comes back, so nothing was dropped"
        );
        assert!(
            removed_in(VersionMask(0b001), order[0]).is_some(),
            "and a mask that really stops does report the boundary"
        );
    }

    /// The alias shares FIX.5.0SP2's tables and its mask bit, so the ordering
    /// must not visit that edition twice.
    #[test]
    fn the_bundled_order_has_one_entry_per_edition() {
        let order: Vec<&str> = bundled_order().map(|(_, name)| name).collect();
        assert_eq!(order.len(), Version::ALL.len() - 1, "{order:?}");
        assert!(!order.contains(&"FIXT.1.1"), "{order:?}");
        let mut bits: Vec<u8> = bundled_order().map(|(bit, _)| bit).collect();
        let before = bits.len();
        bits.sort_unstable();
        bits.dedup();
        assert_eq!(bits.len(), before, "each edition has its own ordinal");
    }

    #[test]
    fn version_parse_roundtrips_all_nine() {
        for v in Version::ALL {
            assert_eq!(Version::parse(v.as_str()), Some(v));
        }
        assert_eq!(Version::parse("FIX.9.9"), None);
        assert_eq!(Version::resolve_alias_str("FIXT.1.1"), "FIX.5.0SP2");
        assert_eq!(Version::resolve_alias_str("FIX.9.9"), "FIX.9.9");
        assert!(Version::is_version_identifier("FIX.2.7"));
        assert!(!Version::is_version_identifier("broker"));
    }

    #[test]
    fn bundled_tables_match_version_order() {
        for (i, v) in Version::BUNDLED.iter().enumerate() {
            let d = bundled(*v);
            assert_eq!(d.version, v.as_str());
            assert_eq!(usize::from(d.mask_bit), i);
        }
    }

    #[test]
    fn fixt11_bundled_is_fix50sp2_tables() {
        assert!(std::ptr::eq(
            bundled(Version::Fixt11),
            bundled(Version::Fix50Sp2)
        ));
    }

    #[test]
    fn every_bundled_dict_answers_begin_string() {
        for v in Version::BUNDLED {
            let d = stat(v);
            let idx = d
                .field_idx(8)
                .unwrap_or_else(|| panic!("{v:?}: no BeginString(8)"));
            assert_eq!(d.field(idx).name(), "BeginString");
        }
    }

    /// QuickFIX/J keeps 5.x session messages in FIXT11.xml only, and the build
    /// folds them into the one version `8=FIXT.1.1` resolves to.
    #[test]
    fn session_messages_live_where_the_session_layer_is() {
        for v in Version::BUNDLED {
            let d = stat(v);
            let heartbeat = d.message_idx("0").map(|m| d.message(m).name());
            let expected = match v {
                Version::Fix50 | Version::Fix50Sp1 => None,
                _ => Some("Heartbeat"),
            };
            assert_eq!(heartbeat, expected, "{v:?}");
        }
        let sp2 = stat(Version::Fix50Sp2);
        assert!(sp2.message(sp2.message_idx("A").unwrap()).is_session());
        assert!(
            sp2.message_idx("AE").is_some(),
            "the application layer stays"
        );
    }

    /// Components are spliced in child order, so a group reached only through
    /// one keeps its first member as the delimiter, nested groups included.
    #[test]
    fn groups_reached_through_components_keep_their_delimiters() {
        let dict = stat(Version::Fix44);
        let order = dict.message_idx("D").unwrap();
        let parties = dict
            .group_by_count_tag(order, 453)
            .expect("NoPartyIDs via Parties");
        assert_eq!(dict.group(parties).delimiter_tag(), 448);
        assert_eq!(dict.group(parties).members(), [448, 447, 452, 802]);

        let owned = Dict::Owned(Arc::new(FIX44_DIRECT.clone()));
        let order = owned.message_idx("D").unwrap();
        let parties = owned.group_by_count_tag(order, 453).unwrap();
        let sub_ids = owned.group(parties).nested().next().expect("NoPartySubIDs");
        assert_eq!(owned.group(sub_ids).count_tag(), 802);
        assert_eq!(owned.group(sub_ids).delimiter_tag(), 523);
    }

    /// FIX 4.2 writes groups inline in the message rather than in a component.
    #[test]
    fn an_inline_group_keeps_its_delimiter() {
        let dict = stat(Version::Fix42);
        let order = dict.message_idx("D").unwrap();
        let allocs = dict.group_by_count_tag(order, 78).expect("NoAllocs");
        assert_eq!(dict.group(allocs).delimiter_tag(), 79);
        assert_eq!(dict.group(allocs).members(), [79, 80]);
    }

    #[test]
    fn static_and_owned_lookup_agree() {
        let stat = stat(Version::Fix44);
        let owned = Dict::Owned(Arc::new(FIX44_DIRECT.clone()));
        for tag in [8, 35, 54, 268] {
            let (a, b) = (
                stat.field(stat.field_idx(tag).unwrap()),
                owned.field(owned.field_idx(tag).unwrap()),
            );
            assert_eq!(a.to_owned_def(), b.to_owned_def(), "tag {tag}");
        }
        let (a, b) = (stat.field_idx(54).unwrap(), owned.field_idx(54).unwrap());
        assert_eq!(
            stat.enum_label(a, "1").unwrap().name(),
            owned.enum_label(b, "1").unwrap().name()
        );
        let (ma, mb) = (
            stat.message_idx("D").unwrap(),
            owned.message_idx("D").unwrap(),
        );
        assert_eq!(stat.message(ma).name(), owned.message(mb).name());
        let (wa, wb) = (
            stat.message_idx("W").unwrap(),
            owned.message_idx("W").unwrap(),
        );
        let (ga, gb) = (
            stat.group_by_count_tag(wa, 268).unwrap(),
            owned.group_by_count_tag(wb, 268).unwrap(),
        );
        assert_eq!(stat.group(ga).members(), owned.group(gb).members());
        assert_eq!(stat.group(ga).delimiter_tag(), 269);
        assert_eq!(owned.to_parsed(), *FIX44_DIRECT);
    }

    #[test]
    fn unknown_tag_and_msgtype_are_none() {
        let d = stat(Version::Fix44);
        assert!(d.field_idx(99999).is_none());
        assert!(d.message_idx("ZZZZ").is_none());
        let idx = d.field_idx(54).unwrap();
        assert!(d.enum_label(idx, "ZZZZ").is_none());
    }

    #[test]
    fn version_view_resolves_renamed_field() {
        let (f40, f44) = (stat(Version::Fix40), stat(Version::Fix44));
        assert_eq!(f40.field(f40.field_idx(64).unwrap()).name(), "FutSettDate");
        assert_eq!(f44.field(f44.field_idx(64).unwrap()).name(), "SettlDate");
        // Same TagRecord behind both views.
        assert_eq!(f40.field_idx(64), f44.field_idx(64));
    }

    /// A tag FIX Latest does not know keeps the version's own spelling.
    #[test]
    fn a_type_outside_fix_latest_keeps_the_versions_spelling() {
        let f42 = stat(Version::Fix42);
        assert_eq!(f42.field(f42.field_idx(47).unwrap()).datatype(), "CHAR");
    }

    #[test]
    fn presence_mask_hides_absent_tags() {
        // ApplVerID(1128) exists from FIX.5.0 on.
        assert!(stat(Version::Fix44).field_idx(1128).is_none());
        assert!(stat(Version::Fix50).field_idx(1128).is_some());
    }

    #[test]
    fn enum_presence_is_per_version() {
        // Every enum visible through the 4.4 view must exist in a direct 4.4 parse, and vice versa.
        let view = stat(Version::Fix44);
        let tag = 35;
        let mut seen: Vec<String> = view
            .field(view.field_idx(tag).unwrap())
            .enums()
            .map(|e| e.value().to_owned())
            .collect();
        let mut expected: Vec<String> = FIX44_DIRECT
            .field(tag)
            .unwrap()
            .enums
            .iter()
            .map(|e| e.value.clone())
            .collect();
        seen.sort();
        expected.sort();
        assert_eq!(seen, expected);
        // And an enum that only exists later is hidden: compare counts across versions.
        let later = stat(Version::Fix50Sp2);
        assert!(later.field(later.field_idx(tag).unwrap()).enums().count() > seen.len());
    }

    /// `field_idx` hides a tag the version does not define, on purpose. A rule
    /// that wants to say "this tag belongs to a later version" needs the row
    /// anyway, so `record` reads it past the mask.
    #[test]
    fn a_tag_this_version_lacks_is_still_readable_as_a_record() {
        let view = stat(Version::Fix44);
        assert!(view.field_idx(1128).is_none(), "4.4 has no ApplVerID(1128)");
        let rec = view.record(1128).expect("but the merged table has it");
        assert_eq!(rec.name, "ApplVerID");
        let defining: Vec<&str> = bundled_order()
            .filter(|(bit, _)| rec.presence.contains(*bit))
            .map(|(_, name)| name)
            .collect();
        assert_eq!(defining, ["FIX.5.0", "FIX.5.0SP1", "FIX.5.0SP2"]);
    }

    #[test]
    fn a_value_this_version_lacks_is_still_readable_as_a_record() {
        let (f44, sp2) = (stat(Version::Fix44), stat(Version::Fix50Sp2));
        let rec = f44
            .enum_record(803, "28")
            .expect("PartySubIDType(803) = 28 is defined somewhere");
        assert_eq!(rec.name, "MarketMaker");
        assert!(!rec.presence.contains(bundled(Version::Fix44).mask_bit));
        assert!(rec.presence.contains(bundled(Version::Fix50Sp2).mask_bit));
        // And the masked lookup keeps disagreeing, which is the point.
        let idx = f44.field_idx(803).expect("the field itself is in 4.4");
        assert!(f44.enum_idx(idx, "28").is_none());
        let idx = sp2.field_idx(803).unwrap();
        assert!(sp2.enum_idx(idx, "28").is_some());
    }

    #[test]
    fn an_owned_dictionary_has_no_records_to_read() {
        let owned = Dict::Owned(Arc::new(FIX44_DIRECT.clone()));
        assert!(owned.record(8).is_none(), "it spans no versions");
        assert!(owned.enum_record(35, "D").is_none());
    }

    /// `0` means no positive pack is recorded — which the source spells two
    /// ways, an absent attribute and `-1`.
    #[test]
    fn the_extension_pack_is_read_back_from_the_tables() {
        let sp2 = stat(Version::Fix50Sp2);
        assert_eq!(sp2.record(1128).unwrap().added_ep, 16);
        assert_eq!(sp2.enum_record(803, "28").unwrap().added_ep, 26);
        assert_eq!(sp2.enum_record(803, "1").unwrap().added_ep, 0);
        // The source writes addedEP="-1" for both of these.
        assert_eq!(sp2.record(1100).unwrap().added_ep, 0);
        assert_eq!(sp2.enum_record(65, "CD").unwrap().added_ep, 0);
        // The views answer the same.
        let idx = sp2.field_idx(1128).unwrap();
        assert_eq!(sp2.field(idx).added_ep(), 16);
        let idx = sp2.field_idx(803).unwrap();
        assert_eq!(sp2.enum_label(idx, "28").unwrap().added_ep(), 26);
    }

    #[test]
    fn added_and_deprecated_are_exposed() {
        let view = stat(Version::Fix44);
        let f = view.field(view.field_idx(8).unwrap());
        let direct = FIX44_DIRECT.field(8).unwrap();
        assert_eq!(f.added(), direct.added);
        assert!(!f.added().is_empty());
        assert_eq!(f.deprecated(), direct.deprecated);
    }

    #[test]
    fn deprecated_does_not_leak_backwards() {
        let (f44, sp2) = (stat(Version::Fix44), stat(Version::Fix50Sp2));
        assert_eq!(f44.field(f44.field_idx(111).unwrap()).deprecated(), "");
        assert!(!sp2
            .field(sp2.field_idx(111).unwrap())
            .deprecated()
            .is_empty());
    }

    /// `added` has one source, FIX Latest, so every version tells the same story.
    #[test]
    fn added_is_the_same_in_every_version() {
        let (f44, sp2) = (stat(Version::Fix44), stat(Version::Fix50Sp2));
        let a = f44.field(f44.field_idx(243).unwrap()).added();
        let b = sp2.field(sp2.field_idx(243).unwrap()).added();
        assert_eq!((a, b), ("FIX.4.3", "FIX.4.3"));
        assert_eq!(a, FIX44_DIRECT.field(243).unwrap().added);

        let (a, b) = (f44.field_idx(35).unwrap(), sp2.field_idx(35).unwrap());
        assert_eq!(f44.enum_label(a, "0").unwrap().added(), "FIX.2.7");
        assert_eq!(sp2.enum_label(b, "0").unwrap().added(), "FIX.2.7");
    }

    #[test]
    fn enum_idx_round_trips_through_enum_at() {
        let dict = stat(Version::Fix44);
        let side = dict.field_idx(54).unwrap();
        let buy = dict.enum_idx(side, "1").expect("Side=1 exists in FIX.4.4");
        assert_eq!(dict.enum_at(side, buy).name(), "Buy");
        assert_eq!(dict.enum_at(side, buy).value(), "1");
        assert!(dict.enum_idx(side, "ZZ").is_none());
    }

    #[test]
    fn enum_idx_agrees_with_the_view_iterator_on_every_bundled_version() {
        for version in Version::BUNDLED {
            let dict = stat(version);
            let msg_type = dict.field_idx(35).unwrap();
            for value in dict.field(msg_type).enums() {
                let at = dict
                    .enum_idx(msg_type, value.value())
                    .expect("a listed enum resolves");
                assert_eq!(
                    dict.enum_at(msg_type, at).name(),
                    value.name(),
                    "{version:?} 35={}",
                    value.value()
                );
            }
        }
    }

    #[test]
    fn enum_idx_respects_per_version_presence() {
        // ApplVerID (1128) exists only from FIX.5.0.
        assert!(stat(Version::Fix44).field_idx(1128).is_none());
        let dict = stat(Version::Fix50Sp2);
        let appl_ver = dict.field_idx(1128).unwrap();
        assert!(dict.enum_idx(appl_ver, "9").is_some());
    }

    #[test]
    fn enum_idx_works_for_owned_dictionaries() {
        let parsed = parse_quickfix(&Path::new(XML).join("custom/synthetic/oe-fix44.xml")).unwrap();
        let dict = Dict::Owned(Arc::new(parsed));
        let msg_type = dict.field_idx(35).unwrap();
        let new_order = dict.enum_idx(msg_type, "D").unwrap();
        assert_eq!(dict.enum_at(msg_type, new_order).name(), "NewOrderSingle");
    }

    #[test]
    fn raw_indices_round_trip() {
        let dict = stat(Version::Fix44);
        let msg_type = dict.field_idx(35).unwrap();
        assert_eq!(FieldIdx::from_raw(msg_type.raw()), msg_type);
        let new_order = dict.enum_idx(msg_type, "D").unwrap();
        assert_eq!(EnumIdx::from_raw(new_order.raw()), new_order);
    }
}
