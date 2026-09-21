---
id: pattern-verify-before-dedup
title: "Verify before dedup: never admit unverified content to a cache"
type: pattern
namespace: pneumatic
visibility: namespace
summary: "Receivers run full verification (signature/envelope/fingerprint) BEFORE inserting into any dedup/corruption cache, so rejected input can never poison a slot a legitimate message would use."
auto_inject: false
applicable_when: "Adding a cache, dedup mechanism, or corruption check on any receive path"
confidence: 0.9
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the Gossiper cache-insert ordering or the snapshot envelope verify flow changes order"
tags: [pattern, security, dedup, cache-poisoning, verification]
edges:
  - target: concept-gossip-fanout
    type: supports
    weight: 1.0
    note: "handle_message verifies the envelope signature, then (and only then) inserts the dedup key"
  - target: concept-data-provider
    type: supports
    weight: 0.8
    note: "StakeSnapshotEnvelope.verify() gates trust of deserialized snapshot bytes"
  - target: pattern-fail-closed
    type: related_to
    weight: 0.7
    note: "Sibling discipline: ambiguous input is rejected, not guessed"
related: []
source_url: "Empty"
---

# Verify before dedup

A recurring discipline across pneumatic's receive paths: **a message is inserted into a dedup / negative-result cache only after it has fully verified**, so forged or corrupted input can never occupy the slot a legitimate message with the same key would use.

Primary instance — `Gossiper::handle_message` (src/gossiper.rs:87-123): the envelope signature check runs *before* the dedup cache is touched (gossiper.rs:105-110); a failed check returns `InvalidSignature` with no cache write, and the cache insert (gossiper.rs:123) happens strictly after. The doc comment states the threat directly: a forged message must not "occupy the cache slot that a legitimate message with the same key/body would use" — otherwise one forgery attempt would permanently suppress the honest message for the whole TTL. This pairs with a **content-keyed** dedup key (`sha256(sender_key) || sha256(body)`, gossiper.rs:140-147) rather than signature bytes, so verification randomness (ML-DSA's fresh nonce) can't defeat dedup while verification stays the admission gate.

Second instance — `DataProvider::get_stake_snapshot` (src/data.rs:237-250): the stored envelope's SHA-256 fingerprint is verified (`env.verify()`) before the payload is trusted; a mismatch surfaces as `SnapshotCorrupt` instead of silently accepting deserialized bytes. The test suite locks both in (gossiper.rs:611-629 tampered-body rejection; data.rs:752-765 corrupted-snapshot rejection).
