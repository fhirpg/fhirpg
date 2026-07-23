//! The `web` subcommand.
//!
//! Ports `WebCommand` (`web.go:20-189`): a browser console for running SQL
//! against the loaded data. Specified in `spec/index.md` §9.
//!
//! # This endpoint executes arbitrary SQL with no authentication
//!
//! That is the feature, and it is why the server binds `127.0.0.1` by default.
//! fhirbase defaults `--webhost` to the empty string, which binds every
//! interface — an unauthenticated database console on the network, in a tool
//! whose whole purpose is holding patient data. That is defect X11.
//!
//! Binding anything other than a loopback address requires saying so
//! explicitly, and prints a warning when you do.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use deadpool_postgres::{Manager, Pool};
use serde_json::{Value, json};

use crate::config::PgConfig;
use crate::error::{Error, Result};

/// The console's static assets, embedded so the binary is self-contained.
const INDEX_HTML: &str = include_str!("../../assets/web/index.html");
/// The console's stylesheet.
const APP_CSS: &str = include_str!("../../assets/web/app.css");
/// The console's script.
const APP_JS: &str = include_str!("../../assets/web/app.js");

/// Shared state for the request handlers.
#[derive(Clone)]
struct AppState {
    pool: Pool,
}

/// Runs the `web` subcommand.
///
/// # Errors
///
/// Returns [`Error::Db`] if the pool cannot be built or the database is
/// unreachable, and [`Error::Config`] if the address cannot be bound.
pub async fn run(config: &PgConfig, host: &str, port: u16) -> Result<()> {
    // Verify the database is reachable and new enough before binding a port;
    // otherwise the first query fails in the browser rather than at startup.
    drop(crate::db::connect(config).await?);

    let manager = Manager::new(config.to_pg_config(), tokio_postgres::NoTls);
    let pool = Pool::builder(manager)
        .max_size(16)
        .build()
        .map_err(|e| Error::Db(format!("cannot build the connection pool: {e}")))?;

    let address: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| Error::Config(format!("cannot parse the address {host}:{port}: {e}")))?;

    warn_if_exposed(address.ip());

    let app = axum::Router::new()
        .route("/", get(index))
        .route("/app.css", get(stylesheet))
        .route("/app.js", get(script))
        .route("/q", get(query))
        .route("/health", get(health))
        .with_state(AppState { pool });

    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|e| Error::Config(format!("cannot bind {address}: {e}")))?;

    println!("SQL console on http://{address}");
    println!("Press Ctrl-C to stop.");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| Error::Config(format!("the web server failed: {e}")))?;

    println!("Server stopped");
    Ok(())
}

/// Prints a warning when the console is bound somewhere reachable.
///
/// Defect X11. Refusing outright would be wrong — exposing it deliberately, on
/// a trusted network, against a scratch database, is legitimate — but it must
/// not happen quietly.
fn warn_if_exposed(ip: IpAddr) {
    if ip.is_loopback() {
        return;
    }
    eprintln!(
        "\n\
         WARNING: the SQL console is bound to {ip}, not a loopback address.\n\
         \n\
         The /q endpoint executes ARBITRARY SQL with NO AUTHENTICATION. Anyone\n\
         who can reach this port can read, modify, or destroy every resource in\n\
         the database. Do not do this on an untrusted network, and never against\n\
         a database holding real patient data.\n"
    );
}

/// Waits for Ctrl-C.
async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        eprintln!("cannot listen for Ctrl-C: {e}");
    }
    println!("\nShutting down...");
}

/// Serves the console page.
async fn index() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX_HTML)
}

/// Serves the stylesheet.
async fn stylesheet() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS)
}

/// Serves the script.
async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        APP_JS,
    )
}

/// `GET /health` — reports whether the database can be reached.
async fn health(State(state): State<AppState>) -> Response {
    match state.pool.get().await {
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"message": format!("cannot acquire a database connection: {e}")}),
        ),
        Ok(client) => match client.query_one("SELECT 1", &[]).await {
            Ok(_) => json_response(StatusCode::OK, &json!({"message": "ok"})),
            Err(e) => json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({"message": format!("the database rejected a trivial query: {e}")}),
            ),
        },
    }
}

/// The `?query=` parameter.
#[derive(serde::Deserialize)]
struct QueryParams {
    query: Option<String>,
}

/// `GET /q?query=…` — runs SQL and returns `{columns, rows}`.
///
/// A SQL error is a non-200 with a `message`, never a panic: the whole point of
/// the console is running statements that may well be wrong.
async fn query(State(state): State<AppState>, Query(params): Query<QueryParams>) -> Response {
    let Some(sql) = params.query.filter(|q| !q.trim().is_empty()) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"message": "Please provide a 'query' query-string parameter"}),
        );
    };

    let client = match state.pool.get().await {
        Ok(client) => client,
        Err(e) => {
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({"message": format!("cannot acquire a database connection: {e}")}),
            );
        }
    };

    // Let PostgreSQL render the values.
    //
    // The console runs arbitrary SQL, so a column can be any type at all —
    // timestamps, arrays, enums, ranges, types added by an extension. Decoding
    // those in Rust means a type table that is wrong the moment someone selects
    // something it has not heard of, and an earlier version of this handler
    // silently returned null for `now()` and for `ARRAY[1,2,3]`.
    //
    // `row_to_json` hands the job to the server, which knows every type by
    // definition, and nests jsonb properly rather than stringifying it. Column
    // order survives because serde_json is built with `preserve_order`.
    let wrapped = format!("SELECT row_to_json(fhirpg_q) FROM ({sql}) AS fhirpg_q");

    let Ok(rows) = client.query(wrapped.as_str(), &[]).await else {
        // The wrapper only accepts something that can be a subquery. A
        // statement that cannot — INSERT, CREATE, VACUUM — is run as given, and
        // so is a SELECT with a genuine error, whose real message is the one
        // worth showing rather than the wrapper's.
        return match client.query(sql.as_str(), &[]).await {
                Ok(_) => json_response(
                    StatusCode::OK,
                    &json!({"columns": [], "rows": [], "message": "statement executed"}),
                ),
            Err(raw_error) => json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({"message": describe(&raw_error)}),
            ),
        };
    };

    let mut columns: Vec<String> = Vec::new();
    let mut body: Vec<Vec<Value>> = Vec::with_capacity(rows.len());

    for row in &rows {
        let Ok(Some(value)) = row.try_get::<_, Option<Value>>(0) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        if columns.is_empty() {
            columns = object.keys().cloned().collect();
        }
        body.push(columns.iter().map(|c| object.get(c).cloned().unwrap_or(Value::Null)).collect());
    }

    json_response(StatusCode::OK, &json!({"columns": columns, "rows": body}))
}

/// Builds a JSON response with a status code.
fn json_response(status: StatusCode, body: &Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(body).unwrap_or_else(|_| "{\"message\":\"?\"}".to_owned()),
    )
        .into_response()
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
    fn the_embedded_console_carries_no_trackers() {
        // Decision D1 removed fhirbase's telemetry from the binary. Its web
        // console shipped two third-party trackers — Google Analytics and
        // Yandex Metrica, the latter with `webvisor: true`, which records the
        // session — on a page that renders patient query results. Reinstating
        // them through the UI would defeat D1 entirely (defect X17).
        let assets = format!("{INDEX_HTML}{APP_JS}");
        for tracker in [
            "googletagmanager",
            "gtag",
            "dataLayer",
            "yandex",
            "Metrika",
            "webvisor",
            "UA-125233238",
        ] {
            assert!(
                !assets.contains(tracker),
                "the console still references {tracker:?}"
            );
        }
    }

    #[test]
    fn the_console_fetches_nothing_from_a_third_party_at_runtime() {
        // The snippet list is built in rather than downloaded on every page
        // load, as fhirbase's is. The only `fetch` left is the same-origin one
        // that runs the user's query.
        let external_fetches = APP_JS.matches("fetch(\"http").count()
            + APP_JS.matches("fetch('http").count();
        assert_eq!(external_fetches, 0, "the console still fetches from a third party");
        assert!(APP_JS.contains("Count patients"), "snippets should be built in");
    }

    #[test]
    fn the_console_is_rebranded() {
        assert!(INDEX_HTML.contains("fhirpg SQL console"));
        assert!(!INDEX_HTML.contains("Fhirbase UI"));
        // Health Samurai's logo and links are their branding, not ours.
        assert!(!INDEX_HTML.contains("health-samurai.io"));
        assert!(!INDEX_HTML.contains("logo.svg"));
    }

    #[test]
    fn the_console_calls_the_endpoints_this_server_serves() {
        assert!(APP_JS.contains("\"/q\""));
        assert!(INDEX_HTML.contains("app.css"));
        assert!(INDEX_HTML.contains("app.js"));
    }

    #[test]
    fn loopback_addresses_do_not_warn() {
        // Not observable directly; the assertion is that these are the
        // addresses treated as safe, which is what X11 turns on.
        for ip in ["127.0.0.1", "::1"] {
            let parsed: IpAddr = ip.parse().unwrap();
            assert!(parsed.is_loopback(), "{ip} should be loopback");
            warn_if_exposed(parsed);
        }
    }

    #[test]
    fn every_non_loopback_address_is_treated_as_exposed() {
        for ip in ["0.0.0.0", "192.168.1.10", "10.0.0.1", "::"] {
            let parsed: IpAddr = ip.parse().unwrap();
            assert!(!parsed.is_loopback(), "{ip} should not be loopback");
        }
    }
}

/// Tests that need a live PostgreSQL 18.
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::assets::FhirVersion;
    use crate::commands::init;
    use crate::testdb;

    /// Builds a pool against a throwaway database with the schema installed.
    async fn app_state(suffix: &str) -> Option<(testdb::TestDb, AppState)> {
        let db = testdb::create(suffix).await?;
        let client = db.connect().await;
        init::perform(&client, FhirVersion::V4_0_0).await.unwrap();
        drop(client);

        let dsn = std::env::var("FHIRPG_TEST_DB").ok()?;
        let mut config: tokio_postgres::Config = dsn.parse().ok()?;
        config.dbname(db.name());

        let manager = Manager::new(config, tokio_postgres::NoTls);
        let pool = Pool::builder(manager).max_size(4).build().ok()?;
        Some((db, AppState { pool }))
    }

    async fn body_of(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn health_reports_ok() {
        let Some((db, state)) = app_state("web_health").await else {
            return;
        };
        let (status, body) = body_of(health(State(state)).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["message"], "ok");
        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn a_query_returns_columns_and_rows() {
        let Some((db, state)) = app_state("web_query").await else {
            return;
        };
        let client = db.connect().await;
        client
            .batch_execute(
                r#"INSERT INTO patient (id, txid, status, resource)
                   VALUES ('p1', 0, 'created', '{"resourceType":"Patient","active":true}'::jsonb)"#,
            )
            .await
            .unwrap();

        let params = QueryParams {
            query: Some("SELECT id, txid, resource FROM patient ORDER BY id".to_owned()),
        };
        let (status, body) = body_of(query(State(state), Query(params)).await).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["columns"], serde_json::json!(["id", "txid", "resource"]));
        assert_eq!(body["rows"][0][0], "p1");
        assert_eq!(body["rows"][0][1], 0);
        // jsonb comes back as JSON, not as a quoted string.
        assert_eq!(body["rows"][0][2]["active"], true);

        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn a_sql_error_is_a_json_message_not_a_panic() {
        let Some((db, state)) = app_state("web_error").await else {
            return;
        };
        let params = QueryParams {
            query: Some("SELECT * FROM no_such_table".to_owned()),
        };
        let (status, body) = body_of(query(State(state), Query(params)).await).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let message = body["message"].as_str().unwrap_or_default();
        assert!(message.contains("no_such_table"), "{message}");

        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn a_missing_query_parameter_is_a_400() {
        let Some((db, state)) = app_state("web_noquery").await else {
            return;
        };
        for query_value in [None, Some(String::new()), Some("   ".to_owned())] {
            let params = QueryParams { query: query_value };
            let (status, body) = body_of(query(State(state.clone()), Query(params)).await).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(body["message"].as_str().is_some_and(|m| m.contains("query")));
        }
        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn an_empty_result_set_still_returns_a_shape() {
        let Some((db, state)) = app_state("web_empty").await else {
            return;
        };
        let params = QueryParams {
            query: Some("SELECT id FROM patient WHERE false".to_owned()),
        };
        let (status, body) = body_of(query(State(state), Query(params)).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["rows"], serde_json::json!([]));
        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18; set FHIRPG_TEST_DB"]
    async fn exotic_column_types_do_not_break_the_response() {
        // The console runs arbitrary SQL, so a column can be any type at all.
        let Some((db, state)) = app_state("web_types").await else {
            return;
        };
        let params = QueryParams {
            query: Some(
                "SELECT now() AS ts, ARRAY[1,2,3] AS arr, 'created'::resource_status AS st, \
                 null::text AS nothing, 1.5::float8 AS f, true AS b"
                    .to_owned(),
            ),
        };
        let (status, body) = body_of(query(State(state), Query(params)).await).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let row = &body["rows"][0];
        assert_eq!(body["columns"], serde_json::json!(["ts", "arr", "st", "nothing", "f", "b"]),
            "columns must keep SELECT order");
        assert!(row[0].is_string(), "timestamp: {row}");
        assert_eq!(row[1], serde_json::json!([1, 2, 3]), "arrays come back as arrays");
        assert_eq!(row[2], "created", "enum as its label");
        assert_eq!(row[3], Value::Null);
        assert_eq!(row[4], 1.5);
        assert_eq!(row[5], true);

        db.drop().await;
    }
}
