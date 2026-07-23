//! The copy loader.
//!
//! Ports `copyLoader.Load` and the `copyFromBundleSource` state machine
//! (`load.go:196-285`, `load.go:664-678`). Specified in `spec/index.md` §8.1.
//!
//! `COPY … FROM STDIN` is roughly three times faster than batched inserts on
//! **grouped** input — resources of a type adjacent, as Bulk Data exports are —
//! and slower on non-grouped input. The reason is structural: a `COPY` targets
//! one table, so a new one must start every time the resource type changes.
//! Grouped input means a handful of long `COPY` runs; input that alternates
//! types means a `COPY` per resource.
//!
//! # Text format, not binary
//!
//! Risk R2. Binary `COPY` needs each column's type OID, and `status` is the
//! `resource_status` **enum**, whose OID is assigned per database at `init`
//! time — so a binary writer would have to look it up before every load. Text
//! format takes the enum's label as-is. The cost is escaping, which is four
//! byte substitutions. Revisit only if T25's profiling justifies it.

use bytes::{Bytes, BytesMut};
use futures_util::SinkExt;
use serde_json::Value;
use tokio_postgres::{Client, CopyInSink};

use crate::error::{Error, Result};
use crate::load::{LoadOptions, LoadStats, PreparedResource, prepare};

/// How much row text to accumulate before handing it to the sink.
const CHUNK_BYTES: usize = 64 * 1024;

/// Streams resources into PostgreSQL with `COPY … FROM STDIN`.
pub struct CopyLoader {
    options: LoadOptions,
}

impl CopyLoader {
    /// Creates a loader with the given options.
    #[must_use]
    pub fn new(options: LoadOptions) -> Self {
        Self { options }
    }

    /// Loads every resource an iterator yields.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if a `COPY` fails, or a preparation error when
    /// `strict` is set.
    pub async fn load<I>(&mut self, client: &Client, resources: I) -> Result<LoadStats>
    where
        I: IntoIterator<Item = Result<Value>>,
    {
        let mut stats = LoadStats::default();
        let mut run: Option<CopyRun> = None;

        for item in resources {
            let resource = match item {
                Ok(resource) => resource,
                Err(e) => {
                    if self.options.strict {
                        if let Some(run) = run.take() {
                            run.finish().await?;
                        }
                        return Err(e);
                    }
                    eprintln!("{e}");
                    stats.malformed += 1;
                    continue;
                }
            };

            let prepared = match prepare(&resource, self.options, &mut stats) {
                Ok(Some(prepared)) => prepared,
                Ok(None) => continue,
                Err(e) => {
                    // Under `strict`. Close the open COPY before surfacing the
                    // error, so the rows already streamed are committed rather
                    // than left to an aborted statement.
                    if let Some(run) = run.take() {
                        run.finish().await?;
                    }
                    return Err(e);
                }
            };

            // A COPY targets one table, so a change of resource type ends the
            // current run and begins another (`load.go:236-243`).
            if run.as_ref().is_some_and(|r| r.table != prepared.table)
                && let Some(finished) = run.take()
            {
                finished.finish().await?;
            }

            if run.is_none() {
                run = Some(CopyRun::start(client, prepared.table).await?);
            }
            let Some(active) = run.as_mut() else {
                return Err(Error::Db("the COPY run vanished".to_owned()));
            };

            stats.record_written(&prepared.resource_type);
            active.write(&prepared, self.options.txid).await?;
        }

        if let Some(run) = run.take() {
            run.finish().await?;
        }

        Ok(stats)
    }
}

/// One `COPY … FROM STDIN` in progress, for one table.
struct CopyRun {
    table: &'static str,
    sink: std::pin::Pin<Box<CopyInSink<Bytes>>>,
    buffer: BytesMut,
}

impl CopyRun {
    /// Opens a `COPY` for a table.
    async fn start(client: &Client, table: &'static str) -> Result<Self> {
        // `table` was resolved from the version's schema by `load::prepare`,
        // and is quoted regardless — which is what lets `Group` work here as
        // well as in the insert loader (defect X2).
        let sql = format!(r#"COPY "{table}" (id, txid, status, resource) FROM STDIN"#);
        let sink = client
            .copy_in::<_, Bytes>(sql.as_str())
            .await
            .map_err(|e| Error::Db(format!("cannot start COPY into \"{table}\": {}", describe(&e))))?;

        Ok(Self {
            table,
            sink: Box::pin(sink),
            buffer: BytesMut::with_capacity(CHUNK_BYTES + 8192),
        })
    }

    /// Appends one row, flushing to the sink when the buffer is full.
    async fn write(&mut self, prepared: &PreparedResource, txid: i64) -> Result<()> {
        // Decision D12's third id-generation site. The copy loader must know
        // the id before the row is written — there is no server-side default to
        // fall back on inside COPY — so it generates one here. fhirbase uses
        // uuid.NewV4 at `load.go:269`; v7 keeps ids time-ordered, matching what
        // `fhirpg_genid` and the insert loader now produce.
        let id = match &prepared.id {
            Some(id) => id.clone(),
            None => uuid::Uuid::now_v7().to_string(),
        };

        let resource = serde_json::to_string(&prepared.resource).map_err(|e| {
            Error::Db(format!("cannot serialize a {} resource: {e}", prepared.resource_type))
        })?;

        append_escaped(&mut self.buffer, &id);
        self.buffer.extend_from_slice(b"\t");
        self.buffer.extend_from_slice(txid.to_string().as_bytes());
        // `status` is the resource_status enum; text COPY takes its label.
        self.buffer.extend_from_slice(b"\tcreated\t");
        append_escaped(&mut self.buffer, &resource);
        self.buffer.extend_from_slice(b"\n");

        if self.buffer.len() >= CHUNK_BYTES {
            self.send_buffer().await?;
        }
        Ok(())
    }

    /// Hands the accumulated text to the sink.
    async fn send_buffer(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let chunk = self.buffer.split().freeze();
        self.sink.send(chunk).await.map_err(|e| {
            Error::Db(format!(
                "cannot stream rows into \"{}\": {}",
                self.table,
                describe(&e)
            ))
        })
    }

    /// Flushes and closes the `COPY`.
    async fn finish(mut self) -> Result<()> {
        self.send_buffer().await?;
        self.sink.as_mut().finish().await.map_err(|e| {
            Error::Db(format!(
                "cannot finish COPY into \"{}\": {}",
                self.table,
                describe(&e)
            ))
        })?;
        Ok(())
    }
}

/// Appends a value in PostgreSQL's `COPY` text format.
///
/// Four characters are special and must be backslash-escaped. The backslash
/// itself is the one that matters in practice: serialized JSON is full of them
/// — every `\"` inside a string — and leaving them raw would corrupt every
/// resource carrying a quote.
fn append_escaped(out: &mut BytesMut, value: &str) {
    for byte in value.bytes() {
        match byte {
            b'\\' => out.extend_from_slice(br"\\"),
            b'\n' => out.extend_from_slice(br"\n"),
            b'\r' => out.extend_from_slice(br"\r"),
            b'\t' => out.extend_from_slice(br"\t"),
            other => out.extend_from_slice(&[other]),
        }
    }
}

/// Renders a database error with its source chain.
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

    #[test]
    fn escaping_covers_every_special_character() {
        let mut out = BytesMut::new();
        append_escaped(&mut out, "a\\b\tc\nd\re");
        assert_eq!(&out[..], br"a\\b\tc\nd\re");
    }

    #[test]
    fn escaping_leaves_ordinary_text_alone() {
        let mut out = BytesMut::new();
        append_escaped(&mut out, "Patient/123 é 日本語");
        assert_eq!(&out[..], "Patient/123 é 日本語".as_bytes());
    }

    #[test]
    fn serialized_json_escapes_survive() {
        // The case that matters: JSON's own backslashes must be doubled, or
        // every resource containing a quote is corrupted on the way in.
        let resource = serde_json::json!({"text": "he said \"hi\"", "path": "a\\b"});
        let json = serde_json::to_string(&resource).unwrap();
        let mut out = BytesMut::new();
        append_escaped(&mut out, &json);

        let escaped = String::from_utf8(out.to_vec()).unwrap();
        assert!(escaped.contains(r#"\\"hi\\""#), "{escaped}");
        // Reversing PostgreSQL's unescaping must give the original JSON back.
        let unescaped = escaped
            .replace(r"\n", "\n")
            .replace(r"\r", "\r")
            .replace(r"\t", "\t")
            .replace(r"\\", r"\");
        assert_eq!(unescaped, json);
    }
}

/// Tests that need a live PostgreSQL 18.
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::assets::FhirVersion;
    use crate::commands::init;
    use crate::load::insert::InsertLoader;
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

        let mut loader = CopyLoader::new(options);
        let stats = loader
            .load(&client, resources.into_iter().map(Ok))
            .await
            .unwrap_or_else(|e| panic!("copy load failed: {e}"));
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
    async fn grouped_input_loads() {
        let resources = vec![
            json!({"resourceType": "Patient", "id": "p1"}),
            json!({"resourceType": "Patient", "id": "p2"}),
            json!({"resourceType": "Observation", "id": "o1", "status": "final"}),
        ];
        let Some((db, client, stats)) =
            loaded("cp_grouped", resources, LoadOptions::new(FhirVersion::V4_0_0)).await
        else {
            return;
        };

        assert_eq!(count(&client, "patient").await, 2);
        assert_eq!(count(&client, "observation").await, 1);
        assert_eq!(stats.written["Patient"], 2);

        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn non_grouped_input_is_still_correct() {
        // Spec §8.1: copy mode is *slower* on alternating types, because each
        // change starts a new COPY — but it must not be wrong.
        let resources: Vec<Value> = (0..12)
            .map(|i| {
                if i % 2 == 0 {
                    json!({"resourceType": "Patient", "id": format!("p{i}")})
                } else {
                    json!({"resourceType": "Observation", "id": format!("o{i}"), "status": "final"})
                }
            })
            .collect();

        let Some((db, client, stats)) =
            loaded("cp_alternating", resources, LoadOptions::new(FhirVersion::V4_0_0)).await
        else {
            return;
        };

        assert_eq!(count(&client, "patient").await, 6);
        assert_eq!(count(&client, "observation").await, 6);
        assert_eq!(stats.total_written(), 12);

        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn a_group_resource_loads() {
        // Defect X2, in the other mode. fhirbase's copy loader happens to be
        // safe here because pgx sanitizes the identifier — but only there.
        let Some((db, client, _)) = loaded(
            "cp_group",
            vec![json!({"resourceType": "Group", "id": "g1", "type": "person"})],
            LoadOptions::new(FhirVersion::V4_0_0),
        )
        .await
        else {
            return;
        };
        assert_eq!(count(&client, "group").await, 1);
        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn a_hostile_resource_type_never_reaches_the_database() {
        let Some((db, client, stats)) = loaded(
            "cp_hostile",
            vec![
                json!({"resourceType": "patient\" ; DROP TABLE patient; --", "id": "x"}),
                json!({"resourceType": "Patient", "id": "ok"}),
            ],
            LoadOptions::new(FhirVersion::V4_0_0),
        )
        .await
        else {
            return;
        };

        assert_eq!(count(&client, "patient").await, 1);
        assert_eq!(stats.total_skipped(), 1);

        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn ids_are_generated_client_side_as_uuidv7() {
        // Decision D12's third site.
        let Some((db, client, _)) = loaded(
            "cp_uuidv7",
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
            .query("SELECT id FROM patient ORDER BY id", &[])
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect();
        assert_eq!(ids.len(), 2);
        for id in &ids {
            let nibble = id.split('-').nth(2).and_then(|g| g.chars().next());
            assert_eq!(nibble, Some('7'), "id {id} is not a UUIDv7");
        }
        // Time-ordered, so lexical order is creation order.
        assert!(ids[0] < ids[1]);

        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn hostile_text_round_trips_through_the_copy_escaping() {
        // The escaping is the part of this loader most likely to corrupt data
        // silently, so feed it every character that means something to COPY.
        let resource = json!({
            "resourceType": "Patient",
            "id": "esc-1",
            "name": [{"family": "back\\slash \"quoted\" \ttab", "given": ["line\nbreak", "日本語 🔥"]}]
        });
        let Some((db, client, _)) = loaded(
            "cp_escape",
            vec![resource.clone()],
            LoadOptions::new(FhirVersion::V4_0_0),
        )
        .await
        else {
            return;
        };

        let stored: Value = client
            .query_one("SELECT resource FROM patient WHERE id = 'esc-1'", &[])
            .await
            .unwrap()
            .get(0);

        assert_eq!(stored["name"][0]["family"], "back\\slash \"quoted\" \ttab");
        assert_eq!(stored["name"][0]["given"][0], "line\nbreak");
        assert_eq!(stored["name"][0]["given"][1], "日本語 🔥");

        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn both_modes_produce_identical_rows() {
        // Spec §8.1 requires this outright, and it is the property most likely
        // to drift as the two loaders evolve independently.
        let resources = vec![
            json!({"resourceType": "Patient", "id": "a", "deceasedBoolean": true}),
            json!({"resourceType": "Patient", "id": "b",
                   "managingOrganization": {"reference": "Organization/7"}}),
            json!({"resourceType": "Group", "id": "g", "type": "person"}),
            json!({"resourceType": "Observation", "id": "o", "status": "final",
                   "valueQuantity": {"value": 1.5}}),
        ];

        let options = LoadOptions::new(FhirVersion::V4_0_0);

        let Some(copy_db) = testdb::create("parity_copy").await else {
            return;
        };
        let copy_client = copy_db.connect().await;
        init::perform(&copy_client, options.version).await.unwrap();
        CopyLoader::new(options)
            .load(&copy_client, resources.clone().into_iter().map(Ok))
            .await
            .unwrap();

        let insert_db = testdb::create("parity_insert").await.unwrap();
        let insert_client = insert_db.connect().await;
        init::perform(&insert_client, options.version).await.unwrap();
        InsertLoader::new(options)
            .load(&insert_client, resources.into_iter().map(Ok))
            .await
            .unwrap();

        for table in ["patient", "group", "observation"] {
            let sql = format!(
                r#"SELECT id, txid, status::text, resource FROM "{table}" ORDER BY id"#
            );
            let from_copy = copy_client.query(sql.as_str(), &[]).await.unwrap();
            let from_insert = insert_client.query(sql.as_str(), &[]).await.unwrap();

            assert_eq!(
                from_copy.len(),
                from_insert.len(),
                "{table}: row counts differ"
            );
            for (c, i) in from_copy.iter().zip(&from_insert) {
                assert_eq!(c.get::<_, String>(0), i.get::<_, String>(0), "{table} id");
                assert_eq!(c.get::<_, i64>(1), i.get::<_, i64>(1), "{table} txid");
                assert_eq!(c.get::<_, String>(2), i.get::<_, String>(2), "{table} status");
                assert_eq!(
                    c.get::<_, Value>(3),
                    i.get::<_, Value>(3),
                    "{table} resource"
                );
            }
        }

        copy_db.drop().await;
        insert_db.drop().await;
    }
}
