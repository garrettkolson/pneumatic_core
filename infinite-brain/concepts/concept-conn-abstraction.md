---
id: concept-conn-abstraction
title: "Connection abstraction: ConnFactory + Sender/Stream/Listener trait families with TCP and UDS duality"
type: concept
namespace: pneumatic
visibility: namespace
summary: "One factory and four trait families (Connection/Sender/Stream/Listener) abstract TCP and Unix sockets; every local path is UDS-first with TCP fallback. NOTE: this is the legacy/local transport layer (data-service channels) — the production inter-node wire for role traffic is RNS (see concept-rns-transport)."
auto_inject: false
applicable_when: "Adding a transport, touching src/conns/**, or building a new local service-to-service channel. For inter-node role traffic, see concept-rns-transport (RNS is the production wire)."
confidence: 0.95
verified_at: "09/25/2026"
verified_by: "dsh-agent"
staleness_signal: "If the Connection/Sender/Stream/Listener trait shapes in src/conns change, or a third transport type is added"
tags: [networking, traits, tcp, uds, factory, framing]
edges:
  - target: fact-wire-protocol
    type: supports
    weight: 0.9
    note: "The 4-byte BE length + MsgPack framing is the shared wire contract of these traits"
  - target: concept-rns-transport
    type: related_to
    weight: 0.8
    note: "RnsConnection is the RNS-side implementation of the same Connection trait"
  - target: pattern-fail-closed
    type: supports
    weight: 0.8
    note: "UDS symlink rejection and the 16 MiB frame cap are fail-closed instantiations"
related: []
source_url: "Empty"
---

# Connection abstraction: ConnFactory and the TCP/UDS trait families

`src/conns` splits transport into four small trait families, all produced by one factory:

- **`Connection`** (src/conns.rs:79-82): `async send(&self, data: &Vec<u8>) -> Result<(), ConnError>` — the node-to-node connection primitive; `TcpConnection` implements it with a read-loop `JoinHandle`.
- **`Sender`** (src/conns/senders.rs:18-23): request/response over a fresh socket; wire form is `[4-byte BE length][auth_tag(32) || body]`. The 32-byte HMAC tag is a no-op when no shared secret is configured; `CONNECT_TIMEOUT_SECS = 5` (senders.rs:12), `AUTH_TAG_LEN = 32` (senders.rs:14).
- **`Stream`** (src/conns/streams.rs): synchronous `read_exact`/`write_all`/`into_split`; `into_split` flips the socket nonblocking and hands it to tokio via `from_std`, powering the async `StreamReader`/`StreamWriter` traits.
- **`Listener`** (src/conns/listeners.rs:1-57): `accept() -> Box<dyn Stream>`; `CoreUdsListener::new` runs `prepare_socket_path` first.

`ConnFactory` (src/conns/factories.rs:112) ties them together: `get_sender` / `get_listener` / `create_connection`, with a default 15 s read/write timeout (factories.rs:17) and `shared_secret: Option<Vec<u8>>`. `ConnTarget::Remote(addr)` maps to TCP; `Local(LocalTarget::Tcp | LocalTarget::Unix)` picks the local transport — and UDS **fails loudly** (`NOT_UNIX_MESSAGE`) on non-Unix builds instead of silently falling back.

The UDS half (src/conns/uds.rs) namespaces sockets per UID under `$XDG_RUNTIME_DIR/pneumatic` with mode 0700 enforced and re-verified, and `prepare_socket_path` is fail-closed: a pre-existing symlink is rejected, a stale socket file is cleaned up. This is how local service-to-service traffic avoids the world-writable-relative-path hijack the old `"data"` path allowed (src/data.rs:76-78).
