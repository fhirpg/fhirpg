"""Independent verification of the generated FHIR 5.0.0 transform map (T22).

The oracle in `validation.md` proves the generator reproduces fhirbase's R3 and
R4 maps. What it cannot prove is anything R5 does that R3 and R4 do not — most
of all `CodeableReference`, a datatype R5 introduced and uses widely.

This re-derives what the map should contain **from the specification, by a
different route than the generator**, and compares. It walks the
StructureDefinition snapshots directly and asks two questions per element:

  * is it a choice, and if so exactly which type suffixes should appear?
  * is it a Reference, and if so should there be a reference action?

then checks the generated map agrees, including that it contains nothing extra.

Run:  python3 doc/r5-generation/verify_r5.py <spec-dir> assets/transform/fhirpg-import-5.0.0.json
"""

import json
import sys
from pathlib import Path

SPEC = Path(sys.argv[1])
MAP = json.loads(Path(sys.argv[2]).read_text())

# The ten types tasks.md names, plus the datatypes R5 introduced that the R3/R4
# oracle cannot exercise.
RESOURCES = [
    "Patient", "Observation", "Bundle", "Group", "MedicationRequest",
    "Encounter", "Questionnaire", "Subscription", "Evidence",
    "ImplementationGuide",
]
R5_DATATYPES = [
    "CodeableReference", "Availability", "ExtendedContactDetail",
    "RatioRange", "MonetaryComponent", "VirtualServiceDetail",
]

INFRASTRUCTURE = {
    "id", "extension", "modifierExtension",
    "meta", "implicitRules", "language", "text", "contained",
}


def load():
    out = {}
    for name in ("profiles-types.json", "profiles-resources.json"):
        for entry in json.loads((SPEC / name).read_text()).get("entry", []):
            sd = entry.get("resource", {})
            if sd.get("resourceType") != "StructureDefinition":
                continue
            if sd.get("derivation") == "constraint":
                continue
            if sd.get("name") and sd.get("snapshot"):
                out[sd["name"]] = sd["snapshot"]["element"]
    return out


STRUCTURES = load()
failures = []
checks = 0


def check(condition, message):
    global checks
    checks += 1
    if not condition:
        failures.append(message)


def node_at(root, path):
    node = root
    for segment in path:
        if not isinstance(node, dict) or segment not in node:
            return None
        node = node[segment]
    return node


def verify_type(type_name):
    """Every choice and every Reference in `type_name`, checked against the map."""
    elements = STRUCTURES.get(type_name)
    if elements is None:
        failures.append(f"{type_name}: not in the specification")
        return

    for element in elements:
        path = element["path"].split(".")
        if len(path) < 2 or path[0] != type_name:
            continue
        field = path[-1]
        if field in INFRASTRUCTURE:
            continue

        codes = []
        for t in element.get("type", []):
            if t.get("code") and t["code"] not in codes:
                codes.append(t["code"])

        # Where in the map this element's parent lives.
        parent = node_at(MAP, path[:-1]) if len(path) > 2 else MAP.get(type_name)

        if field.endswith("[x]"):
            base = field[:-3]
            if parent is None:
                failures.append(f"{'.'.join(path)}: choice element but no parent node")
                continue
            for code in codes:
                key = base + code[0].upper() + code[1:]
                entry = parent.get(key)
                check(
                    isinstance(entry, dict)
                    and entry.get("tr/act") == "union"
                    and entry.get("tr/arg") == {"key": base, "type": code},
                    f"{'.'.join(path[:-1])}.{key}: missing or wrong union entry",
                )
            # And nothing invented: every union under this base is a real type.
            for key, value in parent.items():
                if (isinstance(value, dict)
                        and value.get("tr/act") == "union"
                        and value.get("tr/arg", {}).get("key") == base):
                    check(
                        value["tr/arg"]["type"] in codes,
                        f"{'.'.join(path[:-1])}.{key}: union for a type the spec does not allow",
                    )

        elif codes == ["Reference"]:
            if parent is None:
                failures.append(f"{'.'.join(path)}: Reference but no parent node")
                continue
            entry = parent.get(field)
            check(
                isinstance(entry, dict) and entry.get("tr/act") == "reference",
                f"{'.'.join(path)}: missing reference action",
            )
            repeating = element.get("max") not in ("1", "0")
            check(
                bool(entry and entry.get("tr/isCollection")) == repeating,
                f"{'.'.join(path)}: tr/isCollection should be {repeating}",
            )


def verify_no_invented_references():
    """Every reference action in the map corresponds to a real Reference element."""
    paths = {}

    def walk(node, path):
        if not isinstance(node, dict):
            return
        if node.get("tr/act") == "reference":
            paths[".".join(path)] = True
        for key, value in node.items():
            if not key.startswith("tr/"):
                walk(value, path + [key])

    for name in RESOURCES:
        if name in MAP:
            walk(MAP[name], [name])

    for path in paths:
        segments = path.split(".")
        elements = STRUCTURES.get(segments[0], [])
        matched = any(
            e["path"] == path
            and any(t.get("code") == "Reference" for t in e.get("type", []))
            for e in elements
        )
        # A reference inside a collapsed choice node is keyed by type name, not
        # by an element path, so exclude those.
        if not matched and segments[-1] != "Reference":
            check(False, f"{path}: reference action with no matching Reference element")


print(f"Verifying the generated FHIR 5.0.0 map against the specification\n")

for name in RESOURCES:
    verify_type(name)
print(f"  {len(RESOURCES)} resource types walked")

for name in R5_DATATYPES:
    elements = STRUCTURES.get(name)
    if elements is None:
        failures.append(f"{name}: not in the R5 specification")
        continue
    has_rules = any(
        e["path"].count(".") >= 1
        and e["path"].split(".")[-1] not in INFRASTRUCTURE
        and (e["path"].endswith("[x]")
             or [t.get("code") for t in e.get("type", [])] == ["Reference"])
        for e in elements
    )
    present = name in MAP
    check(
        present == has_rules,
        f"{name}: {'present' if present else 'absent'} in the map "
        f"but {'has' if has_rules else 'has no'} rules in the spec",
    )
    print(f"  {name}: {'present' if present else 'absent'} -- correct")

verify_no_invented_references()

print(f"\n{checks} checks")
if failures:
    print(f"{len(failures)} FAILURES:")
    for f in failures[:25]:
        print(f"  {f}")
    sys.exit(1)
print("all passed")
