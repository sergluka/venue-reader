use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};
use lsp_server::{Connection, Message, Request, ResponseError};
use lsp_types::Uri;
use serde_json::{json, Value};

use super::dispatch::Dispatched;
use super::external::{self, Job, Loader};
use super::{testing, ServerExit, ServerState, Startup};

fn hold(loader: &Loader) -> Sender<()> {
    let (entered, arrived) = bounded(1);
    let (release, wait) = bounded(1);
    loader
        .send(Job::Barrier {
            entered,
            release: wait,
        })
        .unwrap();
    arrived
        .recv_timeout(Duration::from_secs(2))
        .expect("loader reached barrier");
    release
}

#[test]
fn workspace_changes_preserve_an_outstanding_global_pull() {
    let (mut state, uri) = testing::state_with(testing::HEARTBEAT);
    state.client.configuration = true;
    state.pull_base_config();
    let generation = state.config.pending.as_ref().unwrap().generation;
    let started = state.config.pending.as_ref().unwrap().started;
    state.on_notification(lsp_server::Notification::new(
        "workspace/didChangeWorkspaceFolders".into(),
        json!({"event":{"added":[{"uri":"file:///workspace","name":"workspace"}],"removed":[]}}),
    ));
    assert_eq!(
        state.config.pending.as_ref().unwrap().generation,
        generation
    );
    assert_eq!(state.config.pending.as_ref().unwrap().started, started);
    state.on_base_reply(generation, Ok(json!([{"hover":{"enabled":false}}])));
    testing::settle(&mut state);
    assert!(!state.config.base.hover.enabled);
    assert!(!state.effective_config(&uri).unwrap().hover.enabled);
    assert!(state.config.pending.is_none());
}

#[test]
fn global_updates_during_loading_coalesce_to_the_latest_value() {
    let (mut state, uri) = testing::state_with(testing::HEARTBEAT);
    let barrier = hold(&state.loader);
    state.apply_base_value(&json!({"hover":{"enabled":false}}));
    let generation = state.config.pending.as_ref().unwrap().generation;
    state.apply_base_value(&json!({"semanticTokens":{"enabled":false}}));
    state.apply_base_value(&json!({"hover":{"enabled":true},"diagnostics":{"enabled":false}}));
    assert_eq!(
        state.config.pending.as_ref().unwrap().generation,
        generation
    );
    barrier.send(()).unwrap();
    testing::settle(&mut state);
    let effective = state.effective_config(&uri).unwrap();
    assert!(effective.hover.enabled);
    assert!(!effective.diagnostics.enabled);
    assert!(effective.semantic_tokens.enabled);
}

#[test]
fn workspace_changes_during_loading_rediscover_with_the_committed_global_settings() {
    let directory = std::env::temp_dir().join(format!(
        "fix-lsp-workspace-during-load-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(directory.join("logs")).unwrap();
    std::fs::write(
        directory.join(".fix-lsp.toml"),
        "semanticTokens.enabled = false",
    )
    .unwrap();
    let root = Uri::from_file_path(&directory).unwrap();
    let document = Uri::from_file_path(directory.join("logs/orders.fix")).unwrap();
    let (mut state, _) = testing::state_with(testing::HEARTBEAT);
    open(&mut state, &document, testing::HEARTBEAT);
    testing::settle(&mut state);
    assert!(
        state
            .effective_config(&document)
            .unwrap()
            .semantic_tokens
            .enabled
    );
    let barrier = hold(&state.loader);
    state.apply_base_value(&json!({"hover":{"enabled":false}}));
    let generation = state.config.pending.as_ref().unwrap().generation;
    state.on_notification(lsp_server::Notification::new(
        "workspace/didChangeWorkspaceFolders".into(),
        json!({"event":{"added":[{"uri":root.as_str(),"name":"workspace"}],"removed":[]}}),
    ));
    assert_eq!(
        state.config.pending.as_ref().unwrap().generation,
        generation
    );
    barrier.send(()).unwrap();
    testing::settle(&mut state);
    let effective = state.effective_config(&document).unwrap();
    assert!(!effective.hover.enabled);
    assert!(!effective.semantic_tokens.enabled);
    std::fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn dictionary_selection_preserves_an_outstanding_global_pull() {
    let (mut state, uri) = testing::state_with(testing::HEARTBEAT);
    state.client.configuration = true;
    state.pull_base_config();
    let batch = state.config.pending.as_ref().unwrap();
    let generation = batch.generation;
    let started = batch.started;
    state.on_request(Request::new(
        71.into(),
        "workspace/executeCommand".into(),
        json!({"command":"fix.useDictionary","arguments":["FIX.4.2"]}),
    ));
    assert_eq!(
        state.config.pending.as_ref().unwrap().generation,
        generation
    );
    assert_eq!(state.config.pending.as_ref().unwrap().started, started);
    state.on_base_reply(generation, Ok(json!([{"hover":{"enabled":false}}])));
    testing::settle(&mut state);
    assert!(!state.config.base.hover.enabled);
    assert!(!state.effective_config(&uri).unwrap().hover.enabled);
    assert_eq!(state.config.session.as_deref(), Some("FIX.4.2"));
}

fn open(state: &mut ServerState, uri: &Uri, text: &str) -> Uri {
    state.on_did_open(json!({"textDocument":{"uri":uri.as_str(),"version":1,"text":text}}));
    uri.clone()
}

fn receive(state: &ServerState) -> external::Report {
    state
        .loader
        .reports
        .recv_timeout(Duration::from_secs(2))
        .expect("loader report")
}

fn describe(state: &mut ServerState, uri: &Uri) -> Value {
    testing::answered(state.execute_command_task(&json!({
        "command":"fix.describeOverrides", "arguments":[uri.as_str()]
    })))
}

#[test]
fn late_opens_keep_batch_membership_and_receive_a_fresh_budget() {
    assert_eq!(external::TIMEOUT, Duration::from_millis(500));
    let (mut state, first) = testing::state_with(testing::HEARTBEAT);
    let barrier = hold(&state.loader);
    state.apply_base_value(&json!({"hover":{"enabled":false}}));
    let generation = state.config.pending.as_ref().unwrap().generation;
    let started = Instant::now() - Duration::from_millis(300);
    state.config.pending.as_mut().unwrap().started = started;
    let second = open(
        &mut state,
        &testing::file_uri("late-a.fix"),
        "8=FIX.4.4|35=0|",
    );
    let third = open(
        &mut state,
        &testing::file_uri("late-b.fix"),
        "8=FIX.4.4|35=0|",
    );
    assert_eq!(state.config.pending.as_ref().unwrap().started, started);
    assert_eq!(
        state.config.pending.as_ref().unwrap().generation,
        generation
    );
    assert!(state.effective_config(&second).is_none());
    assert!(state.effective_config(&third).is_none());
    assert!(state.effective_config(&first).unwrap().hover.enabled);
    barrier.send(()).unwrap();
    let prepared = receive(&state);
    state.on_configuration_report(prepared);
    assert!(state.fatal.is_none());
    assert!(!state.effective_config(&first).unwrap().hover.enabled);
    assert!(
        state.effective_config(&second).is_none(),
        "not included in the preceding commit"
    );
    let next = state.config.pending.as_ref().unwrap();
    assert!(next.generation > generation);
    assert!(next.started > started + Duration::from_millis(200));
    let fourth = open(
        &mut state,
        &testing::file_uri("later-again.fix"),
        "8=FIX.4.4|35=0|",
    );
    let prepared = receive(&state);
    state.on_configuration_report(prepared);
    assert!(state.effective_config(&second).is_some());
    assert!(state.effective_config(&third).is_some());
    assert!(state.effective_config(&fourth).is_none());
    testing::settle(&mut state);
    assert!(!state.effective_config(&fourth).unwrap().hover.enabled);
}

#[test]
fn enabling_modelines_uses_edits_received_while_the_source_was_disabled() {
    let (mut state, uri) = testing::state_with("# fix: hover.enabled=false\n8=FIX.4.4|35=0|");
    state.apply_base_value(&json!({"overrides":{"modeline":false}}));
    testing::settle(&mut state);
    let barrier = hold(&state.loader);
    state.apply_base_value(&json!({}));
    state.on_did_change(json!({"textDocument":{"uri":uri.as_str(),"version":2},"contentChanges":[{"text":"# fix: hover.enabled=true\n8=FIX.4.4|35=0|"}]}));
    barrier.send(()).unwrap();
    testing::settle(&mut state);
    assert!(state.effective_config(&uri).unwrap().hover.enabled);
    assert_eq!(
        describe(&mut state, &uri)["sources"]["hover.enabled"]["layer"],
        "modeline"
    );
}

#[test]
fn body_only_edits_do_not_invalidate_a_configuration_batch() {
    let (mut state, uri) = testing::state_with(testing::HEARTBEAT);
    let revision = state.config.revisions[&uri];
    let barrier = hold(&state.loader);
    state.apply_base_value(&json!({"hover":{"enabled":false}}));
    state.on_did_change(json!({
        "textDocument":{"uri":uri.as_str(),"version":2},
        "contentChanges":[{"text":"8=FIX.4.4|35=1|"}]
    }));
    assert_eq!(state.config.revisions[&uri], revision);
    barrier.send(()).unwrap();
    testing::settle(&mut state);
    assert!(!state.effective_config(&uri).unwrap().hover.enabled);
    assert_eq!(state.snapshot_for(&uri).unwrap().version, 2);
}

#[test]
fn a_closed_and_reopened_uri_cannot_receive_the_old_lifetime() {
    let (mut state, uri) = testing::state_with(testing::HEARTBEAT);
    let barrier = hold(&state.loader);
    state.apply_base_value(&json!({"hover":{"enabled":false}}));
    let lifetime = state.config.lifetimes[&uri];
    state.on_did_close(json!({"textDocument":{"uri":uri.as_str()}}));
    open(
        &mut state,
        &uri,
        "# fix: hover.enabled=true\n8=FIX.4.4|35=0|",
    );
    assert_ne!(state.config.lifetimes[&uri], lifetime);
    barrier.send(()).unwrap();
    let old = receive(&state);
    state.on_configuration_report(old);
    assert!(state.effective_config(&uri).is_none());
    testing::settle(&mut state);
    assert!(state.effective_config(&uri).unwrap().hover.enabled);
}

#[test]
fn rejection_leaves_new_documents_unprocessed_until_a_valid_update() {
    let (mut state, _) = testing::state_with(testing::HEARTBEAT);
    state.client.configuration = true;
    state.pull_base_config();
    let generation = state.config.pending.as_ref().unwrap().generation;
    let uri = open(
        &mut state,
        &testing::file_uri("waiting.fix"),
        "8=FIX.4.4|35=0|",
    );
    state.on_base_reply(generation, Ok(json!([{"hover":{"enabled":"invalid"}}])));
    assert!(state.config.pending.is_none());
    assert!(state.effective_config(&uri).is_none());
    let result = state.hover_task(
        &json!({"textDocument":{"uri":uri.as_str()},"position":{"line":0,"character":0}}),
    );
    assert!(matches!(result, Dispatched::Now(Err(error)) if error.code == -32803));
    state.on_did_change(json!({"textDocument":{"uri":uri.as_str(),"version":2},"contentChanges":[{"text":"# fix: hover.enabled=false\n8=FIX.4.4|35=0|"}]}));
    assert!(
        state.config.pending.is_none(),
        "no automatic retry of the old base"
    );
    state.apply_base_value(&json!({}));
    testing::settle(&mut state);
    assert_eq!(state.snapshot_for(&uri).unwrap().version, 2);
    assert!(!state.effective_config(&uri).unwrap().hover.enabled);
}

#[test]
fn describe_uses_committed_state_without_loader_work_or_pending_batch_changes() {
    let (mut state, uri) = testing::state_with(testing::HEARTBEAT);
    let before = describe(&mut state, &uri);
    let context = state.config.documents[&uri].ctx.clone();
    let barrier = hold(&state.loader);
    state.apply_base_value(&json!({"hover":{"enabled":false}}));
    let generation = state.config.pending.as_ref().unwrap().generation;
    let started = state.config.pending.as_ref().unwrap().started;
    let jobs = state.loader.jobs_submitted.load(Ordering::Relaxed);
    assert_eq!(describe(&mut state, &uri), before);
    assert_eq!(state.loader.jobs_submitted.load(Ordering::Relaxed), jobs);
    assert_eq!(
        state.config.pending.as_ref().unwrap().generation,
        generation
    );
    assert_eq!(state.config.pending.as_ref().unwrap().started, started);
    assert!(Arc::ptr_eq(&context, &state.config.documents[&uri].ctx));
    barrier.send(()).unwrap();
    testing::settle(&mut state);
    assert_eq!(
        describe(&mut state, &uri)["effective"]["hover.enabled"],
        false
    );
}

#[test]
fn a_loader_panic_becomes_a_fatal_configuration_result() {
    let (mut state, _) = testing::state_with(testing::HEARTBEAT);
    state.client.configuration = true;
    state.pull_base_config();
    let generation = state.config.pending.as_ref().unwrap().generation;
    state.loader.send(Job::Panic(generation)).unwrap();
    let report = receive(&state);
    state.on_configuration_report(report);
    assert!(state.fatal.as_ref().unwrap().contains("panic"));
}

#[test]
fn a_client_configuration_error_is_fatal_without_committing() {
    let (mut state, uri) = testing::state_with(testing::HEARTBEAT);
    let context = state.config.documents[&uri].ctx.clone();
    state.client.configuration = true;
    state.pull_base_config();
    let generation = state.config.pending.as_ref().unwrap().generation;
    state.on_base_reply(
        generation,
        Err(ResponseError {
            code: -32603,
            message: "client unavailable".into(),
            data: None,
        }),
    );
    assert!(state.fatal.as_ref().unwrap().contains("client unavailable"));
    assert!(Arc::ptr_eq(&context, &state.config.documents[&uri].ctx));
}

#[test]
fn describe_does_not_observe_unrefreshed_user_file_changes() {
    let directory =
        std::env::temp_dir().join(format!("fix-lsp-describe-readonly-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("fix-lsp.toml");
    std::fs::write(&path, "[hover]\nenabled = false\n").unwrap();
    let (mut state, uri) = testing::state_with(testing::HEARTBEAT);
    state.user_path = Some(path.clone());
    state.apply_base_value(&json!({}));
    testing::settle(&mut state);
    let before = describe(&mut state, &uri);
    assert_eq!(before["effective"]["hover.enabled"], false);
    let jobs = state.loader.jobs_submitted.load(Ordering::Relaxed);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(describe(&mut state, &uri), before);
    assert_eq!(state.loader.jobs_submitted.load(Ordering::Relaxed), jobs);
    state.apply_base_value(&json!({}));
    testing::settle(&mut state);
    assert_eq!(
        describe(&mut state, &uri)["effective"]["hover.enabled"],
        true
    );
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn fatal_server_return_does_not_join_a_held_configuration_loader() {
    let loader = Loader::start();
    let release = hold(&loader);
    let startup = Startup {
        loader,
        user: external::empty_user(),
        path: None,
    };
    let (server, client) = Connection::memory();
    let (finished, result): (Sender<ServerExit>, Receiver<ServerExit>) = bounded(1);
    let thread = std::thread::spawn(move || {
        let exit = super::serve_started(
            crate::transport::Link {
                receiver: server.receiver,
                sender: server.sender.into(),
            },
            startup,
        );
        finished.send(exit).unwrap();
    });
    client
        .sender
        .send(Message::Request(Request::new(
            1.into(),
            "initialize".into(),
            json!({"capabilities":{}}),
        )))
        .unwrap();
    let exit = result
        .recv_timeout(Duration::from_secs(2))
        .expect("fatal without releasing loader");
    assert!(matches!(exit, ServerExit::Fatal(reason) if reason.contains("500 ms")));
    release.send(()).unwrap();
    thread.join().unwrap();
}

#[test]
fn dictionary_command_responds_synchronously_while_loader_is_busy() {
    let (sender, received) = crossbeam_channel::unbounded::<Message>();
    let mut state = ServerState::new(sender);
    let release = hold(&state.loader);
    state.on_request(Request::new(
        41.into(),
        "workspace/executeCommand".into(),
        json!({
            "command":"fix.useDictionary", "arguments":["FIX.4.2"]
        }),
    ));
    let responses: Vec<_> = received
        .try_iter()
        .filter_map(|message| match message {
            Message::Response(response) => Some(response),
            _ => None,
        })
        .collect();
    assert_eq!(responses.len(), 1);
    assert_eq!(
        responses[0].response_result.as_ref().unwrap()["name"],
        "FIX.4.2"
    );
    assert_eq!(state.config.session.as_deref(), Some("FIX.4.2"));
    release.send(()).unwrap();
}

#[test]
fn rejected_update_keeps_a_committed_session_selection() {
    let (sender, received) = crossbeam_channel::unbounded::<Message>();
    let mut state = ServerState::new(sender);
    state.on_request(Request::new(
        42.into(),
        "workspace/executeCommand".into(),
        json!({
            "command":"fix.useDictionary", "arguments":["FIX.4.2"]
        }),
    ));
    state.apply_base_value(&json!({"hover":{"enabled":"invalid"}}));
    let responses: Vec<_> = received
        .try_iter()
        .filter_map(|message| match message {
            Message::Response(response) => Some(response),
            _ => None,
        })
        .collect();
    assert_eq!(responses.len(), 1);
    assert!(responses[0].response_result.is_ok());
    assert_eq!(state.config.session.as_deref(), Some("FIX.4.2"));
    assert!(state.config.rejected);
}

#[test]
fn a_deferred_document_keeps_its_committed_limits_after_the_global_base_changes() {
    let (mut state, uri) = testing::state_with(testing::HEARTBEAT);
    let first_barrier = hold(&state.loader);
    state.apply_base_value(&json!({"limits":{"semanticTokensFullLines":0,"codeLensLines":0}}));
    state.on_did_change(
        json!({"textDocument":{"uri":uri.as_str(),"version":2},"contentChanges":[{
            "text":format!("# fix: hover.enabled=false\n{}", testing::HEARTBEAT)
        }]}),
    );
    first_barrier.send(()).unwrap();
    let prepared = receive(&state);
    let second_barrier = hold(&state.loader);
    state.on_configuration_report(prepared);
    assert_eq!(state.config.base.limits.semantic_tokens_full_lines, 0);
    assert!(
        state
            .effective_config(&uri)
            .unwrap()
            .limits
            .semantic_tokens_full_lines
            > 0
    );
    let params = json!({"textDocument":{"uri":uri.as_str()}});
    let tokens = testing::answered(state.semantic_tokens_full(&params));
    assert!(
        !tokens["data"].as_array().unwrap().is_empty(),
        "old committed token cap still applies"
    );
    let lenses = testing::answered(state.code_lens_task(&params));
    assert!(
        !lenses.as_array().unwrap().is_empty(),
        "old committed lens cap still applies"
    );
    second_barrier.send(()).unwrap();
    testing::settle(&mut state);
    let tokens = testing::answered(state.semantic_tokens_full(&params));
    assert!(tokens["data"].as_array().unwrap().is_empty());
    let lenses = testing::answered(state.code_lens_task(&params));
    assert!(lenses.as_array().unwrap().is_empty());
}
