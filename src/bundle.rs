//! Reading FHIR resources out of files.
//!
//! Ports the bundle layer of `load.go`. This layer knows about file formats and
//! nothing about the database; the loaders above it know about the database and
//! nothing about file formats (see `AGENTS/architecture.md`).
//!
//! Modules appear as their tasks land: `detect` (T12), then the readers
//! themselves — `ndjson`, `fhir_bundle`, `single`, `multifile` (T13).

pub mod detect;
