---
id: pattern-authenticated-voter-c1
title: "C1 pattern — credit only keys proven by the envelope + role gate"
type: pattern
namespace: pneumatic
visibility: namespace
summary: "Credit only keys proven by the envelope signature + a registered-role gate, never self-reported body keys; fail closed on any anomaly. Used by finalizer, committer, and sentinel inbound handlers."
auto_inject: false
applicable_when: "Adding a new inbound message handler in any role crate, reviewing signature/vote handling, or auditing identity-trust decisions"
confidence: 0.95
verified_at: "09/23/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale when the C1-style checks in finalizer/signing.rs handle_signature / finalizer/shielded.rs handle_sign_shielded, committer.rs authenticate_message, or sentinel/processing.rs handle_process_request change"
tags: [authentication, identity, fail-closed, message-handling, cross-role]
edges:
  - target: pattern-fail-closed
    type: related_to
    weight: 0.9
    note: "C1 is the identity-specific application of fail-closed: any verification anomaly rejects the whole message"
  - target: concept-finalizer-role
    type: related_to
    weight: 0.9
    note: "handle_signature (C1, finalizer/signing.rs:113-153) and the shielded arms (authenticate_shielded_message, shielded.rs:14-54)"
  - target: concept-committer-role
    type: related_to
    weight: 0.9
    note: "authenticate_message: envelope check + registration + action/role gate (committer.rs:267-335)"
  - target: concept-sentinel-role
    type: related_to
    weight: 0.85
    note: "C3 sender auth in handle_process_request — envelope sender must equal tx.sender"
  - target: concept-optimistic-finality
    type: supports
    weight: 0.8
    note: "This is what makes the first-signature optimistic commit safe: an attacker cannot forge a registered executor's signature to trigger it (finalizer/signing.rs:150-155)"
related: []
source_url: "Empty"
---

# C1 pattern — credit only keys proven by the envelope + role gate

A reusable cross-role authentication pattern visible in all four worker crates' inbound handlers. The rule: **the public key that enters consensus-relevant state is the key *proven* by the envelope signature and *confirmed* by a registration role check — never a key self-reported inside the message body.** Any anomaly (bad signature, unregistered key, wrong role) rejects the whole message before any other work.

Instances (code-verified):

- **Finalizer, executor votes** (`finalizer/src/finalizer/signing.rs:113-153`): the comment marks it `(C1)`. Envelope signature verified against `message.public_key`; that key must be a registered `Executor`; then the *inner* signature is checked over the claimed `transaction_hash` with the negated result (a bare `?` would silently accept a failing verify — the code spells this out). Voter stake is stamped from the epoch snapshot, never the message's self-reported field.
- **Finalizer, shielded arms** (`finalizer/src/finalizer/shielded.rs:14-54`): same shape, `Finalizer` role gate, including composite identities registered under several roles.
- **Committer router** (`committer/src/committer.rs:267-335`): envelope check (defense-in-depth with the gossiper's upstream check), registration lookup resolving the full role set, then an action→allowed-roles gate with `SelfOnly`/`AnyRegistered` variants.
- **Sentinel admission** (`sentinel/src/sentinel/processing.rs`, C3 comment in `handle_process_request`; post-09/23 modularization): non-empty sender, sender's signature over canonical tx bytes, and authenticated envelope sender == `tx.sender` — so a peer cannot debit an account it doesn't own.

Why it matters: it is the precondition that makes single-signature optimistic finality and stake-weighted quorum trustworthy, and it keeps the composite node's multi-role identities from cross-contaminating role privileges.
