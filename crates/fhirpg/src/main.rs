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
    /// File holding the hex signing key for the tamper-evidence MAC.
    ///
    /// Global, not just for `serve`: `verify-audit` and `chain-witness` need
    /// the same key, and without it every keyed row reports as
    /// *unverifiable* — which is correct but useless.
    ///
    /// Preferred over FHIRPG_CHAIN_KEY: an environment variable is visible
    /// in /proc, survives into crash dumps, is reported by orchestrators,
    /// and is inherited by child processes. The file must not be readable by
    /// group or other. This is the shape Kubernetes secrets and systemd
    /// credentials already produce.
    #[arg(long, global = true, value_name = "PATH")]
    chain_key_file: Option<PathBuf>,
    /// Identifier recorded with each tag, e.g. `k1`.
    #[arg(long, global = true, default_value = "k1")]
    chain_key_id: String,
    /// A retired key that verifies but never signs, as `id=path`. Repeatable.
    #[arg(long, global = true, value_name = "ID=PATH")]
    chain_key_retired: Vec<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
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

/// `--audit-mode` on the command line.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum AuditModeArg {
    Sync,
    Async,
    Off,
}

// `Serve` carries every deployment flag and dwarfs the others. Boxing it
// would fight clap's derive for no benefit: this enum is constructed once,
// from the command line, and never in a hot path.
#[allow(clippy::large_enum_variant)]
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
    /// Recompute every history hash chain and report any break (spec M3.16).
    /// Exits nonzero if the audit trail has been tampered with.
    VerifyAudit,
    /// Print the chain witness: a digest over every chain head.
    ///
    /// Record it outside the database — a file on another host, a ticket, a
    /// log you do not administer. The keyed tag stops rows being rewritten;
    /// only an external witness makes wholesale deletion visible, because a
    /// chain that no longer contains a version cannot report its absence.
    ChainWitness,
    /// Erase a resource and its entire history (GDPR Art. 17, spec M3.18).
    ///
    /// This is the one sanctioned exception to append-only history. A
    /// tombstone is left recording who erased it, when, and why. Backups and
    /// replicas are NOT touched — erasure across the estate is the
    /// deployment's job.
    Purge {
        rtype: String,
        id: String,
        /// Why the erasure was performed; recorded in the tombstone.
        #[arg(long)]
        reason: String,
        /// Required acknowledgement.
        #[arg(long)]
        allow_erasure: bool,
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
        /// The service base URL this server is reached at, e.g.
        /// `https://fhir.example.org`. Every absolute URL in a response —
        /// Bundle fullUrl, paging links, Location — is built from it. Without
        /// it the server emits URLs for the address it bound, and never
        /// trusts a request header to decide (spec A7.7).
        #[arg(long)]
        base_url: Option<String>,
        /// Honor `X-Forwarded-Proto`/`-Host` from a fronting proxy. Only set
        /// this when a proxy you control is the only way in: these headers
        /// are otherwise attacker-controlled.
        #[arg(long)]
        trust_proxy: bool,
        /// Hosts a trusted proxy may claim, repeatable. Empty means any,
        /// which is why naming them is the better habit.
        #[arg(long = "allowed-host")]
        allowed_hosts: Vec<String>,
        /// Serve without an encrypted database connection while bound to a
        /// non-loopback address. Refused by default: binding to the network
        /// and sending PHI to PostgreSQL in the clear is a decision, not an
        /// accident (spec O10.7).
        #[arg(long)]
        allow_insecure_db: bool,
        /// Header carrying the authenticated principal, set by the fronting
        /// proxy — e.g. `X-Fhirpg-Principal`. Honored only with
        /// --trust-proxy: without it any client could name itself anyone.
        /// Every write records the principal; every read logs a disclosure
        /// (spec §12).
        #[arg(long)]
        principal_header: Option<String>,
        /// Header carrying a purpose of use, recorded alongside the actor.
        #[arg(long)]
        reason_header: Option<String>,
        /// Reject requests that cannot be attributed to a principal (401).
        /// Deployments handling PHI are expected to set this.
        #[arg(long)]
        require_principal: bool,
        /// Stop recording who read what. Refused unless you also pass
        /// --allow-unaudited, because an unlogged disclosure is the failure
        /// this exists to prevent (spec PR12.6).
        #[arg(long)]
        no_audit_reads: bool,
        /// Acknowledge running without read auditing.
        #[arg(long)]
        allow_unaudited: bool,
        /// Wall-clock ceiling for one request, in seconds.
        #[arg(long, default_value_t = 60)]
        request_timeout: u64,
        /// Requests allowed in flight at once before shedding with 503.
        #[arg(long, default_value_t = 256)]
        max_concurrent: usize,
        /// Largest request body accepted, in megabytes.
        #[arg(long, default_value_t = 32)]
        max_body_mb: usize,
        /// Ceiling on `_count`, whatever a client asks for.
        #[arg(long, default_value_t = 1000)]
        max_count: i64,
        /// Ceiling on `_include`/`_revinclude` expansion for one search.
        /// Exceeding it truncates and says so in the bundle.
        #[arg(long, default_value_t = 1000)]
        max_included: usize,
        /// Database connection pool size. Overrides FHIRPG_POOL_SIZE.
        #[arg(long)]
        pool_size: Option<usize>,
        /// Minutes between chain checkpoints, logged on the
        /// `audit_checkpoint` target. 0 disables the interval; startup and
        /// post-erasure checkpoints still happen (spec M3.16c).
        #[arg(long, default_value_t = 60)]
        checkpoint_interval_mins: u64,
        /// How disclosure records reach the log. `sync` commits before
        /// responding, so nothing is disclosed unrecorded, and every read
        /// pays a round trip. `async` queues in memory and writes in
        /// batches: faster, but records queued when the process dies are
        /// lost. Either way a saturated queue refuses the read rather than
        /// dropping the record (spec PR12.6).
        #[arg(long, value_enum, default_value = "sync")]
        audit_mode: AuditModeArg,
        /// Serve /health, /ready, and /metrics on their own address, e.g.
        /// `127.0.0.1:9090`. Without it they share the FHIR port, which
        /// means anyone who can reach patient data can also read operational
        /// metrics (spec O10.9).
        #[arg(long)]
        admin_bind: Option<String>,
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

/// Build the chain key ring from the global flags, falling back to the
/// environment.
///
/// Every command that touches history needs this, not just `serve`:
/// `verify-audit` without the key reports every keyed row as *unverifiable*,
/// which is correct and useless.
fn chain_keys(cli: &Cli) -> Result<fhirpg_store::chain::KeyRing> {
    if let Some(path) = &cli.chain_key_file {
        let retired: Result<Vec<_>> = cli
            .chain_key_retired
            .iter()
            .map(|entry| {
                let (id, p) = entry
                    .split_once('=')
                    .with_context(|| format!("--chain-key-retired {entry:?} is not id=path"))?;
                Ok((id.to_string(), PathBuf::from(p)))
            })
            .collect();
        return fhirpg_store::chain::KeyRing::from_files(
            Some((cli.chain_key_id.as_str(), path.as_path())),
            &retired?,
        )
        .map_err(|e| anyhow::anyhow!(e));
    }
    if std::env::var_os("FHIRPG_CHAIN_KEY").is_some() {
        tracing::warn!(
            "chain key came from FHIRPG_CHAIN_KEY. An environment variable is visible in \
             /proc, survives into crash dumps, is reported by orchestrators, and is \
             inherited by child processes. Prefer --chain-key-file (spec M3.16b)."
        );
    }
    fhirpg_store::chain::KeyRing::from_env().map_err(|e| anyhow::anyhow!(e))
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
        ref base_url,
        trust_proxy,
        ref allowed_hosts,
        allow_insecure_db,
        ref principal_header,
        ref reason_header,
        require_principal,
        no_audit_reads,
        allow_unaudited,
        audit_mode,
        checkpoint_interval_mins,
        request_timeout,
        max_concurrent,
        max_body_mb,
        max_count,
        max_included,
        pool_size,
        ref admin_bind,
    } = cli.cmd
    {
        let opts = ServeOpts {
            bind,
            tls_cert: tls_cert.as_deref(),
            tls_key: tls_key.as_deref(),
            base_url: base_url.clone(),
            trust_proxy,
            allowed_hosts: allowed_hosts.clone(),
            allow_insecure_db,
            principal_header: principal_header.clone(),
            reason_header: reason_header.clone(),
            require_principal,
            no_audit_reads,
            allow_unaudited,
            audit_mode,
            checkpoint_interval_mins,
            limits: fhirpg_server::Limits {
                request_timeout: std::time::Duration::from_secs(request_timeout),
                max_concurrent,
                max_body: max_body_mb * 1024 * 1024,
                max_count,
                max_included,
            },
            pool_size,
            admin_bind: admin_bind.clone(),
        };
        return serve(&cli, &opts).await;
    }
    let schema = cli.fhir_version.schema();
    let map = Arc::new(load_map(&cli.assets, schema)?);
    let cfg = fhirpg_store::pg_config(cli.dsn.as_deref())?;
    let store = fhirpg_store::Store::connect(cfg, map.clone())
        .await?
        .with_chain_keys(chain_keys(&cli)?);
    // CLI writes are attributable to the operator at the keyboard: a load or
    // a delete run by hand is exactly the kind of change an audit asks about
    // later (spec M3.15).
    let audit = fhirpg_store::Audit::cli();
    match cli.cmd {
        Cmd::Init {
            upgrade,
            allow_destructive,
        } => {
            let sum = map_checksum(&cli.assets, schema)?;
            if upgrade {
                let report = store.upgrade(&sum, allow_destructive).await?;
                eprintln!(
                    "{schema}: upgraded — {} additive, {} destructive change(s), \
                     {} value(s) folded",
                    report.additive, report.destructive, report.folded
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
                    match store.put_audited(&res, None, &audit).await {
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
            if store.delete_audited(&rtype, &id, &audit).await? {
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
        Cmd::ChainWitness => {
            println!("{}", store.chain_witness().await?);
        }
        Cmd::VerifyAudit => {
            let breaks = store.verify_audit().await?;
            if breaks.is_empty() {
                // Naming the algorithms matters: a reader whose regime
                // recognises only one of them needs to know that one was
                // actually checked, not infer it from silence (M3.16a).
                let layers = match store.chain_key_id() {
                    Some(k) => format!("sha256, sha3-256, hmac-sha256 [{k}]"),
                    None => "sha256, sha3-256".to_string(),
                };
                // Name the layers that actually ran. A reader whose regime
                // recognises one of them needs to know it was checked, not
                // infer it from silence — and "unkeyed" is a materially
                // weaker claim that should never be implied by omission.
                eprintln!("{schema}: audit chains verify ({layers})");
                if store.chain_key_id().is_none() {
                    eprintln!(
                        "{schema}: note: no FHIRPG_CHAIN_KEY, so the chain is unkeyed. \
                         It detects careless modification, not an attacker with SQL write \
                         access who knows the format (spec M3.16b)."
                    );
                }
            } else {
                for b in &breaks {
                    eprintln!(
                        "{schema}: {}/{} version {} [{}]: {}",
                        b.rtype, b.id, b.version_id, b.algorithm, b.detail
                    );
                }
                bail!("{} history chain break(s) found", breaks.len());
            }
        }
        Cmd::Purge {
            rtype,
            id,
            reason,
            allow_erasure,
        } => {
            if !allow_erasure {
                bail!(
                    "refusing to erase {rtype}/{id} without --allow-erasure. \
                     This deletes the resource and its entire history, which \
                     no other command does."
                );
            }
            let audit = audit.clone().with_reason(Some(reason));
            let report = store.purge(&rtype, &id, &audit).await?;
            if report.existed {
                eprintln!(
                    "{schema}: erased {rtype}/{id} — {} version(s); a tombstone remains",
                    report.versions_erased
                );
            } else {
                eprintln!("{schema}: {rtype}/{id} not found; nothing erased");
                std::process::exit(1);
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

/// Everything `serve` needs that is not the connection config.
struct ServeOpts<'a> {
    bind: &'a str,
    tls_cert: Option<&'a Path>,
    tls_key: Option<&'a Path>,
    base_url: Option<String>,
    trust_proxy: bool,
    allowed_hosts: Vec<String>,
    allow_insecure_db: bool,
    principal_header: Option<String>,
    reason_header: Option<String>,
    require_principal: bool,
    no_audit_reads: bool,
    allow_unaudited: bool,
    audit_mode: AuditModeArg,
    checkpoint_interval_mins: u64,
    limits: fhirpg_server::Limits,
    pool_size: Option<usize>,
    admin_bind: Option<String>,
}

/// Whether a bind address is loopback — the signal that this process is
/// reachable only from its own host.
fn binds_loopback(bind: &str) -> bool {
    use std::net::ToSocketAddrs;
    match bind.to_socket_addrs() {
        Ok(mut addrs) => addrs.all(|a| a.ip().is_loopback()),
        // An unresolvable bind will fail later with a better message; treat
        // it as non-loopback so the safety check is not skipped.
        Err(_) => false,
    }
}

/// Whether to refuse startup: exposing the API to the network while the
/// database link is in the clear moves PHI across an untrusted segment
/// (spec O10.7).
///
/// Split out from `serve` so the policy is testable on its own. The wiring
/// that reads `PGSSLMODE` and binds the socket is not covered here; this
/// pins the decision, which is the part that must not drift.
fn refuse_insecure_db(bind: &str, db_encrypted: bool, allow_insecure: bool) -> bool {
    !binds_loopback(bind) && !db_encrypted && !allow_insecure
}

/// Mount every version whose map asset exists and whose schema is
/// installed, then serve.
async fn serve(cli: &Cli, opts: &ServeOpts<'_>) -> Result<()> {
    // The two halves of the trust boundary are decided together: exposing the
    // API to the network while the database link is in the clear moves PHI
    // across an untrusted segment (spec O10.7).
    let ssl = fhirpg_store::SslPolicy::from_env()?;
    if refuse_insecure_db(opts.bind, ssl.is_encrypted(), opts.allow_insecure_db) {
        bail!(
            "refusing to bind {} with an unencrypted database connection \
             (PGSSLMODE={:?}). Set PGSSLMODE=require (and PGSSLROOTCERT if \
             your server uses a private CA), or pass --allow-insecure-db to \
             accept plaintext PHI on the database link.",
            opts.bind,
            ssl
        );
    }
    if !ssl.is_encrypted() {
        tracing::warn!(
            ?ssl,
            "database connection is not encrypted; set PGSSLMODE=require for PHI"
        );
    }
    let mut versions = std::collections::BTreeMap::new();
    for schema in ["r3", "r4", "r5"] {
        let Ok(map) = load_map(&cli.assets, schema) else {
            continue;
        };
        let cfg = fhirpg_store::pg_config(cli.dsn.as_deref())?;
        let store =
            fhirpg_store::Store::connect_full(cfg, Arc::new(map), ssl, opts.pool_size).await?;
        if store.installed().await? {
            versions.insert(schema.to_string(), Arc::new(store));
        }
    }
    if versions.is_empty() {
        bail!("no installed FHIR schemas found; run `fhirpg init` first");
    }
    let mounted: Vec<String> = versions.keys().cloned().collect();
    let scheme = if opts.tls_cert.is_some() {
        "https"
    } else {
        "http"
    };
    let base = fhirpg_server::BaseUrl::bound(format!("{scheme}://{}", opts.bind))
        .configured(opts.base_url.clone())
        .trusting_proxy(opts.trust_proxy, opts.allowed_hosts.clone());
    // `--no-audit-reads` predates `--audit-mode` and means the same as
    // `--audit-mode off`; either spelling has to be acknowledged.
    let mode = if opts.no_audit_reads {
        AuditModeArg::Off
    } else {
        opts.audit_mode
    };
    if mode == AuditModeArg::Off && !opts.allow_unaudited {
        bail!(
            "audit mode 'off' drops the record of who read which patient. \
             Pass --allow-unaudited to accept that (spec PR12.6)."
        );
    }
    let audit = match mode {
        AuditModeArg::Sync => fhirpg_server::AuditMode::Sync,
        AuditModeArg::Async => fhirpg_server::AuditMode::async_default(),
        AuditModeArg::Off => fhirpg_server::AuditMode::Off,
    };
    // Which tamper-evidence layers this process will actually write. Said
    // once at startup, because "unkeyed" is a materially weaker guarantee
    // than operators tend to assume from the presence of a hash chain.
    // Applied to every mounted version, so a keyed deployment signs and
    // verifies consistently across releases.
    let ring = chain_keys(cli)?;
    if !ring.is_empty() {
        versions = versions
            .into_iter()
            .map(|(v, store)| {
                let store = Arc::into_inner(store).expect("sole owner at startup");
                (v, Arc::new(store.with_chain_keys(ring.clone())))
            })
            .collect();
    }

    // Startup checkpoint, then one per interval (spec M3.16c). A deployment
    // already shipping logs now has an external witness without standing up
    // anything new — provided those logs land where this database cannot
    // reach, which fhirpg cannot enforce and does not claim.
    for store in versions.values() {
        store.emit_checkpoint("startup").await;
    }
    if opts.checkpoint_interval_mins > 0 {
        let period = std::time::Duration::from_secs(opts.checkpoint_interval_mins * 60);
        let stores: Vec<_> = versions.values().cloned().collect();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            // The first tick fires immediately; startup already checkpointed.
            tick.tick().await;
            loop {
                tick.tick().await;
                for store in &stores {
                    store.emit_checkpoint("interval").await;
                }
            }
        });
    }

    match versions.values().next().and_then(|s| s.chain_key_id()) {
        Some(k) => tracing::info!(key_id = %k, "history chains are keyed (hmac-sha256)"),
        None => tracing::warn!(
            "no FHIRPG_CHAIN_KEY: history chains are unkeyed. They detect careless \
             modification and support an external witness, but not an attacker with \
             SQL write access who knows the pre-image format (spec M3.16b)."
        ),
    }
    match mode {
        AuditModeArg::Off => {
            tracing::warn!("read auditing is OFF; disclosures will not be recorded");
        }
        AuditModeArg::Async => {
            // Stated rather than buried: this is the mode's actual cost.
            tracing::warn!(
                "audit mode is async: disclosure records are written in batches, \
                 so records still queued if the process is killed are lost"
            );
        }
        AuditModeArg::Sync => {}
    }
    if opts.principal_header.is_some() && !opts.trust_proxy {
        bail!(
            "--principal-header without --trust-proxy would let any client assert \
             any identity, so the header is ignored. Set --trust-proxy if a proxy \
             you control is the only way in (spec PR12.2)."
        );
    }
    if opts.principal_header.is_none() {
        tracing::warn!(
            "no --principal-header: every write will be recorded as \
             'unauthenticated' (spec PR12.3)"
        );
    }
    let principal = fhirpg_server::PrincipalPolicy::new(
        opts.principal_header.clone(),
        opts.reason_header.clone(),
        opts.trust_proxy,
        opts.require_principal,
    );
    let (app, state) =
        fhirpg_server::router_and_state(versions, base, principal, audit, opts.limits);
    // Kept for the shutdown drain; `admin_router` takes ownership of a clone.
    let audit_state = state.clone();
    if let Some(addr) = &opts.admin_bind {
        let admin = fhirpg_server::admin_router(state);
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("admin bind {addr}"))?;
        eprintln!("fhirpg admin plane on http://{addr}");
        // Its own task: the admin plane must answer while the API is shedding
        // load, since that is exactly when someone is looking at it.
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, admin).await {
                tracing::error!(error = %e, "admin plane stopped");
            }
        });
    }
    let bind = opts.bind;
    if let (Some(cert), Some(key)) = (opts.tls_cert, opts.tls_key) {
        let r = serve_tls(app, bind, cert, key, &mounted).await;
        // After the listener stops, not before: records queued by requests
        // still in flight during graceful shutdown must be written too.
        audit_state.shutdown_audit().await;
        return r;
    }
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    eprintln!("fhirpg serving {} on http://{bind}", mounted.join(", "));
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    audit_state.shutdown_audit().await;
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
    // Every version, since fhir 1.2.1: the typed model rejects anything the
    // release does not define, which is a stricter check than shredding
    // alone (spec V9.2).
    match ver {
        Ver::R3 => {
            serde_json::from_value::<fhir::r3::resources::Resource>(res.clone())?;
        }
        Ver::R4 => {
            serde_json::from_value::<fhir::r4::resources::Resource>(res.clone())?;
        }
        Ver::R5 => {
            serde_json::from_value::<fhir::r5::resources::Resource>(res.clone())?;
        }
    }
    Ok(())
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

#[cfg(all(test, feature = "validate"))]
mod validate_tests {
    //! `--validate` covers every version (spec V9.2).
    //!
    //! It was R5-only until `fhir` 1.2.1: the published crate's r3/r4 features
    //! did not compile, because the `Validate` derive expanded to `crate::r5::`
    //! paths. These tests exist so that regression is caught here rather than
    //! by a user discovering `--validate` silently does less for their version.

    use super::{Ver, validate_typed};
    use serde_json::json;

    #[test]
    fn every_version_validates_a_good_resource() {
        for ver in [Ver::R3, Ver::R4, Ver::R5] {
            let patient = json!({
                "resourceType": "Patient",
                "id": "ok",
                "active": true,
                "name": [{"family": "Chalmers", "given": ["Peter"]}]
            });
            validate_typed(ver, &patient)
                .unwrap_or_else(|e| panic!("{ver:?} rejected a valid Patient: {e}"));
        }
    }

    #[test]
    fn every_version_rejects_a_wrongly_typed_element() {
        // `active` is a boolean in every release; a string is not coercible,
        // so the typed model must refuse it.
        for ver in [Ver::R3, Ver::R4, Ver::R5] {
            let bad = json!({"resourceType": "Patient", "id": "bad", "active": "yes"});
            assert!(
                validate_typed(ver, &bad).is_err(),
                "{ver:?} accepted a string where the model declares a boolean"
            );
        }
    }

    #[test]
    fn every_version_rejects_an_unknown_resource_type() {
        for ver in [Ver::R3, Ver::R4, Ver::R5] {
            let bad = json!({"resourceType": "NotAResource", "id": "bad"});
            assert!(
                validate_typed(ver, &bad).is_err(),
                "{ver:?} accepted a resourceType the release does not define"
            );
        }
    }

    /// What `--validate` does *not* do, asserted so the boundary is explicit.
    ///
    /// serde ignores unknown fields by default, so the typed model tolerates an
    /// element the release never defined. fhirpg's own shredder rejects those
    /// (plan D12), which is the check that actually catches them — `--validate`
    /// adds type and cardinality rigour on top, not unknown-element rejection.
    #[test]
    fn the_typed_model_does_not_catch_unknown_elements() {
        let bad = json!({"resourceType": "Patient", "id": "x", "notAnElement": "v"});
        assert!(
            validate_typed(Ver::R4, &bad).is_ok(),
            "if this now fails, the model gained deny_unknown_fields — good \
             news, and this test should become an assertion that it errors"
        );
    }
}

#[cfg(test)]
mod startup_guard_tests {
    use super::refuse_insecure_db;

    /// The startup refusal is a policy, and policies drift silently. Each row
    /// is a deployment someone will actually attempt.
    #[test]
    fn refuses_only_plaintext_phi_on_the_network() {
        // Public bind + plaintext database = PHI over an untrusted segment.
        assert!(refuse_insecure_db("0.0.0.0:8080", false, false));
        assert!(refuse_insecure_db("192.168.1.10:8080", false, false));
        // Encrypted database link: fine at any bind.
        assert!(!refuse_insecure_db("0.0.0.0:8080", true, false));
        // Loopback only: the database link never leaves the host.
        assert!(!refuse_insecure_db("127.0.0.1:8080", false, false));
        assert!(!refuse_insecure_db("[::1]:8080", false, false));
        // Explicitly accepted by the operator.
        assert!(!refuse_insecure_db("0.0.0.0:8080", false, true));
    }

    /// An unresolvable bind must not skip the check. Treating "I could not
    /// tell" as "not loopback" fails safe; the bind itself errors later with
    /// a better message.
    #[test]
    fn unresolvable_bind_is_not_treated_as_loopback() {
        assert!(refuse_insecure_db(
            "no-such-host.invalid:8080",
            false,
            false
        ));
    }
}
