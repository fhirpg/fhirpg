//! Reading FHIR resources out of files.
//!
//! Ports the bundle layer of `load.go`. This layer knows about file formats and
//! nothing about the database; the loaders above it know about the database and
//! nothing about file formats (see `AGENTS/architecture.md`).
//!
//! [`detect`] classifies a file, [`scanner`] walks JSON without materializing
//! it, and [`reader`] turns a file or a list of files into an iterator of
//! resources.

pub mod detect;
pub mod reader;
pub mod scanner;
