//! fhirpg CLI: generate assets, install schemas, load/export resources, and
//! inspect how a resource shreds.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use fhirpg_map::RelMap;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Parser)]
#[command(name = "fhirpg", version, about = "FHIR in PostgreSQL, relationally")]
struct Cli {
    /// FHIR version to operate on.
    #[arg(long, global = true, default_value = "r5")]
    fhir_version: Ver,
    /// PostgreSQL DSN; defaults to the standard PG* environment variables.
    #[arg(long, global = true)]
    dsn: Option<String>,
    /// Directory holding generated map assets.
    #[arg(long, global = true, env = "FHIRPG_ASSETS", default_value = "assets")]
    assets: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Ver {
    R3,
    R4,
    R5,
}

impl Ver {
    fn schema(self) -> &'static str {
        match self {
            Ver::R3 => "r3",
            Ver::R4 => "r4",
            Ver::R5 => "r5",
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Regenerate relational-map assets from FHIR specification packages.
    Gen {
        /// Directory containing r3/r4/r5 subdirectories with
        /// fhir-definitions-json packages.
        #[arg(long)]
        spec_root: PathBuf,
    },
    /// Create the relational schema for the selected FHIR version, or
    /// upgrade an installed one to the current map assets.
    Init {
        /// Apply additive schema changes to an existing install.
        #[arg(long)]
        upgrade: bool,
        /// Permit destructive upgrade steps (dropped tables/columns).
        #[arg(long)]
        allow_destructive: bool,
    },
    /// Load resources: NDJSON, Bundle, or single-resource JSON, gzipped or
    /// plain, detected by content.
    Load {
        paths: Vec<PathBuf>,
        /// Additionally validate each resource against the typed FHIR model
        /// (requires the `validate` build feature).
        #[arg(long)]
        validate: bool,
    },
    /// Print one resource reconstructed from the database.
    Get { rtype: String, id: String },
    /// Delete one resource (history is retained).
    Delete { rtype: String, id: String },
    /// Export current resources as NDJSON to stdout.
    Export {
        /// Resource types to export; all types when omitted.
        types: Vec<String>,
    },
    /// Search one resource type: `fhirpg search Patient family=Chalmers
    /// birthdate=ge1970`. Prints matching ids, or resources with --full.
    Search {
        rtype: String,
        /// name=value pairs (FHIR search syntax, modifiers allowed).
        params: Vec<String>,
        #[arg(long, default_value_t = 50)]
        count: i64,
        #[arg(long, default_value_t = 0)]
        offset: i64,
        /// Print full resources as NDJSON instead of ids.
        #[arg(long)]
        full: bool,
    },
    /// Show the rows one resource file shreds into, without a database.
    Transform { path: PathBuf },
    /// Drop this FHIR version's schema and ALL its data (tables are dropped
    /// in chunks to respect the server's lock budget).
    Drop {
        /// Required confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Run the FHIR RESTful API server, mounting every installed version
    /// at /{r3|r4|r5}. Binds loopback by default; PHI deployments put
    /// TLS and authentication in front.
    Serve {
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
        /// PEM certificate chain for in-process TLS (requires the `tls`
        /// build feature; both --tls-cert and --tls-key must be given).
        #[arg(long, requires = "tls_key")]
        tls_cert: Option<PathBuf>,
        /// PEM private key for in-process TLS.
        #[arg(long, requires = "tls_cert")]
        tls_key: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Gen { ref spec_root } => gen_assets(spec_root, &cli.assets),
        Cmd::Transform { ref path } => transform(&cli, path),
        _ => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(run_db(cli)),
    }
}

/// The generated map assets ship inside the binary, so `cargo install
/// fhirpg` works with no asset directory; an on-disk asset (via --assets)
/// overrides the embedded copy.
fn embedded_asset(schema: &str) -> Option<&'static [u8]> {
    match schema {
        "r3" => Some(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fhirpg-relmap-r3.json.gz"
        ))),
        "r4" => Some(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fhirpg-relmap-r4.json.gz"
        ))),
        "r5" => Some(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fhirpg-relmap-r5.json.gz"
        ))),
        _ => None,
    }
}

fn asset_bytes(assets: &Path, schema: &str) -> Result<Vec<u8>> {
    let path = assets.join(format!("fhirpg-relmap-{schema}.json.gz"));
    if let Ok(bytes) = std::fs::read(&path) {
        return Ok(bytes);
    }
    embedded_asset(schema)
        .map(<[u8]>::to_vec)
        .with_context(|| format!("no asset at {} and none embedded", path.display()))
}

fn load_map(assets: &Path, schema: &str) -> Result<RelMap> {
    let bytes = asset_bytes(assets, schema)?;
    RelMap::from_gz_bytes(&bytes).context("corrupt map asset")
}

fn map_checksum(assets: &Path, schema: &str) -> Result<String> {
    let bytes = asset_bytes(assets, schema)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn gen_assets(spec_root: &Path, assets: &Path) -> Result<()> {
    std::fs::create_dir_all(assets)?;
    let mut sums = String::new();
    for schema in ["r3", "r4", "r5"] {
        let defs = spec_root.join(schema).join("fhir-definitions-json");
        if !defs.exists() {
            eprintln!("skip {schema}: {} not found", defs.display());
            continue;
        }
        let map = fhirpg_gen::generate(&defs, schema)?;
        let tables: usize = map.resources.values().map(|r| r.tables.len()).sum();
        let cols: usize = map
            .resources
            .values()
            .flat_map(|r| r.tables.iter())
            .map(|t| t.cols.len())
            .sum();
        let bytes = map.to_gz_bytes()?;
        let name = format!("fhirpg-relmap-{schema}.json.gz");
        std::fs::write(assets.join(&name), &bytes)?;
        let mut h = Sha256::new();
        h.update(&bytes);
        let sum: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        sums.push_str(&format!("{sum}  {name}\n"));
        println!(
            "{schema}: fhir {} — {} resources, {tables} tables, {cols} data columns, {} KB",
            map.fhir_version,
            map.resources.len(),
            bytes.len() / 1024
        );
    }
    std::fs::write(assets.join("CHECKSUMS.txt"), sums)?;
    Ok(())
}

fn transform(cli: &Cli, path: &Path) -> Result<()> {
    let map = load_map(&cli.assets, cli.fhir_version.schema())?;
    let v: Value = serde_json::from_slice(&read_maybe_gz(path)?)?;
    let rt = v
        .get("resourceType")
        .and_then(Value::as_str)
        .context("missing resourceType")?;
    let rm = map
        .resources
        .get(rt)
        .with_context(|| format!("unknown resource type {rt:?}"))?;
    let out = fhirpg_map::shred(rm, &v).map_err(|e| anyhow::anyhow!("{e}"))?;
    for row in &out.rows {
        let t = &rm.tables[row.table as usize];
        let cols: Vec<String> = row
            .cols
            .iter()
            .map(|(n, v)| format!("{n}={}", show(v)))
            .collect();
        println!("{}  ords={:?}  {}", t.name, row.ords, cols.join(" "));
    }
    for e in &out.ext {
        println!(
            "{}._ext  path={:?} ords={:?} ext_ord={} url={:?} leaf={:?}",
            rm.base_table().name,
            e.path,
            e.ords,
            e.ext_ord,
            e.url.as_deref().unwrap_or("-"),
            e.leaf
        );
    }
    for d in &out.deep {
        println!(
            "{}._deep  path={:?} ords={:?} leaf={:?}",
            rm.base_table().name,
            d.path,
            d.ords,
            d.leaf
        );
    }
    for (ord, _) in &out.contained {
        println!("{}._contained  ord={ord}", rm.base_table().name);
    }
    Ok(())
}

fn show(v: &fhirpg_map::SqlVal) -> String {
    use fhirpg_map::SqlVal::*;
    match v {
        Bool(b) => b.to_string(),
        Int(n) => n.to_string(),
        Num(s) | Text(s) | Ts(s) | Date(s) => format!("{s:?}"),
        Jsonb(_) => "<jsonb>".to_string(),
    }
}

async fn run_db(cli: Cli) -> Result<()> {
    if let Cmd::Serve {
        ref bind,
        ref tls_cert,
        ref tls_key,
    } = cli.cmd
    {
        return serve(&cli, bind, tls_cert.as_deref(), tls_key.as_deref()).await;
    }
    let schema = cli.fhir_version.schema();
    let map = Arc::new(load_map(&cli.assets, schema)?);
    let cfg = fhirpg_store::pg_config(cli.dsn.as_deref())?;
    let store = fhirpg_store::Store::connect(cfg, map.clone()).await?;
    match cli.cmd {
        Cmd::Init {
            upgrade,
            allow_destructive,
        } => {
            let sum = map_checksum(&cli.assets, schema)?;
            if upgrade {
                let report = store.upgrade(&sum, allow_destructive).await?;
                eprintln!(
                    "{schema}: upgraded — {} additive, {} destructive change(s)",
                    report.additive, report.destructive
                );
            } else {
                let created = store.init(&sum).await?;
                eprintln!(
                    "{schema}: {}",
                    if created {
                        "schema created"
                    } else {
                        "already installed, no-op"
                    }
                );
            }
        }
        Cmd::Load { paths, validate } => {
            if paths.is_empty() {
                bail!("no input files");
            }
            let mut ok = 0usize;
            let mut failed = 0usize;
            for path in &paths {
                for (ctx, res) in read_resources(path)? {
                    if validate && let Err(e) = validate_typed(cli.fhir_version, &res) {
                        failed += 1;
                        eprintln!("{ctx}: validate: {e}");
                        continue;
                    }
                    match store.put(&res).await {
                        Ok(_) => ok += 1,
                        Err(e) => {
                            failed += 1;
                            eprintln!("{ctx}: {e}");
                        }
                    }
                }
            }
            eprintln!("loaded {ok}, failed {failed}");
            if failed > 0 {
                std::process::exit(1);
            }
        }
        Cmd::Get { rtype, id } => match store.get(&rtype, &id).await? {
            Some(got) => println!("{}", serde_json::to_string_pretty(&got.resource)?),
            None => {
                eprintln!("not found");
                std::process::exit(1);
            }
        },
        Cmd::Delete { rtype, id } => {
            if store.delete(&rtype, &id).await? {
                eprintln!("deleted");
            } else {
                eprintln!("not found");
                std::process::exit(1);
            }
        }
        Cmd::Export { types } => {
            let types: Vec<String> = if types.is_empty() {
                map.resources.keys().cloned().collect()
            } else {
                types
            };
            let out = std::io::stdout();
            let mut lock = out.lock();
            use std::io::Write as _;
            for t in &types {
                for id in store.ids(t).await? {
                    if let Some(got) = store.get(t, &id).await? {
                        serde_json::to_writer(&mut lock, &got.resource)?;
                        lock.write_all(b"\n")?;
                    }
                }
            }
        }
        Cmd::Search {
            rtype,
            params,
            count,
            offset,
            full,
        } => {
            let pairs: Vec<(String, String)> = params
                .iter()
                .map(|p| {
                    p.split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .ok_or_else(|| anyhow::anyhow!("expected name=value, got {p:?}"))
                })
                .collect::<Result<_>>()?;
            let ids = store.search(&rtype, &pairs, count, offset).await?;
            if full {
                let out = std::io::stdout();
                let mut lock = out.lock();
                use std::io::Write as _;
                for id in &ids {
                    if let Some(got) = store.get(&rtype, id).await? {
                        serde_json::to_writer(&mut lock, &got.resource)?;
                        lock.write_all(b"\n")?;
                    }
                }
            } else {
                for id in &ids {
                    println!("{id}");
                }
            }
        }
        Cmd::Drop { yes } => {
            if !yes {
                bail!("refusing to drop schema {schema:?} without --yes");
            }
            store.drop_schema().await?;
            eprintln!("{schema}: schema dropped");
        }
        Cmd::Gen { .. } | Cmd::Transform { .. } | Cmd::Serve { .. } => unreachable!(),
    }
    Ok(())
}

/// Mount every version whose map asset exists and whose schema is
/// installed, then serve.
async fn serve(
    cli: &Cli,
    bind: &str,
    tls_cert: Option<&Path>,
    tls_key: Option<&Path>,
) -> Result<()> {
    let mut versions = std::collections::BTreeMap::new();
    for schema in ["r3", "r4", "r5"] {
        let Ok(map) = load_map(&cli.assets, schema) else {
            continue;
        };
        let cfg = fhirpg_store::pg_config(cli.dsn.as_deref())?;
        let store = fhirpg_store::Store::connect(cfg, Arc::new(map)).await?;
        if store.installed().await? {
            versions.insert(schema.to_string(), Arc::new(store));
        }
    }
    if versions.is_empty() {
        bail!("no installed FHIR schemas found; run `fhirpg init` first");
    }
    let mounted: Vec<String> = versions.keys().cloned().collect();
    let app = fhirpg_server::router(versions);
    if let (Some(cert), Some(key)) = (tls_cert, tls_key) {
        return serve_tls(app, bind, cert, key, &mounted).await;
    }
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    eprintln!("fhirpg serving {} on http://{bind}", mounted.join(", "));
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    eprintln!("fhirpg: shut down cleanly");
    Ok(())
}

#[cfg(feature = "tls")]
async fn serve_tls(
    app: axum::Router,
    bind: &str,
    cert: &Path,
    key: &Path,
    mounted: &[String],
) -> Result<()> {
    let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
        .await
        .with_context(|| format!("TLS material {} / {}", cert.display(), key.display()))?;
    let addr: std::net::SocketAddr = bind.parse().with_context(|| format!("bind {bind}"))?;
    let handle = axum_server::Handle::new();
    let shutdown_handle = handle.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
    });
    eprintln!("fhirpg serving {} on https://{bind}", mounted.join(", "));
    axum_server::bind_rustls(addr, config)
        .handle(handle)
        .serve(app.into_make_service())
        .await?;
    eprintln!("fhirpg: shut down cleanly");
    Ok(())
}

#[cfg(not(feature = "tls"))]
async fn serve_tls(
    _app: axum::Router,
    _bind: &str,
    _cert: &Path,
    _key: &Path,
    _mounted: &[String],
) -> Result<()> {
    bail!("this build lacks the `tls` feature; rebuild with --features tls")
}

/// Strict validation through the typed FHIR model (spec V9.2). R5 only:
/// the fhir crate's published r3/r4 features do not currently compile.
#[cfg(feature = "validate")]
fn validate_typed(ver: Ver, res: &Value) -> Result<()> {
    match ver {
        Ver::R5 => {
            serde_json::from_value::<fhir::r5::resources::Resource>(res.clone())?;
            Ok(())
        }
        _ => bail!("--validate currently supports r5 only"),
    }
}

#[cfg(not(feature = "validate"))]
fn validate_typed(_ver: Ver, _res: &Value) -> Result<()> {
    bail!("this build lacks the `validate` feature; rebuild with --features validate")
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

fn read_maybe_gz(path: &Path) -> Result<Vec<u8>> {
    let raw = std::fs::read(path).with_context(|| path.display().to_string())?;
    if raw.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(raw.as_slice()).read_to_end(&mut out)?;
        Ok(out)
    } else {
        Ok(raw)
    }
}

/// Read a file as resources: a Bundle's entries, one NDJSON resource per
/// line, or a single resource — detected by content.
fn read_resources(path: &Path) -> Result<Vec<(String, Value)>> {
    let bytes = read_maybe_gz(path)?;
    let name = path.display();
    if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
        if v.get("resourceType").and_then(Value::as_str) == Some("Bundle") {
            let mut out = Vec::new();
            for (i, e) in v
                .get("entry")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                if let Some(r) = e.get("resource") {
                    out.push((format!("{name} entry {i}"), r.clone()));
                }
            }
            return Ok(out);
        }
        if v.get("resourceType").is_some() {
            return Ok(vec![(name.to_string(), v)]);
        }
        bail!("{name}: JSON without resourceType");
    }
    // NDJSON: one JSON object per line.
    let text = String::from_utf8(bytes).with_context(|| format!("{name}: not UTF-8"))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value =
            serde_json::from_str(line).with_context(|| format!("{name} line {}", i + 1))?;
        out.push((format!("{name} line {}", i + 1), v));
    }
    if out.is_empty() {
        bail!("{name}: no resources found");
    }
    Ok(out)
}
