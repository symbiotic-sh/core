# Shared Rust Core (`symbiotic-client`)

> **Task:** To be created
> **Decision:** Position B — shared Rust core over codegen. Multi-platform (iOS, Android, desktop, web/WASM) is the target.

---

## 1. Problem

The Flutter app duplicates ~700 lines of platform-agnostic business logic in Dart that also exists (or should exist) in Rust. Protocol types (`Kind`, `Status`) are manually copied with zero compile-time safety. Command construction is hand-rolled JSON maps across ~15 call sites. If we build for web/desktop, all this logic must be reimplemented per platform.

## 2. Solution

A new Rust crate (`symbiotic-client`) in the runtime workspace that contains all platform-agnostic client logic. Compiles to:
- `.a` / `.so` for iOS/Android (via flutter_rust_bridge FFI)
- `.dylib` for macOS/Linux/Windows desktop
- `.wasm` for web

## 3. Architecture

```
submodules/runtime/crates/
├── symbiotic-core/          (existing — protocol types, intake, temporal, memory)
├── symbiotic-client/        (NEW — client-side business logic)
│   ├── src/
│   │   ├── lib.rs
│   │   ├── parser.rs        ← EventParser::parse() — replaces Dart event_parser.dart
│   │   ├── router.rs        ← EventRouter::route() — replaces Dart event_router.dart
│   │   ├── commands.rs      ← CommandBuilder — replaces 15 hand-rolled JSON maps
│   │   ├── classifier.rs    ← InputClassifier — replaces Dart input_classifier.dart
│   │   ├── thread.rs        ← ThreadState — replaces Dart thread_model.dart
│   │   ├── goal.rs          ← GoalRoomState — replaces Dart goal_room.dart
│   │   └── entry.rs         ← EntryAggregator — replaces Dart entry_aggregator.dart
│   └── Cargo.toml           (depends on symbiotic-core)
└── symbiotic-mobile-core/   (MOVED from submodules/app/rust-core/)
    ├── src/
    │   ├── lib.rs            ← FFI exports for escrow + client
    │   └── escrow.rs         ← existing escrow crypto
    └── Cargo.toml            (depends on symbiotic-client)
```

Flutter app calls into `symbiotic-mobile-core` via `flutter_rust_bridge`. Web app calls into `symbiotic-client` compiled to WASM.

## 4. What Moves to Rust

| Dart file | Rust module | Lines | Priority |
|-----------|-------------|-------|----------|
| `models/status_event.dart` (EventKind/EventStatus) | `symbiotic-core::protocol` (already exists) | 20 | P0 |
| `state/app_state.dart` (command builders) | `client::commands` | 50 | P0 |
| `services/event_parser.dart` | `client::parser` | 80 | P0 |
| `services/event_router.dart` | `client::router` | 120 | P1 |
| `services/input_classifier.dart` | `client::classifier` | 160 | P1 |
| `services/entry_aggregator.dart` | `client::entry` | 80 | P1 |
| `models/thread_model.dart` | `client::thread` | 100 | P2 |
| `models/goal_room.dart` | `client::goal` | 80 | P2 |

## 5. What Stays in Dart

- **MatrixService** — Dart Matrix SDK is Flutter-specific (transport layer)
- **Flutter UI** — widgets, screens, animations, navigation
- **Platform services** — push notifications, share extension, file system
- **AppState** — thin adapter between Rust core and Flutter UI

## 6. What NOT to Do

- **Don't use codegen** — solves a leaf problem (integer sync) while the root problem is where business logic lives
- **Don't split protocol.rs into its own crate** — symbiotic-core compiles fine for all targets (all deps are pure Rust)
- **Don't keep rust-core in the app repo** — it needs symbiotic-core, so make it a workspace member to avoid cross-submodule path fragility
- **Don't replace the existing escrow MethodChannel with FRB** until the full FRB setup is done — it works, don't break it mid-migration

## 7. Implementation Phases

### Phase 1: Foundation (~1 week)
1. Create `symbiotic-client` crate with protocol re-exports + `CommandBuilder` + `EventParser`
2. Move `rust-core` → `symbiotic-mobile-core` in runtime workspace
3. Set up `flutter_rust_bridge` codegen for the app
4. Replace Dart `EventKind`/`EventStatus` with generated types
5. Replace hand-rolled command maps with `CommandBuilder`

### Phase 2: Business Logic Migration (~2-3 weeks)
6. Move EventRouter, InputClassifier, EntryAggregator to Rust
7. Move ThreadState, GoalRoomState to Rust
8. Thin out AppState to adapter-only

### Phase 3: Multi-Platform (~future)
9. Compile symbiotic-client to WASM
10. Build web app using JS + WASM core
11. Desktop via Flutter desktop + same FFI

## 8. Platform Compilation Targets

| Target | Compile | Integration |
|--------|---------|-------------|
| iOS (aarch64-apple-ios) | `.a` static lib | flutter_rust_bridge FFI |
| Android (aarch64-linux-android) | `.so` shared lib | flutter_rust_bridge FFI |
| macOS (aarch64-apple-darwin) | `.dylib` | flutter_rust_bridge FFI |
| Linux (x86_64-unknown-linux-gnu) | `.so` | flutter_rust_bridge FFI |
| Web (wasm32-unknown-unknown) | `.wasm` | wasm-bindgen / wasm-pack |

## 9. symbiotic-core Mobile Compatibility

All deps are pure Rust — verified safe for all targets:

| Dependency | Mobile-safe | Used by |
|-----------|-------------|---------|
| serde + serde_json | Yes | Everything |
| serde_repr | Yes | protocol.rs |
| sha2 | Yes | intake.rs |
| thiserror | Yes (proc macro) | intake.rs, temporal.rs |
| url | Yes (pure Rust) | intake.rs |

The `harden_file_permissions` / `harden_dir_permissions` functions use `#[cfg(unix)]` — compile to no-ops on non-Unix.
