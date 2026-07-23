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
}
