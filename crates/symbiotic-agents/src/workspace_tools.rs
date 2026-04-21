//! Workspace execution tools: shell_exec, file_read, file_write, file_edit.
//!
//! These tools give agents the ability to interact with the real filesystem
//! and run commands in a workspace directory. All operations are sandboxed
//! to the workspace root and gated by capability checks via the Gatekeeper.
//!
//! Modeled after OpenClaw's group:fs + group:runtime tools.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::builtin_tools::CapabilityChecker;
use crate::tools::{Tool, ToolResult};

/// Shared configuration for workspace-scoped tools.
#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    /// Root directory that all file operations are confined to.
    pub root: PathBuf,
    /// Maximum file size agents can read (bytes). Default: 1MB.
    pub max_read_bytes: usize,
    /// Maximum file size agents can write (bytes). Default: 1MB.
    pub max_write_bytes: usize,
    /// Maximum command execution time. Default: 60s.
    pub exec_timeout: Duration,
    /// Maximum combined stdout+stderr capture (bytes). Default: 256KB.
    pub max_output_bytes: usize,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("/tmp/symbiotic-workspace"),
            max_read_bytes: 1_048_576,
            max_write_bytes: 1_048_576,
            exec_timeout: Duration::from_secs(60),
            max_output_bytes: 262_144,
        }
    }
}

/// Resolve and validate a path is within the workspace root.
/// Prevents path traversal attacks (../../etc/passwd).
fn resolve_workspace_path(root: &Path, relative: &str) -> Result<PathBuf> {
    // Reject absolute paths
    if relative.starts_with('/') || relative.starts_with('\\') {
        return Err(anyhow!(
            "absolute paths not allowed — use paths relative to workspace root"
        ));
    }
    let candidate = root.join(relative);
    let resolved = candidate
        .canonicalize()
        .or_else(|_| {
            // File might not exist yet (for writes). Canonicalize parent instead.
            if let Some(parent) = candidate.parent() {
                std::fs::create_dir_all(parent).ok();
                parent
                    .canonicalize()
                    .map(|p| p.join(candidate.file_name().unwrap_or_default()))
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "cannot resolve path",
                ))
            }
        })
        .map_err(|e| anyhow!("cannot resolve path '{}': {}", relative, e))?;

    let canon_root = root
        .canonicalize()
        .map_err(|e| anyhow!("workspace root '{}' not accessible: {}", root.display(), e))?;

    if !resolved.starts_with(&canon_root) {
        return Err(anyhow!(
            "path '{}' escapes workspace root — access denied",
            relative
        ));
    }
    Ok(resolved)
}

// ---------------------------------------------------------------------------
// ShellExecTool
// ---------------------------------------------------------------------------

/// Runs a shell command in the workspace directory.
/// Capability scope: `workspace.exec`
pub struct ShellExecTool {
    agent_id: String,
    config: Arc<WorkspaceConfig>,
    caps: Arc<dyn CapabilityChecker>,
}

impl ShellExecTool {
    pub fn new(
        agent_id: String,
        config: Arc<WorkspaceConfig>,
        caps: Arc<dyn CapabilityChecker>,
    ) -> Self {
        Self {
            agent_id,
            config,
            caps,
        }
    }
}

#[async_trait::async_trait]
impl Tool for ShellExecTool {
    fn name(&self) -> &str {
        "shell_exec"
    }

    fn description(&self) -> &str {
        "Run a shell command in the workspace directory. Returns stdout, stderr, and exit code."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Maximum execution time in seconds (default: 60)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        self.caps.check(&self.agent_id, "workspace.exec")?;

        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: command"))?;

        let timeout = Duration::from_secs(
            params
                .get("timeout_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(self.config.exec_timeout.as_secs()),
        );

        let output = tokio::time::timeout(timeout, async {
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(&self.config.root)
                .output()
                .await
        })
        .await
        .map_err(|_| anyhow!("command timed out after {}s", timeout.as_secs()))?
        .map_err(|e| anyhow!("failed to execute command: {e}"))?;

        let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();

        // Truncate to max output size
        let max = self.config.max_output_bytes;
        if stdout.len() > max {
            stdout.truncate(max);
            stdout.push_str("\n... [truncated]");
        }
        if stderr.len() > max {
            stderr.truncate(max);
            stderr.push_str("\n... [truncated]");
        }

        let exit_code = output.status.code().unwrap_or(-1);
        let success = output.status.success();

        let result =
            format!("exit_code: {exit_code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");

        Ok(ToolResult {
            success,
            output: result,
        })
    }
}

// ---------------------------------------------------------------------------
// FileReadTool
// ---------------------------------------------------------------------------

/// Reads a file from the workspace directory.
/// Capability scope: `workspace.fs.read`
pub struct FileReadTool {
    agent_id: String,
    config: Arc<WorkspaceConfig>,
    caps: Arc<dyn CapabilityChecker>,
}

impl FileReadTool {
    pub fn new(
        agent_id: String,
        config: Arc<WorkspaceConfig>,
        caps: Arc<dyn CapabilityChecker>,
    ) -> Self {
        Self {
            agent_id,
            config,
            caps,
        }
    }
}

#[async_trait::async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "file_read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file in the workspace. Path must be relative to workspace root."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path to the file (e.g. 'src/main.rs')"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (1-based, default: 1)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read (default: all)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        self.caps.check(&self.agent_id, "workspace.fs.read")?;

        let path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: path"))?;

        let resolved = resolve_workspace_path(&self.config.root, path_str)?;

        let metadata = tokio::fs::metadata(&resolved)
            .await
            .map_err(|e| anyhow!("cannot read '{}': {}", path_str, e))?;

        if metadata.len() as usize > self.config.max_read_bytes {
            return Err(anyhow!(
                "file '{}' is {}B, exceeds max read size of {}B",
                path_str,
                metadata.len(),
                self.config.max_read_bytes
            ));
        }

        let content = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|e| anyhow!("cannot read '{}': {}", path_str, e))?;

        let offset = params
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1) as usize;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|l| l as usize);

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let start = (offset - 1).min(total);
        let end = limit.map(|l| (start + l).min(total)).unwrap_or(total);
        let selected: Vec<String> = lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>4}  {}", start + i + 1, line))
            .collect();

        let output = format!(
            "file: {} ({} lines total)\n{}",
            path_str,
            total,
            selected.join("\n")
        );

        Ok(ToolResult {
            success: true,
            output,
        })
    }
}

// ---------------------------------------------------------------------------
// FileWriteTool
// ---------------------------------------------------------------------------

/// Creates or overwrites a file in the workspace directory.
/// Capability scope: `workspace.fs.write`
pub struct FileWriteTool {
    agent_id: String,
    config: Arc<WorkspaceConfig>,
    caps: Arc<dyn CapabilityChecker>,
}

impl FileWriteTool {
    pub fn new(
        agent_id: String,
        config: Arc<WorkspaceConfig>,
        caps: Arc<dyn CapabilityChecker>,
    ) -> Self {
        Self {
            agent_id,
            config,
            caps,
        }
    }
}

#[async_trait::async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "file_write"
    }

    fn description(&self) -> &str {
        "Create or overwrite a file in the workspace. Creates parent directories if needed."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path to the file (e.g. 'src/index.html')"
                },
                "content": {
                    "type": "string",
                    "description": "File content to write"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        self.caps.check(&self.agent_id, "workspace.fs.write")?;

        let path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: path"))?;
        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: content"))?;

        if content.len() > self.config.max_write_bytes {
            return Err(anyhow!(
                "content is {}B, exceeds max write size of {}B",
                content.len(),
                self.config.max_write_bytes
            ));
        }

        let resolved = resolve_workspace_path(&self.config.root, path_str)?;

        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| anyhow!("cannot create parent dirs for '{}': {}", path_str, e))?;
        }

        tokio::fs::write(&resolved, content)
            .await
            .map_err(|e| anyhow!("cannot write '{}': {}", path_str, e))?;

        Ok(ToolResult {
            success: true,
            output: format!("wrote {} bytes to {}", content.len(), path_str),
        })
    }
}

// ---------------------------------------------------------------------------
// FileEditTool
// ---------------------------------------------------------------------------

/// Edits a file by replacing an exact string match. Workspace-scoped.
/// Capability scope: `workspace.fs.write`
pub struct FileEditTool {
    agent_id: String,
    config: Arc<WorkspaceConfig>,
    caps: Arc<dyn CapabilityChecker>,
}

impl FileEditTool {
    pub fn new(
        agent_id: String,
        config: Arc<WorkspaceConfig>,
        caps: Arc<dyn CapabilityChecker>,
    ) -> Self {
        Self {
            agent_id,
            config,
            caps,
        }
    }
}

#[async_trait::async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str {
        "file_edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing an exact string match. The old_string must appear exactly once in the file."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path to the file (e.g. 'src/main.rs')"
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact string to find and replace (must be unique in the file)"
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement string"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        self.caps.check(&self.agent_id, "workspace.fs.write")?;

        let path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: path"))?;
        let old_string = params
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: old_string"))?;
        let new_string = params
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: new_string"))?;

        let resolved = resolve_workspace_path(&self.config.root, path_str)?;

        let content = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|e| anyhow!("cannot read '{}': {}", path_str, e))?;

        let count = content.matches(old_string).count();
        if count == 0 {
            return Err(anyhow!(
                "old_string not found in '{}' — no changes made",
                path_str
            ));
        }
        if count > 1 {
            return Err(anyhow!(
                "old_string found {} times in '{}' — must be unique. Provide more context.",
                count,
                path_str
            ));
        }

        let new_content = content.replacen(old_string, new_string, 1);

        if new_content.len() > self.config.max_write_bytes {
            return Err(anyhow!(
                "edited file would be {}B, exceeds max write size of {}B",
                new_content.len(),
                self.config.max_write_bytes
            ));
        }

        tokio::fs::write(&resolved, &new_content)
            .await
            .map_err(|e| anyhow!("cannot write '{}': {}", path_str, e))?;

        Ok(ToolResult {
            success: true,
            output: format!(
                "edited {} — replaced {} bytes with {} bytes",
                path_str,
                old_string.len(),
                new_string.len()
            ),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct AllowAll;
    impl CapabilityChecker for AllowAll {
        fn check(&self, _agent_id: &str, _scope: &str) -> Result<()> {
            Ok(())
        }
    }

    struct DenyAll;
    impl CapabilityChecker for DenyAll {
        fn check(&self, _agent_id: &str, scope: &str) -> Result<()> {
            Err(anyhow!("capability denied: {scope}"))
        }
    }

    fn test_config(dir: &Path) -> Arc<WorkspaceConfig> {
        Arc::new(WorkspaceConfig {
            root: dir.to_path_buf(),
            max_read_bytes: 10_000,
            max_write_bytes: 10_000,
            exec_timeout: Duration::from_secs(10),
            max_output_bytes: 10_000,
        })
    }

    // -- resolve_workspace_path --

    #[test]
    fn resolve_rejects_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_workspace_path(dir.path(), "/etc/passwd").unwrap_err();
        assert!(err.to_string().contains("absolute paths not allowed"));
    }

    #[test]
    fn resolve_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.txt"), "hi").unwrap();
        let err = resolve_workspace_path(dir.path(), "../../../etc/passwd").unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace root")
                || err.to_string().contains("cannot resolve")
        );
    }

    #[test]
    fn resolve_allows_valid_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "world").unwrap();
        let resolved = resolve_workspace_path(dir.path(), "hello.txt").unwrap();
        assert!(resolved.ends_with("hello.txt"));
    }

    #[test]
    fn resolve_creates_parent_dirs_for_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_workspace_path(dir.path(), "sub/dir/new.txt").unwrap();
        assert!(resolved.ends_with("new.txt"));
        assert!(dir.path().join("sub/dir").exists());
    }

    // -- ShellExecTool --

    #[tokio::test]
    async fn shell_exec_runs_command() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ShellExecTool::new("a1".into(), test_config(dir.path()), Arc::new(AllowAll));
        let result = tool
            .execute(serde_json::json!({"command": "echo hello world"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("hello world"));
        assert!(result.output.contains("exit_code: 0"));
    }

    #[tokio::test]
    async fn shell_exec_captures_stderr_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ShellExecTool::new("a1".into(), test_config(dir.path()), Arc::new(AllowAll));
        let result = tool
            .execute(serde_json::json!({"command": "echo err >&2; exit 42"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("exit_code: 42"));
        assert!(result.output.contains("err"));
    }

    #[tokio::test]
    async fn shell_exec_runs_in_workspace_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "found").unwrap();
        let tool = ShellExecTool::new("a1".into(), test_config(dir.path()), Arc::new(AllowAll));
        let result = tool
            .execute(serde_json::json!({"command": "cat marker.txt"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("found"));
    }

    #[tokio::test]
    async fn shell_exec_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Arc::new(WorkspaceConfig {
            root: dir.path().to_path_buf(),
            exec_timeout: Duration::from_secs(1),
            ..Default::default()
        });
        let tool = ShellExecTool::new("a1".into(), cfg, Arc::new(AllowAll));
        let err = tool
            .execute(serde_json::json!({"command": "sleep 30"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn shell_exec_denied_without_capability() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ShellExecTool::new("a1".into(), test_config(dir.path()), Arc::new(DenyAll));
        let err = tool
            .execute(serde_json::json!({"command": "echo hi"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("capability denied"));
    }

    // -- FileReadTool --

    #[tokio::test]
    async fn file_read_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), "line1\nline2\nline3").unwrap();
        let tool = FileReadTool::new("a1".into(), test_config(dir.path()), Arc::new(AllowAll));
        let result = tool
            .execute(serde_json::json!({"path": "test.txt"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("line1"));
        assert!(result.output.contains("line3"));
        assert!(result.output.contains("3 lines total"));
    }

    #[tokio::test]
    async fn file_read_with_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), "a\nb\nc\nd\ne").unwrap();
        let tool = FileReadTool::new("a1".into(), test_config(dir.path()), Arc::new(AllowAll));
        let result = tool
            .execute(serde_json::json!({"path": "test.txt", "offset": 2, "limit": 2}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("   2  b"));
        assert!(result.output.contains("   3  c"));
        assert!(!result.output.contains("   1  a"));
        assert!(!result.output.contains("   4  d"));
    }

    #[tokio::test]
    async fn file_read_rejects_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileReadTool::new("a1".into(), test_config(dir.path()), Arc::new(AllowAll));
        let err = tool
            .execute(serde_json::json!({"path": "nonexistent.txt"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot read"));
    }

    #[tokio::test]
    async fn file_read_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileReadTool::new("a1".into(), test_config(dir.path()), Arc::new(AllowAll));
        let err = tool
            .execute(serde_json::json!({"path": "../../etc/passwd"}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace")
                || err.to_string().contains("cannot resolve")
        );
    }

    #[tokio::test]
    async fn file_read_denied_without_capability() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileReadTool::new("a1".into(), test_config(dir.path()), Arc::new(DenyAll));
        let err = tool
            .execute(serde_json::json!({"path": "test.txt"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("capability denied"));
    }

    // -- FileWriteTool --

    #[tokio::test]
    async fn file_write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileWriteTool::new("a1".into(), test_config(dir.path()), Arc::new(AllowAll));
        let result = tool
            .execute(serde_json::json!({"path": "new.txt", "content": "hello agent"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("11 bytes"));
        let content = std::fs::read_to_string(dir.path().join("new.txt")).unwrap();
        assert_eq!(content, "hello agent");
    }

    #[tokio::test]
    async fn file_write_creates_nested_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileWriteTool::new("a1".into(), test_config(dir.path()), Arc::new(AllowAll));
        let result = tool
            .execute(serde_json::json!({
                "path": "src/components/Button.tsx",
                "content": "export const Button = () => <button>Click</button>;"
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(dir.path().join("src/components/Button.tsx").exists());
    }

    #[tokio::test]
    async fn file_write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exist.txt"), "old").unwrap();
        let tool = FileWriteTool::new("a1".into(), test_config(dir.path()), Arc::new(AllowAll));
        let result = tool
            .execute(serde_json::json!({"path": "exist.txt", "content": "new content"}))
            .await
            .unwrap();
        assert!(result.success);
        let content = std::fs::read_to_string(dir.path().join("exist.txt")).unwrap();
        assert_eq!(content, "new content");
    }

    #[tokio::test]
    async fn file_write_rejects_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Arc::new(WorkspaceConfig {
            root: dir.path().to_path_buf(),
            max_write_bytes: 10,
            ..Default::default()
        });
        let tool = FileWriteTool::new("a1".into(), cfg, Arc::new(AllowAll));
        let err = tool
            .execute(serde_json::json!({"path": "big.txt", "content": "this is way too long for the limit"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exceeds max write size"));
    }

    #[tokio::test]
    async fn file_write_denied_without_capability() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileWriteTool::new("a1".into(), test_config(dir.path()), Arc::new(DenyAll));
        let err = tool
            .execute(serde_json::json!({"path": "x.txt", "content": "y"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("capability denied"));
    }

    // -- FileEditTool --

    #[tokio::test]
    async fn file_edit_replaces_string() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("app.rs"),
            "fn main() {\n    println!(\"hello\");\n}",
        )
        .unwrap();
        let tool = FileEditTool::new("a1".into(), test_config(dir.path()), Arc::new(AllowAll));
        let result = tool
            .execute(serde_json::json!({
                "path": "app.rs",
                "old_string": "println!(\"hello\")",
                "new_string": "println!(\"world\")"
            }))
            .await
            .unwrap();
        assert!(result.success);
        let content = std::fs::read_to_string(dir.path().join("app.rs")).unwrap();
        assert!(content.contains("println!(\"world\")"));
        assert!(!content.contains("println!(\"hello\")"));
    }

    #[tokio::test]
    async fn file_edit_rejects_not_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.rs"), "fn main() {}").unwrap();
        let tool = FileEditTool::new("a1".into(), test_config(dir.path()), Arc::new(AllowAll));
        let err = tool
            .execute(serde_json::json!({
                "path": "app.rs",
                "old_string": "this does not exist",
                "new_string": "replacement"
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn file_edit_rejects_ambiguous_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.rs"), "let x = 1;\nlet x = 2;").unwrap();
        let tool = FileEditTool::new("a1".into(), test_config(dir.path()), Arc::new(AllowAll));
        let err = tool
            .execute(serde_json::json!({
                "path": "app.rs",
                "old_string": "let x",
                "new_string": "let y"
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("found 2 times"));
    }

    #[tokio::test]
    async fn file_edit_denied_without_capability() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.rs"), "fn main() {}").unwrap();
        let tool = FileEditTool::new("a1".into(), test_config(dir.path()), Arc::new(DenyAll));
        let err = tool
            .execute(serde_json::json!({
                "path": "app.rs",
                "old_string": "main",
                "new_string": "start"
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("capability denied"));
    }
}
