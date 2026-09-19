//! `fix-lsp` — editor-independent LSP server for FIX protocol logs.
//!
//! Stage 1 is the server skeleton: byte-safe transport, a resilient main loop,
//! an incremental document model, the configuration pull flow, and a hover stub.
//! Dictionaries, codec, and rules arrive in later stages.

pub mod codec;
pub mod config;
pub mod ctx;
pub mod dictionary;
pub mod document;
pub mod logging;
pub mod overrides;
pub mod render;
pub mod rules;
pub mod server;
pub mod tag;
pub mod transport;

use std::path::PathBuf;
use std::process::ExitCode;

pub const SERVER_NAME: &str = "fix-lsp";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Parsed command-line arguments.
pub struct Args {
    pub stdio: bool,
    pub log_file: Option<PathBuf>,
    pub log_level: log::LevelFilter,
}

/// Entry point shared by the binary.
pub fn run(args: Args) -> ExitCode {
    logging::init(args.log_level, args.log_file.as_deref());
    if !args.stdio {
        eprintln!("{}: only --stdio transport is supported", SERVER_NAME);
        return ExitCode::from(2);
    }
    let path = config::user_file::process_location();
    let startup = match server::Startup::load(path) {
        Ok(startup) => startup,
        Err(error) => {
            eprintln!("fix-lsp: {error}");
            return ExitCode::FAILURE;
        }
    };
    let (link, threads) = transport::stdio();
    match server::serve_started(link, startup) {
        server::ServerExit::Normal => {
            threads.join();
            ExitCode::SUCCESS
        }
        server::ServerExit::Fatal(_) => ExitCode::FAILURE,
    }
}
