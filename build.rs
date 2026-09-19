//! Generates the bundled dictionary tables from `xml/standard/`: QuickFIX/J
//! dictionaries for structure, enriched with metadata projected from FIX
//! Orchestra (see `xml/README.md`).
//!
//! Fields and enums are merged across all bundled versions into one `TAGS`
//! table: the base row holds the latest version's values, a presence mask says
//! which versions define the tag, and sparse overrides record where an older
//! version differs (name, type, description, lifecycle attributes). Groups and
//! messages stay per version. The XML is the source of truth; the generated
//! file is never committed.

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

/// The session layer `8=FIXT.1.1` needs; folded into the version the
/// `FIXT.1.1` alias resolves to.
const SESSION: (&str, &str) = ("FIX.5.0SP2", "FIXT11.xml");

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

    let found: BTreeSet<String> = fs::read_dir(&standard)
        .expect("read xml/standard/")
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".xml"))
        .collect();
    let expected: BTreeSet<String> = BUNDLED_VERSIONS
        .iter()
        .map(|(_, file)| *file)
        .chain([SESSION.1, METADATA])
        .map(str::to_owned)
        .collect();
    assert_eq!(
        found, expected,
        "xml/standard/*.xml must match BUNDLED_VERSIONS exactly (add versions deliberately)"
    );

    let metadata = bundle::parse_metadata(&read(&standard, METADATA))
        .unwrap_or_else(|e| panic!("bundled metadata: {e}"));
    let session = parse::parse_quickfix_bytes(&read(&standard, SESSION.1))
        .unwrap_or_else(|e| panic!("bundled session layer: {e}"));

    // The eight parses are independent and dominate the build script's time.
    let dicts: Vec<ParsedDict> = std::thread::scope(|scope| {
        let handles: Vec<_> = BUNDLED_VERSIONS
            .iter()
            .map(|&(version, file)| {
                let (standard, metadata, session) = (&standard, &metadata, &session);
                scope.spawn(move || {
                    let mut dict = parse::parse_quickfix_bytes(&read(standard, file))
                        .unwrap_or_else(|e| panic!("bundled dictionary {version}: {e}"));
                    if version == SESSION.0 {
                        bundle::merge_session(&mut dict, session);
                    }
                    bundle::enrich(&mut dict, metadata, version);
                    dict
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let generated = generate(&dicts);
    // The analysis cache keys on this: editing `xml/` regenerates the tables
    // without moving the crate version, and stale caches would survive.
    let source = format!(
        "{generated}\n/// Content hash of the generated tables.\npub const DICTS_BUILD_HASH: u64 = {};\n",
        xxhash_rust::xxh3::xxh3_64(generated.as_bytes())
    );
    fs::write(out_dir.join("dicts.rs"), source).expect("write dicts.rs");
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

fn generate(dicts: &[ParsedDict]) -> String {
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
    writeln!(out, "pub static TAGS: &[TagRecord] = &[").unwrap();
    for tag in tags {
        let present: Vec<(usize, &FieldDef)> = dicts
            .iter()
            .enumerate()
            .filter_map(|(i, d)| d.field(tag).map(|f| (i, f)))
            .collect();
        let (_, base) = *present.last().expect("tag present somewhere");
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
            "    TagRecord {{ tag: {tag}, name: {}, datatype: {}, datatype_class: {}, description: {}, added: {}, deprecated: {}, added_ep: {}, presence: {}, overrides: &[",
            lit(&base.name), lit(&base.datatype), class(base.datatype_class), lit(&base.description), lit(&base.added), lit(&base.deprecated),
            one_extension_pack(&format!("tag {tag}"), present.iter().map(|(_, f)| f.added_ep)), mask(presence)
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

    // --- per-version groups, messages, dict ------------------------------
    for (ordinal, ((version, _), dict)) in BUNDLED_VERSIONS.iter().zip(dicts).enumerate() {
        let id = ident(version);
        writeln!(out, "static GROUPS_{id}: &[StaticGroup] = &[").unwrap();
        for g in &dict.groups {
            writeln!(
                out,
                "    StaticGroup {{ name: {}, count_tag: {}, delimiter_tag: {}, members: &{:?}, nested: &{:?} }},",
                lit(&g.name), g.count_tag, g.delimiter_tag, g.members, g.nested
            )
            .unwrap();
        }
        writeln!(out, "];").unwrap();
        writeln!(out, "static MESSAGES_{id}: &[StaticMessage] = &[").unwrap();
        for m in &dict.messages {
            writeln!(
                out,
                "    StaticMessage {{ msg_type: {}, name: {}, category: {}, description: {}, groups: &{:?} }},",
                lit(&m.msg_type), lit(&m.name), lit(&m.category), lit(&m.description), m.groups
            )
            .unwrap();
        }
        writeln!(out, "];").unwrap();
        writeln!(
            out,
            "pub static DICT_{id}: StaticDict = StaticDict {{ version: {}, mask_bit: {ordinal}, header: &{:?}, trailer: &{:?}, groups: GROUPS_{id}, messages: MESSAGES_{id} }};",
            lit(version), dict.header, dict.trailer
        )
        .unwrap();
    }

    writeln!(out, "pub static BUNDLED: &[(&str, &StaticDict)] = &[").unwrap();
    for (version, _) in BUNDLED_VERSIONS {
        writeln!(out, "    ({}, &DICT_{}),", lit(version), ident(version)).unwrap();
    }
    writeln!(out, "];").unwrap();
    out
}
