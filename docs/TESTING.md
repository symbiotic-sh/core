# Testing Guide

How to run tests across the Symbiotic project.

## Testing Tiers — READ THIS FIRST

| Tier | Name | What It Tests | Sufficient? |
|------|------|---------------|-------------|
| 1 | **Unit / Widget** | Individual functions, widgets, parsers in isolation | No — proves components work alone |
| 2 | **Integration** | Components wired together with mocked external services | No — mocks hide real protocol/format mismatches |
| 3 | **E2E (real)** | Full user flow: app UI → Matrix → daemon → real LLM → response in app | **Yes — this is the real test** |

**The only test that proves the system works is Tier 3.** If a user can't submit a goal from the app and see it execute end-to-end, nothing else matters. Tier 1 and 2 are safety nets, not proof of correctness.

### What "E2E" means in this project

An E2E test MUST exercise the full pipeline with **real external services** (real Matrix server, real LLM API). If it uses mocked events, stubbed responses, or pre-crafted payloads, it is NOT an E2E test — it is an integration test (Tier 2) at best.

**Before claiming any flow "works", you must:**
1. Run `./scripts/run-local.sh` (starts real stack)
2. Manually or programmatically exercise the flow from the app UI
3. Verify the result appears in the app with correct content

**False confidence warning:** Previous "E2E" tests used pre-crafted Matrix events and never exercised the real deliberation pipeline with a live LLM. This gave a false sense of completion while the real flow had multiple integration bugs (wrong command names, invalid status codes, missing goal_id propagation). Always test the real flow.

## Quick Reference

| What | Command |
|------|---------|
| Rust unit tests (all) | `./scripts/cargo-test.sh` |
| Rust unit tests (one crate) | `./scripts/cargo-test.sh symbiotic-queue` |
| Rust lint | `./scripts/cargo-clippy.sh` |
| Rust format check | `./scripts/cargo-fmt.sh --check` |
| Vault canonical-record lint | `./scripts/lint-vault.sh` |
| Flutter unit/widget tests | `./scripts/flutter-test.sh` |
| Flutter analyze | `./scripts/flutter-test.sh --analyze` |
| E2E test (any) | `./scripts/run-e2e.sh <test-name>` |
| E2E smoke test | `./scripts/run-e2e.sh smoke --start` |
| E2E inquisition test | `./scripts/run-e2e.sh inquisition --start` |
| Queue integration test | `cargo test --manifest-path submodules/runtime/tests/integration/Cargo.toml --test queue_lifecycle` |

## Rust Tests

Run from repo root using the wrapper scripts (never `cd` into submodules):

```bash
# All workspaces
./scripts/cargo-test.sh

# Specific crate
./scripts/cargo-test.sh symbiotic-queue
./scripts/cargo-test.sh symbiotic-daemon --lib

# Integration tests
cargo test --manifest-path submodules/runtime/tests/integration/Cargo.toml
cargo test --manifest-path submodules/runtime/tests/integration/Cargo.toml --test queue_lifecycle
```

## Flutter Tests

```bash
# Unit + widget tests
./scripts/flutter-test.sh

# Static analysis
./scripts/flutter-test.sh --analyze

# Specific test file
./scripts/flutter-test.sh test/widgets/plan_card_test.dart
```

## E2E Tests (Flutter + Daemon + Matrix)

E2E tests exercise the full pipeline: Flutter app → Matrix → Daemon → LLM → response back to app. They run on the iOS simulator against a local Docker stack.

### Prerequisites

1. **`.env.test.local`** — provider and API key configuration:
   ```bash
   cp .env.test.template .env.test.local
   # Edit: set SYMBIOTIC_E2E_PROVIDER and the matching API key
   ```

2. **Docker** — Conduit (Matrix server) + daemon run in Docker:
   ```bash
   # The --start flag handles this automatically, or manually:
   docker compose -f submodules/runtime/docker-compose.vps.yml \
                  -f submodules/runtime/docker-compose.local.yml \
                  up -d --build
   ```

3. **iOS simulator** — must be booted:
   ```bash
   xcrun simctl boot "iPhone 17 Pro"   # or any available device
   ```

4. **Flutter dependencies**:
   ```bash
   cd submodules/app && flutter pub get && cd -
   ```

### Running E2E Tests

```bash
# General form
./scripts/run-e2e.sh <test-name> [--start] [--teardown]

# Start Docker, run smoke test
./scripts/run-e2e.sh smoke --start

# Run inquisition test (Docker already running)
./scripts/run-e2e.sh inquisition

# Full lifecycle: start → test → tear down
./scripts/run-e2e.sh inquisition --start --teardown

# Run any test file by name
./scripts/run-e2e.sh e2e_llm_smoke_test.dart
./scripts/run-e2e.sh live_connection_test.dart
```

### Shorthand Aliases

| Alias | Test File |
|-------|-----------|
| `smoke` | `e2e_llm_smoke_test.dart` |
| `inquisition` | `e2e_inquisition_test.dart` |
| `deliberation` | `e2e_deliberation_test.dart` |
| `full-goal` / `full` | `e2e_full_goal_test.dart` |

### What the Script Does

1. Loads `.env.test.local` (provider, API key, model)
2. Optionally starts Docker stack (`--start`) and waits for Conduit + daemon health
3. Uninstalls stale app from simulator (prevents keychain/crypto issues)
4. Streams daemon logs to `.debug-session/`
5. Runs `flutter test` with `--dart-define` flags for credentials + provider
6. Captures simulator screenshot
7. Prints summary with log paths

### Output

All artifacts go to `.debug-session/e2e-<test>-<timestamp>/`:
- `flutter.log` — Flutter test output
- `daemon.log` — Daemon logs during the test
- `screenshot.png` — Simulator screenshot at test end

### Available E2E Tests

| Test | What It Exercises |
|------|-------------------|
| `e2e_llm_smoke_test.dart` | App → Matrix → Daemon → LLM → response (basic pipeline) |
| `e2e_inquisition_test.dart` | NL goal → deliberation → [question/plan/auto-execute] → completion |
| `e2e_deliberation_test.dart` | NL goal → deliberation classification → auto-execute → step events → completion (2 goals) |
| `e2e_full_goal_test.dart` | Multi-phase: 2 goals via deliberation + goals list verification |
| `live_connection_test.dart` | Matrix connection + room discovery |
| `submit_url_test.dart` | URL intake submission flow |
| `goal_command_test.dart` | Goal command submission |
| `ingest_roundtrip_test.dart` | Full ingest → archive round trip |

### Troubleshooting

**"Matrix server not reachable at localhost:8008"**
→ Start the Docker stack: `./scripts/run-e2e.sh <test> --start`

**"No booted simulator found"**
→ Boot one: `xcrun simctl boot "iPhone 17 Pro"`

**".env.test.local not found"**
→ `cp .env.test.template .env.test.local` and fill in API key

**App fails to connect / crypto errors**
→ The script auto-uninstalls the app, but if issues persist:
`xcrun simctl uninstall booted sh.symbiotic.mobile`

**Test times out waiting for events**
→ Check daemon logs: `tail -50 .debug-session/e2e-*/daemon.log`
→ Verify provider API key is valid and not rate-limited

## Adding New E2E Tests

1. Create `submodules/app/integration_test/<name>_test.dart`
2. Gate on `SYMBIOTIC_E2E_REAL_KEYS=true`:
   ```dart
   const _realKeysRaw = String.fromEnvironment('SYMBIOTIC_E2E_REAL_KEYS');
   // skip: _realKeysRaw != 'true'
   ```
3. Use `IntegrationTestWidgetsFlutterBinding.ensureInitialized()`
4. Optionally add a shorthand alias in `scripts/run-e2e.sh`
5. Run with: `./scripts/run-e2e.sh <name>`
