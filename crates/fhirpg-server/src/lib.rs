//! The FHIR RESTful API over the fhirpg relational store (spec §7).
//!
//! Each installed FHIR version mounts at `/{r3|r4|r5}`. JSON only
//! (`application/fhir+json`); errors are OperationOutcomes; writes carry
//! ETags and honor If-Match.

// Helper fallibility is expressed as Result<T, Response>; the ready-made
// error Response is moved once into the handler's return, so its size is
// not a real cost.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use fhirpg_store::{PutOutcome, ResourceStatus, Store, StoreError, TxOp, TxOutcome};
use serde_json::{Map, Value, json};

const MAX_BODY: usize = 32 * 1024 * 1024;
const DEFAULT_COUNT: i64 = 50;
const MAX_COUNT: i64 = 1000;

pub struct AppState {
    versions: BTreeMap<String, VersionState>,
    metrics: Metrics,
}

/// Low-cardinality process counters, served in Prometheus text format.
#[derive(Default)]
struct Metrics {
    requests: std::sync::atomic::AtomicU64,
    responses_2xx: std::sync::atomic::AtomicU64,
    responses_4xx: std::sync::atomic::AtomicU64,
    responses_5xx: std::sync::atomic::AtomicU64,
    latency_micros: std::sync::atomic::AtomicU64,
}

struct VersionState {
    store: Arc<Store>,
    capability: Value,
}

/// Build the router for a set of installed versions
/// (base path segment → store).
pub fn router(versions: BTreeMap<String, Arc<Store>>) -> Router {
    let state = Arc::new(AppState {
        versions: versions
            .into_iter()
            .map(|(v, store)| {
                let capability = capability_statement(&v, &store);
                (v, VersionState { store, capability })
            })
            .collect(),
        metrics: Metrics::default(),
    });
    Router::new()
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
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(axum::middleware::from_fn_with_state(state.clone(), observe))
        .with_state(state)
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
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|h| h.to_str().ok())
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
        .latency_micros
        .fetch_add(started.elapsed().as_micros() as u64, Relaxed);
    if let Ok(hv) = request_id.parse() {
        resp.headers_mut().insert("x-request-id", hv);
    }
    tracing::info!(%method, path, status = status.as_u16(), request_id, "request");
    resp
}

async fn metrics_endpoint(State(app): State<Arc<AppState>>) -> Response {
    use std::sync::atomic::Ordering::Relaxed;
    let m = &app.metrics;
    let body = format!(
        "# TYPE fhirpg_requests_total counter\n         fhirpg_requests_total {}\n         # TYPE fhirpg_responses_total counter\n         fhirpg_responses_total{{class=\"2xx\"}} {}\n         fhirpg_responses_total{{class=\"4xx\"}} {}\n         fhirpg_responses_total{{class=\"5xx\"}} {}\n         # TYPE fhirpg_request_latency_micros_total counter\n         fhirpg_request_latency_micros_total {}\n",
        m.requests.load(Relaxed),
        m.responses_2xx.load(Relaxed),
        m.responses_4xx.load(Relaxed),
        m.responses_5xx.load(Relaxed),
        m.latency_micros.load(Relaxed),
    );
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
        StoreError::Shred(e) => oo(StatusCode::BAD_REQUEST, "invalid", &e.to_string()),
        StoreError::Other(msg) => oo(StatusCode::BAD_REQUEST, "processing", &msg),
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

fn parse_if_match(headers: &HeaderMap) -> Result<Option<i64>, Response> {
    let Some(h) = headers.get(header::IF_MATCH) else {
        return Ok(None);
    };
    let s = h.to_str().unwrap_or_default();
    let trimmed = s.trim().trim_start_matches("W/").trim_matches('"');
    trimmed.parse::<i64>().map(Some).map_err(|_| {
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
) -> Response {
    let vs = match app.typed(&v, &ty) {
        Ok(vs) => vs,
        Err(r) => return r,
    };
    match vs.store.get(&ty, &id).await {
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
) -> Response {
    let vs = match app.typed(&v, &ty) {
        Ok(vs) => vs,
        Err(r) => return r,
    };
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
) -> Response {
    let vs = match app.typed(&v, &ty) {
        Ok(vs) => vs,
        Err(r) => return r,
    };
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
    // Conditional create: If-None-Exist carries search criteria; 0 matches
    // creates, 1 match returns it unchanged, several is an error.
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
        match vs.store.search(&ty, &criteria, 2, 0).await {
            Ok(matches) => match matches.len() {
                0 => {}
                1 => {
                    return match vs.store.get(&ty, &matches[0]).await {
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
                    };
                }
                _ => {
                    return oo(
                        StatusCode::PRECONDITION_FAILED,
                        "multiple-matches",
                        "If-None-Exist criteria match more than one resource",
                    );
                }
            },
            Err(e) => return err_response(e),
        }
    }
    // The server assigns ids on create; a client-sent id is ignored.
    let id = uuid::Uuid::new_v4().to_string();
    resource["id"] = Value::String(id.clone());
    match vs.store.put_if(&resource, Some(0)).await {
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
    let expected = match parse_if_match(&headers) {
        Ok(e) => e,
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
    match vs.store.put_if(&resource, expected).await {
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
    match vs.store.search(&ty, &params, 2, 0).await {
        Ok(matches) => match matches.len() {
            0 => StatusCode::NO_CONTENT.into_response(),
            1 => match vs.store.delete(&ty, &matches[0]).await {
                Ok(_) => StatusCode::NO_CONTENT.into_response(),
                Err(e) => err_response(e),
            },
            _ => oo(
                StatusCode::PRECONDITION_FAILED,
                "multiple-matches",
                "criteria match more than one resource",
            ),
        },
        Err(e) => err_response(e),
    }
}

async fn delete_instance(
    State(app): State<Arc<AppState>>,
    Path((v, ty, id)): Path<(String, String, String)>,
) -> Response {
    let vs = match app.typed(&v, &ty) {
        Ok(vs) => vs,
        Err(r) => return r,
    };
    match vs.store.delete(&ty, &id).await {
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
                Ok(n) if n >= 0 => count = n.min(MAX_COUNT),
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
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    let base = format!("http://{host}/{v}");
    let mut entries = Vec::with_capacity(ids.len());
    for id in &ids {
        match vs.store.get(ty, id).await {
            Ok(Some(got)) => entries.push(json!({
                "fullUrl": format!("{base}/{ty}/{id}"),
                "resource": with_meta(got.resource, got.version_id),
                "search": {"mode": "match"}
            })),
            Ok(None) => {}
            Err(e) => return err_response(e),
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
                .search(src, &[(param.clone(), joined.clone())], MAX_COUNT, 0)
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
    for (ity, iid) in included {
        if ity == ty && ids.contains(&iid) {
            continue; // already a match entry
        }
        if !vs.store.map().resources.contains_key(&ity) {
            continue; // reference to a type outside this version
        }
        match vs.store.get(&ity, &iid).await {
            Ok(Some(got)) => entries.push(json!({
                "fullUrl": format!("{base}/{ity}/{iid}"),
                "resource": with_meta(got.resource, got.version_id),
                "search": {"mode": "include"}
            })),
            Ok(None) => {} // dangling reference — legal in FHIR
            Err(e) => return err_response(e),
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
}

async fn bundle_endpoint(
    State(app): State<Arc<AppState>>,
    Path(v): Path<String>,
    body: Bytes,
) -> Response {
    let vs = match app.version(&v) {
        Ok(vs) => vs,
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
        "batch" => run_batch(vs, entries).await,
        "transaction" => run_transaction(vs, entries).await,
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
        });
    }
    Ok(out)
}

fn split_type_id(url: &str) -> Option<(&str, &str)> {
    let mut parts = url.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(ty), Some(id), None) if !ty.is_empty() && !id.is_empty() => Some((ty, id)),
        _ => None,
    }
}

async fn run_batch(vs: &VersionState, entries: Vec<BundleEntry>) -> Response {
    let mut responses = Vec::with_capacity(entries.len());
    for e in entries {
        let resp = batch_entry(vs, &e).await;
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

async fn batch_entry(vs: &VersionState, e: &BundleEntry) -> Value {
    let fail = |status: &str, msg: &str| {
        json!({"response": {"status": status, "outcome": {
            "resourceType": "OperationOutcome",
            "issue": [{"severity": "error", "code": "processing", "diagnostics": msg}]
        }}})
    };
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
            match vs.store.put_if(&resource, Some(0)).await {
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
            match vs.store.put(&resource).await {
                Ok(out) => json!({"response": {
                    "status": if out.created { "201" } else { "200" },
                    "location": format!("{ty}/{}/_history/{}", out.id, out.version_id),
                    "etag": etag(out.version_id)
                }}),
                Err(err) => fail("400", &err.to_string()),
            }
        }
        "DELETE" => match split_type_id(&e.url) {
            Some((ty, id)) => match vs.store.delete(ty, id).await {
                Ok(_) => json!({"response": {"status": "204"}}),
                Err(err) => fail("400", &err.to_string()),
            },
            None => fail("400", "DELETE url must be Type/id"),
        },
        m => fail("400", &format!("unsupported method {m:?}")),
    }
}

async fn run_transaction(vs: &VersionState, mut entries: Vec<BundleEntry>) -> Response {
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
                let expected = if e.method == "POST" { Some(0) } else { None };
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
    let outcomes = match vs.store.transact(&ops).await {
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

/// Replace whole-string reference values ("urn:uuid:…") with their assigned
/// Type/id, everywhere in a resource.
fn rewrite_refs(v: &mut Value, urn_map: &[(String, String)]) {
    match v {
        Value::String(s) => {
            if let Some((_, to)) = urn_map.iter().find(|(from, _)| from == s) {
                *s = to.clone();
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| rewrite_refs(x, urn_map)),
        Value::Object(m) => m.values_mut().for_each(|x| rewrite_refs(x, urn_map)),
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
        "rest": [{"mode": "server", "resource": resources}]
    })
}
