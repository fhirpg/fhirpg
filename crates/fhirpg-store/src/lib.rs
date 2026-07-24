//! fhirpg-store: the PostgreSQL layer. Applies generated DDL, writes shredded
//! resources transactionally with history, and reads rows back for
//! reconstruction.
//!
//! Every value crosses the wire as text with explicit casts
//! (`($n::text)::numeric`), which keeps the engine's lexical-fidelity
//! guarantees (decimal scale, partial dates) intact in both directions.

pub mod search;

use std::collections::BTreeSet;
use std::sync::Arc;

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use fhirpg_map::model::{ColTy, RelMap, ResourceMap, TableKind};
use fhirpg_map::reconstruct::{InRow, ReconIn, reconstruct};
use fhirpg_map::shred::{DeepRow, ExtRow, ShredOut, SqlVal, shred};
use fhirpg_map::value::LeafVal;
use serde_json::Value;
use thiserror::Error;
use tokio_postgres::NoTls;
use tokio_postgres::types::ToSql;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("postgres: {0}")]
    Pg(#[from] tokio_postgres::Error),
    #[error("pool: {0}")]
    Pool(String),
    #[error("shred: {0}")]
    Shred(#[from] fhirpg_map::ShredError),
    /// Optimistic-concurrency failure: the caller's expected version does
    /// not match the stored one (HTTP 412 at the API layer).
    #[error("version conflict: expected {expected}, found {found}")]
    Conflict { expected: i64, found: i64 },
    #[error("{0}")]
    Other(String),
}

impl From<deadpool_postgres::PoolError> for StoreError {
    fn from(e: deadpool_postgres::PoolError) -> Self {
        StoreError::Pool(e.to_string())
    }
}

#[derive(Debug)]
pub struct PutOutcome {
    pub id: String,
    pub version_id: i64,
    pub created: bool,
}

#[derive(Debug)]
pub struct UpgradeReport {
    pub additive: usize,
    pub destructive: usize,
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, StoreError> {
    if !s.len().is_multiple_of(2) {
        return Err(StoreError::Other("bad hex asset".into()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| StoreError::Other("bad hex asset".into()))
        })
        .collect()
}

#[derive(Debug)]
pub struct SearchOutcome {
    pub ids: Vec<String>,
    pub total: Option<i64>,
}

#[derive(Debug)]
pub struct Got {
    pub resource: Value,
    pub version_id: i64,
}

/// One write inside a FHIR transaction Bundle.
#[derive(Debug)]
pub enum TxOp {
    Put {
        resource: Value,
        expected: Option<i64>,
    },
    Delete {
        rtype: String,
        id: String,
    },
}

#[derive(Debug)]
pub enum TxOutcome {
    Put(PutOutcome),
    Delete(bool),
}

#[derive(Debug, PartialEq, Eq)]
pub enum ResourceStatus {
    Active(i64),
    /// Deleted; carries the delete marker's version.
    Deleted(i64),
    Unknown,
}

#[derive(Debug)]
pub struct HistEntry {
    pub version_id: i64,
    pub last_updated: String,
    /// 'C' create, 'U' update, 'D' delete.
    pub op: char,
    pub resource: Option<Value>,
}

fn hist_entry(row: tokio_postgres::Row) -> Result<HistEntry, StoreError> {
    let op: String = row.get(2);
    let resource: Option<String> = row.get(3);
    Ok(HistEntry {
        version_id: row.get(0),
        last_updated: row.get(1),
        op: op.chars().next().unwrap_or('?'),
        resource: resource
            .map(|t| serde_json::from_str(&t).map_err(|e| StoreError::Other(e.to_string())))
            .transpose()?,
    })
}

pub struct Store {
    pool: Pool,
    map: Arc<RelMap>,
}

/// Build a tokio-postgres config from the standard PG* environment
/// variables, or parse an explicit DSN.
pub fn pg_config(dsn: Option<&str>) -> Result<tokio_postgres::Config, StoreError> {
    if let Some(dsn) = dsn {
        return dsn
            .parse::<tokio_postgres::Config>()
            .map_err(StoreError::Pg);
    }
    let mut cfg = tokio_postgres::Config::new();
    let user = std::env::var("PGUSER")
        .ok()
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "postgres".into());
    cfg.host(std::env::var("PGHOST").as_deref().unwrap_or("localhost"));
    cfg.port(
        std::env::var("PGPORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(5432),
    );
    cfg.dbname(std::env::var("PGDATABASE").as_deref().unwrap_or(&user));
    if let Ok(pw) = std::env::var("PGPASSWORD") {
        cfg.password(&pw);
    }
    cfg.user(&user);
    // Runaway statements must die server-side; overridable, never unset.
    let stmt_ms: u64 = std::env::var("FHIRPG_STATEMENT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30_000);
    cfg.options(format!("-c statement_timeout={stmt_ms}"));
    Ok(cfg)
}

impl Store {
    pub async fn connect(
        cfg: tokio_postgres::Config,
        map: Arc<RelMap>,
    ) -> Result<Self, StoreError> {
        let mgr = Manager::from_config(
            cfg,
            NoTls,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        // A bounded wait: exhaustion surfaces as 503 + Retry-After at the
        // API layer instead of queueing unboundedly (spec O10.3).
        let pool = Pool::builder(mgr)
            .max_size(16)
            .wait_timeout(Some(std::time::Duration::from_secs(2)))
            .runtime(deadpool_postgres::Runtime::Tokio1)
            .build()
            .map_err(|e| StoreError::Pool(e.to_string()))?;
        Ok(Store { pool, map })
    }

    pub fn map(&self) -> &RelMap {
        &self.map
    }

    fn rm(&self, rtype: &str) -> Result<&ResourceMap, StoreError> {
        self.map
            .resources
            .get(rtype)
            .ok_or_else(|| StoreError::Other(format!("unknown resource type {rtype:?}")))
    }

    /// Apply the generated DDL. Refuses to touch a schema installed from a
    /// different map; a schema installed from the same map is a no-op.
    pub async fn init(&self, checksum: &str) -> Result<bool, StoreError> {
        let mut client = self.pool.get().await?;
        let s = &self.map.schema;
        let existing = client
            .query_opt(
                &format!(
                    "SELECT \"value\" FROM \"{s}\".\"fhirpg_meta\" WHERE \"key\" = 'map_checksum'"
                ),
                &[],
            )
            .await;
        if let Ok(Some(row)) = existing {
            let v: String = row.get(0);
            if v == checksum {
                return Ok(false);
            }
            return Err(StoreError::Other(format!(
                "schema {s} was installed from a different map (checksum {v}); refusing"
            )));
        }
        // Creating thousands of tables in one transaction would exhaust the
        // server's lock table, so stage the install under a temporary schema
        // in chunked transactions and rename it into place atomically.
        let staging = format!("{s}__init");
        client
            .batch_execute(&format!("DROP SCHEMA IF EXISTS \"{staging}\" CASCADE"))
            .await?;
        let statements = fhirpg_map::ddl::ddl_in(&self.map, &staging);
        for chunk in statements.chunks(200) {
            let tx = client.transaction().await?;
            tx.batch_execute(&chunk.join(";\n")).await?;
            tx.commit().await?;
        }
        let asset_hex = hex_encode(
            &self
                .map
                .to_gz_bytes()
                .map_err(|e| StoreError::Other(e.to_string()))?,
        );
        client
            .execute(
                &format!(
                    "INSERT INTO \"{staging}\".\"fhirpg_meta\" (\"key\", \"value\") \
                     VALUES ('map_checksum', $1), ('fhir_version', $2), ('map_asset', $3)"
                ),
                &[&checksum, &self.map.fhir_version.as_str(), &asset_hex],
            )
            .await?;
        client
            .batch_execute(&format!("ALTER SCHEMA \"{staging}\" RENAME TO \"{s}\""))
            .await?;
        Ok(true)
    }

    /// Upgrade an installed schema to this store's map: additive changes
    /// (new tables, new columns, new indexes) apply automatically;
    /// destructive ones (dropped tables/columns/indexes) require
    /// `allow_destructive`. Column type changes always refuse — those need
    /// a manual migration.
    pub async fn upgrade(
        &self,
        checksum: &str,
        allow_destructive: bool,
    ) -> Result<UpgradeReport, StoreError> {
        let s = &self.map.schema;
        let mut client = self.pool.get().await?;
        let old_hex: String = client
            .query_opt(
                &format!(
                    "SELECT \"value\" FROM \"{s}\".\"fhirpg_meta\" WHERE \"key\" = 'map_asset'"
                ),
                &[],
            )
            .await
            .map_err(|_| StoreError::Other(format!("schema {s} is not installed")))?
            .ok_or_else(|| {
                StoreError::Other(
                    "installed schema predates upgrade support (no stored map asset)".into(),
                )
            })?
            .get(0);
        let old_bytes = hex_decode(&old_hex)?;
        let old_map = RelMap::from_gz_bytes(&old_bytes)
            .map_err(|e| StoreError::Other(format!("stored map asset unreadable: {e}")))?;

        // Diff tables and columns by name across all resources.
        use std::collections::HashMap;
        let mut adds: Vec<String> = Vec::new();
        let mut destructive: Vec<String> = Vec::new();
        let mut old_tables: HashMap<&str, &fhirpg_map::model::Table> = HashMap::new();
        for rm in old_map.resources.values() {
            for t in &rm.tables {
                old_tables.insert(t.name.as_str(), t);
            }
        }
        let mut new_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for rm in self.map.resources.values() {
            for t in &rm.tables {
                new_names.insert(t.name.as_str());
                match old_tables.get(t.name.as_str()) {
                    None => adds.push(fhirpg_map::ddl::create_table(s, rm, t)),
                    Some(old_t) => {
                        let old_cols: HashMap<&str, ColTy> =
                            old_t.cols.iter().map(|c| (c.name.as_str(), c.ty)).collect();
                        let new_col_names: std::collections::HashSet<&str> =
                            t.cols.iter().map(|c| c.name.as_str()).collect();
                        for c in &t.cols {
                            match old_cols.get(c.name.as_str()) {
                                None => adds.push(format!(
                                    "ALTER TABLE \"{s}\".\"{}\" ADD COLUMN \"{}\" {}",
                                    t.name,
                                    c.name,
                                    fhirpg_map::ddl::col_sql(c.ty)
                                )),
                                Some(old_ty) if *old_ty != c.ty => {
                                    return Err(StoreError::Other(format!(
                                        "column {}.{} changed type {:?} → {:?}; manual migration required",
                                        t.name, c.name, old_ty, c.ty
                                    )));
                                }
                                Some(_) => {}
                            }
                        }
                        for name in old_cols.keys() {
                            if !new_col_names.contains(name) {
                                destructive.push(format!(
                                    "ALTER TABLE \"{s}\".\"{}\" DROP COLUMN \"{name}\"",
                                    t.name
                                ));
                            }
                        }
                    }
                }
            }
        }
        for name in old_tables.keys() {
            if !new_names.contains(name) {
                destructive.push(format!("DROP TABLE \"{s}\".\"{name}\" CASCADE"));
            }
        }
        // Index diff by full statement text.
        let old_ix: std::collections::HashSet<String> = old_map
            .resources
            .values()
            .flat_map(|rm| fhirpg_map::ddl::search_indexes(s, rm))
            .collect();
        for rm in self.map.resources.values() {
            for stmt in fhirpg_map::ddl::search_indexes(s, rm) {
                if !old_ix.contains(&stmt) {
                    adds.push(stmt);
                }
            }
        }

        if !destructive.is_empty() && !allow_destructive {
            return Err(StoreError::Other(format!(
                "upgrade requires {} destructive change(s); rerun with --allow-destructive (first: {})",
                destructive.len(),
                destructive.first().expect("non-empty")
            )));
        }
        let all: Vec<&String> = adds.iter().chain(destructive.iter()).collect();
        for chunk in all.chunks(100) {
            let tx = client.transaction().await?;
            let joined: Vec<String> = chunk.iter().map(|x| x.to_string()).collect();
            tx.batch_execute(&joined.join(";\n")).await?;
            tx.commit().await?;
        }
        let new_hex = hex_encode(
            &self
                .map
                .to_gz_bytes()
                .map_err(|e| StoreError::Other(e.to_string()))?,
        );
        client
            .execute(
                &format!(
                    "UPDATE \"{s}\".\"fhirpg_meta\" SET \"value\" = CASE \"key\" \
                     WHEN 'map_checksum' THEN $1 WHEN 'map_asset' THEN $2 \
                     WHEN 'fhir_version' THEN $3 ELSE \"value\" END \
                     WHERE \"key\" IN ('map_checksum', 'map_asset', 'fhir_version')"
                ),
                &[&checksum, &new_hex, &self.map.fhir_version.as_str()],
            )
            .await?;
        Ok(UpgradeReport {
            additive: adds.len(),
            destructive: destructive.len(),
        })
    }

    /// Remove this version's schema entirely (tables dropped in chunks to
    /// stay inside the server's lock budget). Destructive; caller confirms.
    pub async fn drop_schema(&self) -> Result<(), StoreError> {
        let client = self.pool.get().await?;
        for schema in [
            self.map.schema.clone(),
            format!("{}__init", self.map.schema),
        ] {
            let rows = client
                .query(
                    "SELECT tablename FROM pg_tables WHERE schemaname = $1",
                    &[&schema],
                )
                .await?;
            for chunk in rows.chunks(50) {
                let stmts: Vec<String> = chunk
                    .iter()
                    .map(|r| {
                        let t: String = r.get(0);
                        format!("DROP TABLE IF EXISTS \"{schema}\".\"{t}\" CASCADE")
                    })
                    .collect();
                client.batch_execute(&stmts.join(";\n")).await?;
            }
            client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
                .await?;
        }
        Ok(())
    }

    /// Create-or-update one resource in a single transaction, appending
    /// history. The resource must carry an id.
    pub async fn put(&self, resource: &Value) -> Result<PutOutcome, StoreError> {
        self.put_if(resource, None).await
    }

    /// Like [`Store::put`], but honoring an If-Match expectation: the write
    /// only proceeds when the stored version equals `expected_version`
    /// (0 = "must not exist yet").
    pub async fn put_if(
        &self,
        resource: &Value,
        expected_version: Option<i64>,
    ) -> Result<PutOutcome, StoreError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let out = self.put_in(&tx, resource, expected_version).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Run several writes as one all-or-nothing database transaction
    /// (FHIR transaction Bundles). Outcomes are returned in op order.
    pub async fn transact(&self, ops: &[TxOp]) -> Result<Vec<TxOutcome>, StoreError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let mut outcomes = Vec::with_capacity(ops.len());
        for op in ops {
            match op {
                TxOp::Put { resource, expected } => {
                    outcomes.push(TxOutcome::Put(self.put_in(&tx, resource, *expected).await?));
                }
                TxOp::Delete { rtype, id } => {
                    outcomes.push(TxOutcome::Delete(self.delete_in(&tx, rtype, id).await?));
                }
            }
        }
        tx.commit().await?;
        Ok(outcomes)
    }

    /// One create-or-update inside a caller-managed transaction.
    async fn put_in(
        &self,
        tx: &tokio_postgres::Transaction<'_>,
        resource: &Value,
        expected_version: Option<i64>,
    ) -> Result<PutOutcome, StoreError> {
        let rtype = resource
            .get("resourceType")
            .and_then(Value::as_str)
            .ok_or_else(|| StoreError::Other("missing resourceType".into()))?
            .to_string();
        let rm = self.rm(&rtype)?;
        let out = shred(rm, resource)?;
        let id = out
            .id
            .clone()
            .ok_or_else(|| StoreError::Other("resource has no id".into()))?;
        let s = &self.map.schema;
        let base = &rm.base_table().name;
        let json = serde_json::to_string(resource).map_err(|e| StoreError::Other(e.to_string()))?;

        let old: Option<i64> = tx
            .query_opt(
                &format!(
                    "SELECT \"version_id\" FROM \"{s}\".\"{base}\" WHERE \"id\" = $1 FOR UPDATE"
                ),
                &[&id],
            )
            .await?
            .map(|r| r.get(0));
        if let Some(expected) = expected_version {
            let found = old.unwrap_or(0);
            if found != expected {
                return Err(StoreError::Conflict { expected, found });
            }
        }
        if old.is_some() {
            tx.execute(
                &format!("DELETE FROM \"{s}\".\"{base}\" WHERE \"id\" = $1"),
                &[&id],
            )
            .await?;
        }
        let hist = rm
            .find_table(TableKind::History)
            .expect("history table")
            .1
            .name
            .clone();
        // Version numbers continue past deletes, so derive from history (a
        // deleted id keeps its history rows but has no base row). The
        // history primary key backstops any create/create race on a
        // deleted id.
        let last_any: Option<i64> = tx
            .query_one(
                &format!("SELECT max(\"version_id\") FROM \"{s}\".\"{hist}\" WHERE \"id\" = $1"),
                &[&id],
            )
            .await?
            .get(0);
        let version = old.unwrap_or(0).max(last_any.unwrap_or(0)) + 1;
        insert_shredded(tx, &self.map, rm, &id, version, &out).await?;
        tx.execute(
            &format!(
                "INSERT INTO \"{s}\".\"{hist}\" (\"id\", \"version_id\", \"last_updated\", \"op\", \"resource\") \
                 VALUES ($1, $2, now(), $3, ($4::text)::jsonb)"
            ),
            &[&id, &version, &(if old.is_some() { "U" } else { "C" }), &json],
        )
        .await?;
        Ok(PutOutcome {
            id,
            version_id: version,
            created: old.is_none(),
        })
    }

    /// Read the current version, reconstructed from the relational tables.
    pub async fn get(&self, rtype: &str, id: &str) -> Result<Option<Got>, StoreError> {
        let rm = self.rm(rtype)?;
        let s = &self.map.schema;
        let client = self.pool.get().await?;

        let base = rm.base_table();
        let base_cols: String = base
            .cols
            .iter()
            .map(|c| format!(", \"{}\"::text", c.name))
            .collect();
        let row = client
            .query_opt(
                &format!(
                    "SELECT \"version_id\"{base_cols} FROM \"{s}\".\"{}\" WHERE \"id\" = $1",
                    base.name
                ),
                &[&id],
            )
            .await?;
        let Some(brow) = row else { return Ok(None) };
        let version_id: i64 = brow.get(0);

        let mut input = ReconIn {
            tables: vec![Vec::new(); rm.tables.len()],
            ..Default::default()
        };
        let mut bcols = std::collections::HashMap::new();
        for (i, c) in base.cols.iter().enumerate() {
            if let Some(v) = brow.get::<_, Option<String>>(i + 1) {
                bcols.insert(c.name.clone(), v);
            }
        }
        input.tables[0].push(InRow {
            ords: Vec::new(),
            cols: bcols,
        });

        // Pipeline all child-table reads on one connection.
        let client = &client;
        let mut futs = Vec::new();
        for (ti, t) in rm.tables.iter().enumerate() {
            if ti == 0 {
                continue;
            }
            let sql = match t.kind {
                TableKind::Elem => {
                    let cols: String = t
                        .cols
                        .iter()
                        .map(|c| format!(", \"{}\"::text", c.name))
                        .collect();
                    format!(
                        "SELECT \"ords\"::text{cols} FROM \"{s}\".\"{}\" WHERE \"rid\" = $1",
                        t.name
                    )
                }
                TableKind::Ext => format!(
                    "SELECT \"path\", \"ords\"::text, \"modifier\", \"ext_ord\", \"url\", \"leaf\", \
                     \"v_kind\", coalesce(\"v_text\", \"v_num\"::text, \"v_bool\"::text) \
                     FROM \"{s}\".\"{}\" WHERE \"rid\" = $1",
                    t.name
                ),
                TableKind::Deep => format!(
                    "SELECT \"path\", \"ords\"::text, \"leaf\", \
                     \"v_kind\", coalesce(\"v_text\", \"v_num\"::text, \"v_bool\"::text) \
                     FROM \"{s}\".\"{}\" WHERE \"rid\" = $1",
                    t.name
                ),
                TableKind::Contained => format!(
                    "SELECT \"ord\", \"resource\"::text FROM \"{s}\".\"{}\" WHERE \"rid\" = $1",
                    t.name
                ),
                TableKind::Base | TableKind::History => continue,
            };
            futs.push(async move {
                let rows = client.query(&sql, &[&id]).await?;
                Ok::<_, tokio_postgres::Error>((ti, rows))
            });
        }
        let results = futures_join_all(futs).await;
        for res in results {
            let (ti, rows) = res?;
            let t = &rm.tables[ti];
            match t.kind {
                TableKind::Elem => {
                    for r in rows {
                        let ords = parse_ords(r.get::<_, String>(0).as_str())?;
                        let mut cols = std::collections::HashMap::new();
                        for (i, c) in t.cols.iter().enumerate() {
                            if let Some(v) = r.get::<_, Option<String>>(i + 1) {
                                cols.insert(c.name.clone(), v);
                            }
                        }
                        input.tables[ti].push(InRow { ords, cols });
                    }
                }
                TableKind::Ext => {
                    for r in rows {
                        let kind: String = r.get(6);
                        let text: Option<String> = r.get(7);
                        input.ext.push(ExtRow {
                            path: r.get(0),
                            ords: parse_ords(r.get::<_, String>(1).as_str())?,
                            modifier: r.get(2),
                            ext_ord: r.get(3),
                            url: r.get(4),
                            leaf: r.get(5),
                            val: LeafVal::from_cols(&kind, text.as_deref())?,
                        });
                    }
                }
                TableKind::Deep => {
                    for r in rows {
                        let kind: String = r.get(3);
                        let text: Option<String> = r.get(4);
                        input.deep.push(DeepRow {
                            path: r.get(0),
                            ords: parse_ords(r.get::<_, String>(1).as_str())?,
                            leaf: r.get(2),
                            val: LeafVal::from_cols(&kind, text.as_deref())?,
                        });
                    }
                }
                TableKind::Contained => {
                    for r in rows {
                        let ord: i16 = r.get(0);
                        let text: String = r.get(1);
                        let v: Value = serde_json::from_str(&text)
                            .map_err(|e| StoreError::Other(e.to_string()))?;
                        input.contained.push((ord, v));
                    }
                }
                _ => unreachable!(),
            }
        }

        let resource = reconstruct(rm, &input, Some(id))?;
        Ok(Some(Got {
            resource,
            version_id,
        }))
    }

    /// Whether an id is active, deleted, or unknown — the read path's 404
    /// vs 410 distinction.
    pub async fn status(&self, rtype: &str, id: &str) -> Result<ResourceStatus, StoreError> {
        let rm = self.rm(rtype)?;
        let s = &self.map.schema;
        let base = &rm.base_table().name;
        let hist = &rm.find_table(TableKind::History).expect("history").1.name;
        let client = self.pool.get().await?;
        if let Some(row) = client
            .query_opt(
                &format!("SELECT \"version_id\" FROM \"{s}\".\"{base}\" WHERE \"id\" = $1"),
                &[&id],
            )
            .await?
        {
            return Ok(ResourceStatus::Active(row.get(0)));
        }
        let last: Option<i64> = client
            .query_one(
                &format!("SELECT max(\"version_id\") FROM \"{s}\".\"{hist}\" WHERE \"id\" = $1"),
                &[&id],
            )
            .await?
            .get(0);
        Ok(match last {
            Some(v) => ResourceStatus::Deleted(v),
            None => ResourceStatus::Unknown,
        })
    }

    /// One historical version, straight from the history archive.
    /// `resource` is None for delete markers.
    pub async fn vread(
        &self,
        rtype: &str,
        id: &str,
        version_id: i64,
    ) -> Result<Option<HistEntry>, StoreError> {
        let rm = self.rm(rtype)?;
        let s = &self.map.schema;
        let hist = &rm.find_table(TableKind::History).expect("history").1.name;
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                &format!(
                    "SELECT \"version_id\", \"last_updated\"::text, \"op\", \"resource\"::text \
                     FROM \"{s}\".\"{hist}\" WHERE \"id\" = $1 AND \"version_id\" = $2"
                ),
                &[&id, &version_id],
            )
            .await?;
        row.map(hist_entry).transpose()
    }

    /// The full history of one id, newest first.
    pub async fn history(&self, rtype: &str, id: &str) -> Result<Vec<HistEntry>, StoreError> {
        let rm = self.rm(rtype)?;
        let s = &self.map.schema;
        let hist = &rm.find_table(TableKind::History).expect("history").1.name;
        let client = self.pool.get().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT \"version_id\", \"last_updated\"::text, \"op\", \"resource\"::text \
                     FROM \"{s}\".\"{hist}\" WHERE \"id\" = $1 ORDER BY \"version_id\" DESC"
                ),
                &[&id],
            )
            .await?;
        rows.into_iter().map(hist_entry).collect()
    }

    /// Delete: removes current rows, appends a delete marker to history.
    pub async fn delete(&self, rtype: &str, id: &str) -> Result<bool, StoreError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let existed = self.delete_in(&tx, rtype, id).await?;
        tx.commit().await?;
        Ok(existed)
    }

    async fn delete_in(
        &self,
        tx: &tokio_postgres::Transaction<'_>,
        rtype: &str,
        id: &str,
    ) -> Result<bool, StoreError> {
        let rm = self.rm(rtype)?;
        let s = &self.map.schema;
        let base = &rm.base_table().name;
        let hist = rm
            .find_table(TableKind::History)
            .expect("history")
            .1
            .name
            .clone();
        let old: Option<i64> = tx
            .query_opt(
                &format!(
                    "SELECT \"version_id\" FROM \"{s}\".\"{base}\" WHERE \"id\" = $1 FOR UPDATE"
                ),
                &[&id],
            )
            .await?
            .map(|r| r.get(0));
        let Some(old) = old else {
            return Ok(false);
        };
        tx.execute(
            &format!("DELETE FROM \"{s}\".\"{base}\" WHERE \"id\" = $1"),
            &[&id],
        )
        .await?;
        let version = old + 1;
        tx.execute(
            &format!(
                "INSERT INTO \"{s}\".\"{hist}\" (\"id\", \"version_id\", \"last_updated\", \"op\", \"resource\") \
                 VALUES ($1, $2, now(), 'D', NULL)"
            ),
            &[&id, &version],
        )
        .await?;
        Ok(true)
    }

    /// Execute a search over compiled parameters. `params` are raw
    /// (name-or-name:modifier, value) pairs; returns matching ids in
    /// id order.
    pub async fn search(
        &self,
        rtype: &str,
        params: &[(String, String)],
        count: i64,
        offset: i64,
    ) -> Result<Vec<String>, StoreError> {
        Ok(self
            .search_full(rtype, params, count, offset, &[], false)
            .await?
            .ids)
    }

    /// Search with sort keys and an optional accurate total.
    pub async fn search_full(
        &self,
        rtype: &str,
        params: &[(String, String)],
        count: i64,
        offset: i64,
        sort: &[search::SortKey],
        want_total: bool,
    ) -> Result<SearchOutcome, StoreError> {
        self.search_page(rtype, params, count, offset, sort, want_total, None)
            .await
    }

    /// Search with an optional keyset cursor (`after_id`) for stable
    /// paging under the default id ordering.
    #[allow(clippy::too_many_arguments)]
    pub async fn search_page(
        &self,
        rtype: &str,
        params: &[(String, String)],
        count: i64,
        offset: i64,
        sort: &[search::SortKey],
        want_total: bool,
        after_id: Option<&str>,
    ) -> Result<SearchOutcome, StoreError> {
        let rm = self.rm(rtype)?;
        let q = search::build_search_sql(&self.map, rm, params, count, offset, sort, after_id)?;
        let client = self.pool.get().await?;
        let refs: Vec<&(dyn ToSql + Sync)> =
            q.binds.iter().map(|b| b as &(dyn ToSql + Sync)).collect();
        let rows = client.query(&q.sql, &refs).await?;
        let ids = rows.iter().map(|r| r.get(0)).collect();
        let total = if want_total {
            // The count query shares only the WHERE binds.
            Some(
                client
                    .query_one(&q.count_sql, &refs[..q.count_binds])
                    .await?
                    .get(0),
            )
        } else {
            None
        };
        Ok(SearchOutcome { ids, total })
    }

    /// Whether this version's schema is installed in the database.
    pub async fn installed(&self) -> Result<bool, StoreError> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT 1 FROM information_schema.schemata WHERE schema_name = $1",
                &[&self.map.schema],
            )
            .await?;
        Ok(row.is_some())
    }

    /// The (type, id) pairs referenced by `param` (a compiled reference
    /// search parameter) across the given resources — the _include lookup.
    pub async fn refs_of(
        &self,
        rtype: &str,
        ids: &[String],
        param: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        use fhirpg_map::model::TargetKind;
        let rm = self.rm(rtype)?;
        let Some(def) = rm.search.iter().find(|d| d.code == param) else {
            return Err(StoreError::Other(format!(
                "unknown _include parameter {param:?}"
            )));
        };
        let s = &self.map.schema;
        let client = self.pool.get().await?;
        let mut out = Vec::new();
        for t in &def.targets {
            let TargetKind::Reference { c_type, c_id, .. } = &t.kind else {
                continue;
            };
            let table = &rm.tables[t.table as usize].name;
            let (id_col, filter) = if t.table == 0 {
                ("\"id\"", "")
            } else {
                ("\"rid\"", "")
            };
            let _ = filter;
            let sql = format!(
                "SELECT DISTINCT \"{c_type}\", \"{c_id}\" FROM \"{s}\".\"{table}\"                  WHERE {id_col} = ANY($1) AND \"{c_type}\" IS NOT NULL AND \"{c_id}\" IS NOT NULL"
            );
            let rows = client.query(&sql, &[&ids]).await?;
            for r in rows {
                out.push((r.get(0), r.get(1)));
            }
        }
        if def.targets.is_empty() {
            return Err(StoreError::Other(format!(
                "search parameter {param:?} has no reference targets"
            )));
        }
        Ok(out)
    }

    /// Cheap connectivity probe for readiness checks.
    pub async fn ping(&self) -> Result<(), StoreError> {
        let client = self.pool.get().await?;
        client.query_one("SELECT 1", &[]).await?;
        Ok(())
    }

    /// All current resource ids of one type.
    pub async fn ids(&self, rtype: &str) -> Result<Vec<String>, StoreError> {
        let rm = self.rm(rtype)?;
        let s = &self.map.schema;
        let client = self.pool.get().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT \"id\" FROM \"{s}\".\"{}\" ORDER BY \"id\"",
                    rm.base_table().name
                ),
                &[],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }
}

/// Insert every shredded row inside the caller's transaction.
async fn insert_shredded(
    tx: &tokio_postgres::Transaction<'_>,
    map: &RelMap,
    rm: &ResourceMap,
    id: &str,
    version: i64,
    out: &ShredOut,
) -> Result<(), StoreError> {
    let s = &map.schema;

    // Group element rows by table.
    let mut by_table: Vec<Vec<&fhirpg_map::shred::Row>> = vec![Vec::new(); rm.tables.len()];
    for row in &out.rows {
        by_table[row.table as usize].push(row);
    }

    for (ti, rows) in by_table.iter().enumerate() {
        if rows.is_empty() {
            continue;
        }
        let t = &rm.tables[ti];
        // The union of populated columns across this table's rows.
        let mut names: BTreeSet<&str> = BTreeSet::new();
        for r in rows {
            for (n, _) in &r.cols {
                names.insert(n);
            }
        }
        let names: Vec<&str> = names.into_iter().collect();
        let types: Vec<ColTy> = names
            .iter()
            .map(|n| {
                t.cols
                    .iter()
                    .find(|c| c.name == **n)
                    .map(|c| c.ty)
                    .expect("shredded column exists in table")
            })
            .collect();

        let (sys_cols, sys_vals): (&str, usize) = match t.kind {
            TableKind::Base => ("\"id\", \"version_id\", \"last_updated\"", 2),
            TableKind::Elem => ("\"rid\", \"ords\"", 2),
            _ => unreachable!("shred rows only target base/elem tables"),
        };

        // Chunk to stay far below the 65535-parameter protocol limit.
        let per_row = sys_vals + names.len();
        let chunk_rows = (30000 / per_row.max(1)).max(1);
        for chunk in rows.chunks(chunk_rows) {
            let mut sql = format!("INSERT INTO \"{s}\".\"{}\" ({sys_cols}", t.name);
            for n in &names {
                sql.push_str(&format!(", \"{n}\""));
            }
            sql.push_str(") VALUES ");
            let mut params: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();
            let mut ords_bufs: Vec<String> = Vec::new();
            for r in chunk {
                ords_bufs.push(fmt_ords(&r.ords));
            }
            for (ri, r) in chunk.iter().enumerate() {
                if ri > 0 {
                    sql.push_str(", ");
                }
                match t.kind {
                    TableKind::Base => {
                        params.push(Box::new(id.to_string()));
                        let p1 = params.len();
                        params.push(Box::new(version));
                        sql.push_str(&format!("(${p1}, ${}, now()", p1 + 1));
                    }
                    TableKind::Elem => {
                        params.push(Box::new(id.to_string()));
                        let p1 = params.len();
                        params.push(Box::new(ords_bufs[ri].clone()));
                        sql.push_str(&format!("(${p1}, (${}::text)::smallint[]", p1 + 1));
                    }
                    _ => unreachable!(),
                }
                for (n, ty) in names.iter().zip(&types) {
                    let val = r.cols.iter().find(|(cn, _)| cn == *n).map(|(_, v)| v);
                    match val {
                        None => sql.push_str(", NULL"),
                        Some(v) => {
                            let image = match v {
                                SqlVal::Bool(b) => b.to_string(),
                                SqlVal::Int(n) => n.to_string(),
                                SqlVal::Num(x)
                                | SqlVal::Text(x)
                                | SqlVal::Ts(x)
                                | SqlVal::Date(x)
                                | SqlVal::Jsonb(x) => x.clone(),
                            };
                            params.push(Box::new(image));
                            // ($n::text)::<type> keeps the wire type text.
                            sql.push_str(&format!(
                                ", (${}::text)::{}",
                                params.len(),
                                fhirpg_map::ddl::col_sql(*ty)
                            ));
                        }
                    }
                }
                sql.push(')');
            }
            let refs: Vec<&(dyn ToSql + Sync)> = params
                .iter()
                .map(|b| b.as_ref() as &(dyn ToSql + Sync))
                .collect();
            tx.execute(&sql, &refs).await?;
        }
    }

    // Extension rows.
    if !out.ext.is_empty() {
        let t = rm
            .find_table(TableKind::Ext)
            .expect("ext table")
            .1
            .name
            .clone();
        for chunk in out.ext.chunks(3000) {
            let mut sql = format!(
                "INSERT INTO \"{s}\".\"{t}\" (\"rid\", \"path\", \"ords\", \"modifier\", \"ext_ord\", \"url\", \"leaf\", \"v_kind\", \"v_text\", \"v_num\", \"v_bool\") VALUES "
            );
            let mut params: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();
            for (ri, e) in chunk.iter().enumerate() {
                if ri > 0 {
                    sql.push_str(", ");
                }
                let (kind, text, num, boolv) = e.val.cols();
                let base = params.len();
                params.push(Box::new(id.to_string()));
                params.push(Box::new(e.path.clone()));
                params.push(Box::new(fmt_ords(&e.ords)));
                params.push(Box::new(e.modifier));
                params.push(Box::new(e.ext_ord));
                params.push(Box::new(e.url.clone()));
                params.push(Box::new(e.leaf.clone()));
                params.push(Box::new(kind.to_string()));
                params.push(Box::new(text.map(str::to_string)));
                params.push(Box::new(num.map(str::to_string)));
                params.push(Box::new(boolv));
                sql.push_str(&format!(
                    "(${}, ${}, (${}::text)::smallint[], ${}, ${}, ${}, ${}, ${}, ${}, (${}::text)::numeric, ${})",
                    base + 1, base + 2, base + 3, base + 4, base + 5, base + 6,
                    base + 7, base + 8, base + 9, base + 10, base + 11
                ));
            }
            let refs: Vec<&(dyn ToSql + Sync)> = params
                .iter()
                .map(|b| b.as_ref() as &(dyn ToSql + Sync))
                .collect();
            tx.execute(&sql, &refs).await?;
        }
    }

    // Spill rows.
    if !out.deep.is_empty() {
        let t = rm
            .find_table(TableKind::Deep)
            .expect("deep table")
            .1
            .name
            .clone();
        for chunk in out.deep.chunks(3000) {
            let mut sql = format!(
                "INSERT INTO \"{s}\".\"{t}\" (\"rid\", \"path\", \"ords\", \"leaf\", \"v_kind\", \"v_text\", \"v_num\", \"v_bool\") VALUES "
            );
            let mut params: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();
            for (ri, d) in chunk.iter().enumerate() {
                if ri > 0 {
                    sql.push_str(", ");
                }
                let (kind, text, num, boolv) = d.val.cols();
                let base = params.len();
                params.push(Box::new(id.to_string()));
                params.push(Box::new(d.path.clone()));
                params.push(Box::new(fmt_ords(&d.ords)));
                params.push(Box::new(d.leaf.clone()));
                params.push(Box::new(kind.to_string()));
                params.push(Box::new(text.map(str::to_string)));
                params.push(Box::new(num.map(str::to_string)));
                params.push(Box::new(boolv));
                sql.push_str(&format!(
                    "(${}, ${}, (${}::text)::smallint[], ${}, ${}, ${}, (${}::text)::numeric, ${})",
                    base + 1,
                    base + 2,
                    base + 3,
                    base + 4,
                    base + 5,
                    base + 6,
                    base + 7,
                    base + 8
                ));
            }
            let refs: Vec<&(dyn ToSql + Sync)> = params
                .iter()
                .map(|b| b.as_ref() as &(dyn ToSql + Sync))
                .collect();
            tx.execute(&sql, &refs).await?;
        }
    }

    // Contained resources.
    if !out.contained.is_empty() {
        let t = rm
            .find_table(TableKind::Contained)
            .expect("contained table")
            .1
            .name
            .clone();
        for (ord, v) in &out.contained {
            let json = serde_json::to_string(v).map_err(|e| StoreError::Other(e.to_string()))?;
            tx.execute(
                &format!(
                    "INSERT INTO \"{s}\".\"{t}\" (\"rid\", \"ord\", \"resource\") VALUES ($1, $2, ($3::text)::jsonb)"
                ),
                &[&id, ord, &json],
            )
            .await?;
        }
    }
    Ok(())
}

fn fmt_ords(ords: &[i16]) -> String {
    let inner: Vec<String> = ords.iter().map(|o| o.to_string()).collect();
    format!("{{{}}}", inner.join(","))
}

fn parse_ords(s: &str) -> Result<Vec<i16>, StoreError> {
    let t = s.trim_start_matches('{').trim_end_matches('}');
    if t.is_empty() {
        return Ok(Vec::new());
    }
    t.split(',')
        .map(|x| {
            x.trim()
                .parse::<i16>()
                .map_err(|_| StoreError::Other(format!("bad ords image {s:?}")))
        })
        .collect()
}

use futures_util::future::join_all as futures_join_all;
