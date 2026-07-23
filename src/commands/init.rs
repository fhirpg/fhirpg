//! The `init` subcommand.
//!
//! Ports `PerformInit` and `InitCommand` (`dbinit.go:35-121`): create the
//! tables, types, and stored procedures that hold FHIR resources. Specified in
//! `spec/index.md` §2.
//!
//! Statements run in three groups, in order:
//!
//! 1. the selected FHIR version's schema — extension, enum, `transaction`, and
//!    a table pair per resource type;
//! 2. the stored procedures, shared by every version;
//! 3. the `concept` and `concept_history` tables, which fhirbase appends in Go
//!    rather than in the asset (`dbinit.go:16-33`).

use tokio_postgres::Client;

use crate::assets::FhirVersion;
use crate::config::PgConfig;
use crate::db;
use crate::error::{Error, Result};

/// The concept tables, appended after the version schema.
///
/// Reproduced from `dbinit.go:16-33`. They live in Go rather than in the asset
/// upstream, and stay here for the same reason: they are version-independent.
const CONCEPT_TABLES: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS "concept" (
id text primary key,
txid bigint not null,
ts timestamptz DEFAULT current_timestamp,
resource_type text default 'Concept',
status resource_status not null,
resource jsonb not null);"#,
    r#"CREATE TABLE IF NOT EXISTS "concept_history" (
id text,
txid bigint not null,
ts timestamptz DEFAULT current_timestamp,
resource_type text default 'Concept',
status resource_status not null,
resource jsonb not null,
PRIMARY KEY (id, txid)
);"#,
];

/// Whether a statement's failure should stop the run.
///
/// Decision D9: PostgreSQL 18 provides `gen_random_uuid()` in core, so the
/// `pgcrypto` extension is dead weight — but the nine vendored schemas open
/// with `CREATE EXTENSION IF NOT EXISTS pgcrypto` and are byte-identical by
/// contract (spec §3), so it cannot be edited out. On a server without
/// `contrib` that statement fails, and nothing depends on it.
fn is_tolerable(statement: &str) -> bool {
    let normalized = statement.trim_start().to_ascii_uppercase();
    normalized.starts_with("CREATE EXTENSION") && normalized.contains("PGCRYPTO")
}

/// Runs the `init` subcommand.
///
/// # Errors
///
/// Returns [`Error::Db`] if a statement fails, naming the statement and its
/// index, or [`Error::UnsupportedServerVersion`] if the server predates
/// PostgreSQL 18.
pub async fn run(config: &PgConfig, version: FhirVersion) -> Result<()> {
    let client = db::connect(config).await?;
    let report = perform(&client, version).await?;

    println!(
        "Database initialized with FHIR schema version '{version}' \
         ({} statements executed{})",
        report.executed,
        if report.tolerated == 0 {
            String::new()
        } else {
            format!(", {} tolerated", report.tolerated)
        }
    );

    Ok(())
}

/// What `perform` did, for reporting and for tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct InitReport {
    /// Statements that executed successfully.
    pub executed: usize,
    /// Statements that failed but were tolerated (see [`is_tolerable`]).
    pub tolerated: usize,
}

/// Executes the schema, the procedures, and the concept tables.
///
/// Separated from [`run`] so tests can drive it with a client they own.
///
/// # Errors
///
/// Returns [`Error::Db`] naming the failing statement and its index.
pub async fn perform(client: &Client, version: FhirVersion) -> Result<InitReport> {
    let mut statements = version.schema_statements()?;
    statements.extend(FhirVersion::function_statements()?);
    statements.extend(CONCEPT_TABLES.iter().map(|s| (*s).to_owned()));

    let total = statements.len();
    let mut report = InitReport::default();

    for (index, statement) in statements.iter().enumerate() {
        match client.batch_execute(statement).await {
            Ok(()) => report.executed += 1,
            Err(error) if is_tolerable(statement) => {
                // Decision D9. Worth saying out loud rather than swallowing:
                // an operator seeing this should understand it is expected.
                eprintln!(
                    "note: statement {} of {total} failed and was tolerated \
                     (PostgreSQL 18 provides gen_random_uuid() in core, so \
                     pgcrypto is not needed): {error}",
                    index + 1
                );
                report.tolerated += 1;
            }
            Err(error) => {
                return Err(Error::Db(format!(
                    "statement {} of {total} failed: {error}\n\
                     The target database may not be empty.\n\
                     Statement was:\n{statement}",
                    index + 1
                )));
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_pgcrypto_extension_is_tolerable() {
        assert!(is_tolerable("CREATE EXTENSION IF NOT EXISTS pgcrypto;"));
        assert!(is_tolerable("  create extension if not exists PGCRYPTO;"));

        // Nothing else may be swallowed: a failure anywhere else means the
        // schema is incomplete, and reporting success would be a lie.
        assert!(!is_tolerable("CREATE EXTENSION IF NOT EXISTS postgis;"));
        assert!(!is_tolerable("CREATE TABLE IF NOT EXISTS \"patient\" (id text)"));
        assert!(!is_tolerable("DROP TABLE patient"));
        assert!(!is_tolerable(
            "-- CREATE EXTENSION pgcrypto\nCREATE TABLE x (id text)"
        ));
    }

    #[test]
    fn the_first_statement_of_every_schema_is_the_tolerable_one() {
        // If this ever stops holding, D9's tolerance is silently covering a
        // different statement than intended.
        for &version in crate::assets::ALL_VERSIONS {
            let statements = version.schema_statements().unwrap();
            assert!(
                is_tolerable(&statements[0]),
                "FHIR {version} no longer opens with CREATE EXTENSION pgcrypto"
            );
            assert_eq!(
                statements.iter().filter(|s| is_tolerable(s)).count(),
                1,
                "FHIR {version} should have exactly one tolerable statement"
            );
        }
    }

    #[test]
    fn the_concept_tables_are_reproduced_from_the_go_source() {
        assert_eq!(CONCEPT_TABLES.len(), 2);
        assert!(CONCEPT_TABLES[0].contains(r#""concept""#));
        assert!(CONCEPT_TABLES[1].contains(r#""concept_history""#));
        assert!(CONCEPT_TABLES[1].contains("PRIMARY KEY (id, txid)"));
    }
}

/// Tests that need a live PostgreSQL 18. Gated behind `FHIRPG_TEST_DB`.
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::testdb;

    /// Runs `init` into a throwaway database and hands the client to `check`.
    async fn with_initialized_db<F, Fut>(version: FhirVersion, suffix: &str, check: F)
    where
        F: FnOnce(Client) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let Some(db) = testdb::create(suffix).await else {
            return;
        };
        let client = db.connect().await;
        let report = perform(&client, version)
            .await
            .unwrap_or_else(|e| panic!("init failed for FHIR {version}: {e}"));
        assert!(report.executed > 0);
        check(client).await;
        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn init_succeeds_for_every_supported_version() {
        for (i, &version) in crate::assets::ALL_VERSIONS.iter().enumerate() {
            with_initialized_db(version, &format!("all_v{i}"), |client| async move {
                // Every version must leave the sequence the single-argument
                // procedures depend on (spec §2.2).
                let row = client
                    .query_one(
                        "SELECT count(*) FROM information_schema.sequences \
                         WHERE sequence_name = 'transaction_id_seq'",
                        &[],
                    )
                    .await
                    .unwrap();
                assert_eq!(row.get::<_, i64>(0), 1, "FHIR {version}: no sequence");
            })
            .await;
        }
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn the_procedures_round_trip_a_patient() {
        with_initialized_db(FhirVersion::V4_0_0, "roundtrip", |client| async move {
            let row = client
                .query_one(
                    r#"SELECT fhirpg_create('{"resourceType":"Patient"}'::jsonb)"#,
                    &[],
                )
                .await
                .expect("fhirpg_create should exist and succeed");

            let created: serde_json::Value = row.get(0);
            assert_eq!(created["resourceType"], "Patient");
            assert!(created["id"].is_string(), "no id: {created}");
            assert!(created["meta"]["versionId"].is_string(), "{created}");
            assert!(created["meta"]["lastUpdated"].is_string(), "{created}");

            // And it can be read back by the procedure that finds it.
            let id = created["id"].as_str().unwrap();
            let row = client
                .query_one("SELECT fhirpg_read('Patient', $1)", &[&id])
                .await
                .unwrap();
            let read: serde_json::Value = row.get(0);
            assert_eq!(read["id"], created["id"]);
        })
        .await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn no_upstream_procedure_names_survive() {
        // Decision D3, checked where it actually matters: in the database.
        with_initialized_db(FhirVersion::V4_0_0, "names", |client| async move {
            let row = client
                .query_one(
                    "SELECT count(*) FROM pg_proc WHERE proname LIKE '%fhirbase%'",
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(row.get::<_, i64>(0), 0, "a fhirbase_* procedure was created");

            let row = client
                .query_one(
                    "SELECT count(*) FROM pg_proc WHERE proname LIKE 'fhirpg\\_%' \
                     OR proname LIKE '\\_fhirpg\\_%'",
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(row.get::<_, i64>(0), 9, "expected nine fhirpg procedures");
        })
        .await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn the_reserved_word_resource_type_gets_a_table() {
        // Defect X2 in the making: FHIR has a `Group` resource and `group` is a
        // PostgreSQL reserved word. The DDL quotes it, so the tables exist —
        // it is fhirbase's *insert loader* that cannot write to them (T14).
        with_initialized_db(FhirVersion::V4_0_0, "reserved", |client| async move {
            let row = client
                .query_one(
                    "SELECT count(*) FROM information_schema.tables \
                     WHERE table_schema = 'public' AND table_name IN ('group', 'group_history')",
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(row.get::<_, i64>(0), 2);

            // The quoted identifier really is usable.
            client
                .batch_execute(
                    r#"INSERT INTO "group" (id, txid, status, resource)
                       VALUES ('g1', 0, 'created', '{"resourceType":"Group"}'::jsonb)"#,
                )
                .await
                .expect("a quoted reserved-word table must accept writes");
        })
        .await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn re_running_init_on_an_initialized_database_is_diagnosed() {
        let Some(db) = testdb::create("rerun").await else {
            return;
        };
        let client = db.connect().await;

        perform(&client, FhirVersion::V4_0_0).await.unwrap();

        // The DDL is largely IF NOT EXISTS, so a second run may well succeed;
        // what must never happen is a panic or a silent lie. Assert we either
        // report success honestly or return an error naming the statement.
        match perform(&client, FhirVersion::V4_0_0).await {
            Ok(report) => assert!(report.executed > 0),
            Err(e) => {
                let message = e.to_string();
                assert!(message.contains("statement"), "{message}");
                assert!(message.contains("may not be empty"), "{message}");
            }
        }

        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn init_succeeds_when_the_pgcrypto_extension_cannot_be_created() {
        // Decision D9. Simulated the way it actually happens in the field: a
        // role without permission to create extensions, which is exactly what a
        // PostgreSQL 18 image without contrib, or a managed service, gives you.
        let Some(db) = testdb::create("nocontrib").await else {
            return;
        };
        let admin = db.connect().await;

        // pgcrypto must NOT already be installed, or the IF NOT EXISTS form
        // would succeed and the test would prove nothing.
        admin
            .batch_execute("DROP EXTENSION IF EXISTS pgcrypto")
            .await
            .unwrap();

        // CONNECT but *not* CREATE on the database, and CREATE on the schema.
        //
        // That combination is the whole trick. `pgcrypto` is a **trusted**
        // extension from PostgreSQL 13 on, so any role with CREATE on the
        // database can install it — granting CREATE there made an earlier
        // version of this test tolerate nothing and prove nothing. Withholding
        // it denies `CREATE EXTENSION` ("must have CREATE privilege on current
        // database") while `CREATE TABLE` still works, because that needs
        // CREATE on the schema instead.
        let role = format!("fhirpg_limited_{}", std::process::id());
        admin
            .batch_execute(&format!(
                "DROP ROLE IF EXISTS {role};
                 CREATE ROLE {role} LOGIN PASSWORD 'limited';
                 GRANT CONNECT ON DATABASE \"{}\" TO {role};
                 GRANT CREATE, USAGE ON SCHEMA public TO {role};",
                db.name()
            ))
            .await
            .unwrap();

        let limited = db.connect_as(&role, "limited").await;
        let report = perform(&limited, FhirVersion::V4_0_0)
            .await
            .expect("D9: init must tolerate a failing CREATE EXTENSION pgcrypto");

        assert_eq!(
            report.tolerated, 1,
            "exactly the pgcrypto statement should have been tolerated"
        );
        assert!(report.executed > 290, "the rest must still have run");

        // The point of D9: gen_random_uuid() works anyway, because it is core
        // in PostgreSQL 13+, so nothing downstream depends on the extension.
        let row = limited
            .query_one(
                r#"SELECT fhirpg_create('{"resourceType":"Patient"}'::jsonb)"#,
                &[],
            )
            .await
            .expect("fhirpg_create must work without pgcrypto");
        let created: serde_json::Value = row.get(0);
        assert!(created["id"].is_string(), "{created}");

        drop(limited);
        admin
            .batch_execute(&format!(
                "REVOKE ALL ON SCHEMA public FROM {role};
                 REVOKE ALL ON DATABASE \"{}\" FROM {role};",
                db.name()
            ))
            .await
            .ok();
        drop(admin);
        db.drop().await;

        // The role outlives the database, so clean it up last.
        if let Some(maintenance) = testdb::maintenance_client().await {
            maintenance
                .batch_execute(&format!("DROP ROLE IF EXISTS {role}"))
                .await
                .ok();
        }
    }
}
