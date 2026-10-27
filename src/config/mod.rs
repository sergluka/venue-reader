//! Typed, materialized configuration and its schema defaults.
//!
//! Sparse source parsing, precedence and origin tracking live in `overrides`;
//! this module describes the effective values consumed by the server.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

pub mod user_file;
use serde_json::{Map, Value};

use crate::dictionary::binding::to_wire;

/// Fully materialized effective configuration.
///
/// Sparse user, client, project and modeline layers are merged before this
/// type is built, so every field contains its schema default when no source
/// supplied a value.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UserConfig {
    /// Whether configuration may load dictionaries from filesystem paths.
    pub allow_dictionary_path: bool,
    /// Gates for document-local configuration sources.
    pub overrides: ConfigOverrides,
    /// Canonical release fallback for missing ApplVerID(1128); selected pairs take precedence.
    #[serde(
        rename = "defaultApplVerID",
        deserialize_with = "deserialize_application_version"
    )]
    pub default_appl_ver_id: Option<String>,
    /// Named external dictionary definitions visible to this configuration.
    pub dictionaries: Vec<DictionarySpec>,
    /// One-line message summary and its delivery channels.
    pub message_summary: MessageSummary,
    /// Diagnostic master switch and per-rule settings.
    pub diagnostics: Diagnostics,
    /// Template used to build external tag-reference URLs.
    pub document_link: DocumentLink,
    /// Document outline settings.
    pub document_symbol: DocumentSymbol,
    /// Hover response settings.
    pub hover: Hover,
    /// Inline tag, enum-value and group-path annotations.
    pub inlay_hints: InlayHints,
    /// Resource and response-size bounds.
    pub limits: Limits,
    /// Semantic-token response settings.
    pub semantic_tokens: SemanticTokens,
}

/// Effective configuration used by the server runtime.
pub type Config = UserConfig;

/// Gates for sources that can override document-level settings.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ConfigOverrides {
    /// Whether discovered `.venue-reader.toml` files contribute project layers.
    pub project_config: bool,
    /// Whether edge modelines contribute a document-local layer.
    pub modeline: bool,
}

impl Default for ConfigOverrides {
    fn default() -> Self {
        Self {
            project_config: true,
            modeline: true,
        }
    }
}

/// An optional switch; omission keeps the owning setting's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Toggle {
    /// Explicit value, or `None` to use the owning setting's default.
    pub enabled: Option<bool>,
}

impl Toggle {
    fn or(self, default: bool) -> bool {
        self.enabled.unwrap_or(default)
    }
}

/// `fix.semanticTokens` — whether the server colours a document at all.
///
/// The provider stays advertised when this is off, as `hover.enabled` does:
/// the switch gates the answer, not the capability, so a client need not
/// reconnect to change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SemanticTokens {
    /// Whether semantic-token requests return colouring information.
    pub enabled: bool,
}

impl Default for SemanticTokens {
    // Manual: a derived Default would silently mean "no colours".
    fn default() -> Self {
        SemanticTokens { enabled: true }
    }
}

/// The schema's `$defs/label`: a switch, plus how the label it draws is
/// written. `enabled` is `Option` for `Toggle`'s reason — "written but said
/// nothing" has to fall back to the owner's default, not the type's.
///
/// **Not a flattened `Toggle`, though it would spell the same JSON.**
/// `serde_path_to_error` cannot see inside a flattened field, so a malformed
/// `tagNames.enabled` was reported as `inlayHints.tagNames` — a path naming an
/// object while the message asked for a boolean, which reads as if the section
/// itself should be one. One field of reuse is not worth the error a user acts
/// on.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Label {
    enabled: Option<bool>,
    format: Option<String>,
}

/// Inline field labels and repeating-group paths, read through their default-aware accessors.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InlayHints {
    tag_names: Label,
    enum_values: Label,
    group_paths: Toggle,
}

impl InlayHints {
    /// Whether a tag is annotated at all. **The gate**: with this off there is
    /// no tag hint, whatever `group_paths` says.
    pub fn tag_names(&self) -> bool {
        self.tag_names.enabled.unwrap_or(true)
    }

    /// Whether known enum values are annotated with their dictionary labels.
    pub fn enum_values(&self) -> bool {
        self.enum_values.enabled.unwrap_or(true)
    }

    /// Whether tag labels use group paths; applies only when tag hints are enabled.
    pub fn group_paths(&self) -> bool {
        self.group_paths.or(true)
    }

    /// The raw format strings. Parsed into `render::Options` when the
    /// configuration commits, so a broken one is reported there rather than on
    /// every line.
    pub fn tag_format(&self) -> &str {
        self.tag_names
            .format
            .as_deref()
            .unwrap_or(crate::render::DEFAULT_LABEL_FORMAT)
    }

    /// Raw format string used for enum-value labels.
    pub fn value_format(&self) -> &str {
        self.enum_values
            .format
            .as_deref()
            .unwrap_or(crate::render::DEFAULT_LABEL_FORMAT)
    }
}

/// `fix.hover` — whether hover answers at all.
///
/// The provider stays advertised when this is off; `*.enabled` gates the
/// response, not the capability, so a client need not reconnect to change it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Hover {
    /// Whether hover requests return field information.
    pub enabled: bool,
}

impl Default for Hover {
    // Manual: a derived Default would silently mean "hover off".
    fn default() -> Self {
        Hover { enabled: true }
    }
}

/// `fix.documentSymbol` — the outline of a log.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DocumentSymbol {
    /// Whether document-symbol requests return an outline.
    pub enabled: bool,
    /// Most detailed kind of node included in the outline.
    pub depth: SymbolDepth,
    /// The message symbol's name. Same slots as `messageSummary.template` and
    /// deliberately a *separate* setting: an outline row and an end-of-line
    /// annotation are read differently, and one that renders to nothing is a
    /// blank annotation in the summary but a list of unnameable rows here.
    ///
    /// Fields, groups and instances are not templated — only the message line
    /// is one a venue would re-shape, and four templates would be four ways to
    /// produce that unnameable list.
    pub template: String,
}

/// How far down the outline goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SymbolDepth {
    /// One symbol per message line.
    Messages,
    /// Fields too, with groups flattened away.
    Fields,
    /// Groups and their entries as well.
    Full,
}

impl Default for DocumentSymbol {
    // Manual: a derived Default would mean "no outline" and an empty name.
    fn default() -> Self {
        DocumentSymbol {
            enabled: true,
            depth: SymbolDepth::Full,
            template: DEFAULT_SYMBOL_TEMPLATE.into(),
        }
    }
}

/// Self-contained on purpose: a client may show only `name`, so a row that
/// needs its `detail` to be identifiable is not identifiable everywhere.
pub const DEFAULT_SYMBOL_TEMPLATE: &str = "#{34} {49}→{56} {35:name} ({35})";

/// `fix.documentLink` — where a tag's reference page lives.
///
/// Read by hover, and by `fix.tagUrl` in 4(d) — one URL rather than a copy per
/// consumer. The name outlived the capability it was chosen for: `documentLink`
/// was dropped in 4c-1, and emptying this is how the link is turned off.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DocumentLink {
    /// Substituted: `{tag}`; `{version}`, the version as the *default site*
    /// spells it (`4.4`, `5.0.SP1`), which is what makes the default URL
    /// correct; and `{fixVersion}`, the line's own version string.
    ///
    /// Both exist because the site's spelling reads `FIXT.1.1` as the SP2
    /// tables and names no version the site does not publish. A template with
    /// `{version}` points into a version's pages, so hover links it only for a
    /// tag that version's bundled dictionary defines; any other template links
    /// every tag.
    pub url: String,
}

impl Default for DocumentLink {
    // Manual: a derived Default would mean an empty URL, which is how a user
    // turns the reference link off.
    fn default() -> Self {
        DocumentLink {
            url: "https://www.onixs.biz/fix-dictionary/{version}/tagNum_{tag}.html".into(),
        }
    }
}

/// `fix.messageSummary` — everything about the one-line summary, including
/// which LSP channels carry it. The template is parsed once when the config is
/// committed, so a broken one is reported there rather than on every render.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MessageSummary {
    /// Master gate for producing message summaries.
    pub enabled: bool,
    /// LSP features used to deliver each summary.
    pub channels: SummaryChannels,
    /// Template rendered from fields in the message.
    pub template: String,
    /// Append the per-MsgType detail as a second part.
    pub details: bool,
}

impl Default for MessageSummary {
    // Manual: a derived Default would disable summaries and blank the template.
    fn default() -> Self {
        MessageSummary {
            enabled: true,
            channels: SummaryChannels::default(),
            template: crate::render::DEFAULT_TEMPLATE.to_owned(),
            details: true,
        }
    }
}

/// Enabling both channels draws the same summary twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SummaryChannels {
    /// Render summaries above lines through `textDocument/codeLens`.
    pub code_lens: bool,
    /// Render summaries inline through `textDocument/inlayHint`.
    pub inlay_hints: bool,
}

impl Default for SummaryChannels {
    // A generic client gets the codeLens channel: it renders above the line,
    // while an inlay hint at column 0 pushes the text right.
    //
    // A client that renders per viewport flips both — not because it would
    // draw the summary differently, but because `CodeLensParams` carries no
    // range: it would have to ask for the whole document, and past
    // `limits.codeLensLines` the answer is `[]`, so on a large log that channel
    // returns nothing at all rather than the same thing slower.
    fn default() -> Self {
        SummaryChannels {
            code_lens: true,
            inlay_hints: false,
        }
    }
}

/// A named pair or transport family; paths are relative to the defining layer.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", try_from = "DictionarySpecInput")]
pub struct DictionarySpec {
    /// Single XML source; its root determines the binding form.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub name: String,
    /// Requests automatic selection for this exact binding; conflicts can disable the claim.
    pub default: bool,
    /// Header/trailer/admin source: bundled identifier or XML path, never a registration name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Application body source: bundled identifier or XML path, never a registration name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    /// Canonical application release; supplies FIXT-rooted identity, otherwise must match XML.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application_version: Option<String>,
    /// Exact application extension pack; `None` differs from `Some(0)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension_pack: Option<u32>,
    /// Exact CstmApplVerID(1129) identity, separate from the release and extension pack.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_version: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DictionarySpecInput {
    #[serde(default)]
    path: Option<String>,
    name: String,
    #[serde(default)]
    default: bool,
    transport: Option<String>,
    application: Option<String>,
    application_version: Option<String>,
    extension_pack: Option<u32>,
    custom_version: Option<String>,
}

impl TryFrom<DictionarySpecInput> for DictionarySpec {
    type Error = String;

    fn try_from(input: DictionarySpecInput) -> Result<Self, Self::Error> {
        let spec = Self {
            path: input.path,
            name: input.name,
            default: input.default,
            transport: input.transport,
            application: input.application,
            application_version: input.application_version,
            extension_pack: input.extension_pack,
            custom_version: input.custom_version,
        };
        spec.validate_shape()?;
        Ok(spec)
    }
}

impl DictionarySpec {
    pub(crate) fn source_description(&self) -> String {
        if let Some(path) = &self.path {
            return path.clone();
        }
        match (self.transport.as_deref(), self.application.as_deref()) {
            (Some(transport), Some(application)) => {
                format!("transport: {transport}, application: {application}")
            }
            (Some(transport), None) => format!("transport: {transport}"),
            (None, Some(application)) => format!("application: {application}"),
            (None, None) => String::new(),
        }
    }

    pub fn validate_shape(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("name must not be blank".into());
        }
        if let Some(path) = &self.path {
            if path.trim().is_empty() {
                return Err("path must not be blank".into());
            }
            if self.transport.is_some() || self.application.is_some() {
                return Err("path is mutually exclusive with transport and application".into());
            }
        } else if self.transport.is_none() {
            return Err("a path or transport source is required".into());
        }
        for (key, value) in [
            ("transport", self.transport.as_deref()),
            ("application", self.application.as_deref()),
            ("customVersion", self.custom_version.as_deref()),
        ] {
            if value.is_some_and(|value| value.trim().is_empty()) {
                return Err(format!("{key} must not be blank"));
            }
        }
        if let Some(version) = &self.application_version {
            if to_wire(version).is_none() {
                return Err("applicationVersion must be a canonical application release".into());
            }
        }
        if self.path.is_none()
            && self.application.is_none()
            && (self.application_version.is_some()
                || self.extension_pack.is_some()
                || self.custom_version.is_some())
        {
            return Err("application identity requires an application source".into());
        }
        Ok(())
    }

    pub fn uses_paths(&self) -> bool {
        self.path.is_some()
            || [self.transport.as_deref(), self.application.as_deref()]
                .into_iter()
                .flatten()
                .any(|source| crate::dictionary::Version::parse(source).is_none())
    }
}

fn deserialize_application_version<'de, Deserializer>(
    deserializer: Deserializer,
) -> Result<Option<String>, Deserializer::Error>
where
    Deserializer: serde::Deserializer<'de>,
{
    let version = Option::<String>::deserialize(deserializer)?;
    if version
        .as_deref()
        .is_some_and(|version| to_wire(version).is_none())
    {
        return Err(serde::de::Error::custom(
            "defaultApplVerID must be null or a canonical application release",
        ));
    }
    Ok(version)
}

/// `fix.diagnostics` — the master gate and per-rule settings. Deliberately not
/// a `CtxFingerprint` input: rules are a pure function of the analysis plus
/// this, so toggling one invalidates no cached line.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Diagnostics {
    /// Master gate for running and publishing diagnostics.
    pub enabled: bool,
    /// Keyed by rule id. An absent id means the rule's own default severity; an
    /// id no rule claims is inert here and reported once when the config is
    /// committed, because a typo must not look like a setting that took effect.
    pub rules: BTreeMap<String, RuleSetting>,
}

impl Default for Diagnostics {
    // Manual: a derived Default would ship with diagnostics off.
    fn default() -> Self {
        Diagnostics {
            enabled: true,
            rules: BTreeMap::new(),
        }
    }
}

/// One rule's settings: `true`/`false`, or a table.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum RuleSetting {
    /// Boolean shorthand that changes only the rule's enabled state.
    Enabled(bool),
    /// Per-rule switches, severity and rule-specific opaque options.
    Detailed {
        /// Explicit enabled state, or the rule's own default when absent.
        #[serde(skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
        /// Explicit severity, or the rule's own default when absent.
        #[serde(skip_serializing_if = "Option::is_none")]
        severity: Option<Severity>,
        /// The rule's own options. Nothing reads them yet; they are kept rather
        /// than dropped because Stage 5's `fix.describeOverrides` has to report
        /// the effective configuration. The same preservation rule applies to
        /// [`Limits::rest`].
        #[serde(flatten)]
        rest: Map<String, Value>,
    },
}

impl RuleSetting {
    /// `None` when the entry says nothing about it.
    pub fn enabled(&self) -> Option<bool> {
        match self {
            RuleSetting::Enabled(on) => Some(*on),
            RuleSetting::Detailed { enabled, .. } => *enabled,
        }
    }

    /// Explicit severity, or `None` when the rule keeps its default.
    pub fn severity(&self) -> Option<Severity> {
        match self {
            RuleSetting::Enabled(_) => None,
            RuleSetting::Detailed { severity, .. } => *severity,
        }
    }

    /// Rule-specific options not interpreted by the configuration layer.
    pub fn options(&self) -> &Map<String, Value> {
        static EMPTY: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);
        match self {
            RuleSetting::Enabled(_) => &EMPTY,
            RuleSetting::Detailed { rest, .. } => rest,
        }
    }
}

/// A diagnostic's severity, named as the schema names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

impl Severity {
    /// Most severe first; [`Severity::rank`] indexes it.
    pub const ALL: [Severity; 4] = [
        Severity::Error,
        Severity::Warning,
        Severity::Info,
        Severity::Hint,
    ];

    /// 0 for the most severe. An index rather than `Ord`, under which `Error <
    /// Hint` would read as "an error is less".
    pub fn rank(self) -> usize {
        self as usize
    }
}

// `ALL` and `rank` index each other; reordering the variants must not
// silently break that.
const _: () = {
    let mut rank = 0;
    while rank < Severity::ALL.len() {
        assert!(Severity::ALL[rank] as usize == rank);
        rank += 1;
    }
};

/// Bounds on external input, retained analysis and LSP response sizes.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Limits {
    /// Per-file cap for dictionary XML,
    /// checked on stat before opening and re-checked after a bounded read. A
    /// file over the cap is never parsed: the entry is refused with an error
    /// and its version falls back to the bundled dictionary.
    pub dictionary_bytes: u64,
    /// Cap on the diagnostics published for one document; beyond it the walker
    /// adds one entry saying how many more it found, and says so in the log.
    ///
    /// 10 000 rather than a rounder 2 000 because a log where every line is
    /// broken is a business case, not an anomaly, and showing a fifth of one is
    /// close to showing none. Measured on the worst case — 100 000 broken lines
    /// in 16 MB — the difference is 69 MB of peak RSS against 150 MB, and 270 ms
    /// against 400 ms. Linear, with no cliff between them; 20 000 would be
    /// 269 MB, which is where it stops being free.
    pub diagnostics_per_document: usize,
    /// Largest span of lines **any** ranged request will answer about: a code
    /// action, an inlay hint, an expanded selection, a coloured viewport, and
    /// whatever comes next.
    ///
    /// One number rather than one per request, because they are one question —
    /// how much of a document the server agrees to read for a request that
    /// names a span. Three of the four also populate the line cache, and that
    /// budget is the reason the number is 2000 rather than the file's length.
    ///
    /// **It bounds the span, not the answer.** Each request keeps its own
    /// behaviour at the boundary, which is a different question and settled in
    /// each handler: a code action is refused, hints and colours answer `[]`
    /// with one log line, an expanded selection still answers every position
    /// but with its own line only. Whole-document limits are elsewhere —
    /// `codeLensLines` and `documentSymbolItems` bound answers, not spans.
    pub range_lines: usize,
    /// Largest document `codeLens` will classify. `CodeLensParams` carries no
    /// range, so the lens *list* is whole-document by protocol; past this the
    /// answer is `[]`, because the alternative is a truncated list whose gaps a
    /// client cannot distinguish from lines that earned no lens.
    pub code_lens_lines: usize,
    /// Largest number of symbols one `documentSymbol` answer may hold, the
    /// truncation marker included. Counted in *symbols*, which scale with lines
    /// times fields — so unlike `codeLensLines` a user cannot predict it from
    /// the file. What makes that survivable is that the answer degrades by
    /// steps rather than emptying: children first, then messages only, then a
    /// prefix and one marker saying how many were left out.
    pub document_symbol_items: usize,
    /// How many lines a whole-document colouring answers about, from the top.
    ///
    /// Unlike a ranged request this one is bounded only by the file, and the
    /// answer is an integer array the client keeps in memory — so past the cap
    /// the tail is uncoloured rather than the request being slow. One log line
    /// names this setting, and only on a whole answer: after the first one a
    /// client sends edits, and saying it again would say it on every keystroke.
    ///
    /// **2000 is chosen by the client, not by the server.** Measured
    /// 2026-09-10 on a 10 000-line log at 63.7 fields a line: the whole file
    /// colours in 491 ms, so server time has room to spare, but a client
    /// rebuilds every range per response and 271 000 tokens costs 87 MB of heap
    /// there. 2000 lines is 254 882 tokens, just under it; 5000 would be around
    /// 200 MB. It also bounds an *edit*: two edits far apart before one delta
    /// collapse into a single span and re-encode everything between them, which
    /// is 44 ms at this cap and 111 ms at 5000.
    pub semantic_tokens_full_lines: usize,
    /// How many lines of one document may hold a cached analysis before the
    /// least recently read are dropped. Read through [`Limits::cache_bound`].
    ///
    /// Lines rather than bytes or a share of the file: memory is absolute,
    /// and a share grows with the file, which is the case a bound exists for.
    /// Measured 2026-09-11 at ~440 bytes a line, so 4000 is about 1.8 MB
    /// steady and twice that at the peak a sweep allows, per open document.
    pub cached_lines: usize,
    /// The schema's remaining `limits.*` keys, kept as raw JSON until the stage
    /// that enforces each one lifts it into a typed field.
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

impl Default for Limits {
    // Manual: a derived Default would silently mean "0 bytes allowed".
    fn default() -> Self {
        Limits {
            dictionary_bytes: 16 * 1024 * 1024,
            diagnostics_per_document: 10_000,
            range_lines: 2000,
            code_lens_lines: 20_000,
            document_symbol_items: 50_000,
            semantic_tokens_full_lines: 2000,
            cached_lines: 4000,
            rest: Map::new(),
        }
    }
}

impl Limits {
    /// `cachedLines`, never below `rangeLines`: one range request may populate
    /// that many, and a bound below it would sweep away its own answer.
    pub fn cache_bound(&self) -> usize {
        self.cached_lines.max(self.range_lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn application_default_uses_the_exact_public_key_and_canonical_release() {
        for version in [Value::Null, json!("FIX.5.0SP2"), json!("FIX.Latest")] {
            let config: Config =
                serde_json::from_value(json!({"defaultApplVerID": version})).unwrap();
            let serialized = serde_json::to_value(config).unwrap();
            assert_eq!(serialized["defaultApplVerID"], version);
            assert!(serialized.get("defaultVersion").is_none());
            assert!(serialized.get("defaultApplVerId").is_none());
        }
        for invalid in [json!("9"), json!("FIXT.1.1"), json!(""), json!(9)] {
            assert!(
                serde_json::from_value::<Config>(json!({"defaultApplVerID": invalid})).is_err()
            );
        }
    }

    #[test]
    fn registration_shapes_require_sources_and_keep_identity_dimensions() {
        for valid in [
            json!({"name":"pair", "transport":"FIXT.1.1", "application":"FIX.5.0SP2"}),
            json!({"name":"family", "transport":"FIXT.1.1"}),
            json!({"name":"combined", "path":"synthetic.xml", "applicationVersion":"FIX.Latest", "extensionPack":0, "customVersion":"synthetic"}),
        ] {
            let spec: DictionarySpec = serde_json::from_value(valid.clone()).unwrap();
            let serialized = serde_json::to_value(&spec).unwrap();
            assert_eq!(
                serde_json::from_value::<DictionarySpec>(serialized).unwrap(),
                spec
            );
            assert_eq!(
                spec.extension_pack,
                valid
                    .get("extensionPack")
                    .map(|value| value.as_u64().unwrap() as u32)
            );
        }
        for invalid in [
            json!({"name":"bad", "path":"x.xml", "transport":"FIXT.1.1"}),
            json!({"name":"bad", "path":"x.xml", "application":"FIX.5.0"}),
            json!({"name":"bad", "transport":"FIXT.1.1", "extensionPack":0}),
            json!({"name":"bad", "application":"FIX.5.0"}),
            json!({"name":"bad", "path":"", "transport":"FIXT.1.1"}),
            json!({"name":"bad", "path":"x.xml", "applicationVersion":"9"}),
            json!({"name":"bad", "path":"x.xml", "customVersion":" "}),
            json!({"name":"bad", "path":"x.xml", "extensionPack":4294967296_u64}),
        ] {
            assert!(
                serde_json::from_value::<DictionarySpec>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn semantic_tokens_default_on_and_the_cap_matches_the_schema() {
        let cfg: Config = serde_json::from_value(json!({})).unwrap();
        assert!(cfg.semantic_tokens.enabled);
        assert_eq!(cfg.limits.semantic_tokens_full_lines, 2000);
        let off: Config =
            serde_json::from_value(json!({"semanticTokens": {"enabled": false}})).unwrap();
        assert!(!off.semantic_tokens.enabled);
    }

    /// The formats are read through the accessors, so an absent one is the
    /// schema default and a written one is taken whatever `enabled` says.
    #[test]
    fn a_label_carries_a_format_beside_its_switch() {
        let cfg = Config::default();
        assert_eq!(
            cfg.inlay_hints.tag_format(),
            crate::render::DEFAULT_LABEL_FORMAT
        );
        assert_eq!(
            cfg.inlay_hints.value_format(),
            crate::render::DEFAULT_LABEL_FORMAT
        );

        let written: Config = serde_json::from_value(json!({
            "inlayHints": {
                "tagNames": { "format": "[{}]" },
                "enumValues": { "enabled": false },
            }
        }))
        .expect("config");
        assert_eq!(written.inlay_hints.tag_format(), "[{}]");
        assert!(
            written.inlay_hints.tag_names(),
            "a format-only setting keeps the enabled default"
        );
        assert!(!written.inlay_hints.enum_values());
        assert_eq!(
            written.inlay_hints.value_format(),
            crate::render::DEFAULT_LABEL_FORMAT,
            "and a switch is not a format"
        );
    }

    #[test]
    fn defaults_are_the_schema_defaults() {
        let cfg: Config = serde_json::from_value(json!({})).unwrap();
        assert_eq!(cfg.default_appl_ver_id, None);
        assert!(cfg.dictionaries.is_empty());
        assert_eq!(cfg.limits.dictionary_bytes, 16 * 1024 * 1024);
        assert!(cfg.hover.enabled);
        assert!(cfg.inlay_hints.tag_names(), "on by default");
        assert!(cfg.inlay_hints.enum_values(), "on by default");
        assert!(cfg.inlay_hints.group_paths(), "on by default");
        assert_eq!(
            cfg.document_link.url,
            "https://www.onixs.biz/fix-dictionary/{version}/tagNum_{tag}.html"
        );
        assert_eq!(cfg.limits.range_lines, 2000);
        assert_eq!(cfg.limits.code_lens_lines, 20_000);
        assert!(cfg.message_summary.enabled);
        assert!(cfg.message_summary.details);
        assert!(cfg.message_summary.channels.code_lens);
        assert!(!cfg.message_summary.channels.inlay_hints);
        assert_eq!(
            cfg.message_summary.template,
            crate::render::DEFAULT_TEMPLATE
        );
    }

    #[test]
    fn a_client_can_flip_the_summary_channels() {
        let cfg: Config = serde_json::from_value(json!({
            "messageSummary": { "channels": { "codeLens": false, "inlayHints": true } }
        }))
        .unwrap();
        assert!(!cfg.message_summary.channels.code_lens);
        assert!(cfg.message_summary.channels.inlay_hints);
        assert!(cfg.message_summary.enabled, "the master gate is untouched");
    }

    #[test]
    fn a_partial_message_summary_keeps_the_other_defaults() {
        let cfg: Config =
            serde_json::from_value(json!({ "messageSummary": { "details": false } })).unwrap();
        assert!(cfg.message_summary.enabled);
        assert!(!cfg.message_summary.details);
        assert_eq!(
            cfg.message_summary.template,
            crate::render::DEFAULT_TEMPLATE
        );
    }

    #[test]
    fn dictionary_entries_parse() {
        let cfg: Config = serde_json::from_value(json!({
            "dictionaries": [
                { "path": "a.xml", "name": "a" },
                { "path": "b.xml", "name": "nine", "default": true }
            ],
            "limits": {
                "dictionaryBytes": 10, "rangeLines": 40, "codeLensLines": 5,
            }
        }))
        .unwrap();
        assert_eq!(cfg.dictionaries[0].path.as_deref(), Some("a.xml"));
        assert_eq!(cfg.dictionaries[1].name, "nine");
        // `default` is the key that says whether an entry answers for every
        // line of its version; absent, it does not — answering for a whole
        // version is chosen rather than fallen into.
        assert!(!cfg.dictionaries[0].default);
        assert!(cfg.dictionaries[1].default);
        assert_eq!(cfg.limits.dictionary_bytes, 10);
        assert_eq!(cfg.limits.range_lines, 40);
        assert_eq!(cfg.limits.code_lens_lines, 5);
        // Lifted out of `rest` by the stage that enforces each; the ones with
        // no stage yet stay raw.
        for lifted in [
            "rangeLines",
            "codeLensLines",
            "semanticTokensFullLines",
            "cachedLines",
        ] {
            assert!(!cfg.limits.rest.contains_key(lifted), "{lifted}");
        }
    }

    #[test]
    fn the_cache_bound_is_never_below_what_one_range_request_populates() {
        let cfg = Config::default();
        assert_eq!(cfg.limits.cached_lines, 4000);
        assert_eq!(cfg.limits.cache_bound(), 4000);

        let low: Config = serde_json::from_value(json!({
            "limits": { "cachedLines": 10, "rangeLines": 50 }
        }))
        .unwrap();
        assert_eq!(low.limits.cached_lines, 10, "as written");
        assert_eq!(low.limits.cache_bound(), 50, "as applied");
    }

    /// Empty sections keep defaults; explicit false settings still take effect.
    #[test]
    fn an_empty_hint_section_keeps_its_own_default() {
        let empty: Config = serde_json::from_value(json!({
            "inlayHints": { "tagNames": {}, "enumValues": {}, "groupPaths": {} }
        }))
        .expect("config");
        assert!(empty.inlay_hints.tag_names());
        assert!(empty.inlay_hints.enum_values());
        assert!(empty.inlay_hints.group_paths());

        let flipped: Config = serde_json::from_value(json!({
            "inlayHints": {
                "tagNames": { "enabled": false },
                "groupPaths": { "enabled": false },
            }
        }))
        .expect("config");
        assert!(!flipped.inlay_hints.tag_names());
        assert!(!flipped.inlay_hints.group_paths());
        assert!(flipped.inlay_hints.enum_values(), "untouched, still on");
    }

    /// The schema's default is on, and the manual `Default` has to agree with
    /// it — a derived one would say `false` and silence hover for everyone who
    /// never wrote the key.
    #[test]
    fn hover_is_on_unless_it_is_turned_off() {
        assert!(Hover::default().enabled);
        let off: Config =
            serde_json::from_value(json!({ "hover": { "enabled": false } })).expect("config");
        assert!(!off.hover.enabled);
        let untouched: Config = serde_json::from_value(json!({ "hover": {} })).expect("config");
        assert!(untouched.hover.enabled, "an empty section changes nothing");
    }

    #[test]
    fn override_sources_are_on_unless_turned_off() {
        let config: Config = serde_json::from_value(json!({
            "overrides": {
                "modeline": false,
            }
        }))
        .expect("config");
        assert!(!config.overrides.modeline);
        assert!(config.overrides.project_config);
    }

    /// The built-in ids; `Registry::from_config` is what turns these into
    /// rules, and this only checks the configuration reaches it intact.
    #[test]
    fn diagnostics_default_to_every_rule_at_its_own_severity() {
        let cfg: Config = serde_json::from_value(json!({})).unwrap();
        assert!(cfg.diagnostics.enabled);
        assert!(
            cfg.diagnostics.rules.is_empty(),
            "an absent id means the rule's own default, not an entry"
        );
        assert_eq!(cfg.limits.diagnostics_per_document, 10_000);
    }

    #[test]
    fn a_rule_can_be_regraded_without_touching_the_others() {
        let cfg: Config = serde_json::from_value(json!({
            "diagnostics": { "rules": { "integrity": { "severity": "warning" } } }
        }))
        .unwrap();
        assert_eq!(
            cfg.diagnostics.rules["integrity"].severity(),
            Some(Severity::Warning)
        );
        assert_eq!(cfg.diagnostics.rules["integrity"].enabled(), None);
        assert_eq!(cfg.diagnostics.rules.len(), 1);
    }

    #[test]
    fn a_rule_can_be_switched_off_either_way() {
        let cfg: Config = serde_json::from_value(json!({
            "diagnostics": { "rules": { "unknownVersion": false, "integrity": { "enabled": false } } }
        }))
        .unwrap();
        assert_eq!(
            cfg.diagnostics.rules["unknownVersion"].enabled(),
            Some(false)
        );
        assert_eq!(cfg.diagnostics.rules["integrity"].enabled(), Some(false));
    }

    #[test]
    fn an_empty_rule_entry_is_the_default() {
        let cfg: Config =
            serde_json::from_value(json!({ "diagnostics": { "rules": { "integrity": {} } } }))
                .unwrap();
        assert_eq!(cfg.diagnostics.rules["integrity"].enabled(), None);
        assert_eq!(cfg.diagnostics.rules["integrity"].severity(), None);
    }

    /// No rule reads an option yet, but dropping one would make Stage 5's
    /// `fix.describeOverrides` report a configuration the user did not write.
    #[test]
    fn an_option_no_rule_reads_is_kept_rather_than_dropped() {
        let cfg: Config = serde_json::from_value(json!({
            "diagnostics": { "rules": { "integrity": { "enabled": true, "autoCorrect": true } } }
        }))
        .unwrap();
        let entry = &cfg.diagnostics.rules["integrity"];
        assert_eq!(entry.enabled(), Some(true));
        assert_eq!(entry.options()["autoCorrect"], json!(true));
    }

    #[test]
    fn malformed_diagnostics_fail_the_whole_config() {
        assert!(serde_json::from_value::<Config>(
            json!({ "diagnostics": { "rules": { "integrity": 3 } } })
        )
        .is_err());
        assert!(serde_json::from_value::<Config>(json!({
            "diagnostics": { "rules": { "integrity": { "severity": "loud" } } }
        }))
        .is_err());
        assert!(serde_json::from_value::<Config>(json!({ "diagnostics": "off" })).is_err());
    }

    /// One format is read, so nothing is left for `mode` to choose; an entry still
    /// setting it is as wrong as any other unknown key.
    #[test]
    fn mode_is_not_a_dictionary_key() {
        let error = serde_json::from_value::<Config>(json!({
            "dictionaries": [{ "path": "venue.xml", "name": "venue", "mode": "quickfix" }]
        }))
        .unwrap_err();
        assert!(
            error.to_string().contains("unknown field `mode`"),
            "{error}"
        );
    }

    #[test]
    fn malformed_dictionaries_fail_the_whole_config() {
        assert!(serde_json::from_value::<Config>(json!({ "dictionaries": "nonsense" })).is_err());
        // An entry without `name` or `path`, or with a mistyped field.
        assert!(
            serde_json::from_value::<Config>(json!({ "dictionaries": [{ "path": "x" }] })).is_err()
        );
        assert!(
            serde_json::from_value::<Config>(json!({ "dictionaries": [{ "name": "x" }] })).is_err()
        );
        assert!(serde_json::from_value::<Config>(
            json!({ "dictionaries": [{ "path": "x", "name": "x", "default": "yes" }] })
        )
        .is_err());
    }
}
