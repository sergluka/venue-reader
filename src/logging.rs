//! Logging sink with a runtime-adjustable level (`$/setTrace`).
//!
//! Below `trace`, neither document text nor JSON-RPC bodies are written — callers
//! gate such logging on [`is_trace_allowed`]. The level lives in an atomic so
//! `$/setTrace` can retune it without reinstalling the logger.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use log::{Level, LevelFilter};

enum Sink {
    Stderr,
    File(Mutex<File>),
}

struct Logger {
    sink: Sink,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::max_level()
    }

    fn log(&self, r: &log::Record) {
        // The `log!` macros already gate on `max_level`, so no re-check here.
        let line = format!("[{}] {}\n", r.level(), r.args());
        match &self.sink {
            Sink::Stderr => {
                let _ = std::io::stderr().write_all(line.as_bytes());
            }
            Sink::File(f) => {
                if let Ok(mut f) = f.lock() {
                    let _ = f.write_all(line.as_bytes());
                }
            }
        }
    }

    fn flush(&self) {
        if let Sink::File(f) = &self.sink {
            if let Ok(mut f) = f.lock() {
                let _ = f.flush();
            }
        }
    }
}

/// Install the global logger. Safe to call once; a later call is ignored (the
/// level still updates).
pub fn init(level: LevelFilter, file: Option<&Path>) {
    let sink = match file {
        Some(p) => match open_log_file(p) {
            Ok(f) => Sink::File(Mutex::new(f)),
            Err(e) => {
                eprintln!("fix-lsp: cannot open log file {}: {e}", p.display());
                Sink::Stderr
            }
        },
        None => Sink::Stderr,
    };
    set_level(level);
    let _ = log::set_boxed_logger(Box::new(Logger { sink }));
}

/// Set the max level at runtime.
pub fn set_level(level: LevelFilter) {
    log::set_max_level(level);
}

/// Map an LSP `$/setTrace` value to a level ceiling.
pub fn set_trace_level(value: &str) {
    let level = match value {
        "off" => LevelFilter::Warn,
        "messages" => LevelFilter::Info,
        "verbose" => LevelFilter::Trace,
        _ => return,
    };
    set_level(level);
}

/// Whether document text / JSON-RPC bodies may be logged (only at `trace`).
pub fn is_trace_allowed() -> bool {
    log::max_level() >= Level::Trace
}

fn open_log_file(p: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_trace_level_adjusts_body_logging() {
        set_trace_level("messages");
        assert!(!is_trace_allowed(), "bodies stay off below trace");
        set_trace_level("verbose");
        assert!(is_trace_allowed(), "verbose enables trace-level bodies");
        set_trace_level("off");
        assert!(!is_trace_allowed());
    }

    #[cfg(unix)]
    #[test]
    fn log_file_is_created_0600() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("fix-lsp-log-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let f = open_log_file(&path).unwrap();
        let mode = f.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "log file must be user-only");
        let _ = std::fs::remove_file(&path);
    }
}
