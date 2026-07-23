//! `fhirpg` — import FHIR data into PostgreSQL and work with it relationally.
//!
//! A Rust translation of [fhirbase](https://github.com/fhirbase/fhirbase).
//! Behaviour is specified in `spec/index.md`; the delivery plan and the decision
//! log are in `plan.md`.
//!
//! This entry point stays thin, as `main.go:30-308` does: parse the command
//! line, dispatch, and map an error to an exit code. Everything else lives in
//! the modules below.

// `unwrap_used`, `expect_used`, and `panic` are denied in Cargo.toml so that no
// input-derived path can panic (spec invariant 2). Tests are the one place
// where panicking IS the reporting mechanism, so they are exempt.
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

mod cli;
mod error;

use clap::{CommandFactory, Parser};

use crate::cli::{Cli, Command};
use crate::error::Error;

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    match run(&cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Dispatches to the requested subcommand.
///
/// With no subcommand, prints help and succeeds — fhirbase does the same
/// (`main.go:291-294`), and spec §7 requires exit code 0 for it.
fn run(cli: &Cli) -> error::Result<()> {
    let Some(command) = &cli.command else {
        let mut help = Cli::command();
        // Writing to stdout, not stderr: this is a successful invocation.
        help.print_help()?;
        return Ok(());
    };

    match command {
        Command::Init => Err(Error::NotImplemented {
            command: "init",
            task: "T11",
        }),
        Command::Transform { .. } => Err(Error::NotImplemented {
            command: "transform",
            task: "T7",
        }),
        Command::Load { .. } => Err(Error::NotImplemented {
            command: "load",
            task: "T16",
        }),
        Command::Bulkget { .. } => Err(Error::NotImplemented {
            command: "bulkget",
            task: "T18",
        }),
        Command::Web { .. } => Err(Error::NotImplemented {
            command: "web",
            task: "T20",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_subcommand_succeeds() {
        let cli = Cli::try_parse_from(["fhirpg"]).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            run(&cli).is_ok(),
            "spec §7: no subcommand prints help, exits 0"
        );
    }

    #[test]
    fn unimplemented_commands_name_their_task() {
        let cli = Cli::try_parse_from(["fhirpg", "init"]).unwrap_or_else(|e| panic!("{e}"));
        let message = match run(&cli) {
            Err(e) => e.to_string(),
            Ok(()) => panic!("init is not implemented yet"),
        };
        assert!(message.contains("init"), "{message}");
        assert!(message.contains("T11"), "{message}");
    }
}
