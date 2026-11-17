//! One background request worker; only the main loop delivers its results.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use lsp_server::{RequestId, ResponseError};
use lsp_types::{ProgressToken, Uri};
use serde_json::json;

use super::dispatch::{
    Answer, BackgroundEvent, BackgroundResult, BackgroundRun, BackgroundTask, Snapshot,
};
use super::progress::Progress;
use super::ServerState;
use crate::transport::{Encoded, Outbound, WriteJson};

/// Most reports one loop pass delivers, so a fast worker cannot starve requests.
const DRAIN_REPORTS: usize = 32;
const DRAIN_TIME: Duration = Duration::from_millis(2);
/// How often a queued job says that it still waits. A client with an
/// inactivity deadline reads silence as a server that stopped.
const WAITING_INTERVAL: Duration = Duration::from_secs(1);

struct Job {
    generation: u64,
    epoch: Instant,
    method: &'static str,
    snapshot: Snapshot,
    run: BackgroundRun,
    /// Pending server-created tokens also need reports, to catch up after approval.
    progress: bool,
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct QueueState {
    jobs: VecDeque<Job>,
    stopped: bool,
}

/// Not a channel: `discard` frees a queued job's `Snapshot` at once, so a
/// cancelled request does not pin an old document while another job runs.
#[derive(Default)]
struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

impl Queue {
    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, job: Job) {
        self.lock().jobs.push_back(job);
        self.ready.notify_one();
    }

    fn discard(&self, generation: u64) {
        self.lock().jobs.retain(|job| job.generation != generation);
    }

    fn stop(&self) {
        let mut state = self.lock();
        state.stopped = true;
        state.jobs.clear();
        self.ready.notify_one();
    }

    /// Waits for a job; `None` once the queue is stopped.
    fn pop(&self) -> Option<Job> {
        let mut state = self.lock();
        loop {
            if state.stopped {
                return None;
            }
            if let Some(job) = state.jobs.pop_front() {
                return Some(job);
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

pub(super) struct Report {
    generation: u64,
    event: Event,
}

enum Event {
    Started,
    Emitted(BackgroundEvent),
    /// The outer error is the message of a contained panic.
    Finished(Result<BackgroundResult, String>),
}

struct Active {
    id: RequestId,
    method: &'static str,
    uri: Uri,
    lifetime: Option<u64>,
    version: i32,
    repair_configuration: Option<RepairConfiguration>,
    partial: Option<ProgressToken>,
    progress: BackgroundProgress,
    cancelled: Arc<AtomicBool>,
    /// False while the job is in the queue.
    running: bool,
    /// When the client last heard that the job waits.
    waiting_reported: Instant,
}

enum BackgroundProgress {
    None,
    Pending {
        token: ProgressToken,
        title: &'static str,
        percentage: u32,
        phase: &'static str,
    },
    Active(Progress),
}

impl BackgroundProgress {
    fn token(&self) -> Option<&ProgressToken> {
        match self {
            Self::None => None,
            Self::Pending { token, .. } => Some(token),
            Self::Active(progress) => Some(progress.token()),
        }
    }
}

struct RepairConfiguration {
    repair_generation: u64,
    range_lines: Option<usize>,
}

impl RepairConfiguration {
    fn matches(&self, state: &ServerState, uri: &Uri) -> bool {
        state.config.documents.get(uri).is_some_and(|document| {
            self.repair_generation == document.repair_generation
                && self
                    .range_lines
                    .is_none_or(|limit| limit == document.resolved.config.limits.range_lines)
        })
    }
}

/// `$/progress` params around a value its producer already encoded.
struct PartialResult {
    token: ProgressToken,
    value: Vec<u8>,
}

impl WriteJson for PartialResult {
    fn write_json(&self, out: &mut Vec<u8>) -> serde_json::Result<()> {
        out.extend_from_slice(br#"{"token":"#);
        serde_json::to_writer(&mut *out, &self.token)?;
        out.extend_from_slice(br#","value":"#);
        out.extend_from_slice(&self.value);
        out.push(b'}');
        Ok(())
    }
}

pub(super) struct Worker {
    epoch: Instant,
    queue: Arc<Queue>,
    pub reports: Receiver<Report>,
    active: HashMap<u64, Active>,
    next_generation: u64,
}

impl Worker {
    pub fn start() -> Self {
        let queue = Arc::new(Queue::default());
        let (sender, reports) = unbounded();
        let work = queue.clone();
        std::thread::Builder::new()
            .name("venue-reader-background".into())
            .spawn(move || run(&work, &sender))
            .expect("spawning the background worker");
        Self {
            epoch: Instant::now(),
            queue,
            reports,
            active: HashMap::new(),
            next_generation: 0,
        }
    }

    pub fn contains(&self, id: &RequestId) -> bool {
        self.active.values().any(|active| &active.id == id)
    }

    /// Time until the next waiting report is due.
    pub fn tick_in(&self, now: Instant) -> Duration {
        self.active
            .values()
            .filter(|active| {
                !active.running && matches!(active.progress, BackgroundProgress::Active(_))
            })
            .map(|active| {
                WAITING_INTERVAL.saturating_sub(now.duration_since(active.waiting_reported))
            })
            .min()
            .unwrap_or(Duration::from_secs(3600))
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        for active in self.active.values() {
            active.cancelled.store(true, Ordering::Relaxed);
        }
        self.queue.stop();
    }
}

fn run(queue: &Queue, sender: &Sender<Report>) {
    while let Some(job) = queue.pop() {
        if job.cancelled.load(Ordering::Relaxed) {
            continue;
        }
        let Job {
            generation,
            epoch,
            method,
            snapshot,
            run,
            progress,
            cancelled,
        } = job;
        let report = |event| sender.send(Report { generation, event });
        log::debug!(
            "background_job generation={generation} event=started clock_us={}",
            epoch.elapsed().as_micros()
        );
        if report(Event::Started).is_err() {
            return;
        }
        let result = super::catch_handler_panic(method, || {
            run(&snapshot, &cancelled, &mut |event| {
                if progress || matches!(event, BackgroundEvent::Chunk(_)) {
                    let _ = report(Event::Emitted(event));
                }
            })
        });
        log::debug!(
            "background_job generation={generation} event=finished clock_us={} cancelled={} error_code={}",
            epoch.elapsed().as_micros(),
            cancelled.load(Ordering::Relaxed),
            match &result {
                Ok(Err(error)) => error.code,
                _ => 0,
            },
        );
        if report(Event::Finished(result)).is_err() {
            return;
        }
    }
}

impl ServerState {
    pub(super) fn start_background(&mut self, id: RequestId, task: BackgroundTask) {
        let BackgroundTask {
            method,
            title,
            uri,
            snapshot,
            work_done,
            partial,
            repair_range_lines,
            run,
        } = task;
        self.background.next_generation += 1;
        let generation = self.background.next_generation;
        let cancelled = Arc::new(AtomicBool::new(false));
        let repair = matches!(
            method,
            super::method::CODE_ACTION | super::method::CODE_ACTION_RESOLVE
        );
        let progress = if let Some(token) = work_done {
            BackgroundProgress::Active(Progress::begin(
                &self.sender,
                token,
                title,
                0,
                Some("Queued"),
                true,
            ))
        } else if repair && self.client.progress {
            BackgroundProgress::Pending {
                token: ProgressToken::String(format!("venue-reader-repair-{generation}")),
                title,
                percentage: 0,
                phase: "Queued",
            }
        } else {
            BackgroundProgress::None
        };
        let create_progress = match &progress {
            BackgroundProgress::Pending { token, .. } => Some(self.req_queue.outgoing.register(
                super::method::PROGRESS_CREATE.into(),
                json!({"token": token}),
                super::OutgoingRequest::BackgroundProgressCreate { generation },
            )),
            _ => None,
        };
        let repair_configuration = repair.then(|| RepairConfiguration {
            // Snapshot creation and admission share one main-loop turn.
            repair_generation: self
                .config
                .documents
                .get(&uri)
                .expect("admitted repair has committed configuration")
                .repair_generation,
            range_lines: repair_range_lines,
        });
        let job = Job {
            generation,
            epoch: self.background.epoch,
            method,
            run,
            progress: progress.token().is_some(),
            cancelled: cancelled.clone(),
            snapshot,
        };
        self.background.active.insert(
            generation,
            Active {
                id,
                method,
                lifetime: self.config.lifetimes.get(&uri).copied(),
                uri,
                version: job.snapshot.version,
                repair_configuration,
                partial,
                progress,
                cancelled,
                running: false,
                waiting_reported: Instant::now(),
            },
        );
        log::debug!(
            "background_job generation={generation} event=admitted clock_us={}",
            self.background.epoch.elapsed().as_micros()
        );
        if let Some(request) = create_progress {
            self.sender.send(request);
        }
        self.background.queue.push(job);
    }

    pub(super) fn on_background_progress_created(&mut self, generation: u64, accepted: bool) {
        let Some(active) = self.background.active.get_mut(&generation) else {
            return;
        };
        let BackgroundProgress::Pending {
            token,
            title,
            percentage,
            phase,
        } = &active.progress
        else {
            return;
        };
        active.progress = if accepted {
            BackgroundProgress::Active(Progress::begin(
                &self.sender,
                token.clone(),
                title,
                *percentage,
                Some(phase),
                true,
            ))
        } else {
            BackgroundProgress::None
        };
    }

    /// Deliver `first` and whatever else is ready, within the drain bounds.
    pub(super) fn drain_background(&mut self, first: Report) {
        let started = Instant::now();
        self.on_background_report(first);
        for _ in 1..DRAIN_REPORTS {
            if started.elapsed() >= DRAIN_TIME {
                break;
            }
            let Ok(report) = self.background.reports.try_recv() else {
                break;
            };
            self.on_background_report(report);
        }
    }

    fn on_background_report(&mut self, report: Report) {
        let generation = report.generation;
        let Some(active) = self.background.active.get(&generation) else {
            return;
        };
        let valid = self.config.lifetimes.get(&active.uri) == active.lifetime.as_ref()
            && self
                .docs
                .get(&active.uri)
                .is_some_and(|doc| doc.version == active.version);
        if !valid {
            let error = super::content_modified(active.method, "the document has changed");
            self.fail_background(generation, "invalidated", error);
            return;
        }
        match report.event {
            Event::Started => {
                if let Some(active) = self.background.active.get_mut(&generation) {
                    active.running = true;
                }
            }
            Event::Emitted(BackgroundEvent::Chunk(value)) => {
                if let Some(token) = &active.partial {
                    self.sender.send(Outbound::Notification {
                        method: super::method::PROGRESS,
                        params: Box::new(PartialResult {
                            token: token.clone(),
                            value,
                        }),
                    });
                }
            }
            Event::Emitted(BackgroundEvent::Progress { percentage, phase }) => {
                let active = self
                    .background
                    .active
                    .get_mut(&generation)
                    .expect("active job");
                match &mut active.progress {
                    BackgroundProgress::None => {}
                    BackgroundProgress::Pending {
                        percentage: pending_percentage,
                        phase: pending_phase,
                        ..
                    } => {
                        *pending_percentage = percentage;
                        *pending_phase = phase;
                    }
                    BackgroundProgress::Active(progress) => {
                        progress.report(percentage, Some(phase));
                    }
                }
            }
            Event::Finished(result) => {
                let active = self
                    .background
                    .active
                    .remove(&generation)
                    .expect("active job");
                // The `end` goes out before the response it belongs to.
                if let BackgroundProgress::Active(progress) = active.progress {
                    progress.finish(if matches!(result, Ok(Ok(_))) {
                        "Finished"
                    } else {
                        "Failed"
                    });
                }
                self.complete_request(
                    active.id,
                    result.map(|handled| {
                        handled.map(|json| Some(Answer::Lazy(Box::new(Encoded(json)))))
                    }),
                );
            }
        }
    }

    /// Tell the client about each job that still waits in the queue.
    pub(super) fn report_waiting_background(&mut self, now: Instant) {
        for active in self.background.active.values_mut() {
            if !active.running && now.duration_since(active.waiting_reported) >= WAITING_INTERVAL {
                active.waiting_reported = now;
                if let BackgroundProgress::Active(progress) = &active.progress {
                    progress.report(0, Some("Queued"));
                }
            }
        }
    }

    /// Take a job out of service: no further event of it reaches the client.
    fn retire(&mut self, generation: u64, event: &str, message: &str) -> Option<Active> {
        let mut active = self.background.active.remove(&generation)?;
        active.cancelled.store(true, Ordering::Relaxed);
        log::debug!(
            "background_job generation={generation} event={event} clock_us={}",
            self.background.epoch.elapsed().as_micros()
        );
        self.background.queue.discard(generation);
        if let BackgroundProgress::Active(progress) =
            std::mem::replace(&mut active.progress, BackgroundProgress::None)
        {
            progress.finish(message);
        }
        Some(active)
    }

    fn fail_background(&mut self, generation: u64, event: &str, error: ResponseError) {
        if let Some(active) = self.retire(generation, event, &error.message) {
            self.complete_request(active.id, Ok(Err(error)));
        }
    }

    pub(super) fn invalidate_background(&mut self, uri: &Uri) {
        self.invalidate_background_where("the document has changed or closed", |_, active| {
            &active.uri == uri
        });
    }

    pub(super) fn invalidate_background_repairs(&mut self, uri: &Uri) {
        self.invalidate_background_where(
            "the repair configuration has changed",
            |state, active| {
                &active.uri == uri
                    && active
                        .repair_configuration
                        .as_ref()
                        .is_some_and(|configuration| !configuration.matches(state, uri))
            },
        );
    }

    fn invalidate_background_where(
        &mut self,
        reason: &str,
        predicate: impl Fn(&Self, &Active) -> bool,
    ) {
        let jobs: Vec<_> = self
            .background
            .active
            .iter()
            .filter(|(_, active)| predicate(self, active))
            .map(|(&generation, active)| (generation, active.method))
            .collect();
        for (generation, method) in jobs {
            let error = super::content_modified(method, reason);
            self.fail_background(generation, "invalidated", error);
        }
    }

    pub(super) fn cancel_background(&mut self, id: &RequestId) {
        let generation = self
            .background
            .active
            .iter()
            .find_map(|(&generation, active)| (&active.id == id).then_some(generation));
        if let Some(generation) = generation {
            self.retire(generation, "cancelled", "Cancelled");
        }
    }

    pub(super) fn cancel_background_progress(&mut self, token: ProgressToken) {
        let id = self
            .background
            .active
            .values()
            .find(|active| active.progress.token() == Some(&token))
            .map(|active| active.id.clone());
        if let Some(id) = id {
            self.cancel_request(id);
        }
    }

    pub(super) fn stop_background(&mut self, message: &str) {
        let generations: Vec<_> = self.background.active.keys().copied().collect();
        for generation in generations {
            let error = super::config_pull::request_failed(message);
            self.fail_background(generation, "aborted", error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{method, testing};
    use super::*;
    use lsp_server::{ErrorCode, Message, Notification, Request, Response};
    use serde_json::{json, Value};

    const WAIT: Duration = Duration::from_secs(2);

    fn state() -> (ServerState, Uri, Receiver<Message>) {
        let (mut state, uri) = testing::state_with(testing::HEARTBEAT);
        let (sender, receiver) = unbounded();
        state.sender = sender.into();
        (state, uri, receiver)
    }

    /// A real `textDocument/documentSymbol` request.
    fn request(state: &mut ServerState, uri: &Uri, id: i32, partial: bool) {
        let mut params = json!({"textDocument":{"uri":uri},"workDoneToken":format!("work-{id}")});
        if partial {
            params["partialResultToken"] = json!(format!("partial-{id}"));
        }
        state.on_request(Request::new(
            id.into(),
            method::DOCUMENT_SYMBOL.into(),
            params,
        ));
    }

    /// Admit a task as request `id`, with the same tokens `request` uses.
    fn admit(state: &mut ServerState, uri: &Uri, id: i32, partial: bool, run: BackgroundRun) {
        admit_method(state, uri, id, partial, method::DOCUMENT_SYMBOL, run);
    }

    fn admit_method(
        state: &mut ServerState,
        uri: &Uri,
        id: i32,
        partial: bool,
        method: &'static str,
        run: BackgroundRun,
    ) {
        state.req_queue.incoming.register(id.into(), ());
        let task = BackgroundTask {
            method,
            title: "Document symbols",
            uri: uri.clone(),
            snapshot: state.snapshot_for(uri).expect("an open document"),
            work_done: Some(ProgressToken::String(format!("work-{id}"))),
            partial: partial.then(|| ProgressToken::String(format!("partial-{id}"))),
            repair_range_lines: None,
            run,
        };
        state.start_background(id.into(), task);
    }

    /// Admit a task that the worker enters and then holds until the gate opens.
    fn hold(state: &mut ServerState, uri: &Uri, id: i32, partial: bool) -> Sender<()> {
        hold_method(state, uri, id, partial, method::DOCUMENT_SYMBOL)
    }

    fn hold_method(
        state: &mut ServerState,
        uri: &Uri,
        id: i32,
        partial: bool,
        method: &'static str,
    ) -> Sender<()> {
        let (gate, opened) = unbounded::<()>();
        let (entered, running) = unbounded();
        admit_method(
            state,
            uri,
            id,
            partial,
            method,
            Box::new(move |_, _, emit| {
                let _ = entered.send(());
                let _ = opened.recv();
                emit(BackgroundEvent::Chunk(b"[]".to_vec()));
                Ok(b"[]".to_vec())
            }),
        );
        running.recv_timeout(WAIT).expect("worker starts job");
        assert!(!deliver(state), "the start report");
        gate
    }

    /// Deliver the next report; true when it was the job's last.
    fn deliver(state: &mut ServerState) -> bool {
        let report = state
            .background
            .reports
            .recv_timeout(WAIT)
            .expect("a worker report");
        let last = matches!(report.event, Event::Finished(_));
        state.on_background_report(report);
        last
    }

    fn finish(state: &mut ServerState) {
        while !state.background.active.is_empty() {
            deliver(state);
        }
    }

    fn messages(receiver: &Receiver<Message>) -> Vec<Message> {
        receiver.try_iter().collect()
    }

    /// Positions of the `$/progress` notifications of one kind under a token.
    fn progress<'a>(
        messages: &'a [Message],
        token: &'a str,
        kind: &'a str,
    ) -> impl Iterator<Item = usize> + 'a {
        messages
            .iter()
            .enumerate()
            .filter_map(move |(position, message)| match message {
                Message::Notification(note)
                    if note.method == method::PROGRESS
                        && note.params["token"] == token
                        && note.params["value"]["kind"] == kind =>
                {
                    Some(position)
                }
                _ => None,
            })
    }

    fn queued(state: &ServerState) -> usize {
        state.background.queue.lock().jobs.len()
    }

    fn hold_repair(
        state: &mut ServerState,
        uri: &Uri,
        id: i32,
        method: &'static str,
        outcome: &'static str,
    ) -> Sender<()> {
        let (gate, opened) = unbounded();
        let snapshot = state.snapshot_for(uri).unwrap();
        state.req_queue.incoming.register(id.into(), ());
        state.start_background(
            id.into(),
            BackgroundTask {
                method,
                title: "Fix all",
                uri: uri.clone(),
                snapshot,
                work_done: None,
                partial: None,
                repair_range_lines: None,
                run: Box::new(move |_, _, emit| {
                    for percentage in [10, 40] {
                        emit(BackgroundEvent::Progress {
                            percentage,
                            phase: "Repairing",
                        });
                    }
                    opened.recv().unwrap();
                    emit(BackgroundEvent::Progress {
                        percentage: 80,
                        phase: "Encoding",
                    });
                    match outcome {
                        "panic" => panic!("repair worker fault injection"),
                        "failed" => Err(super::super::config_pull::request_failed("failed")),
                        _ => Ok(b"[]".to_vec()),
                    }
                }),
            },
        );
        assert!(!deliver(state), "worker starts without a create reply");
        if state.client.progress {
            assert!(!deliver(state));
            assert!(!deliver(state));
        }
        gate
    }

    fn create_request(messages: &[Message]) -> Request {
        let requests: Vec<_> = messages
            .iter()
            .filter_map(|message| match message {
                Message::Request(request) if request.method == "window/workDoneProgress/create" => {
                    Some(request.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(requests.len(), 1);
        requests[0].clone()
    }

    fn response_code(messages: &[Message], id: i32, code: Option<i32>) -> usize {
        let responses: Vec<_> = messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| match message {
                Message::Response(response) if response.id == id.into() => Some((index, response)),
                _ => None,
            })
            .collect();
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0]
                .1
                .response_result
                .as_ref()
                .err()
                .map(|error| error.code),
            code
        );
        responses[0].0
    }

    #[test]
    fn repair_progress_creation_does_not_block_work_or_other_requests() {
        for method in [method::CODE_ACTION, method::CODE_ACTION_RESOLVE] {
            let (mut state, uri, receiver) = state();
            state.client.progress = true;
            let gate = hold_repair(&mut state, &uri, 70, method, "finished");
            let received = messages(&receiver);
            let create = create_request(&received);
            let token = create.params["token"].as_str().unwrap();
            assert!(!received
                .iter()
                .any(|message| matches!(message, Message::Notification(_))));
            state.on_request(Request::new(
                71.into(),
                "textDocument/hover".into(),
                json!({"textDocument":{"uri":uri},"position":{"line":0,"character":0}}),
            ));
            response_code(&messages(&receiver), 71, None);
            state.on_response(Response::new_ok(create.id.clone(), Value::Null));
            let mut received = messages(&receiver);
            let begun = progress(&received, token, "begin").collect::<Vec<_>>();
            assert_eq!(begun.len(), 1);
            let Message::Notification(begin) = &received[begun[0]] else {
                unreachable!()
            };
            assert_eq!(begin.params["value"]["title"], "Fix all");
            assert_eq!(begin.params["value"]["percentage"], 40);
            assert_eq!(begin.params["value"]["message"], "Repairing");
            assert_eq!(begin.params["value"]["cancellable"], true);
            gate.send(()).unwrap();
            finish(&mut state);
            received.extend(messages(&receiver));
            assert_eq!(progress(&received, token, "report").count(), 1);
            let ends = progress(&received, token, "end").collect::<Vec<_>>();
            assert_eq!(ends.len(), 1);
            assert!(ends[0] < response_code(&received, 70, None));
        }
    }

    #[test]
    fn declined_or_unsupported_repair_progress_still_completes_without_notifications() {
        for supported in [true, false] {
            let (mut state, uri, receiver) = state();
            state.client.progress = supported;
            let gate = hold_repair(
                &mut state,
                &uri,
                72,
                method::CODE_ACTION_RESOLVE,
                "finished",
            );
            let received = messages(&receiver);
            if supported {
                let create = create_request(&received);
                state.on_response(Response::new_err(create.id, -32601, "unsupported".into()));
            } else {
                assert!(received.is_empty());
            }
            gate.send(()).unwrap();
            finish(&mut state);
            let received = messages(&receiver);
            response_code(&received, 72, None);
            assert!(!received
                .iter()
                .any(|message| matches!(message, Message::Notification(_))));
        }
    }

    #[test]
    fn repair_progress_ends_on_every_terminal_path_and_ignores_late_creation() {
        for acknowledge in [true, false] {
            for outcome in ["finished", "failed", "panic", "cancel", "edit", "shutdown"] {
                let (mut state, uri, receiver) = state();
                state.client.progress = true;
                let gate = hold_repair(&mut state, &uri, 73, method::CODE_ACTION_RESOLVE, outcome);
                let create = create_request(&messages(&receiver));
                let token = create.params["token"].as_str().unwrap();
                if acknowledge {
                    state.on_response(Response::new_ok(create.id.clone(), Value::Null));
                }
                let code = match outcome {
                    "cancel" => {
                        state.on_notification(Notification::new(
                            "window/workDoneProgress/cancel".into(),
                            json!({"token": token}),
                        ));
                        Some(-32800)
                    }
                    "edit" => {
                        state.on_notification(Notification::new(
                            "textDocument/didChange".into(),
                            json!({"textDocument":{"uri":uri,"version":2},
                                "contentChanges":[{"text":testing::HEARTBEAT}]}),
                        ));
                        Some(-32801)
                    }
                    "shutdown" => {
                        state.on_request(Request::new(74.into(), "shutdown".into(), Value::Null));
                        Some(-32803)
                    }
                    "panic" => Some(-32603),
                    "failed" => Some(-32803),
                    _ => None,
                };
                gate.send(()).unwrap();
                while !deliver(&mut state) {}
                let mut received = messages(&receiver);
                let replied = response_code(&received, 73, code);
                if acknowledge {
                    assert_eq!(progress(&received, token, "begin").count(), 1);
                    let ends = progress(&received, token, "end").collect::<Vec<_>>();
                    assert_eq!(ends.len(), 1);
                    assert!(ends[0] < replied);
                } else {
                    state.on_response(Response::new_ok(create.id.clone(), Value::Null));
                    received.extend(messages(&receiver));
                    assert_eq!(progress(&received, token, "begin").count(), 0);
                    assert_eq!(progress(&received, token, "end").count(), 0);
                }
                assert!(!state.background.contains(&73.into()));
            }
        }
    }

    #[test]
    fn progress_cancellation_releases_queued_repairs_and_accepts_supplied_tokens() {
        let (mut state, uri, receiver) = state();
        state.client.progress = true;
        let gate = hold(&mut state, &uri, 75, false);
        let snapshot = state.snapshot_for(&uri).unwrap();
        state.req_queue.incoming.register(76.into(), ());
        state.start_background(
            76.into(),
            BackgroundTask {
                method: method::CODE_ACTION_RESOLVE,
                title: "Fix all",
                uri: uri.clone(),
                snapshot,
                work_done: None,
                partial: None,
                repair_range_lines: None,
                run: Box::new(|_, _, _| panic!("cancelled queued repair must not run")),
            },
        );
        let create = create_request(&messages(&receiver));
        let token = create.params["token"].as_str().unwrap();
        state.on_response(Response::new_ok(create.id.clone(), Value::Null));
        state.report_waiting_background(Instant::now() + WAITING_INTERVAL);
        state.on_notification(Notification::new(
            "window/workDoneProgress/cancel".into(),
            json!({"token":"unknown"}),
        ));
        assert_eq!(queued(&state), 1);
        state.on_notification(Notification::new(
            "window/workDoneProgress/cancel".into(),
            json!({"token":token}),
        ));
        let received = messages(&receiver);
        assert_eq!(queued(&state), 0);
        assert_eq!(progress(&received, token, "begin").count(), 1);
        assert_eq!(progress(&received, token, "report").count(), 1);
        let ends = progress(&received, token, "end").collect::<Vec<_>>();
        assert_eq!(ends.len(), 1);
        assert!(ends[0] < response_code(&received, 76, Some(-32800)));
        state.on_notification(Notification::new(
            "window/workDoneProgress/cancel".into(),
            json!({"token":"work-75"}),
        ));
        terminal(&messages(&receiver), 75, Some(-32800));
        gate.send(()).unwrap();
        while !deliver(&mut state) {}
        assert!(messages(&receiver).is_empty());
    }

    /// Exactly one response with this outcome, after exactly one progress `end`.
    fn terminal(messages: &[Message], id: i32, code: Option<i32>) {
        let replies: Vec<_> = messages
            .iter()
            .enumerate()
            .filter_map(|(position, message)| match message {
                Message::Response(response) if response.id == id.into() => {
                    Some((position, response))
                }
                _ => None,
            })
            .collect();
        assert_eq!(replies.len(), 1);
        let (replied, response) = replies[0];
        assert_eq!(
            response
                .response_result
                .as_ref()
                .err()
                .map(|error| error.code),
            code
        );
        let ends: Vec<_> = progress(messages, &format!("work-{id}"), "end").collect();
        assert_eq!(ends.len(), 1);
        assert!(ends[0] < replied, "end precedes terminal response");
    }

    #[test]
    fn explicit_cancellation_ends_progress_and_suppresses_late_events() {
        let (mut state, uri, receiver) = state();
        let gate = hold(&mut state, &uri, 1, true);
        state.on_cancel(json!({"id":1}));
        let mut received = messages(&receiver);
        terminal(&received, 1, Some(-32800));
        gate.send(()).expect("release worker");
        while !deliver(&mut state) {}
        received.extend(messages(&receiver));
        terminal(&received, 1, Some(-32800));
        assert!(!received.iter().any(
            |message| matches!(message, Message::Notification(note) if note.params["token"] == "partial-1")
        ));
        request(&mut state, &uri, 1, true);
        finish(&mut state);
        terminal(&messages(&receiver), 1, None);
    }

    #[test]
    fn in_flight_edits_close_and_reopen_return_content_modified_without_tokens() {
        for method in [
            method::DOCUMENT_SYMBOL,
            method::CODE_ACTION,
            method::CODE_ACTION_RESOLVE,
        ] {
            for change in ["edit", "close", "reopen"] {
                let (mut state, uri, receiver) = state();
                let gate = hold_method(&mut state, &uri, 2, false, method);
                match change {
                    "edit" => state.on_did_change(json!({
                        "textDocument": {"uri": uri, "version": 2},
                        "contentChanges": [{"text": testing::HEARTBEAT}],
                    })),
                    "close" => state.on_did_close(json!({"textDocument": {"uri": uri}})),
                    _ => state.on_did_open(json!({
                        "textDocument": {"uri": uri, "version": 1, "text": testing::HEARTBEAT},
                    })),
                }
                terminal(&messages(&receiver), 2, Some(-32801));
                gate.send(()).expect("release worker");
            }
        }
    }

    #[test]
    fn panic_ends_progress_and_worker_serves_next_job() {
        let (mut state, uri, receiver) = state();
        admit(
            &mut state,
            &uri,
            3,
            true,
            Box::new(|_, _, _| panic!("background worker fault injection")),
        );
        finish(&mut state);
        terminal(
            &messages(&receiver),
            3,
            Some(ErrorCode::InternalError as i32),
        );
        request(&mut state, &uri, 4, true);
        finish(&mut state);
        terminal(&messages(&receiver), 4, None);
    }

    #[test]
    fn queued_jobs_begin_immediately_report_waiting_and_release_on_cancel() {
        let (mut state, uri, receiver) = state();
        let gate = hold(&mut state, &uri, 5, false);
        request(&mut state, &uri, 6, true);
        state.report_waiting_background(Instant::now() + WAITING_INTERVAL);
        let received = messages(&receiver);
        assert_eq!(progress(&received, "work-6", "begin").count(), 1);
        assert_eq!(progress(&received, "work-6", "report").count(), 1);
        assert_eq!(
            progress(&received, "work-5", "report").count(),
            0,
            "a running job does not say that it waits"
        );
        assert_eq!(queued(&state), 1);
        state.on_cancel(json!({"id":6}));
        assert_eq!(queued(&state), 0);
        terminal(&messages(&receiver), 6, Some(-32800));
        gate.send(()).expect("release worker");
        finish(&mut state);
        terminal(&messages(&receiver), 5, None);
    }

    #[test]
    fn shutdown_ends_progress_before_shutdown_response_without_joining_worker() {
        let (mut state, uri, receiver) = state();
        let gate = hold(&mut state, &uri, 7, true);
        state.on_request(Request::new(8.into(), method::SHUTDOWN.into(), Value::Null));
        let received = messages(&receiver);
        terminal(&received, 7, Some(-32803));
        assert!(
            matches!(received.last(), Some(Message::Response(response)) if response.id==8.into())
        );
        drop(state);
        gate.send(()).expect("worker was not joined");
    }

    #[test]
    fn repair_configuration_commit_invalidates_running_and_queued_repairs_only() {
        for settings in [
            json!({"diagnostics":{"rules":{"integrity":false}}}),
            json!({"defaultApplVerID":"FIX.5.0SP2"}),
        ] {
            let (mut state, uri, receiver) = state();
            let gate = hold_method(&mut state, &uri, 20, false, method::CODE_ACTION);
            admit_method(
                &mut state,
                &uri,
                21,
                false,
                method::CODE_ACTION,
                Box::new(|_, _, _| panic!("invalidated queued repair must not run")),
            );
            request(&mut state, &uri, 22, false);
            assert_eq!(queued(&state), 2);
            state.on_did_change_configuration(json!({"settings":{"fix":settings}}));
            testing::settle(&mut state);
            let mut received = messages(&receiver);
            terminal(&received, 20, Some(-32801));
            terminal(&received, 21, Some(-32801));
            assert_eq!(queued(&state), 1, "document symbols remain queued");
            gate.send(()).expect("release worker");
            finish(&mut state);
            received.extend(messages(&receiver));
            terminal(&received, 20, Some(-32801));
            terminal(&received, 21, Some(-32801));
            terminal(&received, 22, None);
        }
    }

    #[test]
    fn configuration_changes_invalidate_running_and_queued_action_resolves() {
        let (mut state, uri, receiver) = state();
        let gate = hold_method(&mut state, &uri, 60, false, method::CODE_ACTION_RESOLVE);
        admit_method(
            &mut state,
            &uri,
            61,
            false,
            method::CODE_ACTION_RESOLVE,
            Box::new(|_, _, _| panic!("invalidated resolve must not run")),
        );
        state.on_did_change_configuration(json!({"settings":{"fix":{
            "diagnostics":{"rules":{"integrity":false}}
        }}}));
        testing::settle(&mut state);
        let mut received = messages(&receiver);
        terminal(&received, 60, Some(-32801));
        terminal(&received, 61, Some(-32801));
        assert_eq!(queued(&state), 0);
        gate.send(()).expect("release worker");
        finish(&mut state);
        received.extend(messages(&receiver));
        terminal(&received, 60, Some(-32801));
        terminal(&received, 61, Some(-32801));
    }

    #[test]
    fn unchanged_and_unrelated_configuration_pulls_preserve_repairs() {
        for method in [method::CODE_ACTION, method::CODE_ACTION_RESOLVE] {
            for settings in [
                json!({}),
                json!({"hover":{"enabled":false}}),
                json!({"limits":{"rangeLines":17}}),
                json!({"limits":{"diagnosticsPerDocument":1,"cachedLines":5}}),
            ] {
                let (mut state, uri, receiver) = state();
                let gate = hold_method(&mut state, &uri, 23, false, method);
                state.client.configuration = true;
                state.pull_base_config();
                state.on_base_reply(state.config.global_generation, Ok(json!([settings])));
                testing::settle(&mut state);
                assert!(state.background.contains(&23.into()));
                assert!(!messages(&receiver)
                    .iter()
                    .any(|message| matches!(message, Message::Response(_))));
                gate.send(()).expect("release worker");
                finish(&mut state);
                terminal(&messages(&receiver), 23, None);
            }
        }
    }

    #[test]
    fn range_limit_changes_invalidate_only_mixed_actions_running_or_queued() {
        for (running_only, queued_only, running_mixed) in [
            (
                json!(["source.fixAll"]),
                json!(["quickfix", "source.fixAll"]),
                false,
            ),
            (json!(["source"]), json!([""]), false),
            (
                json!(["quickfix", "source.fixAll"]),
                json!(["source.fixAll"]),
                true,
            ),
            (json!([""]), json!(["source"]), true),
        ] {
            let (mut state, uri, receiver) = state();
            state.client.takes_actions = true;
            let params = |id: i32, only: Value| {
                json!({
                    "textDocument": {"uri": uri},
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                    "context": {"diagnostics": [], "only": only, "triggerKind": 1},
                    "workDoneToken": format!("work-{id}"),
                })
            };
            let dispatched = state.code_action_task_prepared(
                serde_json::from_value(params(40, running_only)).unwrap(),
            );
            let super::super::dispatch::Dispatched::Background(mut task) = dispatched else {
                panic!("source actions run in the background");
            };
            let (gate, opened) = unbounded();
            let (entered, running) = unbounded();
            let run = task.run;
            task.run = Box::new(move |snapshot, cancelled, emit| {
                entered.send(()).unwrap();
                opened.recv().unwrap();
                run(snapshot, cancelled, emit)
            });
            state.req_queue.incoming.register(40.into(), ());
            state.start_background(40.into(), task);
            running.recv_timeout(WAIT).unwrap();
            assert!(!deliver(&mut state));
            state.on_request(Request::new(
                41.into(),
                method::CODE_ACTION.into(),
                params(41, queued_only),
            ));
            assert_eq!(queued(&state), 1);
            state.on_did_change_configuration(json!({"settings": {"fix": {
                "limits": {"rangeLines": 17}
            }}}));
            testing::settle(&mut state);
            let (mixed, source) = if running_mixed { (40, 41) } else { (41, 40) };
            let mut received = messages(&receiver);
            terminal(&received, mixed, Some(-32801));
            assert!(state.background.contains(&source.into()));
            assert!(!received.iter().any(|message| matches!(message,
                Message::Response(response) if response.id == source.into())));
            gate.send(()).unwrap();
            finish(&mut state);
            received.extend(messages(&receiver));
            terminal(&received, mixed, Some(-32801));
            terminal(&received, source, None);
        }
    }

    #[test]
    fn dictionary_rename_invalidates_running_and_queued_repairs_through_configuration() {
        let (mut state, uri, receiver) = state();
        state.client.takes_actions = true;
        let settings = |name: &str| {
            json!({
                "allowDictionaryPath": true,
                "dictionaries": [{
                    "name": name,
                    "path": concat!(env!("CARGO_MANIFEST_DIR"), "/xml/custom/synthetic/oe-fix44.xml"),
                    "default": true,
                }],
            })
        };
        state.on_did_change_configuration(json!({"settings": {"fix": settings("before")}}));
        testing::settle(&mut state);
        let original = state.config.documents[&uri].clone();
        messages(&receiver);
        let gate = hold_method(&mut state, &uri, 42, false, method::CODE_ACTION);
        state.on_request(Request::new(
            43.into(),
            method::CODE_ACTION.into(),
            json!({
                "textDocument": {"uri": uri},
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                "context": {"diagnostics": [], "only": ["source.fixAll"], "triggerKind": 1},
                "workDoneToken": "work-43",
            }),
        ));
        assert_eq!(queued(&state), 1);
        state.on_did_change_configuration(json!({"settings": {"fix": settings("after")}}));
        testing::settle(&mut state);
        let current = &state.config.documents[&uri];
        assert!(Arc::ptr_eq(&original.ctx, &current.ctx));
        assert!(Arc::ptr_eq(&original.rules, &current.rules));
        assert!(!original.names.same_as(&current.names));
        assert_eq!(current.repair_generation, original.repair_generation + 1);
        assert_eq!(queued(&state), 0);
        let mut received = messages(&receiver);
        terminal(&received, 42, Some(-32801));
        terminal(&received, 43, Some(-32801));
        gate.send(()).expect("release worker");
        while !deliver(&mut state) {}
        received.extend(messages(&receiver));
        terminal(&received, 42, Some(-32801));
        terminal(&received, 43, Some(-32801));
    }

    #[test]
    fn base_rule_change_preserves_a_repair_with_unchanged_document_overrides() {
        let (mut state, uri, receiver) = state();
        state.on_did_change(json!({
            "textDocument":{"uri":uri,"version":2},
            "contentChanges":[{"text":format!(
                "# fix: diagnostics.enabled=false\n{}", testing::HEARTBEAT
            )}],
        }));
        testing::settle(&mut state);
        assert!(!state.effective_config(&uri).unwrap().diagnostics.enabled);
        let gate = hold_method(&mut state, &uri, 25, false, method::CODE_ACTION);
        state.on_did_change_configuration(json!({
            "settings":{"fix":{"diagnostics":{"enabled":false}}}
        }));
        testing::settle(&mut state);
        assert!(state.background.contains(&25.into()));
        gate.send(()).expect("release worker");
        finish(&mut state);
        terminal(&messages(&receiver), 25, None);
    }

    #[test]
    fn base_context_convergence_preserves_a_repair_with_unchanged_document_context() {
        let (mut state, uri, receiver) = state();
        state.on_did_change(json!({
            "textDocument":{"uri":uri,"version":2},
            "contentChanges":[{"text":format!(
                "# fix: defaultApplVerID=FIX.5.0SP2\n{}", testing::HEARTBEAT
            )}],
        }));
        testing::settle(&mut state);
        let original = state.config.documents[&uri].ctx.clone();
        assert_ne!(original.fingerprint(), state.ctx.fingerprint());
        let gate = hold_method(&mut state, &uri, 28, false, method::CODE_ACTION);
        state.on_did_change_configuration(json!({
            "settings":{"fix":{"defaultApplVerID":"FIX.5.0SP2"}}
        }));
        testing::settle(&mut state);
        assert_eq!(original.fingerprint(), state.ctx.fingerprint());
        assert!(Arc::ptr_eq(&original, &state.config.documents[&uri].ctx));
        assert!(state.background.contains(&28.into()));
        gate.send(()).expect("release worker");
        finish(&mut state);
        terminal(&messages(&receiver), 28, None);
    }

    #[test]
    fn repair_cancellation_and_shutdown_suppress_late_results() {
        for shutdown in [false, true] {
            let (mut state, uri, receiver) = state();
            let gate = hold_method(&mut state, &uri, 26, false, method::CODE_ACTION);
            if shutdown {
                state.on_request(Request::new(
                    27.into(),
                    method::SHUTDOWN.into(),
                    Value::Null,
                ));
            } else {
                state.on_cancel(json!({"id":26}));
            }
            let code = if shutdown { -32803 } else { -32800 };
            let mut received = messages(&receiver);
            terminal(&received, 26, Some(code));
            gate.send(()).expect("release worker");
            while !deliver(&mut state) {}
            received.extend(messages(&receiver));
            terminal(&received, 26, Some(code));
        }
    }

    #[test]
    fn held_fix_all_work_keeps_hover_and_ordinary_quickfix_available() {
        let (mut state, uri, receiver) = state();
        state.client.takes_actions = true;
        state.on_did_change(json!({
            "textDocument": {"uri": uri, "version": 2},
            "contentChanges": [{"text": testing::HEARTBEAT.replace("9=57", "9=1")}],
        }));
        testing::settle(&mut state);
        let gate = hold_method(&mut state, &uri, 30, false, method::CODE_ACTION);
        state.on_request(Request::new(
            31.into(),
            method::CODE_ACTION.into(),
            json!({
                "textDocument": {"uri": uri},
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                "context": {"diagnostics": [], "triggerKind": 1},
            }),
        ));
        state.on_request(Request::new(
            32.into(),
            method::HOVER.into(),
            json!({
                "textDocument": {"uri": uri}, "position": {"line": 0, "character": 0},
            }),
        ));
        let replies: Vec<_> = messages(&receiver)
            .into_iter()
            .filter_map(|message| match message {
                Message::Response(response) => Some(response),
                _ => None,
            })
            .collect();
        assert_eq!(replies.len(), 2);
        let quickfix = replies
            .iter()
            .find(|response| response.id == 31.into())
            .unwrap();
        assert_eq!(
            quickfix
                .response_result
                .as_ref()
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(replies
            .iter()
            .any(|response| response.id == 32.into() && response.response_result.is_ok()));
        assert!(state.background.contains(&30.into()));
        state.on_cancel(json!({"id": 30}));
        terminal(&messages(&receiver), 30, Some(-32800));
        gate.send(()).unwrap();
        while !deliver(&mut state) {}
    }
}
