//! The subcommand implementations.
//!
//! One module per subcommand, mirroring fhirbase's `*Command` functions. Each
//! is thin orchestration: it owns progress reporting and user-facing output,
//! and delegates the actual work to the layers below (see
//! `AGENTS/architecture.md`).
//!
//! Modules appear as their tasks land: `transform` (T7), then `init` (T11),
//! `load` (T16), `bulkget` (T18), and `web` (T20).

pub mod transform;
