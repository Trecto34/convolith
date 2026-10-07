#!/usr/bin/env sh
# Validate spec/examples/*.json against spec/*.schema.json.
#
# Uses ajv-cli (npx) when available; falls back to python3 + jsonschema.
# Usage: sh spec/validate-examples.sh   (from the repository root)
set -eu
cd "$(dirname "$0")/.."

if command -v npx >/dev/null 2>&1; then
  # $defs/session is reachable through a one-line wrapper, so every example is
  # checked against a real schema document rather than a hand-written subset.
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  printf '{"$schema":"https://json-schema.org/draft/2020-12/schema","$ref":"https://convolith.example/spec/v1/conversation.schema.json#/$defs/session"}' > "$tmp/session.schema.json"
  npx --yes -p ajv-cli@5 -p ajv-formats -- ajv validate --spec=draft2020 --strict=false -c ajv-formats -s spec/event.schema.json -r spec/provenance.schema.json -r spec/conversation.schema.json -r spec/artifact.schema.json \
    -d spec/examples/event.json -d spec/examples/event-conflict-variant.json
  npx --yes -p ajv-cli@5 -p ajv-formats -- ajv validate --spec=draft2020 --strict=false -c ajv-formats -s "$tmp/session.schema.json" -r spec/conversation.schema.json -d spec/examples/session.json
  npx --yes -p ajv-cli@5 -p ajv-formats -- ajv validate --spec=draft2020 --strict=false -c ajv-formats -s spec/conversation.schema.json -d spec/examples/conversation.json
  npx --yes -p ajv-cli@5 -p ajv-formats -- ajv validate --spec=draft2020 --strict=false -c ajv-formats -s spec/artifact.schema.json -d spec/examples/artifact.json
  npx --yes -p ajv-cli@5 -p ajv-formats -- ajv validate --spec=draft2020 --strict=false -c ajv-formats -s spec/provenance.schema.json -d spec/examples/provenance.json
  exit 0
fi

python3 - "$@" <<'PY'
import json, pathlib, sys
try:
    import jsonschema
    from jsonschema import Draft202012Validator
except ImportError:
    print("no validator available: install ajv-cli (node) or jsonschema (python)"); sys.exit(2)
root = pathlib.Path("spec")
schemas = {}
for name in ("event", "conversation", "artifact", "provenance"):
    doc = json.loads((root / f"{name}.schema.json").read_text())
    Draft202012Validator.check_schema(doc)
    schemas[name] = doc
# Resolve the cross-schema $ref in event.schema.json to provenance.schema.json.
store = {"https://convolith.example/spec/v1/provenance.schema.json": schemas["provenance"],
         "provenance.schema.json": schemas["provenance"]}
resolver = jsonschema.RefResolver.from_schema(schemas["event"], store=store)
cases = [("event.schema.json", "event.json"),
         ("event.schema.json", "event-conflict-variant.json"),
         ("conversation.schema.json", "conversation.json"),
         ("artifact.schema.json", "artifact.json"),
         ("provenance.schema.json", "provenance.json")]
for schema_name, example in cases:
    instance = json.loads((root / "examples" / example).read_text())
    v = Draft202012Validator(schemas[schema_name.split(".")[0]], resolver=resolver)
    errs = sorted(v.iter_errors(instance), key=lambda e: e.path)
    for e in errs:
        print(f"{example}: {list(e.path)}: {e.message}")
    if errs:
        sys.exit(1)
    print(f"{example} valid")
# session example against $defs/session
session_schema = {"$ref": "#/$defs/session", **schemas["conversation"]}
instance = json.loads((root / "examples" / "session.json").read_text())
v = Draft202012Validator(session_schema)
for e in v.iter_errors(instance):
    print(f"session.json: {list(e.path)}: {e.message}")
    sys.exit(1)
print("session.json valid")
PY