#!/usr/bin/env bash
# Do the four committers agree on the chain?
#
# On the run4 mesh every host runs the committer role, so every host commits. The
# fault this exists to see is the one ADR-004 was written to remove: a transaction
# reaching several Finalizers, each building its own block for it, and each host
# committing the block it heard first. The signature of that fault is not a crash —
# it is four chains with the same length and different tips, or chains whose
# sequences ran past the number of transactions submitted.
#
# Read through the production read path (examples/read_token_chain against each
# host's data-service sidecar), never a log line. up.sh publishes every sidecar for
# exactly this check.
#
#   ./tips.sh                 # compare now
#   ./tips.sh 0a token        # explicit token id / partition
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
READER="$ROOT/target/debug/examples/read_token_chain"
TOKEN="${1:-0a}"
PARTITION="${2:-token}"
PORTS=(15555 15556 15557 15558)
NAMES=(committer-1 sentinel-1 executor-1 finalizer-1)

[ -x "$READER" ] || { echo "missing $READER — cargo build --example read_token_chain" >&2; exit 1; }

rows=()
for i in "${!PORTS[@]}"; do
    if ! line=$("$READER" "127.0.0.1:${PORTS[$i]}" "$TOKEN" "$PARTITION" 2>&1); then
        echo "UNREADABLE  ${NAMES[$i]} (:${PORTS[$i]}) — $line"
        rows+=("")
        continue
    fi
    printf '%-14s %s\n' "${NAMES[$i]}" "$line"
    rows+=("$line")
done

# The comparison is on (sequence, tip). Equal sequences with different tips is
# divergence; unequal sequences means the hosts committed different numbers of
# blocks, which is divergence with an extra step.
seqs=(); tips=(); unreadable=0
for row in "${rows[@]}"; do
    [ -z "$row" ] && { unreadable=$((unreadable + 1)); continue; }
    seqs+=("$(echo "$row" | tr ' ' '\n' | sed -n 's/^sequence=//p')")
    tips+=("$(echo "$row" | tr ' ' '\n' | sed -n 's/^tip=//p')")
done

if [ "$unreadable" -gt 0 ]; then
    echo "NOT COMPARABLE: $unreadable of ${#PORTS[@]} sidecars unreadable"
    exit 2
fi

uniq_tips=$(printf '%s\n' "${tips[@]}" | sort -u | wc -l | tr -d ' ')
uniq_seqs=$(printf '%s\n' "${seqs[@]}" | sort -u | wc -l | tr -d ' ')
echo "sequences: ${seqs[*]}"
if [ "$uniq_tips" -eq 1 ] && [ "$uniq_seqs" -eq 1 ]; then
    echo "AGREEMENT: all ${#PORTS[@]} committers hold the same tip at the same sequence"
    exit 0
fi
echo "DIVERGENCE: $uniq_seqs distinct sequence(s), $uniq_tips distinct tip(s)"
exit 1
