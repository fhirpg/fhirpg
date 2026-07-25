//! Edge resource limits (spec O10.8, P6.7). Gated on FHIRPG_TEST_DB.

use std::path::PathBuf;
use std::sync::Arc;

use fhirpg_server::AuditMode;
use fhirpg_store::Store;

fn spec_defs() -> Option<PathBuf> {
    let root = std::env::var("FHIRPG_SPEC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(
                "/Users/jph/git/joelparkerhenderson/fhir-rust-crate/doc/fhir-specifications",
            )
        });
    let defs = root.join("r5").join("fhir-definitions-json");
    defs.exists().then_some(defs)
}

async fn test_store(schema: &str) -> Option<Arc<Store>> {
    let db = std::env::var("FHIRPG_TEST_DB").ok()?;
    let defs = spec_defs()?;
    // SAFETY: set before this binary spawns anything concurrent.
    unsafe { std::env::set_var("PGDATABASE", &db) };
    let map = Arc::new(fhirpg_gen::generate(&defs, schema).expect("generate"));
    let cfg = fhirpg_store::pg_config(None).expect("cfg");
    let store = Store::connect(cfg, map).await.expect("connect");
    store.drop_schema().await.expect("drop");
    store.init("edge-limits").await.expect("init");
    Some(Arc::new(store))
}

/// Configured edge limits are honored, not merely stored (spec O10.8, P6.7).
///
/// A limit that parses but never reaches the code that should enforce it is
/// worse than no limit: the operator believes a ceiling exists. Here `_count`
/// is capped below what the client asks for, and the response has to show it.
#[tokio::test]
async fn configured_max_count_caps_the_page() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let Some(store) = test_store("limitstest").await else {
        eprintln!("skipping: FHIRPG_TEST_DB not set or spec missing");
        return;
    };
    for i in 0..8 {
        store
            .put(&serde_json::json!({
                "resourceType": "Patient", "id": format!("lim{i}"),
                "name": [{"family": "Capped"}]
            }))
            .await
            .expect("seed");
    }
    let mut versions = std::collections::BTreeMap::new();
    versions.insert("limitstest".to_string(), store.clone());
    let app = fhirpg_server::router_full(
        versions,
        fhirpg_server::BaseUrl::bound("http://127.0.0.1:8080"),
        fhirpg_server::PrincipalPolicy::default(),
        AuditMode::Sync,
        fhirpg_server::Limits {
            max_count: 3,
            ..fhirpg_server::Limits::default()
        },
    );

    // The client asks for 50; the configured ceiling is 3.
    let req = Request::builder()
        .method("GET")
        .uri("/limitstest/Patient?family=Capped&_count=50")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let entries = body["entry"].as_array().map(Vec::len).unwrap_or(0);
    assert_eq!(
        entries, 3,
        "_count must be clamped to the configured ceiling"
    );
}
