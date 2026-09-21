---
name: infinite-brain-init-vault
description: Scaffold a fresh Infinite Brain knowledge vault with typed nodes, edge schema, agent prompts, and templates. Use when asked to create an Infinite Brain vault, initialize a knowledge graph, or set up an obsidian-infinite-brain style vault.
---

# Init Vault

Scaffold a fresh Infinite Brain vault in the target directory.

## When to use
- "Create an infinite brain vault"
- "Initialize a knowledge graph vault"
- "Set up obsidian-infinite-brain"
- Creating a vault from scratch in a new directory

## Inputs
Ask if not given:
- **target directory** (default: current directory)
- **namespace** for initial example nodes (default: the project name)

## Vault anatomy
18 top-level folders (16 content types + `raw` + `logs`):
```
pillars/ decisions/ concepts/ questions/ playbooks/ tasks/ events/ patterns/
hypotheses/ facts/ sources/ bookmarks/ notes/ contacts/ references/ custom/
raw/ (with raw/processed/)  logs/  _system/  _templates/
```
- 16 content node types: `pillar, decision, concept, question, playbook, task, event, pattern, hypothesis, fact, source, bookmark, note, contact, reference, custom`
- 1 auxiliary type: `log` (audit trail, never indexed, never decayed)
- 10 edge types: `related_to, depends_on, derived_from, contradicts, supports, part_of, preceded_by, followed_by, authored_by, tagged_with`

## Steps
1. Create the 18 root folders + `raw/processed/`.
2. Create `.gitkeep` in each empty folder so git tracks them.
3. Create `_system/` files:
   - `INDEX.md` — master node index, one table per type, empty initially
   - `NODE-TYPES.md` — the 17 type definitions
   - `EDGE-TYPES.md` — the 10 edge definitions
   - `FRONTMATTER-SCHEMA.md` — the frontmatter field spec (id, title, type, namespace, visibility, summary, auto_inject, applicable_when, confidence, verified_at, verified_by, staleness_signal, tags, edges, related, source_url)
   - `LOCAL-TYPES.md` — register any project-specific custom node types
   - `AGENTS.md` — agent operating instructions for this vault
   - `_prompts/` — Create Vault, Convert Note, Query Vault, Organize Vault prompts
4. Create `_templates/Template - Infinite Node.md` with empty frontmatter.
5. Create two example nodes in the chosen namespace (`pillars/pillar-<ns>-foundation.md`, `decisions/decision-<ns>-first.md`) wired with a `supports` edge.
6. Update `_system/INDEX.md` with both example nodes.
7. Confirm: folder count, example nodes wired, next step (`/infinite-brain-convert-note` to ingest content).

## Frontmatter schema (all nodes)
```yaml
id: <type>-<kebab-slug>
title: "Human-readable title"
type: <one of the 16 content types or a registered custom type>
namespace: <namespace>
visibility: public | namespace | private | system
summary: "1-2 sentences for AI scanning."
auto_inject: false
applicable_when: "Condition for relevance" or "Empty"
confidence: 0.0-1.0
verified_at: "MM/DD/YYYY" or "Empty"
verified_by: "name" or "Empty"
staleness_signal: "Observable condition that makes this stale"
tags: []  # 2-8 kebab-case tags
edges: []
related: []
source_url: "URL" or "Empty"
```

## Edge format (in node frontmatter)
```yaml
edges:
  - target: <node-id>
    type: <edge-type>
    weight: 0.0-1.0
    note: "Why this edge exists"
```

## Rules
- All node files must be valid Obsidian-compatible markdown with YAML frontmatter.
- Every non-log node gets a row in `_system/INDEX.md`.
- Node id must be globally unique across the vault (check INDEX before creating).
- Never create a node with zero edges.
