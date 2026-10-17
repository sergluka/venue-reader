use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver};
use serde_json::{json, Value};

const MESSAGE_TIMEOUT: Duration = Duration::from_secs(10);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);

enum Incoming {
    Message(Value),
    Error(String),
    Eof,
}

pub struct StdioClient {
    child: Child,
    input: Arc<Mutex<ChildStdin>>,
    incoming: Receiver<Incoming>,
    pending: Vec<Value>,
    next_request_id: i64,
    stderr: Arc<Mutex<Vec<u8>>>,
    reader_error: Arc<Mutex<Option<String>>>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    directory: PathBuf,
    personal_file: PathBuf,
    workspace: PathBuf,
}

impl StdioClient {
    /// `XDG_CONFIG_HOME` points at a directory of this test's own, which is
    /// where the personal file goes.
    pub fn start(
        label: &str,
        personal_config: Option<&str>,
        configuration_reply: Option<Value>,
    ) -> Self {
        Self::spawn(label, personal_config, true, configuration_reply)
    }

    /// `XDG_CONFIG_HOME` unset, so the personal file goes to `$HOME/.config/fix-lsp`
    /// default the server must fall back to. Only `process_location` resolves
    /// that; `location`'s own unit tests cannot reach it.
    pub fn start_without_config_home(
        label: &str,
        personal_config: Option<&str>,
        configuration_reply: Option<Value>,
    ) -> Self {
        Self::spawn(label, personal_config, false, configuration_reply)
    }

    fn spawn(
        label: &str,
        personal_config: Option<&str>,
        in_config_home: bool,
        configuration_reply: Option<Value>,
    ) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "fix-lsp-stdio-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));

        let home = directory.join("home");
        let workspace = directory.join("workspace");
        let config_home = directory.join("xdg-config");
        std::fs::create_dir_all(&workspace).expect("create test workspace");

        let mut command = Command::new(env!("CARGO_BIN_EXE_fix-lsp"));
        command
            .arg("--stdio")
            .env("HOME", &home)
            .env("USERPROFILE", &home);
        // The developer's own configuration directory must never reach the
        // server under test: every child is given one of the two locations, and
        // neither is inherited.
        let personal_directory = if in_config_home {
            command.env("XDG_CONFIG_HOME", &config_home);
            config_home
        } else {
            command.env_remove("XDG_CONFIG_HOME");
            home.join(".config")
        }
        .join("fix-lsp");
        std::fs::create_dir_all(&personal_directory).expect("create personal configuration dir");
        let personal_file = personal_directory.join("config.toml");
        if let Some(contents) = personal_config {
            std::fs::write(&personal_file, contents).expect("write personal configuration");
        }

        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start fix-lsp");
        let input = Arc::new(Mutex::new(child.stdin.take().expect("piped stdin")));
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr_pipe = child.stderr.take().expect("piped stderr");

        let (incoming_sender, incoming) = unbounded();
        let reader_input = Arc::clone(&input);
        let reader_error = Arc::new(Mutex::new(None));
        let reported_reader_error = Arc::clone(&reader_error);

        let stdout_thread = std::thread::spawn(move || {
            let report_error = |error: String| {
                *reported_reader_error.lock().expect("reader error lock") = Some(error.clone());
                let _ = incoming_sender.send(Incoming::Error(error));
            };
            let mut stdout = BufReader::new(stdout);
            loop {
                let body = match read_frame(&mut stdout) {
                    Ok(Some(body)) => body,
                    Ok(None) => {
                        let _ = incoming_sender.send(Incoming::Eof);
                        return;
                    }
                    Err(error) => {
                        report_error(error);
                        return;
                    }
                };
                let message: Value = match serde_json::from_slice(&body) {
                    Ok(message) => message,
                    Err(error) => {
                        report_error(format!(
                            "invalid JSON response: {error}; body={}",
                            String::from_utf8_lossy(&body)
                        ));
                        return;
                    }
                };
                if message.get("jsonrpc") != Some(&Value::String("2.0".into())) {
                    report_error(format!(
                        "outgoing message has no jsonrpc 2.0 member: {message}"
                    ));
                    return;
                }

                if message.get("method") == Some(&Value::String("workspace/configuration".into()))
                    && message.get("id").is_some()
                {
                    if let Some(result) = configuration_reply.as_ref() {
                        let response = json!({
                            "jsonrpc": "2.0",
                            "id": message["id"].clone(),
                            "result": result,
                        });
                        if let Err(error) = write_value(&reader_input, &response) {
                            report_error(format!("configuration auto-reply failed: {error}"));
                            return;
                        }
                    }
                }
                if incoming_sender.send(Incoming::Message(message)).is_err() {
                    return;
                }
            }
        });

        let stderr = Arc::new(Mutex::new(Vec::new()));
        let collected_stderr = Arc::clone(&stderr);
        let stderr_thread = std::thread::spawn(move || {
            let mut stderr_pipe = stderr_pipe;
            let mut buffer = [0u8; 4096];
            loop {
                match stderr_pipe.read(&mut buffer) {
                    Ok(0) => return,
                    Ok(read) => collected_stderr
                        .lock()
                        .expect("stderr lock")
                        .extend_from_slice(&buffer[..read]),
                    Err(_) => return,
                }
            }
        });

        Self {
            child,
            input,
            incoming,
            pending: Vec::new(),
            next_request_id: 0,
            stderr,
            reader_error,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            directory,
            personal_file,
            workspace,
        }
    }

    /// Where this test's personal configuration was written, whichever location
    /// the child was given.
    pub fn personal_file(&self) -> &Path {
        &self.personal_file
    }

    pub fn write_workspace(&self, relative: &str, contents: &[u8]) -> String {
        let path = self.workspace.join(relative);
        std::fs::create_dir_all(path.parent().expect("workspace file parent"))
            .expect("create workspace directory");
        std::fs::write(&path, contents).expect("write workspace file");
        lsp_types::Uri::from_file_path(path)
            .expect("workspace path is a file URI")
            .to_string()
    }

    pub fn root_uri(&self) -> String {
        lsp_types::Uri::from_file_path(&self.workspace)
            .expect("workspace path is a file URI")
            .to_string()
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send_value(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    pub fn send_raw_body(&self, body: &[u8]) {
        write_body(&self.input, body).expect("write raw JSON-RPC body");
    }

    pub fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_request_id += 1;
        let request_id = self.next_request_id;
        self.send_value(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        }));
        let response = self.wait_for(|message| {
            message.get("id") == Some(&json!(request_id))
                && (message.get("result").is_some() || message.get("error").is_some())
        });
        if let Some(error) = response.get("error") {
            panic!("{method} failed: {error}; {}", self.context());
        }
        response
            .get("result")
            .cloned()
            .expect("successful response has a result")
    }

    pub fn wait_for_request(&mut self, method: &str) -> Value {
        self.wait_for(|message| {
            message.get("method") == Some(&Value::String(method.into()))
                && message.get("id").is_some()
        })
    }

    pub fn wait_for_notification(
        &mut self,
        method: &str,
        predicate: impl Fn(&Value) -> bool,
    ) -> Value {
        let message = self.wait_for(|message| {
            message.get("method") == Some(&Value::String(method.into()))
                && message.get("id").is_none()
                && predicate(&message["params"])
        });
        message["params"].clone()
    }

    pub fn shutdown_and_exit(&mut self) -> ExitStatus {
        assert!(
            self.request("shutdown", Value::Null).is_null(),
            "shutdown result"
        );
        self.notify("exit", Value::Null);
        self.wait_for_exit(PROCESS_TIMEOUT)
    }

    pub fn wait_for_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("query child status") {
                self.join_readers();
                self.assert_reader_clean();
                return status;
            }
            if Instant::now() >= deadline {
                let context = self.context();
                let _ = self.child.kill();
                let _ = self.child.wait();
                self.join_readers();
                panic!("fix-lsp did not exit within {timeout:?}; {context}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().expect("stderr lock")).into_owned()
    }

    fn send_value(&self, value: &Value) {
        write_value(&self.input, value).expect("write JSON-RPC message");
    }

    fn wait_for(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        if let Some(position) = self.pending.iter().position(&predicate) {
            return self.pending.remove(position);
        }
        let deadline = Instant::now() + MESSAGE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!(
                    "no matching server message within {MESSAGE_TIMEOUT:?}; {}",
                    self.context()
                );
            }
            match self.incoming.recv_timeout(remaining) {
                Ok(Incoming::Message(message)) if predicate(&message) => return message,
                Ok(Incoming::Message(message)) => self.pending.push(message),
                Ok(Incoming::Error(error)) => {
                    panic!("stdio reader failed: {error}; {}", self.context())
                }
                Ok(Incoming::Eof) => panic!("server stdout closed; {}", self.context()),
                Err(error) => panic!("no server message: {error}; {}", self.context()),
            }
        }
    }

    fn context(&self) -> String {
        format!("pending={:?}; stderr={:?}", self.pending, self.stderr())
    }

    fn join_readers(&mut self) {
        if let Some(thread) = self.stdout_thread.take() {
            thread.join().expect("stdout reader does not panic");
        }
        if let Some(thread) = self.stderr_thread.take() {
            thread.join().expect("stderr reader does not panic");
        }
    }

    fn assert_reader_clean(&self) {
        if let Some(error) = self
            .reader_error
            .lock()
            .expect("reader error lock")
            .as_ref()
        {
            panic!(
                "stdio reader failed after the last awaited message: {error}; {}",
                self.context()
            );
        }
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.join_readers();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn write_value(input: &Arc<Mutex<ChildStdin>>, value: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(value)?;
    write_body(input, &body)
}

fn write_body(input: &Arc<Mutex<ChildStdin>>, body: &[u8]) -> std::io::Result<()> {
    let mut input = input.lock().expect("stdin lock");
    write!(input, "Content-Length: {}\r\n\r\n", body.len())?;
    input.write_all(body)?;
    input.flush()
}

fn read_frame(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, String> {
    let mut content_length = None;
    let mut content_type_seen = false;
    let mut header_seen = false;
    loop {
        let mut line = Vec::new();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| format!("read stdout header: {error}"))?;
        if read == 0 {
            return if header_seen {
                Err("stdout ended inside a frame header".into())
            } else {
                Ok(None)
            };
        }
        if !line.ends_with(b"\r\n") {
            return Err(format!(
                "stdout header does not end with CRLF: {:?}",
                String::from_utf8_lossy(&line)
            ));
        }
        line.truncate(line.len() - 2);
        if line.is_empty() {
            if !header_seen {
                return Err("stdout contains bytes outside a frame".into());
            }
            break;
        }
        header_seen = true;
        let header = std::str::from_utf8(&line)
            .map_err(|error| format!("stdout header is not UTF-8: {error}"))?;
        if let Some(value) = header.strip_prefix("Content-Length:") {
            if content_length.is_some() {
                return Err("duplicate Content-Length header".into());
            }
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|error| format!("invalid Content-Length: {error}"))?,
            );
        } else if header.starts_with("Content-Type:") {
            if content_type_seen {
                return Err("duplicate Content-Type header".into());
            }
            content_type_seen = true;
        } else {
            return Err(format!("unexpected stdout header: {header:?}"));
        }
    }
    let content_length = content_length.ok_or("missing Content-Length header")?;
    let mut body = vec![0; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|error| format!("read stdout body: {error}"))?;
    Ok(Some(body))
}
