//! The fhirbase transformation algorithm.
//!
//! Ports `transform.go:16-195`. This rewrites a FHIR resource into the
//! representation fhirbase stores in the `resource` `jsonb` column, and it is
//! specified normatively in `spec/index.md` §4.
//!
//! The module is pure: no I/O, no database. That is deliberate — it is the
//! highest-risk part of the port, and keeping it pure means it is testable
//! against fhirbase's own test corpus without PostgreSQL, exactly as in Go.
//!
//! # What it does
//!
//! Two rewrites, everything else structural recursion:
//!
//! - **`union`** collapses a type-suffixed choice key into a tagged object:
//!   `{"deceasedBoolean": true}` becomes `{"deceased": {"boolean": true}}`.
//! - **`reference`** splits a relative reference:
//!   `{"reference": "Practitioner/1"}` becomes
//!   `{"resourceType": "Practitioner", "id": "1"}`.
//!
//! A resource whose type is absent from the map passes through untouched.
//!
//! # Fidelity
//!
//! Two behaviours look like bugs and are not. They are fhirbase's storage
//! model, asserted by its tests, and are reproduced deliberately:
//!
//! 1. The `reference` rewrite is **lossy** — it discards `identifier`, `type`,
//!    and extensions (spec §4.5).
//! 2. `tr/isCollection` is **never read**; arrays recurse with the same
//!    transform node, which already handles repeating fields (spec §4.7).

use serde_json::{Map, Value};

use crate::assets::{TrAct, TrNode, TransformMap};
use crate::error::{Error, Result};

/// Transforms a whole FHIR resource.
///
/// Ports `doTransform` (`transform.go:160-195`).
///
/// # Errors
///
/// Returns [`Error::Transform`] if the value is not a JSON object or has no
/// string `resourceType`.
///
/// # Examples
///
/// ```ignore
/// let map = FhirVersion::V4_0_0.transform_map()?;
/// let out = transform_resource(&json!({
///     "resourceType": "Patient",
///     "deceasedBoolean": true
/// }), map)?;
/// assert_eq!(out["deceased"], json!({"boolean": true}));
/// ```
pub fn transform_resource(resource: &Value, map: &TransformMap) -> Result<Value> {
    let object = resource
        .as_object()
        .ok_or_else(|| Error::transform("<unknown>", "resource is not a JSON object"))?;

    let resource_type = object
        .get("resourceType")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Error::transform("<unknown>", "resource has no string `resourceType` field")
        })?;

    // Spec §4.2 step 2: an unknown resource type passes through untouched.
    // fhirbase does the same (transform.go:173-178), and its test suite asserts
    // it — this is how data from a newer FHIR version survives an older map.
    let Some(node) = map.type_node(resource_type) else {
        return Ok(resource.clone());
    };

    Ok(transform_node(resource, Some(node), map))
}

/// The recursion of spec §4.3.
///
/// Ports `transform` (`transform.go:32-130`). `node` is `None` where fhirbase
/// passes a nil `trNode`, meaning "no rewrite applies here, recurse
/// structurally".
fn transform_node(value: &Value, node: Option<&TrNode>, map: &TransformMap) -> Value {
    // Spec §4.3: the action applies only when the value is NOT an array. An
    // array whose node carries an action falls through to the array branch and
    // each *element* receives the action. That is how repeating choice and
    // reference fields work, and why `tr/isCollection` is redundant.
    if let Some(TrNode::Act { act, .. }) = node
        && !value.is_array()
    {
        return apply_act(value, act, map);
    }

    match value {
        Value::Object(object) => transform_object(object, node, map),
        Value::Array(items) => Value::Array(
            items
                .iter()
                // Same node for every element — not a child node.
                .map(|item| transform_node(item, node, map))
                .collect(),
        ),
        // Scalars pass through.
        other => other.clone(),
    }
}

/// Rewrites an object's fields, renaming and redirecting per the transform node.
fn transform_object(object: &Map<String, Value>, node: Option<&TrNode>, map: &TransformMap) -> Value {
    let mut out = Map::with_capacity(object.len());

    for (field, value) in object {
        let Some(child) = node.and_then(|n| n.child(field)) else {
            // No rule for this field: recurse with no node, which deep-copies
            // while still transforming nothing (transform.go:110-113).
            out.insert(field.clone(), transform_node(value, None, map));
            continue;
        };

        // The output key comes from the child's `tr/arg.key` when it has one,
        // so `deceasedBoolean` is written as `deceased` (transform.go:93-102).
        let key = child.output_key(field).to_owned();

        // `tr/move` replaces the node wholesale before recursing
        // (transform.go:104-106). Move nodes carry no action and no children
        // upstream, so nothing is lost by the replacement.
        let effective = match child {
            TrNode::Move { path, .. } => map.resolve(path),
            other => Some(other),
        };

        let transformed = transform_node(value, effective, map);

        // Spec §4.6: two unions can target the same output key — FHIR forbids
        // it, but input is untrusted. Insertion order here is the object's key
        // order, so the last one wins, deterministically. fhirbase's result
        // depends on Go's randomized map iteration order.
        out.insert(key, transformed);
    }

    Value::Object(out)
}

/// Applies a `union` or `reference` action (spec §4.4, §4.5).
fn apply_act(value: &Value, act: &TrAct, map: &TransformMap) -> Value {
    match act {
        TrAct::Union { ty, .. } => {
            // Spec §4.4 step 2. `Reference` is special-cased ahead of the map
            // lookup, because the map's `Reference` entry describes a
            // Reference's *fields*, not the splitting rewrite
            // (transform.go:46-53).
            let inner = if ty == "Reference" {
                apply_act(value, &TrAct::Reference, map)
            } else if let Some(type_node) = map.type_node(ty) {
                transform_node(value, Some(type_node), map)
            } else {
                value.clone()
            };

            let mut wrapper = Map::with_capacity(1);
            wrapper.insert(ty.clone(), inner);
            Value::Object(wrapper)
        }
        TrAct::Reference => split_reference(value),
    }
}

/// Splits a FHIR `Reference` into `resourceType` and `id` (spec §4.5).
///
/// Ports `transform.go:59-81`.
///
/// **Lossy on purpose.** Only `reference` and `display` are consulted; every
/// other field of the `Reference` — `identifier`, `type`, extensions — is
/// discarded. This is fhirbase's storage model and its tests assert it.
fn split_reference(value: &Value) -> Value {
    let Some(object) = value.as_object() else {
        // fhirbase type-asserts here and would panic on a non-object. Spec
        // invariant 2 forbids that, and a non-object where a Reference belongs
        // is malformed input, so pass it through unchanged.
        return value.clone();
    };

    let mut out = Map::with_capacity(3);

    if let Some(reference) = object.get("reference").and_then(Value::as_str) {
        let parts: Vec<&str> = reference.split('/').collect();
        // Exactly two components means "ResourceType/id"; anything else —
        // including a contained "#ref" or an absolute URL — is kept whole as
        // the id.
        if let [resource_type, id] = parts.as_slice() {
            out.insert("id".to_owned(), Value::String((*id).to_owned()));
            out.insert(
                "resourceType".to_owned(),
                Value::String((*resource_type).to_owned()),
            );
        } else {
            out.insert("id".to_owned(), Value::String(reference.to_owned()));
        }
    }

    if let Some(display) = object.get("display").and_then(Value::as_str) {
        out.insert("display".to_owned(), Value::String(display.to_owned()));
    }

    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::FhirVersion;
    use serde_json::json;

    /// Transforms with the FHIR 3.0.1 map, which is what `transform_test.go`
    /// uses (`transform_test.go:96`).
    fn t(input: impl Into<Value>) -> Value {
        let input = input.into();
        let map = FhirVersion::V3_0_1.transform_map().unwrap();
        transform_resource(&input, map).unwrap()
    }

    // ---------------------------------------------------------------------
    // The five cases from fhirbase's transform_test.go, ported verbatim.
    // Comparison is on serde_json::Value, so key order is not significant
    // (spec invariant 4).
    // ---------------------------------------------------------------------

    #[test]
    fn go_case_0_careplan_references_and_nested_assigner() {
        let input = json!({
            "resourceType": "CarePlan",
            "careTeam": [
                {"reference": "Practitioner/1", "display": "John"},
                {"reference": "Practitioner/2", "display": "Ian"}
            ],
            "identifier": [
                {"system": "foo", "value": "bar"},
                {"system": "foo", "value": "baz",
                 "assigner": {"reference": "Practitioner/42", "display": "John Doe"}}
            ]
        });
        let expected = json!({
            "resourceType": "CarePlan",
            "careTeam": [
                {"id": "1", "resourceType": "Practitioner", "display": "John"},
                {"id": "2", "resourceType": "Practitioner", "display": "Ian"}
            ],
            "identifier": [
                {"system": "foo", "value": "bar"},
                {"system": "foo", "value": "baz",
                 "assigner": {"id": "42", "resourceType": "Practitioner", "display": "John Doe"}}
            ]
        });
        assert_eq!(t(input), expected);
    }

    #[test]
    fn go_case_1_claim_union_of_reference() {
        // Exercises spec §4.4 step 2: a union whose type is `Reference` gets
        // the splitting rewrite, not the map's `Reference` field rules.
        let input = json!({
            "resourceType": "Claim",
            "information": [{"valueReference": {"reference": "Immunization/123"}}]
        });
        let expected = json!({
            "resourceType": "Claim",
            "information": [
                {"value": {"Reference": {"resourceType": "Immunization", "id": "123"}}}
            ]
        });
        assert_eq!(t(input), expected);
    }

    #[test]
    fn go_case_2_patient_choices_and_reference() {
        let input = json!({
            "resourceType": "Patient",
            "name": [{"given": ["Mike"], "family": "Lapshin"}],
            "deceasedBoolean": true,
            "multipleBirthInteger": 2,
            "managingOrganization": {"reference": "Organization/1", "display": "ACME corp"}
        });
        let expected = json!({
            "resourceType": "Patient",
            "name": [{"given": ["Mike"], "family": "Lapshin"}],
            "deceased": {"boolean": true},
            "multipleBirth": {"integer": 2},
            "managingOrganization": {"id": "1", "resourceType": "Organization", "display": "ACME corp"}
        });
        assert_eq!(t(input), expected);
    }

    #[test]
    fn go_case_3_reference_with_only_display() {
        // A Reference with no `reference` field yields an object with just
        // `display` — no id, no resourceType.
        let input = json!({
            "resourceType": "Patient",
            "managingOrganization": {"display": "ACME corp"}
        });
        let expected = json!({
            "resourceType": "Patient",
            "managingOrganization": {"display": "ACME corp"}
        });
        assert_eq!(t(input), expected);
    }

    #[test]
    fn go_case_4_unknown_resource_type_passes_through() {
        let input = json!({"resourceType": "FoobarUnknown", "foo": 42});
        assert_eq!(t(input.clone()), input);
    }

    // ---------------------------------------------------------------------
    // Spec §4.5: the reference split, including the lossy part.
    // ---------------------------------------------------------------------

    #[test]
    fn reference_without_two_components_keeps_the_whole_string_as_id() {
        let input = json!({
            "resourceType": "Patient",
            "managingOrganization": {"reference": "urn:uuid:abc-123"}
        });
        assert_eq!(
            t(input)["managingOrganization"],
            json!({"id": "urn:uuid:abc-123"})
        );
    }

    #[test]
    fn absolute_reference_url_keeps_the_whole_string_as_id() {
        // Four components, not two, so it is not split.
        let input = json!({
            "resourceType": "Patient",
            "managingOrganization": {"reference": "http://example.com/fhir/Organization/1"}
        });
        assert_eq!(
            t(input)["managingOrganization"],
            json!({"id": "http://example.com/fhir/Organization/1"})
        );
    }

    #[test]
    fn reference_discards_every_other_field() {
        // Spec §4.5: lossy on purpose. `identifier`, `type`, and extensions do
        // not survive. If this test ever "fails" because someone preserved
        // them, read the spec before changing it.
        let input = json!({
            "resourceType": "Patient",
            "managingOrganization": {
                "reference": "Organization/1",
                "display": "ACME",
                "type": "Organization",
                "identifier": {"system": "sys", "value": "val"},
                "extension": [{"url": "http://example.com", "valueString": "x"}]
            }
        });
        assert_eq!(
            t(input)["managingOrganization"],
            json!({"id": "1", "resourceType": "Organization", "display": "ACME"})
        );
    }

    #[test]
    fn empty_reference_object_becomes_an_empty_object() {
        let input = json!({"resourceType": "Patient", "managingOrganization": {}});
        assert_eq!(t(input)["managingOrganization"], json!({}));
    }

    #[test]
    fn non_object_where_a_reference_belongs_passes_through() {
        // fhirbase type-asserts and would panic; spec invariant 2 forbids that.
        let input = json!({"resourceType": "Patient", "managingOrganization": "nonsense"});
        assert_eq!(t(input)["managingOrganization"], json!("nonsense"));
    }

    // ---------------------------------------------------------------------
    // Spec §4.3 array handling and §4.7 tr/isCollection.
    // ---------------------------------------------------------------------

    #[test]
    fn repeating_reference_fields_transform_element_wise() {
        // `generalPractitioner` is a repeating reference: the action node is
        // reached with an array value, falls through to the array branch, and
        // each element gets the split.
        let input = json!({
            "resourceType": "Patient",
            "generalPractitioner": [
                {"reference": "Practitioner/a"},
                {"reference": "Organization/b", "display": "Clinic"}
            ]
        });
        assert_eq!(
            t(input)["generalPractitioner"],
            json!([
                {"id": "a", "resourceType": "Practitioner"},
                {"id": "b", "resourceType": "Organization", "display": "Clinic"}
            ])
        );
    }

    #[test]
    fn nested_arrays_recurse_with_the_same_node() {
        let input = json!({
            "resourceType": "Patient",
            "generalPractitioner": [[{"reference": "Practitioner/a"}]]
        });
        assert_eq!(
            t(input)["generalPractitioner"],
            json!([[{"id": "a", "resourceType": "Practitioner"}]])
        );
    }

    // ---------------------------------------------------------------------
    // Spec §4.6 determinism.
    // ---------------------------------------------------------------------

    #[test]
    fn colliding_union_keys_are_deterministic() {
        // Illegal FHIR — a choice element may hold only one variant — but input
        // is untrusted. fhirbase's outcome depends on Go's randomized map
        // iteration; ours must not vary.
        let input = json!({
            "resourceType": "Patient",
            "deceasedBoolean": true,
            "deceasedDateTime": "2020-01-01"
        });
        let first = t(input.clone());
        for _ in 0..100 {
            assert_eq!(t(input.clone()), first, "spec §4.6: output must be stable");
        }
        // serde_json preserves insertion order, and both write `deceased`, so
        // the later key in the input wins.
        assert_eq!(first["deceased"], json!({"dateTime": "2020-01-01"}));
    }

    // ---------------------------------------------------------------------
    // Spec §4.2 entry conditions.
    // ---------------------------------------------------------------------

    #[test]
    fn resource_without_a_resource_type_is_an_error() {
        let map = FhirVersion::V3_0_1.transform_map().unwrap();
        let err = transform_resource(&json!({"foo": 1}), map).unwrap_err();
        assert!(err.to_string().contains("resourceType"), "{err}");
    }

    #[test]
    fn non_object_resource_is_an_error() {
        let map = FhirVersion::V3_0_1.transform_map().unwrap();
        for value in [json!([]), json!("x"), json!(1), json!(null)] {
            assert!(transform_resource(&value, map).is_err(), "{value}");
        }
    }

    #[test]
    fn non_string_resource_type_is_an_error() {
        let map = FhirVersion::V3_0_1.transform_map().unwrap();
        assert!(transform_resource(&json!({"resourceType": 42}), map).is_err());
    }

    // ---------------------------------------------------------------------
    // Cross-version sanity.
    // ---------------------------------------------------------------------

    #[test]
    fn every_version_transforms_a_patient() {
        let input = json!({
            "resourceType": "Patient",
            "deceasedBoolean": true,
            "managingOrganization": {"reference": "Organization/1"}
        });
        for &version in crate::assets::ALL_VERSIONS {
            let map = version.transform_map().unwrap();
            let out = transform_resource(&input, map).unwrap();
            assert_eq!(
                out["deceased"],
                json!({"boolean": true}),
                "FHIR {version} should collapse deceasedBoolean"
            );
            assert_eq!(
                out["managingOrganization"],
                json!({"id": "1", "resourceType": "Organization"}),
                "FHIR {version} should split managingOrganization"
            );
        }
    }

    #[test]
    fn scalars_and_nulls_survive_untouched() {
        let input = json!({
            "resourceType": "Patient",
            "active": true,
            "birthDate": "1970-01-01",
            "extension": null,
            "count": 3
        });
        let out = t(input);
        assert_eq!(out["active"], json!(true));
        assert_eq!(out["birthDate"], json!("1970-01-01"));
        assert_eq!(out["extension"], json!(null));
        assert_eq!(out["count"], json!(3));
    }

    #[test]
    fn deeply_nested_unknown_fields_are_preserved() {
        let input = json!({
            "resourceType": "Patient",
            "unknownField": {"a": {"b": {"c": [1, 2, {"d": "e"}]}}}
        });
        assert_eq!(
            t(input)["unknownField"],
            json!({"a": {"b": {"c": [1, 2, {"d": "e"}]}}})
        );
    }

    #[test]
    fn union_over_an_array_wraps_each_element() {
        // Found by proptest, which shrank to `[]`; the expected values below
        // were then taken from fhirbase itself, not from reasoning. Spec §4.3:
        // the action applies only to a non-array, so an array falls through to
        // the array branch and each element is wrapped — while the rename to
        // `deceased` still happens, because that comes from the parent.
        assert_eq!(
            t(json!({"resourceType": "Patient", "deceasedBoolean": []}))["deceased"],
            json!([])
        );
        assert_eq!(
            t(json!({"resourceType": "Patient", "deceasedBoolean": [true, false]}))["deceased"],
            json!([{"boolean": true}, {"boolean": false}])
        );
        assert_eq!(
            t(json!({"resourceType": "Patient", "deceasedBoolean": [[true]]}))["deceased"],
            json!([[{"boolean": true}]])
        );
    }

    #[test]
    fn r5_codeable_reference_splits_its_nested_reference() {
        // `CodeableReference` is R5's most consequential new datatype and the
        // one the R3/R4 oracle cannot exercise (task T22). It pairs a
        // CodeableConcept with a Reference, so the reference must split while
        // the concept passes through — here reached through a repeating
        // BackboneElement, a repeating CodeableReference, and then the
        // Reference itself.
        let map = FhirVersion::V5_0_0.transform_map().unwrap();
        let input = json!({
            "resourceType": "Encounter",
            "id": "e1",
            "reason": [{"value": [{
                "concept": {"text": "chest pain"},
                "reference": {"reference": "Condition/c1"}
            }]}]
        });
        let out = transform_resource(&input, map).unwrap();
        let value = &out["reason"][0]["value"][0];

        assert_eq!(value["concept"], json!({"text": "chest pain"}));
        assert_eq!(
            value["reference"],
            json!({"id": "c1", "resourceType": "Condition"})
        );
    }

    #[test]
    fn r5_transforms_the_shapes_r4_also_has() {
        // The generated R5 map must behave like the vendored ones on the
        // constructs they share, or the generator has diverged in a way the
        // oracle would not catch.
        let map = FhirVersion::V5_0_0.transform_map().unwrap();
        let out = transform_resource(
            &json!({
                "resourceType": "Patient",
                "deceasedBoolean": true,
                "managingOrganization": {"reference": "Organization/9"},
                "generalPractitioner": [{"reference": "Practitioner/a"}]
            }),
            map,
        )
        .unwrap();

        assert_eq!(out["deceased"], json!({"boolean": true}));
        assert_eq!(
            out["managingOrganization"],
            json!({"id": "9", "resourceType": "Organization"})
        );
        assert_eq!(
            out["generalPractitioner"],
            json!([{"id": "a", "resourceType": "Practitioner"}])
        );
    }

    #[test]
    fn a_resource_r5_added_is_known_only_to_r5() {
        // Transport is new in R5; an R4 map must pass it through untouched
        // while the R5 map rewrites its references.
        let input = json!({
            "resourceType": "Transport",
            "id": "t1",
            "requester": {"reference": "Practitioner/p"}
        });

        let r4 = FhirVersion::V4_0_0.transform_map().unwrap();
        assert_eq!(
            transform_resource(&input, r4).unwrap(),
            input,
            "an unknown resource type passes through (spec §4.2)"
        );

        let r5 = FhirVersion::V5_0_0.transform_map().unwrap();
        assert_eq!(
            transform_resource(&input, r5).unwrap()["requester"],
            json!({"id": "p", "resourceType": "Practitioner"})
        );
    }

    #[test]
    fn the_transformation_is_not_idempotent() {
        // Worth pinning, because it is a natural thing to assume and it is
        // false. `managingOrganization` keeps its `reference` rule, so a second
        // pass re-applies the split to an already-split value: there is no
        // `reference` field left, so `id` and `resourceType` are dropped.
        //
        // This is inherent to fhirbase's design — the map describes input
        // shape, not output shape — so it is a property of the storage model,
        // not a defect. It matters operationally: never re-transform a resource
        // read back out of the database.
        let map = FhirVersion::V3_0_1.transform_map().unwrap();
        let once = transform_resource(
            &json!({
                "resourceType": "Patient",
                "managingOrganization": {"reference": "Organization/1", "display": "ACME"}
            }),
            map,
        )
        .unwrap();
        assert_eq!(
            once["managingOrganization"],
            json!({"id": "1", "resourceType": "Organization", "display": "ACME"})
        );

        let twice = transform_resource(&once, map).unwrap();
        assert_eq!(
            twice["managingOrganization"],
            json!({"display": "ACME"}),
            "a second pass drops id and resourceType"
        );
    }
}

/// Property-based tests for spec §4.8.
///
/// The example tests above prove the algorithm agrees with fhirbase on cases we
/// thought of. These cover the cases we did not: arbitrary, hostile, deeply
/// nested JSON that no FHIR server would ever emit.
///
/// The case count defaults to proptest's own and is raised in CI through
/// `PROPTEST_CASES`, so the local edit-test loop stays fast while CI runs the
/// 10,000 cases spec §4.8 asks for.
#[cfg(test)]
mod property_tests {
    use super::*;
    use crate::assets::FhirVersion;
    use proptest::prelude::*;
    use serde_json::json;

    /// Arbitrary JSON: nulls, bools, numbers, strings, arrays, objects, nested.
    fn arb_json() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|i| Value::Number(i.into())),
            // Include the characters that break naive string handling.
            "[a-zA-Z0-9 /\"\\\\#:.-]{0,12}".prop_map(Value::String),
        ];
        leaf.prop_recursive(4, 48, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                prop::collection::btree_map("[a-zA-Z][a-zA-Z0-9]{0,6}", inner, 0..4)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    /// An arbitrary JSON object, for use as a resource body.
    fn arb_object() -> impl Strategy<Value = serde_json::Map<String, Value>> {
        prop::collection::btree_map("[a-zA-Z][a-zA-Z0-9]{0,6}", arb_json(), 0..6)
            .prop_map(|m| m.into_iter().collect())
    }

    fn map_v4() -> &'static TransformMap {
        FhirVersion::V4_0_0.transform_map().unwrap()
    }

    /// Asserts that `node` is `original` with every non-array leaf wrapped as
    /// `{"boolean": leaf}`, recursing through arrays.
    ///
    /// `Result` is fully qualified because the bare name resolves to the
    /// crate's single-parameter alias, pulled in by `use super::*`.
    fn assert_wrapped(
        node: &Value,
        original: &Value,
    ) -> std::result::Result<(), TestCaseError> {
        match (node, original) {
            (Value::Array(got), Value::Array(want)) => {
                prop_assert_eq!(got.len(), want.len());
                for (g, w) in got.iter().zip(want) {
                    assert_wrapped(g, w)?;
                }
                Ok(())
            }
            (node, original) => {
                let object = node.as_object();
                prop_assert!(object.is_some(), "expected a wrapper object, got {}", node);
                let object = object.unwrap();
                prop_assert_eq!(object.len(), 1);
                // `boolean` is a primitive with no map entry, so the value
                // passes through untouched however hostile it is.
                prop_assert_eq!(&object["boolean"], original);
                Ok(())
            }
        }
    }

    proptest! {
        /// Spec §4.2 step 2. This is the property that lets data from a newer
        /// FHIR version survive an older map unchanged.
        #[test]
        fn unknown_resource_type_is_the_identity(mut body in arb_object()) {
            body.insert(
                "resourceType".to_owned(),
                json!("NoSuchResourceTypeExistsAnywhere"),
            );
            let input = Value::Object(body);
            prop_assert_eq!(transform_resource(&input, map_v4()).unwrap(), input);
        }

        /// Spec invariant 2. Any panic anywhere in the algorithm fails here.
        #[test]
        fn arbitrary_input_never_panics(mut body in arb_object()) {
            for resource_type in ["Patient", "Observation", "Bundle", "Group", "Claim"] {
                body.insert("resourceType".to_owned(), json!(resource_type));
                let input = Value::Object(body.clone());
                // Must return, not unwind, whatever the body looks like.
                let out = transform_resource(&input, map_v4());
                prop_assert!(out.is_ok());
                prop_assert!(out.unwrap().is_object());
            }
        }

        /// A malformed value where a Reference belongs must not panic — this is
        /// the exact site of fhirbase's unchecked type assertion.
        #[test]
        fn arbitrary_reference_position_never_panics(value in arb_json()) {
            let input = json!({
                "resourceType": "Patient",
                "managingOrganization": value,
            });
            prop_assert!(transform_resource(&input, map_v4()).is_ok());
        }

        /// Spec §4.5: the reference rewrite emits nothing but these three keys.
        #[test]
        fn reference_output_has_only_the_three_allowed_keys(body in arb_object()) {
            let input = json!({
                "resourceType": "Patient",
                "managingOrganization": Value::Object(body),
            });
            let out = transform_resource(&input, map_v4()).unwrap();
            let org = out["managingOrganization"].as_object().unwrap();
            for key in org.keys() {
                prop_assert!(
                    matches!(key.as_str(), "id" | "resourceType" | "display"),
                    "reference rewrite emitted an unexpected key: {}",
                    key
                );
            }
        }

        /// Spec §4.4 and §4.3: a union wraps a non-array value in a single-key
        /// object tagged by the declared type. An **array** value falls through
        /// to the array branch instead and each element is wrapped, so the
        /// result is an array — the rename to `deceased` still happens either
        /// way.
        ///
        /// The array half of this property was discovered by proptest, which
        /// shrank to `[]` against an earlier version of this test that assumed
        /// the output was always an object. The behaviour was then confirmed
        /// against fhirbase itself, and the three shapes are pinned in
        /// `union_over_an_array_wraps_each_element` and in the fidelity corpus.
        #[test]
        fn union_output_is_wrapped_by_declared_type(value in arb_json()) {
            let input = json!({
                "resourceType": "Patient",
                "deceasedBoolean": value.clone(),
            });
            let out = transform_resource(&input, map_v4()).unwrap();
            assert_wrapped(&out["deceased"], &value)?;
        }

        /// `resourceType` and `id` are never rewritten by any of the nine maps
        /// — verified across every asset before this was asserted — so they
        /// must survive transformation intact.
        #[test]
        fn resource_type_and_id_survive(mut body in arb_object(), id in "[a-zA-Z0-9-]{1,12}") {
            body.insert("resourceType".to_owned(), json!("Patient"));
            body.insert("id".to_owned(), json!(id.clone()));
            let out = transform_resource(&Value::Object(body), map_v4()).unwrap();
            prop_assert_eq!(&out["resourceType"], &json!("Patient"));
            prop_assert_eq!(&out["id"], &json!(id));
        }

        /// Output is always serializable, so the load path can never produce a
        /// value it cannot hand to PostgreSQL.
        #[test]
        fn output_always_serializes(mut body in arb_object()) {
            body.insert("resourceType".to_owned(), json!("Observation"));
            let out = transform_resource(&Value::Object(body), map_v4()).unwrap();
            prop_assert!(serde_json::to_string(&out).is_ok());
        }

        /// Every version behaves consistently on the same arbitrary input.
        #[test]
        fn no_version_panics_on_arbitrary_input(mut body in arb_object()) {
            body.insert("resourceType".to_owned(), json!("Patient"));
            let input = Value::Object(body);
            for &version in crate::assets::ALL_VERSIONS {
                let map = version.transform_map().unwrap();
                prop_assert!(transform_resource(&input, map).is_ok());
            }
        }
    }
}
