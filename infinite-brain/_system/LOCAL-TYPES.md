# Local Custom Types

This file documents any `custom` node types created for domain-specific needs that fall outside the 16 canonical types defined in `NODE-TYPES.md`.

---

## Registration Format

To add a custom type, create an entry below with:

- **Type name:** lowercase, singular (e.g., `product`, `metric`)
- **Folder:** `custom/` (all custom types go here)
- **Rationale:** why the 16 canonical types don't fit
- **Usage scope:** which namespace(s) use this type

---

## Registered Types

### module
- **Type name:** `module` (used as `type: custom` with tag `module`)
- **Folder:** `custom/`
- **Rationale:** A Rust module/crate is a code artifact with a stable identity (path, public API, dependencies) that does not fit cleanly into `concept` (too concrete) or `fact` (too stateful). Module nodes track a code unit's current responsibility and public surface, and are the natural attachment points for `part_of` edges.
- **Usage scope:** `pneumatic`

---
