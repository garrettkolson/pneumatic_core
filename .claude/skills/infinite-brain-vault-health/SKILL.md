---
name: infinite-brain-vault-health
description: Run the Infinite Brain vault health routine — confidence decay, staleness audit, and a health report. Auto mode performs decay and audit only, never fixes. Use when asked to run vault health, decay node confidence, or produce a vault health report.
---

# Vault Health

Run the vault health routine: confidence decay + staleness audit + report.

## When to use
- "Run vault health"
- "Decay node confidence"
- "Check which nodes are stale"
- "Generate the vault health report"

## Modes
- **auto (default)** — decay + audit + report ONLY. Never edits node content, never adds edges, never fixes issues.
- **fix** — same as auto, then applies confirmed fixes (delegate to organize-vault for the fix phase).

## Steps (auto mode)
1. Read `_system/INDEX.md` and every indexed node's frontmatter.
2. **Confidence decay** — for each node, compute age from `verified_at`:
   - 0-30 days: no change
   - 31-90 days: no change
   - 91-180 days: confidence −0.1
   - 181-365 days: confidence −0.2
   - >365 days: confidence set to 0.1, add `needs-review` tag
   - Skip: `system`-visibility nodes, nodes with `verified_at: "Empty"`, log nodes.
3. **Staleness audit** — flag every node where `staleness_signal` appears triggered by current observable facts; list them separately from age-based decay.
4. **Health report**:
   ```
   ## Vault Health Report
   ### Decay Applied
   | Node | Old | New | Reason |
   ### Needs Review
   <nodes with needs-review tag>
   ### Staleness Signals Triggered
   <node — signal — why it looks triggered>
   ### Graph Vitals
   - total nodes, total edges, orphan count, index drift count
   ```
5. Write a log node:
   - File: `logs/log-vault-health-YYYYMMDD-HHmmss.md`
   - `operation: vault-health`, affected nodes = nodes decayed, summary

## Rules
- Auto mode is read-only on node content — decay updates `confidence` (and the `needs-review` tag) and nothing else.
- Never set confidence below 0.1.
- Never decay `system`-visibility or `log` nodes.
- The report is the deliverable; fixes require explicit user confirmation (fix mode).
