#!/usr/bin/env bash
# Bring up the 4-node multi-namespace rehearsal: one Docker network, four
# "host" containers (each a single-homed machine, exactly the shape a
# per-host placement assumes), four data-service sidecars, all wired from
# the generated manifest — nothing hand-placed.
#
# Deviation from production, stated: the data-service sidecar is a separate
# container reached over the mesh network, not a same-netns loopback process.
# The node→data hop therefore crosses a veth it would not cross on a real
# host. That is harsher than production, never kinder; the mesh itself —
# the thing Phase 2 tests — is exactly per-host shape.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
RUN="$ROOT/deploy/multihost/run4"
IMAGE=pneumatic:phase2
NET=pmesh
GATEWAY=172.31.77.1
USER_ID="$(id -u):$(id -g)"

[ -f "$RUN/manifest.json" ] || { echo "run gen.sh first" >&2; exit 1; }

docker network create --subnet 172.31.77.0/24 "$NET" 2>/dev/null || true

# "name role addr" per node, in manifest order — the manifest is the
# generator's own record of placement, so the scripts cannot disagree with it.
NODES=$(python3 - "$RUN/manifest.json" <<'EOF'
import json, sys
m = json.load(open(sys.argv[1]))
for n in m["nodes"]:
    print(n["name"], n["role"], n["address"])
EOF
)

sidecar_ip() { echo "${1%.*}.$(( ${1##*.} + 10 ))"; }

# --- data services first: every node fails closed at boot without one ------
while read -r name role addr; do
    ds_ip=$(sidecar_ip "$addr")
    docker rm -f "pmesh-dsvc-$name" >/dev/null 2>&1 || true
    # The committer's sidecar publishes 55555 to the host so the commit check
    # (examples/read_token_chain) can read the committed chain from the
    # outside — "observable through the data service", not through a log line.
    # Match the manifest's serde spelling: roles are lowercase there.
    publish=()
    if [ "$role" = "committer" ]; then
        publish=(-p 127.0.0.1:15555:55555)
    fi
    docker run -d --name "pmesh-dsvc-$name" --net "$NET" --ip "$ds_ip" \
        --user "$USER_ID" -v "$RUN:$RUN" ${publish[@]+"${publish[@]}"} \
        "$IMAGE" pneumatic_data_service \
        --listen 0.0.0.0:55555 \
        --genesis "$RUN/genesis.json" \
        --state "$RUN/nodes/$name/data_state.json" >/dev/null
done <<< "$NODES"

echo -n "waiting for genesis on all data services"
for _ in $(seq 1 60); do
    applied=0
    while read -r name _rest; do
        docker logs "pmesh-dsvc-$name" 2>&1 | grep -q "genesis applied" && applied=$((applied+1))
    done <<< "$NODES"
    total=$(echo "$NODES" | wc -l | tr -d ' ')
    [ "$applied" -ge "$total" ] && break
    echo -n "."
    sleep 1
done
echo " done"

# --- nodes ------------------------------------------------------------------
while read -r name role addr; do
    ds_ip=$(sidecar_ip "$addr")
    docker rm -f "pmesh-$name" >/dev/null 2>&1 || true
    extra=()
    # The sentinel gets the client ingress; inside its own namespace binding
    # "any" is as bounded as loopback is on a host. Phase 4's rate limiting is
    # what closes the real-host case, not the bind.
    # Lowercase: the manifest's serde spelling of roles.
    if [ "$role" = "sentinel" ]; then
        extra=(-p 127.0.0.1:19400:9400 -e PNEUMATIC_INGRESS_ADDR=0.0.0.0:9400)
    fi
    docker run -d --name "pmesh-$name" --net "$NET" --ip "$addr" \
        --cap-add NET_ADMIN --user "$USER_ID" -v "$RUN:$RUN" \
        --restart on-failure:5 \
        -e PNEUMATIC_CONFIG_FILE="$RUN/nodes/$name/config.json" \
        -e PNEUMATIC_ENV_DIR="$RUN/nodes/$name/env" \
        -e PNEUMATIC_DATA_ADDR="$ds_ip:55555" \
        -e PNEUMATIC_HEALTH_ADDR=0.0.0.0:9500 \
        -e RUST_LOG=info \
        ${extra[@]+"${extra[@]}"} \
        "$IMAGE" node-server >/dev/null
done <<< "$NODES"

echo
echo "up: $(echo "$NODES" | wc -l | tr -d ' ') nodes + data services on $NET"
echo "fragments land in $RUN/nodes/<name>/mesh_fragment.json"
echo "next: sleep ~15 for peering, then ./probe.sh"
