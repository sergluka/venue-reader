//! Resolving a tokenized line against one dictionary: field and enum indices,
//! the message type, and repeating-group membership.

use super::packed::Packed16;
use super::tokenize::{tokenize, FieldSpan, LineKind, LineTokens};
use super::{Field, GroupInstance, LineAnalysis, MessageAnalysis, NO_GROUP};
use crate::dictionary::{Dict, GroupIdx, MsgIdx};
use std::sync::Arc;

/// Matches `parse.rs`'s component-nesting cap: bounding the recursion by the
/// field count alone would let a custom dictionary with a long
/// nested-count-tag chain exhaust the stack.
pub(crate) const MAX_GROUP_DEPTH: u16 = 32;

/// One more than the largest packable index: the annotator stops when the next
/// instance would be one `Field::group` cannot address.
const MAX_GROUP_INSTANCES: usize = Packed16::MAX_VALUE + 1;

/// Convenience wrapper: tokenizes, then delegates. `analyze` tokenizes once and
/// calls [`decode_tokenized`] directly.
pub fn decode(bytes: &[u8], dict: &Dict, version: &Arc<str>) -> LineAnalysis {
    decode_tokenized(bytes, tokenize(bytes), dict, version)
}

/// Classification only, for a line that resolves to no dictionary.
pub fn decode_non_message(tokenized: LineTokens) -> LineAnalysis {
    match tokenized.kind {
        LineKind::Empty => LineAnalysis::Empty,
        LineKind::Comment => LineAnalysis::Comment,
        // A `Message` with no fields cannot happen — the tokenizer classifies by
        // whether it read any — but treat it as unparsed rather than panicking.
        LineKind::Other | LineKind::Message => LineAnalysis::Other,
    }
}

pub fn decode_tokenized(
    bytes: &[u8],
    tokenized: LineTokens,
    dict: &Dict,
    version: &Arc<str>,
) -> LineAnalysis {
    if tokenized.kind != LineKind::Message {
        return decode_non_message(tokenized);
    }

    let mut fields: Vec<Field> = tokenized
        .fields
        .iter()
        .map(|span| resolve(bytes, span, dict))
        .collect();

    let msg_type = first_value(bytes, &fields, 35)
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|msg_type| dict.message_idx(msg_type));

    let mut groups = Vec::new();
    if let Some(message) = msg_type {
        Annotator {
            bytes,
            dict,
            fields: &mut fields,
            groups: &mut groups,
        }
        .run(message);
    }

    LineAnalysis::Message(MessageAnalysis {
        prefix_len: tokenized.prefix_len,
        dict: dict.clone(),
        version: version.clone(),
        fields,
        groups,
        msg_type,
    })
}

fn resolve(bytes: &[u8], span: &FieldSpan, dict: &Dict) -> Field {
    let mut field = Field::new(span);
    let Some(def) = dict.field_idx(span.tag) else {
        return field;
    };
    field.set_def(def);

    // A value that is not text can match no enum. Resolving it once here keeps
    // every renderer a direct fetch.
    let raw = &bytes[span.value_start.index()..span.value_end.index()];
    if let Some(at) = std::str::from_utf8(raw)
        .ok()
        .and_then(|value| dict.enum_idx(def, value))
    {
        field.set_enum(at);
    }
    field
}

fn first_value<'a>(bytes: &'a [u8], fields: &[Field], tag: u32) -> Option<&'a [u8]> {
    fields
        .iter()
        .find(|field| field.tag == tag)
        .map(|field| field.value(bytes))
}

/// Repeating-group annotation.
struct Annotator<'a> {
    bytes: &'a [u8],
    dict: &'a Dict,
    fields: &'a mut [Field],
    groups: &'a mut Vec<GroupInstance>,
}

impl Annotator<'_> {
    /// A top-level count tag opens a group; anything else is skipped.
    fn run(&mut self, message: MsgIdx) {
        // Gathered once per line, not searched per field: a message declares
        // up to 17 top-level groups (FIX.5.0SP2 ExecutionReport; 10 in 4.4), so
        // re-deriving the message view, the group and its count tag for each of
        // ~62 fields costs far more than the one small allocation it saves.
        let top: Vec<(u32, GroupIdx)> = self
            .dict
            .message(message)
            .groups()
            .map(|idx| (self.dict.group(idx).count_tag(), idx))
            .collect();

        let mut at = 0;
        while at < self.fields.len() {
            let tag = self.fields[at].tag;
            match top.iter().find(|(count_tag, _)| *count_tag == tag) {
                Some((_, group)) => at = self.group(*group, at, NO_GROUP, 1),
                None => at += 1,
            }
        }
    }

    /// Consume up to `count` entries. Returns the index just past the group.
    fn group(&mut self, group: GroupIdx, count_at: usize, parent_idx: u32, depth: u16) -> usize {
        if depth > MAX_GROUP_DEPTH {
            return count_at + 1;
        }
        let count = std::str::from_utf8(self.fields[count_at].value(self.bytes))
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
            .unwrap_or(0);

        let mut at = count_at + 1;
        for entry in 1..=count {
            if at >= self.fields.len() || self.groups.len() >= MAX_GROUP_INSTANCES {
                break;
            }
            let instance = self.groups.len() as u32;
            self.groups.push(GroupInstance {
                group,
                entry,
                count_field_idx: count_at as u32,
                parent_idx,
                depth,
            });
            let (next, consumed) = self.entry(group, at, instance, depth);
            // Both breaks are load-bearing: a count field followed by a
            // non-member, and a nested group that consumed nothing, would
            // otherwise spin.
            if !consumed || next == at {
                self.groups.pop();
                break;
            }
            at = next;
        }
        at
    }

    fn entry(&mut self, group: GroupIdx, start: usize, instance: u32, depth: u16) -> (usize, bool) {
        // Both are loop-invariant; `members` is a linear scan, so it is only
        // consulted once the nested lookup has come up empty.
        let view = self.dict.group(group);
        let delimiter = view.delimiter_tag();
        let members = view.members();

        let mut at = start;
        let mut consumed = false;
        while at < self.fields.len() {
            let tag = self.fields[at].tag;
            if consumed && tag == delimiter {
                break;
            }
            let nested = view
                .nested()
                .find(|idx| self.dict.group(*idx).count_tag() == tag);
            if let Some(nested) = nested {
                // The nested count tag belongs to *this* entry, not to its own.
                self.fields[at].set_group(instance);
                at = self.group(nested, at, instance, depth + 1);
                consumed = true;
            } else if members.contains(&tag) {
                self.fields[at].set_group(instance);
                at += 1;
                consumed = true;
            } else {
                break;
            }
        }
        (at, consumed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::byte;

    /// The annotator must never create an instance whose index cannot be
    /// packed, because `Field::group` would then read as "no group" and the
    /// fields would silently detach from their entry — and it must not stop one
    /// instance early either, which is the easier mistake while rewriting a cap.
    #[test]
    fn the_last_group_instance_is_still_addressable() {
        assert_eq!(
            MAX_GROUP_INSTANCES,
            u16::MAX as usize,
            "the cap does not move"
        );
        let highest = MAX_GROUP_INSTANCES as u32 - 1;
        assert_eq!(highest, Packed16::MAX_VALUE as u32);
        assert_eq!(Packed16::pack(highest).get(), Some(highest));
    }

    /// The test above is arithmetic on constants: it would pass unchanged if
    /// the annotator stopped early or flipped its comparison. This one runs the
    /// guard. ~400 KB of generated line, which is what a `MAX_GROUP_INSTANCES`
    /// boundary costs; there is no smaller way to reach it.
    #[test]
    fn the_annotator_fills_the_group_index_space_exactly() {
        let count = MAX_GROUP_INSTANCES;
        let mut line = format!("8=FIX.4.4|35=W|55=X|268={count}");
        for _ in 0..count {
            line.push_str("|269=0");
        }
        let dict = Dict::Static(crate::dictionary::bundled(
            crate::dictionary::Version::Fix44,
        ));
        let version: Arc<str> = Arc::from("FIX.4.4");
        let analysis = decode(line.as_bytes(), &dict, &version);
        let message = analysis.message().expect("a message");
        assert_eq!(message.groups.len(), count, "every instance is recorded");
        assert_eq!(
            message.fields.last().expect("a trailing 269").group_idx(),
            Some(Packed16::MAX_VALUE as u32),
            "and the last one is addressable, not \"no group\""
        );
        // And the span covers what was annotated: the shortfall at a cap is
        // visible as a range that stops early, not as a group that swallows the
        // unmarked tail.
        let spans = message.group_spans();
        let last = message.fields.last().expect("a trailing 269");
        assert_eq!(
            spans.instance(Packed16::MAX_VALUE as u32),
            Some(last.span())
        );
        let group = spans.group(3).expect("the 268 occurrence");
        assert_eq!(group.end, last.value_end(), "and stops where marking did");
    }
    use crate::dictionary::{bundled, parse_quickfix, Version};
    use proptest::prelude::*;
    use std::path::Path;

    const XML: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/xml");

    fn fix44() -> (Dict, Arc<str>) {
        (Dict::Static(bundled(Version::Fix44)), Arc::from("FIX.4.4"))
    }

    /// Decode against FIX.4.4 and unwrap the message; every test here works on
    /// one, and the classification cases use `decode` directly.
    fn decode_as_44(line: &str) -> MessageAnalysis {
        let (dict, version) = fix44();
        expect_message(decode(line.as_bytes(), &dict, &version))
    }

    fn expect_message(analysis: LineAnalysis) -> MessageAnalysis {
        match analysis {
            LineAnalysis::Message(message) => message,
            other => panic!("expected a message, got {:?}", other.kind()),
        }
    }

    fn paths(analysis: &MessageAnalysis, line: &str) -> Vec<Option<String>> {
        analysis
            .fields
            .iter()
            .map(|field| analysis.group_path(field, line.as_bytes()))
            .collect()
    }

    #[test]
    fn plain_fields_resolve_names_and_enum_labels() {
        let analysis = decode_as_44("8=FIX.4.4|35=W|54=1");
        assert_eq!(
            analysis.field_name(&analysis.fields[0]),
            Some("BeginString")
        );
        assert_eq!(analysis.enum_label(&analysis.fields[2]), Some("Buy"));
        assert_eq!(
            analysis.enum_label(&analysis.fields[0]),
            None,
            "BeginString has no enum"
        );
    }

    #[test]
    fn an_unknown_tag_has_no_field_index() {
        let analysis = decode_as_44("8=FIX.4.4|5000001=x");
        assert!(analysis.fields[1].def().is_none());
        assert!(analysis.fields[1].enum_value().is_none());
    }

    #[test]
    fn a_non_utf8_value_resolves_no_enum_but_keeps_its_span() {
        let (dict, version) = fix44();
        let analysis = expect_message(decode(b"8=FIX.4.4|54=\xff", &dict, &version));
        assert!(analysis.fields[1].def().is_some(), "the tag is still known");
        assert!(analysis.fields[1].enum_value().is_none());
        assert_eq!(analysis.fields[1].value_end(), byte(14));
    }

    #[test]
    fn repeating_group_entries_and_paths() {
        let line =
            "8=FIX.4.4|35=W|55=BTCUSD|268=2|269=0|270=100.25|271=500|269=1|270=100.30|271=300";
        let analysis = decode_as_44(line);
        assert_eq!(analysis.groups.len(), 2, "two entries of NoMDEntries");
        assert!(
            analysis.first(268).unwrap().group_idx().is_none(),
            "the count field is outside its own group"
        );
        let paths = paths(&analysis, line);
        assert_eq!(paths[4].as_deref(), Some("NoMDEntries/1/MDEntryType"));
        assert_eq!(paths[6].as_deref(), Some("NoMDEntries/1/MDEntrySize"));
        assert_eq!(paths[7].as_deref(), Some("NoMDEntries/2/MDEntryType"));
        assert_eq!(paths[2], None, "Symbol is outside the group");
    }

    #[test]
    fn nested_groups_nest() {
        // FIX.4.4 message AN: NoPartyIDs(453) → NoPartySubIDs(802). The
        // structure is pinned by tests/dictionary_parity.rs.
        let line = "8=FIX.4.4|35=AN|453=1|448=ME|447=D|452=1|802=2|523=s1|803=1|523=s2|803=2";
        let analysis = decode_as_44(line);
        let paths = paths(&analysis, line);
        assert_eq!(paths[3].as_deref(), Some("NoPartyIDs/1/PartyID"));
        assert_eq!(
            paths[6].as_deref(),
            Some("NoPartyIDs/1/NoPartySubIDs"),
            "the nested count tag sits in the outer entry"
        );
        assert_eq!(
            paths[7].as_deref(),
            Some("NoPartyIDs/1/NoPartySubIDs/1/PartySubID")
        );
        assert_eq!(
            paths[9].as_deref(),
            Some("NoPartyIDs/1/NoPartySubIDs/2/PartySubID")
        );
        let deepest = analysis.fields[9].group_idx().unwrap();
        assert_eq!(analysis.groups[deepest as usize].depth, 2);
        assert_eq!(analysis.ancestors(&analysis.fields[9]).len(), 2);
    }

    #[test]
    fn a_non_member_tag_ends_the_group() {
        let line = "8=FIX.4.4|35=W|268=2|269=0|10=001|269=1";
        let analysis = decode_as_44(line);
        let paths = paths(&analysis, line);
        assert_eq!(paths[3].as_deref(), Some("NoMDEntries/1/MDEntryType"));
        assert_eq!(paths[4], None, "CheckSum is not a member");
        assert_eq!(paths[5], None, "and the group does not resume after it");
    }

    #[test]
    fn zero_and_unparsable_counts_consume_only_the_count_field() {
        for count in ["0", "abc", ""] {
            let analysis = decode_as_44(&format!("8=FIX.4.4|35=W|268={count}|269=0"));
            assert!(analysis.groups.is_empty(), "count {count:?}");
            assert!(
                analysis
                    .fields
                    .iter()
                    .all(|field| field.group_idx().is_none()),
                "count {count:?}"
            );
        }
    }

    #[test]
    fn a_count_larger_than_the_data_stops_at_the_end() {
        let analysis = decode_as_44("8=FIX.4.4|35=W|268=9|269=0|270=1");
        assert_eq!(analysis.groups.len(), 1);
    }

    #[test]
    fn groups_need_a_msgtype_but_decoding_does_not_need_tag_8() {
        // A bare NoMDEntries block, as `tests/fixtures/validation/fragment.fix`
        // opens with.
        let analysis = decode_as_44("268=2|269=0|270=100.25|271=500");
        assert!(analysis.groups.is_empty());
        assert!(analysis
            .fields
            .iter()
            .all(|field| field.group_idx().is_none()));
        assert!(analysis.fields[0].def().is_some(), "fields still decode");
        // malformed-missing-tag8: no BeginString(8), full decode.
        let analysis = decode_as_44("35=0|49=CLIENT1");
        assert!(analysis.msg_type.is_some());
        assert!(analysis.fields[1].def().is_some());
    }

    #[test]
    fn first_returns_the_first_occurrence_of_a_repeated_tag() {
        let analysis = decode_as_44("8=FIX.4.4|35=D|55=AAA|55=BBB");
        assert_eq!(analysis.first(55).unwrap().value_start(), byte(18));
    }

    #[test]
    fn field_at_addresses_the_occurrence_under_the_offset() {
        let analysis = decode_as_44("8=FIX.4.4|35=D");
        for at in 0..9 {
            assert_eq!(analysis.field_at(byte(at)).unwrap().tag, 8, "byte {at}");
        }
        assert!(
            analysis.field_at(byte(9)).is_none(),
            "the separator belongs to no field"
        );
        assert_eq!(
            analysis.field_at(byte(12)).unwrap().tag,
            35,
            "the `=` belongs to its field"
        );
        let duplicated = decode_as_44("8=FIX.4.4|35=D|55=AAA|55=BBB");
        assert_eq!(
            duplicated.field_at(byte(25)).unwrap().value_start(),
            byte(25),
            "the second 55, not the first"
        );
    }

    #[test]
    fn non_message_lines_carry_no_dictionary() {
        // The shape itself says it: there is no payload to hold fields, a
        // dictionary or integrity facts.
        for (line, expected) in [
            ("", LineKind::Empty),
            ("   ", LineKind::Empty),
            ("# fix: hover.enabled=false", LineKind::Comment),
            ("prose", LineKind::Other),
        ] {
            let analysis = decode_non_message(tokenize(line.as_bytes()));
            assert!(analysis.message().is_none(), "{line:?}");
            assert_eq!(analysis.kind(), expected, "{line:?}");
        }
    }

    fn synthetic() -> (Dict, Arc<str>) {
        let parsed = parse_quickfix(&Path::new(XML).join("custom/synthetic/oe-fix44.xml")).unwrap();
        (Dict::Owned(Arc::new(parsed)), Arc::from("FIX.4.4"))
    }

    #[test]
    fn an_owned_dictionary_decodes_names_and_enum_labels() {
        let (dict, version) = synthetic();
        let analysis = expect_message(decode(b"8=FIX.4.4|35=D", &dict, &version));
        assert_eq!(
            analysis.enum_label(&analysis.fields[1]),
            Some("NewOrderSingle")
        );
        assert!(analysis.msg_type.is_some());
    }

    #[test]
    fn an_owned_dictionary_nests_groups() {
        // oe-fix44.xml message D declares NoPartyIDs → NoPartySubIDs inline.
        let (dict, version) = synthetic();
        let line = b"8=FIX.4.4|35=D|11=c1|453=1|448=ME|452=1|802=1|523=s1";
        let analysis = expect_message(decode(line, &dict, &version));
        assert_eq!(
            analysis.group_path(&analysis.fields[4], line).as_deref(),
            Some("NoPartyIDs/1/PartyID")
        );
        assert_eq!(
            analysis.group_path(&analysis.fields[7], line).as_deref(),
            Some("NoPartyIDs/1/NoPartySubIDs/1/PartySubID")
        );
    }

    #[test]
    fn an_owned_dictionary_annotates_a_group_that_arrives_through_a_component() {
        // md-fix50.xml message `y` reaches NoRelatedSym(146) only via the
        // SecListGrp component, whose group splices Instrument inline.
        let parsed = parse_quickfix(&Path::new(XML).join("custom/synthetic/md-fix50.xml")).unwrap();
        let dict = Dict::Owned(Arc::new(parsed));
        let version: Arc<str> = Arc::from("FIX.5.0");
        let analysis = expect_message(decode(
            b"8=FIX.5.0|35=y|320=r1|322=s1|146=2|55=AAA|167=CS|562=1|55=BBB|167=CS|562=2",
            &dict,
            &version,
        ));
        let line = "8=FIX.5.0|35=y|320=r1|322=s1|146=2|55=AAA|167=CS|562=1|55=BBB|167=CS|562=2";
        assert_eq!(analysis.groups.len(), 2, "two NoRelatedSym entries");
        assert!(
            analysis.fields[4].group_idx().is_none(),
            "the count field is outside its own group"
        );
        let paths = paths(&analysis, line);
        assert_eq!(paths[5].as_deref(), Some("NoRelatedSym/1/Symbol"));
        assert_eq!(paths[7].as_deref(), Some("NoRelatedSym/1/MinTradeVol"));
        assert_eq!(paths[8].as_deref(), Some("NoRelatedSym/2/Symbol"));
    }

    proptest! {
        #[test]
        fn decode_terminates_with_in_range_indices(
            bytes in prop::collection::vec(any::<u8>(), 0..300)
        ) {
            let (dict, version) = fix44();
            let decoded = decode(&bytes, &dict, &version);
            let Some(analysis) = decoded.message() else {
                return Ok(());
            };
            for field in &analysis.fields {
                if let Some(group) = field.group_idx() {
                    prop_assert!((group as usize) < analysis.groups.len());
                }
            }
            for instance in &analysis.groups {
                prop_assert!((instance.count_field_idx as usize) < analysis.fields.len());
                if let Some(parent) = instance.parent_idx() {
                    prop_assert!((parent as usize) < analysis.groups.len());
                    prop_assert!(analysis.groups[parent as usize].depth < instance.depth);
                }
                prop_assert!(instance.depth <= MAX_GROUP_DEPTH);
            }
        }
    }

    /// The spans are derived from what the annotator marked, so the cases
    /// that matter are the ones where marking and nesting disagree.
    ///
    /// `MAX_GROUP_DEPTH` has no case here, and that is not an omission: depth
    /// grows only through `nested()`, which is the *dictionary's* structure, so
    /// no input line can reach a depth its dictionary does not define. The
    /// guard protects against a pathological dictionary, and a test would have
    /// to build one rather than a line. `MAX_GROUP_INSTANCES` is the opposite —
    /// a line reaches it, and `the_annotator_fills_the_group_index_space_exactly`
    /// asserts the span there.
    mod spans {
        use super::*;
        use crate::codec::{ByteOffset, GroupInstance};
        use std::ops::Range;

        fn instance(line: &str, at: u32) -> Range<ByteOffset> {
            decode_as_44(line)
                .group_spans()
                .instance(at)
                .expect("a span")
        }

        /// Sibling entries do not bleed into one another: an entry's fields are
        /// contiguous, so its span stops where the next one starts.
        #[test]
        fn a_sibling_stops_where_the_next_entry_begins() {
            let line = "8=FIX.4.4|35=D|453=2|448=A|447=D|452=1|448=B|447=D|452=2|54=1";
            assert_eq!(instance(line, 0), byte(21)..byte(38));
            assert_eq!(instance(line, 1), byte(39)..byte(56));
        }

        /// The reverse pass earns its place here: the parent's own last direct
        /// field is 802, which ends well before its child's fields do.
        #[test]
        fn a_parent_reaches_past_its_own_last_direct_field() {
            let line =
                "8=FIX.4.4|35=D|453=1|448=A|447=D|452=1|802=2|523=S1|803=1|523=S2|803=2|54=1";
            let analysis = decode_as_44(line);
            assert_eq!(
                analysis.fields[6].span(),
                byte(39)..byte(44),
                "802, the parent's last field of its own"
            );
            let spans = analysis.group_spans();
            assert_eq!(spans.instance(0), Some(byte(21)..byte(70)));
            assert_eq!(spans.instance(2), Some(byte(58)..byte(70)));
        }

        /// A group's span starts at its count tag, which is deliberately not
        /// inside the group it opens — a cursor on `453=2` means the group.
        #[test]
        fn a_group_span_reaches_back_over_its_count_field() {
            let line = "8=FIX.4.4|35=D|453=2|448=A|447=D|452=1|448=B|447=D|452=2|54=1";
            let analysis = decode_as_44(line);
            assert_eq!(analysis.fields[2].span(), byte(15)..byte(20), "453");
            assert_eq!(analysis.group_spans().group(2), Some(byte(15)..byte(56)));
        }

        /// Why a group is keyed by its count field and not by `GroupIdx`: one
        /// message uses `NoPartySubIDs` twice, once per party, and the two are
        /// unrelated ranges.
        #[test]
        fn one_definition_used_twice_is_two_spans() {
            let analysis = decode_as_44(
                "8=FIX.4.4|35=D|453=2|448=A|447=D|452=1|802=1|523=S1|803=1\
                 |448=B|447=D|452=2|802=1|523=S2|803=2|54=1",
            );
            let nested: Vec<&GroupInstance> = analysis
                .groups
                .iter()
                .filter(|instance| instance.depth == 2)
                .collect();
            assert_eq!(nested.len(), 2, "two entries of the same definition");
            assert_eq!(
                nested[0].group, nested[1].group,
                "and they are the same definition"
            );
            let spans = analysis.group_spans();
            let first = spans.group(nested[0].count_field_idx).expect("a span");
            let second = spans.group(nested[1].count_field_idx).expect("a span");
            assert_ne!(nested[0].count_field_idx, nested[1].count_field_idx);
            assert!(
                first.end <= second.start,
                "two ranges, not one merged: {first:?} then {second:?}"
            );
        }

        /// A count that opens nothing has no span at all — not an empty range
        /// at its own start, which a consumer would draw.
        #[test]
        fn a_count_that_opens_nothing_has_no_span() {
            for line in [
                "8=FIX.4.4|35=W|55=X|268=0|10=000",
                "8=FIX.4.4|35=W|55=X|268=x|10=000",
                "8=FIX.4.4|35=W|55=X|268=-1|10=000",
            ] {
                let analysis = decode_as_44(line);
                assert!(analysis.groups.is_empty(), "{line}");
                assert_eq!(analysis.group_spans().group(3), None, "{line}");
            }
        }

        /// The span covers what was consumed, not what the count claimed.
        #[test]
        fn a_count_larger_than_the_fields_present_stops_at_the_end() {
            let line = "8=FIX.4.4|35=W|55=X|268=5|269=0|270=1.5|10=000";
            let analysis = decode_as_44(line);
            assert_eq!(analysis.groups.len(), 1, "one entry, not five");
            assert_eq!(analysis.group_spans().instance(0), Some(byte(26)..byte(39)));
        }

        /// A group does not resume after a non-member, and neither does its
        /// span: the trailing 269 is outside, and swallowing it would put a
        /// field in a group the decoder says it is not in.
        #[test]
        fn a_group_does_not_resume_after_a_non_member() {
            let line = "8=FIX.4.4|35=W|55=X|268=2|269=0|55=Y|269=1|10=000";
            let analysis = decode_as_44(line);
            let trailing = analysis.fields.iter().rfind(|f| f.tag == 269).expect("269");
            assert_eq!(trailing.group_idx(), None, "the second 269 is in no entry");
            let span = analysis.group_spans().instance(0).expect("a span");
            assert!(
                span.end <= trailing.tag_start,
                "the span stops before it: {span:?}"
            );
        }

        /// What the two passes rest on: a child is always pushed after its
        /// parent, so one reverse pass merges every child upward. Asserted on
        /// the decoder rather than assumed by the consumer.
        #[test]
        fn an_instance_always_follows_its_parent() {
            let analysis = decode_as_44(
                "8=FIX.4.4|35=D|453=2|448=A|447=D|452=1|802=2|523=S1|803=1|523=S2|803=2\
                 |448=B|447=D|452=2|802=1|523=S3|803=3|54=1",
            );
            assert!(analysis.groups.len() > 3, "a chain worth checking");
            for (at, instance) in analysis.groups.iter().enumerate() {
                if let Some(parent) = instance.parent_idx() {
                    assert!(
                        (parent as usize) < at,
                        "instance {at} has parent {parent}, which is not earlier"
                    );
                }
            }
        }
    }
}
