---
id: fact-port-allocations
title: "Port-per-registry-type allocations"
type: fact
namespace: pneumatic
visibility: namespace
summary: "Each node registry type has dedicated external and internal ports (e.g. Committer=42001 external / 50000 internal); data service on 55555."
auto_inject: false
applicable_when: "Configuring node deployments, firewalling, or adding a registry type"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If port constants in the conns port module change"
tags: [ports, networking, config, registry]
edges:
  - target: fact-wire-protocol
    type: supports
    weight: 0.8
    note: "Frames travel over these ports"
  - target: pillar-block-lattice
    type: part_of
    weight: 0.7
    note: "One port pair per worker role in the pipeline"
related: []
source_url: "Empty"
---

# Port-per-registry-type allocations

Network topology is defined by **port-per-registry-type**: each node registry type (Committer, Sentinel, Executor, Finalizer, Archiver) has dedicated external and internal port numbers, declared as constants in the conns port module (e.g. **Committer = 42001 external / 50000 internal**). Connections are created per type via `ConnFactory` (`src/conns/factories`), with `TcpSender`/`UdsSender` and `CoreTcpListener`/`CoreUdsListener` pairs.

The local data service (MsgPack over TCP/UDS) runs on port **55555** (TCP loopback fallback on non-Unix; Unix domain socket preferred on Unix) — see `src/data.rs`.
