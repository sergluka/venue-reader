//! The pool-ready request boundary.
//!
//! A request handler `prepare`s a [`RequestTask`] over an immutable [`Snapshot`].
//! Stage 1 runs the task inline in the main loop; Stage 3/4 submits the same task
//! to a bounded executor and completes the response from a result channel — no
//! handler is rewritten, only the main loop's execution site changes.

use std::sync::Arc;

use lsp_server::{Notification, ResponseError};
use lsp_types::{LogMessageParams, MessageType};
use serde_json::{json, Value};

use crate::codec::LineAnalysis;
use crate::ctx::Ctx;
use crate::document::{Line, LineNo, LineRange, Population, PositionEncoding};
use crate::render::Options;
use crate::rules::Registry;
use crate::transport::{Sink, WriteJson};

/// Immutable per-document view a request task runs against.
///
/// Exists to keep handlers pool-ready: a task is `Send + 'static`, so it cannot
/// borrow `ServerState`, and the main loop stays free to mutate documents while a
/// task runs. Cloning is cheap — `Arc` pointers, never line bytes — so a task
/// keeps a coherent frozen view of the document it was scheduled on, instead of
/// stitching its answer from lines that changed underneath it. Stage 1 runs tasks
/// inline; the executor arrives in Stage 3/4 with the handlers unchanged.
#[derive(Clone)]
pub struct Snapshot {
    pub lines: Arc<Vec<Arc<Line>>>,
    pub encoding: PositionEncoding,
    /// Dictionaries, resolution and fingerprint as one value: separate fields
    /// could be paired wrongly, this cannot be.
    pub ctx: Arc<Ctx>,
    /// What hover calls each bound dictionary, from the registry and selection
    /// in force — never from `ctx`, which is reused while its fingerprint holds.
    pub names: Arc<crate::ctx::DictionaryNames>,
    /// The document's count of cached lines, so a task populating on another
    /// thread is counted where the main loop sweeps.
    pub population: Arc<Population>,
    /// Beside the context rather than inside it: rendering configuration
    /// invalidates no analysis.
    pub render: Arc<Options>,
    /// The rules as configured, for the same reason `render` is here: a request
    /// needs them, the document does not own them.
    pub rules: Arc<Registry>,
    /// The version this answer is computed against. An edit that does not carry
    /// it is an edit a client will apply to text that has moved.
    pub version: i32,
}

impl Snapshot {
    /// The line at a number, or nothing when the document is shorter.
    ///
    /// The typed boundary is here rather than on `Document` because this is
    /// what handlers index: an accessor over there would have left every call
    /// site writing `.get(n as usize)`.
    pub fn line(&self, no: LineNo) -> Option<&Arc<Line>> {
        self.lines.get(no.index())
    }

    /// A line's analysis, cached on the line — **the populating accessor**.
    ///
    /// Only viewport-bounded requests may call this; a whole-document walk
    /// calls `ctx::analyze` and drops the result. Here rather than on `Line`
    /// so the context and the population counter are paired once, by the
    /// snapshot that owns both.
    pub fn analysis(&self, line: &Line) -> Arc<LineAnalysis> {
        line.cached_analysis(&self.ctx, &self.population)
    }

    pub fn line_count(&self) -> u32 {
        self.lines.len() as u32
    }

    /// Lines the document has, not counting the empty one a trailing newline
    /// leaves behind.
    ///
    /// Every log ends with a newline, and counting the line it leaves would
    /// refuse a document of exactly `cap` lines — the boundary a cap is most
    /// likely to be set at, and the one the code-lens list hit first.
    pub fn content_lines(&self) -> usize {
        crate::document::content_lines(&self.lines)
    }

    /// This line's identity, as a lazily-resolved item carries it back.
    ///
    /// Two checks that fail for different reasons: the document has moved, or
    /// this one line has. A `didOpen` that replaces a document keeps its
    /// version, which is why the hash is not redundant — and it is **hex**,
    /// because `Line.hash` is a `u64` whose values are mostly past
    /// JavaScript's exact integer range, so a JSON number comes back rounded
    /// from a JS client and never matches again.
    pub fn stamp(&self, uri: &str, number: LineNo, line: &Line) -> Value {
        json!({
            "uri": uri,
            "version": self.version,
            "line": number,
            "hash": format!("{:016x}", line.hash),
        })
    }

    /// The line a `stamp` refers to, or why it is no longer that line.
    pub fn stamped(&self, data: &Value, method: &str) -> Result<&Arc<Line>, ResponseError> {
        let fail = |why| super::content_modified(method, why);
        if data["version"].as_i64() != Some(self.version as i64) {
            return Err(fail("the document has moved"));
        }
        let number = data["line"]
            .as_u64()
            .and_then(|n| LineNo::try_from(n).ok())
            .ok_or_else(|| fail("it carries no line"))?;
        let line = self.line(number).ok_or_else(|| fail("the line is gone"))?;
        if data["hash"].as_str() != Some(format!("{:016x}", line.hash).as_str()) {
            return Err(fail("the line has changed"));
        }
        Ok(line)
    }

    /// A span of lines clamped to the document.
    ///
    /// The clamp only — **not** how a client's `Range` becomes `last`, which
    /// differs by request and must: a cursor at `(n,0)..(n,0)` covers line `n`
    /// for a code action and nothing for an inlay hint. The two handlers
    /// compute their own `last` and share this, which is the half they agree
    /// on; the next range request has something to call rather than a comment
    /// to copy.
    pub fn line_span(&self, first: u32, last: u32) -> LineRange {
        let total = self.line_count();
        LineRange::new(LineNo::new(first.min(total)), LineNo::new(last.min(total)))
    }
}

/// What a running task may say to the client: notifications, nothing else.
///
/// Narrow on purpose. Handlers answer by returning; this exists for the one
/// thing a return value cannot carry — a fact discovered *during* the work,
/// such as a tree that did not fit its budget. A task cannot send a response
/// and cannot send an arbitrary message.
///
/// **Where a notice goes is behind this type, and changes with the executor.**
/// Tasks run inline today, so the loop is blocked inside the task and sending
/// directly is the only way anything precedes the response. When they move to
/// a pool this becomes a channel *into* the loop carrying the request id, and
/// the loop validates before sending — which is what keeps "background results
/// reach the client only through the main loop" true. The task-facing API does
/// not change.
pub struct Notices {
    to: Option<Sink>,
    said: Vec<(MessageType, String)>,
}

impl Notices {
    pub fn to(sender: impl Into<Sink>) -> Notices {
        Notices {
            to: Some(sender.into()),
            said: Vec::new(),
        }
    }

    /// A sink with nowhere to send, which keeps what it was told instead.
    ///
    /// What a task says is then asserted as data rather than by watching a
    /// channel. It does not assert *ordering* — that is a transport property
    /// and needs a test through the protocol.
    pub fn collecting() -> Notices {
        Notices {
            to: None,
            said: Vec::new(),
        }
    }

    pub fn log(&mut self, kind: MessageType, text: &str) {
        match &self.to {
            Some(sender) => {
                let note = Notification::new(
                    super::method::LOG_MESSAGE.into(),
                    json!(LogMessageParams {
                        kind,
                        message: text.to_owned(),
                    }),
                );
                sender.send(note);
            }
            None => self.said.push((kind, text.to_owned())),
        }
    }

    /// What a collecting sink was told; always empty for a sending one.
    pub fn said(&self) -> &[(MessageType, String)] {
        &self.said
    }
}

pub type HandlerResult = Result<Value, ResponseError>;

/// What the loop sends back for a result.
///
/// `Json` is the tree the handlers build. `Lazy` is a value that is serialised
/// on the writer's thread instead — for a payload large enough that building a
/// `Value` and letting it be serialised again costs three times the time and
/// ten times the transient memory of writing it straight.
pub enum Answer {
    Json(Value),
    Lazy(Box<dyn WriteJson>),
}

impl From<Value> for Answer {
    fn from(value: Value) -> Answer {
        Answer::Json(value)
    }
}

/// What a request handler hands back to the main loop.
///
/// Handlers never execute their own work: they either answer immediately from
/// server state, or return a task plus the snapshot it runs against. The loop is
/// the single execution site, so Stage 3/4 submits the task to a bounded executor
/// there — no handler changes shape.
pub enum Dispatched {
    Now(HandlerResult),
    Task(Snapshot, RequestTask),
    /// Answered from server state like `Now`, as a value the writer
    /// serialises rather than a tree this builds.
    Lazy(Result<Box<dyn WriteJson>, ResponseError>),
}

pub(super) fn configuration_not_ready(
    state: &super::ServerState,
    uri: &lsp_types::Uri,
) -> Option<Dispatched> {
    (state.docs.contains_key(uri) && state.effective_config(uri).is_none()).then(|| {
        Dispatched::Now(Err(super::config_pull::request_failed(
            "document configuration is not ready",
        )))
    })
}

/// A `'static + Send` unit of work over a [`Snapshot`]. `Send` is required now so
/// the task can move to a worker thread unchanged in a later stage.
pub type RequestTask = Box<dyn FnOnce(&Snapshot, &mut Notices) -> HandlerResult + Send>;

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_server::Message;

    /// The sink a task holds is the same shape whether it sends or collects, so
    /// a handler cannot behave differently under test than it does in the loop.
    #[test]
    fn a_sending_sink_puts_a_log_notification_on_the_channel() {
        let (sender, receiver) = crossbeam_channel::unbounded::<lsp_server::Message>();
        let mut notices = Notices::to(sender);
        notices.log(MessageType::Info, "the tree did not fit");

        let Ok(Message::Notification(note)) = receiver.try_recv() else {
            panic!("a notification");
        };
        assert_eq!(note.method, super::super::method::LOG_MESSAGE);
        assert_eq!(note.params["message"], "the tree did not fit");
        assert!(notices.said().is_empty(), "a sending sink keeps nothing");
    }

    #[test]
    fn a_collecting_sink_keeps_what_it_was_told() {
        let mut notices = Notices::collecting();
        assert!(notices.said().is_empty(), "and says nothing unprompted");
        notices.log(MessageType::Info, "the tree did not fit");
        assert_eq!(
            notices.said(),
            [(MessageType::Info, "the tree did not fit".to_owned())]
        );
    }
}
