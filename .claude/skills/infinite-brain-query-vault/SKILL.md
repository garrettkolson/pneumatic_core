---
name: infinite-brain-query-vault
description: Answer questions by navigating the Infinite Brain knowledge graph via typed edges and visibility filters. Use when asked to query the brain, look something up in the vault, find related nodes, or synthesize knowledge from the knowledge graph.
---

# Query Vault

Answer a question by navigating the knowledge graph via typed edges and visibility filters.

## When to use
- "What does the brain know about X?"
- "Query the vault for..."
- "Find nodes related to..."
- "What's the history of / evidence for / consequences of..."

## Steps
1. Read `_system/INDEX.md` — get the full map of existing nodes.
2. Determine scope:
   - Named namespace → prefer nodes of that namespace
   - No namespace → `public` nodes, plus `namespace` nodes when the summary clearly matches
   - Explicit private request → `private`
   - Never present `system` nodes as answer content
3. Select likely node types by question shape:
   - "why" → `pillar`, `decision`
   - "how" → `playbook`, `pattern`
   - "what if" → `hypothesis`
   - "what is" → `concept`, `fact`
   - "when/where" → `event`, `note`
   - "who" → `contact`
   - open unknowns → `question`
4. Navigate via edges — do NOT read every node:
   - `supports` / `contradicts` for polarized positions
   - `derived_from` to trace conclusions back to evidence
   - `depends_on` for prerequisite chains
5. Read only nodes whose `summary`, `applicable_when`, `namespace`, and `visibility` match the query.
6. Synthesize and answer in this format:

```
### Answer
<direct answer, 1-3 paragraphs>

### Sources
- [[node-id]] — what it contributed

### Confidence
<average of source nodes' confidence, 0.0-1.0>

### Related Nodes to Explore
- [[node-id]] — why it's relevant
```

7. Offer to save the answer as a synthesis node (type `note`) if it is novel.
8. Write a log node:
   - File: `logs/log-query-vault-YYYYMMDD-HHmmss.md`
   - `operation: query-vault`, affected nodes = nodes read, summary = the question

## Rules
- Cite node ids for every claim — no uncited synthesis.
- If the graph lacks the answer, say so explicitly and suggest which `question` node to file.
- Respect visibility filters strictly.
