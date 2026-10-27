use std::process::ExitCode;

use venue_reader::{Args, SERVER_NAME, SERVER_VERSION};

const USAGE: &str = "\
Usage: venue-reader --stdio [options]

Options:
  --stdio                 Communicate over stdin/stdout (required)
  --log-file <path>       Append logs to <path> (created 0600)
  --log-level <level>     off | error | warn | info | debug | trace  (default: info)
  --version               Print version and exit
  --help                  Print this help and exit
";

fn main() -> ExitCode {
    match parse(std::env::args().skip(1)) {
        Ok(Some(args)) => venue_reader::run(args),
        Ok(None) => ExitCode::SUCCESS, // --version / --help already printed
        Err(msg) => {
            eprintln!("{SERVER_NAME}: {msg}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// `Ok(Some(args))` to run, `Ok(None)` when a terminal flag (`--version`/`--help`)
/// was handled, `Err(msg)` on a usage error.
fn parse(mut it: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut stdio = false;
    let mut log_file = None;
    let mut log_level = log::LevelFilter::Info;

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--stdio" => stdio = true,
            "--version" => {
                println!("{SERVER_NAME} {SERVER_VERSION}");
                return Ok(None);
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "--log-file" => {
                let v = it.next().ok_or("--log-file requires a path")?;
                log_file = Some(v.into());
            }
            "--log-level" => {
                let v = it.next().ok_or("--log-level requires a level")?;
                log_level = v.parse().map_err(|_| format!("invalid log level: {v}"))?;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(Some(Args {
        stdio,
        log_file,
        log_level,
    }))
}
