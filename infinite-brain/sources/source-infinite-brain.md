---
id: source-infinite-brain
title: "Source: obsidian-infinite-brain reference vault"
type: source
namespace: pneumatic
visibility: system
summary: "The Infinite Brain system (JotaSXBR/obsidian-infinite-brain, branch master): 17 node types, 10 edge types, frontmatter schema, decay rules, and the 5 skills this vault implements."
auto_inject: false
applicable_when: "Modifying vault mechanics (schemas, skills, decay) rather than pneumatic content"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "Stale if the reference repo changes in a way that diverges from this vault's _system docs"
tags: [infinite-brain, reference, vault-mechanics, meta]
edges:
  - target: pillar-block-lattice
    type: related_to
    weight: 0.4
    note: "Meta source: how this vault about the lattice is organized"
related: []
source_url: "https://github.com/JotaSXBR/obsidian-infinite-brain"
---

# Source: obsidian-infinite-brain

This vault implements the **Infinite Brain** knowledge-graph system from `JotaSXBR/obsidian-infinite-brain` (default branch **master** — `main` 404s). Mechanics copied/adapted into `_system/`: 16 content node types + `log` (17 total), 10 edge types, the 16-field frontmatter schema, the 8-field log schema, the confidence decay schedule (−0.1 at 91–180d, −0.2 at 181–365d, →0.1 + `needs-review` after 365d; system-visibility/Empty-verified/<30d nodes skipped), and the 5 skills (init-vault, convert-note, query-vault, organize-vault, vault-health).

Adaptations for this repo: vault root is `infinite-brain/` inside the repo; default namespace `pneumatic`; custom type `module` registered in `_system/LOCAL-TYPES.md`; AGENTS.md adds subject-specific rules (verify claims against code, cite file:line, don't record unverified claims as fact).
