//! The `peephole` binary: parse arguments, set up tracing, call the library. Everything worth
//! testing lives in `peephole`; this file stays thin.

// `cargo llvm-cov` on nightly sets `coverage_nightly`; elsewhere this is inert, so the source builds
// on stable. Enable the feature only in a crate root that uses `#[coverage(off)]`: an unused
// feature fails the `unused` lint group on current nightlies.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use std::{io::IsTerminal as _, path::PathBuf, process::ExitCode};

use clap::Parser;
use tracing_subscriber::EnvFilter;

use peephole::{
    base::Void,
    credentials::{Credentials, PASSWORD_ENV, USER_ENV},
    mcp, screenshot,
    session::{Idle, SessionConfig},
};

/// An MCP server that lets an agent see and drive a remote desktop over VNC, including macOS
/// Screen Sharing with Apple's own login.
///
/// It speaks MCP on stdin/stdout. Credentials come only from the environment (`PEEPHOLE_USER`
/// for Apple's login, `PEEPHOLE_PASSWORD`) or from `--password-file`, never from arguments, and
/// are never logged.
#[derive(Debug, Parser)]
#[command(version, about, long_about)]
struct Cli {
    /// The VNC server's host name or address.
    #[arg(long, env = "PEEPHOLE_HOST")]
    host: String,

    /// The VNC server's port.
    #[arg(long, env = "PEEPHOLE_PORT", default_value_t = 5900)]
    port: u16,

    /// Read the password from this file (trailing newlines ignored) instead of `PEEPHOLE_PASSWORD`.
    #[arg(long, env = "PEEPHOLE_PASSWORD_FILE")]
    password_file: Option<PathBuf>,

    /// Disable every input tool, leaving `screenshot`, `screen_info` and `wait_for_change`.
    #[arg(long, env = "PEEPHOLE_VIEW_ONLY")]
    view_only: bool,

    /// The long edge of screen space (and of a default screenshot), in pixels.
    #[arg(long, env = "PEEPHOLE_MAX_EDGE", default_value_t = screenshot::DEFAULT_MAX_EDGE, value_parser = clap::value_parser!(u32).range(64..=8192))]
    max_edge: u32,
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[tokio::main]
async fn main() -> ExitCode {
    // Logs go to stderr: stdout carries the MCP protocol. `RUST_LOG` overrides the default level.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        // `{:#}` prints the whole context chain on one line.
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
async fn run(cli: Cli) -> Void {
    let credentials = Credentials::load(cli.password_file.as_deref())?;
    tracing::info!(
        user = credentials.username.is_some(),
        password = credentials.password.is_some(),
        "credentials from {USER_ENV} / {PASSWORD_ENV} or the password file (values never logged)"
    );
    let config = SessionConfig {
        host: cli.host,
        port: cli.port,
        credentials,
        view_only: cli.view_only,
        idle: Idle::default(),
    };
    mcp::serve(config, cli.max_edge).await
}
