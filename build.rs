//! Generates the bundled dictionary tables from `xml/standard/`: QuickFIX/J
//! dictionaries for structure, enriched with metadata projected from FIX
//! Orchestra (see `xml/README.md`).
//!
//! Application fields and enums share a `TAGS` table across eight releases:
//! the base row holds the latest version's values, a presence mask says
//! which versions define the tag, and sparse overrides record where an older
//! version differs (name, type, description, lifecycle attributes). Groups,
//! messages and the scoped schema stay per version. The XML is the source of
//! truth; the generated file is never committed. FIXT has independent field/enum
//! tables and reuses the same schema generator without a chronology mask.

#[allow(dead_code)]
#[path = "src/dictionary/bundle.rs"]
mod bundle;
#[allow(dead_code)]
#[path = "src/dictionary/model.rs"]
mod model;
#[allow(dead_code)]
#[path = "src/dictionary/parse.rs"]
mod parse;
#[allow(dead_code)]
#[path = "src/dictionary/schema.rs"]
mod schema;
#[allow(dead_code)]
#[path = "src/tag.rs"]
mod tag;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use model::{Datatype, EnumDef, FieldDef, ParsedDict};

/// Ordinal = mask bit; the last entry is the "latest" version whose row is the
/// base. The runtime never repeats this order: `bundled()` looks tables up by
/// name and reads the ordinal from `StaticDict::mask_bit`.
const BUNDLED_VERSIONS: [(&str, &str); 8] = [
    ("FIX.4.0", "FIX40.xml"),
    ("FIX.4.1", "FIX41.xml"),
    ("FIX.4.2", "FIX42.xml"),
    ("FIX.4.3", "FIX43.xml"),
    ("FIX.4.4", "FIX44.xml"),
    ("FIX.5.0", "FIX50.xml"),
    ("FIX.5.0SP1", "FIX50SP1.xml"),
    ("FIX.5.0SP2", "FIX50SP2.xml"),
];

/// Standalone transport, outside the application chronology.
const SESSION: &str = "FIXT11.xml";

const METADATA: &str = "FIXLatest-metadata.xml";

/// Must match `VersionMask::BITS` in `src/dictionary/statics.rs`.
const MASK_BITS: usize = 16;
const _: () = assert!(BUNDLED_VERSIONS.len() <= MASK_BITS, "widen VersionMask");

fn read(dir: &Path, file: &str) -> parse::Source {
    parse::Source::read(dir.join(file)).unwrap_or_else(|e| panic!("{e}"))
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let standard = manifest.join("xml").join("standard");
    println!("cargo:rerun-if-changed=build.rs");
    // A directory here makes cargo scan it recursively, so any edited XML
    // regenerates the tables. Listing paths also narrows the default "any file
    // in the package" trigger to exactly the generator's inputs.
    println!("cargo:rerun-if-changed={}", standard.display());
    println!("cargo:rerun-if-changed=src/dictionary/bundle.rs");
    println!("cargo:rerun-if-changed=src/dictionary/model.rs");
    println!("cargo:rerun-if-changed=src/dictionary/parse.rs");
    println!("cargo:rerun-if-changed=src/dictionary/schema.rs");

    let found: BTreeSet<String> = fs::read_dir(&standard)
        .expect("read xml/standard/")
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".xml"))
        .collect();
    let expected: BTreeSet<String> = BUNDLED_VERSIONS
        .iter()
        .map(|(_, file)| *file)
        .chain([SESSION, METADATA])
        .map(str::to_owned)
        .collect();
    assert_eq!(
        found, expected,
        "xml/standard/*.xml must match BUNDLED_VERSIONS exactly (add versions deliberately)"
    );

    let metadata = bundle::parse_metadata(&read(&standard, METADATA))
        .unwrap_or_else(|e| panic!("bundled metadata: {e}"));
    let mut transport = parse::parse_quickfix_bytes(&read(&standard, SESSION))
        .unwrap_or_else(|e| panic!("bundled session layer: {e}"));
    assert_no_faults(SESSION, &transport.faults);

    // The eight parses are independent and dominate the build script's time.
    let dicts: Vec<ParsedDict> = std::thread::scope(|scope| {
        let handles: Vec<_> = BUNDLED_VERSIONS
            .iter()
            .map(|&(version, file)| {
                let (standard, metadata) = (&standard, &metadata);
                scope.spawn(move || {
                    let mut dict = parse::parse_quickfix_bytes(&read(standard, file))
                        .unwrap_or_else(|e| panic!("bundled dictionary {version}: {e}"));
                    assert_no_faults(file, &dict.faults);
                    bundle::enrich(&mut dict, metadata, version);
                    assert_no_faults(file, &dict.faults);
                    dict
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    bundle::enrich(&mut transport, &metadata, "FIXT.1.1");
    let generated = generate(&dicts, &transport, &metadata);
    // The analysis cache keys on this: editing `xml/` regenerates the tables
    // without moving the crate version, and stale caches would survive.
    let source = format!(
        "{generated}\n/// Content hash of the generated tables.\npub const DICTS_BUILD_HASH: u64 = {};\n",
        xxhash_rust::xxh3::xxh3_64(generated.as_bytes())
    );
    fs::write(out_dir.join("dicts.rs"), source).expect("write dicts.rs");
}

/// A fault loads in a custom file because it is the user's to fix; in a
/// bundled one it is ours.
fn assert_no_faults(file: &str, faults: &[model::Fault]) {
    let faults: Vec<String> = faults.iter().map(ToString::to_string).collect();
    assert!(
        faults.is_empty(),
        "bundled {file} has faults:\n{}",
        faults.join("\n")
    );
}

fn lit(s: &str) -> String {
    format!("{s:?}")
}

fn opt(s: Option<&str>) -> String {
    match s {
        Some(s) => format!("Some({})", lit(s)),
        None => "None".to_owned(),
    }
}

fn class(class: Datatype) -> String {
    format!("Datatype::{class:?}")
}

fn opt_class(class_: Option<Datatype>) -> String {
    match class_ {
        Some(c) => format!("Some({})", class(c)),
        None => "None".to_owned(),
    }
}

fn ident(version: &str) -> String {
    version.replace('.', "_")
}

fn mask(m: u16) -> String {
    format!("VersionMask({m:#018b})")
}

/// `Some(candidate)` when it differs from `base`.
fn diff<'a>(candidate: &'a str, base: &str) -> Option<&'a str> {
    (candidate != base).then_some(candidate)
}

/// The one extension pack every version records for this item.
///
/// It has no override bucket because it cannot differ: every version is
/// enriched from the same pinned metadata projection. Asserted rather than
/// assumed, so a second metadata source would fail here instead of silently
/// letting the latest version's pack speak for all of them.
fn one_extension_pack(what: &str, packs: impl Iterator<Item = u16>) -> u16 {
    let mut found = None;
    for pack in packs {
        match found {
            None => found = Some(pack),
            Some(first) => assert_eq!(first, pack, "{what}: added_ep differs between versions"),
        }
    }
    found.expect("present somewhere")
}

/// What an older version sees differently from the base (latest) row.
#[derive(PartialEq, Eq, PartialOrd, Ord, Default)]
struct FieldDiff<'a> {
    name: Option<&'a str>,
    datatype: Option<&'a str>,
    /// Its own bucket: the shown spelling and the class have different sources.
    datatype_class: Option<Datatype>,
    description: Option<&'a str>,
    added: Option<&'a str>,
    deprecated: Option<&'a str>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Default)]
struct EnumDiff<'a> {
    name: Option<&'a str>,
    description: Option<&'a str>,
    added: Option<&'a str>,
}

/// Presence mask plus overrides bucketed by diff, for one tag or enum value
/// present in the given `(ordinal, item)` rows (ascending; last = base).
fn merge<'a, T, D: Ord + Default>(
    present: &[(usize, &'a T)],
    diff: impl Fn(&'a T, &'a T) -> D,
) -> (u16, BTreeMap<D, u16>) {
    let (_, base) = *present.last().expect("present somewhere");
    let presence = present.iter().fold(0u16, |m, (i, _)| m | (1 << i));
    let mut overrides = BTreeMap::new();
    for (i, item) in &present[..present.len() - 1] {
        let d = diff(item, base);
        if d != D::default() {
            *overrides.entry(d).or_default() |= 1 << i;
        }
    }
    (presence, overrides)
}

/// What FIX Latest knows beyond `TAGS`: the tags no bundled dictionary has,
/// and the bundled tags it spells differently. Asserts the facts
/// `standard.rs` leans on, so a refreshed source that breaks one fails here.
fn latest_tables(
    out: &mut String,
    tags: &BTreeSet<u32>,
    metadata: &bundle::Metadata,
    latest_names: &[(u32, &str)],
) {
    for (tag, field) in &metadata.fields {
        assert!(
            bundle::rank(&field.added).is_some(),
            "tag {tag}: FIX Latest dates it {:?}, which has no rank",
            field.added
        );
        assert!(
            !bundle::USER_DEFINED.contains(tag),
            "tag {tag}: FIX Latest defines a tag in the user-defined range"
        );
    }
    let latest: Vec<(&u32, &bundle::FieldMeta)> = metadata
        .fields
        .iter()
        .filter(|(tag, _)| !tags.contains(tag))
        .collect();
    let versions: Vec<&str> = latest
        .iter()
        .map(|(_, field)| field.added.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    writeln!(out, "pub static LATEST_VERSIONS: &[&str] = &{versions:?};").unwrap();
    writeln!(out, "pub static LATEST_TAGS: &[LatestTag] = &[").unwrap();
    for (tag, field) in latest {
        let release =
            bundle::implied_first_release(&field.added, field.added_ep).expect("ranked above");
        // `Unbundled` is always dated after every bundled release; were it not,
        // the tag would be disputed, and `standard.rs` does not look.
        assert!(
            release > bundle::LAST_BUNDLED_RANK,
            "tag {tag}: FIX Latest only, yet dated into a bundled release"
        );
        let added = versions
            .iter()
            .position(|version| *version == field.added)
            .expect("collected above");
        writeln!(
            out,
            "    LatestTag {{ tag: {tag}, name: {}, added: {}, added_ep: {} }},",
            lit(&field.name),
            u8::try_from(added).expect("few FIX Latest dates"),
            field.added_ep
        )
        .unwrap();
    }
    writeln!(out, "];").unwrap();
    writeln!(
        out,
        "pub static LATEST_NAMES: &[(u32, &str)] = &{latest_names:?};"
    )
    .unwrap();
}

/// A `u16` index column, `NONE` spelled out so the reserved value is visible.
fn index(value: u16) -> String {
    if value == schema::NONE {
        "NONE".to_owned()
    } else {
        value.to_string()
    }
}

/// One version's schema as flat static slices, rows through `schema.rs`'s
/// `const fn`s; returns the `Schema { .. }` expression over them.
fn schema_tables(out: &mut String, id: &str, schema: &schema::SchemaBuf) -> String {
    let rows = |out: &mut String, name: &str, ty: &str, items: Vec<String>| {
        writeln!(out, "static {name}_{id}: &[{ty}] = &[{}];", items.join(",")).unwrap();
    };
    rows(
        out,
        "SPANS",
        "ScopeSpan",
        schema
            .spans
            .iter()
            .map(|span| {
                format!(
                    "span({},{},{},{},{},{})",
                    span.start,
                    span.len,
                    span.nested_start,
                    span.nested_len,
                    span.components_start,
                    span.components_len
                )
            })
            .collect(),
    );
    rows(
        out,
        "SCOPE_TAGS",
        "u32",
        schema.tags.iter().map(u32::to_string).collect(),
    );
    rows(
        out,
        "MEMBERS",
        "Member",
        schema
            .members
            .iter()
            .map(|member| {
                format!(
                    "member({},{},{},{})",
                    member.position,
                    index(member.group),
                    index(member.component),
                    member.flags
                )
            })
            .collect(),
    );
    rows(
        out,
        "ORDER",
        "u16",
        schema.order.iter().map(u16::to_string).collect(),
    );
    rows(
        out,
        "NESTED",
        "u16",
        schema.nested.iter().map(u16::to_string).collect(),
    );
    rows(
        out,
        "COMPONENTS",
        "ComponentRow",
        schema
            .components
            .iter()
            .map(|row| format!("row({},{},{})", row.name, index(row.parent), row.flags))
            .collect(),
    );
    rows(
        out,
        "NAMES",
        "Cow<'static, str>",
        schema
            .names
            .iter()
            .map(|name| format!("Cow::Borrowed({})", lit(name)))
            .collect(),
    );
    format!(
        "Schema {{ spans: SPANS_{id}, tags: SCOPE_TAGS_{id}, members: MEMBERS_{id}, order: ORDER_{id}, nested: NESTED_{id}, components: COMPONENTS_{id}, names: NAMES_{id}, messages: {} }}",
        schema.messages
    )
}

fn generate(dicts: &[ParsedDict], transport: &ParsedDict, metadata: &bundle::Metadata) -> String {
    let mut out = String::new();
    writeln!(out, "// @generated by build.rs from xml/ — do not edit.").unwrap();

    // --- merged TAGS ------------------------------------------------------
    let tags: BTreeSet<u32> = dicts
        .iter()
        .flat_map(|d| d.fields.iter().map(|f| f.tag))
        .collect();
    // `Dict::field_idx` returns a position in this merged table, so it is the
    // union that has to be packable — dictionaries that each parse cleanly can
    // still add up to a table that does not fit.
    assert!(
        tags.len() <= parse::MAX_ITEMS,
        "{} tags merged across the bundled versions exceeds {}",
        tags.len(),
        parse::MAX_ITEMS
    );
    // Bundled tags whose FIX Latest name differs from QuickFIX/J's spelling.
    let mut latest_names: Vec<(u32, &str)> = Vec::new();
    writeln!(out, "pub static TAGS: &[TagRecord] = &[").unwrap();
    for &tag in &tags {
        let present: Vec<(usize, &FieldDef)> = dicts
            .iter()
            .enumerate()
            .filter_map(|(i, d)| d.field(tag).map(|f| (i, f)))
            .collect();
        let (_, base) = *present.last().expect("tag present somewhere");
        // `standard.rs` reads an empty `added` as "FIX Latest lacks the tag".
        let latest = metadata.fields.get(&tag);
        assert_eq!(
            base.added.is_empty(),
            latest.is_none(),
            "tag {tag}: an empty `added` must mean FIX Latest does not know it"
        );
        match latest {
            Some(meta) => {
                if meta.name != base.name {
                    latest_names.push((tag, &meta.name));
                }
            }
            None => assert!(
                tag <= *bundle::USER_DEFINED.end(),
                "tag {tag}: QuickFIX/J-only and above the user-defined range"
            ),
        }
        let allows_other_values = present
            .iter()
            .filter(|(_, f)| f.allows_other_values)
            .fold(0u16, |mask, (i, _)| mask | (1 << i));
        let (presence, overrides) = merge(&present, |f, base| FieldDiff {
            name: diff(&f.name, &base.name),
            datatype: diff(&f.datatype, &base.datatype),
            datatype_class: (f.datatype_class != base.datatype_class).then_some(f.datatype_class),
            description: diff(&f.description, &base.description),
            added: diff(&f.added, &base.added),
            deprecated: diff(&f.deprecated, &base.deprecated),
        });

        writeln!(
            out,
            "    TagRecord {{ tag: {tag}, name: {}, datatype: {}, datatype_class: {}, description: {}, added: {}, deprecated: {}, added_ep: {}, presence: {}, allows_other_values: {}, overrides: &[",
            lit(&base.name), lit(&base.datatype), class(base.datatype_class), lit(&base.description), lit(&base.added), lit(&base.deprecated),
            one_extension_pack(&format!("tag {tag}"), present.iter().map(|(_, f)| f.added_ep)), mask(presence), mask(allows_other_values)
        )
        .unwrap();
        for (d, m) in &overrides {
            writeln!(
                out,
                "        FieldOverride {{ versions: {}, name: {}, datatype: {}, datatype_class: {}, description: {}, added: {}, deprecated: {} }},",
                mask(*m), opt(d.name), opt(d.datatype), opt_class(d.datatype_class), opt(d.description), opt(d.added), opt(d.deprecated)
            )
            .unwrap();
        }
        writeln!(out, "    ], enums: &[").unwrap();

        let values: BTreeSet<&str> = present
            .iter()
            .flat_map(|(_, f)| f.enums.iter().map(|e| e.value.as_str()))
            .collect();
        assert!(
            values.len() <= parse::MAX_ITEMS,
            "tag {tag}: {} merged enum values exceeds {}",
            values.len(),
            parse::MAX_ITEMS
        );
        for value in values {
            // Enums are sorted by value within a field.
            let present_e: Vec<(usize, &EnumDef)> = present
                .iter()
                .filter_map(|(i, f)| {
                    f.enums
                        .binary_search_by(|e| e.value.as_str().cmp(value))
                        .ok()
                        .map(|k| (*i, &f.enums[k]))
                })
                .collect();
            let (_, base_e) = *present_e.last().expect("enum present somewhere");
            let (presence_e, overrides_e) = merge(&present_e, |e, base| EnumDiff {
                name: diff(&e.name, &base.name),
                description: diff(&e.description, &base.description),
                added: diff(&e.added, &base.added),
            });
            writeln!(
                out,
                "        EnumRecord {{ value: {}, name: {}, description: {}, added: {}, added_ep: {}, presence: {}, overrides: &[",
                lit(value), lit(&base_e.name), lit(&base_e.description), lit(&base_e.added),
                one_extension_pack(&format!("tag {tag} = {value}"), present_e.iter().map(|(_, e)| e.added_ep)), mask(presence_e)
            )
            .unwrap();
            for (d, m) in &overrides_e {
                writeln!(
                    out,
                    "            EnumOverride {{ versions: {}, name: {}, description: {}, added: {} }},",
                    mask(*m), opt(d.name), opt(d.description), opt(d.added)
                )
                .unwrap();
            }
            writeln!(out, "        ] }},").unwrap();
        }
        writeln!(out, "    ] }},").unwrap();
    }
    writeln!(out, "];").unwrap();
    let known: BTreeSet<u32> = tags
        .iter()
        .copied()
        .chain(transport.fields.iter().map(|field| field.tag))
        .collect();
    latest_tables(&mut out, &known, metadata, &latest_names);

    // --- per-version groups, messages, dict ------------------------------
    for (ordinal, (version, dict)) in BUNDLED_VERSIONS
        .iter()
        .map(|(version, _)| *version)
        .zip(dicts)
        .chain(std::iter::once(("FIXT.1.1", transport)))
        .enumerate()
    {
        let id = ident(version);
        let storage = if ordinal < BUNDLED_VERSIONS.len() {
            format!("StaticFields::Application {{ mask_bit: {ordinal} }}")
        } else {
            standalone_fields(&mut out, &id, dict);
            format!("StaticFields::Standalone(FIELDS_{id})")
        };
        let schema = schema_tables(&mut out, &id, &dict.schema);
        writeln!(out, "static GROUPS_{id}: &[StaticGroup] = &[").unwrap();
        for g in &dict.groups {
            writeln!(
                out,
                "    StaticGroup {{ name: {}, count_tag: {}, delimiter_tag: {} }},",
                lit(&g.name),
                g.count_tag,
                g.delimiter_tag
            )
            .unwrap();
        }
        writeln!(out, "];").unwrap();
        writeln!(out, "static MESSAGES_{id}: &[StaticMessage] = &[").unwrap();
        for m in &dict.messages {
            writeln!(
                out,
                "    StaticMessage {{ msg_type: {}, name: {}, category: {}, description: {} }},",
                lit(&m.msg_type),
                lit(&m.name),
                lit(&m.category),
                lit(&m.description)
            )
            .unwrap();
        }
        writeln!(out, "];").unwrap();
        writeln!(
            out,
            "pub static DICT_{id}: StaticDict = StaticDict {{ version: {}, fields: {storage}, schema: {schema}, groups: GROUPS_{id}, messages: MESSAGES_{id} }};",
            lit(version)
        )
        .unwrap();
    }

    writeln!(out, "pub static BUNDLED: &[(&str, &StaticDict)] = &[").unwrap();
    for version in BUNDLED_VERSIONS
        .iter()
        .map(|(version, _)| *version)
        .chain(["FIXT.1.1"])
    {
        writeln!(out, "    ({}, &DICT_{}),", lit(version), ident(version)).unwrap();
    }
    writeln!(out, "];").unwrap();
    out
}

fn standalone_fields(out: &mut String, id: &str, dict: &ParsedDict) {
    assert!(dict.fields.len() <= parse::MAX_ITEMS);
    writeln!(out, "static FIELDS_{id}: &[StandaloneField] = &[").unwrap();
    for field in &dict.fields {
        assert!(field.enums.len() <= parse::MAX_ITEMS);
        writeln!(out, "StandaloneField {{ tag: {}, name: {}, datatype: {}, datatype_class: {}, description: {}, added: {}, deprecated: {}, added_ep: {}, allows_other_values: {}, enums: &[",
            field.tag, lit(&field.name), lit(&field.datatype), class(field.datatype_class),
            lit(&field.description), lit(&field.added), lit(&field.deprecated), field.added_ep, field.allows_other_values).unwrap();
        for value in &field.enums {
            writeln!(out, "StandaloneEnum {{ value: {}, name: {}, description: {}, added: {}, added_ep: {} }},",
                lit(&value.value), lit(&value.name), lit(&value.description), lit(&value.added), value.added_ep).unwrap();
        }
        writeln!(out, "] }},").unwrap();
    }
    writeln!(out, "];").unwrap();
}
