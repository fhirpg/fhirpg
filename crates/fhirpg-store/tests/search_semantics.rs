//! Search semantics against live PostgreSQL: token, string, date (incl.
//! precision edges and Periods), reference, number, quantity, _id, and
//! unsupported-parameter behavior. Gated on FHIRPG_TEST_DB.

use std::path::PathBuf;
use std::sync::Arc;

use fhirpg_store::Store;
use serde_json::json;

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

async fn test_store() -> Option<Store> {
    let db = std::env::var("FHIRPG_TEST_DB").ok()?;
    let defs = spec_defs()?;
    // SAFETY: set before concurrent access matters in this test binary.
    unsafe { std::env::set_var("PGDATABASE", &db) };
    let map = Arc::new(fhirpg_gen::generate(&defs, "searchtest").expect("generate"));
    let cfg = fhirpg_store::pg_config(None).expect("cfg");
    let store = Store::connect(cfg, map).await.expect("connect");
    store.drop_schema().await.expect("drop");
    store.init("search-checksum").await.expect("init");
    Some(store)
}

async fn ids(store: &Store, rtype: &str, params: &[(&str, &str)]) -> Vec<String> {
    let p: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    store.search(rtype, &p, 100, 0).await.expect("search")
}

#[tokio::test]
async fn search_semantics() {
    let Some(store) = test_store().await else {
        eprintln!("skipping: FHIRPG_TEST_DB not set or spec missing");
        return;
    };

    // Fixtures.
    store
        .put(&json!({
            "resourceType": "Patient", "id": "alice",
            "active": true, "gender": "female", "birthDate": "1980-03-15",
            "name": [{"family": "Smith", "given": ["Alice", "Beth"]}],
            "identifier": [{"system": "http://acme.org/mrn", "value": "MRN-1"}],
            "managingOrganization": {"reference": "Organization/acme"}
        }))
        .await
        .expect("alice");
    store
        .put(&json!({
            "resourceType": "Patient", "id": "bob",
            "active": false, "gender": "male", "birthDate": "1980-11",
            "name": [{"family": "Smithson"}],
            "identifier": [{"system": "http://other.org", "value": "MRN-1"}]
        }))
        .await
        .expect("bob");
    store
        .put(&json!({
            "resourceType": "Patient", "id": "carol",
            "gender": "female", "birthDate": "1994-06-01",
            "name": [{"family": "Jones", "given": ["Carol"]}]
        }))
        .await
        .expect("carol");
    store
        .put(&json!({
            "resourceType": "Observation", "id": "obs1", "status": "final",
            "code": {"coding": [{"system": "http://loinc.org", "code": "8480-6"}]},
            "subject": {"reference": "Patient/alice"},
            "effectivePeriod": {"start": "2026-01-01", "end": "2026-03-01"},
            "valueQuantity": {"value": 120.5, "system": "http://unitsofmeasure.org", "code": "mm[Hg]"}
        }))
        .await
        .expect("obs1");
    store
        .put(&json!({
            "resourceType": "Observation", "id": "obs2", "status": "preliminary",
            "code": {"coding": [{"system": "http://loinc.org", "code": "9279-1"}]},
            "subject": {"reference": "Patient/bob"},
            "effectiveDateTime": "2026-06-15T10:30:00Z",
            "valueQuantity": {"value": 16, "system": "http://unitsofmeasure.org", "code": "/min"}
        }))
        .await
        .expect("obs2");

    // Token on primitive (code + boolean).
    assert_eq!(
        ids(&store, "Patient", &[("gender", "female")]).await,
        ["alice", "carol"]
    );
    assert_eq!(
        ids(&store, "Patient", &[("active", "true")]).await,
        ["alice"]
    );
    // Token system|value on Identifier.
    assert_eq!(
        ids(
            &store,
            "Patient",
            &[("identifier", "http://acme.org/mrn|MRN-1")]
        )
        .await,
        ["alice"]
    );
    assert_eq!(
        ids(&store, "Patient", &[("identifier", "MRN-1")]).await,
        ["alice", "bob"]
    );
    // Token on CodeableConcept coding.
    assert_eq!(
        ids(
            &store,
            "Observation",
            &[("code", "http://loinc.org|8480-6")]
        )
        .await,
        ["obs1"]
    );
    // OR values.
    assert_eq!(
        ids(&store, "Observation", &[("code", "8480-6,9279-1")]).await,
        ["obs1", "obs2"]
    );

    // String: prefix (default), exact, contains; multi-part name.
    assert_eq!(
        ids(&store, "Patient", &[("family", "Smith")]).await,
        ["alice", "bob"]
    );
    assert_eq!(
        ids(&store, "Patient", &[("family:exact", "Smith")]).await,
        ["alice"]
    );
    assert_eq!(ids(&store, "Patient", &[("name", "beth")]).await, ["alice"]);
    assert_eq!(
        ids(&store, "Patient", &[("name:contains", "ones")]).await,
        ["carol"]
    );

    // Date precision: partial birthDate "1980-11" is inside 1980.
    assert_eq!(
        ids(&store, "Patient", &[("birthdate", "1980")]).await,
        ["alice", "bob"]
    );
    assert_eq!(
        ids(&store, "Patient", &[("birthdate", "1980-03")]).await,
        ["alice"]
    );
    assert_eq!(
        ids(&store, "Patient", &[("birthdate", "ge1980-06")]).await,
        ["bob", "carol"]
    );
    assert_eq!(
        ids(&store, "Patient", &[("birthdate", "lt1980-06")]).await,
        ["alice"]
    );
    // AND across parameters.
    assert_eq!(
        ids(
            &store,
            "Patient",
            &[("gender", "female"), ("birthdate", "ge1990")]
        )
        .await,
        ["carol"]
    );

    // Date on choice effective[x]: Period overlap and dateTime point.
    assert_eq!(
        ids(&store, "Observation", &[("date", "2026-02")]).await,
        ["obs1"]
    );
    assert_eq!(
        ids(&store, "Observation", &[("date", "2026-06-15")]).await,
        ["obs2"]
    );
    assert_eq!(
        ids(&store, "Observation", &[("date", "ge2026-01")]).await,
        ["obs1", "obs2"]
    );

    // Reference.
    assert_eq!(
        ids(&store, "Observation", &[("subject", "Patient/alice")]).await,
        ["obs1"]
    );
    assert_eq!(
        ids(&store, "Patient", &[("organization", "Organization/acme")]).await,
        ["alice"]
    );

    // Quantity.
    assert_eq!(
        ids(
            &store,
            "Observation",
            &[("value-quantity", "120.5|http://unitsofmeasure.org|mm[Hg]")]
        )
        .await,
        ["obs1"]
    );
    assert_eq!(
        ids(&store, "Observation", &[("value-quantity", "gt100")]).await,
        ["obs1"]
    );

    // _id and paging.
    assert_eq!(
        ids(&store, "Patient", &[("_id", "bob,carol")]).await,
        ["bob", "carol"]
    );
    let page: Vec<String> = store.search("Patient", &[], 2, 1).await.expect("page");
    assert_eq!(page, ["bob", "carol"]);

    // Unsupported parameter errors (strict semantics at the store level).
    let err = store
        .search(
            "Patient",
            &[("deceased".to_string(), "true".to_string())],
            10,
            0,
        )
        .await
        .expect_err("deceased is uncompiled (exists() expression)");
    assert!(err.to_string().contains("not supported"), "{err}");
    let err = store
        .search("Patient", &[("nope".to_string(), "x".to_string())], 10, 0)
        .await
        .expect_err("unknown param");
    assert!(err.to_string().contains("unsupported"), "{err}");
}
