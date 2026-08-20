//! Turning `LineAnalysis` plus a `Dict` into text. Later stages add the hover,
//! inlay-hint and symbol renderers here.

pub mod decode;
pub mod hint;
pub mod hover;
pub mod label_format;
pub mod summary;
pub mod symbol;
pub mod tag_url;
pub mod template;
pub mod tokens;

pub use hint::{summary_joined, summary_parts};
pub use hover::hover;
pub use label_format::{LabelFormat, DEFAULT_LABEL_FORMAT};
pub use summary::{summary, Summary};
pub use tag_url::TagUrl;
pub use template::{Template, TemplateError, DEFAULT_TEMPLATE};

use crate::config::Config;

/// What rendering needs from configuration. Deliberately not inside `Ctx`:
/// none of it can invalidate a cached `LineAnalysis`, because an analysis holds
/// no rendered strings. Keeping it out is what lets a `Ctx`'s `Arc` identity
/// mean "the analyses are still valid" without a second comparison.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub template: Template,
    /// The outline's message name. Separate from `template` because an outline
    /// row and an end-of-line annotation are read differently.
    pub symbol_template: Template,
    pub details: bool,
    /// How one field's tag and value are spelled, and whether a tag inside a
    /// group is spelled with its path.
    ///
    /// `group_paths` is here as well as in `InlayHints` because `fix.decode`
    /// renders the same labels from a `Snapshot`, which carries `render` and no
    /// `Config`. The switches stay behind, because that channel ignores them.
    pub tag_format: LabelFormat,
    pub value_format: LabelFormat,
    pub group_paths: bool,
    /// Where a tag's reference page lives. Request-time like the rest of this
    /// struct: changing it re-renders, it cannot invalidate an analysis.
    pub tag_url: TagUrl,
}

impl Options {
    /// A broken template falls back to its own default and is returned so the
    /// caller can report it once — **named**, because two settings can be
    /// broken at once and "the template is invalid" would not say which.
    pub fn from_config(config: &Config) -> (Options, Vec<BadTemplate>) {
        let mut bad = Vec::new();
        let template = parse_or_default(
            &config.message_summary.template,
            Template::parse,
            Template::default_template(),
            BadTemplate::Summary,
            &mut bad,
        );
        let symbol_template = parse_or_default(
            &config.document_symbol.template,
            Template::parse,
            Template::default_symbol_template(),
            BadTemplate::Symbol,
            &mut bad,
        );
        let tag_format = parse_or_default(
            config.inlay_hints.tag_format(),
            LabelFormat::parse,
            LabelFormat::default_label(),
            BadTemplate::TagFormat,
            &mut bad,
        );
        let value_format = parse_or_default(
            config.inlay_hints.value_format(),
            LabelFormat::parse,
            LabelFormat::default_label(),
            BadTemplate::ValueFormat,
            &mut bad,
        );
        (
            Options {
                template,
                symbol_template,
                tag_format,
                value_format,
                group_paths: config.inlay_hints.group_paths(),
                details: config.message_summary.details,
                tag_url: TagUrl::new(config.document_link.url.clone()),
            },
            bad,
        )
    }
}

/// Which setting failed to parse. The message a user acts on names the key.
#[derive(Debug, PartialEq)]
pub enum BadTemplate {
    Summary(TemplateError),
    Symbol(TemplateError),
    TagFormat(TemplateError),
    ValueFormat(TemplateError),
}

impl BadTemplate {
    pub fn setting(&self) -> &'static str {
        match self {
            BadTemplate::Summary(_) => "messageSummary.template",
            BadTemplate::Symbol(_) => "documentSymbol.template",
            BadTemplate::TagFormat(_) => "inlayHints.tagNames.format",
            BadTemplate::ValueFormat(_) => "inlayHints.enumValues.format",
        }
    }

    pub fn error(&self) -> &TemplateError {
        match self {
            BadTemplate::Summary(e)
            | BadTemplate::Symbol(e)
            | BadTemplate::TagFormat(e)
            | BadTemplate::ValueFormat(e) => e,
        }
    }
}

/// Generic over what is being parsed, because a template and a field format
/// fail and fall back the same way and differ only in their grammar.
fn parse_or_default<T: Clone>(
    text: &str,
    parse: fn(&str) -> Result<T, TemplateError>,
    fallback: &T,
    which: fn(TemplateError) -> BadTemplate,
    bad: &mut Vec<BadTemplate>,
) -> T {
    match parse(text) {
        Ok(parsed) => parsed,
        Err(e) => {
            bad.push(which(e));
            fallback.clone()
        }
    }
}

impl Default for Options {
    fn default() -> Options {
        Options::from_config(&Config::default()).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MessageSummary;

    #[test]
    fn a_bad_template_is_reported_and_the_default_is_used() {
        let config = Config {
            message_summary: MessageSummary {
                template: "{nope}".into(),
                ..Default::default()
            },
            ..Config::default()
        };
        let (options, bad) = Options::from_config(&config);
        assert!(matches!(
            bad.as_slice(),
            [BadTemplate::Summary(TemplateError::UnknownRef { .. })]
        ));
        assert_eq!(&options.template, Template::default_template());
    }

    /// Each setting falls back on its own: a broken tag format must not cost
    /// the value format, and the message names the key a user would edit.
    #[test]
    fn a_bad_field_format_is_reported_by_name_and_only_it_falls_back() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "inlayHints": {
                "tagNames": { "format": "{nope}" },
                "enumValues": { "format": "<{}>" },
            }
        }))
        .expect("config");
        let (options, bad) = Options::from_config(&config);
        assert!(
            matches!(
                bad.as_slice(),
                [BadTemplate::TagFormat(TemplateError::UnknownRef { .. })]
            ),
            "{bad:?}"
        );
        assert_eq!(bad[0].setting(), "inlayHints.tagNames.format");
        assert_eq!(&options.tag_format, LabelFormat::default_label());
        assert_eq!(
            &options.value_format,
            &LabelFormat::parse("<{}>").unwrap(),
            "the good one is untouched"
        );
    }

    /// `group_paths` is in two places and they must not disagree: the hints
    /// read the switch, `fix.decode` reads only this.
    #[test]
    fn group_paths_reaches_the_render_options() {
        assert!(Options::default().group_paths, "on by default");
        let off: Config = serde_json::from_value(serde_json::json!({
            "inlayHints": { "groupPaths": { "enabled": false } }
        }))
        .expect("config");
        assert!(!Options::from_config(&off).0.group_paths);
    }

    /// The reference URL is `fix.documentLink.url`, not a constant: a desk with
    /// its own dictionary browser points it there, and `documentLink` (4c) and
    /// `fix.tagUrl` (4d) will read the same key rather than growing their own.
    #[test]
    fn the_reference_url_comes_from_configuration() {
        let configured: Config = serde_json::from_value(serde_json::json!({
            "documentLink": { "url": "https://fix.internal/{version}/t{tag}" }
        }))
        .expect("config");
        let (options, bad) = Options::from_config(&configured);
        assert!(bad.is_empty());
        assert_eq!(
            options.tag_url.of("FIX.4.4", 35).as_deref(),
            Some("https://fix.internal/4.4/t35")
        );
        assert_eq!(
            Options::default().tag_url.of("FIX.4.4", 35).as_deref(),
            Some("https://www.onixs.biz/fix-dictionary/4.4/tagNum_35.html"),
            "and the default is still Onixs"
        );
    }

    #[test]
    fn a_good_template_and_its_details_flag_both_arrive() {
        let config = Config {
            message_summary: MessageSummary {
                template: "{35:name}".into(),
                details: false,
                ..Default::default()
            },
            ..Config::default()
        };
        let (options, bad) = Options::from_config(&config);
        assert!(bad.is_empty());
        assert_ne!(&options.template, Template::default_template());
        assert!(!options.details, "details is configuration, not a constant");
    }
}
