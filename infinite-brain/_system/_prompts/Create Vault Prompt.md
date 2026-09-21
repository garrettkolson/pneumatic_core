# Create Vault Prompt

Scaffold a fresh Infinite Brain vault from scratch in the target directory (default: current directory).

1. Create the 18 root folders: `pillars decisions concepts questions playbooks tasks events patterns hypotheses facts sources bookmarks notes contacts references custom raw logs _system _templates` plus `raw/processed`.
2. Create `.gitkeep` in each empty folder so git tracks them.
3. Create `_system/` files: `INDEX.md` (master node index with an empty table per type), `NODE-TYPES.md`, `EDGE-TYPES.md`, `FRONTMATTER-SCHEMA.md`, `LOCAL-TYPES.md`, `AGENTS.md`, and `_prompts/`.
4. Create `_templates/Template - Infinite Node.md` with empty frontmatter.
5. Create two example nodes in the chosen namespace (`pillars/pillar-[namespace]-foundation.md`, `decisions/decision-[namespace]-first.md`) wired with a `supports` edge.
6. Update `_system/INDEX.md` with both example nodes.
7. Confirm: folder count, example nodes wired, next step (`/convert-note` to ingest content).

All nodes must follow `_system/FRONTMATTER-SCHEMA.md` exactly.
