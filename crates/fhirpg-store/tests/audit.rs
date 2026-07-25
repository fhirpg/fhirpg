//! Auditability (spec T11.8). Gated on FHIRPG_TEST_DB.
//!
//! Five properties, each of which an auditor asks about directly:
//! every change records who made it (M3.15), every read leaves a disclosure
//! record (PR12.5), the history chain verifies (M3.16), and the database
//! itself refuses to let history be rewritten (M3.17).

use std::path::PathBuf;
use std::sync::Arc;

use fhirpg_store::{AccessRecord, Audit, Store};
use serde_json::{Value, json};

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

async fn test_store(schema: &str) -> Option<Arc<Store>> {
    let db = std::env::var("FHIRPG_TEST_DB").ok()?;
    let defs = spec_defs()?;
    // SAFETY: single test binary, set before any threads matter.
    unsafe { std::env::set_var("PGDATABASE", &db) };
    let map = Arc::new(fhirpg_gen::generate(&defs, schema).expect("generate"));
    let cfg = fhirpg_store::pg_config(None).expect("cfg");
    let store = Store::connect(cfg, map).await.expect("connect");
    store.drop_schema().await.expect("drop");
    assert!(store.init("audit-checksum").await.expect("init"));
    Some(Arc::new(store))
}

fn patient(id: &str, family: &str) -> Value {
    json!({
        "resourceType": "Patient",
        "id": id,
        "name": [{"family": family}]
    })
}

fn nurse() -> Audit {
    Audit::principal("practitioner-42", "header:X-Fhirpg-Principal")
        .with_client(Some("10.0.0.7".to_string()))
        .with_request_id(Some("req-abc".to_string()))
        .with_reason(Some("treatment".to_string()))
}

/// Installing a full R5 schema is ~7,355 tables; five of them at once
/// exhausts the server's lock budget. These properties are independent of
/// each other, so they share one schema and run in sequence.
#[tokio::test]
async fn audit_trail_is_complete_and_tamper_evident() {
    let Some(store) = test_store("audit_all").await else {
        eprintln!("skipping: FHIRPG_TEST_DB not set or spec missing");
        return;
    };
    every_change_records_who_made_it(&store).await;
    an_unattributed_write_says_so(&store).await;
    reads_leave_a_disclosure_record(&store).await;
    the_hash_chain_verifies_and_catches_tampering(&store).await;
    the_database_refuses_to_rewrite_history(&store).await;
    erasure_leaves_a_verifiable_hole(&store).await;
}

/// GDPR Art. 17 against append-only history (M3.18): the record goes, the
/// fact that it went does not.
async fn erasure_leaves_a_verifiable_hole(store: &Store) {
    for n in 0..3 {
        store
            .put_audited(&patient("p6", &format!("Erase{n}")), None, &nurse())
            .await
            .expect("write");
    }
    // Scoped to p6: an earlier step left p4 deliberately tampered, so the
    // global chain is *supposed* to be broken by now.
    let breaks_for = |bs: Vec<fhirpg_store::ChainBreak>| {
        bs.into_iter().filter(|b| b.id == "p6").collect::<Vec<_>>()
    };
    assert!(breaks_for(store.verify_audit().await.expect("verify")).is_empty());

    let who = Audit::principal("dpo-7", "cli").with_reason(Some("art-17-request".to_string()));
    let report = store.purge("Patient", "p6", &who).await.expect("purge");
    assert!(report.existed);
    assert_eq!(report.versions_erased, 3);

    // The resource is gone.
    assert!(store.get("Patient", "p6").await.expect("read").is_none());

    // The tombstone remains, naming who erased it and why.
    let rows = store
        .raw_history_audit("Patient", "p6")
        .await
        .expect("audit rows");
    assert_eq!(rows.len(), 1, "only the tombstone survives");
    assert_eq!(rows[0].1, "dpo-7");
    assert_eq!(rows[0].5.as_deref(), Some("art-17-request"));

    // And it is not mistaken for tampering: an erasure is recorded, not a
    // chain break. Reporting it as a break would train an operator to ignore
    // the report.
    assert!(
        breaks_for(store.verify_audit().await.expect("verify")).is_empty(),
        "an erasure must not read as tampering"
    );
}

async fn every_change_records_who_made_it(store: &Store) {
    store
        .put_audited(&patient("p1", "One"), None, &nurse())
        .await
        .expect("create");
    store
        .put_audited(&patient("p1", "Two"), None, &nurse())
        .await
        .expect("update");
    store
        .delete_audited("Patient", "p1", &nurse())
        .await
        .expect("delete");

    let rows = store
        .raw_history_audit("Patient", "p1")
        .await
        .expect("audit rows");
    assert_eq!(rows.len(), 3, "create, update, delete");
    for (version, actor, source, client, request_id, reason) in &rows {
        assert_eq!(actor, "practitioner-42", "version {version}");
        assert_eq!(source.as_deref(), Some("header:X-Fhirpg-Principal"));
        assert_eq!(client.as_deref(), Some("10.0.0.7"));
        assert_eq!(request_id.as_deref(), Some("req-abc"));
        assert_eq!(reason.as_deref(), Some("treatment"));
    }
}

async fn an_unattributed_write_says_so(store: &Store) {
    // The plain `put` is what CLI and tests use; it must still record
    // *something*, because a blank actor is indistinguishable from a lost one.
    store.put(&patient("p2", "Anon")).await.expect("create");
    let rows = store
        .raw_history_audit("Patient", "p2")
        .await
        .expect("audit rows");
    assert_eq!(rows[0].1, "unauthenticated");
}

async fn reads_leave_a_disclosure_record(store: &Store) {
    store.put(&patient("p3", "Seen")).await.expect("create");
    store.get("Patient", "p3").await.expect("read");
    store
        .log_access(&AccessRecord {
            audit: nurse(),
            interaction: "read".to_string(),
            rtype: Some("Patient".to_string()),
            id: Some("p3".to_string()),
            version_id: Some(1),
            outcome: "ok".to_string(),
            result_count: None,
        })
        .await
        .expect("log");

    let seen = store
        .access_log_for("Patient", "p3")
        .await
        .expect("access log");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "practitioner-42");
    assert_eq!(seen[0].1, "read");
    assert_eq!(seen[0].2, "ok");
}

async fn the_hash_chain_verifies_and_catches_tampering(store: &Store) {
    for n in 0..4 {
        store
            .put_audited(&patient("p4", &format!("Gen{n}")), None, &nurse())
            .await
            .expect("write");
    }
    assert!(
        store.verify_audit().await.expect("verify").is_empty(),
        "an untouched chain must verify"
    );

    // Rewrite one row behind the application's back, as an attacker with
    // database access would. The trigger forbids UPDATE, so this has to
    // disable it first — which is exactly the "deliberate DBA act" M3.17
    // describes, and the chain is what notices afterwards.
    store
        .execute_raw_for_test(
            "ALTER TABLE patient_history DISABLE TRIGGER ALL;\n\
             UPDATE patient_history SET resource = jsonb_set(resource, '{name,0,family}', '\"Forged\"') \
               WHERE id = 'p4' AND version_id = 2;\n\
             ALTER TABLE patient_history ENABLE TRIGGER ALL;",
        )
        .await
        .expect("tamper");

    let breaks = store.verify_audit().await.expect("verify");
    assert_eq!(breaks.len(), 1, "exactly the tampered version: {breaks:?}");
    assert_eq!(breaks[0].id, "p4");
    assert_eq!(breaks[0].version_id, 2);
}

async fn the_database_refuses_to_rewrite_history(store: &Store) {
    store.put(&patient("p5", "Fixed")).await.expect("create");

    let update = store
        .execute_raw_for_test("UPDATE patient_history SET actor = 'someone-else' WHERE id = 'p5'")
        .await;
    let err = update
        .expect_err("UPDATE on history must be refused")
        .to_string();
    assert!(
        err.contains("append-only"),
        "the refusal should say why: {err}"
    );

    let delete = store
        .execute_raw_for_test("DELETE FROM patient_history WHERE id = 'p5'")
        .await;
    assert!(delete.is_err(), "DELETE on history must be refused");
}
