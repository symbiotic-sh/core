# Symbiotic - GitHub Copilot Instructions

## STOP - MANDATORY SESSION START

**Before doing ANY work, you MUST complete these steps in order:**

1. **Read `CONTEXT.md`** - Contains all development rules and conventions
2. **Read `tasks/TASKS.md`** - See current priorities and pick highest priority pending task
3. **Only then** proceed with work

Do NOT skip these steps. Do NOT assume you know the rules. Read the files.

---

## Quick Reference

- **Language**: Rust
- **Project**: Personal AI Operating System (Archive, Nucleus, Gatekeeper, Agent Swarms)
- **Task System**: See `tasks/TASKS.md` for current work
- **Architecture**: See `docs/architecture/` for design docs

## Code Style

- Use `thiserror` for error types in library crates
- Use `anyhow` for application-level errors (CLI)
- Never use `unwrap()` in library code - only in tests
- Async by default for I/O operations using `tokio`
- All public functions documented with `///` doc comments
