---
id: concept-gossip-fanout
title: "Gossiper: verify-then-dedup fan-out with a content-keyed TTL cache"
type: concept
namespace: pneumatic
visibility: namespace
summary: "Gossiper deserializes, verifies the envelope signature, dedups on sha256(sender_key)||sha256(body) in a 10k-entry TTL moka cache, then fans out to handlers; send_to_type broadcasts by registry type."
auto_inject: false
applicable_when: "Touching src/gossiper.rs, message routing between node types, or tuning dedup TTL/capacity"
confidence: 0.95
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the handle_message order (verify before cache insert) or the dedup_key formula in src/gossiper.rs changes"
tags: [gossip, dedup, fanout, moka, signature]
edges:
  - target: pattern-verify-before-dedup
    type: supports
    weight: 1.0
    note: "The primary instance of the verify-before-dedup pattern"
  - target: concept-data-provider
    type: related_to
    weight: 0.8
    note: "Both operate on the same wire Message and DataError surface"
  - target: fact-wire-protocol
    type: related_to
    weight: 0.8
    note: "Consumes/produces the length-prefixed MsgPack Message format"
related: []
source_url: "Empty"
---

# Gossiper: verify-then-dedup fan-out

`Gossiper` (src/gossiper.rs:14-37) is the ingress/egress router for inter-node messages. It owns: a node type, a `ConnFactory`, a **moka** dedup cache (`Cache<Vec<u8>, ()>`, max capacity 10,000, configurable TTL — gossiper.rs:51-54), a `Mutex<Vec<Box<dyn Fn(Vec<u8>) + Send + Sync>>>` of handlers, and an `Arc<RwLock<dyn AsymCryptoProvider>>` for verification.

`handle_message` (gossiper.rs:91-133) runs a strict pipeline:

1. Deserialize the MsgPack `Message` — failure returns `DeserializationError`.
2. **Verify the envelope signature** (`check_signature(signature, public_key, body)`) *before* touching the dedup cache; failure returns `InvalidSignature` and the message is never admitted to the cache.
3. Compute the dedup key and silently skip if present (gossiper.rs:116-119).
4. Insert into the cache **after** verification (gossiper.rs:123), then fan out: every registered handler receives a clone of the raw bytes (gossiper.rs:127-130).

The dedup key (gossiper.rs:140-147) is `sha256(public_key) || sha256(body)` — a Merkle-style two-hash construction, deliberately **not** the signature bytes: an honest re-send collapses regardless of signature non-determinism (ML-DSA draws a fresh nonce per signature — see src/messages.rs:106-112), while two different senders with identical bodies never collide.

Outbound, `send_to_type` (gossiper.rs:154-176) iterates the `NodeRegistry`'s nodes of a given type and calls `get_response` through a caller-supplied `sender_for` closure — the production path maps each node to an `RnsSender`, while tests inject recording/scripted senders.
