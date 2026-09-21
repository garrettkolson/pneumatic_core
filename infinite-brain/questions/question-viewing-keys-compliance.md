---
id: question-viewing-keys-compliance
title: "Viewing keys / compliance: open product decision"
type: question
namespace: pneumatic
visibility: namespace
summary: "Unresolved: should recipients (merchants, auditors, compliance) receive viewing keys that can decrypt note ciphertexts? Roadmark flags this as a product decision, not an engineering one."
auto_inject: false
applicable_when: "Scoping S6 work, designing wallet/recipient APIs, or discussing compliance features"
confidence: 0.8
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Resolve (delete/answer) when the roadmap records a viewing-key/compliance decision"
tags: [viewing-keys, compliance, privacy, open-question]
edges:
  - target: source-shielded-roadmap
    type: derived_from
    weight: 1.0
    note: "Listed as an open item"
  - target: pillar-shielded-value-transfer
    type: depends_on
    weight: 0.7
    note: "Shapes the remaining shielded phases"
  - target: concept-note-commitment
    type: related_to
    weight: 0.7
    note: "Viewing keys would operate on the note ciphertexts of commitments"
related: []
source_url: "Empty"
---

# Viewing keys / compliance: open product decision

The roadmap lists **viewing keys / compliance** as an open item that is explicitly a *product* decision, not an engineering one: should note recipients (merchants, auditors, regulators) be able to receive a viewing key that lets them decrypt incoming note ciphertexts and watch the shielded balance — at the cost of breaking unilateral privacy against that recipient?

Until answered, engineering work should keep note ciphertexts decryptable by a key the sender can distribute (the current design supports this), while not hardwiring a viewing-key policy. This question gates part of the remaining S-phase work.
