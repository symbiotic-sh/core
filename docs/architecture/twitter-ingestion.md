# Twitter / X Ingestion

## Overview

The Twitter/X ingestion pipeline converts status URLs into structured thread markdown for Archive storage. The **primary path** is the official X API (Task 30). The **fallback path** is a browser‑session fetch (Playwright) once credentials exist. For MVP tests, the daemon uses deterministic file/fixture fallbacks and placeholder thread output.

**Status (2026-04-20)**: OAuth runtime and bookmarks API wiring are implemented (API + browser/file fallback). Live thread API fetch is implemented with conversation lookup; fixture/browser fallback remains available. The crate was renamed `symbiotic-ingestion` -> `symbiotic-intake` on 2026-02-25; paths below reflect the current layout.

## Components

| File | Purpose |
| --- | --- |
| `submodules/runtime/crates/symbiotic-intake/src/twitter.rs` | `TwitterFetcher` contract (API + fallback) |
| `submodules/runtime/services/symbiotic-daemon/src/lib.rs` | Daemon fetcher routing + stub fallback |

## Data Flow

```mermaid
flowchart TD
    URL[Status URL] --> Parse[Extract handle + tweet_id]
    Parse --> XAPI[X API Client (Primary)]
    Parse --> Fallback[Browser/Fixture Fallback]

    XAPI -->|Thread| Thread[Thread Markdown]
    Fallback -->|Thread| Thread
    Thread --> Archive[Archive Store]
    Archive --> Review[Archive Review Queue]
```

## X API Access

### Login / OAuth

- OAuth 2.0 PKCE initiated in the Symbiotic app (“Connect X”).
- Tokens stored in the credential vault (never exposed to cloud LLMs).
- Refresh handled by credential gateway; failures escalate to `#alerts`.

### Access Modes (Beta)

1. **BYOK (default)**: user connects their own X API app via OAuth PKCE.
2. **Managed API passthrough (paid add‑on)**: Symbiotic uses its own X API app for *public* tweet/thread fetches.

**Note:** Bookmarks are user‑private. Accessing bookmarks requires user OAuth (BYOK) or browser session fallback.

## Fallback Behavior (MVP)

When API credentials are missing:

- Bookmarks sync falls back from API -> API fixture -> browser fixture.
- Status/thread fetch uses live X API lookup + conversation query when OAuth token is available.
- If API fails, daemon falls back to fixture/browser fallback and then deterministic placeholder content.
- Full browser fallback for thread fetch is documented in `docs/architecture/browser-automation.md`.

## Bookmark Ingestion (API or Browser)

Bookmarks are **private** and require user authorization. Daemon behavior:

1. Try X API (`/2/users/me/bookmarks`) using the vault record keyed by service `x-oauth`.
2. If API fails, use deterministic file fallback:
   - `data/intake/bookmarks-api.txt`
   - `data/intake/bookmarks-browser.txt`

See `docs/architecture/ingestion-pipeline.md` for the queue flow.

## Key Decisions

1. **X API primary**: Official API for thread fetches, with OAuth and scope control.
2. **No third‑party scrapers**: FxTwitter/Nitter are removed.
3. **Fallback required**: Browser/fixture fallback preserves ingestion when API is unavailable.
4. **Links returned, not fetched**: Twitter module extracts content; the intake pipeline handles recursive fetch.

## Related Docs

- `docs/architecture/ingestion-pipeline.md`
- `docs/architecture/browser-automation.md`
- `docs/architecture/credential-sandbox.md`
