//! Dynamic Skill Synthesis: generate, compile, test, and archive new skills at runtime.
//!
//! When an agent encounters a novel problem that no existing skill can solve,
//! the synthesis pipeline generates Rust source code, compiles it in a sandboxed
//! environment (Docker), runs tests, and archives the resulting binary alongside
//! a TOML manifest into `operations/skills/`.
//!
//! # Workflow
//!
//! ```text
//! ProblemDescription
//!   → CodeGeneration (LLM produces src/main.rs + Cargo.toml)
//!   → SandboxCompile (Docker: cargo build --release for each target)
//!   → Test           (Docker: cargo test)
//!   → Archive        (write manifest.toml + bin/ to skills dir)
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::codegen::CodeGenerator;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur during skill synthesis.
#[derive(Debug, Error)]
pub enum SynthesisError {
    #[error("code generation failed: {0}")]
    CodeGenFailed(String),
    #[error("sandbox compilation failed (exit {exit_code}): {stderr}")]
    CompileFailed { exit_code: i32, stderr: String },
    #[error("tests failed (exit {exit_code}): {stderr}")]
    TestFailed { exit_code: i32, stderr: String },
    #[error("archival failed: {0}")]
    ArchivalFailed(String),
    #[error("sandbox unavailable: {0}")]
    SandboxUnavailable(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Workflow types
// ---------------------------------------------------------------------------

/// Describes the problem that triggered skill synthesis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynthesisRequest {
    /// Human-readable skill name (kebab-case).
    pub skill_name: String,
    /// Description of what the skill should do.
    pub description: String,
    /// Protocol the skill uses (default: "stdio-json-rpc").
    pub protocol: String,
    /// Target platforms to compile for.
    pub targets: Vec<String>,
    /// The agent ID requesting synthesis.
    pub requesting_agent: String,
    /// Optional session ID for provenance tracking.
    #[serde(default)]
    pub session_id: Option<String>,
}

impl SynthesisRequest {
    /// Create a new synthesis request with default protocol and targets.
    pub fn new(skill_name: String, description: String, requesting_agent: String) -> Self {
        Self {
            skill_name,
            description,
            protocol: "stdio-json-rpc".to_string(),
            targets: default_targets(),
            requesting_agent,
            session_id: None,
        }
    }
}

/// Default compilation targets for skill bundles.
pub fn default_targets() -> Vec<String> {
    vec![
        "aarch64-apple-darwin".to_string(),
        "x86_64-unknown-linux-musl".to_string(),
    ]
}

/// Stages of the synthesis pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynthesisStage {
    /// Waiting to start.
    Pending,
    /// LLM is generating source code.
    CodeGeneration,
    /// Compiling in sandbox.
    Compiling,
    /// Running tests in sandbox.
    Testing,
    /// Writing manifest and binaries to skills directory.
    Archiving,
    /// Synthesis completed successfully.
    Completed,
    /// Synthesis failed at some stage.
    Failed,
}

/// Generated source code from the code generation stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedSource {
    /// Contents of `Cargo.toml`.
    pub cargo_toml: String,
    /// Contents of `src/main.rs`.
    pub main_rs: String,
}

/// Result of a sandbox compilation for a single target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompileResult {
    /// Target triple that was compiled.
    pub target: String,
    /// Whether compilation succeeded.
    pub success: bool,
    /// Exit code from the compiler.
    pub exit_code: i32,
    /// Stdout from compilation.
    pub stdout: String,
    /// Stderr from compilation.
    pub stderr: String,
    /// Path to the compiled binary (relative to sandbox workspace), if successful.
    pub binary_path: Option<String>,
}

/// Result of running tests in the sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    /// Whether all tests passed.
    pub passed: bool,
    /// Exit code from the test runner.
    pub exit_code: i32,
    /// Stdout from tests.
    pub stdout: String,
    /// Stderr from tests.
    pub stderr: String,
}

/// The final result of a completed synthesis pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynthesisResult {
    /// Name of the synthesized skill.
    pub skill_name: String,
    /// Path to the archived skill directory.
    pub skill_dir: PathBuf,
    /// Targets that were successfully compiled.
    pub compiled_targets: Vec<String>,
    /// Stage history for auditing.
    pub stages_completed: Vec<SynthesisStage>,
    /// Number of codegen retries before tests passed (0 = first attempt succeeded).
    pub retry_count: u32,
}

// ---------------------------------------------------------------------------
// Sandbox compiler trait
// ---------------------------------------------------------------------------

/// Trait for sandbox compilation environments.
///
/// Implementations handle the actual Docker/VM invocation. The default
/// `StubSandboxCompiler` returns mock results for development/testing.
#[async_trait::async_trait]
pub trait SandboxCompiler: Send + Sync {
    /// Compile generated source code for a specific target.
    async fn compile(
        &self,
        source: &GeneratedSource,
        target: &str,
        skill_name: &str,
    ) -> Result<CompileResult, SynthesisError>;

    /// Run tests on generated source code.
    async fn test(
        &self,
        source: &GeneratedSource,
        skill_name: &str,
    ) -> Result<TestResult, SynthesisError>;
}

/// Stub sandbox compiler for development. Returns successful mock results.
///
/// In production this will be replaced by a Docker-backed implementation
/// that uses `cross` for cross-compilation (see `docs/design/skill-bundle-manifest.md`).
pub struct StubSandboxCompiler;

#[async_trait::async_trait]
impl SandboxCompiler for StubSandboxCompiler {
    async fn compile(
        &self,
        _source: &GeneratedSource,
        target: &str,
        skill_name: &str,
    ) -> Result<CompileResult, SynthesisError> {
        Ok(CompileResult {
            target: target.to_string(),
            success: true,
            exit_code: 0,
            stdout: format!("Compiling {skill_name} for {target}... done (stub)"),
            stderr: String::new(),
            binary_path: Some(format!("target/{target}/release/{skill_name}")),
        })
    }

    async fn test(
        &self,
        _source: &GeneratedSource,
        skill_name: &str,
    ) -> Result<TestResult, SynthesisError> {
        Ok(TestResult {
            passed: true,
            exit_code: 0,
            stdout: format!("running 1 test for {skill_name}\ntest basic ... ok\n\ntest result: ok. 1 passed; 0 failed"),
            stderr: String::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Skill archiver
// ---------------------------------------------------------------------------

/// Archives a synthesized skill to the skills directory.
///
/// Creates the directory structure:
/// ```text
/// {skills_root}/{skill_name}/
/// ├── manifest.toml
/// ├── src/
/// │   └── main.rs
/// └── bin/
///     ├── aarch64-apple-darwin
///     └── x86_64-unknown-linux-musl
/// ```
pub struct SkillArchiver {
    /// Root directory for all skills (e.g., `knowledge-base/operations/skills/` in the Archive).
    skills_root: PathBuf,
}

impl SkillArchiver {
    /// Create a new archiver targeting the given skills root directory.
    pub fn new(skills_root: &Path) -> Self {
        Self {
            skills_root: skills_root.to_path_buf(),
        }
    }

    /// Returns the skills root path.
    pub fn skills_root(&self) -> &Path {
        &self.skills_root
    }

    /// Archive a synthesized skill: write manifest, source, and binary stubs.
    ///
    /// In production, the binary contents come from the sandbox compiler output.
    /// For the skeleton, we write placeholder files.
    pub fn archive(
        &self,
        request: &SynthesisRequest,
        source: &GeneratedSource,
        compile_results: &[CompileResult],
        retry_count: u32,
    ) -> Result<PathBuf, SynthesisError> {
        let skill_dir = self.skills_root.join(&request.skill_name);

        // Create directory structure
        let src_dir = skill_dir.join("src");
        let bin_dir = skill_dir.join("bin");
        std::fs::create_dir_all(&src_dir).map_err(|e| {
            SynthesisError::ArchivalFailed(format!("failed to create src dir: {e}"))
        })?;
        std::fs::create_dir_all(&bin_dir).map_err(|e| {
            SynthesisError::ArchivalFailed(format!("failed to create bin dir: {e}"))
        })?;

        // Write source files
        std::fs::write(src_dir.join("main.rs"), &source.main_rs)
            .map_err(|e| SynthesisError::ArchivalFailed(format!("failed to write main.rs: {e}")))?;
        std::fs::write(skill_dir.join("Cargo.toml"), &source.cargo_toml).map_err(|e| {
            SynthesisError::ArchivalFailed(format!("failed to write Cargo.toml: {e}"))
        })?;

        // Write binary placeholders for each successful target
        for result in compile_results {
            if result.success {
                let bin_path = bin_dir.join(&result.target);
                std::fs::write(
                    &bin_path,
                    format!("# placeholder binary for {}", result.target),
                )
                .map_err(|e| {
                    SynthesisError::ArchivalFailed(format!(
                        "failed to write binary for {}: {e}",
                        result.target
                    ))
                })?;
            }
        }

        // Build targets map for manifest
        let targets: HashMap<String, String> = compile_results
            .iter()
            .filter(|r| r.success)
            .map(|r| (r.target.clone(), format!("./bin/{}", r.target)))
            .collect();

        // Generate and write manifest.toml
        let manifest_toml = generate_manifest_toml(request, &targets, retry_count);
        std::fs::write(skill_dir.join("manifest.toml"), &manifest_toml).map_err(|e| {
            SynthesisError::ArchivalFailed(format!("failed to write manifest.toml: {e}"))
        })?;

        Ok(skill_dir)
    }
}

/// Generate a TOML manifest string for a synthesized skill.
fn generate_manifest_toml(
    request: &SynthesisRequest,
    targets: &HashMap<String, String>,
    retry_count: u32,
) -> String {
    let mut toml = format!(
        r#"[skill]
name = "{name}"
version = "0.1.0"
description = "{description}"
min_trust_level = "ArchiveWrite"

[capabilities]
required = ["vm.exec"]

[triggers]
keywords = ["{name}"]
invocation = "{name}"

[metadata]
author = "symbiotic-synthesizer"
tags = ["synthesized", "auto-generated"]
"#,
        name = request.skill_name,
        description = request.description.replace('"', "\\\""),
    );

    // Synthesis provenance section
    toml.push_str("\n[synthesis]\n");
    toml.push_str(&format!(
        "requesting_agent = \"{}\"\n",
        request.requesting_agent
    ));
    if let Some(ref session_id) = request.session_id {
        toml.push_str(&format!("synthesis_session = \"{session_id}\"\n"));
    }
    toml.push_str(&format!(
        "problem_description = \"{}\"\n",
        request.description.replace('"', "\\\"")
    ));
    toml.push_str(&format!("retry_count = {retry_count}\n"));

    // Append target binaries
    if !targets.is_empty() {
        toml.push_str("\n[targets]\n");
        for (target, path) in targets {
            toml.push_str(&format!("{target} = \"{path}\"\n"));
        }
    }

    toml
}

// ---------------------------------------------------------------------------
// Skill synthesizer (orchestrator)
// ---------------------------------------------------------------------------

/// Orchestrates the full skill synthesis pipeline.
pub struct SkillSynthesizer {
    compiler: Box<dyn SandboxCompiler>,
    archiver: SkillArchiver,
    codegen: Option<Box<dyn CodeGenerator>>,
    /// Maximum number of codegen retries on test failure (default: 2).
    max_retries: usize,
}

impl SkillSynthesizer {
    /// Create a new synthesizer with the given sandbox compiler and archiver.
    ///
    /// Uses stub code generation. For LLM-backed code generation, call
    /// `with_codegen()` after construction.
    pub fn new(compiler: Box<dyn SandboxCompiler>, archiver: SkillArchiver) -> Self {
        Self {
            compiler,
            archiver,
            codegen: None,
            max_retries: 2,
        }
    }

    /// Set the code generator (replaces stub code generation with LLM-backed).
    pub fn with_codegen(mut self, codegen: Box<dyn CodeGenerator>) -> Self {
        self.codegen = Some(codegen);
        self
    }

    /// Set the maximum number of codegen retries on test failure.
    pub fn with_max_retries(mut self, n: usize) -> Self {
        self.max_retries = n;
        self
    }

    /// Run the full synthesis pipeline.
    ///
    /// If a `CodeGenerator` is configured, uses it for code generation.
    /// Otherwise falls back to stub template code.
    ///
    /// On test failure, retries up to `max_retries` times by calling
    /// `generate_with_feedback()` with the test stderr (only when a
    /// `CodeGenerator` is available).
    pub async fn synthesize(
        &self,
        request: &SynthesisRequest,
    ) -> Result<SynthesisResult, SynthesisError> {
        let mut stages = vec![SynthesisStage::Pending];
        let mut retry_count: u32 = 0;

        // Stage 1: Code generation
        stages.push(SynthesisStage::CodeGeneration);
        let mut source = if let Some(ref codegen) = self.codegen {
            codegen.generate(request).await?
        } else {
            generate_stub_source(&request.skill_name, &request.description)
        };

        // Stage 2: Test (with retry loop)
        stages.push(SynthesisStage::Testing);
        let mut last_test_error;

        loop {
            let test_result = self.compiler.test(&source, &request.skill_name).await?;
            if test_result.passed {
                break;
            }

            last_test_error = test_result.stderr.clone();

            // Can we retry?
            if (retry_count as usize) < self.max_retries {
                if let Some(ref codegen) = self.codegen {
                    retry_count += 1;
                    stages.push(SynthesisStage::CodeGeneration);
                    source = codegen
                        .generate_with_feedback(request, &last_test_error)
                        .await?;
                    stages.push(SynthesisStage::Testing);
                    continue;
                }
            }

            // No retries left or no codegen available
            stages.push(SynthesisStage::Failed);
            return Err(SynthesisError::TestFailed {
                exit_code: test_result.exit_code,
                stderr: last_test_error,
            });
        }

        // Stage 3: Compile for all targets
        stages.push(SynthesisStage::Compiling);
        let mut compile_results = Vec::new();
        for target in &request.targets {
            let result = self
                .compiler
                .compile(&source, target, &request.skill_name)
                .await?;
            if !result.success {
                stages.push(SynthesisStage::Failed);
                return Err(SynthesisError::CompileFailed {
                    exit_code: result.exit_code,
                    stderr: result.stderr,
                });
            }
            compile_results.push(result);
        }

        // Stage 4: Archive
        stages.push(SynthesisStage::Archiving);
        let skill_dir = self
            .archiver
            .archive(request, &source, &compile_results, retry_count)?;

        let compiled_targets: Vec<String> = compile_results
            .iter()
            .filter(|r| r.success)
            .map(|r| r.target.clone())
            .collect();

        stages.push(SynthesisStage::Completed);

        Ok(SynthesisResult {
            skill_name: request.skill_name.clone(),
            skill_dir,
            compiled_targets,
            stages_completed: stages,
            retry_count,
        })
    }
}

/// Generate stub source code for a skill.
///
/// Produces a template stdio-json-rpc binary with basic request/response
/// handling. Used as the fallback when no `CodeGenerator` is configured,
/// and also used by `StubCodeGenerator` in the `codegen` module.
pub fn generate_stub_source(skill_name: &str, description: &str) -> GeneratedSource {
    let cargo_toml = format!(
        r#"[package]
name = "{skill_name}"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = {{ version = "1.0", features = ["derive"] }}
serde_json = "1.0"
"#
    );

    let main_rs = format!(
        r##"//! Auto-synthesized skill: {skill_name}
//! {description}

use std::io::{{self, BufRead, Write}};
use serde::{{Deserialize, Serialize}};

#[derive(Deserialize)]
struct Request {{
    method: String,
    params: serde_json::Value,
}}

#[derive(Serialize)]
struct Response {{
    result: serde_json::Value,
    error: Option<String>,
}}

fn main() {{
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    for line in stdin.lock().lines() {{
        let line = match line {{
            Ok(l) => l,
            Err(_) => break,
        }};
        if line.trim().is_empty() {{
            continue;
        }}

        let request: Request = match serde_json::from_str(&line) {{
            Ok(r) => r,
            Err(e) => {{
                let resp = Response {{
                    result: serde_json::Value::Null,
                    error: Some(format!("parse error: {{e}}")),
                }};
                let _ = writeln!(stdout, "{{}}", serde_json::to_string(&resp).unwrap());
                continue;
            }}
        }};

        let response = match request.method.as_str() {{
            "execute" => Response {{
                result: serde_json::json!({{"status": "ok", "skill": "{skill_name}"}}),
                error: None,
            }},
            _ => Response {{
                result: serde_json::Value::Null,
                error: Some(format!("unknown method: {{}}", request.method)),
            }},
        }};

        let _ = writeln!(stdout, "{{}}", serde_json::to_string(&response).unwrap());
    }}
}}

#[cfg(test)]
mod tests {{
    use super::*;

    #[test]
    fn test_stub_response() {{
        let input = r#"{{"method": "execute", "params": {{}}}}"#;
        let req: Request = serde_json::from_str(input).unwrap();
        assert_eq!(req.method, "execute");
    }}
}}
"##
    );

    GeneratedSource {
        cargo_toml,
        main_rs,
    }
}

// ---------------------------------------------------------------------------
// Resolve host target triple
// ---------------------------------------------------------------------------

/// Returns the Rust target triple for the current host platform.
///
/// Used by the runtime loader to select the correct binary from a skill bundle.
pub fn host_target_triple() -> String {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;

    match (arch, os) {
        ("aarch64", "macos") => "aarch64-apple-darwin".to_string(),
        ("x86_64", "macos") => "x86_64-apple-darwin".to_string(),
        ("x86_64", "linux") => "x86_64-unknown-linux-musl".to_string(),
        ("aarch64", "linux") => "aarch64-unknown-linux-musl".to_string(),
        _ => format!("{arch}-unknown-{os}"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // -- SynthesisRequest --

    #[test]
    fn synthesis_request_new_has_defaults() {
        let req = SynthesisRequest::new(
            "json-patcher".to_string(),
            "Patches JSON files".to_string(),
            "agent-42".to_string(),
        );
        assert_eq!(req.skill_name, "json-patcher");
        assert_eq!(req.protocol, "stdio-json-rpc");
        assert!(!req.targets.is_empty());
        assert!(req.targets.contains(&"aarch64-apple-darwin".to_string()));
        assert!(req
            .targets
            .contains(&"x86_64-unknown-linux-musl".to_string()));
    }

    // -- SynthesisStage --

    #[test]
    fn synthesis_stages_are_serializable() {
        let stage = SynthesisStage::CodeGeneration;
        let json = serde_json::to_string(&stage).unwrap();
        assert_eq!(json, "\"code_generation\"");
        let parsed: SynthesisStage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SynthesisStage::CodeGeneration);
    }

    #[test]
    fn all_stages_round_trip() {
        let stages = [
            SynthesisStage::Pending,
            SynthesisStage::CodeGeneration,
            SynthesisStage::Compiling,
            SynthesisStage::Testing,
            SynthesisStage::Archiving,
            SynthesisStage::Completed,
            SynthesisStage::Failed,
        ];
        for stage in &stages {
            let json = serde_json::to_string(stage).unwrap();
            let parsed: SynthesisStage = serde_json::from_str(&json).unwrap();
            assert_eq!(*stage, parsed);
        }
    }

    // -- GeneratedSource --

    #[test]
    fn generate_stub_source_produces_valid_content() {
        let source = generate_stub_source("my-tool", "Does things");
        assert!(source.cargo_toml.contains("my-tool"));
        assert!(source.cargo_toml.contains("serde"));
        assert!(source.main_rs.contains("fn main()"));
        assert!(source.main_rs.contains("my-tool"));
        assert!(source.main_rs.contains("std::io"));
    }

    // -- StubSandboxCompiler --

    #[tokio::test]
    async fn stub_compiler_compile_succeeds() {
        let compiler = StubSandboxCompiler;
        let source = generate_stub_source("test-skill", "test");
        let result = compiler
            .compile(&source, "x86_64-unknown-linux-musl", "test-skill")
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.exit_code, 0);
        assert!(result.binary_path.is_some());
        assert!(result
            .binary_path
            .unwrap()
            .contains("x86_64-unknown-linux-musl"));
    }

    #[tokio::test]
    async fn stub_compiler_test_succeeds() {
        let compiler = StubSandboxCompiler;
        let source = generate_stub_source("test-skill", "test");
        let result = compiler.test(&source, "test-skill").await.unwrap();
        assert!(result.passed);
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("ok"));
    }

    // -- SkillArchiver --

    #[test]
    fn archiver_creates_directory_structure() {
        let tmp = TempDir::new().unwrap();
        let archiver = SkillArchiver::new(tmp.path());

        let request = SynthesisRequest::new(
            "file-patcher".to_string(),
            "Patches files".to_string(),
            "agent-1".to_string(),
        );
        let source = generate_stub_source("file-patcher", "Patches files");
        let compile_results = vec![
            CompileResult {
                target: "aarch64-apple-darwin".to_string(),
                success: true,
                exit_code: 0,
                stdout: "ok".to_string(),
                stderr: String::new(),
                binary_path: Some("target/aarch64-apple-darwin/release/file-patcher".to_string()),
            },
            CompileResult {
                target: "x86_64-unknown-linux-musl".to_string(),
                success: true,
                exit_code: 0,
                stdout: "ok".to_string(),
                stderr: String::new(),
                binary_path: Some(
                    "target/x86_64-unknown-linux-musl/release/file-patcher".to_string(),
                ),
            },
        ];

        let skill_dir = archiver
            .archive(&request, &source, &compile_results, 0)
            .unwrap();

        // Verify directory structure
        assert!(skill_dir.join("manifest.toml").exists());
        assert!(skill_dir.join("src/main.rs").exists());
        assert!(skill_dir.join("Cargo.toml").exists());
        assert!(skill_dir.join("bin/aarch64-apple-darwin").exists());
        assert!(skill_dir.join("bin/x86_64-unknown-linux-musl").exists());
    }

    #[test]
    fn archiver_manifest_contains_skill_info() {
        let tmp = TempDir::new().unwrap();
        let archiver = SkillArchiver::new(tmp.path());

        let request = SynthesisRequest::new(
            "url-fetcher".to_string(),
            "Fetches URLs safely".to_string(),
            "agent-1".to_string(),
        );
        let source = generate_stub_source("url-fetcher", "Fetches URLs safely");
        let compile_results = vec![CompileResult {
            target: "aarch64-apple-darwin".to_string(),
            success: true,
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            binary_path: Some("target/aarch64-apple-darwin/release/url-fetcher".to_string()),
        }];

        let skill_dir = archiver
            .archive(&request, &source, &compile_results, 0)
            .unwrap();

        let manifest_content = std::fs::read_to_string(skill_dir.join("manifest.toml")).unwrap();
        assert!(manifest_content.contains("name = \"url-fetcher\""));
        assert!(manifest_content.contains("version = \"0.1.0\""));
        assert!(manifest_content.contains("Fetches URLs safely"));
        assert!(manifest_content.contains("synthesized"));
        assert!(manifest_content.contains("aarch64-apple-darwin"));
    }

    #[test]
    fn archiver_skips_failed_targets() {
        let tmp = TempDir::new().unwrap();
        let archiver = SkillArchiver::new(tmp.path());

        let request = SynthesisRequest::new(
            "partial-skill".to_string(),
            "test".to_string(),
            "agent-1".to_string(),
        );
        let source = generate_stub_source("partial-skill", "test");
        let compile_results = vec![
            CompileResult {
                target: "aarch64-apple-darwin".to_string(),
                success: true,
                exit_code: 0,
                stdout: "ok".to_string(),
                stderr: String::new(),
                binary_path: Some("target/aarch64-apple-darwin/release/partial-skill".to_string()),
            },
            CompileResult {
                target: "x86_64-unknown-linux-musl".to_string(),
                success: false,
                exit_code: 1,
                stdout: String::new(),
                stderr: "linker error".to_string(),
                binary_path: None,
            },
        ];

        let skill_dir = archiver
            .archive(&request, &source, &compile_results, 0)
            .unwrap();

        // Only the successful target should have a binary
        assert!(skill_dir.join("bin/aarch64-apple-darwin").exists());
        assert!(!skill_dir.join("bin/x86_64-unknown-linux-musl").exists());

        // Manifest should only list the successful target
        let manifest = std::fs::read_to_string(skill_dir.join("manifest.toml")).unwrap();
        assert!(manifest.contains("aarch64-apple-darwin"));
        assert!(!manifest.contains("x86_64-unknown-linux-musl"));
    }

    // -- generate_manifest_toml --

    #[test]
    fn generate_manifest_toml_format() {
        let request = SynthesisRequest::new(
            "test-skill".to_string(),
            "A test skill".to_string(),
            "agent-1".to_string(),
        );
        let targets: HashMap<String, String> = [(
            "aarch64-apple-darwin".to_string(),
            "./bin/aarch64-apple-darwin".to_string(),
        )]
        .into_iter()
        .collect();

        let toml_str = generate_manifest_toml(&request, &targets, 0);
        assert!(toml_str.contains("[skill]"));
        assert!(toml_str.contains("[capabilities]"));
        assert!(toml_str.contains("[triggers]"));
        assert!(toml_str.contains("[metadata]"));
        assert!(toml_str.contains("[synthesis]"));
        assert!(toml_str.contains("[targets]"));
        assert!(toml_str.contains("test-skill"));
        assert!(toml_str.contains("requesting_agent"));
    }

    #[test]
    fn generate_manifest_toml_escapes_description_quotes() {
        let request = SynthesisRequest {
            skill_name: "quoter".to_string(),
            description: "Handles \"quoted\" strings".to_string(),
            protocol: "stdio-json-rpc".to_string(),
            targets: vec![],
            requesting_agent: "agent-1".to_string(),
            session_id: None,
        };
        let targets = HashMap::new();

        let toml_str = generate_manifest_toml(&request, &targets, 0);
        assert!(toml_str.contains(r#"Handles \"quoted\" strings"#));
    }

    #[test]
    fn generate_manifest_toml_includes_session_id() {
        let request = SynthesisRequest {
            skill_name: "session-skill".to_string(),
            description: "Has session".to_string(),
            protocol: "stdio-json-rpc".to_string(),
            targets: vec![],
            requesting_agent: "agent-1".to_string(),
            session_id: Some("sess-xyz-789".to_string()),
        };
        let targets = HashMap::new();

        let toml_str = generate_manifest_toml(&request, &targets, 3);
        assert!(toml_str.contains("synthesis_session = \"sess-xyz-789\""));
        assert!(toml_str.contains("retry_count = 3"));
    }

    // -- SkillSynthesizer (full pipeline) --

    #[tokio::test]
    async fn synthesizer_full_pipeline_succeeds() {
        let tmp = TempDir::new().unwrap();
        let compiler = Box::new(StubSandboxCompiler);
        let archiver = SkillArchiver::new(tmp.path());
        let synthesizer = SkillSynthesizer::new(compiler, archiver);

        let request = SynthesisRequest::new(
            "hello-skill".to_string(),
            "Says hello".to_string(),
            "agent-1".to_string(),
        );

        let result = synthesizer.synthesize(&request).await.unwrap();

        assert_eq!(result.skill_name, "hello-skill");
        assert!(result.skill_dir.exists());
        assert_eq!(result.compiled_targets.len(), 2);
        assert_eq!(result.retry_count, 0);
        assert!(result.stages_completed.contains(&SynthesisStage::Completed));
        assert!(!result.stages_completed.contains(&SynthesisStage::Failed));

        // Verify files were actually written
        assert!(result.skill_dir.join("manifest.toml").exists());
        assert!(result.skill_dir.join("src/main.rs").exists());
    }

    #[tokio::test]
    async fn synthesizer_fails_on_test_failure() {
        struct FailingTestCompiler;

        #[async_trait::async_trait]
        impl SandboxCompiler for FailingTestCompiler {
            async fn compile(
                &self,
                _source: &GeneratedSource,
                target: &str,
                _skill_name: &str,
            ) -> Result<CompileResult, SynthesisError> {
                Ok(CompileResult {
                    target: target.to_string(),
                    success: true,
                    exit_code: 0,
                    stdout: "ok".to_string(),
                    stderr: String::new(),
                    binary_path: Some(format!("target/{target}/release/skill")),
                })
            }

            async fn test(
                &self,
                _source: &GeneratedSource,
                _skill_name: &str,
            ) -> Result<TestResult, SynthesisError> {
                Ok(TestResult {
                    passed: false,
                    exit_code: 101,
                    stdout: String::new(),
                    stderr: "assertion failed".to_string(),
                })
            }
        }

        let tmp = TempDir::new().unwrap();
        let compiler = Box::new(FailingTestCompiler);
        let archiver = SkillArchiver::new(tmp.path());
        let synthesizer = SkillSynthesizer::new(compiler, archiver);

        let request = SynthesisRequest::new(
            "bad-skill".to_string(),
            "Broken skill".to_string(),
            "agent-1".to_string(),
        );

        let err = synthesizer.synthesize(&request).await.unwrap_err();
        assert!(matches!(err, SynthesisError::TestFailed { .. }));
        assert!(err.to_string().contains("assertion failed"));
    }

    #[tokio::test]
    async fn synthesizer_fails_on_compile_failure() {
        struct FailingCompiler;

        #[async_trait::async_trait]
        impl SandboxCompiler for FailingCompiler {
            async fn compile(
                &self,
                _source: &GeneratedSource,
                target: &str,
                _skill_name: &str,
            ) -> Result<CompileResult, SynthesisError> {
                Ok(CompileResult {
                    target: target.to_string(),
                    success: false,
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "cannot find crate `nonexistent`".to_string(),
                    binary_path: None,
                })
            }

            async fn test(
                &self,
                _source: &GeneratedSource,
                _skill_name: &str,
            ) -> Result<TestResult, SynthesisError> {
                Ok(TestResult {
                    passed: true,
                    exit_code: 0,
                    stdout: "ok".to_string(),
                    stderr: String::new(),
                })
            }
        }

        let tmp = TempDir::new().unwrap();
        let compiler = Box::new(FailingCompiler);
        let archiver = SkillArchiver::new(tmp.path());
        let synthesizer = SkillSynthesizer::new(compiler, archiver);

        let request = SynthesisRequest::new(
            "broken-skill".to_string(),
            "Won't compile".to_string(),
            "agent-1".to_string(),
        );

        let err = synthesizer.synthesize(&request).await.unwrap_err();
        assert!(matches!(err, SynthesisError::CompileFailed { .. }));
        assert!(err.to_string().contains("cannot find crate"));
    }

    // -- Retry tests --

    #[tokio::test]
    async fn synthesizer_retries_on_test_failure_then_succeeds() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Compiler that fails tests on the first call, passes on the second.
        struct RetryCompiler {
            call_count: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl SandboxCompiler for RetryCompiler {
            async fn compile(
                &self,
                _source: &GeneratedSource,
                target: &str,
                _skill_name: &str,
            ) -> Result<CompileResult, SynthesisError> {
                Ok(CompileResult {
                    target: target.to_string(),
                    success: true,
                    exit_code: 0,
                    stdout: "ok".to_string(),
                    stderr: String::new(),
                    binary_path: Some(format!("target/{target}/release/skill")),
                })
            }

            async fn test(
                &self,
                _source: &GeneratedSource,
                _skill_name: &str,
            ) -> Result<TestResult, SynthesisError> {
                let count = self.call_count.fetch_add(1, Ordering::SeqCst);
                if count == 0 {
                    Ok(TestResult {
                        passed: false,
                        exit_code: 101,
                        stdout: String::new(),
                        stderr: "assertion failed: expected 42, got 0".to_string(),
                    })
                } else {
                    Ok(TestResult {
                        passed: true,
                        exit_code: 0,
                        stdout: "ok".to_string(),
                        stderr: String::new(),
                    })
                }
            }
        }

        let tmp = TempDir::new().unwrap();
        let compiler = Box::new(RetryCompiler {
            call_count: AtomicUsize::new(0),
        });
        let archiver = SkillArchiver::new(tmp.path());

        // Use StubCodeGenerator so generate_with_feedback is available
        let codegen: Box<dyn CodeGenerator> = Box::new(crate::codegen::StubCodeGenerator);
        let synthesizer = SkillSynthesizer::new(compiler, archiver)
            .with_codegen(codegen)
            .with_max_retries(2);

        let request = SynthesisRequest::new(
            "retry-skill".to_string(),
            "Skill that needs a retry".to_string(),
            "agent-1".to_string(),
        );

        let result = synthesizer.synthesize(&request).await.unwrap();
        assert_eq!(result.skill_name, "retry-skill");
        assert_eq!(result.retry_count, 1);
        assert!(result.skill_dir.exists());
    }

    #[tokio::test]
    async fn synthesizer_exhausts_retries_then_fails() {
        struct AlwaysFailTestCompiler;

        #[async_trait::async_trait]
        impl SandboxCompiler for AlwaysFailTestCompiler {
            async fn compile(
                &self,
                _source: &GeneratedSource,
                target: &str,
                _skill_name: &str,
            ) -> Result<CompileResult, SynthesisError> {
                Ok(CompileResult {
                    target: target.to_string(),
                    success: true,
                    exit_code: 0,
                    stdout: "ok".to_string(),
                    stderr: String::new(),
                    binary_path: Some(format!("target/{target}/release/skill")),
                })
            }

            async fn test(
                &self,
                _source: &GeneratedSource,
                _skill_name: &str,
            ) -> Result<TestResult, SynthesisError> {
                Ok(TestResult {
                    passed: false,
                    exit_code: 101,
                    stdout: String::new(),
                    stderr: "persistent failure".to_string(),
                })
            }
        }

        let tmp = TempDir::new().unwrap();
        let compiler = Box::new(AlwaysFailTestCompiler);
        let archiver = SkillArchiver::new(tmp.path());
        let codegen: Box<dyn CodeGenerator> = Box::new(crate::codegen::StubCodeGenerator);
        let synthesizer = SkillSynthesizer::new(compiler, archiver)
            .with_codegen(codegen)
            .with_max_retries(2);

        let request = SynthesisRequest::new(
            "doomed-skill".to_string(),
            "Always fails".to_string(),
            "agent-1".to_string(),
        );

        let err = synthesizer.synthesize(&request).await.unwrap_err();
        assert!(matches!(err, SynthesisError::TestFailed { .. }));
        assert!(err.to_string().contains("persistent failure"));
    }

    #[tokio::test]
    async fn synthesizer_no_retry_without_codegen() {
        // Without a codegen, retries are impossible even if max_retries > 0
        struct FailOnceCompiler;

        #[async_trait::async_trait]
        impl SandboxCompiler for FailOnceCompiler {
            async fn compile(
                &self,
                _source: &GeneratedSource,
                target: &str,
                _skill_name: &str,
            ) -> Result<CompileResult, SynthesisError> {
                Ok(CompileResult {
                    target: target.to_string(),
                    success: true,
                    exit_code: 0,
                    stdout: "ok".to_string(),
                    stderr: String::new(),
                    binary_path: Some(format!("target/{target}/release/skill")),
                })
            }

            async fn test(
                &self,
                _source: &GeneratedSource,
                _skill_name: &str,
            ) -> Result<TestResult, SynthesisError> {
                Ok(TestResult {
                    passed: false,
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "test error".to_string(),
                })
            }
        }

        let tmp = TempDir::new().unwrap();
        let compiler = Box::new(FailOnceCompiler);
        let archiver = SkillArchiver::new(tmp.path());
        // No codegen set — retries impossible
        let synthesizer = SkillSynthesizer::new(compiler, archiver).with_max_retries(5);

        let request = SynthesisRequest::new(
            "no-codegen-skill".to_string(),
            "No codegen".to_string(),
            "agent-1".to_string(),
        );

        let err = synthesizer.synthesize(&request).await.unwrap_err();
        assert!(matches!(err, SynthesisError::TestFailed { .. }));
    }

    // -- host_target_triple --

    #[test]
    fn host_target_triple_is_not_empty() {
        let triple = host_target_triple();
        assert!(!triple.is_empty());
        // Should contain at least an arch and OS
        assert!(triple.contains('-'));
    }

    // -- default_targets --

    #[test]
    fn default_targets_include_mac_and_linux() {
        let targets = default_targets();
        assert_eq!(targets.len(), 2);
        assert!(targets.iter().any(|t| t.contains("apple")));
        assert!(targets.iter().any(|t| t.contains("linux")));
    }

    // -- Error display --

    #[test]
    fn synthesis_errors_display_correctly() {
        let err = SynthesisError::CodeGenFailed("LLM timeout".to_string());
        assert_eq!(err.to_string(), "code generation failed: LLM timeout");

        let err = SynthesisError::CompileFailed {
            exit_code: 1,
            stderr: "error[E0433]".to_string(),
        };
        assert!(err.to_string().contains("exit 1"));
        assert!(err.to_string().contains("E0433"));

        let err = SynthesisError::SandboxUnavailable("Docker not running".to_string());
        assert!(err.to_string().contains("Docker not running"));
    }
}
