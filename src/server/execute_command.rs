//! Decode selections, choose dictionaries, and inspect configuration or tag references.
//!
//! Decode returns one versioned result per range and source row. Empty rows
//! retain their identity; filtering and presentation belong to the client.
//! Ranges answer independently, so overlaps repeat fields. An empty range is
//! a cursor and selects the field under it. Decode populates no line cache.
//!
//! `fix.describeTag` is hover for a tag no line carries: `[tag]` or
//! `[tag, value]`, no document, so it works with none open. It names no version
//! either — the question is what the tag means, not whether some edition has
//! it — so the reference source answers: FIXT 1.1 for a transport field, else
//! the newest bundled edition that defines the tag. The tag's own lifespan says
//! when it arrived and when it left. Neither the
//! session selection nor `defaultApplVerID` is consulted; only `tagUrl` is, for
//! the reference link. The initial configuration must be ready before lookup.

use lsp_server::ResponseError;
use lsp_types::{
    ExecuteCommandParams, Range, TextDocumentIdentifier, Uri, VersionedTextDocumentIdentifier,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write;

use super::dispatch::{Dispatched, HandlerResult, Snapshot};
use super::pending_requests::{PendingRequest, REJECTED};
use super::ServerState;
use crate::codec::{ByteOffset, Field};
use crate::dictionary::{DictionaryChoice, Registry, Version};
use crate::document::{Column, LineNo, Offsets};
use crate::render::{decode, hover};

/// Public so `capabilities` advertises these strings rather than copies.
pub const DECODE: &str = "fix.decode";
pub const USE_DICTIONARY: &str = "fix.useDictionary";
pub const LIST_DICTIONARIES: &str = "fix.listDictionaries";
pub const DESCRIBE_OVERRIDES: &str = "fix.describeOverrides";
pub const DESCRIBE_TAG: &str = "fix.describeTag";

const DICTIONARY_RESULT_BYTES: usize = 1024 * 1024;

struct DecodeInput<'a> {
    uri: Uri,
    ranges: &'a [Value],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DecodeResult {
    text_document: VersionedTextDocumentIdentifier,
    results: Vec<RangeResult>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RangeResult {
    range_index: usize,
    lines: Vec<RowResult>,
}

#[derive(Serialize)]
struct RowResult {
    line: u32,
    text: String,
}

#[derive(Serialize)]
struct DictionaryDetails<'a> {
    name: Option<&'a str>,
    version: &'a str,
    source: &'a str,
    transport: TransportDetails<'a>,
    application: Option<ApplicationDetails<'a>>,
}

#[derive(Serialize)]
struct TransportDetails<'a> {
    version: Option<&'a str>,
    source: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApplicationDetails<'a> {
    version: &'a str,
    extension_pack: Option<u32>,
    custom_version: Option<&'a str>,
    source: &'a str,
}

impl<'a> From<&'a DictionaryChoice> for DictionaryDetails<'a> {
    fn from(choice: &'a DictionaryChoice) -> Self {
        Self {
            name: choice.name.as_deref(),
            version: choice.version(),
            source: &choice.source,
            transport: TransportDetails {
                version: choice.transport.version(),
                source: &choice.transport_source,
            },
            application: choice
                .application
                .as_ref()
                .map(|identity| ApplicationDetails {
                    version: &identity.version,
                    extension_pack: identity.extension_pack,
                    custom_version: identity.custom_version.as_deref(),
                    source: &choice.source,
                }),
        }
    }
}

pub(super) fn dictionary_details(choice: &DictionaryChoice) -> Value {
    json!(DictionaryDetails::from(choice))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListedDictionary<'a> {
    #[serde(flatten)]
    details: DictionaryDetails<'a>,
    label: DictionaryLabel<'a>,
    kind: &'static str,
    defined_in: Option<&'a str>,
}

struct DictionaryLabel<'a> {
    name: &'a str,
    kind: &'static str,
}

impl Serialize for DictionaryLabel<'_> {
    fn serialize<Serializer>(
        &self,
        serializer: Serializer,
    ) -> Result<Serializer::Ok, Serializer::Error>
    where
        Serializer: serde::Serializer,
    {
        serializer.collect_str(&format_args!("{} ({})", self.name, self.kind))
    }
}

impl<'a> ListedDictionary<'a> {
    fn new(choice: &'a DictionaryChoice, defined_in: Option<&'a str>) -> Self {
        let kind = if defined_in.is_some() {
            "global"
        } else {
            "bundled"
        };
        Self {
            details: DictionaryDetails::from(choice),
            label: DictionaryLabel {
                name: choice.name.as_deref().expect("named registry choice"),
                kind,
            },
            kind,
            defined_in,
        }
    }
}

fn dictionary_list(registry: &Registry, cap: usize) -> HandlerResult {
    let too_large = || {
        super::config_pull::request_failed(&format!(
            "{LIST_DICTIONARIES}: result exceeds {cap} bytes"
        ))
    };
    let mut budget = ResultBudget(cap);
    budget
        .write_all(br#"{"dictionaries":[]}"#)
        .map_err(|_| too_large())?;
    let mut choices = Vec::new();
    for (choice, defined_in) in registry.choices() {
        if !choices.is_empty() {
            budget.write_all(b",").map_err(|_| too_large())?;
        }
        serde_json::to_writer(&mut budget, &ListedDictionary::new(&choice, defined_in))
            .map_err(|_| too_large())?;
        choices.push((choice, defined_in));
    }
    choices[Version::ALL.len()..].sort_unstable_by(|left, right| left.0.name.cmp(&right.0.name));
    let dictionaries: Vec<_> = choices
        .iter()
        .map(|(choice, defined_in)| ListedDictionary::new(choice, *defined_in))
        .collect();
    Ok(json!({"dictionaries": dictionaries}))
}

impl ServerState {
    pub(super) fn prepare_configuration_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<super::prepared_request::PreparedRequest, ResponseError> {
        use super::prepared_request::PreparedRequest;
        let prepared = match params.command.as_str() {
            USE_DICTIONARY => {
                let [name] = params.arguments.as_deref().unwrap_or_default() else {
                    return Err(invalid("fix.useDictionary takes [dictionaryName]".into()));
                };
                let name = name
                    .as_str()
                    .filter(|name| !name.trim().is_empty())
                    .ok_or_else(|| invalid("dictionary name must be a nonempty string".into()))?;
                PreparedRequest::Dictionary(name.to_owned())
            }
            DESCRIBE_OVERRIDES => {
                let [uri] = params.arguments.as_deref().unwrap_or_default() else {
                    return Err(invalid("fix.describeOverrides takes [uri]".into()));
                };
                PreparedRequest::Overrides(
                    serde_json::from_value(uri.clone())
                        .map_err(|error| invalid(error.to_string()))?,
                )
            }
            DECODE => {
                let input = arguments(&params)?;
                let ranges = input.parsed_ranges()?;
                PreparedRequest::Decode {
                    uri: input.uri,
                    ranges,
                }
            }
            _ => PreparedRequest::Command(params),
        };
        Ok(prepared)
    }

    pub(super) fn execute_command_prepared(
        &mut self,
        mut params: ExecuteCommandParams,
    ) -> Dispatched {
        if params.command == DESCRIBE_OVERRIDES {
            return Dispatched::Now(self.describe_overrides(&params));
        }
        if params.command == USE_DICTIONARY {
            let [raw] = params.arguments.as_deref().unwrap_or_default() else {
                return Dispatched::Now(Err(invalid(
                    "fix.useDictionary takes [dictionaryName]".into(),
                )));
            };
            let Some(raw) = raw.as_str().filter(|raw| !raw.trim().is_empty()) else {
                return Dispatched::Now(Err(invalid(
                    "dictionary name must be a nonempty string".into(),
                )));
            };
            return Dispatched::Now(self.select_dictionary(raw.to_owned()));
        }
        if params.command == DESCRIBE_TAG {
            let (tag, _) = match describe_arguments(&params) {
                Ok(arguments) => arguments,
                Err(error) => return Dispatched::Now(Err(error)),
            };
            let value = params
                .arguments
                .as_mut()
                .and_then(|arguments| arguments.get_mut(1))
                .map(|value| {
                    let Value::String(value) = value.take() else {
                        unreachable!("validated string")
                    };
                    value
                });
            return self.base_command(PendingRequest::DescribeTag { tag, value });
        }
        if params.command == LIST_DICTIONARIES {
            if !params.arguments.as_deref().unwrap_or_default().is_empty() {
                return Dispatched::Now(Err(invalid(format!(
                    "{LIST_DICTIONARIES} takes no arguments"
                ))));
            }
            return self.base_command(PendingRequest::ListDictionaries);
        }
        if params.command != DECODE {
            return Dispatched::Now(Err(invalid(format!(
                "unknown command `{}`; this server offers `{DECODE}`, `{USE_DICTIONARY}`, \
                 `{LIST_DICTIONARIES}`, `{DESCRIBE_OVERRIDES}` and `{DESCRIBE_TAG}`",
                params.command
            ))));
        }
        let input = match arguments(&params) {
            Ok(input) => input,
            Err(e) => return Dispatched::Now(Err(e)),
        };

        let ranges = match input.parsed_ranges() {
            Ok(ranges) => ranges,
            Err(error) => return Dispatched::Now(Err(error)),
        };
        self.decode_prepared(input.uri, ranges)
    }

    pub(super) fn decode_prepared(&mut self, uri: Uri, ranges: Vec<Range>) -> Dispatched {
        let Some(snapshot) = self.snapshot_for(&uri) else {
            return Dispatched::Now(Err(ResponseError {
                code: lsp_server::ErrorCode::RequestFailed as i32,
                message: format!("{}: no open document at {}", DECODE, uri.as_str()),
                data: None,
            }));
        };

        let limits = &self
            .effective_config(&uri)
            .expect("configured snapshot")
            .limits;
        let cap = limits.range_lines;
        if ranges.len() > cap {
            return Dispatched::Now(Err(selection_too_large(cap)));
        }
        if ranges
            .iter()
            .any(|range| range.start.line >= snapshot.line_count())
        {
            return Dispatched::Now(Err(invalid(format!(
                "{DECODE}: range starts beyond the document"
            ))));
        }
        // Counted against the document rather than against what was asked:
        // "to the end of the file" is a sentinel line number in some clients,
        // and refusing a three-line selection for naming `u32::MAX` would be a
        // bound on the arithmetic instead of on the work.
        let last = snapshot.line_count().saturating_sub(1);
        let fits = ranges.iter().try_fold(cap, |remaining, range| {
            remaining.checked_sub(spanned_lines(range, last))
        });
        if fits.is_none() {
            // Refused rather than truncated, as a code action is: the answer
            // goes into a register, and half a selection is worse there than
            // being told the selection is too big.
            return Dispatched::Now(Err(selection_too_large(cap)));
        }
        let result_limit = limits.decode_result_bytes;
        Dispatched::Task(
            snapshot,
            Box::new(move |snapshot, _| answer(&uri, &ranges, snapshot, result_limit)),
        )
    }

    fn describe_overrides(&self, params: &ExecuteCommandParams) -> HandlerResult {
        let [uri] = params.arguments.as_deref().unwrap_or_default() else {
            return Err(invalid("fix.describeOverrides takes [uri]".into()));
        };
        let uri: Uri =
            serde_json::from_value(uri.clone()).map_err(|error| invalid(error.to_string()))?;
        self.describe_overrides_prepared(uri)
    }

    pub(super) fn describe_overrides_prepared(&self, uri: Uri) -> HandlerResult {
        let Some(document) = self.config.documents.get(&uri) else {
            return Err(super::config_pull::request_failed(
                "document is not open or its configuration is not ready",
            ));
        };
        let sources: serde_json::Map<String, Value> = document
            .resolved
            .sources
            .iter()
            .map(|(key, origin)| (key.clone(), self.public_origin(&uri, origin)))
            .collect();
        let warnings: Vec<Value> = document
            .resolved
            .warnings
            .iter()
            .map(|warning| {
                json!({
                    "key": warning.key,
                    "message": warning.message,
                    "severity": warning.severity,
                    "source": self.public_origin(&uri, &warning.origin),
                })
            })
            .collect();
        let dictionaries: Vec<Value> = document
            .project
            .registry
            .definitions()
            .iter()
            .map(|definition| {
                let mut details = dictionary_details(&definition.choice);
                details["path"] = json!(definition.choice.source);
                details["source"] = json!(definition.source);
                details
            })
            .collect();
        Ok(
            json!({"effective":document.resolved.values,"sources":sources,"warnings":warnings,"dictionaries":dictionaries}),
        )
    }

    fn base_command(&self, command: PendingRequest) -> Dispatched {
        if self.config.rejected {
            return Dispatched::Now(Err(super::config_pull::request_failed(REJECTED)));
        }
        if !self.base_configuration_ready() {
            return Dispatched::WaitingForConfiguration(command);
        }
        Dispatched::Now(self.run_base_command(command))
    }

    pub(super) fn run_base_command(&self, command: PendingRequest) -> HandlerResult {
        match command {
            PendingRequest::Request(_) => {
                unreachable!("prepared requests execute through dispatch")
            }
            #[cfg(test)]
            PendingRequest::Panic => panic!("deferred command panicked"),
            PendingRequest::ListDictionaries => {
                dictionary_list(&self.registry, DICTIONARY_RESULT_BYTES)
            }
            PendingRequest::DescribeTag { tag, value } => {
                match hover::describe(tag, value.as_deref(), &self.render) {
                    Some(text) => Ok(json!(text)),
                    None => Err(super::config_pull::request_failed(&format!(
                        "{DESCRIBE_TAG}: no bundled dictionary defines tag {tag}"
                    ))),
                }
            }
        }
    }

    fn public_origin(&self, uri: &Uri, origin: &crate::overrides::Origin) -> Value {
        let mut result = json!({"layer":origin.layer,"source":origin.source});
        if origin.layer == crate::overrides::SourceKind::Modeline {
            let line = self
                .docs
                .get(uri)
                .and_then(|document| document.lines.get(origin.line));
            let range =
                super::origin_range(origin, line.map(std::sync::Arc::as_ref), self.encoding);
            result["range"] = json!(range);
        }
        result
    }
}

fn invalid(message: String) -> ResponseError {
    ResponseError {
        code: lsp_server::ErrorCode::InvalidParams as i32,
        message,
        data: None,
    }
}

fn selection_too_large(cap: usize) -> ResponseError {
    super::config_pull::request_failed(&format!(
        "selection is over {cap} lines; select fewer, or raise fix.limits.rangeLines"
    ))
}

fn result_too_large(cap: usize) -> ResponseError {
    super::config_pull::request_failed(&format!(
        "{DECODE}: result is over {cap} bytes; select less, or raise fix.limits.decodeResultBytes"
    ))
}

/// Parse the envelope before allocating its typed range vector.
fn arguments(params: &ExecuteCommandParams) -> Result<DecodeInput<'_>, ResponseError> {
    let shape = || {
        invalid(format!(
            "{DECODE} takes [{{textDocument: {{uri}}, ranges}}]"
        ))
    };
    let [argument] = params.arguments.as_deref().unwrap_or_default() else {
        return Err(shape());
    };
    let document: TextDocumentIdentifier =
        serde_json::from_value(argument["textDocument"].clone()).map_err(|_| shape())?;
    let ranges = argument["ranges"]
        .as_array()
        .filter(|ranges| !ranges.is_empty())
        .ok_or_else(shape)?;
    Ok(DecodeInput {
        uri: document.uri,
        ranges,
    })
}

impl DecodeInput<'_> {
    fn parsed_ranges(&self) -> Result<Vec<Range>, ResponseError> {
        self.ranges
            .iter()
            .map(|range| Range::deserialize(range).map(ordered))
            .collect::<Result<_, _>>()
            .map_err(|_| invalid(format!("{DECODE}: invalid range positions")))
    }
}

/// `[tag]` or `[tag, value]`, typed: a positive tag number, an optional string.
fn describe_arguments(params: &ExecuteCommandParams) -> Result<(u32, Option<&str>), ResponseError> {
    let shape = || invalid(format!("{DESCRIBE_TAG} takes [tag] or [tag, value]"));
    let args = params.arguments.as_deref().unwrap_or_default();
    let (tag, value) = match args {
        [tag] => (tag, None),
        [tag, value] => (tag, Some(value.as_str().ok_or_else(shape)?)),
        _ => return Err(shape()),
    };
    let tag = tag
        .as_u64()
        .and_then(|tag| u32::try_from(tag).ok())
        .filter(|tag| *tag > 0)
        .ok_or_else(shape)?;
    Ok((tag, value))
}

/// LSP requires a range's start to precede its end, but nothing on the wire
/// enforces it and a client building one from an anchor and a cursor can send
/// either order. **Normalised rather than refused**, because a reversed range
/// has exactly one meaning and refusing it would fail a person who selected
/// upwards. Left as it arrived, the two halves disagree: reversed lines make
/// an empty loop and answer with nothing, while reversed columns invert the
/// window and answer with whatever field spans the gap.
fn ordered(range: Range) -> Range {
    if (range.start.line, range.start.character) <= (range.end.line, range.end.character) {
        range
    } else {
        Range {
            start: range.end,
            end: range.start,
        }
    }
}

/// How many lines of the document a range covers. Counted per range rather
/// than over their union: a blockwise selection is one range a line, so the
/// two readings agree where it matters.
fn spanned_lines(range: &Range, last: u32) -> usize {
    let first = range.start.line.min(last);
    let end = range.end.line.min(last);
    (end - first) as usize + 1
}

fn answer(uri: &Uri, ranges: &[Range], snapshot: &Snapshot, cap: usize) -> HandlerResult {
    let last = snapshot.line_count().saturating_sub(1);
    let mut result = DecodeResult {
        text_document: VersionedTextDocumentIdentifier {
            version: snapshot.version,
            text_document_identifier: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
        },
        results: ranges
            .iter()
            .enumerate()
            .map(|(range_index, range)| RangeResult {
                range_index,
                lines: (range.start.line..=range.end.line.min(last))
                    .map(|line| RowResult {
                        line,
                        text: String::new(),
                    })
                    .collect(),
            })
            .collect(),
    };
    let mut budget = ResultBudget(cap);
    serde_json::to_writer(&mut budget, &result).map_err(|_| result_too_large(cap))?;
    for (range, answer) in ranges.iter().zip(&mut result.results) {
        for row in &mut answer.lines {
            row.text = decoded_line(range, LineNo::new(row.line), snapshot, &mut budget.0)
                .map_err(|_| result_too_large(cap))?
                .unwrap_or_default();
        }
    }
    serde_json::to_writer(ResultBudget(cap), &result).map_err(|_| result_too_large(cap))?;
    Ok(json!(result))
}

struct ResultBudget(usize);

impl std::io::Write for ResultBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("result byte limit"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// No selected field leaves this source row's result text empty.
fn decoded_line(
    range: &Range,
    number: LineNo,
    snapshot: &Snapshot,
    remaining: &mut usize,
) -> Result<Option<String>, std::fmt::Error> {
    let Some(line) = snapshot.line(number) else {
        return Ok(None);
    };
    // `analyze` and drop: see the module doc.
    let analysis = crate::ctx::analyze(&line.bytes, &snapshot.ctx);
    let Some(message) = analysis.message() else {
        return Ok(None);
    };
    let mut render = |fields: &mut dyn Iterator<Item = &Field>| {
        decode::bounded_line(message, &line.bytes, fields, &snapshot.render, remaining)
    };

    let first = range.start.line == number.raw();
    let last = range.end.line == number.raw();
    let start_column = if first { range.start.character } else { 0 };
    // Past the line's end, which `position_to_byte` clamps — "to the end of
    // this line" needs no separate spelling.
    let end_column = if last { range.end.character } else { u32::MAX };
    let offsets = Offsets::of(
        line,
        [Column::new(start_column), Column::new(end_column)],
        snapshot.encoding,
    );
    let start = offsets.at(Column::new(start_column));
    let end = offsets.at(Column::new(end_column));

    // An empty range is a cursor: the field under it, and nothing when it sits
    // on a separator.
    if range.start == range.end {
        let Some(field) = ByteOffset::from_len(start).and_then(|at| message.field_at(at)) else {
            return Ok(None);
        };
        return render(&mut std::iter::once(field)).map(Some);
    }

    let mut selected = message
        .fields
        .iter()
        .filter(|field| overlaps(field, start, end))
        .peekable();
    if selected.peek().is_none() {
        return Ok(None);
    }
    render(&mut selected).map(Some)
}

/// Half-open both ends, which is what makes a selection ending at column 0 of
/// the next line reach none of that line's fields.
fn overlaps(field: &Field, start: usize, end: usize) -> bool {
    field.tag_start.index() < end && field.value_end().index() > start
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::PositionEncoding;
    #[cfg(not(feature = "bench-no-line-cache"))]
    use crate::server::testing::cached;
    use crate::server::testing::{answered, state_with, HEARTBEAT};
    use lsp_types::Uri;

    const THREE: &str = "8=FIX.4.4|35=0|34=1|10=171|\n\
8=FIX.4.4|35=1|34=2|10=172|\n\
8=FIX.4.4|35=2|34=3|10=173|";

    fn listing_registry(names: &[String], origin: &str) -> Registry {
        use crate::config::{DictionarySpec, Limits};
        use crate::dictionary::{BuildContext, DictionaryLayer, DictionaryStore};
        let specs: Vec<_> = names
            .iter()
            .map(|name| DictionarySpec {
                name: name.clone(),
                transport: Some("FIXT.1.1".into()),
                application: Some("FIX.5.0SP2".into()),
                extension_pack: Some(0),
                custom_version: Some("synthetic-\"β\"\n".into()),
                ..Default::default()
            })
            .collect();
        let (registry, events) = DictionaryStore::default()
            .build_registry(
                &[DictionaryLayer {
                    specs: &specs,
                    source: origin,
                    root_dir: None,
                }],
                &BuildContext {
                    root_dir: None,
                    home_dir: None,
                    limits: &Limits::default(),
                },
            )
            .unwrap();
        assert!(events.is_empty(), "{events:?}");
        registry
    }

    #[test]
    fn dictionary_listing_budget_counts_the_complete_escaped_result() {
        let cases = [
            (Vec::new(), "client"),
            (
                vec![" quoted-\"β\"\\\n\t\u{1} ".into()],
                "file:///synthetic/\"β\"\\\n",
            ),
            ((0..50).map(|i| format!("alias-{i}")).collect(), "client"),
        ];
        for (names, origin) in cases {
            let registry = listing_registry(&names, origin);
            let result = dictionary_list(&registry, usize::MAX).unwrap();
            let bytes = serde_json::to_vec(&result).unwrap().len();
            assert_eq!(dictionary_list(&registry, bytes).unwrap(), result);
            assert_eq!(dictionary_list(&registry, bytes + 1).unwrap(), result);
            for cap in [0, bytes - 1] {
                let error = dictionary_list(&registry, cap).unwrap_err();
                assert_eq!(error.code, lsp_server::ErrorCode::RequestFailed as i32);
                assert_eq!(
                    error.message,
                    format!("fix.listDictionaries: result exceeds {cap} bytes")
                );
            }
            assert_eq!(
                result["dictionaries"].as_array().unwrap().len(),
                9 + names.len()
            );
            if let Some(name) = names.first() {
                let entry = result["dictionaries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|entry| entry["name"] == *name)
                    .unwrap();
                assert_eq!(entry["label"], format!("{name} (global)"));
                assert_eq!(entry["definedIn"], origin);
                assert_eq!(entry["application"]["extensionPack"], 0);
                assert_eq!(entry["application"]["customVersion"], "synthetic-\"β\"\n");
            }
        }
    }

    #[test]
    fn dictionary_listing_refuses_an_oversized_name_or_origin_without_skipping_it() {
        let base = dictionary_list(&Registry::bundled_only(), usize::MAX).unwrap();
        let cap = serde_json::to_vec(&base).unwrap().len() + 512;
        for (names, origin) in [
            (vec!["z".repeat(cap), "a".into()], "client".into()),
            (vec!["a".into()], "source".repeat(cap)),
        ] {
            let registry = listing_registry(&names, &origin);
            assert!(dictionary_list(&registry, cap).is_err());
        }
    }

    fn selected_text(result: Value) -> Value {
        let text = result["results"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|range| range["lines"].as_array().unwrap())
            .filter_map(|line| line["text"].as_str().filter(|text| !text.is_empty()))
            .collect::<Vec<_>>()
            .join("\n");
        json!(text)
    }

    fn ask(state: &mut ServerState, uri: &Uri, ranges: Value) -> Value {
        let request = lsp_server::Request::new(
            1.into(),
            super::super::method::EXECUTE_COMMAND.into(),
            json!({
                "command": DECODE,
                "arguments": [{"textDocument": {"uri": uri}, "ranges": ranges}],
            }),
        );
        selected_text(answered(
            state
                .prepare_request(&request)
                .unwrap()
                .expect("prepared decode"),
        ))
    }

    fn range(sl: u32, sc: u32, el: u32, ec: u32) -> Value {
        json!({
            "start": { "line": sl, "character": sc },
            "end": { "line": el, "character": ec },
        })
    }

    fn refused(state: &mut ServerState, uri: &Uri, ranges: Value) -> ResponseError {
        let request = lsp_server::Request::new(
            1.into(),
            super::super::method::EXECUTE_COMMAND.into(),
            json!({
                "command": DECODE,
                "arguments": [{"textDocument": {"uri": uri.as_str()}, "ranges": ranges}],
            }),
        );
        match state.prepare_request(&request) {
            Err(error) | Ok(Some(Dispatched::Now(Err(error)))) => error,
            _ => panic!("refused"),
        }
    }

    /// The first line from its start column, the last to its end column, the
    /// ones between whole — a range read the way LSP means one.
    #[test]
    fn a_range_takes_part_of_its_first_line_and_part_of_its_last() {
        let (mut state, uri) = state_with(THREE);
        // From `35=0` on line 0 to just past `35=1` on line 2.
        let answer = ask(&mut state, &uri, json!([range(0, 10, 2, 14)]));
        assert_eq!(
            answer,
            json!(
                "35(MsgType)=0(Heartbeat)|34(MsgSeqNum)=1|10(CheckSum)=171\n\
8(BeginString)=FIX.4.4|35(MsgType)=1(TestRequest)|34(MsgSeqNum)=2|10(CheckSum)=172\n\
8(BeginString)=FIX.4.4|35(MsgType)=2(ResendRequest)"
            )
        );
    }

    /// A linewise selection, as a client sends one: to column 0 of the line
    /// after. The exclusive end falls out of the window being empty there.
    #[test]
    fn a_range_ending_at_column_zero_selects_no_fields_on_that_line() {
        let (mut state, uri) = state_with(THREE);
        let answer = ask(&mut state, &uri, json!([range(0, 0, 1, 0)]));
        let text = answer.as_str().expect("a string");
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("0(Heartbeat)"), "{text}");
    }

    /// A block is ranges, one per line — the shape two positions cannot carry.
    #[test]
    fn a_block_can_select_fields_from_each_line() {
        let (mut state, uri) = state_with(THREE);
        let answer = ask(
            &mut state,
            &uri,
            json!([
                range(0, 10, 0, 14),
                range(1, 10, 1, 14),
                range(2, 10, 2, 14)
            ]),
        );
        assert_eq!(
            answer,
            json!(
                "35(MsgType)=0(Heartbeat)\n\
35(MsgType)=1(TestRequest)\n\
35(MsgType)=2(ResendRequest)"
            )
        );
    }

    #[test]
    fn an_empty_range_is_a_cursor_and_answers_with_one_field() {
        let (mut state, uri) = state_with(THREE);
        assert_eq!(
            ask(&mut state, &uri, json!([range(1, 11, 1, 11)])),
            json!("35(MsgType)=1(TestRequest)")
        );
        // A separator contributes no decoded field text.
        assert_eq!(ask(&mut state, &uri, json!([range(1, 9, 1, 9)])), json!(""));
    }

    /// Columns are the client's units, so a non-ASCII line must not be read as
    /// bytes. **Asserted as a difference**: the emoji is two utf-16 units and
    /// four bytes, so the same column pair straddles a field boundary under one
    /// reading and not the other. An equality against one encoding alone would
    /// hold with the conversion deleted, which is what this test used to do.
    #[test]
    fn a_column_is_converted_before_it_selects_anything() {
        let text = format!("{HEARTBEAT}\n8=FIX.4.4|35=0|58=🙂ok|10=1|");
        let (mut state, uri) = state_with(&text);
        let ranges = vec![serde_json::from_value(range(1, 22, 1, 30)).expect("a range")];

        let mut snapshot = state.snapshot_for(&uri).expect("open");
        snapshot.encoding = PositionEncoding::Utf8;
        let as_bytes =
            selected_text(answer(&uri, &ranges, &snapshot, usize::MAX).expect("answered"));
        snapshot.encoding = PositionEncoding::Utf16;
        let as_units =
            selected_text(answer(&uri, &ranges, &snapshot, usize::MAX).expect("answered"));

        assert_eq!(as_bytes, json!("58(Text)=🙂ok|10(CheckSum)=1"));
        assert_eq!(as_units, json!("10(CheckSum)=1"));
        assert_ne!(as_bytes, as_units, "the conversion is what decides");
    }

    /// A client that builds a range from an anchor and a cursor can send it
    /// either way round. Both orders answer the same rather than one of them
    /// answering with nothing and the other with whatever spans the gap.
    #[test]
    fn a_reversed_range_answers_as_the_selection_it_describes() {
        let (mut state, uri) = state_with(THREE);
        let forward = ask(&mut state, &uri, json!([range(0, 10, 1, 14)]));
        let backward = ask(&mut state, &uri, json!([range(1, 14, 0, 10)]));
        assert_eq!(forward, backward);
        assert!(forward
            .as_str()
            .expect("a string")
            .contains("1(TestRequest)"));

        // Reversed on one line, where the window would otherwise invert and
        // select whatever field spanned it.
        let one = state_with("8=FIX.4.4|35=0|58=aaaaaaaaaaaaaaaaaaaaaaaa|10=1|");
        let (mut wide, wide_uri) = one;
        assert_eq!(
            ask(&mut wide, &wide_uri, json!([range(0, 35, 0, 20)])),
            ask(&mut wide, &wide_uri, json!([range(0, 20, 0, 35)]))
        );
    }

    /// A sentinel end line is how some clients say "to the end of the file";
    /// the bound counts what the document has, not what the client named.
    #[test]
    fn a_range_past_the_end_of_the_document_is_counted_as_the_document() {
        let (mut state, uri) = state_with(THREE);
        state.config.base.limits.range_lines = 3;
        super::super::testing::reconfigure(&mut state);
        let answer = ask(&mut state, &uri, json!([range(0, 0, u32::MAX, 0)]));
        assert_eq!(answer.as_str().expect("a string").lines().count(), 3);
    }

    /// The bound counts lines across every range, so a block cannot slip past
    /// it by being many small ranges.
    #[test]
    fn a_selection_over_the_bound_is_refused_and_says_which_setting() {
        let (mut state, uri) = state_with(&crate::server::testing::heartbeats(10));
        state.config.base.limits.range_lines = 2;
        super::super::testing::reconfigure(&mut state);
        let error = refused(
            &mut state,
            &uri,
            json!([range(0, 0, 0, 5), range(1, 0, 1, 5), range(2, 0, 2, 5)]),
        );
        assert!(error.message.contains("rangeLines"), "{}", error.message);
    }

    #[test]
    fn an_unknown_command_and_a_malformed_argument_list_are_invalid_params() {
        let (mut state, uri) = state_with(HEARTBEAT);
        let unknown = match state.execute_command_prepared(
            serde_json::from_value(json!({
                "command": "fix.somethingElse",
                "arguments": [uri.as_str(), [range(0, 0, 0, 1)]],
            }))
            .unwrap(),
        ) {
            Dispatched::Now(Err(e)) => e,
            _ => panic!("refused"),
        };
        assert_eq!(unknown.code, lsp_server::ErrorCode::InvalidParams as i32);
        assert!(unknown.message.contains("fix.somethingElse"), "{unknown:?}");
        for offered in [
            DECODE,
            USE_DICTIONARY,
            LIST_DICTIONARIES,
            DESCRIBE_OVERRIDES,
            DESCRIBE_TAG,
        ] {
            assert!(unknown.message.contains(offered), "{unknown:?}");
        }

        let short = match state.execute_command_prepared(
            serde_json::from_value(json!({
                "command": DECODE,
                "arguments": [uri.as_str()],
            }))
            .unwrap(),
        ) {
            Dispatched::Now(Err(e)) => e,
            _ => panic!("refused"),
        };
        assert_eq!(short.code, lsp_server::ErrorCode::InvalidParams as i32);

        let empty = refused(&mut state, &uri, json!([]));
        assert_eq!(empty.code, lsp_server::ErrorCode::InvalidParams as i32);
    }

    fn describe(state: &mut ServerState, arguments: Value) -> Result<Value, ResponseError> {
        match state.execute_command_prepared(
            serde_json::from_value(json!({
                "command": DESCRIBE_TAG,
                "arguments": arguments,
            }))
            .unwrap(),
        ) {
            Dispatched::Now(result) => result,
            Dispatched::Task(..)
            | Dispatched::Lazy(..)
            | Dispatched::WaitingForConfiguration(..) => {
                panic!("a description needs no document")
            }
        }
    }

    fn described(state: &mut ServerState, arguments: Value) -> String {
        describe(state, arguments)
            .expect("described")
            .as_str()
            .expect("a string")
            .to_owned()
    }

    fn refused_description(state: &mut ServerState, arguments: Value) -> ResponseError {
        describe(state, arguments).expect_err("refused")
    }

    /// A description is about a tag, so the configured version has no say in
    /// it: the newest edition that defines the tag answers, and the answer
    /// names no version it was not asked about.
    #[test]
    fn a_tag_is_described_without_consulting_the_configured_version() {
        let (mut state, _) = state_with(HEARTBEAT);
        let plain = described(&mut state, json!([49]));
        assert!(plain.starts_with("**SenderCompID** (49)"), "{plain}");
        let later = described(&mut state, json!([1328]));
        assert!(later.starts_with("**RejectText** (1328)"), "{later}");
        assert!(later.contains("*Introduced in FIX.5.0SP1*"), "{later}");
        assert!(!later.contains("Not in"), "{later}");
    }

    #[test]
    fn a_value_argument_adds_the_value_paragraph() {
        let (mut state, _) = state_with(HEARTBEAT);
        assert!(described(&mut state, json!([54, "1"])).contains("Value: `1` — **Buy**"));
    }

    #[test]
    fn a_tag_no_source_defines_is_refused_by_name() {
        let (mut state, _) = state_with(HEARTBEAT);
        let error = refused_description(&mut state, json!([9999]));
        assert_eq!(error.code, lsp_server::ErrorCode::RequestFailed as i32);
        assert_eq!(
            error.message,
            "fix.describeTag: no bundled dictionary defines tag 9999"
        );
    }

    #[test]
    fn a_malformed_description_request_is_invalid_params() {
        let (mut state, _) = state_with(HEARTBEAT);
        for arguments in [
            json!(["1328"]),
            json!([0]),
            json!([]),
            json!([1328, 5]),
            json!([1328, "1", 2]),
        ] {
            let error = refused_description(&mut state, arguments.clone());
            assert_eq!(
                error.code,
                lsp_server::ErrorCode::InvalidParams as i32,
                "{arguments}: {}",
                error.message
            );
            assert!(error.message.contains("[tag]"), "{}", error.message);
        }
    }

    /// Wait rather than render with an uncommitted reference-link template.
    #[test]
    fn a_description_before_the_first_commit_is_deferred() {
        let (sender, _receiver) = crossbeam_channel::unbounded::<lsp_server::Message>();
        let mut state = ServerState::new(sender);
        assert!(matches!(
            state.execute_command_prepared(
                serde_json::from_value(json!({"command": DESCRIBE_TAG, "arguments": [49]}))
                    .unwrap()
            ),
            Dispatched::WaitingForConfiguration(_)
        ));
    }

    /// A rejected first configuration never commits, so "retry later" would
    /// be wrong forever: the rejection is what is reported, and a valid update
    /// after it answers.
    #[test]
    fn a_rejected_first_configuration_is_reported_as_such_until_a_valid_one_lands() {
        let (sender, _receiver) = crossbeam_channel::unbounded::<lsp_server::Message>();
        let mut state = ServerState::new(sender);
        state.config.rejected = true;
        let error = refused_description(&mut state, json!([49]));
        assert!(
            error.message.contains("send a valid configuration update"),
            "{}",
            error.message
        );
        super::super::testing::reconfigure(&mut state);
        assert!(!state.config.rejected);
        assert!(described(&mut state, json!([49])).starts_with("**SenderCompID** (49)"));
    }

    /// A committed configuration is not the whole story: while the client has
    /// been asked for its settings and has not answered, the link template is
    /// still the one without that layer.
    #[test]
    fn a_description_while_a_pull_is_unanswered_is_deferred() {
        let (mut state, _) = state_with(HEARTBEAT);
        state.client.configuration = true;
        state.pull_base_config();

        assert!(matches!(
            state.execute_command_prepared(
                serde_json::from_value(json!({"command": DESCRIBE_TAG, "arguments": [49]}))
                    .unwrap()
            ),
            Dispatched::WaitingForConfiguration(_)
        ));

        let generation = state.config.global_generation;
        state.on_base_reply(generation, Ok(json!([null])));
        super::super::testing::settle(&mut state);
        assert!(described(&mut state, json!([49])).starts_with("**SenderCompID** (49)"));
    }

    /// Neither a session selection nor `defaultApplVerID` reaches a description.
    /// SenderCompID(49) is the tag that can tell: every edition defines it, so
    /// a configured start would answer from that edition and link to its pages,
    /// where the reference source is FIXT 1.1. A later tag like 1328 could not
    /// discriminate — it reaches SP2 from either start.
    #[test]
    fn a_session_selection_and_a_default_version_contribute_nothing() {
        use crate::dictionary::parse::fixtures::{field, quickfix};
        use crate::dictionary::{Dict, DictionaryChoice};
        let (mut state, _) = state_with(HEARTBEAT);
        let plain = described(&mut state, json!([49]));
        assert!(
            plain.ends_with("/fix-dictionary/fixt1.1/tagNum_49.html)"),
            "{plain}"
        );

        state.config.base.default_appl_ver_id = Some("FIX.4.2".into());
        super::super::testing::reconfigure(&mut state);
        assert_eq!(described(&mut state, json!([49])), plain);

        let dict = Dict::Owned(std::sync::Arc::new(quickfix(
            "FIX.4.2",
            &field(9999, "DeskOnly"),
        )));
        state.config.session_choice = Some(DictionaryChoice {
            transport: dict.clone(),
            transport_source: "desk.xml".into(),
            application: Some(crate::dictionary::ApplicationIdentity::release("FIX.4.2")),
            dict,
            source: "desk.xml".into(),
            identity: 7,
            name: Some("desk".into()),
        });
        assert_eq!(described(&mut state, json!([49])), plain);
        // And a dictionary only the desk has is still no dictionary to a
        // description: bundled editions are all it reads.
        let error = refused_description(&mut state, json!([9999]));
        assert_eq!(
            error.message,
            "fix.describeTag: no bundled dictionary defines tag 9999"
        );
    }

    #[test]
    #[cfg(not(feature = "bench-no-line-cache"))]
    fn range_decode_caches_nothing_on_success_or_output_refusal() {
        let (mut state, uri) = state_with(THREE);
        let snapshot = state.snapshot_for(&uri).expect("open");
        let ranges = vec![Range {
            start: lsp_types::Position {
                line: 0,
                character: 0,
            },
            end: lsp_types::Position {
                line: 2,
                character: u32::MAX,
            },
        }];
        let response = answer(&uri, &ranges, &snapshot, usize::MAX).expect("answered");
        let size = serde_json::to_vec(&response).unwrap().len();
        assert!(cached(&mut state, &uri).iter().all(|present| !present));
        let error = answer(&uri, &ranges, &snapshot, size - 1).expect_err("over budget");
        assert_eq!(error.code, lsp_server::ErrorCode::RequestFailed as i32);
        assert!(cached(&mut state, &uri).iter().all(|present| !present));
    }
}
