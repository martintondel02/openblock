# Diagrams

Architecture and flow diagrams live inline in the documents that need them, as
[mermaid](https://mermaid.js.org/) code fences. Inline diagrams review in diffs,
cannot go stale without the surrounding text changing, and render on GitHub.

This directory holds standalone `.mmd` files only when a diagram is referenced
from more than one document or is too large for comfortable inline editing.

| File | Used by |
| --- | --- |
| _(none yet)_ | |

Conventions:

- Prefix files with the owning document, e.g. `01-crate-graph.mmd`.
- Keep colours and styling out of the diagram source; use the site theme.
- When a diagram is duplicated inline, the `.mmd` file is the source of truth.
