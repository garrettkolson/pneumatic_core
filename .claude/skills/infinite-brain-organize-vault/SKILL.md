---
name: infinite-brain-organize-vault
description: Audit the Infinite Brain vault for health issues — orphan nodes, contradictions, stale confidence, broken edges, index drift — and apply fixes after confirmation. Use when asked to organize the vault, audit the knowledge graph, or run vault health checks.
---

# Organize Vault

Audit the knowledge graph for health issues and apply confirmed fixes.

## When to use
- "Organize the vault"
- "Audit the knowledge graph"
- "Check the vault's health"
- "Fix broken edges / orphan nodes"

## Steps
1. Read `_system/INDEX.md` for the current state baseline.
2. Run these checks (report all findings before fixing):
   - **Orphan Census** — nodes with zero edges and zero `related`; suggest 2-3 connection targets per orphan.
   - **Contradiction Scan** — conflicting claims within the same namespace; wire `contradicts` edges where missing.
   - **Confidence Gaps** — missing/0.0 confidence, or high confidence with a triggered staleness signal.
   - **Stale Node Detection** — `verified_at` older than 90 days. Priority order: pillars > decisions > facts > patterns > hypotheses.
   - **Cross-Link Opportunities** — nodes sharing 2+ tags with no edge between them.
   - **Taxonomy Health** — inconsistent tag spellings, near-duplicate tags, tags outside the established vocabulary.
   - **Visibility Health** — missing `visibility`, or visibility conflicting with content sensitivity.
   - **Summary Quality** — summaries over 200 chars, placeholder text, or content mismatch with body.
   - **Edge Integrity** — edges pointing to non-existent node ids; index rows with no file, or files with no index row.
3. Deliver a report:
   ```
   ## Vault Organization Report
   ### Summary
   <counts per check>
   ### Priority Actions
   1. [high] <action> — <why>
   2. [med] <action> — <why>
   ### Details
   | File | Issue | Suggested Fix |
   ```
4. Ask the user which actions to execute — never auto-fix without confirmation.
5. Apply confirmed fixes: edit node frontmatter, add missing edges, repair the index, dedupe tags.
6. Write a log node:
   - File: `logs/log-organize-vault-YYYYMMDD-HHmmss.md`
   - `operation: organize-vault`, affected nodes, summary of changes

## Rules
- Never delete nodes during organization — restructure, relink, or flag instead.
- Every fix must preserve the meaning of the node.
- After fixes, the index must exactly match the files on disk.
