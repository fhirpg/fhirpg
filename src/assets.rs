//! Embedded FHIR schema and transform assets, and the version registry.
//!
//! Ports the `packr` boxes fhirbase uses in `dbinit.go:42-49` and
//! `transform.go:139-157`, and the memoization at `transform.go:132-158`.
//!
//! The assets under `assets/` are vendored **byte-identical** from fhirbase
//! (spec §3); only the filenames changed. `assets/CHECKSUMS.txt` records their
//! SHA-256 sums and a test verifies them, so an accidental edit cannot pass
//! unnoticed.
//!
//! # Why the transform map is parsed into types
//!
//! fhirbase carries the map as untyped `map[string]interface{}` and
//! re-interprets `tr/…` keys at every node during transformation. That is where
//! two of its defects live: an unchecked type assertion that panics on a
//! malformed path (X4), and an unrecognized `tr/act` that silently nulls a
//! field (X5).
//!
//! Here the map is parsed once into [`TransformMap`] and validated: every
//! `tr/act` must be recognized and every `tr/move` target must resolve. Both
//! hold for all nine vendored assets — 11,384 directives and 2,644 move targets
//! — so after [`FhirVersion::transform_map`] succeeds, transformation itself
//! cannot fail on a malformed map.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use crate::error::{Error, Result};

/// A FHIR release whose schema and transform assets are embedded in the binary.
///
/// FHIR 5.0.0 (R5) is the default at the command line (decision D4) and is
/// added by task T23, once its generated assets exist (T21, T22).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum FhirVersion {
    /// FHIR 1.0.2 (DSTU2).
    V1_0_2,
    /// FHIR 1.1.0.
    V1_1_0,
    /// FHIR 1.4.0.
    V1_4_0,
    /// FHIR 1.6.0.
    V1_6_0,
    /// FHIR 1.8.0.
    V1_8_0,
    /// FHIR 3.0.1 (STU3).
    V3_0_1,
    /// FHIR 3.2.0.
    V3_2_0,
    /// FHIR 3.3.0.
    V3_3_0,
    /// FHIR 4.0.0 (R4).
    V4_0_0,
}

/// Every supported version, in release order.
///
/// Mirrors `AvailableSchemas` (`main.go:12-16`).
pub const ALL_VERSIONS: &[FhirVersion] = &[
    FhirVersion::V1_0_2,
    FhirVersion::V1_1_0,
    FhirVersion::V1_4_0,
    FhirVersion::V1_6_0,
    FhirVersion::V1_8_0,
    FhirVersion::V3_0_1,
    FhirVersion::V3_2_0,
    FhirVersion::V3_3_0,
    FhirVersion::V4_0_0,
];

/// The embedded schema DDL, one entry per version, in `ALL_VERSIONS` order.
static SCHEMA_JSON: &[&str] = &[
    include_str!("../assets/schema/fhirpg-1.0.2.sql.json"),
    include_str!("../assets/schema/fhirpg-1.1.0.sql.json"),
    include_str!("../assets/schema/fhirpg-1.4.0.sql.json"),
    include_str!("../assets/schema/fhirpg-1.6.0.sql.json"),
    include_str!("../assets/schema/fhirpg-1.8.0.sql.json"),
    include_str!("../assets/schema/fhirpg-3.0.1.sql.json"),
    include_str!("../assets/schema/fhirpg-3.2.0.sql.json"),
    include_str!("../assets/schema/fhirpg-3.3.0.sql.json"),
    include_str!("../assets/schema/fhirpg-4.0.0.sql.json"),
];

/// The embedded transform maps, one entry per version, in `ALL_VERSIONS` order.
static TRANSFORM_JSON: &[&str] = &[
    include_str!("../assets/transform/fhirpg-import-1.0.2.json"),
    include_str!("../assets/transform/fhirpg-import-1.1.0.json"),
    include_str!("../assets/transform/fhirpg-import-1.4.0.json"),
    include_str!("../assets/transform/fhirpg-import-1.6.0.json"),
    include_str!("../assets/transform/fhirpg-import-1.8.0.json"),
    include_str!("../assets/transform/fhirpg-import-3.0.1.json"),
    include_str!("../assets/transform/fhirpg-import-3.2.0.json"),
    include_str!("../assets/transform/fhirpg-import-3.3.0.json"),
    include_str!("../assets/transform/fhirpg-import-4.0.0.json"),
];

/// The stored procedures, shared by every version (`dbinit.go:49`).
static FUNCTIONS_JSON: &str = include_str!("../assets/schema/functions.sql.json");

/// One parsed transform map per version, filled on first use.
///
/// fhirbase memoizes the same way (`transform.go:132-137`); a load run touches
/// exactly one version, so this is at most one parse per process.
static TRANSFORM_CACHE: [OnceLock<TransformMap>; 9] = [const { OnceLock::new() }; 9];

impl FhirVersion {
    /// Returns the dotted version string, e.g. `"4.0.0"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1_0_2 => "1.0.2",
            Self::V1_1_0 => "1.1.0",
            Self::V1_4_0 => "1.4.0",
            Self::V1_6_0 => "1.6.0",
            Self::V1_8_0 => "1.8.0",
            Self::V3_0_1 => "3.0.1",
            Self::V3_2_0 => "3.2.0",
            Self::V3_3_0 => "3.3.0",
            Self::V4_0_0 => "4.0.0",
        }
    }

    /// Index into the parallel asset arrays.
    fn index(self) -> usize {
        match self {
            Self::V1_0_2 => 0,
            Self::V1_1_0 => 1,
            Self::V1_4_0 => 2,
            Self::V1_6_0 => 3,
            Self::V1_8_0 => 4,
            Self::V3_0_1 => 5,
            Self::V3_2_0 => 6,
            Self::V3_3_0 => 7,
            Self::V4_0_0 => 8,
        }
    }

    /// A comma-separated list of every known version, for error messages.
    pub fn known() -> String {
        ALL_VERSIONS
            .iter()
            .map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Returns the schema DDL statements for this version, in execution order.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Asset`] if the embedded JSON is not an array of strings.
    pub fn schema_statements(self) -> Result<Vec<String>> {
        let raw = SCHEMA_JSON
            .get(self.index())
            .ok_or_else(|| Error::Asset(format!("no schema asset for FHIR {self}")))?;
        parse_statements(raw, "schema", self.as_str())
    }

    /// Returns the stored-procedure statements, shared by every version.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Asset`] if the embedded JSON is not an array of strings.
    pub fn function_statements() -> Result<Vec<String>> {
        parse_statements(FUNCTIONS_JSON, "functions", "all")
    }

    /// Returns this version's transform map, parsing and validating it once.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Asset`] if the map is malformed: a `tr/act` that is not
    /// `union` or `reference`, a `union` without a well-formed `tr/arg`, or a
    /// `tr/move` whose target does not resolve.
    pub fn transform_map(self) -> Result<&'static TransformMap> {
        let cell = TRANSFORM_CACHE
            .get(self.index())
            .ok_or_else(|| Error::Asset(format!("no transform asset for FHIR {self}")))?;

        if let Some(map) = cell.get() {
            return Ok(map);
        }

        let raw = TRANSFORM_JSON
            .get(self.index())
            .ok_or_else(|| Error::Asset(format!("no transform asset for FHIR {self}")))?;
        let map = TransformMap::parse(raw, self)?;

        // A concurrent caller may have won the race; either value is equally
        // valid, so keep whichever landed first.
        Ok(cell.get_or_init(|| map))
    }
}

impl std::fmt::Display for FhirVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for FhirVersion {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        ALL_VERSIONS
            .iter()
            .copied()
            .find(|v| v.as_str() == s)
            .ok_or_else(|| Error::UnknownFhirVersion {
                requested: s.to_owned(),
                known: Self::known(),
            })
    }
}

/// Parses an asset that is a JSON array of SQL statement strings.
fn parse_statements(raw: &str, kind: &str, version: &str) -> Result<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| Error::Asset(format!("{kind} asset for FHIR {version} is not JSON: {e}")))?;

    let array = value.as_array().ok_or_else(|| {
        Error::Asset(format!(
            "{kind} asset for FHIR {version} is not a JSON array"
        ))
    })?;

    array
        .iter()
        .enumerate()
        .map(|(i, v)| {
            v.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                Error::Asset(format!(
                    "{kind} asset for FHIR {version}: statement {i} is not a string"
                ))
            })
        })
        .collect()
}

/// What a transform node does to the value at its position (spec §4.1).
///
/// Exhaustive by construction: parsing rejects any other `tr/act`, which is the
/// fix for defect X5, where fhirbase silently replaces the field with `null`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrAct {
    /// `tr/act: "union"` — collapse a type-suffixed choice key into
    /// `{ type: value }` under `key` (spec §4.4).
    Union {
        /// The output key, e.g. `deceased` for `deceasedBoolean`.
        key: String,
        /// The FHIR type name, e.g. `boolean`.
        ty: String,
    },
    /// `tr/act: "reference"` — split a relative reference into `resourceType`
    /// and `id` (spec §4.5).
    Reference,
}

/// A node of the transformation map.
///
/// The three shapes are mutually exclusive in every vendored asset, which was
/// verified before this type was written: across all nine files, no node
/// carries both `tr/move` and `tr/act`, and neither an action node nor a move
/// node ever has field children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrNode {
    /// A leaf carrying an action.
    Act {
        /// The action to apply.
        act: TrAct,
        /// `tr/isCollection`, parsed but never read — see spec §4.7.
        is_collection: bool,
    },
    /// A redirect: continue with the node at this path from the map root.
    Move {
        /// The path to follow, e.g. `["Identifier"]`.
        path: Vec<String>,
        /// `tr/isCollection`, parsed but never read — see spec §4.7.
        is_collection: bool,
    },
    /// An interior node whose children are field names.
    Branch {
        /// Child nodes by FHIR field name.
        children: BTreeMap<String, TrNode>,
        /// `tr/isCollection`, parsed but never read — see spec §4.7.
        is_collection: bool,
    },
}

impl TrNode {
    /// The key this node's value is written under in the output.
    ///
    /// A `union` node renames its field — `deceasedBoolean` becomes `deceased`
    /// — which fhirbase reads from the child's `tr/arg.key`
    /// (`transform.go:93-102`). Every other node keeps the original field name.
    pub fn output_key<'a>(&'a self, field: &'a str) -> &'a str {
        match self {
            Self::Act {
                act: TrAct::Union { key, .. },
                ..
            } => key,
            _ => field,
        }
    }

    /// The child node for a field, if this node has one.
    pub fn child(&self, field: &str) -> Option<&Self> {
        match self {
            Self::Branch { children, .. } => children.get(field),
            _ => None,
        }
    }
}

/// A parsed, validated transformation map for one FHIR version (spec §4.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransformMap {
    /// The FHIR version this map belongs to.
    version: FhirVersion,
    /// Top-level entries, keyed by FHIR type name.
    types: BTreeMap<String, TrNode>,
}

impl TransformMap {
    /// Parses and validates a transform map.
    fn parse(raw: &str, version: FhirVersion) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| {
            Error::Asset(format!(
                "transform asset for FHIR {version} is not JSON: {e}"
            ))
        })?;

        let object = value.as_object().ok_or_else(|| {
            Error::Asset(format!(
                "transform asset for FHIR {version} is not a JSON object"
            ))
        })?;

        let mut types = BTreeMap::new();
        for (name, node) in object {
            types.insert(name.clone(), parse_node(node, version, name)?);
        }

        let map = Self { version, types };
        map.validate_moves()?;
        Ok(map)
    }

    /// The FHIR version this map belongs to.
    pub fn version(&self) -> FhirVersion {
        self.version
    }

    /// The top-level node for a FHIR type, if the map has one.
    ///
    /// A resource type absent from the map passes through the transformation
    /// untouched — see spec §4.2 step 2.
    pub fn type_node(&self, type_name: &str) -> Option<&TrNode> {
        self.types.get(type_name)
    }

    /// Resolves a `tr/move` path from the map root.
    ///
    /// Ports `getByPath` (`transform.go:16-30`), which performs an unchecked
    /// type assertion and panics when a segment is missing (defect X4). This
    /// returns `None` instead — and because [`Self::validate_moves`] runs at
    /// parse time, `None` is unreachable for a validated map.
    pub fn resolve(&self, path: &[String]) -> Option<&TrNode> {
        let (first, rest) = path.split_first()?;
        let mut node = self.types.get(first)?;
        for segment in rest {
            node = node.child(segment)?;
        }
        Some(node)
    }

    /// Checks that every `tr/move` in the map resolves.
    ///
    /// Holds for all nine vendored assets: 2,644 move targets, none dangling.
    fn validate_moves(&self) -> Result<()> {
        let mut dangling = Vec::new();
        for (name, node) in &self.types {
            collect_dangling_moves(self, node, &mut vec![name.clone()], &mut dangling);
        }

        if let Some(first) = dangling.first() {
            return Err(Error::Asset(format!(
                "transform asset for FHIR {}: {} unresolvable tr/move target(s); first at {}",
                self.version,
                dangling.len(),
                first
            )));
        }
        Ok(())
    }

    /// The number of top-level type entries. Used by tests.
    pub fn type_count(&self) -> usize {
        self.types.len()
    }
}

/// Walks a node, recording the location of every `tr/move` that does not resolve.
fn collect_dangling_moves(
    map: &TransformMap,
    node: &TrNode,
    path: &mut Vec<String>,
    out: &mut Vec<String>,
) {
    match node {
        TrNode::Move { path: target, .. } => {
            if map.resolve(target).is_none() {
                out.push(format!("{} -> {:?}", path.join("."), target));
            }
        }
        TrNode::Branch { children, .. } => {
            for (field, child) in children {
                path.push(field.clone());
                collect_dangling_moves(map, child, path, out);
                path.pop();
            }
        }
        TrNode::Act { .. } => {}
    }
}

/// Parses one node of the transformation map.
fn parse_node(value: &serde_json::Value, version: FhirVersion, at: &str) -> Result<TrNode> {
    let object = value.as_object().ok_or_else(|| {
        Error::Asset(format!(
            "transform asset for FHIR {version}: node at {at} is not a JSON object"
        ))
    })?;

    let is_collection = object
        .get("tr/isCollection")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    if let Some(act) = object.get("tr/act") {
        let act = act.as_str().ok_or_else(|| {
            Error::Asset(format!(
                "transform asset for FHIR {version}: tr/act at {at} is not a string"
            ))
        })?;

        let act = match act {
            "reference" => TrAct::Reference,
            "union" => {
                let arg = object.get("tr/arg").and_then(serde_json::Value::as_object);
                let arg = arg.ok_or_else(|| {
                    Error::Asset(format!(
                        "transform asset for FHIR {version}: union at {at} has no tr/arg object"
                    ))
                })?;
                let key = string_field(arg, "key", version, at)?;
                let ty = string_field(arg, "type", version, at)?;
                TrAct::Union { key, ty }
            }
            other => {
                // Defect X5: fhirbase falls through here with an unset result,
                // silently replacing the field with null.
                return Err(Error::Asset(format!(
                    "transform asset for FHIR {version}: unrecognized tr/act {other:?} at {at}; \
                     expected \"union\" or \"reference\""
                )));
            }
        };

        return Ok(TrNode::Act { act, is_collection });
    }

    if let Some(target) = object.get("tr/move") {
        let array = target.as_array().ok_or_else(|| {
            Error::Asset(format!(
                "transform asset for FHIR {version}: tr/move at {at} is not an array"
            ))
        })?;
        let path = array
            .iter()
            .map(|segment| {
                segment.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                    Error::Asset(format!(
                        "transform asset for FHIR {version}: tr/move at {at} has a non-string segment"
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;

        if path.is_empty() {
            return Err(Error::Asset(format!(
                "transform asset for FHIR {version}: tr/move at {at} is empty"
            )));
        }

        return Ok(TrNode::Move {
            path,
            is_collection,
        });
    }

    let mut children = BTreeMap::new();
    for (field, child) in object {
        if field.starts_with("tr/") {
            continue;
        }
        let child_at = format!("{at}.{field}");
        children.insert(field.clone(), parse_node(child, version, &child_at)?);
    }

    Ok(TrNode::Branch {
        children,
        is_collection,
    })
}

/// Reads a required string field out of a `tr/arg` object.
fn string_field(
    arg: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    version: FhirVersion,
    at: &str,
) -> Result<String> {
    arg.get(field)
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            Error::Asset(format!(
                "transform asset for FHIR {version}: union at {at} has no string tr/arg.{field}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn every_version_round_trips_through_its_string() {
        for &version in ALL_VERSIONS {
            let parsed = FhirVersion::from_str(version.as_str()).unwrap();
            assert_eq!(parsed, version);
        }
    }

    #[test]
    fn version_list_matches_fhirbase() {
        // `AvailableSchemas`, main.go:12-16.
        assert_eq!(
            FhirVersion::known(),
            "1.0.2, 1.1.0, 1.4.0, 1.6.0, 1.8.0, 3.0.1, 3.2.0, 3.3.0, 4.0.0"
        );
    }

    #[test]
    fn unknown_version_error_lists_the_known_ones() {
        let err = FhirVersion::from_str("9.9.9").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("9.9.9"), "{message}");
        assert!(message.contains("4.0.0"), "{message}");
    }

    #[test]
    fn every_schema_asset_parses() {
        for &version in ALL_VERSIONS {
            let statements = version.schema_statements().unwrap();
            assert!(
                !statements.is_empty(),
                "FHIR {version} schema is empty"
            );
            assert!(
                statements[0].contains("CREATE EXTENSION"),
                "FHIR {version} should open with the pgcrypto extension"
            );
        }
    }

    #[test]
    fn schema_4_0_0_has_the_expected_statement_count() {
        // Counted from the upstream asset before vendoring: 293 statements,
        // covering 145 resource tables plus their history tables and preamble.
        let statements = FhirVersion::V4_0_0.schema_statements().unwrap();
        assert_eq!(statements.len(), 293);
    }

    #[test]
    fn function_asset_has_ten_statements() {
        let statements = FhirVersion::function_statements().unwrap();
        assert_eq!(statements.len(), 10);
    }

    #[test]
    fn the_procedures_are_rebranded_and_carry_no_upstream_identifiers() {
        // Decision D3. functions.sql.json is the one vendored asset we edit,
        // and the edit must be total: a surviving `fhirbase_` identifier would
        // mean a procedure that init creates under the old name, or worse, a
        // call site pointing at a procedure that does not exist.
        let statements = FhirVersion::function_statements().unwrap();
        let joined = statements.join("\n");

        assert!(
            !joined.contains("fhirbase"),
            "a fhirbase identifier survived the D3 rebrand"
        );

        // Both arities of create, update and delete, plus read, genid, and the
        // internal row-to-resource helper.
        for expected in [
            "FUNCTION fhirpg_genid()",
            "FUNCTION _fhirpg_to_resource(",
            "FUNCTION fhirpg_create(resource jsonb, txid bigint)",
            "FUNCTION fhirpg_create(resource jsonb)",
            "FUNCTION fhirpg_update(resource jsonb, txid bigint)",
            "FUNCTION fhirpg_update(resource jsonb)",
            "FUNCTION fhirpg_read(resource_type text, id text)",
            "FUNCTION fhirpg_delete(resource_type text, id text, txid bigint)",
            "FUNCTION fhirpg_delete(resource_type text, id text)",
        ] {
            assert!(joined.contains(expected), "missing: {expected}");
        }

        // The `_resource` composite type keeps its unbranded name (spec §2.4).
        assert!(joined.contains("CREATE TYPE _resource AS ("));

        // The single-argument forms depend on the sequence that the schema's
        // `transaction` table creates implicitly (spec §2.2).
        assert!(joined.contains("nextval('transaction_id_seq')"));
    }

    #[test]
    fn no_vendored_schema_asset_mentions_fhirbase() {
        // The nine per-version schema files carry no branded identifiers at
        // all, which is why the D3 rebrand touches exactly one file.
        for &version in ALL_VERSIONS {
            let joined = version.schema_statements().unwrap().join("\n");
            assert!(
                !joined.contains("fhirbase"),
                "FHIR {version} schema unexpectedly mentions fhirbase"
            );
        }
    }

    #[test]
    fn every_transform_map_parses_and_validates() {
        // Top-level type counts, read from the vendored assets. Pinning the
        // exact numbers means a silently truncated or swapped asset fails here
        // rather than somewhere downstream.
        let expected: [(FhirVersion, usize); 9] = [
            (FhirVersion::V1_0_2, 94),
            (FhirVersion::V1_1_0, 104),
            (FhirVersion::V1_4_0, 117),
            (FhirVersion::V1_6_0, 113),
            (FhirVersion::V1_8_0, 124),
            (FhirVersion::V3_0_1, 126),
            (FhirVersion::V3_2_0, 152),
            (FhirVersion::V3_3_0, 147),
            (FhirVersion::V4_0_0, 155),
        ];

        for (version, count) in expected {
            let map = version.transform_map().unwrap();
            assert_eq!(map.version(), version);
            assert_eq!(map.type_count(), count, "FHIR {version} type count");
        }
    }

    #[test]
    fn transform_map_is_cached() {
        let first = FhirVersion::V3_0_1.transform_map().unwrap();
        let second = FhirVersion::V3_0_1.transform_map().unwrap();
        assert!(
            std::ptr::eq(first, second),
            "transform.go:132-137 memoizes; so must we"
        );
    }

    #[test]
    fn transform_map_4_0_0_matches_the_upstream_shape() {
        let map = FhirVersion::V4_0_0.transform_map().unwrap();
        assert_eq!(map.type_count(), 155);
        assert!(map.type_node("Patient").is_some());
        assert!(map.type_node("Reference").is_some());
        assert!(map.type_node("NoSuchResource").is_none());
    }

    #[test]
    fn patient_choice_elements_parse_as_unions() {
        let map = FhirVersion::V4_0_0.transform_map().unwrap();
        let patient = map.type_node("Patient").unwrap();
        let node = patient.child("deceasedBoolean").unwrap();
        assert_eq!(
            node,
            &TrNode::Act {
                act: TrAct::Union {
                    key: "deceased".to_owned(),
                    ty: "boolean".to_owned(),
                },
                is_collection: false,
            }
        );
        assert_eq!(node.output_key("deceasedBoolean"), "deceased");
    }

    #[test]
    fn patient_reference_elements_parse_as_references() {
        let map = FhirVersion::V4_0_0.transform_map().unwrap();
        let patient = map.type_node("Patient").unwrap();
        let node = patient.child("managingOrganization").unwrap();
        assert!(matches!(
            node,
            TrNode::Act {
                act: TrAct::Reference,
                ..
            }
        ));
        // A non-union node keeps its field name.
        assert_eq!(node.output_key("managingOrganization"), "managingOrganization");
    }

    #[test]
    fn move_nodes_resolve_to_a_top_level_type() {
        let map = FhirVersion::V4_0_0.transform_map().unwrap();
        let patient = map.type_node("Patient").unwrap();
        let TrNode::Move { path, .. } = patient.child("identifier").unwrap() else {
            panic!("Patient.identifier is a tr/move node upstream");
        };
        assert_eq!(path, &["Identifier".to_owned()]);
        assert!(map.resolve(path).is_some());
    }

    #[test]
    fn resolve_rejects_a_dangling_path() {
        let map = FhirVersion::V4_0_0.transform_map().unwrap();
        // Defect X4: fhirbase panics here instead of returning None.
        assert!(map.resolve(&["NoSuchType".to_owned()]).is_none());
        assert!(map.resolve(&[]).is_none());
    }

    #[test]
    fn is_collection_is_parsed_even_though_it_is_never_read() {
        // Spec §4.7: retained for asset fidelity, deliberately unused.
        let map = FhirVersion::V4_0_0.transform_map().unwrap();
        let patient = map.type_node("Patient").unwrap();
        let TrNode::Move { is_collection, .. } = patient.child("identifier").unwrap() else {
            panic!("Patient.identifier is a tr/move node upstream");
        };
        assert!(is_collection);
    }

    #[test]
    fn unrecognized_tr_act_is_rejected() {
        // Defect X5: fhirbase silently nulls the field instead.
        let raw = r#"{"Patient": {"foo": {"tr/act": "explode"}}}"#;
        let err = TransformMap::parse(raw, FhirVersion::V4_0_0).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("explode"), "{message}");
        assert!(message.contains("Patient.foo"), "{message}");
    }

    #[test]
    fn union_without_tr_arg_is_rejected() {
        let raw = r#"{"Patient": {"foo": {"tr/act": "union"}}}"#;
        let err = TransformMap::parse(raw, FhirVersion::V4_0_0).unwrap_err();
        assert!(err.to_string().contains("tr/arg"), "{err}");
    }

    #[test]
    fn dangling_move_target_is_rejected_at_parse_time() {
        let raw = r#"{"Patient": {"foo": {"tr/move": ["NoSuchType"]}}}"#;
        let err = TransformMap::parse(raw, FhirVersion::V4_0_0).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("tr/move"), "{message}");
        assert!(message.contains("NoSuchType"), "{message}");
    }

    #[test]
    fn multi_segment_move_paths_resolve() {
        // 169 of the 2,644 move targets upstream have more than one segment.
        let raw = r#"{
            "A": {"inner": {"tr/act": "reference"}},
            "B": {"field": {"tr/move": ["A", "inner"]}}
        }"#;
        let map = TransformMap::parse(raw, FhirVersion::V4_0_0).unwrap();
        let resolved = map.resolve(&["A".to_owned(), "inner".to_owned()]).unwrap();
        assert!(matches!(
            resolved,
            TrNode::Act {
                act: TrAct::Reference,
                ..
            }
        ));
    }

    #[test]
    fn vendored_assets_match_their_recorded_checksums() {
        use sha2::{Digest, Sha256};

        let manifest = include_str!("../assets/CHECKSUMS.txt");
        let mut checked = 0;

        for line in manifest.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((expected, path)) = line.split_once("  ") else {
                panic!("malformed CHECKSUMS.txt line: {line}");
            };

            let bytes = std::fs::read(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("assets")
                    .join(path),
            )
            .unwrap_or_else(|e| panic!("cannot read assets/{path}: {e}"));

            let actual = format!("{:x}", Sha256::digest(&bytes));
            assert_eq!(
                actual, expected,
                "assets/{path} does not match its recorded checksum; \
                 vendored assets are byte-identical by contract (spec §3)"
            );
            checked += 1;
        }

        assert_eq!(checked, 19, "expected 10 schema assets and 9 transform maps");
    }
}
