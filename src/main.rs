//! Binary entry point: delegates to the CLI runner.
//!
//! All argument parsing, dispatch, platform checks, output, and exit-code
//! mapping live in `cli::run`; this file is a thin async entry.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    simtop::cli::run().await
}
