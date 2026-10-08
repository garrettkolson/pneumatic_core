#!/usr/bin/env bash
# Generate the 4-node per-host testnet for the local multi-namespace rehearsal.
#
# The output tree is mounted into every container at the SAME absolute path it
# has on the host, because the generator writes absolute identity_path /
# mesh_fragment_path values (emit.rs:340-346 — "portable only until the
# operator rewrites both keys or mounts the same paths"). Same-path mount
# beats rewriting keys: fewer edited bytes on the path to a verdict.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
RUN="$ROOT/deploy/multihost/run4"
GEN="$ROOT/target/debug/pneumatic_testnet_gen"
TX="$ROOT/target/debug/pneumatic-tx"

for bin in "$GEN" "$TX"; do
    [ -x "$bin" ] || { echo "missing $bin — run: cargo build -p pneumatic_testnet_gen -p pneumatic_client" >&2; exit 1; }
done

# The sender key must exist in genesis accounts BEFORE its first submission,
# and it must be the same derivation `pneumatic-tx` uses. One code path gives
# it to us: `pneumatic-tx account` prints it.
CLIENT_PK=$("$TX" account)

"$GEN" \
    --out "$RUN" \
    --validators 4 \
    --addresses-file "$ROOT/deploy/multihost/addresses.txt" \
    --tx-token 0a \
    --client-account "$CLIENT_PK"

# Containers write keystores, logs, fragments, and data-service state into
# this tree as the host user (the run scripts pass --user).
chmod -R a+rwX "$RUN"
echo
echo "generated $RUN (sender account $CLIENT_PK, token 0a)"
