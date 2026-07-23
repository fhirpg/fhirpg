//! The stored-procedure behaviour suite (task T11a).
//!
//! Test-only module. This is the **safety net** that must exist before T11b
//! changes identifier generation and T11c rewrites the archival SQL: it pins
//! what `fhirpg_create`, `fhirpg_update`, `fhirpg_read`, and `fhirpg_delete`
//! actually do today, against the faithfully translated procedures.
//!
//! It has three kinds of test, and the distinction matters:
//!
//! 1. **Golden tests** — behaviour to preserve. T11c must leave every one of
//!    these passing unchanged; that is how we know the rewrite preserved
//!    semantics.
//! 2. **Defect witnesses** — behaviour that is wrong and is scheduled to be
//!    fixed. Each asserts the *current, broken* result and names the defect.
//!    T11c flips them, and the flip is the evidence the fix landed.
//! 3. **The concurrency test** — decision D13's justification. Either it
//!    demonstrates the stale-pre-image race, or D13 is downgraded to a
//!    simplification. See [`d13_concurrency`].
//!
//! Every test here needs a live PostgreSQL 18 and is `#[ignore]`d.

use tokio_postgres::Client;

use crate::assets::FhirVersion;
use crate::commands::init;
use crate::testdb;

/// Creates a throwaway database with the FHIR 4.0.0 schema installed.
async fn initialized(suffix: &str) -> Option<(testdb::TestDb, Client)> {
    let db = testdb::create(suffix).await?;
    let client = db.connect().await;
    init::perform(&client, FhirVersion::V4_0_0)
        .await
        .unwrap_or_else(|e| panic!("init failed: {e}"));
    Some((db, client))
}

/// Calls a procedure returning `jsonb` and gives back the value.
async fn call(client: &Client, sql: &str) -> serde_json::Value {
    let row = client
        .query_one(sql, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql} failed: {e}"));
    row.get::<_, Option<serde_json::Value>>(0)
        .unwrap_or(serde_json::Value::Null)
}

/// Renders a database error including its source chain.
///
/// `tokio_postgres::Error`'s own `Display` is the useless string `"db error"`;
/// everything that identifies the failure — the SQLSTATE, the message, the
/// offending statement — hangs off `source()`. Without this, a failing
/// assertion reports nothing you can act on.
fn describe(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut current = error.source();
    while let Some(cause) = current {
        parts.push(cause.to_string());
        current = cause.source();
    }
    parts.join(": ")
}

/// The `(txid, status)` pairs in a resource's history, oldest first.
async fn history(client: &Client, id: &str) -> Vec<(i64, String)> {
    client
        .query(
            "SELECT txid, status::text FROM patient_history WHERE id = $1 ORDER BY txid",
            &[&id],
        )
        .await
        .unwrap_or_else(|e| panic!("cannot read history: {e}"))
        .iter()
        .map(|r| (r.get::<_, i64>(0), r.get::<_, String>(1)))
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Golden tests — behaviour T11c must preserve.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
async fn create_then_recreate_archives_the_previous_version() {
    let Some((db, client)) = initialized("proc_create").await else {
        return;
    };

    let first = call(
        &client,
        r#"SELECT fhirpg_create('{"resourceType":"Patient","id":"p1","v":1}'::jsonb)"#,
    )
    .await;
    assert_eq!(first["id"], "p1");
    assert_eq!(first["v"], 1);
    assert_eq!(first["meta"]["versionId"], "1");

    // Nothing to archive on a first create.
    assert!(history(&client, "p1").await.is_empty());

    let second = call(
        &client,
        r#"SELECT fhirpg_create('{"resourceType":"Patient","id":"p1","v":2}'::jsonb)"#,
    )
    .await;
    assert_eq!(second["v"], 2);
    assert_eq!(second["meta"]["versionId"], "2");

    // The previous version is archived with the status it had.
    assert_eq!(history(&client, "p1").await, vec![(1, "created".to_owned())]);

    // And the live row is marked `recreated`, not `created`.
    let row = client
        .query_one("SELECT status::text FROM patient WHERE id = 'p1'", &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "recreated");

    db.drop().await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
async fn update_archives_and_marks_the_live_row_updated() {
    let Some((db, client)) = initialized("proc_update").await else {
        return;
    };

    call(
        &client,
        r#"SELECT fhirpg_create('{"resourceType":"Patient","id":"p1","v":1}'::jsonb)"#,
    )
    .await;
    let updated = call(
        &client,
        r#"SELECT fhirpg_update('{"resourceType":"Patient","id":"p1","v":2}'::jsonb)"#,
    )
    .await;

    assert_eq!(updated["v"], 2);
    assert_eq!(updated["id"], "p1");
    assert_eq!(history(&client, "p1").await, vec![(1, "created".to_owned())]);

    let row = client
        .query_one("SELECT status::text FROM patient WHERE id = 'p1'", &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "updated");

    db.drop().await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
async fn create_and_update_disagree_on_whether_id_lives_in_the_jsonb() {
    // Not a defect so much as an asymmetry, but it is load-bearing: `create`
    // does `jsonb_set(resource, '{id}', …)` while `update` does `resource -
    // 'id'`. `_fhirpg_to_resource` puts `id` back on the way out, so the API
    // hides it — but anything reading the `resource` column directly, which is
    // the whole point of this tool, sees the difference.
    let Some((db, client)) = initialized("proc_idcol").await else {
        return;
    };

    call(
        &client,
        r#"SELECT fhirpg_create('{"resourceType":"Patient","id":"p1"}'::jsonb)"#,
    )
    .await;
    let after_create: bool = client
        .query_one("SELECT resource ? 'id' FROM patient WHERE id = 'p1'", &[])
        .await
        .unwrap()
        .get(0);
    assert!(after_create, "create stores id inside the jsonb");

    call(
        &client,
        r#"SELECT fhirpg_update('{"resourceType":"Patient","id":"p1","v":2}'::jsonb)"#,
    )
    .await;
    let after_update: bool = client
        .query_one("SELECT resource ? 'id' FROM patient WHERE id = 'p1'", &[])
        .await
        .unwrap()
        .get(0);
    assert!(!after_update, "update strips id from the jsonb");

    // Either way the procedure API returns it.
    let read = call(&client, "SELECT fhirpg_read('Patient','p1')").await;
    assert_eq!(read["id"], "p1");

    db.drop().await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
async fn read_returns_null_for_an_absent_resource() {
    let Some((db, client)) = initialized("proc_read").await else {
        return;
    };
    let missing = call(&client, "SELECT fhirpg_read('Patient','nope')").await;
    assert!(missing.is_null());
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
async fn update_without_an_id_is_rejected() {
    let Some((db, client)) = initialized("proc_noid").await else {
        return;
    };
    let err = client
        .query_one(
            r#"SELECT fhirpg_update('{"resourceType":"Patient"}'::jsonb)"#,
            &[],
        )
        .await
        .expect_err("update must require an id");
    // The upstream message has a typo ("and id"); pinned as-is because it is
    // the observable behaviour, not because it is good.
    let message = describe(&err);
    assert!(message.contains("does not have"), "{message}");
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
async fn delete_removes_the_live_row_and_returns_the_pre_image() {
    let Some((db, client)) = initialized("proc_delete").await else {
        return;
    };

    call(
        &client,
        r#"SELECT fhirpg_create('{"resourceType":"Patient","id":"p1","v":1}'::jsonb)"#,
    )
    .await;
    let returned = call(&client, "SELECT fhirpg_delete('Patient','p1')").await;

    assert_eq!(returned["v"], 1, "delete returns the archived pre-image");

    let remaining: i64 = client
        .query_one("SELECT count(*) FROM patient WHERE id = 'p1'", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(remaining, 0);

    db.drop().await;
}

// ---------------------------------------------------------------------------
// 2. Defect witnesses — behaviour that is wrong, pinned so the fix is visible.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
async fn witness_x13_delete_never_records_the_deleted_status() {
    // Defect X13. `fhirpg_delete`'s second CTE copies `status` from the live
    // row instead of writing 'deleted', so the `resource_status` enum's
    // 'deleted' value is never produced by any procedure and history cannot
    // distinguish a delete from an update.
    let Some((db, client)) = initialized("x13").await else {
        return;
    };

    call(
        &client,
        r#"SELECT fhirpg_create('{"resourceType":"Patient","id":"p1"}'::jsonb)"#,
    )
    .await;
    call(
        &client,
        r#"SELECT fhirpg_update('{"resourceType":"Patient","id":"p1","v":2}'::jsonb)"#,
    )
    .await;
    call(&client, "SELECT fhirpg_delete('Patient','p1')").await;

    let statuses: Vec<String> = history(&client, "p1")
        .await
        .into_iter()
        .map(|(_, s)| s)
        .collect();

    // CURRENT (broken): the delete is recorded as 'updated'.
    assert_eq!(statuses, vec!["created", "updated", "updated"]);
    assert!(
        !statuses.iter().any(|s| s == "deleted"),
        "X13 is fixed — flip this witness to assert the last status is 'deleted'"
    );

    db.drop().await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
async fn witness_x14_delete_collides_when_txid_matches_the_live_row() {
    // Defect X14. `fhirpg_delete` inserts into `_history` twice: once with the
    // row's existing txid and once with the given txid. `_history`'s primary
    // key is (id, txid), so passing a txid equal to the row's current one is a
    // unique violation.
    //
    // This is not hypothetical. Every bulk-loaded resource is written with
    // txid = 0 (fhirbase hardcodes it — defect X10), so
    // `fhirpg_delete(rt, id, 0)` on loaded data always fails.
    let Some((db, client)) = initialized("x14").await else {
        return;
    };

    client
        .batch_execute(
            r#"INSERT INTO patient (id, txid, status, resource)
               VALUES ('loaded', 0, 'created', '{"resourceType":"Patient"}'::jsonb)"#,
        )
        .await
        .unwrap();

    let err = client
        .query_one("SELECT fhirpg_delete('Patient','loaded',0)", &[])
        .await
        .expect_err("X14 is fixed — flip this witness");

    let message = describe(&err);
    assert!(
        message.contains("duplicate key") || message.contains("unique constraint"),
        "expected a primary-key violation, got: {message}"
    );

    // A distinct txid works, which is why this has gone unnoticed.
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
async fn witness_x15_procedures_interpolate_the_resource_type_unquoted() {
    // Defect X15, the stored-procedure twin of X2. Every procedure builds SQL
    // with `format('%s', resource_type)` rather than `%I`, so:
    //
    //   * a resource type that lowercases to a reserved word is unusable —
    //     FHIR's `Group` is exactly that; and
    //   * the value, which comes from resource content, is concatenated into a
    //     string that `EXECUTE` runs.
    let Some((db, client)) = initialized("x15").await else {
        return;
    };

    // The tables exist and are writable when quoted (asserted in T11's suite),
    // so the failure is entirely the procedure's unquoted interpolation.
    let err = client
        .query_one(
            r#"SELECT fhirpg_create('{"resourceType":"Group","id":"g1"}'::jsonb)"#,
            &[],
        )
        .await
        .expect_err("X15 is fixed — flip this witness");
    let message = describe(&err);
    assert!(message.contains("syntax error"), "{message}");
    assert!(message.contains("Group"), "the unquoted type reaches the SQL: {message}");

    let err = client
        .query_one("SELECT fhirpg_read('Group','g1')", &[])
        .await
        .expect_err("X15 is fixed — flip this witness");
    assert!(describe(&err).contains("syntax error"), "{}", describe(&err));

    // And the untrusted value really does reach the executed statement.
    //
    // `fhirpg_read` builds `SELECT … FROM %s r WHERE r.id = $1`. Feeding it
    // `patient r2, pg_class WHERE false; --` yields
    // `FROM patient r2, pg_class WHERE false; -- r`, so the injected text has
    // introduced a join, a predicate, and commented out the query's own alias
    // — hence "missing FROM-clause entry for table r". The structure of the
    // executed query was changed by resource-derived data, which is the whole
    // of the finding.
    //
    // We assert the mechanism, not a working exploit. Whether a payload can be
    // crafted that stays syntactically valid at all four interpolation sites is
    // beside the point: the fix is `%I` either way.
    let err = client
        .query_one(
            "SELECT fhirpg_read('patient r2, pg_class WHERE false; --', 'x')",
            &[],
        )
        .await
        .expect_err("X15 is fixed — flip this witness");
    let message = describe(&err);
    assert!(
        message.contains("missing FROM-clause entry")
            || message.contains("syntax error")
            || message.contains("pg_class"),
        "the injected text did not reach the query: {message}"
    );

    db.drop().await;
}

// ---------------------------------------------------------------------------
// 3. The D13 concurrency test.
// ---------------------------------------------------------------------------

/// Decision D13 claims the archival CTE can archive a stale pre-image.
///
/// The reasoning: `archived` reads the row through a separate
/// `SELECT … WHERE id = $2`, which sees the statement snapshot, while the
/// sibling `INSERT … ON CONFLICT DO UPDATE` re-reads the live row when it
/// blocks on a concurrent writer (`EvalPlanQual`). Under `READ COMMITTED` those
/// two can therefore disagree, and the version written to `_history` would not
/// be the version actually replaced — silently losing it from history.
///
/// This test forces the interleaving deterministically, with no sleeps:
///
/// 1. session A begins a transaction and updates the row, holding its lock;
/// 2. session B calls `fhirpg_create`, which blocks on that lock;
/// 3. we wait until `pg_stat_activity` shows B genuinely blocked;
/// 4. session A commits;
/// 5. B proceeds, and we inspect what landed in `_history`.
#[tokio::test]
#[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
async fn d13_concurrency() {
    let Some((db, client)) = initialized("d13").await else {
        return;
    };

    call(
        &client,
        r#"SELECT fhirpg_create('{"resourceType":"Patient","id":"p1","v":1}'::jsonb)"#,
    )
    .await;

    let writer = db.connect().await;
    let creator = db.connect().await;

    let creator_pid: i32 = creator
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);

    // 1. Session A takes the row lock and holds it.
    writer.batch_execute("BEGIN").await.unwrap();
    writer
        .batch_execute(
            "UPDATE patient SET resource = jsonb_set(resource, '{v}', '2'), \
             txid = 99, status = 'updated' WHERE id = 'p1'",
        )
        .await
        .unwrap();

    // 2. Session B calls create; it will block on A's lock.
    let creating = tokio::spawn(async move {
        let row = creator
            .query_one(
                r#"SELECT fhirpg_create('{"resourceType":"Patient","id":"p1","v":3}'::jsonb)"#,
                &[],
            )
            .await;
        row.map(|r| r.get::<_, Option<serde_json::Value>>(0))
    });

    // 3. Wait until B is genuinely blocked. Bounded, and deterministic in the
    //    sense that we observe the state rather than guess at a duration.
    let mut blocked = false;
    for _ in 0..200 {
        let waiting: i64 = client
            .query_one(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE pid = $1 AND wait_event_type = 'Lock'",
                &[&creator_pid],
            )
            .await
            .unwrap()
            .get(0);
        if waiting > 0 {
            blocked = true;
            break;
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(blocked, "session B never blocked; the test proves nothing");

    // 4. Release the lock.
    writer.batch_execute("COMMIT").await.unwrap();

    // 5. Let B finish.
    let created = creating
        .await
        .expect("the create task panicked")
        .expect("create failed");
    let created = created.expect("create returned null");
    assert_eq!(created["v"], 3, "the live row should now be B's version");

    let archived = history(&client, "p1").await;
    let archived_versions: Vec<i64> = client
        .query(
            "SELECT (resource->>'v')::bigint FROM patient_history WHERE id = 'p1' ORDER BY txid",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<_, i64>(0))
        .collect();

    // The question D13 turns on: was A's committed version (v = 2) archived?
    //
    // If v = 2 is missing, the race is real: B replaced A's row but archived
    // the pre-A version, so A's version is lost from history entirely.
    let race_reproduced = !archived_versions.contains(&2);

    println!(
        "D13 concurrency result: history = {archived:?}, versions = {archived_versions:?}, \
         race_reproduced = {race_reproduced}"
    );

    // Recorded either way. This assertion documents the outcome that was
    // actually observed; see spec §2.4 and plan.md D13.
    assert!(
        race_reproduced,
        "The stale-pre-image race did NOT reproduce: A's version (v=2) was \
         archived correctly as {archived_versions:?}. D13's correctness \
         justification does not hold and must be downgraded to a \
         simplification-only change in plan.md and spec §2.4."
    );

    db.drop().await;
}
