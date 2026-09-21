---
id: question-external-audit-before-real-value
title: "External audit of the Action circuit before real value"
type: question
namespace: pneumatic
visibility: namespace
summary: "Open: before shielded transfers touch real value, the Action circuit needs external review/audit — a silently-accepting invalid proof is a zk constraint bug, a different risk class from a Rust bug."
auto_inject: false
applicable_when: "Planning the real-value/mainnet milestone, budgeting security work, or scoping S6 exit criteria"
confidence: 0.9
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Resolve (delete/answer) when an external circuit audit is commissioned or completed"
tags: [open-question, security, zk, audit, circuit]
edges:
  - target: source-shielded-plan
    type: derived_from
    weight: 1.0
    note: "Open question 1 (lines 1017-1019)"
  - target: concept-action-circuit
    type: related_to
    weight: 0.9
    note: "The artifact requiring external review"
  - target: pillar-shielded-value-transfer
    type: depends_on
    weight: 0.7
    note: "Gates the 'real value' deployment milestone"
  - target: decision-halo2-no-trusted-setup
    type: related_to
    weight: 0.6
    note: "Halo2's no-trusted-setup posture is part of the assurance story"
  - target: question-viewing-keys-compliance
    type: related_to
    weight: 0.5
    note: "Sibling open product/security question in the same Part-6/7 list"
related: []
source_url: "repo:pneumatic-shielded-implementation-plan.md"
---

# External audit of the Action circuit before real value

The implementation plan's "Open questions to flag to the roadmap owner" (lines 1014–1028) lists this first: **before the shielded stack touches real value, the Action circuit needs external review/audit**, because "zk constraint bugs are a different risk class from Rust bugs — silently-accepting invalid proofs."

The asymmetry is the crux: a Rust bug panics or fails a test, but a constraint bug can mint value from nothing while every local test stays green — the repo's own history of wire-framing/nonce/RNG audit findings shows the project is sensitive to exactly this class of silently-accepted error. The roadmap's Part-6 open problems pair it with an explicit **external-audit budget** question.

Until answered, the default engineering posture is: ship test vectors + adversarial coverage (S6.2's six rejection cases) as the in-repo assurance, and treat external audit as a precondition of the real-value milestone, not a post-hoc item.
