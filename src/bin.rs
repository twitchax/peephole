//! The `peephole` binary: parse arguments, set up tracing, call the library. Everything worth
//! testing lives in `peephole`; this file stays thin.

// `cargo llvm-cov` on nightly sets `coverage_nightly`; elsewhere this is inert, so the source builds
// on stable. Enable the feature only in a crate root that uses `#[coverage(off)]`: an unused
// feature fails the `unused` lint group on current nightlies.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use std::{io::IsTerminal as _, process::ExitCode};

use clap::Parser;
use tracing_subscriber::EnvFilter;

use peephole::greet;

/// An MCP server that lets an agent see and drive a remote desktop over VNC, including macOS Screen Sharing
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Who to greet.
    #[arg(long, env = "PEEPHOLE_NAME", default_value = "world")]
    name: String,
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn main() -> ExitCode {
    // Logs go to stderr so stdout stays clean for output; `RUST_LOG` overrides the default level.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    let cli = Cli::parse();
    match greet(&cli.name) {
        Ok(greeting) => {
            println!("{greeting}");
            ExitCode::SUCCESS
        }
        // `{:#}` prints the whole context chain on one line.
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
