# Convert Note Prompt

Ingest raw content from `raw/` into atomic typed nodes.

1. Pick the target file(s) in `raw/` (or "all"). Treat source files as immutable.
2. Decompose into atomic nodes — one concept per node, 50-300 words each.
3. For each node: classify with exactly one of the 16 content types; assign a unique `id` in `type-descriptive-slug` format; populate all frontmatter fields per `_system/FRONTMATTER-SCHEMA.md`; wire to at least one other node using the 10 edge types; place in the correct folder (`type: concept` → `concepts/<id>.md`).
4. Write each node file.
5. Append each new node's row to `_system/INDEX.md` under the correct type section.
6. Move processed source files from `raw/` to `raw/processed/`.
7. Write a log node to `logs/log-convert-note-YYYYMMDD-HHmmss.md` (operation `convert-note`, affected nodes, one-sentence summary, 30-80 word body).

Rules: never merge concepts (err toward more atomic nodes); summaries under 200 chars; confidence reflects actual certainty; `visibility` defaults to `namespace`; `staleness_signal` must be a specific observable condition; check `_system/INDEX.md` first to avoid duplicate ids.
