//! Generates fhirpg's FHIR schema and transform assets from the official
//! specification.
//!
//! Task T21. Not published, not part of the binary: this runs once per FHIR
//! release, and its output is committed (decision D5).
//!
//! ```text
//! cargo run -p xtask -- generate <spec-dir> <version> <out-dir>
//! cargo run -p xtask -- validate <spec-dir> <version> <reference-transform.json>
//! ```
//!
//! `<spec-dir>` holds the official `profiles-types.json` and
//! `profiles-resources.json`. It reads those directly rather than going through
//! the sibling `fhir` crate's API, which keeps `fhir` a dependency of nothing
//! (risk R5) and means any HL7 download works.
//!
//! # Why this is trustworthy
//!
//! The rules below were derived by analyzing fhirbase's own 4.0.0 asset and
//! then confirmed by regenerating fhirbase's 3.0.1 and 4.0.0 maps and diffing
//! node by node: R3 reproduces at 126/126, R4 at 151/155, and the four
//! differences are the `Meta` open-type addition between spec 4.0.0 and 4.0.1.
//! `validate` is that comparison, kept runnable. See
//! `doc/r5-generation/validation.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{Map, Value, json};

/// The `Element`, `Resource`, and `DomainResource` base fields.
///
/// Excluded outright, even though `Extension` and `Meta` carry rules of their
/// own. Not visible anywhere in fhirbase's Go — found by diffing its asset,
/// where it accounted for 147 of 155 initial mismatches.
const INFRASTRUCTURE: &[&str] = &[
    "id",
    "extension",
    "modifierExtension",
    "meta",
    "implicitRules",
    "language",
    "text",
    "contained",
];

/// FHIR primitive type codes, which never carry transformation rules.
const PRIMITIVES: &[&str] = &[
    "base64Binary",
    "boolean",
    "canonical",
    "code",
    "date",
    "dateTime",
    "decimal",
    "id",
    "instant",
    "integer",
    "integer64",
    "markdown",
    "oid",
    "positiveInt",
    "string",
    "time",
    "unsignedInt",
    "uri",
    "url",
    "uuid",
    "xhtml",
    "http://hl7.org/fhirpath/System.String",
];

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("generate") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(spec), Some(version), Some(out)) => {
                generate(Path::new(spec), version, Path::new(out))
            }
            _ => Err("usage: xtask generate <spec-dir> <version> <out-dir>".to_owned()),
        },
        Some("validate") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(spec), Some(version), Some(reference)) => {
                validate(Path::new(spec), version, Path::new(reference))
            }
            _ => Err("usage: xtask validate <spec-dir> <version> <reference.json>".to_owned()),
        },
        _ => Err(
            "usage:\n  \
             xtask generate <spec-dir> <version> <out-dir>\n  \
             xtask validate <spec-dir> <version> <reference-transform.json>"
                .to_owned(),
        ),
    };

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Writes both assets for a FHIR version.
fn generate(spec: &Path, version: &str, out: &Path) -> Result<(), String> {
    let structures = Structures::load(spec)?;

    let transform = structures.transform_map();
    let schema = structures.schema_statements();

    std::fs::create_dir_all(out.join("transform"))
        .map_err(|e| format!("cannot create the output directory: {e}"))?;
    std::fs::create_dir_all(out.join("schema"))
        .map_err(|e| format!("cannot create the output directory: {e}"))?;

    let transform_path = out.join(format!("transform/fhirpg-import-{version}.json"));
    let schema_path = out.join(format!("schema/fhirpg-{version}.sql.json"));

    write_json(&transform_path, &Value::Object(transform.clone()))?;
    write_json(&schema_path, &json!(schema))?;

    println!("wrote {}", transform_path.display());
    println!("  {} top-level entries, {}", transform.len(), count_directives(&transform));
    println!("wrote {}", schema_path.display());
    println!("  {} statements, {} resource tables", schema.len(), structures.storable().len());

    Ok(())
}

/// Regenerates a version and diffs it against a reference asset.
///
/// This is the oracle: fhirbase generated its maps from the same
/// `StructureDefinition`s, so reproducing them is what makes generating a release
/// fhirbase never saw trustworthy.
fn validate(spec: &Path, _version: &str, reference: &Path) -> Result<(), String> {
    let structures = Structures::load(spec)?;
    let generated = structures.transform_map();

    let raw = std::fs::read_to_string(reference)
        .map_err(|e| format!("cannot read {}: {e}", reference.display()))?;
    let reference: Map<String, Value> = serde_json::from_str(&raw)
        .map_err(|e| format!("cannot parse {}: {e}", "the reference asset"))?;

    let generated_keys: BTreeSet<&String> = generated.keys().collect();
    let reference_keys: BTreeSet<&String> = reference.keys().collect();

    let shared: Vec<String> = generated_keys
        .intersection(&reference_keys)
        .map(|k| (*k).clone())
        .collect();
    let identical = shared
        .iter()
        .filter(|k| generated.get(k.as_str()) == reference.get(k.as_str()))
        .count();

    let only_generated: Vec<String> = generated_keys
        .difference(&reference_keys)
        .map(|k| (*k).clone())
        .collect();
    let only_reference: Vec<String> = reference_keys
        .difference(&generated_keys)
        .map(|k| (*k).clone())
        .collect();

    println!("generated {} entries, reference {}", generated.len(), reference.len());
    println!("  shared {}, identical {}, differing {}", shared.len(), identical, shared.len() - identical);
    println!("  only in generated: {only_generated:?}");
    println!("  only in reference: {only_reference:?}");

    for key in &shared {
        if generated.get(key.as_str()) != reference.get(key.as_str()) {
            println!("  differs: {key}");
        }
    }

    if !only_reference.is_empty() {
        return Err(format!(
            "{} entries the reference has and the generator does not; \
             every reference entry must be reproduced",
            only_reference.len()
        ));
    }

    Ok(())
}

/// Writes JSON with the two-space indentation the vendored assets use.
fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|e| format!("cannot serialize: {e}"))?;
    text.push('\n');
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Counts the directives in a map, for the summary line.
fn count_directives(map: &Map<String, Value>) -> String {
    fn walk(node: &Value, unions: &mut usize, references: &mut usize, moves: &mut usize) {
        let Some(object) = node.as_object() else { return };
        match object.get("tr/act").and_then(Value::as_str) {
            Some("union") => *unions += 1,
            Some("reference") => *references += 1,
            _ => {}
        }
        if object.contains_key("tr/move") {
            *moves += 1;
        }
        for (key, value) in object {
            if !key.starts_with("tr/") {
                walk(value, unions, references, moves);
            }
        }
    }

    let (mut unions, mut references, mut moves) = (0_usize, 0_usize, 0_usize);
    for node in map.values() {
        walk(node, &mut unions, &mut references, &mut moves);
    }
    format!("{unions} union, {references} reference, {moves} move")
}

/// One FHIR `StructureDefinition`'s snapshot, keyed by type name.
struct Structures {
    /// Every type's snapshot elements, by type name.
    elements: BTreeMap<String, Vec<Element>>,
    /// Concrete resource names, which are the ones that get tables.
    resources: BTreeSet<String>,
}

/// The parts of an `ElementDefinition` this generator needs.
struct Element {
    path: String,
    max: String,
    type_codes: Vec<String>,
    content_reference: Option<String>,
}

impl Structures {
    /// Reads `profiles-types.json` and `profiles-resources.json`.
    fn load(spec: &Path) -> Result<Self, String> {
        let mut elements = BTreeMap::new();
        let mut resources = BTreeSet::new();

        for filename in ["profiles-types.json", "profiles-resources.json"] {
            let path = spec.join(filename);
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let bundle: Value = serde_json::from_str(&raw)
                .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;

            let entries = bundle
                .get("entry")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("{} has no entry array", path.display()))?;

            for entry in entries {
                let Some(sd) = entry.get("resource") else { continue };
                if sd.get("resourceType").and_then(Value::as_str) != Some("StructureDefinition") {
                    continue;
                }
                // Profiles constrain a base type; only the base definitions
                // describe structure.
                if sd.get("derivation").and_then(Value::as_str) == Some("constraint") {
                    continue;
                }
                let Some(name) = sd.get("name").and_then(Value::as_str) else { continue };
                let Some(snapshot) = sd
                    .get("snapshot")
                    .and_then(|s| s.get("element"))
                    .and_then(Value::as_array)
                else {
                    continue;
                };

                let is_resource = sd.get("kind").and_then(Value::as_str) == Some("resource");
                let is_abstract = sd.get("abstract").and_then(Value::as_bool) == Some(true);
                if is_resource && !is_abstract {
                    resources.insert(name.to_owned());
                }

                elements.insert(name.to_owned(), snapshot.iter().map(Element::from_json).collect());
            }
        }

        if elements.is_empty() {
            return Err(format!("no StructureDefinitions found in {}", spec.display()));
        }

        Ok(Self { elements, resources })
    }

    /// The resource types that get tables: every concrete resource.
    ///
    /// fhirbase's 4.0.0 set omits `Bundle`, `SearchParameter`, and `TestScript`
    /// and includes two types that do not exist in 4.0.1 at all — artifacts of
    /// whatever snapshot it generated from. Storing every concrete resource is
    /// the rule that can be stated, and it makes those three storable.
    fn storable(&self) -> &BTreeSet<String> {
        &self.resources
    }

    /// Builds the whole transformation map.
    fn transform_map(&self) -> Map<String, Value> {
        let mut nodes: BTreeMap<String, Value> = BTreeMap::new();
        let mut building = BTreeSet::new();

        for name in self.elements.keys() {
            let node = self.node_for_type(name, &mut nodes, &mut building);
            if !is_empty_object(&node) {
                nodes.insert(name.clone(), node);
            }
        }

        let mut root: Map<String, Value> = nodes.into_iter().collect();
        prune_dangling_moves(&mut root);
        root
    }

    /// The node for a named type, memoized, with cycles broken.
    fn node_for_type(
        &self,
        name: &str,
        cache: &mut BTreeMap<String, Value>,
        building: &mut BTreeSet<String>,
    ) -> Value {
        if let Some(node) = cache.get(name) {
            return node.clone();
        }
        if building.contains(name) {
            return Value::Object(Map::new());
        }
        let Some(elements) = self.elements.get(name) else {
            return Value::Object(Map::new());
        };

        building.insert(name.to_owned());
        let node = self.build_node(elements, name, cache, building);
        building.remove(name);
        cache.insert(name.to_owned(), node.clone());
        node
    }

    /// Builds the node for the element at `prefix`, pruning empties.
    fn build_node(
        &self,
        elements: &[Element],
        prefix: &str,
        cache: &mut BTreeMap<String, Value>,
        building: &mut BTreeSet<String>,
    ) -> Value {
        let mut node = Map::new();

        for child in direct_children(elements, prefix) {
            let Some(field) = child.path.rsplit('.').next() else { continue };
            if INFRASTRUCTURE.contains(&field) {
                continue;
            }
            let collection = child.max != "1" && child.max != "0";

            // FHIR's recursion marker. Whether it survives depends on the
            // target carrying rules, which `prune_dangling_moves` decides.
            if let Some(reference) = &child.content_reference {
                let target: Vec<Value> = reference
                    .trim_start_matches('#')
                    .split('.')
                    .map(|s| Value::String(s.to_owned()))
                    .collect();
                node.insert(field.to_owned(), move_node(target, collection));
                continue;
            }

            if let Some(base) = field.strip_suffix("[x]") {
                let mut collapsed = Map::new();
                for code in &child.type_codes {
                    node.insert(
                        format!("{base}{}", capitalize(code)),
                        json!({"tr/act": "union", "tr/arg": {"key": base, "type": code}}),
                    );
                    if code == "Reference" {
                        // The transformation special-cases a union of Reference
                        // (spec §4.4 step 2), so the collapsed child is the
                        // splitting rule, not the Reference type's own node.
                        collapsed.insert(code.clone(), json!({"tr/act": "reference"}));
                    } else if !is_empty_object(&self.node_for_type(code, cache, building)) {
                        collapsed.insert(code.clone(), json!({"tr/move": [code]}));
                    }
                }
                if !collapsed.is_empty() {
                    node.insert(base.to_owned(), Value::Object(collapsed));
                }
                continue;
            }

            if child.type_codes == ["Reference"] {
                let mut entry = Map::new();
                entry.insert("tr/act".to_owned(), Value::String("reference".to_owned()));
                if collection {
                    entry.insert("tr/isCollection".to_owned(), Value::Bool(true));
                }
                node.insert(field.to_owned(), Value::Object(entry));
                continue;
            }

            if child.type_codes == ["BackboneElement"] || child.type_codes == ["Element"] {
                let inner = self.build_node(elements, &child.path, cache, building);
                if let Value::Object(mut inner) = inner
                    && !inner.is_empty()
                {
                    if collection {
                        inner.insert("tr/isCollection".to_owned(), Value::Bool(true));
                    }
                    node.insert(field.to_owned(), Value::Object(inner));
                }
                continue;
            }

            if let [code] = child.type_codes.as_slice()
                && !PRIMITIVES.contains(&code.as_str())
                && !is_empty_object(&self.node_for_type(code, cache, building))
            {
                node.insert(
                    field.to_owned(),
                    move_node(vec![Value::String(code.clone())], collection),
                );
            }
        }

        Value::Object(node)
    }

    /// Builds the schema DDL statements, in execution order.
    fn schema_statements(&self) -> Vec<String> {
        let mut statements = Vec::new();

        // Decision D9: no `CREATE EXTENSION pgcrypto`. PostgreSQL 18 provides
        // `gen_random_uuid()` in core, and the statement fails outright on an
        // installation without `contrib`.
        statements.push(
            "DO $$\nBEGIN\n    \
             IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'resource_status') THEN\n       \
             CREATE TYPE resource_status AS ENUM ('created', 'updated', 'deleted', 'recreated');\n    \
             END IF;\nEND\n$$;"
                .to_owned(),
        );
        statements.push(
            "CREATE TABLE IF NOT EXISTS transaction (\n  \
             id serial primary key,\n  \
             ts timestamptz DEFAULT current_timestamp,\n  \
             resource jsonb);"
                .to_owned(),
        );

        for name in self.storable() {
            let table = name.to_lowercase();
            statements.push(resource_table(&table, name));
            statements.push(history_table(&table, name));
        }

        statements
    }
}

/// `CREATE TABLE` for a resource, matching the vendored assets' shape exactly.
fn resource_table(table: &str, resource_type: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS \"{table}\" (\n  \
         id text primary key,\n  \
         txid bigint not null,\n  \
         ts timestamptz DEFAULT current_timestamp,\n  \
         resource_type text default '{resource_type}',\n  \
         status resource_status not null,\n  \
         resource jsonb not null\n);"
    )
}

/// `CREATE TABLE` for a resource's history.
fn history_table(table: &str, resource_type: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS \"{table}_history\" (\n  \
         id text,\n  \
         txid bigint not null,\n  \
         ts timestamptz DEFAULT current_timestamp,\n  \
         resource_type text default '{resource_type}',\n  \
         status resource_status not null,\n  \
         resource jsonb not null,\n  \
         PRIMARY KEY (id, txid)\n);"
    )
}

impl Element {
    fn from_json(value: &Value) -> Self {
        let path = value
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let max = value
            .get("max")
            .and_then(Value::as_str)
            .unwrap_or("1")
            .to_owned();
        let type_codes = value
            .get("type")
            .and_then(Value::as_array)
            .map(|types| {
                let mut codes: Vec<String> = Vec::new();
                for entry in types {
                    if let Some(code) = entry.get("code").and_then(Value::as_str)
                        && !codes.iter().any(|c| c == code)
                    {
                        codes.push(code.to_owned());
                    }
                }
                codes
            })
            .unwrap_or_default();
        let content_reference = value
            .get("contentReference")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        Self {
            path,
            max,
            type_codes,
            content_reference,
        }
    }
}

/// The elements one level below `prefix`.
fn direct_children<'a>(elements: &'a [Element], prefix: &str) -> Vec<&'a Element> {
    let depth = prefix.matches('.').count() + 1;
    let needle = format!("{prefix}.");
    elements
        .iter()
        .filter(|e| e.path.starts_with(&needle) && e.path.matches('.').count() == depth)
        .collect()
}

/// A `tr/move` node, with `tr/isCollection` when repeating.
fn move_node(target: Vec<Value>, collection: bool) -> Value {
    let mut entry = Map::new();
    entry.insert("tr/move".to_owned(), Value::Array(target));
    if collection {
        entry.insert("tr/isCollection".to_owned(), Value::Bool(true));
    }
    Value::Object(entry)
}

/// Capitalizes a type code for a choice-element suffix: `boolean` → `Boolean`.
fn capitalize(code: &str) -> String {
    let mut chars = code.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Whether a value is an object with no entries.
fn is_empty_object(value: &Value) -> bool {
    value.as_object().is_some_and(Map::is_empty)
}

/// Whether a node carries an action, a move, or any rule-bearing descendant.
fn has_rules(node: &Value) -> bool {
    let Some(object) = node.as_object() else {
        return false;
    };
    if object.contains_key("tr/act") || object.contains_key("tr/move") {
        return true;
    }
    object
        .iter()
        .any(|(key, value)| !key.starts_with("tr/") && has_rules(value))
}

/// Follows a `tr/move` path from the map root.
fn resolve<'a>(root: &'a Map<String, Value>, path: &[Value]) -> Option<&'a Value> {
    let mut segments = path.iter().map(|v| v.as_str().unwrap_or_default());
    let first = segments.next()?;
    let mut node = root.get(first)?;
    for segment in segments {
        node = node.as_object()?.get(segment)?;
    }
    Some(node)
}

/// Drops moves whose target carries no rules, then re-prunes emptied parents.
///
/// fhirbase omits these. `CapabilityStatement.rest.operation` points at
/// `CapabilityStatement.rest.resource.operation`, which has no references and no
/// choices, so the branch is dead weight — while `QuestionnaireResponse.item.item`
/// points at a node that does carry rules and is kept. Removing one can empty
/// its parent, so this runs to a fixpoint.
fn prune_dangling_moves(root: &mut Map<String, Value>) {
    for _ in 0..20 {
        let snapshot = root.clone();
        let mut changed = false;

        let keys: Vec<String> = root.keys().cloned().collect();
        for key in keys {
            let Some(node) = root.get(&key) else { continue };
            if let Some(pruned) = prune_node(node, &snapshot) {
                if &pruned != node {
                    root.insert(key, pruned);
                    changed = true;
                }
            } else {
                root.remove(&key);
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }
}

/// Prunes one node, returning `None` when nothing rule-bearing survives.
fn prune_node(node: &Value, root: &Map<String, Value>) -> Option<Value> {
    let object = node.as_object()?;
    let mut out = Map::new();

    for (key, value) in object {
        if key == "tr/move" {
            let target = value.as_array()?;
            let resolved = resolve(root, target)?;
            if !has_rules(resolved) {
                return None;
            }
            out.insert(key.clone(), value.clone());
        } else if key.starts_with("tr/") {
            out.insert(key.clone(), value.clone());
        } else if let Some(child) = prune_node(value, root) {
            out.insert(key.clone(), child);
        }
    }

    let result = Value::Object(out);
    // A node carrying nothing but `tr/isCollection` is not a rule.
    has_rules(&result).then_some(result)
}
