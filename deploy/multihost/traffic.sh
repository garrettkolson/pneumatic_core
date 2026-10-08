#!/usr/bin/env bash
# Trap #1: a green probe is control-plane formation only. This sends traffic
# — a sustained stream through the sentinel's ingress — and then asserts the
# effect on the production read path: the committer host's data service must
# show the token chain GROW. An accepted-but-never-committed stream is a
# transport failure, which is exactly what Phase 2 is looking for.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
RUN="$ROOT/deploy/multihost/run4"
TX="$ROOT/target/debug/pneumatic-tx"
READER="$ROOT/target/debug/examples/read_token_chain"
ADDR=127.0.0.1:19400            # published ingress of the sentinel
COMMITTER_DS=127.0.0.1:15555    # published data service of the committer
REPEAT="${1:-20}"
INTERVAL_MS="${2:-200}"
NONCE_FILE="$RUN/nonce.txt"
NONCE=$(cat "$NONCE_FILE" 2>/dev/null || echo 1)

[ -x "$TX" ] || { echo "missing $TX — cargo build -p pneumatic_client" >&2; exit 1; }
[ -x "$READER" ] || { echo "missing $READER — cargo build --example read_token_chain" >&2; exit 1; }

before=$("$READER" "$COMMITTER_DS" 0a token | awk "{print \$5}")
echo "committer chain before: $before block(s)"

"$TX" submit --addr "$ADDR" --chain-id env --token 0a --to 77 \
    --amount 100 --nonce "$NONCE" --repeat "$REPEAT" --interval-ms "$INTERVAL_MS" \
    --id-prefix ph2

echo "$((NONCE + REPEAT))" > "$NONCE_FILE"

# Blocks land asynchronously through the pipeline; give the last transaction
# time to be committed before reading the terminal state back.
echo "waiting for the pipeline to drain, then reading the committer chain"
for _ in $(seq 1 30); do
    sleep 2
    after=$("$READER" "$COMMITTER_DS" 0a token | awk "{print \$5}")
    [ "$after" -ge $((before + REPEAT)) ] && break
done

after=$("$READER" "$COMMITTER_DS" 0a token | awk "{print \$5}")
echo "committer chain after: $after block(s) (submitted $REPEAT)"
if [ "$after" -ge $((before + REPEAT)) ]; then
    echo "DELIVERED: every submitted transaction is visible as a committed block through the data service"
else
    echo "NOT DELIVERED: chain grew by $((after - before)) of $REPEAT — transport or pipeline failure"
    exit 1
fi
