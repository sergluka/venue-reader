//! Background validation of whole documents.
//!
//! Diagnostics are the only whole-document obligation in the design, so this is
//! the first caller the cache-population contract was written for: the walk
//! calls [`analyze`] and drops the result, never `Line::cached_analysis`.
//!
//! **The worker does not talk to the client.** It walks and reports back; the
//! main loop validates that the document is still open and the generation is
//! still current, and performs the single outbound send. Publishing from the
//! worker would race `didClose` — which drops the document immediately — and a
//! configuration change is worse, because the stale and fresh sets carry the
//! *same* document version and a client cannot tell them apart.
//!

mod cap;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use lsp_server::Notification;
use lsp_types::Uri;
use serde_json::json;

use super::dispatch::Snapshot;
use super::{OutgoingRequest, ServerState};
use crate::ctx::analyze;
use crate::document::{Column, Columns, LineNo};
use crate::rules::{Diag, Scope};
use crate::transport::Sink;
use cap::{Admit, CappedFindings, Quota};

/// Collapses a typing burst into one walk.
const DEBOUNCE: Duration = Duration::from_millis(80);
/// A publish costs the client a redraw of its whole problem list.
const PUBLISH_EVERY: Duration = Duration::from_millis(250);
/// Cancellation is cooperative; this is how often the walk asks.
const CANCEL_EVERY: usize = 256;
/// The `default` arm still has to name a timeout when nothing is pending.
const IDLE: Duration = Duration::from_secs(3600);

/// One document to validate, at one version.
struct Job {
    uri: Uri,
    generation: u64,
    snapshot: Snapshot,
    cancel: Arc<AtomicBool>,
    cap: usize,
    /// How often the walk reports back. A policy rather than a constant of the
    /// algorithm, which is also what lets a test drive it without a clock.
    every: Duration,
}

/// What the walk has to say. Never sent to the client directly.
pub(crate) enum Report {
    /// Diagnostics found so far, as `(line, diagnostic)`.
    Batch {
        uri: Uri,
        generation: u64,
        found: Vec<(LineNo, Diag)>,
    },
    /// How far the walk has got, as a percentage of the document. Sent on the
    /// same cadence as batches but independently of them: a clean document
    /// finds nothing and would otherwise report no progress at all.
    Progressed {
        uri: Uri,
        generation: u64,
        percentage: u32,
    },
    /// The walk finished. `total` counts everything found, including whatever
    /// the cap left out.
    Done {
        uri: Uri,
        generation: u64,
        total: usize,
    },
    /// The walk panicked. The previous set stays; the worker survives.
    Panicked {
        uri: Uri,
        generation: u64,
        reason: String,
    },
}

struct Inflight {
    generation: u64,
    cancel: Arc<AtomicBool>,
    /// CappedFindings so the byte→position conversion uses the lines the walk saw, and so
    /// the publish carries the version the walk was about.
    snapshot: Snapshot,
    capped: CappedFindings,
    published_at: Option<Instant>,
    /// The token the client agreed to, before anything is said under it.
    /// `None` until the `create` is answered, and for clients that refuse.
    token: Option<String>,
    /// `None` until the walk has run long enough to be worth announcing.
    progress: Option<Progress>,
}

/// A begun `$/progress` sequence. Every one owes an `end`, on completion,
/// cancellation, coalescing, worker panic and shutdown alike — so the `end`
/// lives in `Drop` and no exit path can forget it.
struct Progress {
    sender: Sink,
    token: String,
}

impl Progress {
    /// Announce the work under a token the client has already agreed to.
    fn begin(sender: &Sink, token: String, percentage: u32) -> Progress {
        notify(
            sender,
            super::method::PROGRESS,
            json!({
                "token": token,
                // With a percentage the client renders a determinate bar; the
                // walk always knows how many lines it has left.
                "value": { "kind": "begin", "title": "Validating FIX", "percentage": percentage },
            }),
        );
        Progress {
            sender: sender.clone(),
            token,
        }
    }
}

impl Progress {
    fn report(&self, percentage: u32) {
        notify(
            &self.sender,
            super::method::PROGRESS,
            json!({
                "token": self.token,
                "value": { "kind": "report", "percentage": percentage },
            }),
        );
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        notify(
            &self.sender,
            super::method::PROGRESS,
            json!({ "token": self.token, "value": { "kind": "end" } }),
        );
    }
}

fn notify(sender: &Sink, method: &str, params: serde_json::Value) {
    sender.send(Notification {
        method: method.into(),
        params,
    });
}

/// The token for one walk. Unique per generation, so a stale `create` reply
/// cannot begin progress for the walk that replaced it.
fn token_for(generation: u64) -> String {
    format!("fix/diagnostics/{generation}")
}

/// The scheduler, owned by the main loop, plus the worker it feeds.
pub(crate) struct Worker {
    jobs_tx: Option<Sender<Job>>,
    pub(super) reports_rx: Receiver<Report>,
    thread: Option<JoinHandle<()>>,
    /// Documents that changed and have not been dispatched yet.
    dirty: HashMap<Uri, (i32, Instant)>,
    inflight: HashMap<Uri, Inflight>,
    /// Combined identity suppresses unchanged publishes.
    published: HashMap<Uri, u64>,
    configuration: HashMap<Uri, Vec<serde_json::Value>>,
    // Retaining a Value tree here would duplicate the largest publish payload.
    fix_results: HashMap<Uri, Vec<u8>>,
    generation: u64,
}

impl Worker {
    pub(super) fn start() -> Worker {
        let (jobs_tx, jobs_rx) = unbounded::<Job>();
        let (reports_tx, reports_rx) = unbounded::<Report>();
        let thread = std::thread::Builder::new()
            .name("fix-lsp-diagnostics".into())
            .spawn(move || run(&jobs_rx, &reports_tx))
            .expect("spawning the diagnostics worker");
        Worker {
            jobs_tx: Some(jobs_tx),
            reports_rx,
            thread: Some(thread),
            dirty: HashMap::new(),
            inflight: HashMap::new(),
            published: HashMap::new(),
            configuration: HashMap::new(),
            fix_results: HashMap::new(),
            generation: 0,
        }
    }

    /// How long the main loop may block before the debounce is due.
    pub(super) fn tick_in(&self) -> Duration {
        let now = Instant::now();
        self.dirty
            .values()
            .map(|(_, since)| DEBOUNCE.saturating_sub(now.saturating_duration_since(*since)))
            .min()
            .unwrap_or(IDLE)
    }

    pub(super) fn mark_dirty(&mut self, uri: &Uri, version: i32) {
        self.dirty.insert(uri.clone(), (version, Instant::now()));
    }

    /// Stop believing anything about this document. The caller publishes the
    /// empty set, because only it can send.
    pub(super) fn forget(&mut self, uri: &Uri) {
        self.dirty.remove(uri);
        if let Some(inflight) = self.inflight.remove(uri) {
            inflight.cancel.store(true, Ordering::Relaxed);
        }
        self.published.remove(uri);
        self.configuration.remove(uri);
        self.fix_results.remove(uri);
    }

    /// Stop the walk of one document, keeping whatever the client already knows
    /// about it.
    pub(super) fn cancel(&mut self, uri: &Uri) {
        if let Some(inflight) = self.inflight.remove(uri) {
            inflight.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Everything an in-flight walk was told; used when the interpretation
    /// moves under it.
    pub(super) fn cancel_all(&mut self) {
        for inflight in self.inflight.values() {
            inflight.cancel.store(true, Ordering::Relaxed);
        }
        self.inflight.clear();
    }

    /// Post a job for every document whose debounce has elapsed.
    fn due(&mut self) -> Vec<(Uri, i32)> {
        let now = Instant::now();
        let due: Vec<Uri> = self
            .dirty
            .iter()
            .filter(|(_, (_, since))| now.saturating_duration_since(*since) >= DEBOUNCE)
            .map(|(uri, _)| uri.clone())
            .collect();
        due.into_iter()
            .filter_map(|uri| self.dirty.remove(&uri).map(|(version, _)| (uri, version)))
            .collect()
    }

    fn submit(&mut self, uri: Uri, snapshot: Snapshot, cap: usize) {
        if let Some(previous) = self.inflight.remove(&uri) {
            previous.cancel.store(true, Ordering::Relaxed);
        }
        self.generation += 1;
        let generation = self.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.inflight.insert(
            uri.clone(),
            Inflight {
                generation,
                cancel: cancel.clone(),
                snapshot: snapshot.clone(),
                capped: CappedFindings::new(cap),
                published_at: None,
                token: None,
                progress: None,
            },
        );
        let job = Job {
            uri,
            generation,
            snapshot,
            cancel,
            cap,
            every: PUBLISH_EVERY,
        };
        if let Some(jobs_tx) = &self.jobs_tx {
            let _ = jobs_tx.send(job);
        }
    }

    pub(super) fn shutdown(&mut self) {
        // Dropping the sender is what ends the worker's loop, so it has to
        // happen before the join rather than in whatever order `Drop` runs.
        self.jobs_tx = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    pub(super) fn abandon(&mut self) {
        self.cancel_all();
        self.dirty.clear();
        self.jobs_tx = None;
        self.thread.take();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The walk. One job at a time; a panic in one costs that document's
/// diagnostics, not the session's.
fn run(jobs: &Receiver<Job>, reports: &Sender<Report>) {
    for job in jobs {
        let uri = job.uri.clone();
        let generation = job.generation;
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| walk(&job, reports)));
        let report = match outcome {
            Ok(total) => Report::Done {
                uri,
                generation,
                total,
            },
            Err(payload) => Report::Panicked {
                uri,
                generation,
                reason: super::panic_reason(&*payload),
            },
        };
        if reports.send(report).is_err() {
            return; // the main loop is gone
        }
    }
}

/// Returns everything found, including what the cap left out.
fn walk(job: &Job, reports: &Sender<Report>) -> usize {
    let mut total = 0usize;
    let mut batch: Vec<(LineNo, Diag)> = Vec::new();
    // The main loop replays the same decisions over what is sent.
    let mut quota = Quota::new(job.cap);
    let mut last_sent = Instant::now();
    let mut document_keys = HashSet::new();

    for (number, line) in job.snapshot.lines.iter().enumerate() {
        if number % CANCEL_EVERY == 0 && job.cancel.load(Ordering::Relaxed) {
            return total;
        }
        // `analyze`, not `cached_analysis`: a whole-document walk may not
        // populate the cache. This is the caller that rule exists for.
        let analysis = analyze(&line.bytes, &job.snapshot.ctx);
        let scope = Scope {
            label: analysis
                .message()
                .and_then(|message| job.snapshot.names.of(&message.version)),
            // What the cap would refuse is only counted. Fixed for the whole
            // line, so one line still renders all it finds.
            count_from: quota.refused_from(),
            only: None,
        };
        let checked = job.snapshot.rules.check(&line.bytes, &analysis, &scope);
        total += checked.counted;
        for diag in checked.diags {
            if let Some(key) = &diag.document_key {
                if !document_keys.insert((diag.rule, key.clone())) {
                    continue;
                }
            }
            total += 1;
            if quota.admit(diag.severity) != Admit::Refused {
                batch.push((LineNo::new(number as u32), diag));
            }
        }
        if last_sent.elapsed() >= job.every {
            last_sent = Instant::now();
            let sent = reports.send(Report::Progressed {
                uri: job.uri.clone(),
                generation: job.generation,
                percentage: percent(number + 1, job.snapshot.lines.len()),
            });
            if sent.is_err() {
                return total;
            }
            if !batch.is_empty() {
                let found = std::mem::take(&mut batch);
                if reports
                    .send(Report::Batch {
                        uri: job.uri.clone(),
                        generation: job.generation,
                        found,
                    })
                    .is_err()
                {
                    return total;
                }
            }
        }
    }
    if !batch.is_empty() {
        let _ = reports.send(Report::Batch {
            uri: job.uri.clone(),
            generation: job.generation,
            found: batch,
        });
    }
    total
}

/// LSP takes a `uinteger` 0..=100. An empty document is finished, not nowhere.
fn percent(done: usize, total: usize) -> u32 {
    if total == 0 {
        return 100;
    }
    ((done.min(total) * 100) / total) as u32
}

/// The LSP shape of one diagnostic. `data` carries the rule id so a code action
/// can find its way back without re-parsing the message.
fn as_lsp(line: LineNo, diag: &Diag, columns: (Column, Column)) -> serde_json::Value {
    json!({
        "range": {
            "start": { "line": line, "character": columns.0 },
            "end": { "line": line, "character": columns.1 },
        },
        "severity": lsp_severity(diag.severity),
        "code": diag.code,
        "source": "fix",
        "message": diag.message,
        // The rule, not the code: it is the `diagnostics.rules` key that governs
        // this diagnostic, and one rule can report more than one code.
        "data": { "rule": diag.rule },
    })
}

pub(super) fn lsp_severity(severity: crate::config::Severity) -> lsp_types::DiagnosticSeverity {
    use crate::config::Severity::*;
    use lsp_types::DiagnosticSeverity;
    match severity {
        Error => DiagnosticSeverity::Error,
        Warning => DiagnosticSeverity::Warning,
        Info => DiagnosticSeverity::Information,
        Hint => DiagnosticSeverity::Hint,
    }
}

/// Convert one line's diagnostics in a single ascending walk.
pub(super) fn render(found: &[(LineNo, &Diag)], snapshot: &Snapshot) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(found.len());
    let mut at = 0usize;
    while at < found.len() {
        let line_number = found[at].0;
        let mut end = at;
        while end < found.len() && found[end].0 == line_number {
            end += 1;
        }
        let group = &found[at..end];
        let Some(line) = snapshot.line(line_number) else {
            at = end;
            continue;
        };
        let columns = Columns::of(
            line,
            group.iter().flat_map(|(_, diag)| [diag.start, diag.end]),
            snapshot.encoding,
        );
        for (_, diag) in group {
            out.push(as_lsp(
                line_number,
                diag,
                (columns.at(diag.start), columns.at(diag.end)),
            ));
        }
        at = end;
    }
    out
}

pub(super) fn publish(sender: &Sink, uri: &Uri, version: i32, diagnostics: Vec<serde_json::Value>) {
    let mut params = json!({ "uri": uri, "version": version });
    params
        .as_object_mut()
        .expect("publish object")
        .insert("diagnostics".into(), serde_json::Value::Array(diagnostics));
    notify(sender, super::method::PUBLISH_DIAGNOSTICS, params);
}

impl ServerState {
    /// A document changed; the walk starts once the debounce elapses.
    pub(super) fn schedule_diagnostics(&mut self, uri: &Uri, version: i32) {
        let Some(config) = self.effective_config(uri) else {
            return;
        };
        if !config.diagnostics.enabled {
            return;
        }
        // Whatever is walking this document is now about text nobody has. Stop
        // it here rather than when the debounce submits its replacement 80 ms
        // later, which is long enough for it to finish and publish.
        self.diagnostics.cancel(uri);
        self.diagnostics.mark_dirty(uri, version);
    }

    /// Forget a document and tell the client so. Its marks would otherwise
    /// outlive it.
    pub(super) fn clear_diagnostics(&mut self, uri: &Uri, version: i32) {
        let had = self.diagnostics.published.contains_key(uri);
        self.diagnostics.forget(uri);
        if had {
            publish(&self.sender, uri, version, Vec::new());
        }
    }

    pub(super) fn set_configuration_diagnostics(
        &mut self,
        uri: &Uri,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) {
        let rendered = diagnostics
            .into_iter()
            .map(|diagnostic| json!(diagnostic))
            .collect();
        self.diagnostics.configuration.insert(uri.clone(), rendered);
    }

    pub(super) fn diagnostics_changed_for(&mut self, uri: &Uri) {
        self.diagnostics.cancel(uri);
        self.diagnostics.dirty.remove(uri);
        // The retained results may describe older text or interpretation.
        self.diagnostics.fix_results.remove(uri);
        let Some(configuration) = self.config.documents.get(uri) else {
            return;
        };
        let enabled = !configuration.rules.is_empty();
        let Some(version) = self.docs.get(uri).map(|doc| doc.version) else {
            return;
        };
        if enabled {
            self.diagnostics.mark_dirty(uri, version);
        }
        self.publish_combined(uri, version);
    }

    fn publish_combined(&mut self, uri: &Uri, version: i32) {
        let mut bytes = self
            .diagnostics
            .fix_results
            .get(uri)
            .cloned()
            .unwrap_or_else(|| b"[]".to_vec());
        if let Some(configuration) = self
            .diagnostics
            .configuration
            .get(uri)
            .filter(|values| !values.is_empty())
        {
            bytes.pop();
            if bytes.len() > 1 {
                bytes.push(b',');
            }
            let encoded =
                serde_json::to_vec(configuration).expect("configuration diagnostics serialize");
            bytes.extend_from_slice(&encoded[1..]);
        }
        if bytes == b"[]"
            && !self.diagnostics.published.contains_key(uri)
            && !self.diagnostics.fix_results.contains_key(uri)
        {
            return;
        }
        let identity = xxhash_rust::xxh3::xxh3_64(&bytes);
        if self.diagnostics.published.get(uri) == Some(&identity) {
            return;
        }
        let rendered = serde_json::from_slice(&bytes).expect("retained diagnostics JSON");
        publish(&self.sender, uri, version, rendered);
        self.diagnostics.published.insert(uri.clone(), identity);
    }

    /// The debounce elapsed for at least one document.
    pub(super) fn dispatch_due_diagnostics(&mut self) {
        for (uri, _version) in self.diagnostics.due() {
            let Some(config) = self.effective_config(&uri) else {
                continue;
            };
            let cap = config.limits.diagnostics_per_document;
            let Some(snapshot) = self.snapshot_for(&uri) else {
                continue; // closed between the edit and the debounce
            };
            if snapshot.rules.is_empty() {
                continue;
            }
            self.diagnostics.submit(uri.clone(), snapshot, cap);
            self.request_progress(&uri);
        }
    }

    /// Ask for a token for the walk just submitted. The walk does not wait for
    /// the answer — it may well outlive the round trip — but nothing may be
    /// reported under the token until the client has agreed to it.
    ///
    /// The method name comes from `lsp_types` rather than a literal: spelled by
    /// hand it was `window/workDoneProgressCreate` for a whole stage, and every
    /// client was right to answer `MethodNotFound`.
    fn request_progress(&mut self, uri: &Uri) {
        if !self.client.progress {
            return;
        }
        let Some(inflight) = self.diagnostics.inflight.get(uri) else {
            return;
        };
        let generation = inflight.generation;
        let req = self.req_queue.outgoing.register(
            super::method::PROGRESS_CREATE.into(),
            json!({ "token": token_for(generation) }),
            OutgoingRequest::ProgressCreate {
                uri: uri.clone(),
                generation,
            },
        );
        self.sender.send(req);
    }

    /// The client answered the `create`. A refusal is not an error: the walk
    /// runs, it simply reports no progress.
    pub(super) fn on_progress_created(&mut self, uri: &Uri, generation: u64, agreed: bool) {
        if !agreed {
            // A client that advertised `window.workDoneProgress` and then
            // refuses a token will refuse the next one too, so the refusal is
            // the better evidence: stop asking, and say so once. Clearing the
            // flag is what makes "once" automatic.
            self.client.progress = false;
            log::info!(
                "client advertised window.workDoneProgress but refused to create a token; \
                 reporting no progress for the rest of this session"
            );
            return;
        }
        // The walk may have finished or been replaced during the round trip;
        // keeping a token for it then would leave one nothing can end.
        let Some(inflight) = self.diagnostics.inflight.get_mut(uri) else {
            return;
        };
        if inflight.generation != generation || inflight.progress.is_some() {
            return;
        }
        // The token is kept, not used. Nothing is announced until the walk has
        // reported once, which is the first moment it has lasted long enough
        // to be worth a bar: a 10 000-line log is walked in ~17 ms, and a bar
        // that appears and vanishes inside one frame is worse than none. The
        // report cadence is what "long enough" means, so there is no second
        // number to keep in step with it.
        inflight.token = Some(token_for(generation));
    }

    pub(super) fn on_diagnostic_report(&mut self, report: Report) {
        match report {
            Report::Batch {
                uri,
                generation,
                found,
            } => {
                if !self.accept(&uri, generation) {
                    return;
                }
                let inflight = self.diagnostics.inflight.get_mut(&uri).expect("accepted");
                for (line, diag) in found {
                    inflight.capped.insert(line, diag);
                }
                let due = inflight
                    .published_at
                    .is_none_or(|at| at.elapsed() >= PUBLISH_EVERY);
                if due {
                    self.publish_current(&uri, None);
                }
            }
            Report::Progressed {
                uri,
                generation,
                percentage,
            } => {
                if !self.accept(&uri, generation) {
                    return;
                }
                let sender = self.sender.clone();
                let Some(inflight) = self.diagnostics.inflight.get_mut(&uri) else {
                    return;
                };
                match &inflight.progress {
                    Some(progress) => progress.report(percentage),
                    // The first report is what opens the bar, and it opens at
                    // the percentage the walk has actually reached.
                    None => {
                        if let Some(token) = inflight.token.take() {
                            inflight.progress = Some(Progress::begin(&sender, token, percentage));
                        }
                    }
                }
            }
            Report::Done {
                uri,
                generation,
                total,
            } => {
                if !self.accept(&uri, generation) {
                    return;
                }
                self.publish_current(&uri, Some(total));
                self.diagnostics.inflight.remove(&uri);
            }
            Report::Panicked {
                uri,
                generation,
                reason,
            } => {
                if !self.accept(&uri, generation) {
                    return;
                }
                self.diagnostics.inflight.remove(&uri);
                let message = format!("internal error while validating {}: {reason}", uri.as_str());
                log::error!("{message}");
                self.log_message(lsp_types::MessageType::Error, &message);
            }
        }
    }

    /// Whether a report still answers a live question: the document is open and
    /// the generation is the one currently in flight.
    fn accept(&self, uri: &Uri, generation: u64) -> bool {
        let Some(doc) = self.docs.get(uri) else {
            return false;
        };
        self.diagnostics.inflight.get(uri).is_some_and(|inflight| {
            // Generation answers "is this the walk we last asked for"; version
            // answers "is it still about the text we have". An edit inside the
            // debounce window moves the text while the generation stays current.
            inflight.generation == generation && inflight.snapshot.version == doc.version
        })
    }

    /// Send what has accumulated, but only when it differs from what the client
    /// already has: a publish redraws its whole problem list.
    fn publish_current(&mut self, uri: &Uri, total: Option<usize>) {
        let inflight = self.diagnostics.inflight.get_mut(uri).expect("in flight");
        let left_out = total.map_or(0, |total| total.saturating_sub(inflight.capped.len()));
        inflight.published_at = Some(Instant::now());
        let version = inflight.snapshot.version;
        let mut rendered = render(&inflight.capped.in_line_order(), &inflight.snapshot);
        if left_out > 0 {
            rendered.push(truncated(left_out));
        }
        let shown = inflight.capped.len();
        let encoded = serde_json::to_vec(&rendered).expect("FIX diagnostics serialize");
        drop(rendered);
        self.diagnostics.fix_results.insert(uri.clone(), encoded);
        self.publish_combined(uri, version);
        if left_out > 0 {
            // Through `logMessage` rather than `log::info!`: the fact is about
            // this document and the user is the one who can act on it, and the
            // client's own log is where they will look. Once per completed
            // walk — `total` is only ever `Some` at `Done`.
            //
            // Info rather than Warning: a log too large for the cap is ordinary
            // operation, not a defect in the document or in the server. The
            // `truncated` diagnostic is what makes the fact visible; this only
            // names the setting. Accepted consequence: a client whose own log
            // level hides `logMessage` at this severity will not show it.
            self.log_message(
                lsp_types::MessageType::Info,
                &format!(
                    "fix-lsp: {} — showing {shown} problems of {}; \
                     raise fix.limits.diagnosticsPerDocument to see the rest",
                    uri.as_str(),
                    shown + left_out
                ),
            );
        }
    }
}

/// Not a rule, and deliberately not addressable under `diagnostics.rules`: it
/// is the walker's own statement about its cap.
fn truncated(more: usize) -> serde_json::Value {
    json!({
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 0, "character": 0 },
        },
        "severity": 3,
        "code": "truncated",
        "source": "fix",
        "message": format!("{more} more problems not shown"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Diagnostics;
    use crate::ctx::Ctx;
    use crate::document::Document;
    use crate::rules::Registry;
    use lsp_server::Message;

    fn uri(path: &str) -> Uri {
        format!("file:///{path}").parse().expect("a uri")
    }

    fn snapshot(text: &str) -> Snapshot {
        let ctx = Ctx::for_test("FIX.4.4");
        let doc = Document::open(text.as_bytes(), 1, ctx);
        Snapshot {
            names: Arc::new(crate::ctx::DictionaryNames::new(
                Arc::new(crate::dictionary::Registry::bundled_only()),
                None,
            )),
            lines: doc.lines.clone(),
            encoding: crate::document::PositionEncoding::Utf8,
            ctx: doc.ctx.clone(),
            population: doc.population.clone(),
            render: Arc::new(crate::render::Options::default()),
            rules: Arc::new(Registry::from_config(&Diagnostics::default())),
            version: doc.version,
        }
    }

    const BAD: &str = "8=FIX.4.4|9=7|35=0|34=1|49=A|56=B|10=001|";

    #[test]
    fn configuration_diagnostics_publish_without_a_fix_walk_and_clear_independently() {
        let (sender, receiver) = unbounded::<lsp_server::Message>();
        let mut state = ServerState::new(sender);
        let uri = uri("configured.fix");
        let warning = lsp_types::Diagnostic {
            range: lsp_types::Range::default(),
            severity: Some(lsp_types::DiagnosticSeverity::Error),
            code: Some("projectConfig".into()),
            source: Some("fix".into()),
            message: "duplicate dictionary name in project configuration".into(),
            ..Default::default()
        };
        state.set_configuration_diagnostics(&uri, vec![warning]);
        state.publish_combined(&uri, 1);
        let Message::Notification(first) = receiver.try_recv().expect("configuration publish")
        else {
            panic!("notification");
        };
        assert_eq!(first.params["diagnostics"][0]["code"], "projectConfig");
        assert!(state.diagnostics.inflight.is_empty());
        state.publish_combined(&uri, 1);
        assert!(receiver.try_recv().is_err(), "unchanged combined identity");

        let fix = json!({"code": "beginString", "message": "missing BeginString"});
        state
            .diagnostics
            .fix_results
            .insert(uri.clone(), serde_json::to_vec(&vec![fix.clone()]).unwrap());
        state.publish_combined(&uri, 1);
        let Message::Notification(combined) = receiver.try_recv().expect("combined publish") else {
            panic!("notification");
        };
        assert_eq!(combined.params["diagnostics"].as_array().unwrap().len(), 2);
        state.set_configuration_diagnostics(&uri, Vec::new());
        state.publish_combined(&uri, 1);
        let Message::Notification(cleared) = receiver.try_recv().expect("clear configuration only")
        else {
            panic!("notification");
        };
        assert_eq!(cleared.params["diagnostics"], json!([fix]));
    }

    #[test]
    fn fatal_abandon_does_not_join_a_held_worker() {
        let mut worker = Worker::start();
        worker.shutdown();
        let (release, held) = unbounded::<()>();
        worker.thread = Some(std::thread::spawn(move || {
            let _ = held.recv();
        }));
        worker.abandon();
        assert!(worker.thread.is_none());
        release.send(()).expect("held thread still exists");
    }

    #[test]
    fn dictionary_mismatch_is_deduplicated_per_resolved_version_and_per_walk() {
        let text = "8=FIX.4.4|35=0|\n8=FIX.4.4|35=0|\n8=FIX.4.2|35=0|";
        let mut job = job(text, Arc::new(AtomicBool::new(false)), 10);
        let mut config = Diagnostics::default();
        config.rules.insert(
            "integrity".into(),
            crate::config::RuleSetting::Enabled(false),
        );
        job.snapshot.rules = Arc::new(Registry::with_selected_version(&config, Some("FIX.4.3")));
        for _ in 0..2 {
            let (sender, receiver) = unbounded();
            assert_eq!(walk(&job, &sender), 2);
            let found: Vec<_> = receiver
                .try_iter()
                .filter_map(|report| match report {
                    Report::Batch { found, .. } => Some(found),
                    _ => None,
                })
                .flatten()
                .collect();
            assert_eq!(found.len(), 2);
            assert_eq!(found[0].0, LineNo::new(0));
            assert_eq!(found[1].0, LineNo::new(2));
        }
        job.cap = 1;
        let (sender, receiver) = unbounded();
        assert_eq!(walk(&job, &sender), 2);
        let carried: usize = receiver
            .try_iter()
            .filter_map(|report| match report {
                Report::Batch { found, .. } => Some(found.len()),
                _ => None,
            })
            .sum();
        assert_eq!(carried, 1);
    }

    fn job(text: &str, cancel: Arc<AtomicBool>, cap: usize) -> Job {
        job_every(text, cancel, cap, PUBLISH_EVERY)
    }

    fn job_every(text: &str, cancel: Arc<AtomicBool>, cap: usize, every: Duration) -> Job {
        Job {
            uri: uri("a.fix"),
            generation: 1,
            snapshot: snapshot(text),
            cancel,
            cap,
            every,
        }
    }

    /// Deterministic where the end-to-end test cannot be: the flag is already
    /// set, so the walk must return without looking at a single line.
    #[test]
    fn a_cancelled_walk_reports_nothing() {
        let (tx, rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(true));
        let total = walk(&job(BAD, cancel, 2000), &tx);
        assert_eq!(total, 0);
        drop(tx);
        assert!(rx.try_iter().next().is_none(), "no batch was sent");
    }

    #[test]
    fn the_cap_bounds_what_is_carried_not_what_is_counted() {
        let (tx, rx) = unbounded();
        let lines = [BAD, BAD, BAD].join("\n");
        let total = walk(&job(&lines, Arc::new(AtomicBool::new(false)), 1), &tx);
        drop(tx);
        assert_eq!(total, 6, "two problems on each of three lines");
        let carried: usize = rx
            .try_iter()
            .map(|report| match report {
                Report::Batch { found, .. } => found.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(carried, 1, "only the cap is carried");
    }

    /// What the main loop ends up holding after a walk: its own `CappedFindings`, fed
    /// every batch in order.
    fn kept_after(job: &Job) -> (usize, Vec<(u32, crate::config::Severity)>) {
        let (tx, rx) = unbounded();
        let total = walk(job, &tx);
        drop(tx);
        let mut capped = CappedFindings::new(job.cap);
        for report in rx.try_iter() {
            if let Report::Batch { found, .. } = report {
                for (line, diag) in found {
                    capped.insert(line, diag);
                }
            }
        }
        let held = capped
            .in_line_order()
            .into_iter()
            .map(|(line, diag)| (line.raw(), diag.severity))
            .collect();
        (total, held)
    }

    /// Two fragments' `beginString` warnings fill the cap first; the errors
    /// further down still take it.
    #[test]
    fn a_less_severe_finding_never_keeps_an_error_out() {
        use crate::config::Severity::*;
        let lines = ["1=x|", "1=x|", BAD].join("\n");
        let (total, held) = kept_after(&job(&lines, Arc::new(AtomicBool::new(false)), 2));
        assert_eq!(total, 4);
        assert_eq!(held, [(2, Error), (2, Error)]);

        let (total, held) = kept_after(&job(&lines, Arc::new(AtomicBool::new(false)), 3));
        assert_eq!(total, 4);
        assert_eq!(
            held,
            [(0, Warning), (2, Error), (2, Error)],
            "the earlier warning stays"
        );
    }

    /// Two undefined tags on a line whose trailer is sound.
    fn venue_line() -> String {
        crate::rules::tests::repaired("8=FIX.4.4|9=0|35=0|9000=a|9000=b|10=000|")
    }

    fn with_rules(mut job: Job, config: &Diagnostics, selected: Option<&str>) -> Job {
        job.snapshot.rules = Arc::new(Registry::with_selected_version(config, selected));
        job
    }

    /// The cap fills with `unknownTag` findings, refuses the next one, counts
    /// the line after without rendering it, and still gives way to the errors.
    #[test]
    fn the_walk_counts_what_the_cap_refuses() {
        use crate::config::{RuleSetting, Severity::*};
        let venue = venue_line();
        let lines = [venue.as_str(), &venue, BAD, &venue].join("\n");
        let fresh = || Arc::new(AtomicBool::new(false));

        let (total, held) = kept_after(&job(&lines, fresh(), 3));
        assert_eq!(total, 8, "six unknown tags and two integrity errors");
        assert_eq!(held, [(0, Info), (2, Error), (2, Error)]);

        let mut config = Diagnostics::default();
        config.rules.insert(
            "unknownTag".into(),
            RuleSetting::Detailed {
                enabled: None,
                severity: Some(Warning),
                rest: Default::default(),
            },
        );
        let regraded = with_rules(job(&lines, fresh(), 3), &config, None);
        let (total, held) = kept_after(&regraded);
        assert_eq!(total, 8);
        assert_eq!(held, [(0, Warning), (2, Error), (2, Error)]);

        // A finding deduplicated per document is counted once, however late.
        let mismatched = with_rules(
            job(&lines, fresh(), 3),
            &Diagnostics::default(),
            Some("FIX.4.3"),
        );
        let (total, held) = kept_after(&mismatched);
        assert_eq!(total, 9, "plus one dictionaryVersion for the document");
        assert_eq!(held, [(0, Warning), (2, Error), (2, Error)]);
    }

    /// How often [`Rendering`] was asked to render rather than count.
    static RENDERED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// One `info` finding per line, and a count of the times it was rendered.
    struct Rendering;

    impl crate::rules::Rule for Rendering {
        fn id(&self) -> &'static str {
            "rendering"
        }
        fn precondition(&self) -> crate::rules::Precondition {
            crate::rules::Precondition::Fields
        }
        fn default_severity(&self) -> crate::config::Severity {
            crate::config::Severity::Info
        }
        fn check(
            &self,
            _ctx: &crate::rules::LineCtx,
            severity: crate::config::Severity,
        ) -> Vec<Diag> {
            RENDERED.fetch_add(1, Ordering::Relaxed);
            vec![Diag {
                code: self.id(),
                rule: self.id(),
                start: crate::codec::ByteOffset::new(0),
                end: crate::codec::ByteOffset::new(1),
                message: String::new(),
                severity,
                document_key: None,
            }]
        }
        fn count(&self, _ctx: &crate::rules::LineCtx) -> Option<usize> {
            Some(1)
        }
    }

    /// Once the cap holds two `info` findings, the next three lines are counted
    /// and never rendered.
    #[test]
    fn the_walk_stops_rendering_what_the_cap_refuses() {
        let lines = ["55=X|"; 5].join("\n");
        let mut job = job(&lines, Arc::new(AtomicBool::new(false)), 2);
        job.snapshot.rules = Arc::new(Registry::of_rules(
            vec![(&Rendering, crate::config::Severity::Info)],
            None,
        ));
        let (total, held) = kept_after(&job);
        assert_eq!(total, 5);
        assert_eq!(held.len(), 2);
        assert_eq!(RENDERED.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn more_errors_than_the_cap_keep_the_first_in_line_order() {
        use crate::config::Severity::*;
        let lines = [BAD, BAD, BAD].join("\n");
        let (total, held) = kept_after(&job(&lines, Arc::new(AtomicBool::new(false)), 3));
        assert_eq!(total, 6);
        assert_eq!(held, [(0, Error), (0, Error), (1, Error)]);
    }

    /// The guarantee the `Drop` exists for: every begun sequence is ended, on
    /// whatever path the `Inflight` holding it goes away. Asserted here rather
    /// than end to end, where a short walk finishes before the client answers
    /// the `create` and the assertion would hold vacuously.
    #[test]
    fn dropping_a_begun_progress_ends_it() {
        let (tx, rx) = unbounded::<lsp_server::Message>();
        let tx = crate::transport::Sink::Memory(tx);
        let progress = Progress::begin(&tx, token_for(7), 0);
        let begun = rx.try_recv().expect("a begin");
        drop(progress);
        let ended = rx.try_recv().expect("an end");

        for (message, kind) in [(begun, "begin"), (ended, "end")] {
            let Message::Notification(note) = message else {
                panic!("progress is a notification");
            };
            assert_eq!(note.method, "$/progress");
            assert_eq!(note.params["token"], "fix/diagnostics/7");
            assert_eq!(note.params["value"]["kind"], kind);
        }
        assert!(rx.try_recv().is_err(), "and nothing else");
    }

    #[test]
    fn a_percentage_stays_inside_what_lsp_accepts() {
        assert_eq!(percent(0, 0), 100, "an empty document is finished");
        assert_eq!(percent(0, 4), 0);
        assert_eq!(percent(1, 4), 25);
        assert_eq!(percent(4, 4), 100);
        assert_eq!(percent(9, 4), 100, "never over 100");
        assert_eq!(percent(1, 3), 33, "truncated, not rounded up past the end");
    }

    #[test]
    fn a_report_carries_the_percentage_under_the_agreed_token() {
        let (tx, rx) = unbounded::<lsp_server::Message>();
        let tx = crate::transport::Sink::Memory(tx);
        let progress = Progress::begin(&tx, token_for(3), 0);
        let _ = rx.try_recv();
        progress.report(42);
        let Ok(Message::Notification(note)) = rx.try_recv() else {
            panic!("a report");
        };
        assert_eq!(note.params["token"], "fix/diagnostics/3");
        assert_eq!(note.params["value"]["kind"], "report");
        assert_eq!(note.params["value"]["percentage"], 42);
    }

    /// A clean document finds nothing, so nothing would be batched — and the
    /// bar would sit at zero for the whole walk if progress rode on batches.
    #[test]
    fn a_walk_reports_progress_even_when_it_finds_nothing() {
        let (tx, rx) = unbounded();
        let sound = "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|\
52=20251026-09:02:00.000|10=171|";
        let lines = vec![sound; 200].join("\n");
        // Report on every line: whether a walk outlives the production cadence
        // depends on the machine and the build profile, and that is not what
        // this test is about.
        let job = job_every(&lines, Arc::new(AtomicBool::new(false)), 10, Duration::ZERO);
        let total = walk(&job, &tx);
        drop(tx);
        assert_eq!(total, 0, "the fixture is sound");
        let reported: Vec<u32> = rx
            .try_iter()
            .filter_map(|report| match report {
                Report::Progressed { percentage, .. } => Some(percentage),
                _ => None,
            })
            .collect();
        assert_eq!(reported.len(), 200, "one per line at this cadence");
        assert_eq!(reported.last(), Some(&100), "and it ends at the end");
        assert!(reported.is_sorted(), "monotonic: {reported:?}");
    }

    /// Agreeing to a token is not announcing anything. The bar opens on the
    /// walk's first report — the first moment it has lasted long enough to be
    /// worth one — and it opens at the percentage already reached rather than
    /// at zero.
    #[test]
    fn the_bar_opens_on_the_first_report_and_not_before() {
        let (sender, receiver) = unbounded::<lsp_server::Message>();
        let mut state = ServerState::new(sender);
        let doc = uri("a.fix");
        state.docs.insert(
            doc.clone(),
            Document::open(BAD.as_bytes(), 1, Ctx::for_test("FIX.4.4")),
        );
        state.configure_open_document(&doc);
        super::super::testing::settle(&mut state);
        for _ in receiver.try_iter() {}
        let snapshot = state.snapshot_for(&doc).expect("open");
        state.diagnostics.submit(doc.clone(), snapshot, 10);
        let generation = state.diagnostics.inflight[&doc].generation;

        state.on_progress_created(&doc, generation, true);
        assert!(
            receiver.try_recv().is_err(),
            "agreeing to a token says nothing on its own"
        );

        let progressed = |percentage| Report::Progressed {
            uri: doc.clone(),
            generation,
            percentage,
        };
        state.on_diagnostic_report(progressed(40));
        state.on_diagnostic_report(progressed(80));

        let kinds: Vec<(String, u64)> = receiver
            .try_iter()
            .filter_map(|message| match message {
                Message::Notification(note) if note.method == crate::server::method::PROGRESS => {
                    Some((
                        note.params["value"]["kind"]
                            .as_str()
                            .unwrap_or("?")
                            .to_owned(),
                        note.params["value"]["percentage"].as_u64().unwrap_or(0),
                    ))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            vec![("begin".to_owned(), 40), ("report".to_owned(), 80)]
        );
    }

    /// The token is per generation, so a `create` reply that arrives after the
    /// walk was replaced cannot begin progress for the one that replaced it.
    #[test]
    fn each_walk_proposes_its_own_token() {
        assert_ne!(token_for(1), token_for(2));
    }

    /// Generation answers "is this the walk we last asked for". It does not
    /// answer "is it still about the text we have": an edit inside the debounce
    /// window leaves the generation current while the text moves underneath it,
    /// and publishing then tags the previous text with the previous version —
    /// which a client has no reason to reject.
    #[test]
    fn a_walk_about_text_that_has_moved_is_not_accepted() {
        let (sender, receiver) = unbounded::<lsp_server::Message>();
        let mut state = ServerState::new(sender);
        let doc = uri("a.fix");
        state.docs.insert(
            doc.clone(),
            Document::open(BAD.as_bytes(), 1, Ctx::for_test("FIX.4.4")),
        );
        state.configure_open_document(&doc);
        super::super::testing::settle(&mut state);
        for _ in receiver.try_iter() {}
        let snapshot = state.snapshot_for(&doc).expect("open");
        state.diagnostics.submit(doc.clone(), snapshot, 10);
        let generation = state.diagnostics.inflight[&doc].generation;
        assert!(state.accept(&doc, generation), "before the edit");

        state
            .docs
            .get_mut(&doc)
            .expect("open")
            .apply_changes(
                &[crate::document::Change::Full(b"8=FIX.4.4|35=0".to_vec())],
                2,
                crate::document::PositionEncoding::Utf8,
            )
            .expect("applies");
        assert!(
            !state.accept(&doc, generation),
            "the walk is about text nobody has any more"
        );
    }

    /// A document dirtied twice before the debounce elapses is walked once, at
    /// the version it ended up on.
    #[test]
    fn dirtying_twice_dispatches_once() {
        let mut worker = Worker::start();
        let doc = uri("a.fix");
        worker.mark_dirty(&doc, 1);
        worker.mark_dirty(&doc, 2);
        assert_eq!(worker.dirty.len(), 1);
        std::thread::sleep(DEBOUNCE);
        assert_eq!(worker.due(), vec![(doc, 2)]);
        assert!(worker.dirty.is_empty());
    }

    #[test]
    fn nothing_is_due_before_the_debounce_elapses() {
        let mut worker = Worker::start();
        worker.mark_dirty(&uri("a.fix"), 1);
        assert!(worker.due().is_empty());
        assert!(worker.tick_in() <= DEBOUNCE);
    }

    #[test]
    fn an_idle_scheduler_does_not_ask_to_be_woken() {
        let worker = Worker::start();
        assert_eq!(worker.tick_in(), IDLE);
    }

    /// Closing forgets the document *and* stops the walk that is running for
    /// it; otherwise its batches would arrive after the marks were cleared.
    #[test]
    fn forgetting_a_document_cancels_its_walk() {
        let mut worker = Worker::start();
        let doc = uri("a.fix");
        worker.submit(doc.clone(), snapshot(BAD), 10);
        let cancel = worker.inflight[&doc].cancel.clone();
        worker.forget(&doc);
        assert!(cancel.load(Ordering::Relaxed));
        assert!(!worker.inflight.contains_key(&doc));
    }

    #[test]
    fn a_second_submission_cancels_the_first() {
        let mut worker = Worker::start();
        let doc = uri("a.fix");
        worker.submit(doc.clone(), snapshot(BAD), 10);
        let first = worker.inflight[&doc].cancel.clone();
        let first_generation = worker.inflight[&doc].generation;
        worker.submit(doc.clone(), snapshot(BAD), 10);
        assert!(first.load(Ordering::Relaxed), "the older walk is stopped");
        assert!(worker.inflight[&doc].generation > first_generation);
    }
}
