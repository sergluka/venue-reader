//! `workspace/executeCommand` — `fix.decode`, `fix.useDictionary`,
//! `fix.describeOverrides` and `fix.describeTag`.
//!
//! `fix.decode` first. The decoded text has no home among the standard requests: hover answers one
//! position, a code lens one line, and nothing takes a selection and gives back
//! a string. A command does exactly that, and what the client does with the
//! string — a register, a clipboard, a scratch buffer — is the client's.
//!
//! **The argument is a list of ranges and there is no mode.** An LSP `Range` is
//! two positions in reading order, which *is* a characterwise selection; a
//! linewise one is the same thing from column 0 to the end of the last line.
//! A rectangle is the one shape two positions cannot carry, so a blockwise
//! selection is composed by the client as one range per line — as is a
//! multi-cursor selection, which some editors have natively. Ranges answer
//! independently and an overlap duplicates: merging them is a decision about
//! what the user selected, and the client is the one that knows.
//!
//! An empty range is a cursor, and answers with the field under it.
//!
//! Not a cache populator. The range is bounded, so caching would be permitted,
//! but a yank is a one-shot gesture and nothing re-reads these lines.
//!
//! `fix.describeTag` is hover for a tag no line carries: `[tag]` or
//! `[tag, value]`, no document, so it works with none open. The configured
//! version is where the search starts, and a tag it lacks is answered from
//! the newest bundled version that has it — the question is what the tag
//! means, not whether this log may carry it. Bundled tables only: a custom
//! session selection lends its version, never its definitions.

use lsp_server::ResponseError;
use lsp_types::{ExecuteCommandParams, Range, Uri};
use serde_json::{json, Value};

use super::dispatch::{Dispatched, HandlerResult, Notices, RequestTask, Snapshot};
use super::ServerState;
use crate::codec::{ByteOffset, Field};
use crate::dictionary::Version;
use crate::document::{Column, LineNo, Offsets};
use crate::render::{decode, hover};

/// Public so `capabilities` advertises these strings rather than copies.
pub const DECODE: &str = "fix.decode";
pub const USE_DICTIONARY: &str = "fix.useDictionary";
pub const DESCRIBE_OVERRIDES: &str = "fix.describeOverrides";
pub const DESCRIBE_TAG: &str = "fix.describeTag";

impl ServerState {
    pub(super) fn execute_command_task(&mut self, params: &Value) -> Dispatched {
        let params: ExecuteCommandParams =
            match super::params(super::method::EXECUTE_COMMAND, params) {
                Ok(params) => params,
                Err(e) => return Dispatched::Now(Err(e)),
            };
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
            return Dispatched::Now(self.describe_tag(&params));
        }
        if params.command != DECODE {
            return Dispatched::Now(Err(invalid(format!(
                "unknown command `{}`; this server offers `{DECODE}`, `{USE_DICTIONARY}`, \
                 `{DESCRIBE_OVERRIDES}` and `{DESCRIBE_TAG}`",
                params.command
            ))));
        }
        let (uri, ranges) = match arguments(&params) {
            Ok(pair) => pair,
            Err(e) => return Dispatched::Now(Err(e)),
        };
        if let Some(not_ready) = super::dispatch::configuration_not_ready(self, &uri) {
            return not_ready;
        }

        let Some(snapshot) = self.snapshot_for(&uri) else {
            // A command is invoked by a person, so say so rather than handing
            // back an empty string they would paste without noticing. Ahead of
            // the bound, or a selection past the end of a closed document is
            // reported as too big rather than as closed.
            return Dispatched::Now(Err(ResponseError {
                code: lsp_server::ErrorCode::RequestFailed as i32,
                message: format!("{DECODE}: no open document at {}", uri.as_str()),
                data: None,
            }));
        };

        let cap = self
            .effective_config(&uri)
            .expect("configured snapshot")
            .limits
            .range_lines;
        // Counted against the document rather than against what was asked:
        // "to the end of the file" is a sentinel line number in some clients,
        // and refusing a three-line selection for naming `u32::MAX` would be a
        // bound on the arithmetic instead of on the work.
        let last = snapshot.line_count().saturating_sub(1);
        let asked: usize = ranges.iter().map(|range| spanned_lines(range, last)).sum();
        if asked > cap {
            // Refused rather than truncated, as a code action is: the answer
            // goes into a register, and half a selection is worse there than
            // being told the selection is too big.
            return Dispatched::Now(Err(ResponseError {
                code: lsp_server::ErrorCode::RequestFailed as i32,
                message: format!(
                    "selection is over {cap} lines; select fewer, or raise \
                     fix.limits.rangeLines"
                ),
                data: None,
            }));
        }
        Dispatched::Task(snapshot, prepare(ranges))
    }

    fn describe_overrides(&self, params: &ExecuteCommandParams) -> HandlerResult {
        let [uri] = params.arguments.as_deref().unwrap_or_default() else {
            return Err(invalid("fix.describeOverrides takes [uri]".into()));
        };
        let uri: Uri =
            serde_json::from_value(uri.clone()).map_err(|error| invalid(error.to_string()))?;
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
        let dictionaries: Vec<Value> = document.project.registry.definitions().iter().map(|definition| json!({"name":definition.spec.name,"path":definition.choice.source,"version":definition.choice.version,"source":definition.source})).collect();
        Ok(
            json!({"effective":document.resolved.values,"sources":sources,"warnings":warnings,"dictionaries":dictionaries}),
        )
    }

    fn describe_tag(&self, params: &ExecuteCommandParams) -> HandlerResult {
        let (tag, value) = describe_arguments(params)?;
        // Rejection first: a rejected first pull never commits, and "retry
        // later" would then be wrong forever.
        if self.config.rejected {
            return Err(super::config_pull::request_failed(
                "configuration is not ready; send a valid configuration update",
            ));
        }
        if !self.config.committed {
            return Err(super::config_pull::request_failed(
                "configuration is not ready, retry later",
            ));
        }
        // The selection's version when it names a bundled one, else the
        // configured default; a custom file may declare a string no table has.
        let start = self
            .config
            .session_choice
            .as_ref()
            .and_then(|choice| Version::parse(&choice.version))
            .or_else(|| Version::parse(&self.config.base.default_version));
        match hover::describe(start, tag, value, &self.render) {
            Some(text) => Ok(json!(text)),
            None => Err(super::config_pull::request_failed(&format!(
                "{DESCRIBE_TAG}: no dictionary defines tag {tag}"
            ))),
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

/// `[uri, ranges]`, typed. The protocol says only "any[]", so the shape is
/// this server's to document and to check.
fn arguments(params: &ExecuteCommandParams) -> Result<(Uri, Vec<Range>), ResponseError> {
    let args = params.arguments.as_deref().unwrap_or_default();
    let [uri, ranges] = args else {
        return Err(invalid(format!(
            "{DECODE} takes [uri, ranges]; got {} argument(s)",
            args.len()
        )));
    };
    let uri: Uri = serde_json::from_value(uri.clone())
        .map_err(|e| invalid(format!("{DECODE} argument 1 (uri): {e}")))?;
    let ranges: Vec<Range> = serde_json::from_value(ranges.clone())
        .map_err(|e| invalid(format!("{DECODE} argument 2 (ranges): {e}")))?;
    if ranges.is_empty() {
        return Err(invalid(format!("{DECODE}: no ranges to decode")));
    }
    Ok((uri, ranges.into_iter().map(ordered).collect()))
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

fn prepare(ranges: Vec<Range>) -> RequestTask {
    Box::new(move |snapshot: &Snapshot, _: &mut Notices| answer(&ranges, snapshot))
}

fn answer(ranges: &[Range], snapshot: &Snapshot) -> HandlerResult {
    let mut out: Vec<String> = Vec::new();
    // Clamped to the document, not merely bounded by `rangeLines`: a client
    // saying "to the end of the file" with a sentinel line number would
    // otherwise spin through four billion numbers that address nothing.
    let last = snapshot.line_count().saturating_sub(1);
    for range in ranges {
        for line in range.start.line.min(last)..=range.end.line.min(last) {
            if let Some(text) = decoded_line(range, LineNo::new(line), snapshot) {
                out.push(text);
            }
        }
    }
    Ok(json!(out.join("\n")))
}

/// One line of one range, or `None` when the window selects no field — an
/// empty string per skipped line would be a blank line in the answer.
fn decoded_line(range: &Range, number: LineNo, snapshot: &Snapshot) -> Option<String> {
    let line = snapshot.line(number)?;
    // `analyze` and drop: see the module doc.
    let analysis = crate::ctx::analyze(&line.bytes, &snapshot.ctx);
    let message = analysis.message()?;

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
        let at = ByteOffset::from_len(start)?;
        let field = message.field_at(at)?;
        return Some(decode::line(
            message,
            &line.bytes,
            [field].into_iter(),
            &snapshot.render,
        ));
    }

    let selected: Vec<&Field> = message
        .fields
        .iter()
        .filter(|field| overlaps(field, start, end))
        .collect();
    (!selected.is_empty())
        .then(|| decode::line(message, &line.bytes, selected.into_iter(), &snapshot.render))
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
    #[cfg(not(feature = "no-line-cache"))]
    use crate::server::testing::cached;
    use crate::server::testing::{answered, state_with, HEARTBEAT};
    use lsp_types::Uri;

    const THREE: &str = "8=FIX.4.4|35=0|34=1|10=171|\n\
8=FIX.4.4|35=1|34=2|10=172|\n\
8=FIX.4.4|35=2|34=3|10=173|";

    fn ask(state: &mut ServerState, uri: &Uri, ranges: Value) -> Value {
        answered(state.execute_command_task(&json!({
            "command": DECODE,
            "arguments": [uri.as_str(), ranges],
        })))
    }

    fn range(sl: u32, sc: u32, el: u32, ec: u32) -> Value {
        json!({
            "start": { "line": sl, "character": sc },
            "end": { "line": el, "character": ec },
        })
    }

    fn refused(state: &mut ServerState, uri: &Uri, ranges: Value) -> ResponseError {
        match state.execute_command_task(&json!({
            "command": DECODE,
            "arguments": [uri.as_str(), ranges],
        })) {
            Dispatched::Now(Err(e)) => e,
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
    fn a_range_ending_at_column_zero_does_not_reach_that_line() {
        let (mut state, uri) = state_with(THREE);
        let answer = ask(&mut state, &uri, json!([range(0, 0, 1, 0)]));
        let text = answer.as_str().expect("a string");
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("0(Heartbeat)"), "{text}");
    }

    /// A block is ranges, one per line — the shape two positions cannot carry.
    #[test]
    fn a_block_is_one_range_a_line_and_they_join() {
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
        // On a separator there is no field, and no line in the answer.
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
        let as_bytes = answer(&ranges, &snapshot).expect("answered");
        snapshot.encoding = PositionEncoding::Utf16;
        let as_units = answer(&ranges, &snapshot).expect("answered");

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
        let unknown = match state.execute_command_task(&json!({
            "command": "fix.somethingElse",
            "arguments": [uri.as_str(), [range(0, 0, 0, 1)]],
        })) {
            Dispatched::Now(Err(e)) => e,
            _ => panic!("refused"),
        };
        assert_eq!(unknown.code, lsp_server::ErrorCode::InvalidParams as i32);
        assert!(unknown.message.contains("fix.somethingElse"), "{unknown:?}");
        for offered in [DECODE, USE_DICTIONARY, DESCRIBE_OVERRIDES, DESCRIBE_TAG] {
            assert!(unknown.message.contains(offered), "{unknown:?}");
        }

        let short = match state.execute_command_task(&json!({
            "command": DECODE,
            "arguments": [uri.as_str()],
        })) {
            Dispatched::Now(Err(e)) => e,
            _ => panic!("refused"),
        };
        assert_eq!(short.code, lsp_server::ErrorCode::InvalidParams as i32);

        let empty = refused(&mut state, &uri, json!([]));
        assert_eq!(empty.code, lsp_server::ErrorCode::InvalidParams as i32);
    }

    fn describe(state: &mut ServerState, arguments: Value) -> Result<Value, ResponseError> {
        match state.execute_command_task(&json!({
            "command": DESCRIBE_TAG,
            "arguments": arguments,
        })) {
            Dispatched::Now(result) => result,
            Dispatched::Task(..) | Dispatched::Lazy(..) => {
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

    /// The configured default version is where the search starts; a tag it
    /// lacks is still answered, from the version that has it.
    #[test]
    fn a_tag_is_described_from_the_default_version_or_the_one_that_has_it() {
        let (mut state, _) = state_with(HEARTBEAT);
        assert!(described(&mut state, json!([49])).starts_with("*Under FIX.4.4*"));
        let later = described(&mut state, json!([1328]));
        assert!(later.starts_with("*Under FIX.5.0SP2*"), "{later}");
        assert!(
            later.contains("*Not in FIX.4.4, introduced in FIX.5.0SP1*"),
            "{later}"
        );
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
            "fix.describeTag: no dictionary defines tag 9999"
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

    /// Before the first configuration commit the start version is a guess, so
    /// the command says to come back rather than answer under defaults.
    #[test]
    fn a_description_before_the_first_commit_is_refused() {
        let (sender, _receiver) = crossbeam_channel::unbounded::<lsp_server::Message>();
        let mut state = ServerState::new(sender);
        let error = refused_description(&mut state, json!([49]));
        assert_eq!(error.code, lsp_server::ErrorCode::RequestFailed as i32);
        assert_eq!(error.message, "configuration is not ready, retry later");
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
        assert!(described(&mut state, json!([49])).starts_with("*Under FIX.4.4*"));
    }

    /// A custom session selection lends its version as the start and nothing
    /// else: its own definitions are not where a description comes from.
    #[test]
    fn a_custom_session_selection_contributes_only_its_version() {
        use crate::dictionary::parse::fixtures::{field, quickfix};
        use crate::dictionary::{Dict, DictionaryChoice};
        let (mut state, _) = state_with(HEARTBEAT);
        state.config.session_choice = Some(DictionaryChoice {
            dict: Dict::Owned(std::sync::Arc::new(quickfix(
                "FIX.4.2",
                &field(9999, "DeskOnly"),
            ))),
            version: "FIX.4.2".into(),
            source: "desk.xml".into(),
            identity: 7,
            name: Some("desk".into()),
        });
        let text = described(&mut state, json!([1328]));
        assert!(
            text.contains("*Not in FIX.4.2, introduced in FIX.5.0SP1*"),
            "{text}"
        );
        let error = refused_description(&mut state, json!([9999]));
        assert_eq!(
            error.message,
            "fix.describeTag: no dictionary defines tag 9999"
        );
    }

    /// A yank is one-shot: the contract permits it to cache and it declines.
    #[test]
    #[cfg(not(feature = "no-line-cache"))]
    fn a_decode_caches_nothing() {
        let (mut state, uri) = state_with(THREE);
        ask(&mut state, &uri, json!([range(0, 0, 2, 27)]));
        assert!(
            cached(&mut state, &uri).iter().all(|seen| !seen),
            "a decode reads every line it answers about and keeps none"
        );
    }
}
