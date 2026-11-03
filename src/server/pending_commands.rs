//! Bounded waiting for commands that need committed base configuration.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use lsp_server::{RequestId, ResponseError};

use super::config_pull::request_failed;

const MAX_COMMANDS: usize = 64;
const MAX_BYTES: usize = 1024 * 1024;
const WAIT: Duration = Duration::from_secs(5);
pub(super) const REJECTED: &str = "configuration is not ready; send a valid configuration update";
pub(super) const EXPIRED: &str = "configuration command wait deadline exceeded (5 seconds)";
const CAPACITY: &str = "pending configuration command capacity exceeded";

/// Parsed commands retained without their original JSON request.
pub enum BaseCommand {
    ListDictionaries,
    #[cfg(test)]
    Panic,
    DescribeTag {
        tag: u32,
        value: Option<String>,
    },
}

impl BaseCommand {
    fn bytes(&self) -> usize {
        match self {
            Self::ListDictionaries => 0,
            #[cfg(test)]
            Self::Panic => 0,
            Self::DescribeTag { value, .. } => value.as_deref().map_or(0, str::len),
        }
    }
}

pub(super) struct PendingCommand {
    pub id: RequestId,
    pub command: BaseCommand,
    deadline: Instant,
    bytes: usize,
}

impl PendingCommand {
    pub fn expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }
}

#[derive(Default)]
pub(super) struct PendingCommands {
    entries: VecDeque<PendingCommand>,
    bytes: usize,
}

impl PendingCommands {
    pub fn contains(&self, id: &RequestId) -> bool {
        self.entries.iter().any(|entry| &entry.id == id)
    }

    pub fn push(
        &mut self,
        id: RequestId,
        mut command: BaseCommand,
        now: Instant,
    ) -> Result<(), ResponseError> {
        if self.entries.len() == MAX_COMMANDS {
            return Err(request_failed(CAPACITY));
        }
        let mut counter = ByteCount(0);
        serde_json::to_writer(&mut counter, &id).map_err(|_| request_failed(CAPACITY))?;
        let bytes = counter.0.checked_add(command.bytes());
        let total = bytes.and_then(|bytes| self.bytes.checked_add(bytes));
        let Some(total) = total.filter(|total| *total <= MAX_BYTES) else {
            return Err(request_failed(CAPACITY));
        };
        if let BaseCommand::DescribeTag {
            value: Some(value), ..
        } = &mut command
        {
            value.shrink_to_fit();
        }
        self.entries.push_back(PendingCommand {
            id,
            command,
            deadline: now + WAIT,
            bytes: total - self.bytes,
        });
        self.bytes = total;
        Ok(())
    }

    pub fn remove(&mut self, id: &RequestId) {
        if let Some(index) = self.entries.iter().position(|entry| &entry.id == id) {
            let entry = self.entries.remove(index).expect("located entry");
            self.bytes -= entry.bytes;
        }
    }

    pub fn pop(&mut self) -> Option<PendingCommand> {
        let entry = self.entries.pop_front()?;
        self.bytes -= entry.bytes;
        Some(entry)
    }

    pub fn expired(&self, now: Instant) -> bool {
        self.entries
            .front()
            .is_some_and(|entry| now >= entry.deadline)
    }

    pub fn tick_in(&self, now: Instant) -> Duration {
        self.entries
            .front()
            .map_or(Duration::from_secs(3600), |entry| {
                entry.deadline.saturating_duration_since(now)
            })
    }
}

struct ByteCount(usize);

impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("byte count overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{method, testing, ServerState};
    use crossbeam_channel::{bounded, Receiver};
    use lsp_server::{ErrorCode, Message, Request, Response};
    use serde_json::json;

    fn state() -> (ServerState, Receiver<Message>) {
        let (sender, receiver) = crossbeam_channel::unbounded();
        (ServerState::new(sender), receiver)
    }

    fn request(state: &mut ServerState, id: i32, tag: u32) {
        state.on_request(Request::new(
            id.into(),
            method::EXECUTE_COMMAND.into(),
            json!({
                "command": "fix.describeTag", "arguments": [tag]
            }),
        ));
    }

    fn responses(receiver: &Receiver<Message>) -> Vec<Response> {
        receiver
            .try_iter()
            .filter_map(|message| match message {
                Message::Response(response) => Some(response),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn encoded_ids_and_values_have_exact_admission_boundaries() {
        let now = Instant::now();
        let id: RequestId = "\"é\n".to_owned().into();
        let id_bytes = serde_json::to_vec(&id).unwrap().len();
        let mut queue = PendingCommands::default();
        let command = |bytes| BaseCommand::DescribeTag {
            tag: 54,
            value: Some("a".repeat(bytes)),
        };
        assert!(queue
            .push(id.clone(), command(MAX_BYTES - id_bytes + 1), now)
            .is_err());
        assert_eq!(queue.bytes, 0);
        queue
            .push(id.clone(), command(MAX_BYTES - id_bytes), now)
            .unwrap();
        assert_eq!(queue.bytes, MAX_BYTES);
        assert!(queue
            .push(2.into(), BaseCommand::ListDictionaries, now)
            .is_err());
        queue.remove(&id);
        assert_eq!(queue.bytes, 0);
        queue.push(id, command(MAX_BYTES - id_bytes), now).unwrap();
        queue.pop().unwrap();
        assert_eq!(queue.bytes, 0);
        let oversized_id: RequestId = "a".repeat(MAX_BYTES).into();
        assert!(queue
            .push(oversized_id, BaseCommand::ListDictionaries, now)
            .is_err());
        assert!(queue.entries.is_empty());
    }

    #[test]
    fn count_capacity_and_cancelled_slots_are_reusable() {
        let now = Instant::now();
        let mut queue = PendingCommands::default();
        for index in 0..MAX_COMMANDS {
            queue
                .push((index as i32).into(), BaseCommand::ListDictionaries, now)
                .unwrap();
        }
        assert!(queue
            .push(100.into(), BaseCommand::ListDictionaries, now)
            .is_err());
        queue.remove(&10.into());
        queue.remove(&10.into());
        queue
            .push(100.into(), BaseCommand::ListDictionaries, now)
            .unwrap();
        while queue.pop().is_some() {}
        assert_eq!(queue.bytes, 0);
    }

    #[test]
    fn deadline_is_fixed_and_expiry_wins_at_readiness_equality() {
        let (mut state, receiver) = state();
        request(&mut state, 1, 35);
        let deadline = state.pending_commands.entries.front().unwrap().deadline;
        assert_eq!(state.pending_commands.tick_in(deadline), Duration::ZERO);
        state.process_pending_commands(deadline - Duration::from_nanos(1));
        assert!(responses(&receiver).is_empty());
        assert_eq!(
            state.pending_commands.entries.front().unwrap().deadline,
            deadline
        );
        state.config.committed = true;
        state.process_pending_commands(deadline);
        state.process_pending_commands(deadline + WAIT);
        let replies = responses(&receiver);
        assert_eq!(replies.len(), 1);
        let error = replies[0].response_result.as_ref().unwrap_err();
        assert_eq!(error.code, ErrorCode::RequestFailed as i32);
        assert_eq!(error.message, EXPIRED);
        assert_eq!(state.pending_commands.bytes, 0);
        request(&mut state, 1, 35);
        assert!(responses(&receiver).pop().unwrap().response_result.is_ok());
    }

    #[test]
    fn deferred_errors_and_panics_complete_once_and_release_capacity() {
        let (mut state, receiver) = state();
        request(&mut state, 1, 9999);
        state.req_queue.incoming.register(2.into(), ());
        state
            .pending_commands
            .push(2.into(), BaseCommand::Panic, Instant::now())
            .unwrap();
        state.config.committed = true;
        state.process_pending_commands(Instant::now());
        state.process_pending_commands(Instant::now());
        let replies = responses(&receiver);
        assert_eq!(replies.len(), 2);
        assert_eq!(
            replies[0].response_result.as_ref().unwrap_err().code,
            ErrorCode::RequestFailed as i32
        );
        assert_eq!(
            replies[1].response_result.as_ref().unwrap_err().code,
            ErrorCode::InternalError as i32
        );
        assert_eq!(state.pending_commands.bytes, 0);
        request(&mut state, 3, 35);
        assert!(responses(&receiver).pop().unwrap().response_result.is_ok());
    }

    #[test]
    fn fatal_precedes_readiness_and_rejection_clears_all_pending_requests() {
        for fatal in [false, true] {
            let (mut state, receiver) = state();
            request(&mut state, 1, 35);
            request(&mut state, 2, 54);
            state.config.committed = true;
            if fatal {
                state.fatal = Some("configuration worker disconnected".into());
            } else {
                state.config.rejected = true;
            }
            state.process_pending_commands(Instant::now());
            state.process_pending_commands(Instant::now());
            let replies = responses(&receiver);
            assert_eq!(replies.len(), 2);
            for reply in replies {
                let error = reply.response_result.unwrap_err();
                assert_eq!(error.code, ErrorCode::RequestFailed as i32);
                assert_eq!(
                    error.message,
                    if fatal {
                        "configuration failed; server is exiting"
                    } else {
                        REJECTED
                    }
                );
            }
            assert_eq!(state.pending_commands.bytes, 0);
            assert!(!state.req_queue.incoming.has_pending());
        }
    }

    fn hold_loader(state: &ServerState) -> crossbeam_channel::Sender<()> {
        let (entered, arrived) = bounded(1);
        let (release, wait) = bounded(1);
        state
            .loader
            .send(crate::server::external::Job::Barrier {
                entered,
                release: wait,
            })
            .unwrap();
        arrived.recv_timeout(Duration::from_secs(2)).unwrap();
        release
    }

    #[test]
    fn initial_push_loading_waits_for_the_first_commit() {
        let (mut state, receiver) = state();
        let release = hold_loader(&state);
        state.apply_base_value(&json!({"documentLink":{"url":"https://example.org/tag/{tag}"}}));
        request(&mut state, 1, 35);
        state.process_pending_commands(Instant::now());
        assert!(responses(&receiver).is_empty());
        release.send(()).unwrap();
        testing::settle(&mut state);
        state.process_pending_commands(Instant::now());
        let result = responses(&receiver).pop().unwrap().response_result.unwrap();
        assert!(result
            .as_str()
            .unwrap()
            .contains("https://example.org/tag/35"));
        assert_eq!(state.pending_commands.bytes, 0);
    }

    #[test]
    fn submitted_refresh_uses_previous_commit_without_waiting_for_loader() {
        let (mut state, receiver) = state();
        state.apply_base_value(&json!({"documentLink":{"url":"https://example.org/old/{tag}"}}));
        testing::settle(&mut state);
        state.client.configuration = true;
        state.pull_base_config();
        let release = hold_loader(&state);
        request(&mut state, 1, 35);
        assert!(responses(&receiver).is_empty());
        let generation = state.config.global_generation;
        state.on_base_reply(
            generation,
            Ok(json!([{ "documentLink": {"url":"https://example.org/new/{tag}"}}])),
        );
        state.process_pending_commands(Instant::now());
        let result = responses(&receiver).pop().unwrap().response_result.unwrap();
        assert!(result
            .as_str()
            .unwrap()
            .contains("https://example.org/old/35"));
        release.send(()).unwrap();
        testing::settle(&mut state);
        request(&mut state, 2, 35);
        let result = responses(&receiver).pop().unwrap().response_result.unwrap();
        assert!(result
            .as_str()
            .unwrap()
            .contains("https://example.org/new/35"));
    }
}
