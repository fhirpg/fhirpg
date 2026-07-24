//! Full-corpus round trip: every example shipped with the FHIR
//! specifications (examples-json.zip), when a corpus directory is present.
//! Set FHIRPG_CORPUS_DIR to a directory containing r5/, r4/, stu3/
//! subdirectories of raw example JSON files.

use std::collections::HashMap;
use std::path::PathBuf;

use fhirpg_map::reconstruct::{InRow, ReconIn, reconstruct};
use fhirpg_map::shred::{SqlVal, shred};
use serde_json::Value;

fn corpus_root() -> Option<PathBuf> {
    let candidates = [
        std::env::var("FHIRPG_CORPUS_DIR").ok().map(PathBuf::from),
        Some(PathBuf::from(
            "/private/tmp/claude-501/-Users-jph-git-joelparkerhenderson-fhirpg/909837b9-87b5-4b52-a210-a96a407e5308/scratchpad/corpus",
        )),
    ];
    candidates.into_iter().flatten().find(|p| p.exists())
}

fn spec_root() -> Option<PathBuf> {
    let candidates = [
        std::env::var("FHIRPG_SPEC_DIR").ok().map(PathBuf::from),
        Some(PathBuf::from(
            "/Users/jph/git/joelparkerhenderson/fhir-rust-crate/doc/fhir-specifications",
        )),
    ];
    candidates.into_iter().flatten().find(|p| p.exists())
}

fn to_recon(rm: &fhirpg_map::ResourceMap, out: &fhirpg_map::ShredOut) -> ReconIn {
    let mut tables: Vec<Vec<InRow>> = vec![Vec::new(); rm.tables.len()];
    for row in &out.rows {
        let mut cols = HashMap::new();
        for (name, val) in &row.cols {
            let text = match val {
                SqlVal::Bool(b) => b.to_string(),
                SqlVal::Int(n) => n.to_string(),
                SqlVal::Num(s) | SqlVal::Text(s) | SqlVal::Ts(s) | SqlVal::Date(s) => s.clone(),
                SqlVal::Jsonb(s) => s.clone(),
            };
            cols.insert(name.clone(), text);
        }
        tables[row.table as usize].push(InRow {
            ords: row.ords.clone(),
            cols,
        });
    }
    ReconIn {
        tables,
        ext: out.ext.clone(),
        deep: out.deep.clone(),
        contained: out.contained.clone(),
    }
}

fn sem_eq(a: &Value, b: &Value, path: &str, diffs: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(ma), Value::Object(mb)) => {
            for (k, va) in ma {
                match mb.get(k) {
                    Some(vb) => sem_eq(va, vb, &format!("{path}.{k}"), diffs),
                    None => diffs.push(format!("{path}.{k}: missing in reconstruction")),
                }
            }
            for k in mb.keys() {
                if !ma.contains_key(k) {
                    diffs.push(format!("{path}.{k}: extra in reconstruction"));
                }
            }
        }
        (Value::Array(aa), Value::Array(ab)) => {
            if aa.len() != ab.len() {
                diffs.push(format!("{path}: array length {} vs {}", aa.len(), ab.len()));
                return;
            }
            for (i, (va, vb)) in aa.iter().zip(ab).enumerate() {
                sem_eq(va, vb, &format!("{path}[{i}]"), diffs);
            }
        }
        _ => {
            if a != b {
                diffs.push(format!("{path}: {a} vs {b}"));
            }
        }
    }
}

#[test]
fn roundtrip_full_corpus() {
    let (Some(corpus), Some(spec)) = (corpus_root(), spec_root()) else {
        eprintln!("skipping: corpus or spec dir missing");
        return;
    };
    let mut all_failures: Vec<String> = Vec::new();
    let mut total = 0usize;
    for (cdir, sdir, schema) in [("stu3", "r3", "r3"), ("r4", "r4", "r4"), ("r5", "r5", "r5")] {
        let ex = corpus.join(cdir);
        let defs = spec.join(sdir).join("fhir-definitions-json");
        if !ex.exists() || !defs.exists() {
            continue;
        }
        let map = fhirpg_gen::generate(&defs, schema).expect("generate");
        let mut entries: Vec<_> = std::fs::read_dir(&ex)
            .expect("dir")
            .map(|e| e.expect("entry").path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        entries.sort();
        let mut count = 0usize;
        let mut failures: Vec<String> = Vec::new();
        for path in entries {
            let bytes = std::fs::read(&path).expect("read");
            let Ok(v): Result<Value, _> = serde_json::from_slice(&bytes) else {
                continue;
            };
            let Some(rt) = v.get("resourceType").and_then(Value::as_str) else {
                continue;
            };
            let Some(rm) = map.resources.get(rt) else {
                // Conformance/abstract artifacts not in the concrete set.
                continue;
            };
            count += 1;
            let name = path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .to_string();
            let out = match shred(rm, &v) {
                Ok(o) => o,
                Err(e) => {
                    failures.push(format!("{name}: shred: {e}"));
                    continue;
                }
            };
            let rin = to_recon(rm, &out);
            let back = match reconstruct(rm, &rin, out.id.as_deref()) {
                Ok(b) => b,
                Err(e) => {
                    failures.push(format!("{name}: reconstruct: {e}"));
                    continue;
                }
            };
            let mut diffs = Vec::new();
            sem_eq(&v, &back, "$", &mut diffs);
            if !diffs.is_empty() {
                diffs.truncate(3);
                failures.push(format!("{name}: {}", diffs.join(" | ")));
            }
        }
        eprintln!(
            "{sdir}: {count} corpus examples, {} failures",
            failures.len()
        );
        total += count;
        for f in failures.iter().take(40) {
            all_failures.push(format!("[{sdir}] {f}"));
        }
        if failures.len() > 40 {
            all_failures.push(format!("[{sdir}] … and {} more", failures.len() - 40));
        }
    }
    assert!(total > 1000, "expected a real corpus, ran {total}");
    assert!(
        all_failures.is_empty(),
        "round-trip failures:\n{}",
        all_failures.join("\n")
    );
}
