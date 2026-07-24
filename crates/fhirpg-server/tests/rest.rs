//! REST API integration: CRUD, vread/history, ETag concurrency, search over
//! HTTP with paging, batch, and all-or-nothing transactions — driven through
//! the router in-process. Gated on FHIRPG_TEST_DB.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

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

async fn test_router() -> Option<Router> {
    let db = std::env::var("FHIRPG_TEST_DB").ok()?;
    let defs = spec_defs()?;
    // SAFETY: set before this test binary spawns anything concurrent.
    unsafe { std::env::set_var("PGDATABASE", &db) };
    let map = Arc::new(fhirpg_gen::generate(&defs, "resttest").expect("generate"));
    let cfg = fhirpg_store::pg_config(None).expect("cfg");
    let store = fhirpg_store::Store::connect(cfg, map)
        .await
        .expect("connect");
    store.drop_schema().await.expect("drop");
    store.init("rest-checksum").await.expect("init");
    let mut versions = BTreeMap::new();
    versions.insert("resttest".to_string(), Arc::new(store));
    Some(fhirpg_server::router(versions))
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value, axum::http::HeaderMap) {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = match body {
        Some(v) => req
            .header("content-type", "application/fhir+json")
            .body(Body::from(v.to_string()))
            .expect("request"),
        None => req.body(Body::empty()).expect("request"),
    };
    let resp = app.clone().oneshot(req).await.expect("response");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, v, headers)
}

#[tokio::test]
async fn rest_api() {
    let Some(app) = test_router().await else {
        eprintln!("skipping: FHIRPG_TEST_DB not set or spec missing");
        return;
    };
    let b = "/resttest";

    // health/ready/metadata
    let (st, _, _) = send(&app, "GET", "/health", None, &[]).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _, _) = send(&app, "GET", "/ready", None, &[]).await;
    assert_eq!(st, StatusCode::OK);
    let (st, cap, _) = send(&app, "GET", &format!("{b}/metadata"), None, &[]).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(cap["resourceType"], "CapabilityStatement");
    assert_eq!(cap["fhirVersion"], "5.0.0");
    assert!(cap["rest"][0]["resource"].as_array().unwrap().len() > 150);

    // create → read → update (If-Match) → conflict → history → vread → delete → 410
    let (st, created, hdrs) = send(
        &app,
        "POST",
        &format!("{b}/Patient"),
        Some(json!({"resourceType": "Patient", "gender": "female",
                    "name": [{"family": "Restful"}]})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("id").to_string();
    assert_eq!(created["meta"]["versionId"], "1");
    assert_eq!(hdrs.get("etag").unwrap(), "W/\"1\"");
    assert!(
        hdrs.get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .contains(&format!("Patient/{id}/_history/1"))
    );

    let (st, read, hdrs) = send(&app, "GET", &format!("{b}/Patient/{id}"), None, &[]).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(read["name"][0]["family"], "Restful");
    assert_eq!(hdrs.get("etag").unwrap(), "W/\"1\"");

    let (st, _, _) = send(
        &app,
        "PUT",
        &format!("{b}/Patient/{id}"),
        Some(
            json!({"resourceType": "Patient", "id": id, "gender": "female",
                    "name": [{"family": "Updated"}]}),
        ),
        &[("if-match", "W/\"1\"")],
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, outcome, _) = send(
        &app,
        "PUT",
        &format!("{b}/Patient/{id}"),
        Some(json!({"resourceType": "Patient", "id": id, "gender": "female"})),
        &[("if-match", "W/\"1\"")],
    )
    .await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
    assert_eq!(outcome["resourceType"], "OperationOutcome");

    let (st, hist, _) = send(
        &app,
        "GET",
        &format!("{b}/Patient/{id}/_history"),
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hist["type"], "history");
    assert_eq!(hist["entry"].as_array().unwrap().len(), 2);

    let (st, v1, _) = send(
        &app,
        "GET",
        &format!("{b}/Patient/{id}/_history/1"),
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v1["name"][0]["family"], "Restful");

    let (st, _, _) = send(&app, "DELETE", &format!("{b}/Patient/{id}"), None, &[]).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _, _) = send(&app, "GET", &format!("{b}/Patient/{id}"), None, &[]).await;
    assert_eq!(st, StatusCode::GONE);
    let (st, _, _) = send(&app, "GET", &format!("{b}/Patient/nope"), None, &[]).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // search over HTTP with paging
    for i in 0..5 {
        let (st, _, _) = send(
            &app,
            "PUT",
            &format!("{b}/Patient/pg{i}"),
            Some(json!({"resourceType": "Patient", "id": format!("pg{i}"),
                        "gender": "male", "name": [{"family": "Pager"}]})),
            &[],
        )
        .await;
        assert!(st.is_success());
    }
    let (st, bundle, _) = send(
        &app,
        "GET",
        &format!("{b}/Patient?family=Pager&_count=2"),
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(bundle["type"], "searchset");
    assert_eq!(bundle["entry"].as_array().unwrap().len(), 2);
    let next = bundle["link"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["relation"] == "next")
        .expect("next link")["url"]
        .as_str()
        .unwrap()
        .to_string();
    let next_path = next.trim_start_matches("http://localhost");
    let (st, page2, _) = send(&app, "GET", next_path, None, &[]).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(page2["entry"].as_array().unwrap().len(), 2, "{next_path}");

    // _sort and _total
    let (st, sorted, _) = send(
        &app,
        "GET",
        &format!("{b}/Patient?family=Pager&_sort=-_id&_total=accurate"),
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(sorted["total"], 5);
    let first = sorted["entry"][0]["resource"]["id"].as_str().unwrap();
    assert_eq!(first, "pg4", "descending _id sort");
    // Unsupported sort target errors instead of silently mis-sorting.
    let (st, _, _) = send(&app, "GET", &format!("{b}/Patient?_sort=family"), None, &[]).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // gender is a base-table column: sortable.
    let (st, _, _) = send(&app, "GET", &format!("{b}/Patient?_sort=gender"), None, &[]).await;
    assert_eq!(st, StatusCode::OK);

    // request id header + metrics endpoint
    let (_, _, hdrs) = send(&app, "GET", "/health", None, &[]).await;
    assert!(hdrs.get("x-request-id").is_some());
    let (_, _, hdrs) = send(
        &app,
        "GET",
        "/health",
        None,
        &[("x-request-id", "trace-me-7")],
    )
    .await;
    assert_eq!(hdrs.get("x-request-id").unwrap(), "trace-me-7");
    let req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(text.contains("fhirpg_requests_total"), "{text}");

    // Conditional create (If-None-Exist)
    let cc = json!({"resourceType": "Patient",
                    "identifier": [{"system": "http://cc.example", "value": "one"}]});
    let (st, first, _) = send(
        &app,
        "POST",
        &format!("{b}/Patient"),
        Some(cc.clone()),
        &[("if-none-exist", "identifier=http://cc.example|one")],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, second, _) = send(
        &app,
        "POST",
        &format!("{b}/Patient"),
        Some(cc.clone()),
        &[("if-none-exist", "identifier=http://cc.example|one")],
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "second conditional create must not create"
    );
    assert_eq!(second["id"], first["id"]);

    // POST _search form
    let req = Request::builder()
        .method("POST")
        .uri(format!("{b}/Patient/_search"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("family=Pager&_count=10"))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // unknown search parameter → 400
    let (st, _, _) = send(&app, "GET", &format!("{b}/Patient?bogus=1"), None, &[]).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // unknown type → 404
    let (st, _, _) = send(&app, "GET", &format!("{b}/Bogus/1"), None, &[]).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // batch
    let (st, bresp, _) = send(
        &app,
        "POST",
        b,
        Some(json!({
            "resourceType": "Bundle", "type": "batch",
            "entry": [
                {"request": {"method": "GET", "url": "Patient/pg0"}},
                {"request": {"method": "GET", "url": "Patient/absent"}},
                {"request": {"method": "DELETE", "url": "Patient/pg4"}}
            ]
        })),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(bresp["type"], "batch-response");
    let entries = bresp["entry"].as_array().unwrap();
    assert_eq!(entries[0]["response"]["status"], "200");
    assert_eq!(entries[1]["response"]["status"], "404");
    assert_eq!(entries[2]["response"]["status"], "204");

    // transaction: urn:uuid reference resolution, all-or-nothing
    let (st, tresp, _) = send(
        &app,
        "POST",
        b,
        Some(json!({
            "resourceType": "Bundle", "type": "transaction",
            "entry": [
                {"fullUrl": "urn:uuid:aaaa-bbbb", "resource":
                    {"resourceType": "Patient", "name": [{"family": "TxnPatient"}]},
                 "request": {"method": "POST", "url": "Patient"}},
                {"resource":
                    {"resourceType": "Observation", "status": "final",
                     "code": {"text": "obs"},
                     "subject": {"reference": "urn:uuid:aaaa-bbbb"}},
                 "request": {"method": "POST", "url": "Observation"}}
            ]
        })),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{tresp}");
    assert_eq!(tresp["type"], "transaction-response");
    let loc = tresp["entry"][1]["response"]["location"].as_str().unwrap();
    let obs_id = loc.split('/').nth(1).unwrap();
    let (st, obs, _) = send(&app, "GET", &format!("{b}/Observation/{obs_id}"), None, &[]).await;
    assert_eq!(st, StatusCode::OK);
    let subject = obs["subject"]["reference"].as_str().unwrap();
    assert!(
        subject.starts_with("Patient/") && !subject.contains("urn:"),
        "urn not rewritten: {subject}"
    );
    let (st, pat_check, _) = send(&app, "GET", &format!("{b}/{subject}"), None, &[]).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(pat_check["name"][0]["family"], "TxnPatient");

    // _include and _revinclude (single hop)
    let (st, inc, _) = send(
        &app,
        "GET",
        &format!("{b}/Observation?status=final&_include=Observation:subject"),
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let modes: Vec<&str> = inc["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["search"]["mode"].as_str().unwrap())
        .collect();
    assert!(modes.contains(&"match"), "{modes:?}");
    assert!(modes.contains(&"include"), "{modes:?}");
    let inc_types: Vec<&str> = inc["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["resource"]["resourceType"].as_str().unwrap())
        .collect();
    assert!(inc_types.contains(&"Patient"), "{inc_types:?}");

    let (st, rinc, _) = send(
        &app,
        "GET",
        &format!("{b}/Patient?family=TxnPatient&_revinclude=Observation:subject"),
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let rmodes: Vec<(&str, &str)> = rinc["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["resource"]["resourceType"].as_str().unwrap(),
                e["search"]["mode"].as_str().unwrap(),
            )
        })
        .collect();
    assert!(rmodes.contains(&("Patient", "match")), "{rmodes:?}");
    assert!(rmodes.contains(&("Observation", "include")), "{rmodes:?}");

    // Chained reference search: Observations whose subject is a Patient
    // with a given family name.
    let (st, chained, _) = send(
        &app,
        "GET",
        &format!("{b}/Observation?subject:Patient.family=TxnPatient"),
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{chained}");
    assert_eq!(chained["entry"].as_array().unwrap().len(), 1, "{chained}");
    // Untyped chain is an honest error, not a guess.
    let (st, _, _) = send(
        &app,
        "GET",
        &format!("{b}/Observation?subject.family=TxnPatient"),
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Cursor paging: default ordering pages by keyset, not offset.
    let (st, cpage, _) = send(
        &app,
        "GET",
        &format!("{b}/Patient?family=Pager&_count=2"),
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let next = cpage["link"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["relation"] == "next")
        .expect("next")["url"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(next.contains("_cursor="), "keyset expected: {next}");
    let next_path = next.trim_start_matches("http://localhost").to_string();
    let (st, cpage2, _) = send(&app, "GET", &next_path, None, &[]).await;
    assert_eq!(st, StatusCode::OK);
    let p1_ids: Vec<&str> = cpage["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["resource"]["id"].as_str().unwrap())
        .collect();
    let p2_ids: Vec<&str> = cpage2["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["resource"]["id"].as_str().unwrap())
        .collect();
    assert!(
        p1_ids.iter().all(|i| !p2_ids.contains(i)),
        "{p1_ids:?} {p2_ids:?}"
    );
    assert_eq!(p2_ids.len(), 2);

    // Conditional delete: several matches → 412; narrowed → deletes.
    let (st, _, _) = send(
        &app,
        "DELETE",
        &format!("{b}/Patient?family=Pager"),
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
    let (st, _, _) = send(&app, "DELETE", &format!("{b}/Patient?_id=pg3"), None, &[]).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _, _) = send(&app, "GET", &format!("{b}/Patient/pg3"), None, &[]).await;
    assert_eq!(st, StatusCode::GONE);

    // poison transaction rolls everything back
    let (st, _, _) = send(
        &app,
        "POST",
        b,
        Some(json!({
            "resourceType": "Bundle", "type": "transaction",
            "entry": [
                {"resource": {"resourceType": "Patient", "name": [{"family": "Poisoned"}]},
                 "request": {"method": "POST", "url": "Patient"}},
                {"resource": {"resourceType": "Patient", "nonsenseElement": true},
                 "request": {"method": "POST", "url": "Patient"}}
            ]
        })),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, empty, _) = send(
        &app,
        "GET",
        &format!("{b}/Patient?family=Poisoned"),
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        empty["entry"].as_array().unwrap().len(),
        0,
        "rollback failed"
    );
}
