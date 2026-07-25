//! The FHIR RESTful API over the fhirpg relational store (spec §7).
//!
//! Each installed FHIR version mounts at `/{r3|r4|r5}`. JSON only
//! (`application/fhir+json`); errors are OperationOutcomes; writes carry
//! ETags and honor If-Match.

// Helper fallibility is expressed as Result<T, Response>; the ready-made
// error Response is moved once into the handler's return, so its size is
// not a real cost.
#![allow(clippy::result_large_err)]

pub mod audit;

use std::collections::BTreeMap;
use std::sync::Arc;

use audit::AuditSink;
pub use audit::{AuditMetrics, AuditMode};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use fhirpg_store::{
    AccessRecord, Audit, PutOutcome, ResourceStatus, Store, StoreError, TxOp, TxOutcome,
};
use serde_json::{Map, Value, json};

const MAX_BODY: usize = 32 * 1024 * 1024;
const DEFAULT_COUNT: i64 = 50;
const MAX_COUNT: i64 = 1000;
/// Ceiling on `_include`/`_revinclude` expansion for one search (spec P6.7).
const MAX_INCLUDED: usize = 1000;

pub struct AppState {
    versions: BTreeMap<String, VersionState>,
    metrics: Metrics,
    base: BaseUrl,
    principal: PrincipalPolicy,
    limits: Limits,
}

/// How the server derives the absolute URLs it emits — `Bundle.entry.fullUrl`,
/// `link`, `Location` (spec A7.7).
///
/// The `Host` header is attacker-controlled. Deriving links from it lets a
/// caller decide which host a client will follow for the next page, so the
/// default here is the address the server actually bound, and a forwarded
/// host is honored only when the deployment says a proxy is in front and
/// names the hosts that proxy may claim.
#[derive(Debug, Clone)]
pub struct BaseUrl {
    /// The configured service base (`--base-url`), used verbatim when set.
    configured: Option<String>,
    /// Fallback origin: the scheme and address the server bound.
    bound: String,
    /// Whether `Host`/`X-Forwarded-*` may name the origin at all.
    trust_proxy: bool,
    /// Hosts a trusted proxy may claim. Empty means "any", which is only
    /// reachable once the deployment has already opted into trusting it.
    allowed_hosts: Vec<String>,
}

impl BaseUrl {
    /// The safe default: emit URLs for the address we are actually serving,
    /// and ignore request headers entirely.
    #[must_use]
    pub fn bound(origin: impl Into<String>) -> Self {
        Self {
            configured: None,
            bound: origin.into(),
            trust_proxy: false,
            allowed_hosts: Vec::new(),
        }
    }

    /// Pin every emitted URL to one configured service base.
    #[must_use]
    pub fn configured(mut self, base: Option<String>) -> Self {
        self.configured = base.map(|b| b.trim_end_matches('/').to_string());
        self
    }

    /// Honor `X-Forwarded-Proto`/`-Host` (falling back to `Host`) from a
    /// fronting proxy, restricted to `allowed_hosts` when that list is
    /// non-empty.
    #[must_use]
    pub fn trusting_proxy(mut self, trust: bool, allowed_hosts: Vec<String>) -> Self {
        self.trust_proxy = trust;
        self.allowed_hosts = allowed_hosts;
        self
    }

    /// The base URL for one FHIR version's endpoint.
    fn resolve(&self, version: &str, headers: &HeaderMap) -> String {
        if let Some(base) = &self.configured {
            return format!("{base}/{version}");
        }
        if self.trust_proxy
            && let Some(origin) = self.forwarded_origin(headers)
        {
            return format!("{origin}/{version}");
        }
        format!("{}/{version}", self.bound)
    }

    fn forwarded_origin(&self, headers: &HeaderMap) -> Option<String> {
        let host = headers
            .get("x-forwarded-host")
            .or_else(|| headers.get(header::HOST))
            .and_then(|h| h.to_str().ok())?;
        // A single header may carry a proxy chain; the first hop is ours.
        let host = host.split(',').next()?.trim();
        if host.is_empty() || !host_is_sane(host) {
            return None;
        }
        if !self.allowed_hosts.is_empty()
            && !self
                .allowed_hosts
                .iter()
                .any(|h| h.eq_ignore_ascii_case(host))
        {
            return None;
        }
        let proto = headers
            .get("x-forwarded-proto")
            .and_then(|h| h.to_str().ok())
            .and_then(|p| p.split(',').next())
            .map(str::trim)
            .filter(|p| *p == "http" || *p == "https")
            .unwrap_or("https");
        Some(format!("{proto}://{host}"))
    }
}

/// How an authenticated identity reaches fhirpg (spec §12).
///
/// fhirpg does not authenticate — the perimeter does (plan D13) — but that
/// cannot mean the record of who did what is nobody's job (D15). The
/// perimeter knows the identity; only the store knows which rows were
/// touched.
#[derive(Debug, Clone, Default)]
pub struct PrincipalPolicy {
    /// Header carrying the authenticated principal, e.g.
    /// `X-Fhirpg-Principal`. `None` disables attribution entirely.
    header: Option<String>,
    /// Header carrying a purpose of use.
    reason_header: Option<String>,
    /// Whether the fronting proxy is trusted to assert these headers at all.
    /// Without it a header is *ignored*, not honored: otherwise any client
    /// could name itself anyone.
    trust_proxy: bool,
    /// Reject requests that cannot be attributed (PR12.3).
    require: bool,
}

impl PrincipalPolicy {
    #[must_use]
    pub fn new(
        header: Option<String>,
        reason_header: Option<String>,
        trust_proxy: bool,
        require: bool,
    ) -> Self {
        Self {
            header,
            reason_header,
            trust_proxy,
            require,
        }
    }

    /// Build the audit envelope for one request, or `None` when the request
    /// cannot be attributed and attribution is required.
    fn audit(&self, headers: &HeaderMap, request_id: Option<String>) -> Option<Audit> {
        let claimed = if self.trust_proxy {
            self.header
                .as_deref()
                .and_then(|h| headers.get(h))
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty() && v.len() <= 256 && principal_is_sane(v))
        } else {
            None
        };
        let reason = if self.trust_proxy {
            self.reason_header
                .as_deref()
                .and_then(|h| headers.get(h))
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty() && v.len() <= 256 && principal_is_sane(v))
                .map(str::to_string)
        } else {
            None
        };
        match claimed {
            Some(actor) => Some(
                Audit::principal(
                    actor,
                    format!("header:{}", self.header.as_deref().unwrap_or("?")),
                )
                .with_request_id(request_id)
                .with_reason(reason),
            ),
            None if self.require => None,
            None => Some(Audit::unattributed().with_request_id(request_id)),
        }
    }
}

impl AppState {
    /// The audit envelope for one request, or a 401 when the deployment
    /// requires attribution and this request carries none (PR12.3).
    fn audit_for(&self, headers: &HeaderMap) -> Result<Audit, Response> {
        let request_id = headers
            .get("x-request-id")
            .and_then(|h| h.to_str().ok())
            .map(str::to_string);
        let client = headers
            .get("x-forwarded-for")
            .and_then(|h| h.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(|v| v.trim().to_string());
        self.principal
            .audit(headers, request_id)
            .map(|a| a.with_client(client))
            .ok_or_else(|| {
                oo(
                    StatusCode::UNAUTHORIZED,
                    "login",
                    "this deployment requires an authenticated principal",
                )
            })
    }

    /// Record one disclosure (PR12.5, PR12.6), or refuse the read.
    ///
    /// Returning `Err` means the disclosure could not be recorded, and the
    /// caller must not serve the data. That is the whole point of an audit
    /// requirement: a read nobody can account for afterwards is worse than a
    /// read that did not happen.
    #[allow(clippy::too_many_arguments)]
    async fn audit_read(
        &self,
        vs: &VersionState,
        audit: &Audit,
        interaction: &str,
        rtype: Option<&str>,
        id: Option<&str>,
        outcome: &str,
        result_count: Option<i64>,
    ) -> Result<(), Response> {
        let rec = AccessRecord {
            audit: audit.clone(),
            interaction: interaction.to_string(),
            rtype: rtype.map(str::to_string),
            id: id.map(str::to_string),
            version_id: None,
            outcome: outcome.to_string(),
            result_count,
        };
        vs.audit.record(&vs.store, rec).await.map_err(|_| {
            oo(
                StatusCode::SERVICE_UNAVAILABLE,
                "transient",
                "the disclosure log is unavailable; this read was not served",
            )
        })
    }
}

impl AppState {
    /// Flush every version's queued disclosure records and stop its writer.
    ///
    /// A clean shutdown that skipped this would lose exactly the records most
    /// likely to matter — the most recent ones — and lose them silently.
    pub async fn shutdown_audit(&self) {
        for vs in self.versions.values() {
            vs.audit.shutdown().await;
        }
    }
}

/// A principal is an identifier, not free text: it lands in an audit column
/// and in logs, so it must not carry control characters or separators that
/// could make one identity read as another.
fn principal_is_sane(v: &str) -> bool {
    v.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@' | '/' | '|' | '+')
    })
}

/// Reject anything that could smuggle a path, query, or credentials into an
/// origin we are about to interpolate into a URL.
fn host_is_sane(host: &str) -> bool {
    host.len() <= 255
        && !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
}

/// Upper bounds of the latency histogram buckets, in microseconds.
///
/// A cumulative total and a request count give only the mean, and a mean
/// hides exactly the tail an operator is paged about: it cannot distinguish
/// "every request took 40ms" from "99% took 5ms and 1% took 4 seconds". These
/// are the Prometheus default buckets (1ms to 10s), which is what dashboards
/// and `histogram_quantile` expect.
const LATENCY_BUCKETS_MICROS: [u64; 12] = [
    1_000, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000, 1_000_000, 2_500_000,
    5_000_000, 10_000_000,
];

/// Low-cardinality process counters, served in Prometheus text format.
#[derive(Default)]
struct Metrics {
    requests: std::sync::atomic::AtomicU64,
    responses_2xx: std::sync::atomic::AtomicU64,
    responses_4xx: std::sync::atomic::AtomicU64,
    responses_5xx: std::sync::atomic::AtomicU64,
    latency_micros: std::sync::atomic::AtomicU64,
    /// Non-cumulative per-bucket counts; rendered cumulatively, as the
    /// `le` convention requires. Requests slower than the last bound land in
    /// `overflow` and are reported as `+Inf`.
    latency_hist: [std::sync::atomic::AtomicU64; LATENCY_BUCKETS_MICROS.len()],
    latency_overflow: std::sync::atomic::AtomicU64,
}

impl Metrics {
    fn observe_latency(&self, micros: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.latency_micros.fetch_add(micros, Relaxed);
        match LATENCY_BUCKETS_MICROS.iter().position(|&b| micros <= b) {
            Some(i) => self.latency_hist[i].fetch_add(1, Relaxed),
            None => self.latency_overflow.fetch_add(1, Relaxed),
        };
    }
}

struct VersionState {
    store: Arc<Store>,
    capability: Value,
    audit: AuditSink,
}

/// Build the router for a set of installed versions
/// (base path segment → store), emitting URLs for the loopback default.
pub fn router(versions: BTreeMap<String, Arc<Store>>) -> Router {
    router_with(
        versions,
        BaseUrl::bound("http://127.0.0.1:8080"),
        PrincipalPolicy::default(),
        AuditMode::Sync,
    )
}

/// Edge resource limits (spec O10.8).
///
/// The pool already bounds database work, but a request can be expensive
/// before it ever reaches the pool, and an unbounded number of them can be in
/// flight at once. These shed load at the edge instead — 503 with
/// `Retry-After`, never an unbounded queue.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Wall-clock ceiling for one request.
    pub request_timeout: std::time::Duration,
    /// Requests allowed to be in flight at once.
    pub max_concurrent: usize,
    /// Largest request body accepted, in bytes.
    pub max_body: usize,
    /// Ceiling on `_count`, whatever the client asks for.
    pub max_included: usize,
    /// Ceiling on `_include`/`_revinclude` expansion for one search.
    pub max_count: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            request_timeout: std::time::Duration::from_secs(60),
            // Comfortably above the default pool size, so the pool's own
            // 503 remains the usual signal and this is the backstop.
            max_concurrent: 256,
            max_body: MAX_BODY,
            max_included: MAX_INCLUDED,
            max_count: MAX_COUNT,
        }
    }
}

/// Build the router with explicit URL, principal, and audit policy
/// (spec A7.7, §12).
pub fn router_with(
    versions: BTreeMap<String, Arc<Store>>,
    base: BaseUrl,
    principal: PrincipalPolicy,
    audit: AuditMode,
) -> Router {
    router_full(versions, base, principal, audit, Limits::default())
}

/// Build the router with every policy stated explicitly.
pub fn router_full(
    versions: BTreeMap<String, Arc<Store>>,
    base: BaseUrl,
    principal: PrincipalPolicy,
    audit: AuditMode,
    limits: Limits,
) -> Router {
    router_and_state(versions, base, principal, audit, limits).0
}

/// [`router_full`], also returning the shared state, so an admin plane can
/// be mounted on a second address against the same counters.
pub fn router_and_state(
    versions: BTreeMap<String, Arc<Store>>,
    base: BaseUrl,
    principal: PrincipalPolicy,
    audit: AuditMode,
    limits: Limits,
) -> (Router, Arc<AppState>) {
    let state = Arc::new(AppState {
        versions: versions
            .into_iter()
            .map(|(v, store)| {
                let capability = capability_statement(&v, &store);
                let sink = AuditSink::new(audit, store.clone());
                (
                    v,
                    VersionState {
                        store,
                        capability,
                        audit: sink,
                    },
                )
            })
            .collect(),
        metrics: Metrics::default(),
        base,
        principal,
        limits,
    });
    let app = Router::new()
        // The operational endpoints stay mounted here as well so a
        // single-port deployment keeps working; `admin_router` serves them
        // on their own address when one is configured (spec O10.9).
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics_endpoint))
        .route("/{v}/metadata", get(metadata))
        .route("/{v}", post(bundle_endpoint))
        .route(
            "/{v}/{ty}",
            get(search_type).post(create).delete(conditional_delete),
        )
        .route("/{v}/{ty}/_search", post(search_type_post))
        .route(
            "/{v}/{ty}/{id}",
            get(read).put(update).delete(delete_instance),
        )
        .route("/{v}/{ty}/{id}/_history", get(history))
        .route("/{v}/{ty}/{id}/_history/{vid}", get(vread))
        .layer(DefaultBodyLimit::max(limits.max_body))
        .layer(axum::middleware::from_fn_with_state(state.clone(), observe))
        // One stack, outermost first: errors from the fallible middleware
        // below are turned into OperationOutcomes by `shed`, so a shed or
        // timed-out request answers in FHIR rather than as a bare hangup.
        .layer(
            tower::ServiceBuilder::new()
                .layer(axum::error_handling::HandleErrorLayer::new(shed))
                .layer(tower::load_shed::LoadShedLayer::new())
                .layer(tower::limit::ConcurrencyLimitLayer::new(
                    limits.max_concurrent,
                ))
                .layer(tower::timeout::TimeoutLayer::new(limits.request_timeout)),
        )
        .with_state(state.clone());
    (app, state)
}

/// Turn a shed or timed-out request into the FHIR answer for "not now"
/// (spec O10.8): 503 with `Retry-After`, or 504 when the work itself ran
/// long. Both are OperationOutcomes, like every other error.
async fn shed(err: axum::BoxError) -> Response {
    if err.is::<tower::timeout::error::Elapsed>() {
        return oo(
            StatusCode::GATEWAY_TIMEOUT,
            "timeout",
            "the request exceeded the server's time budget",
        );
    }
    let mut resp = oo(
        StatusCode::SERVICE_UNAVAILABLE,
        "transient",
        "server at capacity; retry",
    );
    resp.headers_mut()
        .insert(header::RETRY_AFTER, "2".parse().expect("static"));
    resp
}

/// The operational plane — liveness, readiness, metrics — on its own router
/// (spec O10.9).
///
/// `/metrics` on the clinical port means anyone who can reach patient data
/// can also read request rates and pool statistics, and vice versa. Serving
/// them on a separate address lets a deployment expose one to its monitoring
/// network and the other to its clients, without a proxy rule standing
/// between them and a mistake.
///
/// It shares `AppState`, so the counters are the same ones the API updates.
pub fn admin_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics_endpoint))
        .with_state(state)
}

/// Headers every response carries, and the no-store pair that responses
/// which may contain PHI carry (spec A7.8).
///
/// A FHIR response body is patient data; letting a shared cache or a browser
/// keep a copy is a disclosure the server never sees.
fn phi_headers(resp: &mut Response) {
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, "no-store".parse().expect("static"));
    h.insert(header::PRAGMA, "no-cache".parse().expect("static"));
    h.insert("x-content-type-options", "nosniff".parse().expect("static"));
    h.insert(
        header::REFERRER_POLICY,
        "no-referrer".parse().expect("static"),
    );
}

/// Request-id header plus request counters and latency accounting. Logs
/// carry method, path, status, and the id — never resource content (PHI).
async fn observe(
    State(app): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    use std::sync::atomic::Ordering::Relaxed;
    let started = std::time::Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    // A client-supplied correlation id is echoed, but it is untrusted input:
    // cap its length and keep it to characters that cannot confuse a log
    // consumer.
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|h| h.to_str().ok())
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
        })
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    app.metrics.requests.fetch_add(1, Relaxed);
    let mut resp = next.run(req).await;
    let status = resp.status();
    let bucket = if status.is_server_error() {
        &app.metrics.responses_5xx
    } else if status.is_client_error() {
        &app.metrics.responses_4xx
    } else {
        &app.metrics.responses_2xx
    };
    bucket.fetch_add(1, Relaxed);
    app.metrics
        .observe_latency(started.elapsed().as_micros() as u64);
    if let Ok(hv) = request_id.parse() {
        resp.headers_mut().insert("x-request-id", hv);
    }
    phi_headers(&mut resp);
    tracing::info!(%method, path, status = status.as_u16(), request_id, "request");
    resp
}

async fn metrics_endpoint(State(app): State<Arc<AppState>>) -> Response {
    use std::sync::atomic::Ordering::Relaxed;
    let m = &app.metrics;
    let mut body = format!(
        "# TYPE fhirpg_requests_total counter\n\
         fhirpg_requests_total {}\n\
         # TYPE fhirpg_responses_total counter\n\
         fhirpg_responses_total{{class=\"2xx\"}} {}\n\
         fhirpg_responses_total{{class=\"4xx\"}} {}\n\
         fhirpg_responses_total{{class=\"5xx\"}} {}\n\
         # TYPE fhirpg_request_latency_micros_total counter\n\
         fhirpg_request_latency_micros_total {}\n",
        m.requests.load(Relaxed),
        m.responses_2xx.load(Relaxed),
        m.responses_4xx.load(Relaxed),
        m.responses_5xx.load(Relaxed),
        m.latency_micros.load(Relaxed),
    );
    // Histogram buckets are cumulative: each `le` counts every observation at
    // or below that bound, so p99 is answerable via `histogram_quantile`.
    body.push_str(
        "# TYPE fhirpg_request_latency_seconds histogram\n\
         # HELP fhirpg_request_latency_seconds Request latency, cumulative buckets.\n",
    );
    let mut cumulative = 0u64;
    for (i, bound) in LATENCY_BUCKETS_MICROS.iter().enumerate() {
        cumulative += m.latency_hist[i].load(Relaxed);
        body.push_str(&format!(
            "fhirpg_request_latency_seconds_bucket{{le=\"{:.3}\"}} {cumulative}\n",
            *bound as f64 / 1_000_000.0
        ));
    }
    cumulative += m.latency_overflow.load(Relaxed);
    body.push_str(&format!(
        "fhirpg_request_latency_seconds_bucket{{le=\"+Inf\"}} {cumulative}\n\
         fhirpg_request_latency_seconds_sum {:.6}\n\
         fhirpg_request_latency_seconds_count {cumulative}\n",
        m.latency_micros.load(Relaxed) as f64 / 1_000_000.0
    ));
    // The audit path, per version. `lost` above zero means disclosures
    // happened that the log does not show, and `refused` means reads were
    // turned away to keep that from being true — different incidents, so
    // they are different counters.
    body.push_str(
        "# TYPE fhirpg_audit_records_total counter\n\
         # TYPE fhirpg_audit_queue_depth gauge\n",
    );
    for (v, vs) in &app.versions {
        let a = vs.audit.metrics();
        body.push_str(&format!(
            "fhirpg_audit_records_total{{version=\"{v}\",state=\"enqueued\"}} {}\n\
             fhirpg_audit_records_total{{version=\"{v}\",state=\"written\"}} {}\n\
             fhirpg_audit_records_total{{version=\"{v}\",state=\"refused\"}} {}\n\
             fhirpg_audit_records_total{{version=\"{v}\",state=\"lost\"}} {}\n\
             fhirpg_audit_queue_depth{{version=\"{v}\"}} {}\n",
            a.enqueued.load(Relaxed),
            a.written.load(Relaxed),
            a.refused.load(Relaxed),
            a.lost.load(Relaxed),
            a.depth(),
        ));
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}

// ---------- helpers ----------

fn fhir_json(status: StatusCode, v: &Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/fhir+json")],
        v.to_string(),
    )
        .into_response()
}

fn oo(status: StatusCode, code: &str, diagnostics: &str) -> Response {
    let sev = if status.is_server_error() {
        "fatal"
    } else {
        "error"
    };
    fhir_json(
        status,
        &json!({
            "resourceType": "OperationOutcome",
            "issue": [{"severity": sev, "code": code, "diagnostics": diagnostics}]
        }),
    )
}

fn err_response(e: StoreError) -> Response {
    match e {
        StoreError::Conflict { expected, found } => oo(
            StatusCode::PRECONDITION_FAILED,
            "conflict",
            &format!("version conflict: expected {expected}, found {found}"),
        ),
        // Shred errors name the element path and the rule broken, which is
        // what a client needs and all it gets: the submitted value stays out
        // of the response and out of the log (spec A7.11).
        StoreError::Shred(e) => oo(StatusCode::BAD_REQUEST, "invalid", &e.to_string()),
        // Client-safe: describes the caller's own request back to them.
        StoreError::Unsupported(msg) => oo(StatusCode::BAD_REQUEST, "not-supported", &msg),
        StoreError::Other(msg) => {
            // `Other` carries free-form internal text — a query, a column
            // name, sometimes a value. It is diagnostics for us, not for the
            // caller.
            let incident = uuid::Uuid::new_v4();
            tracing::warn!(%incident, detail = %msg, "request rejected");
            oo(
                StatusCode::BAD_REQUEST,
                "processing",
                &format!("request rejected (incident {incident})"),
            )
        }
        StoreError::Pool(_) => {
            tracing::warn!(error = %e, "pool exhausted");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [
                    (header::RETRY_AFTER, "2"),
                    (header::CONTENT_TYPE, "application/fhir+json"),
                ],
                json!({
                    "resourceType": "OperationOutcome",
                    "issue": [{"severity": "error", "code": "transient",
                               "diagnostics": "server busy; retry"}]
                })
                .to_string(),
            )
                .into_response()
        }
        StoreError::Pg(_) => {
            tracing::error!(error = %e, "internal error");
            oo(
                StatusCode::INTERNAL_SERVER_ERROR,
                "exception",
                "internal error",
            )
        }
    }
}

fn etag(vid: i64) -> String {
    format!("W/\"{vid}\"")
}

/// The version inside a weak ETag (`W/"3"`, `"3"`, or a bare `3`).
fn parse_etag_version(s: &str) -> Option<i64> {
    s.trim()
        .trim_start_matches("W/")
        .trim_matches('"')
        .parse::<i64>()
        .ok()
}

fn parse_if_match(headers: &HeaderMap) -> Result<Option<i64>, Response> {
    let Some(h) = headers.get(header::IF_MATCH) else {
        return Ok(None);
    };
    let s = h.to_str().unwrap_or_default();
    parse_etag_version(s).map(Some).ok_or_else(|| {
        oo(
            StatusCode::BAD_REQUEST,
            "invalid",
            &format!("unparseable If-Match {s:?}"),
        )
    })
}

/// Response copy of a resource with server-known metadata injected.
fn with_meta(mut resource: Value, vid: i64) -> Value {
    if let Some(obj) = resource.as_object_mut() {
        let meta = obj
            .entry("meta".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(m) = meta.as_object_mut() {
            m.insert("versionId".to_string(), Value::String(vid.to_string()));
        }
    }
    resource
}

fn parse_body(body: &Bytes) -> Result<Value, Response> {
    serde_json::from_slice(body).map_err(|e| {
        oo(
            StatusCode::BAD_REQUEST,
            "structure",
            &format!("invalid JSON: {e}"),
        )
    })
}

/// The FHIR `id` production: `[A-Za-z0-9\-\.]{1,64}` (spec R4.6).
///
/// Ids arrive from the URL path and from request bodies, and flow into
/// `text` columns and into the URLs the server emits. Anything outside this
/// production is not a FHIR id, and storing it would put a value in the
/// database that no conformant client can address.
fn valid_fhir_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
}

fn check_id(id: &str) -> Result<(), Response> {
    if valid_fhir_id(id) {
        return Ok(());
    }
    Err(oo(
        StatusCode::BAD_REQUEST,
        "value",
        "resource id must match [A-Za-z0-9-.]{1,64}",
    ))
}

impl AppState {
    fn version(&self, v: &str) -> Result<&VersionState, Response> {
        self.versions.get(v).ok_or_else(|| {
            oo(
                StatusCode::NOT_FOUND,
                "not-found",
                &format!("unknown FHIR base {v:?}"),
            )
        })
    }

    fn typed(&self, v: &str, ty: &str) -> Result<&VersionState, Response> {
        let vs = self.version(v)?;
        if !vs.store.map().resources.contains_key(ty) {
            return Err(oo(
                StatusCode::NOT_FOUND,
                "not-supported",
                &format!("unknown resource type {ty:?}"),
            ));
        }
        Ok(vs)
    }
}

// ---------- handlers ----------

async fn health() -> &'static str {
    "ok"
}

async fn ready(State(app): State<Arc<AppState>>) -> Response {
    for vs in app.versions.values() {
        if let Err(e) = vs.store.ping().await {
            return oo(
                StatusCode::SERVICE_UNAVAILABLE,
                "transient",
                &format!("database not ready: {e}"),
            );
        }
    }
    "ok".into_response()
}

async fn metadata(State(app): State<Arc<AppState>>, Path(v): Path<String>) -> Response {
    match app.version(&v) {
        Ok(vs) => fhir_json(StatusCode::OK, &vs.capability),
        Err(r) => r,
    }
}

async fn read(
    State(app): State<Arc<AppState>>,
    Path((v, ty, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let vs = match app.typed(&v, &ty) {
        Ok(vs) => vs,
        Err(r) => return r,
    };
    if let Err(r) = check_id(&id) {
        return r;
    }
    let audit = match app.audit_for(&headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let result = vs.store.get(&ty, &id).await;
    let outcome = match &result {
        Ok(Some(_)) => "ok",
        Ok(None) => "not-found",
        Err(_) => "error",
    };
    if let Err(r) = app
        .audit_read(vs, &audit, "read", Some(&ty), Some(&id), outcome, None)
        .await
    {
        return r;
    }
    match result {
        Ok(Some(got)) => {
            let body = with_meta(got.resource, got.version_id);
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "application/fhir+json".to_string()),
                    (header::ETAG, etag(got.version_id)),
                ],
                body.to_string(),
            )
                .into_response()
        }
        Ok(None) => match vs.store.status(&ty, &id).await {
            Ok(ResourceStatus::Deleted(_)) => oo(
                StatusCode::GONE,
                "deleted",
                &format!("{ty}/{id} is deleted"),
            ),
            _ => oo(
                StatusCode::NOT_FOUND,
                "not-found",
                &format!("{ty}/{id} not found"),
            ),
        },
        Err(e) => err_response(e),
    }
}

async fn vread(
    State(app): State<Arc<AppState>>,
    Path((v, ty, id, vid)): Path<(String, String, String, i64)>,
    headers: HeaderMap,
) -> Response {
    let vs = match app.typed(&v, &ty) {
        Ok(vs) => vs,
        Err(r) => return r,
    };
    if let Err(r) = check_id(&id) {
        return r;
    }
    let audit = match app.audit_for(&headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    if let Err(r) = app
        .audit_read(vs, &audit, "vread", Some(&ty), Some(&id), "ok", None)
        .await
    {
        return r;
    }
    match vs.store.vread(&ty, &id, vid).await {
        Ok(Some(entry)) => match entry.resource {
            Some(r) => fhir_json(StatusCode::OK, &with_meta(r, entry.version_id)),
            None => oo(
                StatusCode::GONE,
                "deleted",
                &format!("{ty}/{id} version {vid} is a delete marker"),
            ),
        },
        Ok(None) => oo(
            StatusCode::NOT_FOUND,
            "not-found",
            &format!("{ty}/{id} has no version {vid}"),
        ),
        Err(e) => err_response(e),
    }
}

async fn history(
    State(app): State<Arc<AppState>>,
    Path((v, ty, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let vs = match app.typed(&v, &ty) {
        Ok(vs) => vs,
        Err(r) => return r,
    };
    if let Err(r) = check_id(&id) {
        return r;
    }
    let audit = match app.audit_for(&headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    if let Err(r) = app
        .audit_read(vs, &audit, "history", Some(&ty), Some(&id), "ok", None)
        .await
    {
        return r;
    }
    match vs.store.history(&ty, &id).await {
        Ok(entries) if entries.is_empty() => oo(
            StatusCode::NOT_FOUND,
            "not-found",
            &format!("{ty}/{id} has no history"),
        ),
        Ok(entries) => {
            let items: Vec<Value> = entries
                .into_iter()
                .map(|h| {
                    let method = match h.op {
                        'C' => "POST",
                        'D' => "DELETE",
                        _ => "PUT",
                    };
                    let mut e = json!({
                        "request": {"method": method, "url": format!("{ty}/{id}")},
                        "response": {
                            "status": if h.op == 'D' { "204" } else { "200" },
                            "etag": etag(h.version_id),
                            "lastModified": h.last_updated,
                        }
                    });
                    if let Some(r) = h.resource {
                        e["resource"] = with_meta(r, h.version_id);
                    }
                    e
                })
                .collect();
            fhir_json(
                StatusCode::OK,
                &json!({
                    "resourceType": "Bundle",
                    "type": "history",
                    "entry": items
                }),
            )
        }
        Err(e) => err_response(e),
    }
}

async fn create(
    State(app): State<Arc<AppState>>,
    Path((v, ty)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let vs = match app.typed(&v, &ty) {
        Ok(vs) => vs,
        Err(r) => return r,
    };
    let audit = match app.audit_for(&headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let mut resource = match parse_body(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    if resource.get("resourceType").and_then(Value::as_str) != Some(ty.as_str()) {
        return oo(
            StatusCode::BAD_REQUEST,
            "invalid",
            "body resourceType does not match the URL",
        );
    }
    // The server assigns ids on create; a client-sent id is ignored.
    let id = uuid::Uuid::new_v4().to_string();
    resource["id"] = Value::String(id.clone());

    // Conditional create: If-None-Exist carries search criteria; 0 matches
    // creates, 1 match returns it unchanged, several is an error. Match and
    // write happen in one locked transaction, so two identical concurrent
    // conditional creates cannot both create (spec A7.10).
    if let Some(h) = headers.get("if-none-exist") {
        let q = h.to_str().unwrap_or_default();
        let criteria: Vec<(String, String)> =
            match serde_urlencoded::from_str::<Vec<(String, String)>>(q) {
                Ok(c) if !c.is_empty() => c,
                _ => {
                    return oo(
                        StatusCode::BAD_REQUEST,
                        "invalid",
                        "unparseable If-None-Exist criteria",
                    );
                }
            };
        return match vs
            .store
            .conditional_create_audited(&ty, &criteria, &resource, &audit)
            .await
        {
            Ok(fhirpg_store::CondCreate::Created(out)) => {
                created_response(&v, &ty, &out, with_meta(resource, out.version_id))
            }
            Ok(fhirpg_store::CondCreate::Existing(existing)) => {
                match vs.store.get(&ty, &existing).await {
                    Ok(Some(got)) => (
                        StatusCode::OK,
                        [
                            (header::CONTENT_TYPE, "application/fhir+json".to_string()),
                            (header::ETAG, etag(got.version_id)),
                        ],
                        with_meta(got.resource, got.version_id).to_string(),
                    )
                        .into_response(),
                    Ok(None) => oo(StatusCode::NOT_FOUND, "not-found", "match vanished"),
                    Err(e) => err_response(e),
                }
            }
            Ok(fhirpg_store::CondCreate::Multiple) => oo(
                StatusCode::PRECONDITION_FAILED,
                "multiple-matches",
                "If-None-Exist criteria match more than one resource",
            ),
            Err(e) => err_response(e),
        };
    }
    match vs.store.put_audited(&resource, Some(0), &audit).await {
        Ok(out) => created_response(&v, &ty, &out, with_meta(resource, out.version_id)),
        Err(e) => err_response(e),
    }
}

fn created_response(v: &str, ty: &str, out: &PutOutcome, body: Value) -> Response {
    (
        StatusCode::CREATED,
        [
            (header::CONTENT_TYPE, "application/fhir+json".to_string()),
            (header::ETAG, etag(out.version_id)),
            (
                header::LOCATION,
                format!("/{v}/{ty}/{}/_history/{}", out.id, out.version_id),
            ),
        ],
        body.to_string(),
    )
        .into_response()
}

async fn update(
    State(app): State<Arc<AppState>>,
    Path((v, ty, id)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let vs = match app.typed(&v, &ty) {
        Ok(vs) => vs,
        Err(r) => return r,
    };
    if let Err(r) = check_id(&id) {
        return r;
    }
    let expected = match parse_if_match(&headers) {
        Ok(e) => e,
        Err(r) => return r,
    };
    let audit = match app.audit_for(&headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let mut resource = match parse_body(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    if resource.get("resourceType").and_then(Value::as_str) != Some(ty.as_str()) {
        return oo(
            StatusCode::BAD_REQUEST,
            "invalid",
            "body resourceType does not match the URL",
        );
    }
    match resource.get("id").and_then(Value::as_str) {
        None => {
            resource["id"] = Value::String(id.clone());
        }
        Some(bid) if bid == id => {}
        Some(bid) => {
            return oo(
                StatusCode::BAD_REQUEST,
                "invalid",
                &format!("body id {bid:?} does not match URL id {id:?}"),
            );
        }
    }
    match vs.store.put_audited(&resource, expected, &audit).await {
        Ok(out) if out.created => {
            created_response(&v, &ty, &out, with_meta(resource, out.version_id))
        }
        Ok(out) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/fhir+json".to_string()),
                (header::ETAG, etag(out.version_id)),
            ],
            with_meta(resource, out.version_id).to_string(),
        )
            .into_response(),
        Err(e) => err_response(e),
    }
}

/// DELETE {ty}?criteria — deletes a single match; several matches is a
/// client error, zero is a no-op.
async fn conditional_delete(
    State(app): State<Arc<AppState>>,
    Path((v, ty)): Path<(String, String)>,
    Query(params): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    let vs = match app.typed(&v, &ty) {
        Ok(vs) => vs,
        Err(r) => return r,
    };
    if params.is_empty() {
        return oo(
            StatusCode::BAD_REQUEST,
            "invalid",
            "conditional delete requires search criteria",
        );
    }
    let audit = match app.audit_for(&headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    match vs
        .store
        .conditional_delete_audited(&ty, &params, &audit)
        .await
    {
        // FHIR conditional delete is idempotent: no match is not an error.
        Ok(fhirpg_store::CondDelete::Deleted | fhirpg_store::CondDelete::NoMatch) => {
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(fhirpg_store::CondDelete::Multiple) => oo(
            StatusCode::PRECONDITION_FAILED,
            "multiple-matches",
            "criteria match more than one resource",
        ),
        Err(e) => err_response(e),
    }
}

async fn delete_instance(
    State(app): State<Arc<AppState>>,
    Path((v, ty, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let vs = match app.typed(&v, &ty) {
        Ok(vs) => vs,
        Err(r) => return r,
    };
    if let Err(r) = check_id(&id) {
        return r;
    }
    let audit = match app.audit_for(&headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    match vs.store.delete_audited(&ty, &id, &audit).await {
        // FHIR delete is idempotent: deleting the absent succeeds.
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_response(e),
    }
}

// ---------- search ----------

async fn search_type(
    State(app): State<Arc<AppState>>,
    Path((v, ty)): Path<(String, String)>,
    Query(params): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    run_search(&app, &v, &ty, params, &headers).await
}

/// POST {ty}/_search with a form-encoded body.
async fn search_type_post(
    State(app): State<Arc<AppState>>,
    Path((v, ty)): Path<(String, String)>,
    Query(qparams): Query<Vec<(String, String)>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut params = qparams;
    match serde_urlencoded::from_bytes::<Vec<(String, String)>>(&body) {
        Ok(mut b) => params.append(&mut b),
        Err(e) => {
            return oo(
                StatusCode::BAD_REQUEST,
                "structure",
                &format!("invalid form body: {e}"),
            );
        }
    }
    run_search(&app, &v, &ty, params, &headers).await
}

async fn run_search(
    app: &AppState,
    v: &str,
    ty: &str,
    params: Vec<(String, String)>,
    headers: &HeaderMap,
) -> Response {
    let vs = match app.typed(v, ty) {
        Ok(vs) => vs,
        Err(r) => return r,
    };
    let audit = match app.audit_for(headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let mut count = DEFAULT_COUNT;
    let mut offset: i64 = 0;
    let mut sort: Vec<fhirpg_store::search::SortKey> = Vec::new();
    let mut want_total = false;
    let mut includes: Vec<String> = Vec::new();
    let mut revincludes: Vec<(String, String)> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut search_params: Vec<(String, String)> = Vec::new();
    for (k, val) in &params {
        match k.as_str() {
            "_count" => match val.parse::<i64>() {
                Ok(n) if n >= 0 => count = n.min(app.limits.max_count),
                _ => return oo(StatusCode::BAD_REQUEST, "invalid", "invalid _count"),
            },
            "_offset" => match val.parse::<i64>() {
                Ok(n) if n >= 0 => offset = n,
                _ => return oo(StatusCode::BAD_REQUEST, "invalid", "invalid _offset"),
            },
            "_sort" => {
                for key in val.split(',') {
                    let (descending, code) = match key.strip_prefix('-') {
                        Some(c) => (true, c),
                        None => (false, key),
                    };
                    sort.push(fhirpg_store::search::SortKey {
                        code: code.to_string(),
                        descending,
                    });
                }
            }
            "_total" => want_total = val != "none",
            "_cursor" => cursor = Some(val.clone()),
            "_include" => {
                let mut it = val.split(':');
                let (src, p) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
                if src != ty || p.is_empty() {
                    return oo(
                        StatusCode::BAD_REQUEST,
                        "invalid",
                        &format!("_include must be {ty}:<reference-param>"),
                    );
                }
                includes.push(p.to_string());
            }
            "_revinclude" => {
                let mut it = val.split(':');
                let (src, p) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
                if src.is_empty() || p.is_empty() {
                    return oo(
                        StatusCode::BAD_REQUEST,
                        "invalid",
                        "_revinclude must be SourceType:<reference-param>",
                    );
                }
                revincludes.push((src.to_string(), p.to_string()));
            }
            // Tolerated result parameters with no effect.
            "_format" | "_pretty" => {}
            _ if k.starts_with('_') && k != "_id" && k != "_lastUpdated" => {
                return oo(
                    StatusCode::NOT_IMPLEMENTED,
                    "not-supported",
                    &format!("result parameter {k:?} is not implemented"),
                );
            }
            _ => search_params.push((k.clone(), val.clone())),
        }
    }
    if cursor.is_some() && !sort.is_empty() {
        return oo(
            StatusCode::BAD_REQUEST,
            "invalid",
            "_cursor cannot be combined with _sort",
        );
    }
    let outcome = match vs
        .store
        .search_page(
            ty,
            &search_params,
            count,
            offset,
            &sort,
            want_total,
            cursor.as_deref(),
        )
        .await
    {
        Ok(o) => o,
        Err(e) => return err_response(e),
    };
    let ids = outcome.ids;
    // A search is a disclosure of every resource it returns; the count is
    // what makes "this account read 4,000 charts at 03:00" visible.
    if let Err(r) = app
        .audit_read(
            vs,
            &audit,
            "search",
            Some(ty),
            None,
            "ok",
            Some(ids.len() as i64),
        )
        .await
    {
        return r;
    }
    let base = app.base.resolve(v, headers);
    // One snapshot for the whole page (spec R4.5): entries in a bundle
    // describe one consistent moment, not one moment per entry.
    let page: Vec<(String, String)> = ids.iter().map(|id| (ty.to_string(), id.clone())).collect();
    let got_page = match vs.store.get_all(&page).await {
        Ok(g) => g,
        Err(e) => return err_response(e),
    };
    let mut entries = Vec::with_capacity(ids.len());
    for (id, got) in ids.iter().zip(got_page) {
        if let Some(got) = got {
            entries.push(json!({
                "fullUrl": format!("{base}/{ty}/{id}"),
                "resource": with_meta(got.resource, got.version_id),
                "search": {"mode": "match"}
            }));
        }
    }
    // Single-hop _include: referenced resources of the matches.
    let mut included: Vec<(String, String)> = Vec::new();
    for param in &includes {
        match vs.store.refs_of(ty, &ids, param).await {
            Ok(refs) => included.extend(refs),
            Err(e) => return err_response(e),
        }
    }
    // Single-hop _revinclude: resources of another type referencing the
    // matches, found through the ordinary search machinery.
    if !revincludes.is_empty() && !ids.is_empty() {
        let targets: Vec<String> = ids.iter().map(|id| format!("{ty}/{id}")).collect();
        let joined = targets.join(",");
        for (src, param) in &revincludes {
            let rev = match vs
                .store
                .search(
                    src,
                    &[(param.clone(), joined.clone())],
                    app.limits.max_count,
                    0,
                )
                .await
            {
                Ok(r) => r,
                Err(e) => return err_response(e),
            };
            included.extend(rev.into_iter().map(|id| (src.clone(), id)));
        }
    }
    included.sort();
    included.dedup();
    included.retain(|(ity, iid)| {
        // Already a match entry, or a reference to a type this version does
        // not define.
        !(ity == ty && ids.contains(iid)) && vs.store.map().resources.contains_key(ity)
    });
    // Cap the expansion, and say so in the bundle when it bites: silently
    // returning some of the requested context is a patient-safety defect,
    // not a performance trade (spec P6.7).
    let truncated = included.len() > app.limits.max_included;
    included.truncate(app.limits.max_included);
    let got_included = match vs.store.get_all(&included).await {
        Ok(g) => g,
        Err(e) => return err_response(e),
    };
    for ((ity, iid), got) in included.iter().zip(got_included) {
        // A `None` here is a dangling reference, which FHIR permits.
        if let Some(got) = got {
            entries.push(json!({
                "fullUrl": format!("{base}/{ity}/{iid}"),
                "resource": with_meta(got.resource, got.version_id),
                "search": {"mode": "include"}
            }));
        }
    }

    let self_q = rebuild_query(&params);
    let mut links = vec![json!({
        "relation": "self",
        "url": format!("{base}/{ty}{self_q}")
    })];
    if ids.len() as i64 == count && count > 0 {
        let mut next_params: Vec<(String, String)> = params
            .iter()
            .filter(|(k, _)| k != "_offset" && k != "_cursor")
            .cloned()
            .collect();
        if sort.is_empty() {
            // Keyset cursor: stable under concurrent writes.
            next_params.push((
                "_cursor".to_string(),
                ids.last().expect("non-empty page").clone(),
            ));
        } else {
            next_params.push(("_offset".to_string(), (offset + count).to_string()));
        }
        links.push(json!({
            "relation": "next",
            "url": format!("{base}/{ty}{}", rebuild_query(&next_params))
        }));
    }
    if truncated {
        entries.push(json!({
            "search": {"mode": "outcome"},
            "resource": {
                "resourceType": "OperationOutcome",
                "issue": [{
                    "severity": "warning",
                    "code": "too-costly",
                    "diagnostics": format!(
                        "_include/_revinclude expansion was capped at {} \
                         resources; narrow the search to see the rest",
                        app.limits.max_included
                    )
                }]
            }
        }));
    }
    let mut bundle = json!({
        "resourceType": "Bundle",
        "type": "searchset",
        "link": links,
        "entry": entries
    });
    if let Some(total) = outcome.total {
        bundle["total"] = json!(total);
    }
    fhir_json(StatusCode::OK, &bundle)
}

fn rebuild_query(params: &[(String, String)]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let q = serde_urlencoded::to_string(params).unwrap_or_default();
    format!("?{q}")
}

// ---------- batch / transaction ----------

struct BundleEntry {
    index: usize,
    full_url: Option<String>,
    resource: Option<Value>,
    method: String,
    url: String,
    /// `Bundle.entry.request.ifMatch`, parsed to a version expectation.
    if_match: Option<i64>,
    /// Any precondition present that this server does not implement. Held
    /// rather than dropped: accepting a precondition and ignoring it leaves
    /// the client believing it has concurrency control that it has not
    /// (spec A7.9).
    unsupported_precondition: Option<String>,
}

async fn bundle_endpoint(
    State(app): State<Arc<AppState>>,
    Path(v): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let vs = match app.version(&v) {
        Ok(vs) => vs,
        Err(r) => return r,
    };
    let audit = match app.audit_for(&headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let bundle = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if bundle.get("resourceType").and_then(Value::as_str) != Some("Bundle") {
        return oo(StatusCode::BAD_REQUEST, "invalid", "expected a Bundle");
    }
    let btype = bundle.get("type").and_then(Value::as_str).unwrap_or("");
    let entries = match parse_entries(&bundle) {
        Ok(e) => e,
        Err(r) => return r,
    };
    match btype {
        "batch" => run_batch(vs, entries, &audit).await,
        "transaction" => run_transaction(vs, entries, &audit).await,
        other => oo(
            StatusCode::BAD_REQUEST,
            "invalid",
            &format!("unsupported Bundle.type {other:?} at the system endpoint"),
        ),
    }
}

fn parse_entries(bundle: &Value) -> Result<Vec<BundleEntry>, Response> {
    let mut out = Vec::new();
    for (i, e) in bundle
        .get("entry")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let req = e.get("request").ok_or_else(|| {
            oo(
                StatusCode::BAD_REQUEST,
                "invalid",
                &format!("entry {i} has no request"),
            )
        })?;
        let str_field = |name: &str| req.get(name).and_then(Value::as_str).map(str::to_string);
        let raw_if_match = str_field("ifMatch");
        let mut unsupported = None;
        let mut if_match = None;
        if let Some(raw) = &raw_if_match {
            match parse_etag_version(raw) {
                Some(v) => if_match = Some(v),
                None => unsupported = Some(format!("unparseable ifMatch {raw:?}")),
            }
        }
        for name in ["ifNoneExist", "ifModifiedSince", "ifNoneMatch"] {
            if unsupported.is_none() && str_field(name).is_some() {
                unsupported = Some(format!("{name} in a Bundle entry is not implemented"));
            }
        }
        out.push(BundleEntry {
            index: i,
            full_url: e.get("fullUrl").and_then(Value::as_str).map(str::to_string),
            resource: e.get("resource").cloned(),
            method: req
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_uppercase(),
            url: req
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim_start_matches('/')
                .to_string(),
            if_match,
            unsupported_precondition: unsupported,
        });
    }
    Ok(out)
}

/// `Type/id` from a Bundle entry url, with the id held to the FHIR `id`
/// production (spec R4.6) — a bundle entry is no less untrusted than a URL.
fn split_type_id(url: &str) -> Option<(&str, &str)> {
    let mut parts = url.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(ty), Some(id), None) if !ty.is_empty() && valid_fhir_id(id) => Some((ty, id)),
        _ => None,
    }
}

async fn run_batch(vs: &VersionState, entries: Vec<BundleEntry>, audit: &Audit) -> Response {
    let mut responses = Vec::with_capacity(entries.len());
    for e in entries {
        let resp = batch_entry(vs, &e, audit).await;
        responses.push(resp);
    }
    fhir_json(
        StatusCode::OK,
        &json!({
            "resourceType": "Bundle",
            "type": "batch-response",
            "entry": responses
        }),
    )
}

async fn batch_entry(vs: &VersionState, e: &BundleEntry, audit: &Audit) -> Value {
    let fail = |status: &str, msg: &str| {
        json!({"response": {"status": status, "outcome": {
            "resourceType": "OperationOutcome",
            "issue": [{"severity": "error", "code": "processing", "diagnostics": msg}]
        }}})
    };
    if let Some(why) = &e.unsupported_precondition {
        return fail("501", why);
    }
    match e.method.as_str() {
        "GET" => match split_type_id(&e.url) {
            Some((ty, id)) => match vs.store.get(ty, id).await {
                Ok(Some(got)) => json!({
                    "resource": with_meta(got.resource, got.version_id),
                    "response": {"status": "200", "etag": etag(got.version_id)}
                }),
                Ok(None) => fail("404", "not found"),
                Err(err) => fail("400", &err.to_string()),
            },
            None => fail("501", "only instance reads are supported in batch GET"),
        },
        "POST" => {
            let ty = e.url.split('/').next().unwrap_or("");
            let Some(mut resource) = e.resource.clone() else {
                return fail("400", "POST entry without a resource");
            };
            let id = uuid::Uuid::new_v4().to_string();
            resource["id"] = Value::String(id);
            match vs.store.put_audited(&resource, Some(0), audit).await {
                Ok(out) => json!({"response": {
                    "status": "201",
                    "location": format!("{ty}/{}/_history/{}", out.id, out.version_id),
                    "etag": etag(out.version_id)
                }}),
                Err(err) => fail("400", &err.to_string()),
            }
        }
        "PUT" => {
            let Some((ty, id)) = split_type_id(&e.url) else {
                return fail("400", "PUT url must be Type/id");
            };
            let Some(mut resource) = e.resource.clone() else {
                return fail("400", "PUT entry without a resource");
            };
            resource["id"] = Value::String(id.to_string());
            let put = match e.if_match {
                Some(v) => vs.store.put_audited(&resource, Some(v), audit).await,
                None => vs.store.put_audited(&resource, None, audit).await,
            };
            match put {
                Ok(out) => json!({"response": {
                    "status": if out.created { "201" } else { "200" },
                    "location": format!("{ty}/{}/_history/{}", out.id, out.version_id),
                    "etag": etag(out.version_id)
                }}),
                // A failed precondition is 412, not a generic 400: the client
                // needs to tell "you lost a race" from "your request was
                // malformed".
                Err(err @ StoreError::Conflict { .. }) => fail("412", &err.to_string()),
                Err(err) => fail("400", &err.to_string()),
            }
        }
        "DELETE" => match split_type_id(&e.url) {
            Some((ty, id)) => match vs.store.delete_audited(ty, id, audit).await {
                Ok(_) => json!({"response": {"status": "204"}}),
                Err(err) => fail("400", &err.to_string()),
            },
            None => fail("400", "DELETE url must be Type/id"),
        },
        m => fail("400", &format!("unsupported method {m:?}")),
    }
}

async fn run_transaction(
    vs: &VersionState,
    mut entries: Vec<BundleEntry>,
    audit: &Audit,
) -> Response {
    // A transaction is all-or-nothing, so an unimplementable precondition
    // fails the whole bundle rather than one entry (spec A7.9).
    if let Some(e) = entries
        .iter()
        .find(|e| e.unsupported_precondition.is_some())
    {
        return oo(
            StatusCode::NOT_IMPLEMENTED,
            "not-supported",
            e.unsupported_precondition.as_deref().expect("checked"),
        );
    }
    // Assign ids to creates, then rewrite urn:uuid references everywhere.
    let mut urn_map: Vec<(String, String)> = Vec::new();
    for e in &mut entries {
        if e.method == "POST" {
            let ty = e.url.split('/').next().unwrap_or("").to_string();
            let id = uuid::Uuid::new_v4().to_string();
            if let Some(r) = &mut e.resource {
                r["id"] = Value::String(id.clone());
            }
            if let Some(fu) = &e.full_url {
                urn_map.push((fu.clone(), format!("{ty}/{id}")));
            }
            e.url = format!("{ty}/{id}");
        } else if e.method == "PUT"
            && let (Some(fu), Some((ty, id))) = (&e.full_url, split_type_id(&e.url))
        {
            urn_map.push((fu.clone(), format!("{ty}/{id}")));
        }
    }
    for e in &mut entries {
        if let Some(r) = &mut e.resource {
            rewrite_refs(r, &urn_map);
        }
    }
    // FHIR processing order: DELETE, POST, PUT. Reads inside transactions
    // are not supported yet.
    if entries.iter().any(|e| e.method == "GET") {
        return oo(
            StatusCode::NOT_IMPLEMENTED,
            "not-supported",
            "GET entries in a transaction are not implemented",
        );
    }
    let mut order: Vec<usize> = (0..entries.len()).collect();
    let rank = |m: &str| match m {
        "DELETE" => 0,
        "POST" => 1,
        "PUT" => 2,
        _ => 3,
    };
    order.sort_by_key(|&i| (rank(&entries[i].method), i));

    let mut ops = Vec::with_capacity(entries.len());
    let mut op_entry: Vec<usize> = Vec::with_capacity(entries.len());
    for &i in &order {
        let e = &entries[i];
        match e.method.as_str() {
            "DELETE" => {
                let Some((ty, id)) = split_type_id(&e.url) else {
                    return oo(
                        StatusCode::BAD_REQUEST,
                        "invalid",
                        "DELETE url must be Type/id",
                    );
                };
                ops.push(TxOp::Delete {
                    rtype: ty.to_string(),
                    id: id.to_string(),
                });
            }
            "POST" | "PUT" => {
                let Some(mut resource) = e.resource.clone() else {
                    return oo(
                        StatusCode::BAD_REQUEST,
                        "invalid",
                        "write entry without a resource",
                    );
                };
                if let Some((_, id)) = split_type_id(&e.url) {
                    resource["id"] = Value::String(id.to_string());
                }
                // POST must not overwrite (expect version 0); PUT honors the
                // entry's ifMatch when it carries one.
                let expected = if e.method == "POST" {
                    Some(0)
                } else {
                    e.if_match
                };
                ops.push(TxOp::Put { resource, expected });
            }
            m => {
                return oo(
                    StatusCode::BAD_REQUEST,
                    "invalid",
                    &format!("unsupported method {m:?} in transaction"),
                );
            }
        }
        op_entry.push(e.index);
    }
    let outcomes = match vs.store.transact_audited(&ops, audit).await {
        Ok(o) => o,
        Err(e) => return err_response(e),
    };
    // Responses in original entry order.
    let mut responses: Vec<Value> = vec![Value::Null; entries.len()];
    for (oi, outcome) in outcomes.iter().enumerate() {
        let entry_index = op_entry[oi];
        let url = &entries
            .iter()
            .find(|e| e.index == entry_index)
            .expect("entry")
            .url;
        responses[entry_index] = match outcome {
            TxOutcome::Put(out) => json!({"response": {
                "status": if out.created { "201" } else { "200" },
                "location": format!("{url}/_history/{}", out.version_id),
                "etag": etag(out.version_id)
            }}),
            TxOutcome::Delete(_) => json!({"response": {"status": "204"}}),
        };
    }
    fhir_json(
        StatusCode::OK,
        &json!({
            "resourceType": "Bundle",
            "type": "transaction-response",
            "entry": responses
        }),
    )
}

/// Replace `urn:uuid:…` **reference** values with their assigned `Type/id`.
///
/// Only `Reference.reference` values are rewritten. A blanket whole-string
/// substitution would also rewrite narrative text, `valueString` extensions,
/// `Identifier.value`, and any other place a client legitimately carries the
/// urn as data — silently corrupting the resource it was asked to store.
fn rewrite_refs(v: &mut Value, urn_map: &[(String, String)]) {
    match v {
        Value::Array(a) => a.iter_mut().for_each(|x| rewrite_refs(x, urn_map)),
        Value::Object(m) => {
            for (k, child) in m.iter_mut() {
                if k == "reference"
                    && let Value::String(s) = child
                    && let Some((_, to)) = urn_map.iter().find(|(from, _)| from == s)
                {
                    *s = to.clone();
                    continue;
                }
                rewrite_refs(child, urn_map);
            }
        }
        _ => {}
    }
}

// ---------- capability ----------

fn capability_statement(v: &str, store: &Store) -> Value {
    let map = store.map();
    let resources: Vec<Value> = map
        .resources
        .values()
        .map(|rm| {
            let params: Vec<Value> = rm
                .search
                .iter()
                .filter(|d| !d.targets.is_empty())
                .map(|d| {
                    json!({
                        "name": d.code,
                        "type": format!("{:?}", d.ty).to_lowercase()
                    })
                })
                .collect();
            json!({
                "type": rm.name,
                "interaction": [
                    {"code": "read"}, {"code": "vread"}, {"code": "update"},
                    {"code": "delete"}, {"code": "create"},
                    {"code": "history-instance"}, {"code": "search-type"}
                ],
                // Declared because they are implemented (spec A7.12). An
                // over-claiming CapabilityStatement is a conformance defect:
                // clients decide what to attempt by reading this.
                "versioning": "versioned",
                "readHistory": true,
                "updateCreate": true,
                "conditionalCreate": true,
                "conditionalUpdate": false,
                "conditionalDelete": "single",
                "conditionalRead": "not-supported",
                "referencePolicy": ["literal", "local"],
                "searchInclude": ["*"],
                "searchRevInclude": ["*"],
                "searchParam": params
            })
        })
        .collect();
    json!({
        "resourceType": "CapabilityStatement",
        "status": "active",
        "date": "2026-01-01",
        "kind": "instance",
        "software": {"name": "fhirpg", "version": env!("CARGO_PKG_VERSION")},
        "implementation": {"description": format!("fhirpg {v} relational store")},
        "fhirVersion": map.fhir_version,
        "format": ["application/fhir+json"],
        "rest": [{
            "mode": "server",
            // fhirpg authenticates nothing itself; saying so here is more
            // useful to a client than silence, and it is the honest reading
            // of the trust boundary (spec PR12.8, A7.12).
            "security": {
                "description": "Authentication and authorization are provided by \
                                the deployment perimeter; fhirpg records the \
                                principal it is given but verifies none."
            },
            "interaction": [{"code": "transaction"}, {"code": "batch"}],
            "resource": resources
        }]
    })
}
