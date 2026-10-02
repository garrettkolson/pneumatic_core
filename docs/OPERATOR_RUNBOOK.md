# Pneumatic Node — Operator Runbook (Phase 8)

Audience: operators deploying and running the `pneumatic_committer` and
`node-server` binaries, bare-metal or in containers. Protocol design lives
in the ADRs and the Infinite Brain vault (`infinite-brain/`); this document
covers only *running the thing*.

---

## 1. What you are operating

| Binary | Role |
|---|---|
| `pneumatic_committer` | Dedicated committer node: commit pipeline, conflict resolution, epoch loop, shielded pool authority. |
| `node-server` | Composite runtime: one process hosting *all four* role-plugins (sentinel / executor / finalizer / committer) that this node's stake qualifies for, re-evaluated each epoch. |

Both are built from this workspace; inter-node traffic runs over **RNS**
(Reticulum Network Stack, UDP) with destination-encrypted packets.

**The node requires an external DATA SERVICE** — a process answering framed
MsgPack requests (`get_user`, `get_token`, stake snapshots, shielded-pool
state) over the local UDS/TCP channel. It is not part of this repo. **Both
binaries fail closed at boot when it is unreachable**, by design: the
committer at the stake-snapshot load, and the composite node-server at the
shielded-pool load (a missing pool state is indistinguishable from corrupt —
re-seeding would forget prior spends). A container that exits immediately with
`load shielded pool at boot: … refusing to re-seed` in its log has no
reachable data service; fix the data channel (`PNEUMATIC_DATA_ADDR`,
reachability, secret), do not restart-loop.

## 2. Configuration

### 2.1 Files

| Path | Read by | Notes |
|---|---|---|
| `./config.json` | `Config::build()` | Node spec. Valid examples: `deploy/config/*/config.json`. Fields: `is_full_node`, `rest_api_version`, `environments` (required, unused — keep `[]`), `main_env_id`, `reconciliation_partition_id`, optional `identity_path`, `bootstrap_peers[]` (`public_key` hex, `ip`, `port`), `rns_port` (default 4242), `transport_enabled` (default false = leaf; true = relay). |
| `/env/*.json` | `Config::build()` | Per-environment specs, loaded **at absolute path `/env`** (containers: bind-mount; bare metal: create the directory). One file per environment; `environment_id` must match `main_env_id`. Invalid specs **fail boot** by design. Example: `deploy/config/env/env.json`. |
| `./node_identity.json` | boot, created | Keystore: RNS keypair + Ed25519 signing key. Created on first boot if absent. **Back it up: losing it orphans the node's stake** (a corrupt file aborts boot rather than silently regenerating). |

### 2.2 Environment variables

| Var | Default | Meaning |
|---|---|---|
| `PNEUMATIC_DATA_SECRET` | unset | HMAC-SHA256 shared secret for the data-service channel. Unset = legacy unauthenticated framing (dev only; the binary warns at boot). **Set it in production.** |
| `PNEUMATIC_DATA_ADDR` | unset | `host:port` of a remote data service (container topology). Unset = default local channel: per-UID Unix domain socket, TCP loopback :55555 fallback on non-Unix. |
| `PNEUMATIC_HEALTH_ADDR` | `127.0.0.1:9500` | Health/metrics HTTP bind. The Docker image sets `0.0.0.0:9500`. Bind failure warns and continues (health is an ops affordance, not a consensus dependency). |
| `PNEUMATIC_EPOCH_INTERVAL_MS` | `5000` | `node-server` only: epoch-coordinator tick cadence. |
| `RUST_LOG` | `info` | Standard `tracing_subscriber::EnvFilter` syntax (`info`, `pneumatic_committer=debug,info`, …). |

## 3. Ports

| Port | Proto | Purpose |
|---|---|---|
| 4242/udp | RNS | Inter-node transport (`rns_port`). Peers listed in `bootstrap_peers` are contacted at their `port`; interfaces inside one RNS node bind `base+i` per interface. |
| 9500/tcp | HTTP | `/health` + `/metrics` (see §6). Publish to `127.0.0.1` only unless you must scrape across hosts. |
| 55555/tcp | data | Legacy data-service TCP loopback fallback (non-Unix) when `PNEUMATIC_DATA_ADDR` is unset. |

## 4. Deploy with Docker Compose (reference topology)

```
deploy/
  docker-compose.yml          # committer + full-node, one shared image
  config/committer/config.json   # + node_identity.json + logs land here (rw volume)
  config/full-node/config.json
  config/env/env.json            # shared, mounted read-only at /env
```

1. `.env` (compose picks it up automatically):

   ```bash
   PNEUMATIC_DATA_SECRET=change…cret
   PNEUMATIC_DATA_ADDR=192.0.2.10:55555   # if the data service is remote
   RUST_LOG=info
   ```

2. `docker compose -f deploy/docker-compose.yml up -d --build`

3. First-boot identities: each node prints
   `[pneumatic] node identity rhash=... ed25519=... rns_public_key=...`.
   To interlink the two nodes, copy each side's `rns_public_key` hex into the
   **other** side's `config.json → bootstrap_peers[]` (with the peer's DNS
   name/IP and UDP port), then restart both.

4. Verify: `curl -fsS localhost:9501/health` (committer) and
   `:9502/health` (full-node); `docker ps` should show both *healthy* once
   the data service is reachable.

Graceful stop: `docker compose stop` (sends SIGTERM; the node drains within
~2s — well inside the default 10s SIGKILL grace). **Persist the
`config/<service>/` volumes**: the keystore lives there.

## 5. Bare-metal deploy

```bash
cargo build --release -p pneumatic_committer --bin pneumatic_committer \
                      -p pneumatic_node_server --bin node-server
# in the node's working directory:
mkdir -p /env && cp deploy/config/env/env.json /env/
cp deploy/config/committer/config.json ./config.json
RUST_LOG=info PNEUMATIC_DATA_SECRET=*** ./target/release/pneumatic_committer
```

## 6. Observability

### Health — `GET /health`

- `200 {"status":"ok","service":"…","version":"…"}` — running.
- `503 {"status":"stopping",…}` — shutdown initiated; load balancers and
  orchestrators should drain the node (this happens **before** any other
  shutdown work).

### Metrics — `GET /metrics` (Prometheus text format)

| Series | Binary | Meaning |
|---|---|---|
| `pneumatic_up` | both | Always 1 while the poller runs (liveness companion). |
| `pneumatic_epoch_current` | both | Epoch number observed by this node. |
| `pneumatic_node_peers` | both | Registered peers across all five registry types. |
| `pneumatic_pending_transactions` | both | In-flight standard transactions. |
| `pneumatic_shielded_transactions` | both | Admitted shielded transactions (never-evicted map). |
| `pneumatic_transport_up` | committer | 1 = RNS transport started, 0 = booted without it. |
| `pneumatic_installed_roles` | node-server | Role-plugins currently installed. |
| `pneumatic_tokens_cached` | node-server | Token-cache depth (chains loaded in memory). |

Gauges refresh every 10s (committer: tied to the epoch-loop cadence poller).
Message/block counters are not yet instrumented (consensus hot paths are
deliberately untouched by Phase 8; wiring counters there is future work).

### Logs

- **stdout**: structured `tracing` (RUST_LOG-filtered) — container log
  collectors / journald ingest this.
- **File**: the env-spec `log_file` gets the durable `Logger` stream
  (epoch/committer lifecycle events, file-locked appends). Both channels
  exist by design; they are not bridged.

## 7. Graceful shutdown

Send **SIGTERM** (`docker stop`, `systemctl stop`) or Ctrl-C. Order:

1. `/health` flips to 503 (drain signal to LBs/orchestrators);
2. the epoch loop / coordinator stop (no new epoch work starts);
3. the off-thread stake-refresher (committer) or role-plugin shutdown fan-out
   (node-server) runs;
4. a 2s grace window lets already-spawned message tasks finish; the process
   exits. RNS worker threads are process-scoped and die with it.

State durability: chain state and the shielded pool live in the **data
service** (persisted at commit); the keystore file must be backed up
separately (§2.1).

## 8. Troubleshooting

| Symptom | Cause → action |
|---|---|
| Boot aborts with `Could not load file … as environment spec` | A file in `/env` is malformed or fails validation (quorum %, risk, gas bounds). Fix or remove it — bad specs fail boot by design. |
| `failed to load node identity …` | Keystore corrupt. **Do not delete to "regenerate"** — that orphans stake. Restore from backup. |
| Committer panics at `load stake snapshot at boot` | Data service unreachable. Start it / set `PNEUMATIC_DATA_ADDR`. Fail-closed by design. |
| Container exits at `Composite runtime failed to boot: … load shielded pool at boot … refusing to re-seed` | Same cause: the data service is unreachable, so stored pool state can't be distinguished from absent (fail-closed). Fix the data channel; do not restart-loop. |
| Log says `booting node without transport` | UDP port conflict (another RNS node on 4242). Adjust `rns_port` per instance. |
| `health server unavailable; continuing` | 9500 already in use. Set `PNEUMATIC_HEALTH_ADDR` per instance. |
| Registration rejected for a peer with stake | Stake must be in the current-epoch **snapshot** the gate consults; verify in the data service, and check `per_type_min_stake` floors in the env spec. |
| Wire/serialization `TypeMismatch(Array…)` | rmp moved to **named maps** on 10/01/2026 — mixed old/new binaries in one network are incompatible at the block-hash level; deploy a single version cluster. |
| High memory from ignored tests | Live proving / live-RNS tests are `#[ignore]`d (benchmark-only). Never enable them in CI: `cargo test --workspace` is the regression suite. |

## 9. Upgrades

Nodes are consensus-consistent **only at identical versions** (wire canonical
bytes and block hashing are consensus surface — see the rmp named-maps
incident). Upgrade procedure: take the whole cluster to the new version;
there is no mixed-version rolling upgrade. Re-read `fact-rmp-wire-named-maps`
(vault) and the release notes before each rollout. Back up every keystore
and the data service before upgrading.

---

*This runbook is the Phase 8 deliverable; when Phase 8 items change, update
this file and the vault node `docs/runbook` status together.*
