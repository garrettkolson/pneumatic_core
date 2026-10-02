---
id: question-anonymity-bootstrap-policy
title: "Anonymity bootstrap: production anonymity-set / genesis policy for the shielded tree"
type: question
namespace: pneumatic
visibility: namespace
summary: "Open operational decision (audit close item #4): the Merkle tree starts at one leaf in tests — how many notes must exist before a transfer is meaningfully 'anonymous', and does genesis seed a synthetic anonymity set? Undecided; nothing in code prescribes it."
auto_inject: false
applicable_when: "Planning the shielded real-value launch, writing a genesis spec, or assessing the anonymity guarantees actually provided at epoch 0"
confidence: 0.9
verified_at: "10/01/2026"
verified_by: "dsh-agent"
staleness_signal: "A recorded decision (decision node) or a genesis policy landing in the committer's pool-seeding path (ShieldedPoolState genesis at boot)"
tags: [question, shielded, anonymity, genesis, open-decision]
edges:
  - target: question-external-audit-before-real-value
    type: related_to
    weight: 0.7
    note: "Sibling launch gate from the same audit 'Open items at close' list"
  - target: question-viewing-keys-compliance
    type: related_to
    weight: 0.6
    note: "Both decide what 'private' actually means for shielded transfers in production"
related: []
source_url: "repo:AUDIT_CHECKLIST.md — S-phase 'Open items at close' #4"
---

# Anonymity bootstrap: production anonymity-set / genesis policy

`AUDIT_CHECKLIST.md` (Phase S close, "Open items at close" #4) flags this as a non-code
decision gate: the shielded Merkle tree **starts at one leaf in every test**, so at
genesis a transfer's anonymity set is trivially small — an observer can correlate
spends against a near-empty tree. The question: how many notes must exist before a
transfer can be considered "anonymous" in practice, and does the launch genesis seed
synthetic/placeholder leaves (or an epoch-0 distribution) to build that set?

Why it's load-bearing: the pool's boot contract (S5.3) already treats the committed
tree as authoritative — genesis seeding interacts directly with the
fail-closed/re-seed-refusal logic in `DefaultDataProvider::get_shielded_pool`, so an
undevised policy risks either a weak anonymity set at launch or ad-hoc genesis hacks
in deployment tooling. Nothing in the codebase prescribes a policy; the engineering
constraint (pool state is persisted and never silently re-seeded) is already in place.
