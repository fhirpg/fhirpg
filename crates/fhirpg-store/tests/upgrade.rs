//! Schema upgrade (T26): install a reduced map, upgrade to the full one,
//! verify new tables/columns appear, destructive changes are guarded, and
//! data survives. Gated on FHIRPG_TEST_DB.

use std::path::PathBuf;
use std::sync::Arc;

use fhirpg_store::Store;
use serde_json::json;

fn spec_defs() -> Option<PathBuf> {
    let root = std::env::var("FHIRPG_SPEC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(
                "/Users/jph/git/joelparkerhenderson/fhir-rust-crate/doc/fhir-specifications",
            )
        });
    let defs = root.join("r5").join("fhir-definitions-json");
    defs.exists().then_some(defs)
}

#[tokio::test]
async fn upgrade_applies_diff() {
    let Ok(db) = std::env::var("FHIRPG_TEST_DB") else {
        eprintln!("skipping: FHIRPG_TEST_DB not set");
        return;
    };
    let Some(defs) = spec_defs() else {
        eprintln!("skipping: no spec dir");
        return;
    };
    // SAFETY: single-threaded at this point.
    unsafe { std::env::set_var("PGDATABASE", &db) };

    let full = fhirpg_gen::generate(&defs, "uptest").expect("generate");
    // The "old" deployment: no Basic resource at all, and Patient's base
    // table missing its last data column.
    let mut reduced = full.clone();
    reduced.resources.remove("Basic").expect("Basic exists");
    let removed_col = {
        let pat = reduced.resources.get_mut("Patient").expect("Patient");
        pat.tables[0].cols.pop().expect("has cols").name
    };

    let cfg = fhirpg_store::pg_config(None).expect("cfg");
    let old_store = Store::connect(cfg, Arc::new(reduced))
        .await
        .expect("connect");
    old_store.drop_schema().await.expect("drop");
    old_store.init("old-sum").await.expect("init old");
    // Seed data that must survive the upgrade.
    old_store
        .put(&json!({"resourceType": "Patient", "id": "keep",
                     "name": [{"family": "Survivor"}]}))
        .await
        .expect("seed");

    let cfg = fhirpg_store::pg_config(None).expect("cfg");
    let new_store = Store::connect(cfg, Arc::new(full)).await.expect("connect");
    // Plain init refuses (different checksum), upgrade applies.
    assert!(new_store.init("new-sum").await.is_err());
    let report = new_store.upgrade("new-sum", false).await.expect("upgrade");
    assert!(report.additive > 0, "expected additive changes");
    assert_eq!(report.destructive, 0);

    // The new column and the Basic tables exist and work.
    let got = new_store
        .get("Patient", "keep")
        .await
        .expect("get")
        .expect("kept");
    assert_eq!(got.resource["name"][0]["family"], "Survivor");
    new_store
        .put(&json!({"resourceType": "Basic", "id": "b1",
                     "code": {"text": "now supported"}}))
        .await
        .expect("basic put");
    let b = new_store
        .get("Basic", "b1")
        .await
        .expect("get")
        .expect("b1");
    assert_eq!(b.resource["code"]["text"], "now supported");
    let _ = removed_col;

    // Idempotent: a second upgrade to the same map is a no-op diff.
    let again = new_store
        .upgrade("new-sum", false)
        .await
        .expect("re-upgrade");
    assert_eq!(again.additive, 0);
    assert_eq!(again.destructive, 0);

    // Downgrade direction is destructive and must be guarded.
    let mut reduced2 = new_store.map().clone();
    reduced2.resources.remove("Basic");
    let cfg = fhirpg_store::pg_config(None).expect("cfg");
    let down_store = Store::connect(cfg, Arc::new(reduced2))
        .await
        .expect("connect");
    let err = down_store
        .upgrade("down-sum", false)
        .await
        .expect_err("guarded");
    assert!(err.to_string().contains("destructive"), "{err}");
    let report = down_store.upgrade("down-sum", true).await.expect("forced");
    assert!(report.destructive > 0);
}
