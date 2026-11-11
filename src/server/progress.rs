//! Work-done `$/progress` sequences.

use lsp_server::Notification;
use lsp_types::ProgressToken;
use serde_json::{json, Value};

use crate::transport::Sink;

/// A begun `$/progress` sequence. Every one owes an `end`, on completion,
/// cancellation, coalescing, worker panic and shutdown alike — so the `end`
/// lives in `Drop` and no exit path can forget it.
pub(super) struct Progress {
    sender: Sink,
    token: ProgressToken,
    cancellable: bool,
    ended: bool,
}

impl Progress {
    /// Announce the work under a token the client already knows.
    pub fn begin(
        sender: &Sink,
        token: ProgressToken,
        title: &str,
        percentage: u32,
        message: Option<&str>,
        cancellable: bool,
    ) -> Progress {
        let progress = Progress {
            sender: sender.clone(),
            token,
            cancellable,
            ended: false,
        };
        let mut value = json!({ "kind": "begin", "title": title, "percentage": percentage });
        progress.send(&mut value, message);
        progress
    }

    pub fn report(&self, percentage: u32, message: Option<&str>) {
        let mut value = json!({ "kind": "report", "percentage": percentage });
        self.send(&mut value, message);
    }

    /// End the sequence with a closing message instead of a bare `end`.
    pub fn finish(mut self, message: &str) {
        self.end(Some(message));
    }

    fn end(&mut self, message: Option<&str>) {
        if !std::mem::replace(&mut self.ended, true) {
            let mut value = json!({ "kind": "end" });
            if let Some(message) = message {
                value["message"] = message.into();
            }
            self.notify(value);
        }
    }

    fn send(&self, value: &mut Value, message: Option<&str>) {
        if let Some(message) = message {
            value["message"] = message.into();
        }
        if self.cancellable {
            value["cancellable"] = true.into();
        }
        self.notify(value.take());
    }

    fn notify(&self, value: Value) {
        notify(
            &self.sender,
            super::method::PROGRESS,
            json!({ "token": self.token, "value": value }),
        );
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.end(None);
    }
}

pub(super) fn notify(sender: &Sink, method: &str, params: Value) {
    sender.send(Notification {
        method: method.into(),
        params,
    });
}
