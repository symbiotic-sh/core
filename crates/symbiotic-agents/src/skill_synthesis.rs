//! Skill synthesis: automatic creation of reusable skills from PE observations.
//!
//! When the Process Engineer detects a manually-solved pattern recurring 2+ times,
//! it triggers skill synthesis. The synthesized skill is validated and hot-loaded
//! into the skill registry.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Minimum occurrences of a pattern before triggering skill synthesis.
pub const SYNTHESIS_THRESHOLD: u32 = 2;

/// A detected pattern that may warrant skill synthesis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedPattern {
    /// Human-readable pattern name (e.g., "yaml-frontmatter-parsing").
    pub name: String,
    /// Description of what the pattern does.
    pub description: String,
    /// Goal types where this pattern was observed.
    pub observed_in: Vec<String>,
    /// Number of times this pattern was manually implemented.
    pub occurrences: u32,
    /// Example invocation or usage snippet.
    pub example: Option<String>,
}

/// Result of attempting skill synthesis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SynthesisOutcome {
    /// Skill was successfully created and registered.
    Created {
        skill_name: String,
        skill_dir: PathBuf,
    },
    /// Pattern hasn't reached the synthesis threshold yet.
    BelowThreshold {
        pattern: String,
        occurrences: u32,
        threshold: u32,
    },
    /// Skill already exists for this pattern.
    AlreadyExists { skill_name: String },
}

/// Generate a skill manifest template from a detected pattern.
pub fn generate_skill_template(pattern: &DetectedPattern, skills_root: &Path) -> Result<PathBuf> {
    let skill_dir = skills_root.join(&pattern.name);
    std::fs::create_dir_all(&skill_dir)
        .with_context(|| format!("creating skill dir: {}", skill_dir.display()))?;

    // Write manifest.toml
    let manifest = format!(
        r#"[skill]
name = "{name}"
version = "0.1.0"
description = "{description}"
invocation = "{name}"

[skill.triggers]
keywords = [{keywords}]

[skill.requirements]
trust_level = "standard"
capabilities = ["archive.read"]
"#,
        name = pattern.name,
        description = pattern.description.replace('"', r#"\""#),
        keywords = pattern
            .observed_in
            .iter()
            .map(|k| format!("\"{}\"", k))
            .collect::<Vec<_>>()
            .join(", "),
    );
    std::fs::write(skill_dir.join("manifest.toml"), &manifest)?;

    // Write SKILL.md
    let skill_md = format!(
        "# {name}\n\n\
        ## Description\n\n\
        {description}\n\n\
        ## Usage\n\n\
        ```\n\
        skill:{name}\n\
        ```\n\n\
        ## Origin\n\n\
        Auto-synthesized by Process Engineer after detecting pattern \
        across {count} goal executions: {goals}\n\n\
        {example}\
        ## Implementation\n\n\
        <!-- PE or human implements the skill logic here -->\n",
        name = pattern.name,
        description = pattern.description,
        count = pattern.occurrences,
        goals = pattern.observed_in.join(", "),
        example = pattern
            .example
            .as_ref()
            .map(|e| format!("## Example\n\n```\n{e}\n```\n\n"))
            .unwrap_or_default(),
    );
    std::fs::write(skill_dir.join("SKILL.md"), &skill_md)?;

    Ok(skill_dir)
}

/// Check if a skill already exists for the given pattern name.
pub fn skill_exists(pattern_name: &str, skills_root: &Path) -> bool {
    skills_root
        .join(pattern_name)
        .join("manifest.toml")
        .exists()
}

/// Attempt to synthesize a skill from a detected pattern.
///
/// Returns `SynthesisOutcome` indicating what happened.
pub fn try_synthesize(pattern: &DetectedPattern, skills_root: &Path) -> Result<SynthesisOutcome> {
    if pattern.occurrences < SYNTHESIS_THRESHOLD {
        return Ok(SynthesisOutcome::BelowThreshold {
            pattern: pattern.name.clone(),
            occurrences: pattern.occurrences,
            threshold: SYNTHESIS_THRESHOLD,
        });
    }

    if skill_exists(&pattern.name, skills_root) {
        return Ok(SynthesisOutcome::AlreadyExists {
            skill_name: pattern.name.clone(),
        });
    }

    let skill_dir = generate_skill_template(pattern, skills_root)?;
    Ok(SynthesisOutcome::Created {
        skill_name: pattern.name.clone(),
        skill_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn synthesis_below_threshold_returns_early() {
        let tmp = TempDir::new().unwrap();
        let pattern = DetectedPattern {
            name: "test-pattern".to_string(),
            description: "A test pattern".to_string(),
            observed_in: vec!["research".to_string()],
            occurrences: 1,
            example: None,
        };

        let result = try_synthesize(&pattern, tmp.path()).unwrap();
        assert!(matches!(result, SynthesisOutcome::BelowThreshold { .. }));
    }

    #[test]
    fn synthesis_creates_skill_at_threshold() {
        let tmp = TempDir::new().unwrap();
        let pattern = DetectedPattern {
            name: "yaml-parsing".to_string(),
            description: "Parse YAML frontmatter from markdown".to_string(),
            observed_in: vec!["intake".to_string(), "import".to_string()],
            occurrences: 3,
            example: Some("parse_frontmatter(content)".to_string()),
        };

        let result = try_synthesize(&pattern, tmp.path()).unwrap();
        match result {
            SynthesisOutcome::Created {
                skill_name,
                skill_dir,
            } => {
                assert_eq!(skill_name, "yaml-parsing");
                assert!(skill_dir.join("manifest.toml").exists());
                assert!(skill_dir.join("SKILL.md").exists());
            }
            _ => panic!("expected Created"),
        }
    }

    #[test]
    fn synthesis_detects_existing_skill() {
        let tmp = TempDir::new().unwrap();
        let pattern = DetectedPattern {
            name: "existing-skill".to_string(),
            description: "Already exists".to_string(),
            observed_in: vec![],
            occurrences: 5,
            example: None,
        };

        // Create skill dir manually
        let skill_dir = tmp.path().join("existing-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("manifest.toml"),
            "[skill]\nname = \"existing-skill\"\n",
        )
        .unwrap();

        let result = try_synthesize(&pattern, tmp.path()).unwrap();
        assert!(matches!(result, SynthesisOutcome::AlreadyExists { .. }));
    }
}
