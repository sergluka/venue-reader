//! Bundled-dictionary parity: the runtime parser against the static tables, a
//! hand-written structure fixture, snapshot counters, and custom-file checks.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use fix_lsp::dictionary::bundle::{assemble, parse_metadata};
use fix_lsp::dictionary::parse::Source;
use fix_lsp::dictionary::schema::{Schema, Scope};
use fix_lsp::dictionary::statics::TAGS;
use fix_lsp::dictionary::{bundled, parse_quickfix, Dict, ParsedDict, Version};

const XML: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/xml");

fn standard_file(version: Version) -> &'static str {
    match version {
        Version::Fix40 => "FIX40.xml",
        Version::Fix41 => "FIX41.xml",
        Version::Fix42 => "FIX42.xml",
        Version::Fix43 => "FIX43.xml",
        Version::Fix44 => "FIX44.xml",
        Version::Fix50 => "FIX50.xml",
        Version::Fix50Sp1 => "FIX50SP1.xml",
        Version::Fix50Sp2 | Version::Fixt11 => "FIX50SP2.xml",
    }
}

/// The static side reconstructs each version through presence masks and
/// overrides; the runtime side assembles each version on its own and never
/// merges across versions. For fields and enums this independently validates
/// the merge codegen. Groups, messages and the schema share the parsing
/// algorithm with build.rs, so the fixtures below cover them; here they prove
/// the generated tables say what the parser said.
#[test]
fn runtime_parse_equals_static_tables_for_every_version() {
    let standard = Path::new(XML).join("standard");
    let meta =
        parse_metadata(&Source::read(standard.join("FIXLatest-metadata.xml")).unwrap()).unwrap();
    let session = parse_quickfix(&standard.join("FIXT11.xml")).unwrap();
    for v in Version::BUNDLED {
        let parsed = parse_quickfix(&standard.join(standard_file(v))).unwrap();
        let assembled = assemble(v.as_str(), parsed, &session, &meta).unwrap();
        let direct = assembled.dict;
        let stat = Dict::Static(bundled(v));
        let reconstructed = stat.to_parsed();
        assert_eq!(reconstructed.version.as_deref(), Some(v.as_str()));
        assert_eq!(
            reconstructed.fields.len(),
            direct.fields.len(),
            "{v:?}: field count"
        );
        for (a, b) in reconstructed.fields.iter().zip(&direct.fields) {
            assert_eq!(a, b, "{v:?}: tag {}", b.tag);
        }
        assert_eq!(reconstructed.groups, direct.groups, "{v:?}: groups");
        assert_eq!(reconstructed.messages, direct.messages, "{v:?}: messages");

        let (ours, theirs) = (
            reconstructed.schema.as_schema().to_defs(),
            direct.schema.as_schema().to_defs(),
        );
        assert_eq!(ours.header, theirs.header, "{v:?}: header scope");
        assert_eq!(ours.trailer, theirs.trailer, "{v:?}: trailer scope");
        assert_eq!(ours.messages.len(), theirs.messages.len(), "{v:?}");
        for (i, (ours_message, theirs_message)) in
            ours.messages.iter().zip(&theirs.messages).enumerate()
        {
            let msg_type = &direct.messages[i].msg_type;
            assert_eq!(
                ours_message, theirs_message,
                "{v:?}: MsgType(35) = {msg_type}"
            );
        }
        assert_eq!(ours.groups.len(), theirs.groups.len(), "{v:?}");
        for (i, (ours_group, theirs_group)) in ours.groups.iter().zip(&theirs.groups).enumerate() {
            assert_eq!(
                ours_group, theirs_group,
                "{v:?}: group {i}, {}",
                direct.groups[i].name
            );
        }
        assert_eq!(reconstructed.schema, direct.schema, "{v:?}: packed schema");

        // QuickFIX/J keeps the 5.x session layer in FIXT11.xml alone.
        let has_session = !matches!(v, Version::Fix50 | Version::Fix50Sp1);
        assert_eq!(stat.has_session_layer(), assembled.session_layer, "{v:?}");
        assert_eq!(assembled.session_layer, has_session, "{v:?}");
        assert_eq!(
            theirs.header.members.is_empty(),
            !has_session,
            "{v:?}: header scope"
        );
        assert_eq!(stat.faults(), &direct.faults[..], "{v:?}: faults");
        assert!(direct.faults.is_empty(), "{v:?}: {:?}", direct.faults);
    }
}

/// Transcribed from the `<header>` and `<trailer>` of `xml/standard/FIX44.xml`.
/// The header's last entry is the `NoHops` group, so 627 and its three members
/// are session tags too — the case a non-recursive walk would miss.
#[test]
fn hand_written_session_layer_fix44() {
    let d = Dict::Static(bundled(Version::Fix44));
    let header = [
        8, 9, 34, 35, 43, 49, 50, 52, 56, 57, 90, 91, 97, 115, 116, 122, 128, 129, 142, 143, 144,
        145, 212, 213, 347, 369, 627, 628, 629, 630,
    ];
    for tag in header {
        assert!(d.is_header(tag), "4.4 header holds {tag}");
    }
    for tag in [10, 89, 93] {
        assert!(d.is_trailer(tag), "4.4 trailer holds {tag}");
        assert!(!d.is_header(tag), "{tag} is trailer, not header");
    }
    assert!(!d.is_header(55) && !d.is_trailer(55), "Symbol is neither");
}

fn message<'a>(parsed: &'a ParsedDict, msg_type: &str) -> Scope<'a> {
    let index = u16::try_from(parsed.message_index(msg_type).unwrap()).unwrap();
    parsed.schema.as_schema().message(index)
}

/// The entries of the group `scope` opens with `count_tag`.
fn entry<'a>(schema: Schema<'a>, scope: Scope<'a>, count_tag: u32) -> Scope<'a> {
    let group = scope.find(count_tag).and_then(|member| member.group());
    schema.group(group.expect("a group counter"))
}

/// Member tags in declared order, counters of nested groups included.
fn tags(scope: Scope<'_>) -> Vec<u32> {
    scope.declared().map(|(tag, _)| tag).collect()
}

/// The count tags of the groups a scope opens, in declared order.
fn counters(scope: Scope<'_>) -> Vec<u32> {
    scope.groups().map(|(tag, _)| tag).collect()
}

/// Transcribed by hand from `xml/standard/FIX44.xml` (message W's `NoMDEntries`;
/// the `Parties` component for message AN), not produced by any parser.
#[test]
fn hand_written_group_structure_fix44() {
    let d = Dict::Static(bundled(Version::Fix44));
    let parsed = d.to_parsed();
    let schema = parsed.schema.as_schema();

    let w = d.message_idx("W").unwrap();
    let md = d.group(d.group_by_count_tag(w, 268).expect("NoMDEntries"));
    assert_eq!(md.name(), "NoMDEntries");
    assert_eq!(md.delimiter_tag(), 269);
    let md = entry(schema, message(&parsed, "W"), 268);
    assert_eq!(
        tags(md),
        &[
            269, 270, 15, 271, 272, 273, 274, 275, 336, 625, 276, 277, 282, 283, 284, 286, 59, 432,
            126, 110, 18, 287, 37, 299, 288, 289, 346, 290, 546, 811, 58, 354, 355
        ]
    );
    assert_eq!(md.groups().count(), 0);

    let an = d.message_idx("AN").unwrap();
    let parties = d.group(d.group_by_count_tag(an, 453).expect("NoPartyIDs"));
    assert_eq!(parties.name(), "NoPartyIDs");
    assert_eq!(parties.delimiter_tag(), 448);
    let parties = entry(schema, message(&parsed, "AN"), 453);
    assert_eq!(tags(parties), &[448, 447, 452, 802]);
    assert_eq!(counters(parties), [802]);
    let nested = parties.find(802).and_then(|member| member.group()).unwrap();
    let subs = &parsed.groups[usize::from(nested)];
    assert_eq!(subs.name, "NoPartySubIDs");
    assert_eq!((subs.count_tag, subs.delimiter_tag), (802, 523));
    let subs = entry(schema, parties, 802);
    assert_eq!(tags(subs), &[523, 803]);
    assert_eq!(subs.groups().count(), 0);
    // AN's top-level groups in document order. Components are spliced inline,
    // so their groups surface here too: NoPartyIDs (453) from Parties,
    // NoSecurityAltID (454) and NoEvents (864) from Instrument. The header is
    // declared apart from the messages, so NoHops is not among them.
    assert_eq!(
        counters(message(&parsed, "AN")),
        vec![453, 454, 864, 555, 711, 386]
    );
}

/// Per-version counters through the views plus merge-level counters. The
/// numbers are the vendored data's; a change here is either a deliberate data
/// update or a codegen regression.
#[test]
fn snapshot_counters() {
    // (version, fields, enums, messages, groups). The header's NoHops(627) is
    // a group in FIX.4.3, FIX.4.4 and FIX.5.0SP2; FIX.4.3's NoRelatedSym(146)
    // and FIX.4.4's NoUnderlyings(711) are two groups each, one per
    // requiredness of the component inside.
    let expected = [
        (Version::Fix40, 138, 230, 27, 7),
        (Version::Fix41, 208, 337, 28, 8),
        (Version::Fix42, 403, 625, 46, 29),
        (Version::Fix43, 655, 1222, 67, 53),
        (Version::Fix44, 916, 1814, 92, 93),
        (Version::Fix50, 1132, 2442, 93, 100),
        (Version::Fix50Sp1, 1353, 2598, 105, 134),
        (Version::Fix50Sp2, 1452, 2821, 115, 146),
    ];
    let mut actual = Vec::new();
    for (version, ..) in expected {
        let p = Dict::Static(bundled(version)).to_parsed();
        let enums: usize = p.fields.iter().map(|f| f.enums.len()).sum();
        actual.push((
            version,
            p.fields.len(),
            enums,
            p.messages.len(),
            p.groups.len(),
        ));
    }
    assert_eq!(
        actual, expected,
        "per-version (fields, enums, messages, groups)"
    );

    let field_overrides: usize = TAGS.iter().map(|t| t.overrides.len()).sum();
    let enum_overrides: usize = TAGS
        .iter()
        .flat_map(|t| t.enums)
        .map(|e| e.overrides.len())
        .sum();
    let merged = (TAGS.len(), field_overrides, enum_overrides);
    assert_eq!(
        merged,
        (1498, 212, 25),
        "(TAGS union, field override rows, enum override rows)"
    );
}

#[test]
fn owned_and_static_dictionaries_share_one_api() {
    let oe = parse_quickfix(&Path::new(XML).join("custom/synthetic/oe-fix44.xml")).unwrap();
    let owned = Dict::Owned(Arc::new(oe));
    let idx = owned.field_idx(35).unwrap();
    assert_eq!(owned.enum_label(idx, "D").unwrap().name(), "NewOrderSingle");
    let stat = Dict::Static(bundled(Version::Fix44));
    let idx = stat.field_idx(35).unwrap();
    assert_eq!(stat.enum_label(idx, "D").unwrap().name(), "NewOrderSingle");
}

/// The bundled files are exactly what `SHA256SUMS` records, so a byte changed
/// by hand, by a tool or by a checkout is caught before it is compiled in.
#[test]
fn bundled_files_match_their_manifest() {
    let standard = Path::new(XML).join("standard");
    let manifest = fs::read_to_string(standard.join("SHA256SUMS")).unwrap();
    let mut listed = BTreeSet::new();
    for line in manifest.lines() {
        let (digest, name) = line.split_once("  ").expect("`<sha256>  <file>`");
        let bytes = fs::read(standard.join(name)).unwrap();
        let actual: String = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(actual, digest, "{name}");
        listed.insert(name.to_owned());
    }
    let present: BTreeSet<String> = fs::read_dir(&standard)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != "SHA256SUMS")
        .collect();
    assert_eq!(
        listed, present,
        "every bundled file is listed, and only those"
    );
}

/// Equal counts do not prove the right composition, so the fields the join
/// rule is argued from carry their whole value set here.
#[test]
fn the_join_rule_produces_these_exact_values() {
    let values = |version, tag| -> Vec<String> {
        let dict = Dict::Static(bundled(version));
        let idx = dict.field_idx(tag).expect("the field is in this version");
        dict.field(idx)
            .enums()
            .map(|e| format!("{}={}", e.value(), e.name()))
            .collect()
    };

    // PartySubIDType(803): FIX44.xml lists none. 1..26 were the protocol's by
    // 4.4; 27..33 arrived with extension packs and stay out of it.
    let in_44 = values(Version::Fix44, 803);
    assert_eq!(in_44.len(), 26, "{in_44:?}");
    assert!(in_44.contains(&"1=Firm".to_owned()));
    assert!(in_44.contains(&"26=PositionAccountType".to_owned()));
    assert!(!in_44
        .iter()
        .any(|v| v.starts_with("27=") || v.starts_with("28=")));
    let in_sp2 = values(Version::Fix50Sp2, 803);
    assert_eq!(in_sp2.len(), 33, "{in_sp2:?}");
    assert!(in_sp2.contains(&"28=MarketMaker".to_owned()));

    // QuoteEntryRejectReason(368) shares its code set with other fields. Its
    // 25 Orchestra codes relabel the nine QuickFIX 4.4 values and add nothing.
    assert_eq!(
        values(Version::Fix44, 368),
        [
            "1=UnknownSymbol",
            "2=Exchange",
            "3=QuoteRequestExceedsLimit",
            "4=TooLateToEnter",
            "5=UnknownQuote",
            "6=DuplicateQuote",
            "7=InvalidBid",
            "8=InvalidPrice",
            "9=NotAuthorizedToQuoteSecurity",
        ]
    );

    // MsgType(35) is the version's own message list, never Orchestra's.
    let msg_types = values(Version::Fix44, 35);
    assert_eq!(msg_types.len(), 93);
    assert!(msg_types.contains(&"D=NewOrderSingle".to_owned()));
    assert!(
        !msg_types.iter().any(|v| v.starts_with("BQ=")),
        "a FIX Latest message"
    );
}
