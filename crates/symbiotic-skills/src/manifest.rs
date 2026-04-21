//! Skill manifest types parsed from TOML.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use symbiotic_trust::AgentTrustLevel;
use thiserror::Error;

/// Errors from manifest parsing and validation.
#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("failed to read manifest: {0}")]
    ReadFailed(#[from] std::io::Error),
    #[error("invalid TOML: {0}")]
    InvalidToml(#[from] toml::de::Error),
    #[error("manifest name {manifest_name:?} does not match directory name {dir_name:?}")]
    NameMismatch {
        manifest_name: String,
        dir_name: String,
    },
    #[error("invalid semver version: {0}")]
    InvalidVersion(String),
    #[error("referenced file does not exist: {0}")]
    MissingFile(String),
    #[error("invalid trust level: {0}")]
    InvalidAgentTrustLevel(String),
    #[error("referenced path escapes skill directory: {0}")]
    PathEscape(String),
}

/// Raw TOML structure matching the manifest schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestToml {
    pub skill: SkillSection,
    #[serde(default)]
    pub capabilities: Option<CapabilitiesSection>,
    #[serde(default)]
    pub triggers: Option<TriggersSection>,
    #[serde(default)]
    pub files: Option<FilesSection>,
    #[serde(default)]
    pub validation: Option<ValidationSection>,
    #[serde(default)]
    pub metadata: Option<MetadataSection>,
    #[serde(default)]
    pub synthesis: Option<SynthesisProvenance>,
    #[serde(default)]
    pub targets: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSection {
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default = "default_trust_level")]
    pub min_trust_level: String,
}

fn default_trust_level() -> String {
    "ReadOnly".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapabilitiesSection {
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default)]
    pub optional: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TriggersSection {
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub invocation: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilesSection {
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub rubric: Option<String>,
    #[serde(default)]
    pub examples: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValidationSection {
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_on_failure")]
    pub on_failure: String,
}

fn default_timeout() -> u64 {
    30
}

fn default_on_failure() -> String {
    "warn".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetadataSection {
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Provenance tracking for synthesized skills.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SynthesisProvenance {
    /// Agent that requested synthesis.
    #[serde(default)]
    pub requesting_agent: Option<String>,
    /// Session ID linking back to the synthesis run.
    #[serde(default)]
    pub synthesis_session: Option<String>,
    /// Description of the problem that triggered synthesis.
    #[serde(default)]
    pub problem_description: Option<String>,
    /// Number of code-generation retries before tests passed.
    #[serde(default)]
    pub retry_count: Option<u32>,
}

/// Validated skill manifest ready for use.
#[derive(Debug, Clone)]
pub struct SkillManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub min_trust_level: AgentTrustLevel,
    pub required_capabilities: HashSet<String>,
    pub optional_capabilities: HashSet<String>,
    pub keywords: Vec<String>,
    pub domains: Vec<String>,
    pub invocation: Option<String>,
    pub prompt_file: Option<String>,
    pub rubric_file: Option<String>,
    pub example_globs: Vec<String>,
    pub validation_script: Option<String>,
    pub validation_timeout_secs: u64,
    pub on_failure: OnFailure,
    pub tags: Vec<String>,
    pub skill_dir: PathBuf,
    /// Provenance for synthesized skills (None for hand-crafted skills).
    pub synthesis: Option<SynthesisProvenance>,
    /// Target-triple to binary-path map for multi-platform skill bundles.
    pub targets: HashMap<String, String>,
}

/// What to do when validation fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnFailure {
    Escalate,
    Retry,
    Warn,
}

impl OnFailure {
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "escalate" => Self::Escalate,
            "retry" => Self::Retry,
            _ => Self::Warn,
        }
    }
}

/// Parse a trust level string to the actual AgentTrustLevel enum.
pub fn parse_trust_level(s: &str) -> Result<AgentTrustLevel, ManifestError> {
    match s {
        "ReadOnly" => Ok(AgentTrustLevel::ReadOnly),
        "ArchiveWrite" | "Basic" | "Standard" => Ok(AgentTrustLevel::ArchiveWrite),
        "CredentialAccess" | "Trusted" => Ok(AgentTrustLevel::CredentialAccess),
        "ExternalAct" | "FullyTrusted" => Ok(AgentTrustLevel::ExternalAct),
        other => Err(ManifestError::InvalidAgentTrustLevel(other.to_string())),
    }
}

/// Parse and validate a manifest from a TOML string, given the skill directory.
pub fn parse_manifest(toml_str: &str, skill_dir: &Path) -> Result<SkillManifest, ManifestError> {
    let raw: ManifestToml = toml::from_str(toml_str)?;
    validate_manifest(&raw, skill_dir)
}

/// Load and validate a manifest from a skill directory (reads manifest.toml).
pub fn load_manifest(skill_dir: &Path) -> Result<SkillManifest, ManifestError> {
    let manifest_path = skill_dir.join("manifest.toml");
    let content = std::fs::read_to_string(&manifest_path)?;
    parse_manifest(&content, skill_dir)
}

fn validate_manifest(raw: &ManifestToml, skill_dir: &Path) -> Result<SkillManifest, ManifestError> {
    // Name must match directory name
    if let Some(dir_name) = skill_dir.file_name().and_then(|n| n.to_str()) {
        if raw.skill.name != dir_name {
            return Err(ManifestError::NameMismatch {
                manifest_name: raw.skill.name.clone(),
                dir_name: dir_name.to_string(),
            });
        }
    }

    // Version must be valid semver (basic check: X.Y.Z)
    validate_semver(&raw.skill.version)?;

    // Trust level must be valid
    let min_trust_level = parse_trust_level(&raw.skill.min_trust_level)?;

    // Validate referenced files exist (if skill_dir exists on disk)
    if skill_dir.exists() {
        let canonical_skill_dir = std::fs::canonicalize(skill_dir)?;
        if let Some(ref files) = raw.files {
            if let Some(ref prompt) = files.prompt {
                validate_referenced_path(&canonical_skill_dir, skill_dir, prompt)?;
            }
            if let Some(ref rubric) = files.rubric {
                validate_referenced_path(&canonical_skill_dir, skill_dir, rubric)?;
            }
        }
        if let Some(ref val) = raw.validation {
            if let Some(ref script) = val.script {
                validate_referenced_path(&canonical_skill_dir, skill_dir, script)?;
            }
        }
    }

    let caps = raw.capabilities.clone().unwrap_or_default();
    let triggers = raw.triggers.clone().unwrap_or_default();
    let files = raw.files.clone().unwrap_or_default();
    let validation = raw.validation.clone().unwrap_or_default();
    let metadata = raw.metadata.clone().unwrap_or_default();

    Ok(SkillManifest {
        name: raw.skill.name.clone(),
        version: raw.skill.version.clone(),
        description: raw.skill.description.clone(),
        min_trust_level,
        required_capabilities: caps.required.into_iter().collect(),
        optional_capabilities: caps.optional.into_iter().collect(),
        keywords: triggers.keywords,
        domains: triggers.domains,
        invocation: triggers.invocation,
        prompt_file: files.prompt,
        rubric_file: files.rubric,
        example_globs: files.examples,
        validation_script: validation.script,
        validation_timeout_secs: validation.timeout_secs,
        on_failure: OnFailure::parse(&validation.on_failure),
        tags: metadata.tags,
        skill_dir: skill_dir.to_path_buf(),
        synthesis: raw.synthesis.clone(),
        targets: raw.targets.clone().unwrap_or_default(),
    })
}

fn validate_semver(version: &str) -> Result<(), ManifestError> {
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() != 3 {
        return Err(ManifestError::InvalidVersion(version.to_string()));
    }
    for part in parts {
        if part.parse::<u64>().is_err() {
            return Err(ManifestError::InvalidVersion(version.to_string()));
        }
    }
    Ok(())
}

fn validate_referenced_path(
    canonical_skill_dir: &Path,
    skill_dir: &Path,
    relative: &str,
) -> Result<(), ManifestError> {
    let path = skill_dir.join(relative);
    if !path.exists() {
        return Err(ManifestError::MissingFile(relative.to_string()));
    }

    let canonical = std::fs::canonicalize(&path)?;
    if !canonical.starts_with(canonical_skill_dir) {
        return Err(ManifestError::PathEscape(relative.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_skill_dir(name: &str) -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        (tmp, dir)
    }

    fn minimal_toml(name: &str) -> String {
        format!(
            r#"
[skill]
name = "{name}"
version = "1.0.0"
description = "A test skill"
min_trust_level = "ReadOnly"
"#
        )
    }

    #[test]
    fn parse_minimal_manifest() {
        let (_tmp, dir) = make_skill_dir("test-skill");
        let manifest = parse_manifest(&minimal_toml("test-skill"), &dir).unwrap();
        assert_eq!(manifest.name, "test-skill");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.min_trust_level, AgentTrustLevel::ReadOnly);
        assert!(manifest.keywords.is_empty());
        assert!(manifest.required_capabilities.is_empty());
    }

    #[test]
    fn parse_full_manifest() {
        let (_tmp, dir) = make_skill_dir("code-review");
        // Create referenced files
        fs::write(dir.join("prompt.md"), "review prompt").unwrap();
        fs::write(dir.join("rubric.md"), "rubric content").unwrap();
        fs::write(dir.join("validation.sh"), "#!/bin/bash\nexit 0").unwrap();

        let toml = r#"
[skill]
name = "code-review"
version = "1.0.0"
description = "Structured code review"
min_trust_level = "ArchiveWrite"

[capabilities]
required = ["archive.read", "file.read"]
optional = ["file.write"]

[triggers]
keywords = ["review", "code review", "PR review"]
domains = ["software-engineering"]
invocation = "code-review"

[files]
prompt = "prompt.md"
rubric = "rubric.md"
examples = ["examples/*.md"]

[validation]
script = "validation.sh"
timeout_secs = 30
on_failure = "escalate"

[metadata]
author = "symbiotic"
created = "2026-02-06"
tags = ["quality", "security"]
"#;

        let manifest = parse_manifest(toml, &dir).unwrap();
        assert_eq!(manifest.name, "code-review");
        assert_eq!(manifest.min_trust_level, AgentTrustLevel::ArchiveWrite);
        assert!(manifest.required_capabilities.contains("archive.read"));
        assert!(manifest.required_capabilities.contains("file.read"));
        assert_eq!(manifest.keywords.len(), 3);
        assert_eq!(manifest.domains, vec!["software-engineering"]);
        assert_eq!(manifest.invocation.as_deref(), Some("code-review"));
        assert_eq!(manifest.on_failure, OnFailure::Escalate);
        assert_eq!(manifest.validation_script.as_deref(), Some("validation.sh"));
    }

    #[test]
    fn name_mismatch_fails() {
        let (_tmp, dir) = make_skill_dir("actual-name");
        let err = parse_manifest(&minimal_toml("wrong-name"), &dir).unwrap_err();
        assert!(err.to_string().contains("does not match directory name"));
    }

    #[test]
    fn invalid_semver_fails() {
        let (_tmp, dir) = make_skill_dir("test-skill");
        let toml = r#"
[skill]
name = "test-skill"
version = "not-semver"
description = "bad"
"#;
        let err = parse_manifest(toml, &dir).unwrap_err();
        assert!(err.to_string().contains("invalid semver"));
    }

    #[test]
    fn invalid_trust_level_fails() {
        let (_tmp, dir) = make_skill_dir("test-skill");
        let toml = r#"
[skill]
name = "test-skill"
version = "1.0.0"
description = "bad"
min_trust_level = "SuperAdmin"
"#;
        let err = parse_manifest(toml, &dir).unwrap_err();
        assert!(err.to_string().contains("invalid trust level"));
    }

    #[test]
    fn missing_referenced_file_fails() {
        let (_tmp, dir) = make_skill_dir("test-skill");
        let toml = r#"
[skill]
name = "test-skill"
version = "1.0.0"
description = "bad"

[files]
prompt = "nonexistent.md"
"#;
        let err = parse_manifest(toml, &dir).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn validation_script_path_escape_fails() {
        let (tmp, dir) = make_skill_dir("test-skill");
        let outside = tmp.path().join("outside.sh");
        fs::write(&outside, "#!/bin/bash\nexit 0\n").unwrap();
        let toml = r#"
[skill]
name = "test-skill"
version = "1.0.0"
description = "bad"

[validation]
script = "../outside.sh"
"#;
        let err = parse_manifest(toml, &dir).unwrap_err();
        assert!(matches!(err, ManifestError::PathEscape(_)));
    }

    #[test]
    fn invalid_toml_fails() {
        let (_tmp, dir) = make_skill_dir("test-skill");
        let err = parse_manifest("this is not valid toml {{{", &dir).unwrap_err();
        assert!(err.to_string().contains("invalid TOML"));
    }

    #[test]
    fn load_manifest_from_dir() {
        let (_tmp, dir) = make_skill_dir("my-skill");
        fs::write(dir.join("prompt.md"), "instructions").unwrap();
        fs::write(
            dir.join("manifest.toml"),
            r#"
[skill]
name = "my-skill"
version = "0.1.0"
description = "Loads from disk"

[files]
prompt = "prompt.md"
"#,
        )
        .unwrap();

        let manifest = load_manifest(&dir).unwrap();
        assert_eq!(manifest.name, "my-skill");
        assert_eq!(manifest.prompt_file.as_deref(), Some("prompt.md"));
    }

    #[test]
    fn parse_manifest_with_synthesis_provenance() {
        let (_tmp, dir) = make_skill_dir("synth-skill");
        let toml = r#"
[skill]
name = "synth-skill"
version = "1.0.0"
description = "A synthesized skill"

[synthesis]
requesting_agent = "agent-42"
synthesis_session = "sess-abc-123"
problem_description = "Needed to parse CSV files"
retry_count = 1

[targets]
"aarch64-apple-darwin" = "./bin/aarch64-apple-darwin"
"x86_64-unknown-linux-musl" = "./bin/x86_64-unknown-linux-musl"
"#;
        let manifest = parse_manifest(toml, &dir).unwrap();
        let synth = manifest.synthesis.unwrap();
        assert_eq!(synth.requesting_agent.as_deref(), Some("agent-42"));
        assert_eq!(synth.synthesis_session.as_deref(), Some("sess-abc-123"));
        assert_eq!(synth.retry_count, Some(1));
        assert_eq!(manifest.targets.len(), 2);
        assert_eq!(
            manifest.targets.get("aarch64-apple-darwin").unwrap(),
            "./bin/aarch64-apple-darwin"
        );
    }

    #[test]
    fn parse_manifest_without_synthesis_provenance() {
        let (_tmp, dir) = make_skill_dir("plain-skill");
        let manifest = parse_manifest(&minimal_toml("plain-skill"), &dir).unwrap();
        assert!(manifest.synthesis.is_none());
        assert!(manifest.targets.is_empty());
    }

    #[test]
    fn synthesis_provenance_round_trips() {
        let prov = SynthesisProvenance {
            requesting_agent: Some("agent-1".to_string()),
            synthesis_session: Some("sess-xyz".to_string()),
            problem_description: Some("Parsing binary formats".to_string()),
            retry_count: Some(2),
        };
        let toml_str = toml::to_string(&prov).unwrap();
        let parsed: SynthesisProvenance = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.requesting_agent, prov.requesting_agent);
        assert_eq!(parsed.synthesis_session, prov.synthesis_session);
        assert_eq!(parsed.retry_count, prov.retry_count);
    }

    #[test]
    fn parse_trust_level_maps_design_names() {
        // Design doc names map to code trust levels
        assert_eq!(
            parse_trust_level("ReadOnly").unwrap(),
            AgentTrustLevel::ReadOnly
        );
        assert_eq!(
            parse_trust_level("Basic").unwrap(),
            AgentTrustLevel::ArchiveWrite
        );
        assert_eq!(
            parse_trust_level("Standard").unwrap(),
            AgentTrustLevel::ArchiveWrite
        );
        assert_eq!(
            parse_trust_level("Trusted").unwrap(),
            AgentTrustLevel::CredentialAccess
        );
        assert_eq!(
            parse_trust_level("FullyTrusted").unwrap(),
            AgentTrustLevel::ExternalAct
        );
        // Direct code names also work
        assert_eq!(
            parse_trust_level("ArchiveWrite").unwrap(),
            AgentTrustLevel::ArchiveWrite
        );
        assert_eq!(
            parse_trust_level("CredentialAccess").unwrap(),
            AgentTrustLevel::CredentialAccess
        );
        assert_eq!(
            parse_trust_level("ExternalAct").unwrap(),
            AgentTrustLevel::ExternalAct
        );
    }
}
