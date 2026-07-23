"""Prototype of the fhirpg transform-map generator.

Proves the generation rules against fhirbase's vendored FHIR 4.0.0 asset before
any of it is written in Rust. Reads the official FHIR StructureDefinitions --
the same source fhirbase generated from -- and emits the same map shape.

Rules, derived by analyzing the 4.0.0 asset:

  * a choice element `f[x]` with types C1..Cn emits `f<C1>..f<Cn>`, each
    {tr/act: union, tr/arg: {key: f, type: Ci}}
  * ...and, if any Ci has a non-empty node of its own, a collapsed `f` node
    whose children are those Ci. Reference is special: its collapsed child is
    {tr/act: reference}, not the Reference type's node.
  * a non-choice Reference element emits {tr/act: reference}
  * an element of a complex datatype with a non-empty node emits
    {tr/move: [TypeName]}
  * a BackboneElement emits its own children inline
  * contentReference (recursion) emits {tr/move: [path, segments]}
  * max != "1" adds tr/isCollection: true
  * empty nodes are pruned, at every level
"""

import json
import sys
from pathlib import Path

SPEC = Path(sys.argv[1])
OUT = Path(sys.argv[2])

# Element, Resource and DomainResource base fields. fhirbase's map omits these
# entirely -- Patient has no `extension`, `meta` or `text` entry even though
# Extension and Meta both carry rules of their own.
INFRASTRUCTURE = {
    "id", "extension", "modifierExtension",       # Element / BackboneElement
    "meta", "implicitRules", "language",          # Resource
    "text", "contained",                          # DomainResource
}

PRIMITIVES = {
    "base64Binary", "boolean", "canonical", "code", "date", "dateTime",
    "decimal", "id", "instant", "integer", "integer64", "markdown", "oid",
    "positiveInt", "string", "time", "unsignedInt", "uri", "url", "uuid",
    "xhtml", "http://hl7.org/fhirpath/System.String",
}


def load_structures():
    """Returns {name: {"kind": ..., "elements": [ElementDefinition]}}."""
    out = {}
    for filename in ("profiles-types.json", "profiles-resources.json"):
        bundle = json.loads((SPEC / filename).read_text())
        for entry in bundle.get("entry", []):
            sd = entry.get("resource", {})
            if sd.get("resourceType") != "StructureDefinition":
                continue
            if sd.get("derivation") == "constraint":
                continue
            name = sd.get("name")
            snapshot = sd.get("snapshot", {}).get("element", [])
            if not name or not snapshot:
                continue
            out[name] = {"kind": sd.get("kind"), "elements": snapshot}
    return out


STRUCTURES = load_structures()


def children_of(elements, prefix):
    """Direct children of `prefix` among `elements`."""
    depth = prefix.count(".") + 1
    return [
        e for e in elements
        if e["path"].startswith(prefix + ".") and e["path"].count(".") == depth
    ]


def type_codes(element):
    codes = []
    for t in element.get("type", []):
        code = t.get("code")
        if code and code not in codes:
            codes.append(code)
    return codes


def capitalize(code):
    return code[:1].upper() + code[1:]


# Memoized nodes for whole types, and a stack to break cycles.
_type_nodes = {}
_building = set()


def node_for_type(name):
    """The transform node for a named type, or {} if it needs no rules."""
    if name in _type_nodes:
        return _type_nodes[name]
    if name in _building or name not in STRUCTURES:
        return {}
    _building.add(name)
    node = build_node(STRUCTURES[name]["elements"], name)
    _building.discard(name)
    _type_nodes[name] = node
    return node


def build_node(elements, prefix):
    """Builds the node for the element at `prefix`, pruning empties."""
    node = {}

    for child in children_of(elements, prefix):
        path = child["path"]
        field = path.rsplit(".", 1)[1]
        if field in INFRASTRUCTURE:
            continue
        collection = child.get("max") not in ("1", "0")

        # Recursion: contentReference points back at an ancestor path.
        ref = child.get("contentReference")
        if ref:
            target = ref.lstrip("#").split(".")
            entry = {"tr/move": target}
            if collection:
                entry["tr/isCollection"] = True
            node[field] = entry
            continue

        codes = type_codes(child)

        if field.endswith("[x]"):
            base = field[: -len("[x]")]
            collapsed = {}
            for code in codes:
                node[base + capitalize(code)] = {
                    "tr/act": "union",
                    "tr/arg": {"key": base, "type": code},
                }
                if code == "Reference":
                    collapsed[code] = {"tr/act": "reference"}
                else:
                    inner = node_for_type(code)
                    if inner:
                        collapsed[code] = {"tr/move": [code]}
            if collapsed:
                node[base] = collapsed
            continue

        if codes == ["Reference"]:
            entry = {"tr/act": "reference"}
            if collection:
                entry["tr/isCollection"] = True
            node[field] = entry
            continue

        if codes in (["BackboneElement"], ["Element"]):
            inner = build_node(elements, path)
            if inner:
                if collection:
                    inner["tr/isCollection"] = True
                node[field] = inner
            continue

        if len(codes) == 1 and codes[0] not in PRIMITIVES:
            inner = node_for_type(codes[0])
            if inner:
                entry = {"tr/move": [codes[0]]}
                if collection:
                    entry["tr/isCollection"] = True
                node[field] = entry
            continue

    return node


def resolve(root, path):
    """Follows a tr/move path from the map root, or None."""
    node = root
    for segment in path:
        if not isinstance(node, dict) or segment not in node:
            return None
        node = node[segment]
    return node


def has_rules(node):
    """Whether a node carries an action, a move, or any rule-bearing child."""
    if not isinstance(node, dict):
        return False
    if "tr/act" in node or "tr/move" in node:
        return True
    return any(has_rules(v) for k, v in node.items() if not k.startswith("tr/"))


def prune_dangling_moves(root):
    """Drops moves whose target carries no rules, then re-prunes empties.

    fhirbase omits these. `CapabilityStatement.rest.operation` points at
    `CapabilityStatement.rest.resource.operation`, which has no references and
    no choices, so the whole branch is dead weight -- while
    `QuestionnaireResponse.item.item` points at a node that does carry rules and
    is kept. Removing one can empty its parent, so this runs to a fixpoint.
    """
    for _ in range(20):
        changed = False

        def walk(node):
            nonlocal changed
            if not isinstance(node, dict):
                return node
            out = {}
            for k, v in node.items():
                if k == "tr/move":
                    target = resolve(root, v)
                    if target is None or not has_rules(target):
                        changed = True
                        return None
                    out[k] = v
                elif k.startswith("tr/"):
                    out[k] = v
                else:
                    child = walk(v)
                    if child is None:
                        changed = True
                    else:
                        out[k] = child
            # A node with nothing but tr/isCollection is not a rule.
            if not has_rules(out):
                return None
            return out

        for name in list(root):
            pruned = walk(root[name])
            if pruned is None:
                del root[name]
                changed = True
            else:
                root[name] = pruned

        if not changed:
            break
    return root


def main():
    result = {}
    for name in sorted(STRUCTURES):
        node = node_for_type(name)
        if node:
            result[name] = node
    result = prune_dangling_moves(result)
    OUT.write_text(json.dumps(result, indent=2, sort_keys=True))
    print(f"generated {len(result)} top-level entries -> {OUT}")


main()
