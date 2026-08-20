//! `tag(Name)=value(Label)` — a field spelled out in full.
//!
//! **The switches do not reach here, the formats do.** An inlay hint is a
//! reading aid a user turns off when the screen gets busy; this text is the
//! whole answer to an explicit request, and a client that asked for a decode
//! and got the bytes back unchanged was given nothing. But *how* a field is
//! spelled is one choice, not two, so `inlayHints.tagNames.format`,
//! `.enumValues.format` and `groupPaths` all apply — only `enabled` does not.

use crate::codec::{Field, MessageAnalysis};

use super::Options;

/// Between fields, as the log itself reads. Not `SOH`: this text is going into
/// a register or a clipboard, where a control byte is invisible.
const SEPARATOR: char = '|';

/// The selected fields of one line.
pub fn line<'a>(
    analysis: &MessageAnalysis,
    bytes: &[u8],
    fields: impl Iterator<Item = &'a Field>,
    options: &Options,
) -> String {
    let mut out = String::new();
    for field in fields {
        if !out.is_empty() {
            out.push(SEPARATOR);
        }
        push_field(analysis, bytes, field, options, &mut out);
    }
    out
}

fn push_field(
    analysis: &MessageAnalysis,
    bytes: &[u8],
    field: &Field,
    options: &Options,
    out: &mut String,
) {
    // The digits as written, not `field.tag`: a run that overflowed a `u32`
    // reads as `4294967295` there and as itself here.
    out.push_str(&String::from_utf8_lossy(field.tag_digits(bytes)));
    if let Some(label) = super::hint::tag_label(analysis, field, bytes, options) {
        out.push_str(&label);
    }
    out.push('=');
    // Lossy because the answer is a JSON string and a FIX value need not be
    // text — RawData carries whatever the sender put there.
    out.push_str(&String::from_utf8_lossy(field.value(bytes)));
    if let Some(label) = super::hint::value_label(analysis, field, options) {
        out.push_str(&label);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::LineAnalysis;
    use crate::ctx::{analyze, Ctx};
    use serde_json::json;

    const GROUPS: &str =
        "8=FIX.4.4|9=124|35=W|34=3|49=EXCHANGE|56=CLIENT|52=20251026-09:05:00.000|\
55=BTCUSD|268=2|269=0|270=65000.50|271=1.5|269=1|270=65001.00|271=2.0|10=145|";

    fn message_of(line: &[u8]) -> MessageAnalysis {
        match analyze(line, &Ctx::for_test("FIX.4.4")) {
            LineAnalysis::Message(message) => message,
            other => panic!("expected a message, got {:?}", other.kind()),
        }
    }

    /// Every field of a line, which is what a linewise selection asks for.
    fn whole(bytes: &[u8]) -> String {
        let message = message_of(bytes);
        line(&message, bytes, message.fields.iter(), &Options::default())
    }

    #[test]
    fn a_field_carries_its_name_and_its_enum_label() {
        let decoded = whole(b"8=FIX.4.4|35=0|34=1|10=171|");
        assert_eq!(
            decoded,
            "8(BeginString)=FIX.4.4|35(MsgType)=0(Heartbeat)|\
34(MsgSeqNum)=1|10(CheckSum)=171"
        );
    }

    /// Inside a group the path replaces the name, as the hint's does — the two
    /// read `tag_label` rather than each deciding.
    #[test]
    fn a_field_in_a_group_carries_its_path() {
        let decoded = whole(GROUPS.as_bytes());
        assert!(
            decoded.contains("269(NoMDEntries/1/MDEntryType)=0(Bid)"),
            "{decoded}"
        );
        assert!(
            decoded.contains("270(NoMDEntries/2/MDEntryPx)=65001.00"),
            "{decoded}"
        );
    }

    /// The values Orchestra restores decode like any other: FIX44.xml gives
    /// PartySubIDType(803) no values at all, so `1` would otherwise be bare.
    #[test]
    fn a_restored_value_decodes_with_its_label() {
        let decoded = whole(b"8=FIX.4.4|35=D|453=1|448=A|447=D|452=1|802=1|523=S1|803=1|10=171|");
        assert!(
            decoded.contains("803(NoPartyIDs/1/NoPartySubIDs/1/PartySubIDType)=1(Firm)"),
            "{decoded}"
        );
    }

    /// A tag the dictionary does not define still decodes; it just has no name.
    #[test]
    fn an_unknown_tag_keeps_its_digits_and_its_value() {
        let decoded = whole(b"8=FIX.4.4|35=0|9999=whatever|10=171|");
        assert!(decoded.contains("|9999=whatever|"), "{decoded}");
    }

    #[test]
    fn a_value_that_is_not_text_is_written_lossily() {
        let decoded = whole(b"8=FIX.4.4|35=0|58=\xff\xfe|10=171|");
        assert!(decoded.contains("58(Text)=\u{fffd}\u{fffd}"), "{decoded}");
    }

    fn rendered(config: serde_json::Value) -> Options {
        let config: crate::config::Config = serde_json::from_value(config).expect("config");
        Options::from_config(&config).0
    }

    /// The formats reach this channel; the switches do not. A user who turned
    /// the hints off is reading the bytes unaided on screen and still wants a
    /// decode to say what a tag is.
    #[test]
    fn a_decode_follows_the_formats_and_not_the_switches() {
        let bytes = b"8=FIX.4.4|35=0|34=1|";
        let message = message_of(bytes);
        let options = rendered(json!({
            "inlayHints": {
                "tagNames": { "enabled": false, "format": "[{}]" },
                "enumValues": { "enabled": false, "format": "->{}" },
            }
        }));
        assert_eq!(
            line(&message, bytes, message.fields.iter(), &options),
            "8[BeginString]=FIX.4.4|35[MsgType]=0->Heartbeat|34[MsgSeqNum]=1"
        );
    }

    /// `groupPaths` reaches it too, which is why `Options` carries it: a
    /// snapshot has no `Config` to ask.
    #[test]
    fn a_decode_follows_group_paths() {
        let bytes = GROUPS.as_bytes();
        let message = message_of(bytes);
        let off = rendered(json!({
            "inlayHints": { "groupPaths": { "enabled": false } }
        }));
        let decoded = line(&message, bytes, message.fields.iter(), &off);
        assert!(decoded.contains("269(MDEntryType)=0(Bid)"), "{decoded}");
        assert!(!decoded.contains("NoMDEntries/"), "{decoded}");
    }

    /// An empty value is legal FIX. It matches no enum, so the value label is
    /// suppressed rather than drawn empty.
    #[test]
    fn an_empty_value_carries_no_label() {
        let bytes = b"8=FIX.4.4|35=0|58=|10=1|";
        let message = message_of(bytes);
        let decoded = line(&message, bytes, message.fields.iter(), &Options::default());
        assert!(decoded.contains("|58(Text)=|"), "{decoded}");
    }

    /// A selection is a subset, and the separator belongs between fields
    /// rather than after each — an empty selection is the empty string.
    #[test]
    fn only_the_fields_it_is_given() {
        let bytes = b"8=FIX.4.4|35=0|34=1|10=171|";
        let message = message_of(bytes);
        let picked: Vec<_> = message
            .fields
            .iter()
            .filter(|field| field.tag == 34 || field.tag == 35)
            .collect();
        assert_eq!(
            line(&message, bytes, picked.into_iter(), &Options::default()),
            "35(MsgType)=0(Heartbeat)|34(MsgSeqNum)=1"
        );
        assert_eq!(line(&message, bytes, [].iter(), &Options::default()), "");
    }
}
