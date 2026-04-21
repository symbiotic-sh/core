//! Docker-backed sandbox compiler for skill synthesis.
//!
//! Uses Docker containers for safe compilation and testing of generated skill
//! source code. Each compile/test operation runs in an isolated container with
//! no network access (for security) and resource limits.
//!
//! For cross-compilation, the container uses the Rust `cross` image which
//! includes the necessary toolchains for multiple targets.

use std::path::PathBuf;

use tokio::process::Command;

use crate::synthesis::{
    CompileResult, GeneratedSource, SandboxCompiler, SynthesisError, TestResult,
};

/// Configuration for the Docker sandbox.
#[derive(Debug, Clone)]
pub struct DockerSandboxConfig {
    /// Docker image to use for compilation (default: "rust:1.88-slim").
    pub image: String,
    /// Memory limit for the container (e.g. "512m").
    pub memory_limit: String,
    /// CPU quota (e.g. "1.0" = 1 full CPU core).
    pub cpu_limit: String,
    /// Timeout in seconds for each compile/test operation.
    pub timeout_secs: u64,
    /// Whether to disable network access in the container.
    pub no_network: bool,
    /// Temporary directory for staging source files.
    pub staging_dir: PathBuf,
}

impl Default for DockerSandboxConfig {
    fn default() -> Self {
        Self {
            image: "rust:1.88-slim".to_string(),
            memory_limit: "512m".to_string(),
            cpu_limit: "1.0".to_string(),
            timeout_secs: 120,
            no_network: true,
            staging_dir: std::env::temp_dir().join("symbiotic-skill-sandbox"),
        }
    }
}

/// Docker-backed implementation of `SandboxCompiler`.
///
/// Compiles and tests generated skill code inside isolated Docker containers.
/// Each operation creates a temporary directory with the source, mounts it
/// into a container, runs the build/test, and extracts the result.
pub struct DockerSandboxCompiler {
    config: DockerSandboxConfig,
}

impl DockerSandboxCompiler {
    /// Create a new Docker sandbox compiler with the given configuration.
    pub fn new(config: DockerSandboxConfig) -> Self {
        Self { config }
    }

    /// Check if Docker is available on the system.
    pub async fn check_available() -> Result<(), SynthesisError> {
        let output = Command::new("docker")
            .arg("info")
            .output()
            .await
            .map_err(|e| SynthesisError::SandboxUnavailable(format!("docker not found: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SynthesisError::SandboxUnavailable(format!(
                "docker not running: {stderr}"
            )));
        }
        Ok(())
    }

    /// Stage source files to a temporary directory and return the path.
    fn stage_source(
        &self,
        source: &GeneratedSource,
        skill_name: &str,
    ) -> Result<PathBuf, SynthesisError> {
        let work_dir = self.config.staging_dir.join(format!(
            "{}-{}",
            skill_name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ));
        let src_dir = work_dir.join("src");
        std::fs::create_dir_all(&src_dir)?;
        std::fs::write(work_dir.join("Cargo.toml"), &source.cargo_toml)?;
        std::fs::write(src_dir.join("main.rs"), &source.main_rs)?;
        Ok(work_dir)
    }

    /// Build Docker run arguments common to compile and test operations.
    fn docker_run_args(&self, work_dir: &std::path::Path) -> Vec<String> {
        let mut args = vec![
            "run".to_string(),
            "--rm".to_string(),
            "-v".to_string(),
            format!("{}:/workspace", work_dir.display()),
            "-w".to_string(),
            "/workspace".to_string(),
            "--memory".to_string(),
            self.config.memory_limit.clone(),
            "--cpus".to_string(),
            self.config.cpu_limit.clone(),
        ];

        if self.config.no_network {
            args.push("--network".to_string());
            args.push("none".to_string());
        }

        args.push(self.config.image.clone());
        args
    }

    /// Clean up the staging directory for a skill.
    fn cleanup(&self, work_dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(work_dir);
    }
}

#[async_trait::async_trait]
impl SandboxCompiler for DockerSandboxCompiler {
    async fn compile(
        &self,
        source: &GeneratedSource,
        target: &str,
        skill_name: &str,
    ) -> Result<CompileResult, SynthesisError> {
        let work_dir = self.stage_source(source, skill_name)?;

        let mut args = self.docker_run_args(&work_dir);
        args.extend([
            "cargo".to_string(),
            "build".to_string(),
            "--release".to_string(),
            "--target".to_string(),
            target.to_string(),
        ]);

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.config.timeout_secs),
            Command::new("docker").args(&args).output(),
        )
        .await
        .map_err(|_| SynthesisError::CompileFailed {
            exit_code: -1,
            stderr: format!("compilation timed out after {}s", self.config.timeout_secs),
        })?
        .map_err(|e| SynthesisError::SandboxUnavailable(format!("failed to run docker: {e}")))?;

        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let success = output.status.success();

        let binary_path = if success {
            Some(format!("target/{target}/release/{skill_name}"))
        } else {
            None
        };

        self.cleanup(&work_dir);

        Ok(CompileResult {
            target: target.to_string(),
            success,
            exit_code,
            stdout,
            stderr,
            binary_path,
        })
    }

    async fn test(
        &self,
        source: &GeneratedSource,
        skill_name: &str,
    ) -> Result<TestResult, SynthesisError> {
        let work_dir = self.stage_source(source, skill_name)?;

        let mut args = self.docker_run_args(&work_dir);
        args.extend(["cargo".to_string(), "test".to_string()]);

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.config.timeout_secs),
            Command::new("docker").args(&args).output(),
        )
        .await
        .map_err(|_| SynthesisError::CompileFailed {
            exit_code: -1,
            stderr: format!("tests timed out after {}s", self.config.timeout_secs),
        })?
        .map_err(|e| SynthesisError::SandboxUnavailable(format!("failed to run docker: {e}")))?;

        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let passed = output.status.success();

        self.cleanup(&work_dir);

        Ok(TestResult {
            passed,
            exit_code,
            stdout,
            stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sensible_values() {
        let config = DockerSandboxConfig::default();
        assert_eq!(config.image, "rust:1.88-slim");
        assert_eq!(config.memory_limit, "512m");
        assert_eq!(config.cpu_limit, "1.0");
        assert_eq!(config.timeout_secs, 120);
        assert!(config.no_network);
    }

    #[test]
    fn stage_source_creates_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = DockerSandboxConfig {
            staging_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let compiler = DockerSandboxCompiler::new(config);

        let source = GeneratedSource {
            cargo_toml: "[package]\nname = \"test\"\nversion = \"0.1.0\"\nedition = \"2021\""
                .to_string(),
            main_rs: "fn main() { println!(\"hello\"); }".to_string(),
        };

        let work_dir = compiler.stage_source(&source, "test-skill").unwrap();
        assert!(work_dir.join("Cargo.toml").exists());
        assert!(work_dir.join("src/main.rs").exists());

        let cargo = std::fs::read_to_string(work_dir.join("Cargo.toml")).unwrap();
        assert!(cargo.contains("test"));

        let main = std::fs::read_to_string(work_dir.join("src/main.rs")).unwrap();
        assert!(main.contains("hello"));
    }

    #[test]
    fn docker_run_args_include_resource_limits() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = DockerSandboxConfig {
            memory_limit: "256m".to_string(),
            cpu_limit: "0.5".to_string(),
            no_network: true,
            image: "rust:1.88".to_string(),
            staging_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let compiler = DockerSandboxCompiler::new(config);

        let args = compiler.docker_run_args(tmp.path());
        assert!(args.contains(&"--rm".to_string()));
        assert!(args.contains(&"256m".to_string()));
        assert!(args.contains(&"0.5".to_string()));
        assert!(args.contains(&"none".to_string())); // network none
        assert!(args.contains(&"rust:1.88".to_string()));
    }

    #[test]
    fn docker_run_args_allows_network() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = DockerSandboxConfig {
            no_network: false,
            staging_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let compiler = DockerSandboxCompiler::new(config);

        let args = compiler.docker_run_args(tmp.path());
        assert!(!args.contains(&"--network".to_string()));
    }

    #[test]
    fn cleanup_removes_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work_dir = tmp.path().join("test-cleanup");
        std::fs::create_dir_all(&work_dir).unwrap();
        std::fs::write(work_dir.join("file.txt"), "data").unwrap();
        assert!(work_dir.exists());

        let config = DockerSandboxConfig {
            staging_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let compiler = DockerSandboxCompiler::new(config);
        compiler.cleanup(&work_dir);
        assert!(!work_dir.exists());
    }

    #[test]
    fn cleanup_noop_on_nonexistent() {
        let config = DockerSandboxConfig::default();
        let compiler = DockerSandboxCompiler::new(config);
        // Should not panic
        compiler.cleanup(std::path::Path::new("/nonexistent/path/xyz"));
    }

    #[tokio::test]
    async fn check_available_returns_error_when_docker_missing() {
        // This test is environment-dependent. If Docker IS available, it succeeds.
        // If not, it returns an error. We just verify it doesn't panic.
        let result = DockerSandboxCompiler::check_available().await;
        // We can't assert success or failure portably, but we can assert
        // that any error is the right type.
        if let Err(e) = result {
            assert!(
                matches!(e, SynthesisError::SandboxUnavailable(_)),
                "unexpected error type: {e}"
            );
        }
    }
}
