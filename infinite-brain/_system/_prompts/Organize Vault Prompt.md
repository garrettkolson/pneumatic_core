# Organize Vault Prompt

Audit the knowledge graph for health issues.

1. Read `_system/INDEX.md` for the current state baseline.
2. Run these checks:
   - **Orphan Census** — nodes with zero edges and zero `related`; suggest 2-3 connection targets per orphan.
   - **Contradiction Scan** — conflicting claims within the same namespace.
   - **Confidence Gaps** — missing/0.0 confidence, or high confidence with a triggered staleness signal.
   - **Stale Node Detection** — `verified_at` over 90 days old. Priority: pillars > decisions > facts > patterns > hypotheses.
   - **Cross-Link Opportunities** — nodes sharing 2+ tags with no edge.
   - **Taxonomy Health** — inconsistent tag spellings, near-duplicate tags.
   - **Visibility Health** — missing `visibility` or visibility conflicting with content sensitivity.
   - **Summary Quality** — summaries over 200 chars, placeholder text, or content mismatch.
3. Deliver a report: summary counts, top-5 priority actions with `[high]`/`[med]` labels, then a details table (file | issue | suggested fix).
4. Ask which actions to execute — never auto-fix without confirmation.
5. Write a log node to `logs/log-organize-vault-YYYYMMDD-HHmmss.md` after fixes are applied.
