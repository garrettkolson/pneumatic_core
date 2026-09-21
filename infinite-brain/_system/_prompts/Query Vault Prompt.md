# Query Vault Prompt

Answer a question by navigating the knowledge graph via typed edges and visibility filters.

1. Read `_system/INDEX.md` — get the full map of existing nodes.
2. Determine scope: named namespace → prefer nodes of that namespace; no namespace → `public` nodes, plus `namespace` nodes only when the summary clearly matches; explicit private request → `private`; never present `system` nodes as answer content.
3. Select likely node types by question shape: "why" → `pillar`/`decision`; "how" → `playbook`/`pattern`; "what if" → `hypothesis`; "what is" → `concept`/`fact`; "when/where" → `event`/`note`; "who" → `contact`; open unknowns → `question`.
4. Navigate via edges — do NOT read every node: `supports`/`contradicts` for polarized positions; `derived_from` to trace conclusions to evidence; `depends_on` for prerequisites.
5. Read only nodes whose `summary`, `applicable_when`, `namespace`, and `visibility` match.
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

7. Offer to save the answer as a synthesis node.
8. Write a log node to `logs/log-query-vault-YYYYMMDD-HHmmss.md` (operation `query-vault`, affected nodes = nodes read, summary = the question).
