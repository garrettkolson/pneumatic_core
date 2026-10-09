#!/usr/bin/env bash
# Phase 2 egress: loss injection UNDER SUSTAINED TRAFFIC.
#
# Green control-plane (probe.sh) and clean-wire delivery (traffic.sh) both run
# on a lossless bridge. Real networks are worse. This applies `tc netem` loss
# to the egress of BOTH ends of one mesh link (sentinel <-> committer) while a
# sustained tx stream runs, then asserts two things:
#   1. the committer chain keeps GROWING during loss — the Resource path's
#      retransmission carries the pipeline, a stall is a transport failure;
#   2. after the qdisc is removed, the chain ends up FULLY committed — loss
#      delays, it does not drop transactions on the floor.
# Containers carry --cap-add NET_ADMIN for exactly this; tc runs as root via
# `docker exec --user 0` without changing what the node process sees.
#
# usage: netem.sh [LOSS_PCT=10] [REPEAT=60] [INTERVAL_MS=150]
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
RUN="$ROOT/deploy/multihost/run4"
TX="$ROOT/target/debug/pneumatic-tx"
READER="$ROOT/target/debug/examples/read_token_chain"
ADDR=127.0.0.1:19400            # published ingress of the sentinel
COMMITTER_DS=127.0.0.1:15555    # published data service of the committer
LOSS="${1:-10}"
REPEAT="${2:-60}"
INTERVAL_MS="${3:-150}"
NONCE_FILE="$RUN/nonce.txt"
NONCE=$(cat "$NONCE_FILE" 2>/dev/null || echo 1)

[ -x "$TX" ] || { echo "missing $TX — cargo build -p pneumatic_client" >&2; exit 1; }
[ -x "$READER" ] || { echo "missing $READER — cargo build --example read_token_chain" >&2; exit 1; }

# The reader prints `token=… blocks=… sequence=… tip=…`; take the field by name.
# Both of this script's assertions are growth assertions, so both read `sequence`
# (bumped once per committed block, never on a trim) rather than `blocks` — a
# token's chain is a 5-block sliding window (Token::security_level,
# src/tokens.rs:46), which pins the count at 5 and makes any count-difference gate
# unsatisfiable. See traffic.sh and RUNBOOK §4.
chain() { "$READER" "$COMMITTER_DS" 0a token | tr ' ' '\n' | sed -n 's/^sequence=//p'; }

netem_add() {   # netem_add CONTAINER
    docker exec --user 0 "pmesh-$1" tc qdisc add dev eth0 root netem loss "${LOSS}%"
    echo "loss ${LOSS}% injected on $1 egress"
}
netem_del() {   # netem_del CONTAINER  (tolerate absence — always-clean exit)
    docker exec --user 0 "pmesh-$1" tc qdisc del dev eth0 root 2>/dev/null || true
}
# Whatever happens from here on, the network goes back clean.
trap 'netem_del sentinel-1; netem_del committer-1' EXIT

before=$(chain)
echo "committer commit counter before: $before"

# Sustained stream in the background; injection lands mid-stream.
"$TX" submit --addr "$ADDR" --chain-id env --token 0a --to 77 \
    --amount 100 --nonce "$NONCE" --repeat "$REPEAT" --interval-ms "$INTERVAL_MS" \
    --id-prefix netem > "$RUN/netem-tx.log" 2>&1 &
TX_PID=$!

sleep 4                       # stream is up and committing
netem_add sentinel-1
netem_add committer-1

loss_start=$(chain)
sleep 6                       # the loss window
loss_mid=$(chain)
echo "chain during loss window: $loss_start -> $loss_mid (grew $((loss_mid - loss_start)))"

netem_del sentinel-1
netem_del committer-1
trap - EXIT

wait "$TX_PID" || { echo "tx stream failed — see $RUN/netem-tx.log"; tail -5 "$RUN/netem-tx.log"; exit 1; }
echo "$((NONCE + REPEAT))" > "$NONCE_FILE"

if [ "$loss_mid" -le "$loss_start" ]; then
    echo "STALL UNDER LOSS: chain did not grow while loss was applied"
    exit 1
fi

for _ in $(seq 1 45); do
    sleep 2
    after=$(chain)
    [ "$after" -ge $((before + REPEAT)) ] && break
done
after=$(chain)
echo "committer commit counter after: $after (submitted $REPEAT under ${LOSS}% loss)"
if [ "$after" -ge $((before + REPEAT)) ]; then
    echo "DELIVERED UNDER LOSS: every tx committed despite ${LOSS}% injected loss — retransmission carried the pipeline"
else
    echo "INCOMPLETE UNDER LOSS: $((after - before)) commits landed of $REPEAT submitted"
    exit 1
fi
