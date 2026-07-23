# Validating the transform-map generator

Task T21. Decision D5 chose "generate the R5 assets once and vendor them"
partly because there was no oracle to check a generator against: fhirbase stops
at FHIR 4.0.0, so anything generated for 5.0.0 could only be hand-inspected.

That turned out to be wrong, in a useful direction.

## There is an oracle after all

`~/git/joelparkerhenderson/fhir-rust-crate` has grown multi-release support
since the plan was written. At v1.1.0 it ships the official FHIR
StructureDefinitions for **R3 (3.0.2), R4 (4.0.1), and R5 (5.0.0)** under
`doc/fhir-specifications/`.

fhirbase generated its maps from the same StructureDefinitions. So a generator
can be pointed at R3 and R4 and its output compared, node by node, against
fhirbase's own vendored assets for 3.0.1 and 4.0.0. If it reproduces those, the
same code applied to R5 is trustworthy for the same reasons.

This also removes risk R5. The generator reads the spec JSON directly rather
than calling `fhir::r5::meta`, so `fhir` is not a dependency of anything —
nothing blocks `cargo publish`. The spec JSON is also available from HL7
directly, so the generator is not tied to that checkout either.

## Result

| Release | Generated from | Compared against | Shared nodes | Identical |
| --- | --- | --- | ---: | ---: |
| R3 | spec 3.0.2 | fhirbase 3.0.1 | 126 | **126 (100%)** |
| R4 | spec 4.0.1 | fhirbase 4.0.0 | 155 | **151 (97%)** |

Every difference is accounted for.

### The four differing R4 nodes

`ElementDefinition`, `Parameters`, `StructureMap`, and `Task` differ by exactly
one thing each: a `union` entry for the choice type `Meta`
(`Task.input.valueMeta`, `ElementDefinition.fixedMeta`, and so on).

This is a **specification change, not a generator defect**. FHIR's open type
list gained `Meta` between 4.0.0 and 4.0.1:

```
Task.input.value[x]   fhirbase 4.0.0 asset : 49 types
                      spec 4.0.1           : 50 types
                      difference           : Meta, added
```

The generator was given 4.0.1 because that is the R4 the crate ships. Given
4.0.0 it would produce fhirbase's asset exactly.

### The four extra top-level entries

`Bundle`, `Extension`, `SearchParameter`, and `TestScript` appear in the
generated map and not in fhirbase's. None of them has a table in fhirbase's
4.0.0 schema — they are not storable resources there, so its generator left
them out of the map too.

Extra entries are harmless: a map entry is only ever consulted for a resource
type being stored, or as a `tr/move` target. The Rust generator should
nonetheless filter to the schema's resource set plus reachable datatypes, so
the two assets agree on what exists.

## The rules, as validated

Derived by analyzing the 4.0.0 asset, then confirmed by reproducing 3.0.1
exactly:

1. A choice element `f[x]` with types `C1..Cn` emits `fC1..fCn`, each
   `{tr/act: union, tr/arg: {key: f, type: Ci}}`.
2. …and, if any `Ci` has a non-empty node, a collapsed `f` node whose children
   are those `Ci`. `Reference` is special: its collapsed child is
   `{tr/act: reference}`, not the `Reference` type's own node — which mirrors
   the transformation's own special case (spec §4.4 step 2).
3. A non-choice element of type `Reference` emits `{tr/act: reference}`.
4. An element of a complex datatype with a non-empty node emits
   `{tr/move: [TypeName]}`.
5. A `BackboneElement` emits its children inline.
6. `contentReference` — FHIR's recursion marker — emits
   `{tr/move: [path, segments]}`, **but only when the target carries rules**.
   `QuestionnaireResponse.item.item` is kept because
   `QuestionnaireResponse.item` has rules; `CapabilityStatement.rest.operation`
   is dropped because `CapabilityStatement.rest.resource.operation` has none.
   Removing one can empty its parent, so pruning runs to a fixpoint.
7. `max` other than `"1"` adds `tr/isCollection: true` — emitted for fidelity,
   never read (spec §4.7).
8. The Element, Resource, and DomainResource base fields — `id`, `extension`,
   `modifierExtension`, `meta`, `implicitRules`, `language`, `text`,
   `contained` — are excluded entirely, even though `Extension` and `Meta` both
   carry rules of their own.
9. Empty nodes are pruned at every level.

Rules 6 and 8 are the two that are not obvious from reading fhirbase's Go, and
neither would have been found without the diff. Rule 8 accounted for 147 of the
155 initial mismatches; rule 6 for the remaining 51 sub-node differences.

## R5 output

Applying the same rules to the 5.0.0 spec yields 175 top-level entries:

| Directive | R4 (fhirbase 4.0.0) | R5 (generated) |
| --- | ---: | ---: |
| `union` | 929 | 1,359 |
| `reference` | 737 | 847 |
| `tr/move` | 381 | 688 |

The growth is consistent with R5's larger resource set and its wider use of
`CodeableReference`.

## Status

`gen_transform_prototype.py` is the prototype that established the rules. It is
kept as the reference for the Rust `xtask`, and as the thing to re-run if the
Rust port's output ever needs explaining.

**Still to do:** the Rust `xtask` port, the schema-DDL generator, the
hand-verification of ten resource types (T22), and flipping the default to
5.0.0 (T23).
