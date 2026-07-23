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

// Schema statements and the version list are read by `init` (T11) and the
// loaders (T14, T15); only the transform map is consumed so far. The module's
// own tests cover the rest, so the expectation is non-test only.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "schema accessors are consumed by tasks T11-T15")
)]
mod assets;
mod bundle;
mod cli;
mod commands;
mod config;
mod db;
mod error;
mod load;
mod memory;
#[cfg(test)]
mod procedures_suite;
#[cfg(test)]
mod testdb;
mod transform;

use std::str::FromStr;

use clap::{CommandFactory, Parser};

use crate::assets::FhirVersion;
use crate::config::PgConfig;
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

    // Parsed once, here, so an unknown `--fhir` fails before any command does
    // work — and fails the same way for every command (spec §3).
    let version = FhirVersion::from_str(&cli.fhir)?;

    match command {
        Command::Init => {
            let config = PgConfig::from_args(&cli.connection);
            runtime()?.block_on(commands::init::run(&config, version))
        }
        Command::Transform { file } => commands::transform::run(file, version),
        Command::Load {
            sources,
            mode,
            strict,
            count_first,
            memusage,
            txid,
            ..
        } => {
            let config = PgConfig::from_args(&cli.connection);
            let request = commands::load::LoadRequest {
                sources: sources.clone(),
                mode: *mode,
                strict: *strict,
                count_first: *count_first,
                memusage: *memusage,
                new_txid: txid.is_some(),
            };
            runtime()?.block_on(commands::load::run(&config, version, &request))
        }
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

/// Builds the async runtime on demand.
///
/// Built here rather than with `#[tokio::main]` so that commands needing no
/// database — `transform` — start no runtime at all.
fn runtime() -> error::Result<tokio::runtime::Runtime> {
    tokio::runtime::Runtime::new()
        .map_err(|e| Error::Config(format!("cannot start the async runtime: {e}")))
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
        // Repointed from `init` when T11 landed. When the last of these is
        // implemented, delete this test along with `Error::NotImplemented`.
        let cli =
            Cli::try_parse_from(["fhirpg", "bulkget", "http://x", "/tmp"]).unwrap_or_else(|e| panic!("{e}"));
        let message = match run(&cli) {
            Err(e) => e.to_string(),
            Ok(()) => panic!("bulkget is not implemented yet"),
        };
        assert!(message.contains("bulkget"), "{message}");
        assert!(message.contains("T18"), "{message}");
    }
}
