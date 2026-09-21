---
id: concept-validation-specs
title: "Validation spec framework: traits, registries, fail-closed lookup"
type: concept
namespace: pneumatic
visibility: namespace
summary: "TransactionValidationSpec/BlockValidatorSpec traits with name-keyed registries; SelfSigned vs Executed specs; unregistered specs and unknown shielded actions fail closed."
auto_inject: false
applicable_when: "Adding a validation spec, changing tx/block validation, wiring the shielded spec, or debugging rejections"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If the trait surfaces, registry lookup semantics, or SelfSigned/Executed check sets change"
tags: [validation, specs, registry, fail-closed]
edges:
  - target: concept-transaction-lifecycle
    type: supports
    weight: 0.85
    note: "This is the framework the shielded spec plugs into (validate_shielded override, validation.rs:44-51)"
  - target: pattern-fail-closed
    type: supports
    weight: 0.9
    note: "Unknown spec name, missing owner, unknown shielded action all reject rather than pass"
  - target: concept-per-token-chains
    type: related_to
    weight: 0.75
    note: "Tokens name their block spec (tokens.rs:38) and lookup is per-token (tokens.rs:174-191)"
  - target: concept-env-driven-config
    type: related_to
    weight: 0.7
    note: "Spec registries live on EnvironmentMetadata and are populated from env spec name lists"
related: []
source_url: "Empty"
---

# Validation spec framework: traits, registries, fail-closed lookup

`TransactionValidationSpec` (`src/validation.rs:23`) is the action-based validation trait: `validate(tx, token, env) -> TransactionValidationResult` (with risk metrics + assigned finalizer), `calculate_risk`, `name`, and `validate_shielded` whose **default implementation fails closed** with `UnsupportedAction` (lines 44-51) — only `ShieldedValidationSpec` overrides it.

Two concrete specs: `SelfSignedBlockValidatorSpec` (line 62, for owner-is-authority tokens — sender must equal the hex-encoded owner in token metadata, lines 80-110; block side requires chain integrity + `is_self_verified` flag, lines 128-150) and `ExecutedBlockValidatorSpec` (line 160 — sender present, amount present and nonzero, `sequence_number == 0` rejected as `InvalidNonce` at lines 207-208, min-stake and `max_risk` gates; block side requires `result_hash` + executor signatures + finalizer signature, lines 248-278).

Lookup is name-keyed: `ValidationSpecRegistry` (line 309) for transactions, `BlockValidatorSpecRegistry` (line 599) for blocks, both storing `Arc<dyn Spec>`; `register_defaults` (line 333) installs SelfSigned + Executed, `register_shielded` (line 344) installs the shielded spec separately so non-shielded deployments never build the Halo2 verifying key. Registries live on `EnvironmentMetadata`, populated from each environment's spec name lists. `BlockValidatorSpec` (line 286) is the block-level counterpart used by Committers/Archivers. There is **no accept-all fallback** anywhere in the chain: a name that resolves to nothing rejects the input.
