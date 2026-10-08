#!/usr/bin/env bash
# Trap #2: the exit test needs per-node numbers at target size, not vibes.
# Records per node: threads, RSS, sockets, CPU time, mesh-peers gauge, and
# ingress counters — from /proc and /metrics INSIDE each container (root for
# /proc access; the node process itself runs as the host user).
#
# The four roadmap numbers are verifications/s, threads, RSS, sockets. All
# four are measured: threads/RSS/sockets from /proc inside each container, and
# verifications/s from the `pneumatic_signature_verifications_total` gauge
# (the crypto chokepoint counter — differenced across two samples against the
# wall-clock delta to yield a rate). CPU jiffies ride as the load-time proxy.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
RUN="$ROOT/deploy/multihost/run4"
LABEL="${1:-sample}"

csv="$RUN/resources-$LABEL.csv"
echo "node,ts_utc,threads,rss_kb,cpu_jiffies,fd_sockets,udp_sockets,tcp_established,peers,tokens_cached,epoch,verif_total" > "$csv"

NODES=$(python3 - "$RUN/manifest.json" <<'EOF'
import json, sys
m = json.load(open(sys.argv[1]))
for n in m["nodes"]:
    print(n["name"])
EOF
)

while read -r name; do
    c="pmesh-$name"
    docker exec --user 0 "$c" sh -c '
        pid=""
        for d in /proc/[0-9]*; do
            [ "$(cat $d/comm 2>/dev/null)" = "node-server" ] && pid=${d#/proc/} && break
        done
        [ -n "$pid" ] || { echo "node-server not running"; exit 1; }
        threads=$(ls /proc/$pid/task | wc -l)
        rss=$(awk "/VmRSS/ {print \$2}" /proc/$pid/status)
        cpu=$(awk "{print \$14+\$15}" /proc/$pid/stat)
        fds=$(ls /proc/$pid/fd 2>/dev/null | wc -l)
        sockets=$(ls -l /proc/$pid/fd 2>/dev/null | grep -c "socket:" || true)
        udp=$(ss -uan 2>/dev/null | tail -n +2 | wc -l)
        tcp=$(ss -tan "state established" 2>/dev/null | tail -n +2 | wc -l)
        metrics=$(curl -s --max-time 3 127.0.0.1:9500/metrics 2>/dev/null || true)
        peers=$(echo "$metrics" | awk "/^pneumatic_node_peers / {print \$2}")
        tokens=$(echo "$metrics" | awk "/^pneumatic_tokens_cached / {print \$2}")
        epoch=$(echo "$metrics" | awk "/^pneumatic_epoch_current / {print \$2}")
        verif=$(echo "$metrics" | awk "/^pneumatic_signature_verifications_total / {print \$2}")
        printf "%s %s %s %s %s %s %s %s %s %s\n" "$threads" "$rss" "$cpu" "$sockets" "$udp" "$tcp" "${peers:-NA}" "${tokens:-NA}" "${epoch:-NA}" "${verif:-NA}"
    ' | { read -r threads rss cpu sockets udp tcp peers tokens epoch verif
        echo "$name,$(date -u +%Y-%m-%dT%H:%M:%SZ),$threads,$rss,$cpu,$sockets,$udp,$tcp,$peers,$tokens,$epoch,$verif" >> "$csv"
    }
done <<< "$NODES"

echo "wrote $csv"
column -s, -t < "$csv"
