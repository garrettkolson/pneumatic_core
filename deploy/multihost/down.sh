#!/usr/bin/env bash
# Tear down the rehearsal. The generated tree (keys, state files, fragments)
# is left in place — deleting keystores orphans genesis stake, and the
# resource CSVs and state files are the run's evidence. Remove run4/ by hand
# only when you intend to orphan the keys.
set -euo pipefail

for c in $(docker ps -aq --filter "name=pmesh-"); do
    docker rm -f "$c" >/dev/null
done
docker network rm pmesh 2>/dev/null || true
echo "down: pmesh-* containers removed; generated tree and evidence kept"
