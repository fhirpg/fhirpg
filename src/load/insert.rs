//! The insert loader.
//!
//! Ports `insertLoader.Load` (`load.go:680-733`). Specified in `spec/index.md`
//! §8.1 and §8.3.
//!
//! Batched `INSERT … ON CONFLICT (id) DO NOTHING`: order-insensitive, tolerant
//! of duplicate ids, and equally fast on grouped and non-grouped input. It is
//! the default for local files, where the input's ordering is unknown.
//!
//! Resources are buffered per table and flushed as one multi-row `INSERT` each,
//! so a batch of 2,000 mixed resources costs one statement per distinct type
//! rather than 2,000 statements.

use std::collections::BTreeMap;

use serde_json::Value;
use tokio_postgres::Client;

use crate::error::{Error, Result};
use crate::load::{LoadOptions, LoadStats, PreparedResource, prepare};

/// A row waiting to be written.
struct PendingRow {
    id: Option<String>,
    resource: Value,
}

/// Buffers resources by table and writes them in batches.
pub struct InsertLoader {
    options: LoadOptions,
    /// Rows waiting to be written, keyed by quoted-safe table name.
    pending: BTreeMap<&'static str, Vec<PendingRow>>,
    /// Total rows buffered across every table.
    buffered: usize,
}

impl InsertLoader {
    /// Creates a loader with the given options.
    #[must_use]
    pub fn new(options: LoadOptions) -> Self {
        Self {
            options,
            pending: BTreeMap::new(),
            buffered: 0,
        }
    }

    /// Loads every resource an iterator yields.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if a write fails, or a preparation error when
    /// `strict` is set.
    pub async fn load<I>(&mut self, client: &Client, resources: I) -> Result<LoadStats>
    where
        I: IntoIterator<Item = Result<Value>>,
    {
        let mut stats = LoadStats::default();

        for item in resources {
            let resource = match item {
                Ok(resource) => resource,
                Err(e) => {
                    if self.options.strict {
                        return Err(e);
                    }
                    // A reader error already names its file and line.
                    eprintln!("{e}");
                    stats.malformed += 1;
                    continue;
                }
            };

            let Some(prepared) = prepare(&resource, self.options, &mut stats)? else {
                continue;
            };
            self.push(prepared, &mut stats);

            // Defect X7. fhirbase flushes on `curResource % batchSize == 0`,
            // which fires on the very first resource, and terminates on
            // `curResource == totalCount-1`, which depends on a count its own
            // multifile bundle overstates whenever a file fails to open. Flush
            // when the buffer is full, and once at the end. Nothing here reads
            // a count.
            if self.buffered >= self.options.batch_size {
                self.flush(client).await?;
            }
        }

        self.flush(client).await?;
        Ok(stats)
    }

    /// Buffers one prepared resource.
    fn push(&mut self, prepared: PreparedResource, stats: &mut LoadStats) {
        stats.record_written(&prepared.resource_type);
        self.pending.entry(prepared.table).or_default().push(PendingRow {
            id: prepared.id,
            resource: prepared.resource,
        });
        self.buffered += 1;
    }

    /// Writes everything buffered.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if a statement fails.
    async fn flush(&mut self, client: &Client) -> Result<()> {
        if self.buffered == 0 {
            return Ok(());
        }

        for (table, rows) in std::mem::take(&mut self.pending) {
            if rows.is_empty() {
                continue;
            }
            write_batch(client, table, &rows, self.options.txid).await?;
        }

        self.buffered = 0;
        Ok(())
    }
}

/// Writes one table's worth of rows as a single multi-row `INSERT`.
async fn write_batch(
    client: &Client,
    table: &str,
    rows: &[PendingRow],
    txid: i64,
) -> Result<()> {
    // `table` came from the version's schema (see `load::prepare`) and is
    // quoted regardless — quoting is what makes `Group` work at all.
    //
    // `txid` is a program-controlled integer, not input, so it is formatted in
    // rather than bound; that keeps the parameter count to two per row and well
    // clear of PostgreSQL's 65,535 limit.
    use std::fmt::Write as _;

    let mut sql = format!(r#"INSERT INTO "{table}" (id, txid, status, resource) VALUES "#);
    let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = Vec::new();
    let mut next = 1;

    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        match &row.id {
            Some(id) => {
                let _ = write!(sql, "(${next}, {txid}, 'created', ");
                params.push(id);
                next += 1;
            }
            // Decision D12: UUIDv7, not the v4 `gen_random_uuid()` fhirbase
            // uses at `load.go:704`. Time-ordered ids keep insertion into the
            // `id` primary key mostly append-only.
            None => {
                let _ = write!(sql, "(uuidv7()::text, {txid}, 'created', ");
            }
        }
        let _ = write!(sql, "${next})");
        params.push(&row.resource);
        next += 1;
    }

    // Duplicate ids keep the first occurrence, which is why fhirbase recommends
    // this mode for input that may repeat a resource.
    sql.push_str(" ON CONFLICT (id) DO NOTHING");

    client.execute(sql.as_str(), &params).await.map_err(|e| {
        Error::Db(format!(
            "cannot insert {} row(s) into \"{table}\": {}",
            rows.len(),
            describe(&e)
        ))
    })?;

    Ok(())
}

/// Renders a database error with its source chain.
///
/// `tokio_postgres::Error`'s `Display` is the bare string `"db error"`; the
/// SQLSTATE and message hang off `source()`.
fn describe(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut current = error.source();
    while let Some(cause) = current {
        parts.push(cause.to_string());
        current = cause.source();
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::FhirVersion;
    use crate::commands::init;
    use crate::testdb;
    use serde_json::json;

    async fn loaded(
        suffix: &str,
        resources: Vec<Value>,
        options: LoadOptions,
    ) -> Option<(testdb::TestDb, Client, LoadStats)> {
        let db = testdb::create(suffix).await?;
        let client = db.connect().await;
        init::perform(&client, options.version).await.unwrap();

        let mut loader = InsertLoader::new(options);
        let stats = loader
            .load(&client, resources.into_iter().map(Ok))
            .await
            .unwrap_or_else(|e| panic!("load failed: {e}"));
        Some((db, client, stats))
    }

    async fn count(client: &Client, table: &str) -> i64 {
        client
            .query_one(&format!(r#"SELECT count(*) FROM "{table}""#), &[])
            .await
            .unwrap()
            .get(0)
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn resources_land_in_their_tables() {
        let resources = vec![
            json!({"resourceType": "Patient", "id": "p1"}),
            json!({"resourceType": "Patient", "id": "p2"}),
            json!({"resourceType": "Observation", "id": "o1", "status": "final"}),
        ];
        let Some((db, client, stats)) =
            loaded("ins_basic", resources, LoadOptions::new(FhirVersion::V4_0_0)).await
        else {
            return;
        };

        assert_eq!(count(&client, "patient").await, 2);
        assert_eq!(count(&client, "observation").await, 1);
        assert_eq!(stats.written["Patient"], 2);
        assert_eq!(stats.written["Observation"], 1);
        assert_eq!(stats.total_skipped(), 0);

        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn a_group_resource_loads() {
        // Defect X2, the sharpest regression in the suite: fhirbase's insert
        // loader builds `INSERT INTO group`, a syntax error, so it cannot load
        // a FHIR Group at all — in its default mode.
        let Some((db, client, stats)) = loaded(
            "ins_group",
            vec![
                json!({"resourceType": "Group", "id": "g1", "type": "person", "actual": true}),
                json!({"resourceType": "Group", "id": "g2", "type": "person", "actual": false}),
            ],
            LoadOptions::new(FhirVersion::V4_0_0),
        )
        .await
        else {
            return;
        };

        assert_eq!(count(&client, "group").await, 2);
        assert_eq!(stats.written["Group"], 2);

        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn a_hostile_resource_type_never_reaches_the_database() {
        let Some((db, client, stats)) = loaded(
            "ins_hostile",
            vec![
                json!({"resourceType": "patient; DROP TABLE patient; --", "id": "x"}),
                json!({"resourceType": "Patient", "id": "ok"}),
            ],
            LoadOptions::new(FhirVersion::V4_0_0),
        )
        .await
        else {
            return;
        };

        // The good resource still loaded; the hostile one was tallied.
        assert_eq!(count(&client, "patient").await, 1);
        assert_eq!(stats.total_skipped(), 1);
        assert_eq!(stats.unknown_type.len(), 1);

        // And the table it tried to drop is still there.
        let exists: i64 = client
            .query_one(
                "SELECT count(*) FROM information_schema.tables WHERE table_name = 'patient'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(exists, 1);

        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn duplicate_ids_keep_the_first_occurrence() {
        // Spec §8.1. Both duplicates are in the SAME batch, which is the case
        // that decides whether `ON CONFLICT DO NOTHING` is enough on its own.
        let Some((db, client, _)) = loaded(
            "ins_dupes",
            vec![
                json!({"resourceType": "Patient", "id": "dup", "v": 1}),
                json!({"resourceType": "Patient", "id": "dup", "v": 2}),
                json!({"resourceType": "Patient", "id": "dup", "v": 3}),
            ],
            LoadOptions::new(FhirVersion::V4_0_0),
        )
        .await
        else {
            return;
        };

        assert_eq!(count(&client, "patient").await, 1);
        let kept: Value = client
            .query_one("SELECT resource FROM patient WHERE id = 'dup'", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(kept["v"], 1, "the first occurrence must win");

        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn resources_without_an_id_get_a_uuidv7() {
        // Decision D12.
        let Some((db, client, _)) = loaded(
            "ins_uuidv7",
            vec![
                json!({"resourceType": "Patient"}),
                json!({"resourceType": "Patient", "id": ""}),
            ],
            LoadOptions::new(FhirVersion::V4_0_0),
        )
        .await
        else {
            return;
        };

        let ids: Vec<String> = client
            .query("SELECT id FROM patient", &[])
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect();
        assert_eq!(ids.len(), 2);
        for id in ids {
            let uuid: uuid_shape::Uuid = id.parse().unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(uuid.version, 7, "id {id} is not a UUIDv7");
        }

        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn a_batch_boundary_does_not_lose_or_duplicate_rows() {
        // Defect X7. fhirbase flushes on `index % batchSize == 0`, which fires
        // at index 0, and stops on a count that can be wrong. Load a number of
        // resources that is deliberately not a multiple of the batch size.
        let mut options = LoadOptions::new(FhirVersion::V4_0_0);
        options.batch_size = 10;

        let resources: Vec<Value> = (0..25)
            .map(|i| json!({"resourceType": "Patient", "id": format!("p{i}")}))
            .collect();

        let Some((db, client, stats)) = loaded("ins_batch", resources, options).await else {
            return;
        };

        assert_eq!(count(&client, "patient").await, 25);
        assert_eq!(stats.written["Patient"], 25);

        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn the_stored_body_is_transformed() {
        let Some((db, client, _)) = loaded(
            "ins_transform",
            vec![json!({
                "resourceType": "Patient",
                "id": "t1",
                "deceasedBoolean": true,
                "managingOrganization": {"reference": "Organization/9"}
            })],
            LoadOptions::new(FhirVersion::V4_0_0),
        )
        .await
        else {
            return;
        };

        let stored: Value = client
            .query_one("SELECT resource FROM patient WHERE id = 't1'", &[])
            .await
            .unwrap()
            .get(0);

        assert_eq!(stored["deceased"], json!({"boolean": true}));
        assert_eq!(
            stored["managingOrganization"],
            json!({"id": "9", "resourceType": "Organization"})
        );
        assert!(stored.get("deceasedBoolean").is_none());

        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn an_empty_input_writes_nothing_and_succeeds() {
        let Some((db, client, stats)) = loaded(
            "ins_empty",
            vec![],
            LoadOptions::new(FhirVersion::V4_0_0),
        )
        .await
        else {
            return;
        };
        assert_eq!(count(&client, "patient").await, 0);
        assert_eq!(stats.total_written(), 0);
        db.drop().await;
    }

    /// A tiny UUID shape check, so the loader tests can assert the version
    /// nibble without the crate taking a `uuid` dependency it does not
    /// otherwise need.
    mod uuid_shape {
        pub struct Uuid {
            pub version: u8,
        }

        impl std::str::FromStr for Uuid {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let groups: Vec<&str> = s.split('-').collect();
                if groups.len() != 5 {
                    return Err(format!("{s} is not a UUID"));
                }
                let third = groups[2];
                let version = third
                    .chars()
                    .next()
                    .and_then(|c| c.to_digit(16))
                    .ok_or_else(|| format!("{s} has no version nibble"))?;
                Ok(Self {
                    version: u8::try_from(version).unwrap_or(0),
                })
            }
        }
    }
}
