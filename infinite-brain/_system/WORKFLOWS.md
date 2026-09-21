# Vault Workflows

Agent-agnostic workflow definitions for Infinite Brain automation. Each workflow can be triggered by any agent that supports scheduled tasks or cron execution. All paths are relative to the vault root `infinite-brain/` at the repository root.

---

## vault-health

**Trigger:** Weekly (recommended: Monday morning)
**Skill:** `/vault-health`
**Output:** `notes/note-vault-health-YYYYMMDD.md` + updated `_system/INDEX.md`

**What it does:**
1. Applies confidence decay to nodes not verified in 90+ days
2. Audits orphans, contradictions, stale signals, cross-link gaps, taxonomy, visibility, and summary quality
3. Writes a structured health report as a `note` node
4. Presents priority actions for human approval before executing any fix

**Two modes:**
- `/vault-health` — interactive: decay + audit + ask before each fix
- `/vault-health auto` — automated: decay + audit + report node only, zero fixes, zero prompts

**Setup by agent:**

### Claude Code
```bash
# Run once to register the weekly schedule (auto mode, no prompts):
/schedule weekly /vault-health auto
```

### GitHub Actions
Create `.github/workflows/vault-health.yml` — triggers the scheduled agent remotely via Claude Code CLI on a cron.

### Cursor / Gemini CLI / Copilot
Manually invoke `_system/_prompts/Organize Vault Prompt.md` + apply decay rules from `.claude/skills/vault-health.md` Phase 1. The skill file is readable by any agent.

---

## convert-note (on-demand, not scheduled)

**Trigger:** Manual — run after dropping files into `raw/`
**Skill:** `/convert-note`
**Output:** New typed nodes in their respective folders + updated `_system/INDEX.md`

No automation recommended — conversion requires human review of type classification.

---

## Workflow Principles

1. **Automated phases never delete.** Decay modifies `confidence`. Audit only collects. Fixes require human approval.
2. **Every automated run writes a node.** The health report in `notes/` creates an audit trail inside the vault itself.
3. **Workflows are additive.** New automation should write nodes, not overwrite them. Old health reports are preserved with their date in the ID.
