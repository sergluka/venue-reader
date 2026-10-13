use super::decode::decode_sources;
use super::{
    tokenize, Field, LineAnalysis, MessageAnalysis, Resolution, ResolutionIssue, Selector,
};
use crate::config::Config;
use crate::ctx::{analyze, Ctx};
use crate::dictionary::parse::{fixtures::source, parse_quickfix_bytes};
use crate::dictionary::{
    ApplicationIdentity, Dict, DictionaryChoice, DictionarySources, Registry, SourceRole,
};
use std::num::NonZeroU64;
use std::sync::Arc;

// Independently constructed QuickFIX schemas exercise ownership, not venue behavior.
const FIELDS: &str = r#"
    <field number='8' name='BeginString' type='STRING'/>
    <field number='10' name='CheckSum' type='STRING'/>
    <field number='35' name='MsgType' type='STRING'/>
    <field number='54' name='Side' type='CHAR'>
        <value enum='1' description='Buy'/><value enum='2' description='Sell'/>
    </field>
    <field number='55' name='Symbol' type='STRING'/>
    <field number='78' name='NoAllocs' type='NUMINGROUP'/>
    <field number='79' name='AllocAccount' type='STRING'/>
    <field number='268' name='NoMDEntries' type='NUMINGROUP'/>
    <field number='269' name='MDEntryType' type='CHAR'>
        <value enum='0' description='Bid'/>
    </field>
    <field number='448' name='PartyID' type='STRING'/>
    <field number='452' name='PartyRole' type='INT'/>
    <field number='453' name='NoPartyIDs' type='NUMINGROUP'/>
    <field number='523' name='PartySubID' type='STRING'/>
    <field number='627' name='NoHops' type='NUMINGROUP'/>
    <field number='628' name='HopCompID' type='STRING'/>
    <field number='802' name='NoPartySubIDs' type='NUMINGROUP'/>
    <field number='1128' name='ApplVerID' type='STRING'/>
    <field number='1129' name='CstmApplVerID' type='STRING'/>
    <field number='1156' name='ApplExtID' type='INT'/>
"#;

const HEADER: &str = r#"
    <field name='BeginString' required='Y'/>
    <field name='MsgType' required='Y'/>
    <field name='ApplVerID' required='N'/>
    <field name='ApplExtID' required='N'/>
    <field name='CstmApplVerID' required='N'/>
"#;
const HOPS: &str = r#"
    <group name='NoHops' required='N'>
        <field name='HopCompID' required='Y'/>
    </group>
"#;
const PARTY_GROUP: &str = r#"
    <group name='NoPartyIDs' required='N'>
        <field name='PartyID' required='Y'/>
    </group>
"#;
const BODY_PARTY_GROUP: &str = r#"
    <group name='NoPartyIDs' required='N'>
        <field name='PartyID' required='Y'/>
        <field name='PartyRole' required='N'/>
    </group>
"#;
const PARTY_FIELD: &str = "<field name='NoPartyIDs' required='N'/>";
const BODY: &str = r#"
    <field name='Side' required='N'/>
    <field name='Symbol' required='N'/>
    <group name='NoAllocs' required='N'>
        <field name='AllocAccount' required='Y'/>
    </group>
"#;
const TRAILER: &str = "<field name='CheckSum' required='Y'/>";
const CONFLICT_LINE: &str = "8=FIXT.1.1|35=D|1128=9|54=1|627=1|628=PREFIX|453=1|448=PARTY|54=2|55=AFTER|78=1|79=TAIL|10=000";

fn dictionary(version: &str, header: &str, body: &str, trailer: &str) -> Dict {
    let xml = format!(
        "<fix version='{version}'>
            <header>{header}</header><trailer>{trailer}</trailer>
            <messages><message name='SyntheticMessage' msgtype='D' msgcat='app'>
                {body}
            </message></messages><fields>{FIELDS}</fields>
        </fix>"
    );
    let parsed = parse_quickfix_bytes(&source("synthetic-version.xml", xml)).unwrap();
    assert!(parsed.faults.is_empty(), "{:?}", parsed.faults);
    Dict::Owned(Arc::new(parsed))
}

fn sources(header: &str, body: &str, trailer: &str) -> DictionarySources {
    DictionarySources {
        transport: Some(dictionary("FIXT.1.1", header, "", trailer)),
        application: Some(dictionary("FIX.5.0SP2", "", body, "")),
    }
}

fn message(analysis: LineAnalysis) -> MessageAnalysis {
    match analysis {
        LineAnalysis::Message(message) => message,
        other => panic!("expected a message, got {:?}", other.kind()),
    }
}

fn decode(line: &str, sources: DictionarySources) -> MessageAnalysis {
    let version = Arc::from(sources.application.as_ref().unwrap().version().unwrap());
    message(decode_sources(
        line.as_bytes(),
        tokenize(line.as_bytes()),
        sources,
        version,
        Resolution::default(),
    ))
}

fn assert_unowned(analysis: &MessageAnalysis, field: &Field, line: &str) {
    assert!(field.unresolved(), "{field:?}");
    assert_eq!(field.owner(), None);
    assert_eq!(field.def(), None);
    assert_eq!(field.enum_value(), None);
    assert_eq!(field.group_idx(), None);
    assert!(!field.is_header() && !field.is_trailer() && !field.is_counter());
    assert!(analysis.field_dict(field).is_none());
    assert!(analysis.field_definition(field).is_none());
    assert_eq!(analysis.field_version(field), None);
    assert_eq!(analysis.field_name(field), None);
    assert_eq!(analysis.enum_label(field), None);
    assert_eq!(analysis.group_path(field, line.as_bytes()), None);
}

fn assert_raw_fields(analysis: &MessageAnalysis, line: &str) {
    let tokens = tokenize(line.as_bytes());
    assert_eq!(analysis.fields.len(), tokens.fields.len());
    for (field, token) in analysis.fields.iter().zip(tokens.fields) {
        assert_eq!(field.tag, token.tag);
        assert_eq!(field.span(), token.tag_start..token.value_end);
        assert_eq!(
            field.value(line.as_bytes()),
            &line.as_bytes()[token.value_start.index()..token.value_end.index()]
        );
    }
}

fn assert_conflict_keeps_only_safe_prefix(sources: DictionarySources, line: &str) {
    let original = sources.clone();
    let analysis = decode(line, sources);
    assert_raw_fields(&analysis, line);
    assert_eq!(analysis.resolution.issue, None);
    for owner in [SourceRole::Transport, SourceRole::Application] {
        assert!(analysis
            .sources
            .source(owner)
            .unwrap()
            .ptr_eq(original.source(owner).unwrap()));
    }
    for field in analysis.fields.iter().take(6) {
        assert!(field.def().is_some(), "{field:?}");
        assert!(analysis.field_name(field).is_some());
        assert!(analysis
            .field_dict(field)
            .unwrap()
            .ptr_eq(original.source(field.owner().unwrap()).unwrap()));
    }
    assert_eq!(analysis.msg_type.unwrap().owner, SourceRole::Application);
    assert_eq!(
        analysis.message_definition().unwrap().name(),
        "SyntheticMessage"
    );
    let side = analysis.first(54).unwrap();
    assert_eq!(side.owner(), Some(SourceRole::Application));
    assert_eq!(analysis.enum_label(side), Some("Buy"));
    let hop = analysis.first(628).unwrap();
    assert_eq!(hop.owner(), Some(SourceRole::Transport));
    assert!(hop.is_header());
    assert_eq!(analysis.field_name(hop), Some("HopCompID"));
    assert_eq!(
        analysis.group_path(hop, line.as_bytes()).as_deref(),
        Some("NoHops/1/HopCompID")
    );
    assert_eq!(analysis.groups.len(), 1);
    assert_eq!(analysis.groups[0].owner, SourceRole::Transport);
    let spans = analysis.group_spans();
    assert_eq!(spans.instance(0), Some(hop.span()));
    assert_eq!(
        spans.group(4),
        Some(analysis.fields[4].tag_start..hop.value_end())
    );
    assert_eq!(spans.group_def(4).unwrap().owner, SourceRole::Transport);
    for (index, field) in analysis.fields.iter().enumerate().skip(6) {
        assert_unowned(&analysis, field, line);
        assert_eq!(spans.group(index as u32), None);
        assert_eq!(spans.group_def(index as u32), None);
    }
}

#[test]
fn transport_header_group_conflicts_with_application_body_plain_field() {
    assert_conflict_keeps_only_safe_prefix(
        sources(
            &format!("{HEADER}{HOPS}{PARTY_GROUP}"),
            &format!("{BODY}{PARTY_FIELD}"),
            TRAILER,
        ),
        CONFLICT_LINE,
    );
}

#[test]
fn transport_header_plain_field_conflicts_with_application_body_group() {
    assert_conflict_keeps_only_safe_prefix(
        sources(
            &format!("{HEADER}{HOPS}{PARTY_FIELD}"),
            &format!("{BODY}{BODY_PARTY_GROUP}"),
            TRAILER,
        ),
        CONFLICT_LINE,
    );
}

#[test]
fn transport_and_application_groups_conflict_across_physical_sources() {
    assert_conflict_keeps_only_safe_prefix(
        sources(
            &format!("{HEADER}{HOPS}{PARTY_GROUP}"),
            &format!("{BODY}{BODY_PARTY_GROUP}"),
            TRAILER,
        ),
        CONFLICT_LINE,
    );
}

#[test]
fn equal_xml_in_separate_physical_sources_still_conflicts() {
    let header = format!("{HEADER}{HOPS}{PARTY_GROUP}");
    let body = format!("{BODY}{BODY_PARTY_GROUP}");
    let transport = dictionary("FIX.4.4", &header, &body, TRAILER);
    let application = dictionary("FIX.4.4", &header, &body, TRAILER);
    assert!(!transport.ptr_eq(&application));
    assert_conflict_keeps_only_safe_prefix(
        DictionarySources {
            transport: Some(transport),
            application: Some(application),
        },
        &CONFLICT_LINE.replace("FIXT.1.1", "FIX.4.4"),
    );
}

#[test]
fn same_physical_source_preserves_groups_and_body_group_precedence() {
    for (header_member, body_member, owner, role_in_group) in [
        (PARTY_GROUP, PARTY_FIELD, SourceRole::Transport, false),
        (PARTY_FIELD, BODY_PARTY_GROUP, SourceRole::Application, true),
        (PARTY_GROUP, BODY_PARTY_GROUP, SourceRole::Application, true),
    ] {
        let dict = dictionary(
            "FIX.4.4",
            &format!("{HEADER}{header_member}"),
            &format!("{BODY}{body_member}"),
            TRAILER,
        );
        let line = "8=FIX.4.4|35=D|453=1|448=PARTY|452=1|55=AFTER|10=000";
        let analysis = decode(
            line,
            DictionarySources {
                transport: Some(dict.clone()),
                application: Some(dict.clone()),
            },
        );
        assert_eq!(analysis.resolution.issue, None);
        assert_eq!(analysis.groups.len(), 1);
        assert_eq!(analysis.groups[0].owner, owner);
        let party = analysis.first(448).unwrap();
        assert_eq!(party.owner(), Some(owner));
        assert!(analysis.field_dict(party).unwrap().ptr_eq(&dict));
        assert_eq!(
            analysis.group_path(party, line.as_bytes()).as_deref(),
            Some("NoPartyIDs/1/PartyID")
        );
        let role = analysis.first(452).unwrap();
        assert_eq!(role.group_idx().is_some(), role_in_group);
        assert_eq!(analysis.field_name(role), Some("PartyRole"));
        let symbol = analysis.first(55).unwrap();
        assert_eq!(analysis.field_name(symbol), Some("Symbol"));
        assert_eq!(symbol.group_idx(), None);
    }
}

#[test]
fn plain_plain_cross_source_collision_does_not_stop_suffix_groups() {
    let line = "8=FIXT.1.1|35=D|1128=9|453=1|55=AFTER|78=1|79=TAIL|10=000";
    let analysis = decode(
        line,
        sources(
            &format!("{HEADER}{PARTY_FIELD}"),
            &format!("{BODY}{PARTY_FIELD}"),
            TRAILER,
        ),
    );
    assert_unowned(&analysis, analysis.first(453).unwrap(), line);
    assert_eq!(
        analysis.field_name(analysis.first(55).unwrap()),
        Some("Symbol")
    );
    assert_eq!(analysis.groups.len(), 1);
    assert_eq!(analysis.groups[0].owner, SourceRole::Application);
    assert_eq!(
        analysis
            .group_path(analysis.first(79).unwrap(), line.as_bytes())
            .as_deref(),
        Some("NoAllocs/1/AllocAccount")
    );
}

fn nested_header() -> String {
    format!(
        "{HEADER}<group name='NoHops' required='N'>
            <field name='HopCompID' required='Y'/>
            <group name='NoPartyIDs' required='N'>
                <field name='PartyID' required='Y'/>
                <field name='Side' required='N'/>
            </group>
        </group>"
    )
}

#[test]
fn inactive_descendant_scope_does_not_conflict_with_body_root_group() {
    let line = "8=FIXT.1.1|35=D|1128=9|453=1|448=PARTY|452=1|55=AFTER|10=000";
    let analysis = decode(
        line,
        sources(
            &nested_header(),
            &format!("{BODY}{BODY_PARTY_GROUP}"),
            TRAILER,
        ),
    );
    assert_eq!(analysis.resolution.issue, None);
    assert_eq!(analysis.groups.len(), 1);
    assert_eq!(analysis.groups[0].owner, SourceRole::Application);
    assert_eq!(
        analysis.first(448).unwrap().owner(),
        Some(SourceRole::Application)
    );
    assert_eq!(
        analysis
            .group_path(analysis.first(452).unwrap(), line.as_bytes())
            .as_deref(),
        Some("NoPartyIDs/1/PartyRole")
    );
    assert_eq!(
        analysis.field_name(analysis.first(55).unwrap()),
        Some("Symbol")
    );
}

#[test]
fn opened_header_group_keeps_ownership_despite_application_root_candidates() {
    let line = "8=FIXT.1.1|35=D|1128=9|627=1|628=HOP|453=1|448=PARTY|54=1|55=AFTER|10=000";
    let original = sources(
        &nested_header(),
        &format!("{BODY}{BODY_PARTY_GROUP}"),
        TRAILER,
    );
    let analysis = decode(line, original.clone());
    assert_eq!(analysis.resolution.issue, None);
    assert_eq!(analysis.groups.len(), 2);
    assert_eq!(analysis.groups[1].parent_idx(), Some(0));
    assert_eq!(analysis.groups[1].depth, 2);
    assert!(analysis
        .groups
        .iter()
        .all(|group| group.owner == SourceRole::Transport));
    for tag in [628, 453, 448, 54] {
        let field = analysis.first(tag).unwrap();
        assert_eq!(field.owner(), Some(SourceRole::Transport));
        assert!(field.is_header());
        assert!(analysis
            .field_dict(field)
            .unwrap()
            .ptr_eq(original.transport.as_ref().unwrap()));
    }
    let side = analysis.first(54).unwrap();
    assert_eq!(analysis.enum_label(side), Some("Buy"));
    assert_eq!(
        analysis.group_path(side, line.as_bytes()).as_deref(),
        Some("NoHops/1/NoPartyIDs/1/Side")
    );
    assert_eq!(
        analysis.first(55).unwrap().owner(),
        Some(SourceRole::Application)
    );
    assert_eq!(analysis.first(55).unwrap().group_idx(), None);
}

fn selected_context(sources: DictionarySources, identity: ApplicationIdentity) -> Arc<Ctx> {
    let choice = DictionaryChoice {
        dict: sources.application.unwrap(),
        source: "synthetic-application.xml".into(),
        identity: 32,
        name: Some("synthetic-pair".into()),
        transport: sources.transport.unwrap(),
        application: Some(identity),
        transport_source: "synthetic-transport.xml".into(),
    };
    Ctx::with_choice(
        &Config {
            default_appl_ver_id: Some("FIX.5.0SP1".into()),
            ..Config::default()
        },
        Arc::new(Registry::bundled_only()),
        Some(choice),
    )
}

fn assert_invalidated(analysis: &MessageAnalysis, line: &str, issue: ResolutionIssue) {
    assert_eq!(analysis.resolution.issue, Some(issue));
    assert!(analysis.sources.transport.is_none());
    assert!(analysis.sources.application.is_none());
    assert!(analysis.msg_type.is_none());
    assert!(analysis.message_definition().is_none());
    assert!(analysis.groups.is_empty());
    assert_raw_fields(analysis, line);
    let spans = analysis.group_spans();
    for (index, field) in analysis.fields.iter().enumerate() {
        assert_unowned(analysis, field, line);
        assert_eq!(spans.group(index as u32), None);
    }
}

#[test]
fn each_nested_selector_invalidates_sources_references_and_groups_without_fallback() {
    for (tag, name, value) in [
        (8, "BeginString", "FIXT.1.1"),
        (35, "MsgType", "D"),
        (1128, "ApplVerID", "9"),
        (1156, "ApplExtID", "0"),
        (1129, "CstmApplVerID", "SYNTHETIC"),
    ] {
        let body = format!(
            "{BODY}<group name='NoPartyIDs' required='N'>
                <field name='PartyID' required='Y'/>
                <group name='NoPartySubIDs' required='N'>
                    <field name='PartySubID' required='Y'/>
                    <field name='{name}' required='N'/>
                    <field name='Side' required='N'/>
                </group>
            </group>"
        );
        let context = selected_context(
            sources(HEADER, &body, TRAILER),
            ApplicationIdentity {
                version: "FIX.5.0SP2".into(),
                extension_pack: (tag == 1156).then_some(0),
                custom_version: (tag == 1129).then(|| "SYNTHETIC".into()),
            },
        );
        let envelope = [(8, "FIXT.1.1"), (35, "D"), (1128, "9")]
            .into_iter()
            .filter(|(candidate, _)| *candidate != tag)
            .map(|(candidate, value)| format!("{candidate}={value}|"))
            .collect::<String>();
        let entries = "453=1|448=PARTY|802=1|523=SUB|";
        let suffix = "54=1|55=AFTER|10=000";
        let control = format!("{envelope}{tag}={value}|{entries}{suffix}");
        let decoded = message(analyze(control.as_bytes(), &context));
        assert_eq!(decoded.resolution.issue, None, "{control}");
        assert_eq!(decoded.resolution.application_binding, NonZeroU64::new(32));
        assert_eq!(decoded.resolution.transport_binding, NonZeroU64::new(32));
        assert_eq!(decoded.groups.len(), 2);
        assert_eq!(decoded.groups[1].parent_idx(), Some(0));
        assert_eq!(decoded.enum_label(decoded.first(54).unwrap()), Some("Buy"));
        let nested = format!("{envelope}{entries}{tag}={value}|{suffix}");
        assert_invalidated(
            &message(analyze(nested.as_bytes(), &context)),
            &nested,
            ResolutionIssue::SelectorInGroup(Selector::of(tag).unwrap()),
        );
    }
}

#[test]
fn selector_in_ambiguous_suffix_discards_even_safe_prefix_references() {
    let context = selected_context(
        sources(
            &format!("{HEADER}{HOPS}{PARTY_GROUP}"),
            &format!("{BODY}{PARTY_FIELD}"),
            TRAILER,
        ),
        ApplicationIdentity::from_wire("9").unwrap(),
    );
    let line = "8=FIXT.1.1|35=D|54=1|627=1|628=PREFIX|453=1|448=PARTY|1128=9|55=AFTER|10=000";
    assert_invalidated(
        &message(analyze(line.as_bytes(), &context)),
        line,
        ResolutionIssue::AmbiguousSelector(Selector::APPL_VER_ID),
    );
}

#[test]
fn combined_transport_source_does_not_decode_application_body_without_identity() {
    let combined = dictionary("FIXT.1.1", HEADER, BODY, TRAILER);
    let side_definition = combined.field_idx(54).unwrap();
    assert!(combined.enum_idx(side_definition, "1").is_some());
    assert!(!combined
        .message(combined.message_idx("D").unwrap())
        .is_session());
    let line = "8=FIXT.1.1|35=D|54=1|55=SYNTHETIC|78=1|79=SYNTHETIC_ACCOUNT|10=000";
    let control = decode(
        line,
        DictionarySources {
            transport: Some(combined.clone()),
            application: Some(combined.clone()),
        },
    );
    assert_eq!(control.enum_label(control.first(54).unwrap()), Some("Buy"));
    assert_eq!(control.groups.len(), 1);

    let analysis = message(decode_sources(
        line.as_bytes(),
        tokenize(line.as_bytes()),
        DictionarySources {
            transport: Some(combined.clone()),
            application: None,
        },
        Arc::from("FIXT.1.1"),
        Resolution {
            issue: Some(ResolutionIssue::MissingApplication),
            ..Resolution::default()
        },
    ));
    assert_raw_fields(&analysis, line);
    assert_eq!(
        analysis.resolution.issue,
        Some(ResolutionIssue::MissingApplication)
    );
    assert!(analysis.sources.application.is_none());
    assert!(analysis
        .sources
        .transport
        .as_ref()
        .unwrap()
        .ptr_eq(&combined));
    assert!(analysis.msg_type.is_none());
    assert!(analysis.groups.is_empty());
    for tag in [54, 55, 78, 79] {
        assert_unowned(&analysis, analysis.first(tag).unwrap(), line);
    }
    for (tag, name) in [(8, "BeginString"), (35, "MsgType"), (10, "CheckSum")] {
        let field = analysis.first(tag).unwrap();
        assert_eq!(field.owner(), Some(SourceRole::Transport));
        assert!(analysis.field_dict(field).unwrap().ptr_eq(&combined));
        assert_eq!(analysis.field_name(field), Some(name));
        assert_eq!(field.group_idx(), None);
    }
    assert!(analysis.first(8).unwrap().is_header());
    assert!(analysis.first(35).unwrap().is_header());
    assert!(analysis.first(10).unwrap().is_trailer());
}

#[test]
fn missing_or_unknown_msgtype_disables_transport_header_and_trailer_groups() {
    let trailer = format!(
        "<group name='NoMDEntries' required='N'>
            <field name='MDEntryType' required='Y'/>
        </group>{TRAILER}"
    );
    let original = sources(&format!("{HEADER}{HOPS}"), BODY, &trailer);
    let suffix = "1128=9|627=1|628=HEADER|55=BODY|268=1|269=0|10=000";
    let control = format!("8=FIXT.1.1|35=D|{suffix}");
    let decoded = decode(&control, original.clone());
    assert_eq!(decoded.groups.len(), 2);
    assert!(decoded.first(628).unwrap().is_header());
    assert!(decoded.first(269).unwrap().is_trailer());
    assert_eq!(
        decoded
            .group_path(decoded.first(269).unwrap(), control.as_bytes())
            .as_deref(),
        Some("NoMDEntries/1/MDEntryType")
    );
    for msgtype in ["", "35=ZZ|"] {
        let line = format!("8=FIXT.1.1|{msgtype}{suffix}");
        let analysis = decode(&line, original.clone());
        assert_raw_fields(&analysis, &line);
        assert!(analysis.msg_type.is_none());
        assert!(analysis.groups.is_empty());
        for field in &analysis.fields {
            assert_eq!(field.group_idx(), None);
            assert!(!field.is_counter());
            assert_eq!(analysis.group_path(field, line.as_bytes()), None);
        }
        for tag in [8, 627, 268, 10] {
            let field = analysis.first(tag).unwrap();
            assert_eq!(field.owner(), Some(SourceRole::Transport));
            assert!(analysis.field_definition(field).is_some());
        }
        assert!(analysis.first(627).unwrap().is_header());
        assert!(analysis.first(268).unwrap().is_trailer());
    }
}
