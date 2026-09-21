---
id: concept-per-token-chains
title: "Per-token chains — a token is its own blockchain"
type: concept
namespace: pneumatic
visibility: namespace
summary: "Token (tokens.rs:18) embeds its own Blockchain; every commit, trim, and validation decision is per-token, so protocol effects are localized to one chain."
auto_inject: false
applicable_when: "Modifying Token, committing blocks, per-token validation, or minting token types"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If Token no longer embeds a Blockchain, or per-token validation spec lookup changes"
tags: [block-lattice, tokens, per-token, chains]
edges:
  - target: pillar-block-lattice
    type: supports
    weight: 1.0
    note: "Literal implementation of the lattice pillar: independent parallel ledgers (tokens.rs:15)"
  - target: concept-block-and-factory
    type: depends_on
    weight: 0.9
    note: "Each Token embeds a Blockchain (tokens.rs:24); commits hash/append on that chain"
  - target: concept-validation-specs
    type: depends_on
    weight: 0.85
    note: "Per-token block validation picks a spec by name (tokens.rs:174) and fails closed if unregistered"
  - target: concept-shielded-pool
    type: related_to
    weight: 0.7
    note: "Shielded opt-in is a per-token metadata flag checked before pool state is touched (tokens.rs:110)"
related: []
source_url: "Empty"
---

# Per-token chains — a token is its own blockchain

The root invariant of the block lattice: "A token IS its own blockchain — independent parallel ledgers" (`src/tokens.rs:15`). The `Token` struct (`src/tokens.rs:18`) holds `id`, `metadata`, its own `blockchain: Blockchain` (line 24), serialized `asset_data` + `asset_hash`, and policy fields: `security_level` (trim depth, line 29), `is_self_verified` (owner-is-authority tokens skip Executor/Finalizer entirely, lines 32-34), `is_non_transferable`, `block_validation_spec_name` (line 38), and `environment_id`.

Effects are localized per token because all mutation flows through the token: `Token::create_block` (line 214) and `Token::commit_block` (line 240) operate on that token's chain; a non-archiver chain prunes its own oldest block on reaching max length inside `commit_block`. `Token::validate_block` (line 164) first checks chain linkage against its own blockchain, then resolves the block-validation spec by name from the environment registry — with **no accept-all fallback**: an unregistered spec name rejects the block (fail closed, lines 174-191).

Mint paths (`TokenFactory`, lines 379-487) create token types — user, contract, proxy_auth — each with its own chain. Shielded value transfer is a per-token opt-in: only `metadata["shielded_opt_in"] == "true"` exactly enables it (line 110, `is_shielded_opt_in`), so shielded pool state is never touched for non-opt-in tokens.
