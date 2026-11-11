//! One background request worker; only the main loop delivers its results.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use lsp_server::{RequestId, ResponseError};
use lsp_types::{ProgressToken, Uri};

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
    /// Whether the request gave a work-done token; without one nobody reads progress.
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
    partial: Option<ProgressToken>,
    progress: Option<Progress>,
    cancelled: Arc<AtomicBool>,
    /// False while the job is in the queue.
    running: bool,
    /// When the client last heard that the job waits.
    waiting_reported: Instant,
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
            .filter(|active| !active.running && active.progress.is_some())
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
            run,
        } = task;
        self.background.next_generation += 1;
        let generation = self.background.next_generation;
        let cancelled = Arc::new(AtomicBool::new(false));
        let progress = work_done
            .map(|token| Progress::begin(&self.sender, token, title, 0, Some("Queued"), true));
        let job = Job {
            generation,
            epoch: self.background.epoch,
            method,
            run,
            progress: progress.is_some(),
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
        self.background.queue.push(job);
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
                if let Some(progress) = &active.progress {
                    progress.report(percentage, Some(phase));
                }
            }
            Event::Finished(result) => {
                let mut active = self
                    .background
                    .active
                    .remove(&generation)
                    .expect("active job");
                // The `end` goes out before the response it belongs to.
                if let Some(progress) = active.progress.take() {
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
                if let Some(progress) = &active.progress {
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
        if let Some(progress) = active.progress.take() {
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
        let jobs: Vec<_> = self
            .background
            .active
            .iter()
            .filter(|(_, active)| &active.uri == uri)
            .map(|(&generation, active)| (generation, active.method))
            .collect();
        for (generation, method) in jobs {
            let error = super::content_modified(method, "the document has changed or closed");
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
    use lsp_server::{ErrorCode, Message, Request};
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
        state.req_queue.incoming.register(id.into(), ());
        let task = BackgroundTask {
            method: method::DOCUMENT_SYMBOL,
            title: "Document symbols",
            uri: uri.clone(),
            snapshot: state.snapshot_for(uri).expect("an open document"),
            work_done: Some(ProgressToken::String(format!("work-{id}"))),
            partial: partial.then(|| ProgressToken::String(format!("partial-{id}"))),
            run,
        };
        state.start_background(id.into(), task);
    }

    /// Admit a task that the worker enters and then holds until the gate opens.
    fn hold(state: &mut ServerState, uri: &Uri, id: i32, partial: bool) -> Sender<()> {
        let (gate, opened) = unbounded::<()>();
        let (entered, running) = unbounded();
        admit(
            state,
            uri,
            id,
            partial,
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
        for change in ["edit", "close", "reopen"] {
            let (mut state, uri, receiver) = state();
            let gate = hold(&mut state, &uri, 2, false);
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
}
