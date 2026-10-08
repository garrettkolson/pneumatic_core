#!/usr/bin/env bash
# The verdict: aggregate every node's signed mesh fragment and judge it
# against the manifest. Exit code is the contract: 0 complete, 1 findings,
# 2 unusable inputs. A green run means CONTROL-PLANE FORMATION ONLY — then
# run traffic.sh.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
RUN="$ROOT/deploy/multihost/run4"
PROBE="$ROOT/target/debug/mesh-probe"

[ -x "$PROBE" ] || { echo "missing $PROBE — cargo build -p pneumatic_testnet_gen" >&2; exit 2; }

# Fragments are collected under unique per-node names: every node writes the
# same base filename, and a directory of same-named files is how a cluster
# silently loses three quarters of its evidence.
mkdir -p "$RUN/fragments"
python3 - "$RUN" <<'EOF'
import json, os, shutil, sys
run = sys.argv[1]
m = json.load(open(os.path.join(run, "manifest.json")))
dst = os.path.join(run, "fragments")
for n in m["nodes"]:
    src = os.path.join(run, "nodes", n["name"], "mesh_fragment.json")
    if os.path.exists(src):
        shutil.copy(src, os.path.join(dst, n["name"] + ".json"))
EOF

# Fragments are only current for one eviction window (probe default 30 s):
# copy and judge in one breath.
MAX_AGE="${MAX_AGE:-30}"
exec "$PROBE" --manifest "$RUN/manifest.json" --fragments "$RUN/fragments" --max-age "$MAX_AGE"
