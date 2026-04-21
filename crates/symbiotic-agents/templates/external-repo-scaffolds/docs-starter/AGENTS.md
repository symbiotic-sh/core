# {{project_name}} — Agent Roles

Role map for agents operating on `{{project_slug}}`. Replace with project-specific roles during onboarding.

## Active Roles

*(to be filled in during Source Archeology onboarding)*

| Role | Tier | Scope | Writes to |
|---|---|---|---|
| `inspector` | `fast` | read-only | — |
| `reconciler` | `deep` | `docs/**` | `docs/*.md` |
| `scaffolder` | `deep` | scaffold templates | `{{project_slug}}-docs` |

## Conventions

- Tier names per `docs/NAMING-CANON.md` §Model Tiers (`fast` / `balanced` / `deep`).
- Scope is the minimum set of paths this role's workers can read/write, enforced by the sandbox write-scope.
- Role additions MUST land alongside an `archeology_policy` update on the repo manifest if they change triage thresholds.

<!-- Symbiotic Source Archeology scaffold — placeholder tokens:
     {{project_name}}, {{project_slug}} -->
