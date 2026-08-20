//! Assembles the bundled dictionaries: QuickFIX/J structure per version, the
//! FIXT.1.1 session layer under its documented alias, and presentation metadata
//! projected from FIX Orchestra. Shared with `build.rs` via `#[path]`, so this
//! file may depend only on `std`, `roxmltree`, `super::model` and `super::parse`.

use std::collections::{HashMap, HashSet};

use super::model::{EnumDef, FieldDef, GroupDef, ParsedDict};
use super::parse::{attr, parse_doc, ParseError, Source};

/// Protocol order for comparing Orchestra pedigree against a bundled version.
/// FIXT.1.1 shipped with FIX.5.0, so it shares that rank.
const VERSION_RANK: [(&str, u8); 12] = [
    ("FIX.2.7", 0),
    ("FIX.3.0", 1),
    ("FIX.4.0", 2),
    ("FIX.4.1", 3),
    ("FIX.4.2", 4),
    ("FIX.4.3", 5),
    ("FIX.4.4", 6),
    ("FIX.5.0", 7),
    ("FIXT.1.1", 7),
    ("FIX.5.0SP1", 8),
    ("FIX.5.0SP2", 9),
    ("FIX.Latest", 10),
];

const MSG_TYPE: u32 = 35;

fn rank(version: &str) -> Option<u8> {
    VERSION_RANK
        .iter()
        .find(|(name, _)| *name == version)
        .map(|(_, rank)| *rank)
}

#[derive(Debug, Default)]
pub struct CodeMeta {
    pub name: String,
    pub added: String,
    pub added_ep: u16,
    pub synopsis: String,
}

#[derive(Debug, Default)]
pub struct FieldMeta {
    pub datatype: String,
    pub added: String,
    pub added_ep: u16,
    pub deprecated: String,
    /// The field this code set is named after; only it may take values from it.
    pub own_code_set: bool,
    pub synopsis: String,
    pub codes: HashMap<String, CodeMeta>,
}

#[derive(Debug, Default)]
pub struct MessageMeta {
    pub category: String,
    pub synopsis: String,
}

/// `xml/standard/FIXLatest-metadata.xml`, keyed for joining on id.
#[derive(Debug, Default)]
pub struct Metadata {
    pub fields: HashMap<u32, FieldMeta>,
    pub messages: HashMap<String, MessageMeta>,
}

/// The generator writes `addedEP` only when it is a positive number that fits,
/// so anything else names no pack.
fn extension_pack(node: roxmltree::Node) -> u16 {
    attr(node, "addedEP").parse().unwrap_or(0)
}

pub fn parse_metadata(src: &Source) -> Result<Metadata, ParseError> {
    let doc = parse_doc(src)?;
    let mut meta = Metadata::default();
    for node in doc.descendants().filter(|n| n.has_tag_name("field")) {
        let text = attr(node, "id");
        let tag = text.parse().map_err(|_| ParseError::BadTag {
            path: src.path.clone(),
            text,
        })?;
        let codes = node
            .children()
            .filter(|n| n.has_tag_name("code"))
            .map(|code| {
                let meta = CodeMeta {
                    name: attr(code, "name"),
                    added: attr(code, "added"),
                    added_ep: extension_pack(code),
                    synopsis: attr(code, "synopsis"),
                };
                (attr(code, "value"), meta)
            })
            .collect();
        let field = FieldMeta {
            datatype: attr(node, "type"),
            added: attr(node, "added"),
            added_ep: extension_pack(node),
            deprecated: attr(node, "deprecated"),
            own_code_set: attr(node, "ownCodeSet") == "true",
            synopsis: attr(node, "synopsis"),
            codes,
        };
        meta.fields.insert(tag, field);
    }
    for node in doc.descendants().filter(|n| n.has_tag_name("message")) {
        let message = MessageMeta {
            category: attr(node, "category"),
            synopsis: attr(node, "synopsis"),
        };
        meta.messages.insert(attr(node, "msgType"), message);
    }
    Ok(meta)
}

/// The codes of the field's own set that this version should already have had.
///
/// Dated later than the version, dated not at all, or brought in by an
/// extension pack: not this version's. Everything QuickFIX declares stays,
/// whatever Orchestra says about it — the file is the version.
fn restore_values(field: &mut FieldDef, meta: &FieldMeta, current: Option<u8>) {
    if field.tag == MSG_TYPE || !meta.own_code_set {
        return;
    }
    let Some(current) = current else { return };
    let mut added = false;
    for (value, code) in &meta.codes {
        if code.added_ep != 0 || !matches!(rank(&code.added), Some(r) if r <= current) {
            continue;
        }
        if field.enums.iter().any(|e| &e.value == value) {
            continue;
        }
        field.enums.push(EnumDef {
            value: value.clone(),
            name: code.name.clone(),
            description: code.synopsis.clone(),
            added: code.added.clone(),
            added_ep: code.added_ep,
        });
        added = true;
    }
    if added {
        field.enums.sort_by(|a, b| a.value.cmp(&b.value));
    }
}

/// Adds presentation to what `dict` already defines, and restores the enum
/// values QuickFIX/J leaves out: the version's file alone decides which fields,
/// messages and groups exist, but not which values a field has.
///
/// A version lists what its QuickFIX file declares plus the Orchestra codes of
/// the field's **own** code set that Orchestra dates to that version or earlier
/// and that no extension pack added. A shared set (`RefMsgType(372)` and 1321
/// others) only relabels what is already there — its codes belong to the field
/// the set is named after, not to every field that reuses it. MsgType(35) is
/// never extended: its values are the version's own messages.
///
/// Orchestra describes FIX Latest, so a deprecation later than `version` is
/// not a fact about `version` and is dropped. Field names stay the version's
/// own (tags were renamed); enum labels and the shown type take Orchestra's
/// spelling, while the type class stays the version's, so value checks keep
/// following the version.
pub fn enrich(dict: &mut ParsedDict, meta: &Metadata, version: &str) {
    let current = rank(version);
    let messages: HashMap<&str, &str> = dict
        .messages
        .iter()
        .map(|m| (m.msg_type.as_str(), m.name.as_str()))
        .collect();
    for field in &mut dict.fields {
        let Some(found) = meta.fields.get(&field.tag) else {
            continue;
        };
        if !found.synopsis.is_empty() {
            field.description = found.synopsis.clone();
        }
        if !found.datatype.is_empty() {
            field.datatype = found.datatype.clone();
        }
        field.added = found.added.clone();
        field.added_ep = found.added_ep;
        let applies = matches!((rank(&found.deprecated), current), (Some(d), Some(c)) if d <= c);
        field.deprecated = if applies {
            found.deprecated.clone()
        } else {
            String::new()
        };
        for value in &mut field.enums {
            // A MsgType value the version defines a message for is labelled by
            // that message's own name.
            if field.tag == MSG_TYPE {
                if let Some(name) = messages.get(value.value.as_str()) {
                    value.name = (*name).to_owned();
                    value.description = (*name).to_owned();
                }
            }
            let Some(code) = found.codes.get(&value.value) else {
                continue;
            };
            let relabel = field.tag != MSG_TYPE || !messages.contains_key(value.value.as_str());
            if relabel && !code.name.is_empty() {
                value.name = code.name.clone();
            }
            if relabel && !code.synopsis.is_empty() {
                value.description = code.synopsis.clone();
            }
            value.added = code.added.clone();
            value.added_ep = code.added_ep;
        }
        restore_values(field, found, current);
    }
    for message in &mut dict.messages {
        let Some(found) = meta.messages.get(&message.msg_type) else {
            continue;
        };
        message.description = found.synopsis.clone();
        if !found.category.is_empty() {
            message.category = found.category.clone();
        }
    }
}

/// Folds the FIXT.1.1 session layer into an application dictionary: QuickFIX/J
/// keeps the header, trailer and admin messages only in `FIXT11.xml`, while
/// `8=FIXT.1.1` resolves to the application tables. What `app` defines wins.
pub fn merge_session(app: &mut ParsedDict, session: &ParsedDict) {
    app.header = session.header.clone();
    app.trailer = session.trailer.clone();

    // Looked up before anything is appended: the lookup needs sorted fields.
    let mut added = Vec::new();
    for field in &session.fields {
        let Some(i) = app.field_index(field.tag) else {
            added.push(field.clone());
            continue;
        };
        let own = &mut app.fields[i];
        for value in &field.enums {
            if let Err(at) = own
                .enums
                .binary_search_by(|e| e.value.as_str().cmp(&value.value))
            {
                own.enums.insert(at, value.clone());
            }
        }
    }
    app.fields.append(&mut added);
    app.fields.sort_by_key(|f| f.tag);

    let mut index: HashMap<GroupDef, u16> = app
        .groups
        .iter()
        .enumerate()
        .map(|(i, g)| (g.clone(), i as u16))
        .collect();
    // Session groups only ever nest earlier ones, so one ordered pass remaps.
    let mut remap = Vec::with_capacity(session.groups.len());
    for group in &session.groups {
        let mut def = group.clone();
        def.nested = group.nested.iter().map(|&n| remap[n as usize]).collect();
        let idx = *index.entry(def.clone()).or_insert_with(|| {
            app.groups.push(def);
            (app.groups.len() - 1) as u16
        });
        remap.push(idx);
    }

    let own_types: HashSet<String> = app.messages.iter().map(|m| m.msg_type.clone()).collect();
    for message in &session.messages {
        if !own_types.contains(&message.msg_type) {
            let mut own = message.clone();
            own.groups = message.groups.iter().map(|&g| remap[g as usize]).collect();
            app.messages.push(own);
        }
    }
    app.messages.sort_by(|a, b| a.msg_type.cmp(&b.msg_type));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictionary::model::{Datatype, EnumDef, FieldDef, MessageDef};
    use std::path::PathBuf;

    fn field(tag: u32, values: &[&str]) -> FieldDef {
        FieldDef {
            tag,
            name: format!("F{tag}"),
            enums: values
                .iter()
                .map(|v| EnumDef {
                    value: (*v).to_owned(),
                    name: format!("V{v}"),
                    description: format!("V{v}"),
                    ..EnumDef::default()
                })
                .collect(),
            ..FieldDef::default()
        }
    }

    fn metadata(xml: &str) -> Metadata {
        parse_metadata(&Source {
            path: PathBuf::from("meta.xml"),
            bytes: xml.as_bytes().to_vec(),
        })
        .unwrap()
    }

    const META: &str = r#"<metadata>
        <fields>
          <field id="1" type="char" added="FIX.4.0" deprecated="FIX.4.4" synopsis="One">
            <code value="A" name="Alpha" added="FIX.4.2" synopsis="The first"/>
            <code value="B" synopsis="Bravo"/>
          </field>
          <field id="9" synopsis="Not in the dictionary"/>
          <field id="35" type="String">
            <code value="D" name="NewOrderSingle" added="FIX.2.7" synopsis="New order"/>
          </field>
        </fields>
        <messages>
          <message msgType="D" category="SingleGeneralOrderHandling" synopsis="Order"/>
          <message msgType="Z" synopsis="Not in the dictionary"/>
        </messages>
      </metadata>"#;

    fn dict() -> ParsedDict {
        let mut first = field(1, &["A"]);
        first.datatype = "STRING".into();
        first.datatype_class = Datatype::Text;
        ParsedDict {
            fields: vec![first],
            messages: vec![MessageDef {
                msg_type: "D".into(),
                name: "NewOrderSingle".into(),
                category: "app".into(),
                ..MessageDef::default()
            }],
            ..ParsedDict::default()
        }
    }

    #[test]
    fn enrichment_never_adds_fields_or_messages() {
        let mut d = dict();
        enrich(&mut d, &metadata(META), "FIX.4.4");
        assert_eq!(d.fields.len(), 1);
        assert_eq!(
            d.fields[0].enums.len(),
            1,
            "code B is from a shared set: it labels, it does not add"
        );
        assert_eq!(d.messages.len(), 1);

        let f = &d.fields[0];
        assert_eq!(
            (f.description.as_str(), f.added.as_str()),
            ("One", "FIX.4.0")
        );
        assert_eq!(f.deprecated, "FIX.4.4");
        let e = &f.enums[0];
        assert_eq!(
            (e.name.as_str(), e.description.as_str()),
            ("Alpha", "The first")
        );
        assert_eq!(f.datatype, "char");
        assert_eq!(
            f.datatype_class,
            Datatype::Text,
            "the class stays the version's"
        );
        assert_eq!(e.added, "FIX.4.2");
        let m = &d.messages[0];
        assert_eq!(m.description, "Order");
        assert_eq!(m.category, "SingleGeneralOrderHandling");
    }

    /// Orchestra's own-set codes, dated to the version or earlier and brought
    /// by no extension pack. QuickFIX's own members stay whatever it says.
    const META_OWN: &str = r#"<metadata>
        <fields>
          <field id="1" type="char" ownCodeSet="true">
            <code value="A" name="Alpha" added="FIX.4.0"/>
            <code value="B" name="Bravo" added="FIX.4.2"/>
            <code value="C" name="Charlie" added="FIX.4.4"/>
            <code value="D" name="Delta" added="FIX.4.2" addedEP="26"/>
            <code value="E" name="Echo"/>
          </field>
          <field id="2" type="char">
            <code value="A" name="Alpha" added="FIX.4.0"/>
            <code value="B" name="Bravo" added="FIX.4.0"/>
          </field>
        </fields>
      </metadata>"#;

    #[test]
    fn an_own_set_restores_the_values_the_version_should_have_had() {
        let mut d = ParsedDict {
            // C is dated later than 4.2 and Z is unknown to Orchestra: both
            // are in the version's own file, so both stay.
            fields: vec![field(1, &["C", "Z"])],
            ..ParsedDict::default()
        };
        enrich(&mut d, &metadata(META_OWN), "FIX.4.2");
        let values: Vec<(&str, &str)> = d.fields[0]
            .enums
            .iter()
            .map(|e| (e.value.as_str(), e.name.as_str()))
            .collect();
        assert_eq!(
            values,
            [
                ("A", "Alpha"),
                ("B", "Bravo"),
                ("C", "Charlie"),
                ("Z", "VZ"),
            ],
            "D was added by EP26 and E is dated nowhere; the rest sorts by value"
        );
        assert_eq!(d.fields[0].enums[0].added_ep, 0);
    }

    #[test]
    fn a_shared_set_relabels_without_adding() {
        let mut d = ParsedDict {
            fields: vec![field(2, &["A"])],
            ..ParsedDict::default()
        };
        enrich(&mut d, &metadata(META_OWN), "FIX.4.4");
        let values: Vec<(&str, &str)> = d.fields[0]
            .enums
            .iter()
            .map(|e| (e.value.as_str(), e.name.as_str()))
            .collect();
        assert_eq!(
            values,
            [("A", "Alpha")],
            "B belongs to the field the set is named after"
        );
    }

    #[test]
    fn msg_type_is_never_extended() {
        let mut d = dict();
        d.fields.push(field(35, &["D"]));
        enrich(&mut d, &metadata(META_OWN_MSG_TYPE), "FIX.5.0SP2");
        let values: Vec<&str> = d
            .field(35)
            .unwrap()
            .enums
            .iter()
            .map(|e| e.value.as_str())
            .collect();
        assert_eq!(values, ["D"], "the version's messages are the whole list");
        assert_eq!(d.field(35).unwrap().enums[0].name, "NewOrderSingle");
    }

    const META_OWN_MSG_TYPE: &str = r#"<metadata>
        <fields>
          <field id="35" type="String" ownCodeSet="true">
            <code value="D" name="LatestOrder" added="FIX.2.7"/>
            <code value="8" name="ExecutionReport" added="FIX.2.7"/>
          </field>
        </fields>
      </metadata>"#;

    /// `build.rs` merges the session layer and then enriches, so the rule runs
    /// over a field that already carries values from two files.
    #[test]
    fn restoring_values_survives_the_session_merge() {
        let mut app = ParsedDict {
            fields: vec![field(1, &["C"])],
            ..ParsedDict::default()
        };
        let session = ParsedDict {
            fields: vec![field(1, &["Z"])],
            ..ParsedDict::default()
        };
        merge_session(&mut app, &session);
        enrich(&mut app, &metadata(META_OWN), "FIX.5.0SP2");
        let values: Vec<&str> = app.fields[0]
            .enums
            .iter()
            .map(|e| e.value.as_str())
            .collect();
        assert_eq!(values, ["A", "B", "C", "Z"], "sorted, and nothing doubled");
    }

    /// A message the version defines names its MsgType value; any other value
    /// takes FIX Latest's name like every other enum.
    #[test]
    fn msg_type_values_take_the_message_names() {
        let mut d = dict();
        d.fields.push(field(35, &["D", "Z"]));
        enrich(&mut d, &metadata(META_WITH_Z), "FIX.4.4");
        let values = &d.field(35).unwrap().enums;
        let label = |i: usize| (values[i].name.as_str(), values[i].description.as_str());
        assert_eq!(label(0), ("NewOrderSingle", "NewOrderSingle"));
        assert_eq!(values[0].added, "FIX.2.7");
        assert_eq!(label(1), ("Zed", "The last"));
    }

    const META_WITH_Z: &str = r#"<metadata>
        <fields>
          <field id="35" type="String">
            <code value="D" name="LatestOrder" added="FIX.2.7" synopsis="New order"/>
            <code value="Z" name="Zed" synopsis="The last"/>
          </field>
        </fields>
      </metadata>"#;

    #[test]
    fn a_later_deprecation_is_not_a_fact_about_an_older_version() {
        let mut d = dict();
        enrich(&mut d, &metadata(META), "FIX.4.2");
        assert_eq!(d.fields[0].deprecated, "");
        assert_eq!(d.fields[0].added, "FIX.4.0");
    }

    #[test]
    fn session_layer_is_folded_in_without_overriding_the_application() {
        let group = |count_tag, nested: Vec<u16>| GroupDef {
            name: format!("G{count_tag}"),
            count_tag,
            delimiter_tag: count_tag + 1,
            members: vec![count_tag + 1],
            nested,
        };
        let mut app = dict();
        app.groups = vec![group(50, vec![])];
        let session = ParsedDict {
            header: vec![8, 9, 35],
            trailer: vec![10],
            fields: vec![field(1, &["A", "B"]), field(35, &["A"])],
            groups: vec![group(50, vec![]), group(60, vec![0])],
            messages: vec![
                MessageDef {
                    msg_type: "A".into(),
                    name: "Logon".into(),
                    groups: vec![1],
                    ..MessageDef::default()
                },
                MessageDef {
                    msg_type: "D".into(),
                    name: "SessionD".into(),
                    ..MessageDef::default()
                },
            ],
            ..ParsedDict::default()
        };
        merge_session(&mut app, &session);

        assert_eq!(
            (app.header.as_slice(), app.trailer.as_slice()),
            (&[8, 9, 35][..], &[10][..])
        );
        let tags: Vec<u32> = app.fields.iter().map(|f| f.tag).collect();
        assert_eq!(tags, [1, 35]);
        let values: Vec<&str> = app.fields[0]
            .enums
            .iter()
            .map(|e| e.value.as_str())
            .collect();
        assert_eq!(values, ["A", "B"]);
        assert_eq!(app.fields[0].enums[0].name, "VA");

        assert_eq!(app.groups.len(), 2, "the shared group is interned once");
        assert_eq!(app.groups[1].nested, [0]);
        let logon = app.message("A").unwrap();
        assert_eq!(logon.groups, [1]);
        assert_eq!(app.message("D").unwrap().name, "NewOrderSingle");
    }
}
