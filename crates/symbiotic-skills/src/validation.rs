//! Validation hooks for skill output verification.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use thiserror::Error;

use crate::manifest::OnFailure;

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("validation script not found: {0}")]
    ScriptNotFound(String),
    #[error("validation script failed (exit {code}): {stderr}")]
    ScriptFailed { code: i32, stderr: String },
    #[error("validation timed out after {0}s")]
    Timeout(u64),
    #[error("failed to run validation: {0}")]
    ExecutionError(String),
}

/// Result of running a validation hook.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    pub passed: bool,
    pub message: Option<String>,
}

/// Action to take after validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationAction {
    /// Output is valid, proceed.
    Accept,
    /// Validation failed, escalate to human.
    Escalate { reason: String },
    /// Validation failed, retry the skill once.
    Retry { reason: String },
    /// Validation failed but just warn and continue.
    Warn { reason: String },
}

/// Run a validation script against skill output.
///
/// The script receives `output_json` on stdin and must:
/// - Exit 0 for pass
/// - Exit non-zero for fail, with reason on stderr
///
/// Returns a `ValidationResult` on success, or `ValidationError` on infrastructure failure.
pub fn run_validation_script(
    script_path: &Path,
    output_json: &str,
    timeout_secs: u64,
) -> Result<ValidationResult, ValidationError> {
    if !script_path.exists() {
        return Err(ValidationError::ScriptNotFound(
            script_path.display().to_string(),
        ));
    }
    let canonical_script = std::fs::canonicalize(script_path)
        .map_err(|_| ValidationError::ScriptNotFound(script_path.display().to_string()))?;
    if !canonical_script.is_file() {
        return Err(ValidationError::ScriptNotFound(
            script_path.display().to_string(),
        ));
    }

    let result = Command::new("bash")
        .arg(&canonical_script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();

    let mut child = match result {
        Ok(child) => child,
        Err(e) => return Err(ValidationError::ExecutionError(e.to_string())),
    };

    // Write input to stdin
    if let Some(ref mut stdin) = child.stdin {
        use std::io::Write;
        let _ = stdin.write_all(output_json.as_bytes());
    }
    // Drop stdin so the process can proceed
    drop(child.stdin.take());

    // Wait with timeout
    let timeout = Duration::from_secs(timeout_secs);
    match child.wait_timeout(timeout) {
        Ok(Some(status)) => {
            let stderr = child
                .stderr
                .as_mut()
                .map(|s| {
                    let mut buf = String::new();
                    use std::io::Read;
                    let _ = s.read_to_string(&mut buf);
                    buf
                })
                .unwrap_or_default();

            if status.success() {
                Ok(ValidationResult {
                    passed: true,
                    message: None,
                })
            } else {
                let _code = status.code().unwrap_or(-1);
                Ok(ValidationResult {
                    passed: false,
                    message: Some(stderr.trim().to_string()),
                })
            }
        }
        Ok(None) => {
            // Timeout - kill the process
            let _ = child.kill();
            Err(ValidationError::Timeout(timeout_secs))
        }
        Err(e) => Err(ValidationError::ExecutionError(e.to_string())),
    }
}

/// Determine the action to take based on validation result and on_failure policy.
pub fn determine_action(result: &ValidationResult, on_failure: &OnFailure) -> ValidationAction {
    if result.passed {
        return ValidationAction::Accept;
    }

    let reason = result
        .message
        .clone()
        .unwrap_or_else(|| "validation failed".to_string());

    match on_failure {
        OnFailure::Escalate => ValidationAction::Escalate { reason },
        OnFailure::Retry => ValidationAction::Retry { reason },
        OnFailure::Warn => ValidationAction::Warn { reason },
    }
}

/// Trait extension on `Command` child for wait with timeout.
trait WaitTimeout {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<std::process::ExitStatus>, std::io::Error>;
}

impl WaitTimeout for std::process::Child {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<std::process::ExitStatus>, std::io::Error> {
        let start = std::time::Instant::now();
        let poll_interval = Duration::from_millis(50);

        loop {
            match self.try_wait()? {
                Some(status) => return Ok(Some(status)),
                None => {
                    if start.elapsed() >= timeout {
                        return Ok(None);
                    }
                    std::thread::sleep(poll_interval);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_script(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn validation_script_passes() {
        let tmp = TempDir::new().unwrap();
        let script = write_script(tmp.path(), "validate.sh", "#!/bin/bash\nexit 0\n");

        let result = run_validation_script(&script, "{}", 5).unwrap();
        assert!(result.passed);
        assert!(result.message.is_none());
    }

    #[test]
    fn validation_script_fails_with_stderr() {
        let tmp = TempDir::new().unwrap();
        let script = write_script(
            tmp.path(),
            "validate.sh",
            "#!/bin/bash\necho 'missing section' >&2\nexit 1\n",
        );

        let result = run_validation_script(&script, "{}", 5).unwrap();
        assert!(!result.passed);
        assert_eq!(result.message.as_deref(), Some("missing section"));
    }

    #[test]
    fn validation_script_reads_stdin() {
        let tmp = TempDir::new().unwrap();
        let script = write_script(
            tmp.path(),
            "validate.sh",
            r#"#!/bin/bash
input=$(cat)
if echo "$input" | grep -q '"valid":true'; then
    exit 0
else
    echo "input not valid" >&2
    exit 1
fi
"#,
        );

        let result = run_validation_script(&script, r#"{"valid":true}"#, 5).unwrap();
        assert!(result.passed);

        let result = run_validation_script(&script, r#"{"valid":false}"#, 5).unwrap();
        assert!(!result.passed);
    }

    #[test]
    fn validation_script_not_found() {
        let err = run_validation_script(Path::new("/nonexistent/script.sh"), "{}", 5).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn validation_timeout() {
        let tmp = TempDir::new().unwrap();
        let script = write_script(tmp.path(), "slow.sh", "#!/bin/bash\nsleep 60\n");

        let err = run_validation_script(&script, "{}", 1).unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn determine_action_accept_on_pass() {
        let result = ValidationResult {
            passed: true,
            message: None,
        };
        assert_eq!(
            determine_action(&result, &OnFailure::Escalate),
            ValidationAction::Accept
        );
    }

    #[test]
    fn determine_action_escalate_on_fail() {
        let result = ValidationResult {
            passed: false,
            message: Some("bad output".to_string()),
        };
        assert_eq!(
            determine_action(&result, &OnFailure::Escalate),
            ValidationAction::Escalate {
                reason: "bad output".to_string()
            }
        );
    }

    #[test]
    fn determine_action_retry_on_fail() {
        let result = ValidationResult {
            passed: false,
            message: Some("retry me".to_string()),
        };
        assert_eq!(
            determine_action(&result, &OnFailure::Retry),
            ValidationAction::Retry {
                reason: "retry me".to_string()
            }
        );
    }

    #[test]
    fn determine_action_warn_on_fail() {
        let result = ValidationResult {
            passed: false,
            message: None,
        };
        assert_eq!(
            determine_action(&result, &OnFailure::Warn),
            ValidationAction::Warn {
                reason: "validation failed".to_string()
            }
        );
    }
}
