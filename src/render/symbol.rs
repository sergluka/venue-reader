//! The outline's names.
//!
//! Self-contained on purpose: a client may show a symbol's `name` and nothing
//! else, so a row that needs its `detail` to be identifiable is not identifiable
//! everywhere.

use crate::codec::MessageAnalysis;
use crate::dictionary::GroupIdx;

use super::{summary, Options, Template};

/// The message row, from `fix.documentSymbol.template`.
///
/// A blank name falls back to the default shape. LSP forbids an empty *or*
/// whitespace-only name, and a perfectly valid template like `{49}` renders
/// blank on a message that carries no SenderCompID(49) — so this is a
/// rendering rule and
/// not a configuration error, which is why nothing is reported here. The
/// default cannot itself render blank: its literals are not whitespace.
pub fn message(line: &[u8], analysis: &MessageAnalysis, options: &Options) -> String {
    let name = summary::render(line, analysis, &options.symbol_template);
    if name.trim().is_empty() {
        return summary::render(line, analysis, Template::default_symbol_template());
    }
    name
}

/// `Side (54) = 1 (Buy)` — and `54 = 1` when the dictionary does not know the
/// tag, because a bare `(54)` reads as a name that failed to render.
pub fn field(line: &[u8], analysis: &MessageAnalysis, at: usize) -> String {
    let Some(field) = analysis.fields.get(at) else {
        return String::new();
    };
    let mut out = match analysis.field_name(field) {
        Some(name) => format!("{name} ({})", field.tag),
        None => field.tag.to_string(),
    };
    // A value that is not UTF-8 is left off rather than replaced: the name
    // still identifies the field, and any stand-in would be invented.
    if let Some(value) = field.text(line).filter(|text| !text.is_empty()) {
        out.push_str(" = ");
        out.push_str(value);
        if let Some(label) = analysis.enum_label(field) {
            out.push_str(" (");
            out.push_str(label);
            out.push(')');
        }
    }
    out
}

/// `NoMDEntries (268) = 3`, named from the *group* rather than from its count
/// field: the two usually agree, and where they do not the group's own name is
/// what the tree is about.
pub fn group(
    line: &[u8],
    analysis: &MessageAnalysis,
    count_field_idx: usize,
    of: GroupIdx,
) -> String {
    let name = analysis.dict.group(of).name();
    let Some(count) = analysis.fields.get(count_field_idx) else {
        return name.to_owned();
    };
    match count.text(line).filter(|text| !text.is_empty()) {
        Some(value) => format!("{name} ({}) = {value}", count.tag),
        None => format!("{name} ({})", count.tag),
    }
}

/// `[1]`. The number is the entry as a person counts them, which is the one
/// place `GroupInstance::entry` is used.
pub fn instance(entry: u32) -> String {
    format!("[{entry}]")
}

/// What the budget left out. Never a bare `[]`: an outline that stops without
/// saying so reads as a server that gave up.
pub fn truncated(messages: usize) -> String {
    format!("… {messages} more messages")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::LineAnalysis;
    use crate::ctx::{analyze, Ctx};

    fn message_of(text: &str) -> LineAnalysis {
        analyze(text.as_bytes(), &Ctx::for_test("FIX.4.4"))
    }

    #[test]
    fn a_field_carries_its_name_its_tag_its_value_and_its_label() {
        let analysis = message_of("8=FIX.4.4|35=D|54=1|9999=x");
        let message = analysis.message().expect("a message");
        let line = b"8=FIX.4.4|35=D|54=1|9999=x";
        assert_eq!(field(line, message, 2), "Side (54) = 1 (Buy)");
        assert_eq!(
            field(line, message, 3),
            "9999 = x",
            "an unknown tag names itself"
        );
    }

    #[test]
    fn a_group_is_named_by_the_group_and_carries_its_count() {
        let text = "8=FIX.4.4|35=D|453=2|448=A|447=D|452=1|448=B|447=D|452=2";
        let analysis = message_of(text);
        let message = analysis.message().expect("a message");
        let first = &message.groups[0];
        assert_eq!(
            group(
                text.as_bytes(),
                message,
                first.count_field_idx as usize,
                first.group
            ),
            "NoPartyIDs (453) = 2"
        );
        assert_eq!(instance(first.entry), "[1]");
    }

    /// The one failure the fallback exists for: a template that parses and then
    /// renders to nothing on this particular message.
    #[test]
    fn a_name_that_renders_blank_falls_back_to_the_default_shape() {
        let text = "8=FIX.4.4|35=D|54=1";
        let analysis = message_of(text);
        let decoded = analysis.message().expect("a message");
        let options = Options {
            symbol_template: Template::parse("{49|}").expect("parses"),
            ..Options::default()
        };
        let name = message(text.as_bytes(), decoded, &options);
        assert!(
            !name.trim().is_empty(),
            "a blank render must not reach the client: {name:?}"
        );
        assert!(name.contains("(D)"), "the default shape: {name}");
    }
}
