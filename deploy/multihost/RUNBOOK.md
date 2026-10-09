# Phase 2 Runbook — 4-Host Mesh (cloud, real network namespaces)

Objective: form the RNS mesh across **separate machines**, prove control-plane
formation (probe), prove sustained delivery under clean and lossy links
(traffic, netem), and record the four per-node resource numbers
(verifications/s, threads, RSS, sockets). The Docker rehearsal in this
directory is the dress rehearsal: same placement model, same scripts, one
`--ip`/hostname swap away from cloud.

## Placement model

One shared UDP base port per cluster (21000). Node *j*'s peer-facing sockets
occupy `base_port + i` for its i-th bootstrap peer, and every peer dials node
*j* at the port that *j* uses for *that* link (the j-rule, emitted by
`pneumatic_testnet_gen`). On separate machines the address disambiguates what
the port cannot share: every node runs on base 21000 with different IPs.

Firewall per host: allow inbound UDP `21000..(21000 + validators)` and TCP
`55555` (data service — bind it to the private interface only), plus
`9400` (ingress) and `9500` (metrics) on the sentinel host / ops network.

## 1. Generate (one control machine)

```bash
./gen.sh                        # writes run4/ (per-host trees + manifest)
```

`gen.sh` calls `pneumatic_testnet_gen` with `--addresses-file` (one IP per
line, see `addresses.txt`) and seeds genesis with the transfer token
(`--tx-token 0a`) and the funded client account. For cloud, replace the
172.31.77.x addresses with each host's **private** interface address.

Ship exactly one directory per node to its host: `run4/nodes/<name>/`
(config.json + env/ specs + keystore dir) and `run4/data-services/<name>/`
(genesis.json + state dir). The keystore (`node_identity.json`) is created on
first boot — keep the volume, or identity churn will orphan the genesis keys.

## 2. Boot (each host)

Data service first, node second — the node refuses to start without its
sidecar:

```bash
docker run -d --name pmesh-dsvc-<name> --net host \
    -v /opt/pmesh:/opt/pmesh pneumatic:phase2 \
    pneumatic_data_service --listen 0.0.0.0:55555 \
    --genesis /opt/pmesh/run4/data-services/<name>/genesis.json \
    --state    /opt/pmesh/run4/data-services/<name>/state
# wait for: "genesis applied" in `docker logs pmesh-dsvc-<name>`

docker run -d --name pmesh-<name> --net host --cap-add NET_ADMIN \
    -v /opt/pmesh:/opt/pmesh \
    -e PNEUMATIC_CONFIG_FILE=/opt/pmesh/run4/nodes/<name>/config.json \
    -e PNEUMATIC_ENV_DIR=/opt/pmesh/run4/nodes/<name>/env \
    -e PNEUMATIC_DATA_ADDR=127.0.0.1:55555 \
    -e PNEUMATIC_HEALTH_ADDR=0.0.0.0:9500 \
    pneumatic:phase2 node-server
```

Boot order between hosts does **not** matter: the transport re-announces every
10 s, so routes converge within ~2 intervals of the last host coming up
(boot-skew convergence is pinned by the wrapper test
`boot_skew_routes_converge_via_periodic_announce` — a one-shot startup
announce was the exact Phase 2 blocker found in the 10/08/2026 rehearsal).

On the control machine the same boot is one command — `./down.sh && ./up.sh`
over the generated `run4/`, four node containers plus four sidecars on one
`pmesh` network. `up.sh` defaults to `pneumatic:phase2`; `IMAGE=pneumatic:<tag>
./up.sh` rehearses a candidate build without displacing the tag a recorded run
was made with. Build notes for this harness: the image's build context excludes
`target/`, `deploy/`, `infinite-brain/`, `docs/`, `plans/`, `*.md` and `.git/`
(`.dockerignore`) — the image ships binaries only, and the generated tree arrives
at runtime through the bind mount. The release build of the whole workspace runs
inside the image (halo2 dominates, ~4 min warm), so build the tag you mean to
rehearse: reusing a tag you did not build from the tree you are testing is how a
rehearsal measures the wrong binary.

## 3. Prove control-plane formation

From the control machine (needs SSH or a collector that can read each node's
`mesh_fragment.json` — in Docker `probe.sh` `docker cp`s; in cloud, scp the
per-host fragments into `run4/fragments/`):

```bash
./probe.sh        # exit 0 = every node's signed self-report agrees
```

Exit codes: 0 complete, 1 findings (report them, they are the exit test),
2 unusable inputs.

## 4. Prove sustained delivery

```bash
./traffic.sh 50 200        # 50 tx at 200 ms apart; asserts 50 commits landed
./netem.sh 10 60 150       # 10% loss on one link's egress UNDER traffic
```

Both read the committer host's data service over the production read path —
an accepted-but-never-committed stream fails the gate on purpose.

**The gate reads the commit counter, not the block count.** A token's chain is a
sliding window: `Token::security_level` (`src/tokens.rs:46`, default **5**) doubles
as the maximum chain length, and `Token::commit_block` trims the oldest block once
the count reaches it. So `blocks` sits at 5 forever while the chain advances — and a
"chain grew by N" assertion becomes unsatisfiable the moment the window fills. That
mis-report happened for real on 10/08/2026: a 20-tx run reported "grew by 0 of 20 —
NOT DELIVERED" while the committer's log kept printing `COMMIT-OK` and the tip kept
moving. `traffic.sh` therefore gates on `sequence` (`Token::sequence_number`: bumped
once per committed block, never on a trim) and prints `blocks` alongside.

Two honest limits of that number: it counts **appends**, so a conflict-resolution
replacement moves it too (a 20-tx run once moved it by 24) — it is a floor on
delivery, not a per-transaction proof; and with a 5-block window, a batch larger
than 5 cannot be checked by transaction id from outside the node at all. Per-tx
observability needs a genesis-settable window larger than the batch, or an
archiver to read from.

Negative control, so the gate stays honest: `docker stop pmesh-committer-1`, submit
three, expect "0 of 3 submissions reached a committed block" and exit 1.

Each data service re-applies `genesis.json` at boot, so chains reset on every
`./up.sh` — delivery numbers are per-bring-up, and the first run of a session
rightly reads `before: 0`.

## 5. Record the four numbers

```bash
./sample.sh idle           # threads, RSS, sockets, verif_total per node
./traffic.sh 200 100 &      # sustained load...
./sample.sh load            # ...sampled while traffic is flowing
```

`verifications/s = (verif_total[t2] - verif_total[t1]) / (t2 - t1)` per node —
the sampler exports the counter (`pneumatic_signature_verifications_total`,
the `check_signature` chokepoint) and differences two CSV rows; threads/RSS/
sockets come from `/proc` inside each container.

## Troubleshooting

- `failed to decrypt inbound packet; dropping` — expected background noise
  (point-to-point links see each other's traffic). Not an error.
- `no live route to [rhash] ... retry after its announce` at boot — self-heals
  within ~20 s via the re-announce ticker. **Stuck past ~60 s** means the
  announce is not traversing the network at all: check the UDP firewall first,
  then that each peer's j-ported socket is listening (`ss -uan` shows
  21000..21002 in the container/host).
- `pneumatic_node_peers 0` with routes live — a *restart* symptom, and as of
  this writing it does **not** self-heal. The restarted node's request side is
  provably correct (announces validated, registers + directory requests
  sent — the catch-up loop re-asks bootstrap peers every tick), but the
  long-running peers answer into **stale session state**: their cached RNS
  link/ratchet for the restarted identity belongs to the dead instance, so
  RegisterAcks and directory responses die on the return leg (restarted node:
  `verifications_total` stays 0 — nothing decryptable ever arrives). Proof:
  restarting the *peer as well* unblocks the restarted node within seconds
  (observed 0→8 peers right after the peer restarted, 10/08/2026 rehearsal).
  Until session invalidation on re-announce lands (Phase 3 work — sketch:
  announce carries a per-boot seq; receiver drops cached links on change),
  **single-node restart is NOT a supported operation on this stack**.
- Recovery from any incident: a **full re-up** — `./down.sh && ./up.sh` (the
  data services persist state files; only peer sessions reset). Restarting
  one node (`docker rm -f pmesh-<name>` + replicate its `docker run` line,
  keeping the sentinel's ingress `-p`) leaves it permanently at 0 peers.
- `Exited (0)` seconds after boot, every node, on a **re-up**, with the log line
  `Could not load file ".../env/pneumatic.log" as environment spec` / `invalid
  type: integer \`2026\`` — the node writes its runtime log into the (bind-mounted)
  `env/` dir, and the config loader used to parse **every** file there as a spec,
  so a non-empty log from the previous run failed boot fail-closed. Fixed in
  `Config::get_environment_metadata_from` (only `.json` files are spec candidates;
  a malformed `.json` still stays fatal). On an image without the fix: `rm
  run4/nodes/*/env/*.log` before `./up.sh`.
- Zero peers but everything "healthy" — suspect key mixups first: genesis uses
  Ed25519 keys, `bootstrap_peers` uses the 64-byte RNS transport keys; the
  generator emits both, never swap them.
