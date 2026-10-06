# Agent rules

Base rules: [House Rules](https://github.com/jak-pan/house-rules), its `AGENTS.md` (layout and naming in its `STRUCTURE.md`). Read and follow them first. This file holds only this repository's own rules. An override may tighten or loosen a House Rules rule, and it names the rule it changes.

Reviews: Warden, our review service, reviews pull requests on request: comment `/warden review` on the pull request.

## Repository rules

- Use `thiserror` for error types in library crates.
- Use `anyhow` for application-level errors (CLI).
- Never use `unwrap()` in library code, only in tests.
- I/O is async by default, using `tokio`.
- Document every public function with `///` doc comments.
