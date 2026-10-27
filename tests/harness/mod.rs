//! In-process generic LSP client over `Connection::memory()`.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use serde_json::{json, Value};

/// Spelled out here, and checked against the metamodel below, so a test cannot
/// agree with the server on a name the protocol does not have — which is how
/// `window/workDoneProgressCreate` survived a whole stage.
pub const PROGRESS_CREATE: &str = "window/workDoneProgress/create";

#[test]
fn the_progress_create_method_is_the_one_the_protocol_defines() {
    use lsp_types::Request as _;
    assert_eq!(
        PROGRESS_CREATE,
        lsp_types::WorkDoneProgressCreateRequest::METHOD.as_str()
    );
}

/// Error codes the LSP spec fixes. Spelled here because the harness answers in
/// raw JSON-RPC, and because five copies of `-32801` had accumulated.
pub const CONTENT_MODIFIED: i32 = -32801;
pub const INVALID_PARAMS: i32 = -32602;
pub const REQUEST_FAILED: i32 = -32803;

/// A message with BodyLength(9) and CheckSum(10) computed apart from the
/// integrity repair of the server. `|` stands for SOH.
pub fn frame(begin: &str, body: &str) -> String {
    let message = format!("8={begin}|9={}|{body}", body.len());
    let checksum: u32 = message
        .bytes()
        .map(|byte| u32::from(if byte == b'|' { 1 } else { byte }))
        .sum();
    format!("{message}10={:03}|", checksum % 256)
}

/// Inlay hint labels as text. A label is a string or a list of parts.
pub fn hint_labels(hints: &[Value]) -> Vec<String> {
    hints
        .iter()
        .map(|hint| match &hint["label"] {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts
                .iter()
                .map(|part| part["value"].as_str().expect("label part"))
                .collect(),
            other => panic!("a label is a string or parts: {other}"),
        })
        .collect()
}

pub fn file_uri(path: &Path) -> String {
    lsp_types::Uri::from_file_path(path).unwrap().to_string()
}

/// A temporary directory, removed on drop even when the test panics.
pub struct Workspace(pub PathBuf);

impl Workspace {
    pub fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "venue-reader-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }

    pub fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }

    pub fn uri(&self, relative: &str) -> String {
        file_uri(&self.0.join(relative))
    }

    #[cfg(unix)]
    pub fn link(&self, directory: &str, name: &str, target: &Path) {
        let directory = self.0.join(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::os::unix::fs::symlink(target, directory.join(name)).unwrap();
    }

    /// Rooted here, UTF-8 positions, `settings` as `initializationOptions.fix`.
    pub fn initialize(&self, client: &mut TestClient, settings: Value) -> Value {
        client.initialize_with(json!({
            "rootUri": file_uri(&self.0),
            "capabilities": {"general":{"positionEncodings":["utf-8"]}},
            "initializationOptions":{"fix":settings}
        }))
    }

    /// Settings binding `xml`, written here as `desk.xml`, as the default
    /// custom dictionary named "desk".
    pub fn desk_dictionary(&self, xml: &str) -> Value {
        let path = self.write("desk.xml", xml);
        json!({
            "allowDictionaryPath": true,
            "dictionaries": [{ "path": path, "name": "desk", "default": true }],
        })
    }

    pub fn client(&self, settings: Value) -> TestClient {
        let mut client = TestClient::start();
        self.initialize(&mut client, settings);
        client
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct TestClient {
    conn: Connection,
    server: Option<JoinHandle<venue_reader::server::ServerExit>>,
    completed: crossbeam_channel::Receiver<()>,
    next_id: i32,
    /// Notifications seen while waiting for something else; handed out by
    /// `request_collecting` so nothing the server said is silently lost.
    pending: Vec<(String, Value)>,
    pending_requests: Vec<Request>,
}

impl TestClient {
    pub fn start() -> TestClient {
        Self::start_with_user_file(None)
    }

    pub fn start_with_user_file(path: Option<PathBuf>) -> TestClient {
        let (server_conn, client_conn) = Connection::memory();
        let (completion, completed) = crossbeam_channel::bounded(1);
        let server = std::thread::spawn(move || {
            let outcome = venue_reader::server::serve_with_user_file(
                venue_reader::transport::Link {
                    receiver: server_conn.receiver,
                    sender: venue_reader::transport::Sink::Memory(server_conn.sender),
                },
                path,
            );
            let _ = completion.send(());
            outcome
        });
        TestClient {
            conn: client_conn,
            server: Some(server),
            completed,
            next_id: 0,
            pending: Vec::new(),
            pending_requests: Vec::new(),
        }
    }

    /// Send a request and wait for its response. Notifications seen meanwhile
    /// stay pending for a later `request_collecting`. Returns `Ok(result)` or
    /// `Err(error_code)`.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, i32> {
        let (result, notes) = self.request_collecting(method, params);
        self.pending = notes;
        result
    }

    /// Capabilities with both `workspace/configuration` and progress.
    pub fn caps_with_pull_and_progress() -> Value {
        json!({
            "general": { "positionEncodings": ["utf-8"] },
            "workspace": { "configuration": true },
            "window": { "workDoneProgress": true },
        })
    }

    /// A client that can take a versioned code-action literal, and says it
    /// understands `isPreferred`.
    pub fn caps_taking_actions() -> Value {
        json!({
            "general": { "positionEncodings": ["utf-8"] },
            "textDocument": { "codeAction": {
                "codeActionLiteralSupport": { "codeActionKind": { "valueSet": ["quickfix"] } },
                "isPreferredSupport": true,
            }},
            "workspace": { "workspaceEdit": { "documentChanges": true } },
        })
    }

    /// Answer the next server→client request, asserting its method.
    pub fn answer(&mut self, method: &str, result: Value) -> Value {
        let (id, seen, params) = self.recv_server_request();
        assert_eq!(seen, method);
        self.reply(id, result);
        params
    }

    /// Refuse the next server→client request with an error.
    pub fn refuse(&mut self, method: &str) {
        let (id, seen, _) = self.recv_server_request();
        assert_eq!(seen, method);
        let _ = self.conn.sender.send(Message::Response(Response::new_err(
            id,
            -32603,
            "no".into(),
        )));
    }

    /// Capability-only initialize with `workspace/configuration` support.
    pub fn caps_with_pull() -> Value {
        json!({
            "general": { "positionEncodings": ["utf-8"] },
            "workspace": { "configuration": true },
        })
    }

    pub fn hover_params(uri: &str, line: u32, character: u32) -> Value {
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        })
    }

    /// Like `request`, but also returns every notification that arrived before
    /// the response (the channel is FIFO, so this captures what the server sent
    /// since the previous receive).
    pub fn request_collecting(
        &mut self,
        method: &str,
        params: Value,
    ) -> (Result<Value, i32>, Vec<(String, Value)>) {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.conn
            .sender
            .send(Message::Request(Request::new(
                id.clone(),
                method.into(),
                params,
            )))
            .unwrap();
        let mut notes = std::mem::take(&mut self.pending);
        loop {
            match self
                .conn
                .receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("response within 10s")
            {
                Message::Response(r) if r.id == id => {
                    return (r.response_result.map_err(|e| e.code), notes);
                }
                Message::Notification(n) => notes.push((n.method, n.params)),
                Message::Request(request) => self.pending_requests.push(request),
                _ => {}
            }
        }
    }

    /// `initialize` with arbitrary params (root, initializationOptions, …), then
    /// `initialized`. Does not auto-answer configuration pulls.
    pub fn initialize_with(&mut self, params: Value) -> Value {
        let res = self.request("initialize", params).expect("initialize ok");
        self.notify("initialized", json!({}));
        res
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self
            .conn
            .sender
            .send(Message::Notification(Notification::new(
                method.into(),
                params,
            )));
    }

    pub fn initialize(&mut self, position_encodings: &[&str]) -> Value {
        self.initialize_caps(json!({
            "general": { "positionEncodings": position_encodings }
        }))
    }

    /// Answer the base `workspace/configuration` pull the server sends on
    /// `initialized`, so a test can get to the exchange it actually cares about.
    pub fn answer_base_pull(&mut self) {
        let (id, method, params) = self.recv_server_request();
        assert_eq!(method, "workspace/configuration");
        assert_eq!(params, json!({ "items": [{ "section": "fix" }] }));
        self.reply(id, json!([null]));
    }

    /// Initialize with explicit client capabilities (for config-pull tests), then
    /// send `initialized`. Does not auto-answer the server's configuration pulls.
    pub fn initialize_caps(&mut self, capabilities: Value) -> Value {
        self.initialize_with(json!({ "capabilities": capabilities }))
    }

    /// Wait for the next server→client request, returning `(id, method, params)`.
    pub fn recv_server_request(&mut self) -> (RequestId, String, Value) {
        if !self.pending_requests.is_empty() {
            let request = self.pending_requests.remove(0);
            return (request.id, request.method, request.params);
        }
        loop {
            match self
                .conn
                .receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("server request within 10s")
            {
                Message::Request(r) => return (r.id, r.method, r.params),
                Message::Notification(n) => self.pending.push((n.method, n.params)),
                Message::Response(_) => {}
            }
        }
    }

    pub fn reply(&self, id: RequestId, result: Value) {
        let _ = self
            .conn
            .sender
            .send(Message::Response(Response::new_ok(id, result)));
    }

    /// Returns the `fix.describeOverrides` report that proved the document ready.
    pub fn did_open(&mut self, uri: &str, text: &str, version: i64) -> Value {
        self.did_open_unready(uri, text, version);
        self.wait_ready(uri)
    }

    pub fn did_open_unready(&self, uri: &str, text: &str, version: i64) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": { "uri": uri, "languageId": "fix", "version": version, "text": text }
            }),
        );
    }

    pub fn describe(&mut self, uri: &str) -> Result<Value, i32> {
        self.request(
            "workspace/executeCommand",
            json!({"command": "fix.describeOverrides", "arguments": [uri]}),
        )
    }

    pub fn wait_ready(&mut self, uri: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.describe(uri) {
                Ok(report) => return report,
                Err(REQUEST_FAILED) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                error => panic!("document {uri} did not become ready: {error:?}"),
            }
        }
    }

    /// Wait until the base configuration has committed, through the one
    /// request that needs no document: `fix.describeTag` is refused until then.
    pub fn wait_base_ready(&mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let asked = self.request(
                "workspace/executeCommand",
                json!({"command": "fix.describeTag", "arguments": [1328]}),
            );
            match asked {
                Ok(text) => return text.as_str().expect("a string").to_owned(),
                Err(REQUEST_FAILED) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                error => panic!("the base configuration did not commit: {error:?}"),
            }
        }
    }

    pub fn wait_for_exit(&mut self) -> venue_reader::server::ServerExit {
        self.completed
            .recv_timeout(Duration::from_secs(5))
            .expect("server exits within 5s");
        for message in self.conn.receiver.try_iter() {
            if let Message::Notification(note) = message {
                self.pending.push((note.method, note.params));
            }
        }
        self.server
            .take()
            .expect("running server")
            .join()
            .expect("server does not panic")
    }

    pub fn hover(&mut self, uri: &str, line: u32, character: u32) -> Result<Value, i32> {
        self.request(
            "textDocument/hover",
            Self::hover_params(uri, line, character),
        )
    }

    /// The hover markdown for the field that starts with `field` on `line`,
    /// whose text is `line_text`. `None` when the server has no answer.
    pub fn hover_markdown(
        &mut self,
        uri: &str,
        line: u32,
        line_text: &str,
        field: &str,
    ) -> Option<String> {
        let column = line_text.find(field).expect("the field is on the line") as u32;
        let answer = self.hover(uri, line, column).expect("hover response");
        answer["contents"]["value"].as_str().map(str::to_owned)
    }

    /// Wait until `fix.describeOverrides` reports `expected` as the effective `key`.
    pub fn wait_effective(&mut self, uri: &str, key: &str, expected: Value) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let report = self.describe(uri);
            if let Ok(report) = &report {
                if report["effective"][key] == expected {
                    return report.clone();
                }
            }
            assert!(
                Instant::now() < deadline,
                "{key} did not become {expected}: {report:?}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Block until the server sends `method`, returning its params. Notifications
    /// seen meanwhile stay pending. Panics rather than hanging the suite.
    pub fn wait_for(&mut self, method: &str) -> Value {
        if let Some(at) = self.pending.iter().position(|(m, _)| m == method) {
            return self.pending.remove(at).1;
        }
        let deadline = std::time::Duration::from_secs(10);
        loop {
            match self.conn.receiver.recv_timeout(deadline) {
                Ok(Message::Notification(n)) if n.method == method => return n.params,
                Ok(Message::Notification(n)) => self.pending.push((n.method, n.params)),
                Ok(_) => {}
                Err(e) => panic!("no {method} within {deadline:?}: {e}"),
            }
        }
    }

    /// The `window/logMessage` whose text contains `needle`.
    ///
    /// By content, because the server has several things it can say at a
    /// session's start and which arrives first is nobody's subject: a test that
    /// takes the first log message asserts an ordering it does not care about
    /// and breaks when the server learns to say something else.
    pub fn wait_for_log(&mut self, needle: &str) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(at) = self.pending.iter().position(|(method, params)| {
                method == "window/logMessage"
                    && params["message"]
                        .as_str()
                        .is_some_and(|text| text.contains(needle))
            }) {
                return self.pending.remove(at).1;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "no log containing {needle:?}");
            match self.conn.receiver.recv_timeout(left) {
                Ok(Message::Notification(n)) => self.pending.push((n.method, n.params)),
                Ok(_) => {}
                Err(e) => panic!("no log containing {needle:?}: {e}"),
            }
        }
    }

    /// Every notification already received, plus whatever arrives in `wait`.
    /// Used to assert that something did *not* happen.
    pub fn drain_notifications(&mut self, wait: std::time::Duration) -> Vec<(String, Value)> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            match self.conn.receiver.recv_timeout(left) {
                Ok(Message::Notification(n)) => self.pending.push((n.method, n.params)),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        std::mem::take(&mut self.pending)
    }

    /// Every method the server sent within `wait`, requests included — each of
    /// those answered `null` so the server is not left waiting.
    ///
    /// `drain_notifications` cannot be used to prove a *request* was not sent:
    /// it drops everything that is not a notification, so a refresh — which is
    /// a request — is invisible to it either way.
    pub fn drain_server_methods(&mut self, wait: std::time::Duration) -> Vec<String> {
        let deadline = std::time::Instant::now() + wait;
        let mut seen = Vec::new();
        for request in std::mem::take(&mut self.pending_requests) {
            seen.push(request.method);
            self.reply(request.id, Value::Null);
        }
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            match self.conn.receiver.recv_timeout(left) {
                Ok(Message::Notification(n)) => seen.push(n.method),
                Ok(Message::Request(request)) => {
                    seen.push(request.method.clone());
                    self.reply(request.id, Value::Null);
                }
                Ok(Message::Response(_)) => {}
                Err(_) => break,
            }
        }
        seen
    }

    pub fn did_change_full(&self, uri: &str, text: &str, version: i64) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }]
            }),
        );
    }

    pub fn did_close(&self, uri: &str) {
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        );
    }

    /// Graceful shutdown; `exit` + join happen in `Drop`.
    pub fn shutdown(mut self) {
        let _ = self.request("shutdown", Value::Null);
    }
}

impl Drop for TestClient {
    fn drop(&mut self) {
        if let Some(h) = self.server.take() {
            self.notify("exit", Value::Null);
            let _ = h.join();
        }
    }
}

// --- FIX sample logs, for tests that read them off disk -------------------

pub const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

/// Bundled dictionaries, `FIX.4.4` as the fallback — what a plain `venue-reader`
/// reading a log with no configuration uses.
pub fn test_ctx() -> std::sync::Arc<venue_reader::ctx::Ctx> {
    venue_reader::ctx::Ctx::new(
        &venue_reader::config::Config::default(),
        std::sync::Arc::new(venue_reader::dictionary::Registry::bundled_only()),
        None,
    )
}

/// Every `.fix` file under `tests/fixtures/`, as `(name, line bytes)`. Sorted,
/// so a failure names the same line on every run.
pub fn fixture_lines() -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, into: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("fixtures directory")
            .map(|entry| entry.expect("entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, into);
            } else if path.extension().is_some_and(|e| e == "fix") {
                into.push(path);
            }
        }
    }
    let root = Path::new(FIXTURES);
    let mut files = Vec::new();
    walk(root, &mut files);

    let mut lines = Vec::new();
    for path in files {
        let name = path
            .strip_prefix(root)
            .expect("under the fixtures root")
            .to_string_lossy()
            .into_owned();
        let text = std::fs::read(&path).expect("read fixture");
        for (lnum, line) in text.split(|&byte| byte == b'\n').enumerate() {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if !line.is_empty() {
                lines.push((format!("{name}:{lnum}"), line.to_vec()));
            }
        }
    }
    lines
}

/// Applies edits to the bytes they were computed against. Back to front: an
/// earlier edit would move the spans of the later ones.
pub fn apply_edits(bytes: &[u8], edits: &[venue_reader::rules::Edit]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    for edit in edits.iter().rev() {
        out.splice(edit.start.index()..edit.end.index(), edit.text.bytes());
    }
    out
}
