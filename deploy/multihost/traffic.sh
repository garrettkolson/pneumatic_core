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

# The reader prints `token=… blocks=N sequence=N tip=…`; take the field by name.
# The gate is on `sequence`, NOT `blocks`, and the 10/08/2026 rehearsal is the
# reason: a token's chain is a sliding window (`Token::security_level`, default
# 5 — `src/tokens.rs:46`). Once the window is full every commit trims the oldest
# block and appends the new one, so the COUNT sits at 5 forever while the chain
# advances — a count gate reports "grew by 0 of 20" against a pipeline that was
# delivering every transaction. `sequence` is bumped exactly once per committed
# block (`Token::commit_block`) and never on a trim, so it is monotonic in
# commits no matter what the window is doing.
read_field() { "$READER" "$COMMITTER_DS" 0a token | tr ' ' '\n' | sed -n "s/^$1=//p"; }

before_seq=$(read_field sequence)
before_blocks=$(read_field blocks)
echo "committer chain before: sequence=$before_seq blocks=$before_blocks"

"$TX" submit --addr "$ADDR" --chain-id env --token 0a --to 77 \
    --amount 100 --nonce "$NONCE" --repeat "$REPEAT" --interval-ms "$INTERVAL_MS" \
    --id-prefix ph2

echo "$((NONCE + REPEAT))" > "$NONCE_FILE"

# Blocks land asynchronously through the pipeline; give the last transaction
# time to be committed before reading the terminal state back.
echo "waiting for the pipeline to drain, then reading the committer chain"
for _ in $(seq 1 30); do
    sleep 2
    after_seq=$(read_field sequence)
    [ "$after_seq" -ge $((before_seq + REPEAT)) ] && break
done

after_seq=$(read_field sequence)
after_blocks=$(read_field blocks)
echo "committer chain after: sequence=$after_seq blocks=$after_blocks (submitted $REPEAT)"
if [ "$after_seq" -ge $((before_seq + REPEAT)) ]; then
    echo "DELIVERED: every submitted transaction is visible as a committed block through the data service"
    if [ "$after_blocks" -le "$before_blocks" ]; then
        echo "  (blocks did not grow — that is the $after_blocks-block window trimming as it appends; sequence moved $before_seq → $after_seq)"
    fi
else
    echo "NOT DELIVERED: $((after_seq - before_seq)) of $REPEAT submissions reached a committed block — transport or pipeline failure"
    exit 1
fi
