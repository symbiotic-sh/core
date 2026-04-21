# Skill Bundle Architecture (Multiplatform Execution)


**Target**: submodules/runtime/crates/symbiotic-skills
**Status**: Approved

## The Problem
Compiled Rust binary skills (like symbiotic-file-patcher) are incredibly fast and secure, but a binary built on macOS ARM64 will not run on a Linux x86_64 VPS. We need a way for the AI Orchestrator to call a skill without worrying about the underlying operating system.

## The Solution: The "Fat Skill" Bundle
A Symbiotic Skill is not a single executable. It is a directory containing pre-compiled native binaries for all supported target platforms, governed by a `manifest.toml`.

### Directory Structure
```text
knowledge-base/operations/skills/[skill-name]/
├── manifest.toml
├── src/                # (Optional) Original synthesized source code
└── bin/
    ├── aarch64-apple-darwin      # Mac Silicon (default target)
    └── x86_64-unknown-linux-musl # Linux VPS / Docker (default target, statically linked)
```

Additional target triples (Mac Intel `x86_64-apple-darwin`, Windows `x86_64-pc-windows-msvc`) remain aspirational — they are NOT included in `symbiotic-skills::default_targets()` and must be requested per-synthesis if wanted.

### The manifest.toml Contract
When the symbiotic-daemon loads a skill, it parses this manifest (see `symbiotic-skills::manifest::ManifestToml`, which loads from `skill_dir.join("manifest.toml")`).

```toml
[skill]
name = "file-patcher"
version = "1.0.0"
description = "Writes base64 encoded strings to files safely."
protocol = "stdio-json-rpc"

[targets]
aarch64-apple-darwin = "./bin/aarch64-apple-darwin"
x86_64-unknown-linux-musl = "./bin/x86_64-unknown-linux-musl"
```

## Runtime Resolution Logic (Rust)
Inside symbiotic-skills (see `host_target_triple()`):
1. The daemon reads its own host architecture using std::env::consts::OS and std::env::consts::ARCH.
2. It maps this to the Rust target triple (e.g., macos + aarch64 = aarch64-apple-darwin).
3. It looks up the target triple in the skill's `manifest.toml` → `[targets]` map.
4. It spawns that specific binary via std::process::Command.

If the current architecture is missing from the manifest, the daemon returns an explicit error to the Orchestrator: Error: Skill 'file-patcher' lacks a binary for target 'aarch64-unknown-linux-gnu'.

## Cross-Compilation Strategy (Mac Host)
**Yes, you can compile for Linux directly from your Mac.**

When the Symbiotic "Coder Agent" synthesizes a new skill on your Mac, it doesn't need a Linux machine to build the Linux binary. The runtime uses the `DockerSandboxCompiler` (in `symbiotic-skills::docker`), which runs `cargo build --release --target <triple>` inside an isolated `rust:1.88-slim` Docker container with memory/CPU limits and `--network none`. This gives Symbiotic cross-compilation and sandboxing in a single primitive, with no dependency on the external `cross` crate.

**The Compilation Pipeline (Executed by the Agent):**
1. Write `src/main.rs`.
2. For each target in `default_targets()` (today: `aarch64-apple-darwin`, `x86_64-unknown-linux-musl`):
   - Stage source into a temp directory.
   - Invoke `DockerSandboxCompiler::compile(source, target, skill_name)`.
   - Collect the compiled binary.
3. Move all outputs to `bin/` and write `manifest.toml` via `SkillArchiver`.

This allows your local Mac to author skills that instantly work when deployed to your Linux VPS Goal Sandboxes.
