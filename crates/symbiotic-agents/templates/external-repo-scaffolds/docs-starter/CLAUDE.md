# {{project_name}} — Claude/Agent Instructions

Agent-facing rules for working within the `{{project_slug}}-docs` repository.

## Before editing

1. Read `docs/architecture.md` to understand the project's module boundaries.
2. Read the relevant subsystem doc (`docs/build.md`, `docs/test.md`, `docs/deploy.md`) for the change you're making.
3. Check for an open Source Archeology checkpoint under the triggering goal's artifacts folder — your change may already be in a Defer bucket.

## When editing

- Keep docs concise; prefer bullet lists over prose.
- Never reference files, scripts, or commands that don't exist in the source repo — the Excavate stage will flag them as `WireMismatch`.
- Mark speculative or planned content with `> **Planned:**` prefix so the Date stage classifies it correctly.

## After editing

- Run `markdownlint` if available locally — the Verify stage uses the same tool.
- If you changed build/test/deploy steps, update the corresponding `docs/*.md` in the same commit.

<!-- Symbiotic Source Archeology scaffold — placeholder tokens:
     {{project_name}}, {{project_slug}} -->
