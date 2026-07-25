//! Async disclosure logging (spec PR12.6). Gated on FHIRPG_TEST_DB.
//!
//! Three properties, each of which is the difference between a usable audit
//! trail and a decorative one: batched records actually reach the log,
//! shutdown drains what is still queued, and a saturated queue refuses the
//! read instead of dropping the record.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fhirpg_server::audit::{AuditMode, AuditSink};
use fhirpg_store::{AccessRecord, Audit, Store};

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
    // SAFETY: set before this binary spawns anything concurrent.
    unsafe { std::env::set_var("PGDATABASE", &db) };
    let map = Arc::new(fhirpg_gen::generate(&defs, schema).expect("generate"));
    let cfg = fhirpg_store::pg_config(None).expect("cfg");
    let store = Store::connect(cfg, map).await.expect("connect");
    store.drop_schema().await.expect("drop");
    store.init("audit-async").await.expect("init");
    Some(Arc::new(store))
}

fn record(n: usize) -> AccessRecord {
    AccessRecord {
        audit: Audit {
            actor: format!("clinician-{n}"),
            actor_source: Some("test".into()),
            client: None,
            request_id: Some(format!("req-{n}")),
            reason: Some("treatment".into()),
        },
        interaction: "read".into(),
        rtype: Some("Patient".into()),
        id: Some("p1".into()),
        version_id: None,
        outcome: "ok".into(),
        result_count: None,
    }
}

/// Queued records reach the log, and shutdown drains what is still in flight.
///
/// The drain is the part worth pinning: closing the channel is what tells the
/// writer to finish, so a shutdown that waits on the wrong side of it hangs
/// forever instead of flushing. This test is wrapped in a timeout for exactly
/// that reason — a deadlock here must fail, not hang the suite.
#[tokio::test]
async fn async_mode_batches_and_drains_on_shutdown() {
    let Some(store) = test_store("auditasync").await else {
        eprintln!("skipping: FHIRPG_TEST_DB not set or spec missing");
        return;
    };
    let sink = AuditSink::new(
        AuditMode::Async {
            capacity: 1024,
            batch: 16,
            // Long enough that the batch threshold, not the timer, is what
            // flushes most of these.
            interval: Duration::from_secs(30),
        },
        store.clone(),
    );

    const N: usize = 100;
    for i in 0..N {
        sink.record(&store, record(i)).await.expect("enqueue");
    }
    // 100 records at a batch size of 16 leaves a partial batch that only the
    // shutdown drain can flush, since the timer will not fire for 30s.
    tokio::time::timeout(Duration::from_secs(20), sink.shutdown())
        .await
        .expect("shutdown drained without deadlocking");

    let rows = store
        .access_log_for("Patient", "p1")
        .await
        .expect("access log");
    assert_eq!(rows.len(), N, "every queued record must be written");
    assert!(rows.iter().all(|(_, i, o)| i == "read" && o == "ok"));

    let m = sink.metrics();
    assert_eq!(
        m.enqueued.load(std::sync::atomic::Ordering::Relaxed),
        N as u64
    );
    assert_eq!(
        m.written.load(std::sync::atomic::Ordering::Relaxed),
        N as u64
    );
    assert_eq!(m.lost.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(m.depth(), 0, "nothing should remain queued after a drain");
}

/// A full queue refuses the read rather than dropping the record.
///
/// Dropping it would serve patient data with no trace that it was served,
/// which is indistinguishable afterwards from a disclosure that never
/// happened. Refusing is the only answer that stays honest under load.
#[tokio::test]
async fn saturated_queue_refuses_rather_than_drops() {
    let Some(store) = test_store("auditsat").await else {
        eprintln!("skipping: FHIRPG_TEST_DB not set or spec missing");
        return;
    };
    // Capacity 1 and a timer that will not fire during the test: the writer
    // takes at most one record, so the rest have nowhere to go.
    let sink = AuditSink::new(
        AuditMode::Async {
            capacity: 1,
            batch: 1,
            interval: Duration::from_secs(3600),
        },
        store.clone(),
    );

    let mut refused = 0;
    for i in 0..200 {
        if sink.record(&store, record(i)).await.is_err() {
            refused += 1;
        }
    }
    assert!(
        refused > 0,
        "a queue of one should have refused something out of 200"
    );
    assert_eq!(
        sink.metrics()
            .refused
            .load(std::sync::atomic::Ordering::Relaxed),
        refused as u64,
        "every refusal must be counted"
    );
    assert_eq!(
        sink.metrics()
            .lost
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "refusing is not losing: nothing was dropped silently"
    );
    tokio::time::timeout(Duration::from_secs(20), sink.shutdown())
        .await
        .expect("shutdown");
}
