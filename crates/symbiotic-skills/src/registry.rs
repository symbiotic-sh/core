//! Skill registry: loading, auto-detection, and trust gating.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{anyhow, Result};
use symbiotic_trust::AgentTrustLevel;
use thiserror::Error;

use crate::manifest::{load_manifest, SkillManifest};

/// Maximum number of skills auto-loaded per agent execution.
pub const MAX_SKILLS_PER_EXECUTION: usize = 3;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("skill not found: {0}")]
    SkillNotFound(String),
    #[error("trust too low: agent has {agent_level:?}, skill requires {required_level:?}")]
    TrustTooLow {
        agent_level: AgentTrustLevel,
        required_level: AgentTrustLevel,
    },
    #[error("missing required capabilities: {missing:?}")]
    MissingCapabilities { missing: Vec<String> },
    #[error("max skills limit reached ({0})")]
    MaxSkillsReached(usize),
}

/// A skill selected for loading, with its match priority.
#[derive(Debug, Clone)]
pub struct SkillMatch {
    pub manifest: SkillManifest,
    pub match_kind: MatchKind,
}

/// How a skill was matched.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchKind {
    /// Explicit invocation always wins (highest priority).
    Explicit,
    /// Keyword match in task text.
    Keyword,
    /// Domain match.
    Domain,
}

/// Context for skill auto-load detection.
pub struct AutoLoadContext<'a> {
    /// The task/goal text to search for triggers.
    pub task_text: &'a str,
    /// The domain of the current task (if known).
    pub task_domain: Option<&'a str>,
    /// Agent's current trust level.
    pub agent_trust: AgentTrustLevel,
    /// Agent's available capability scopes.
    pub agent_capabilities: &'a HashSet<String>,
}

/// Registry holding all known skills.
#[derive(Debug, Default)]
pub struct SkillRegistry {
    skills: Vec<SkillManifest>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self { skills: Vec::new() }
    }

    /// Register a pre-parsed manifest.
    pub fn register(&mut self, manifest: SkillManifest) {
        self.skills.push(manifest);
    }

    /// Hot-register a skill, replacing any existing skill with the same name.
    ///
    /// Returns the previous manifest if one was replaced, or `None` if this
    /// is a brand-new skill. Used by the synthesis pipeline to make a newly
    /// forged skill immediately available without restarting.
    pub fn register_hot(&mut self, manifest: SkillManifest) -> Option<SkillManifest> {
        if let Some(pos) = self.skills.iter().position(|s| s.name == manifest.name) {
            let old = std::mem::replace(&mut self.skills[pos], manifest);
            Some(old)
        } else {
            self.skills.push(manifest);
            None
        }
    }

    /// Scan a skills root directory and load all valid manifests.
    /// Skips directories that fail to parse (logs warning would happen at caller).
    pub fn load_from_dir(&mut self, skills_root: &Path) -> Result<Vec<String>> {
        let mut loaded = Vec::new();
        let mut errors = Vec::new();

        if !skills_root.exists() {
            return Ok(loaded);
        }

        let entries = std::fs::read_dir(skills_root)
            .map_err(|e| anyhow!("cannot read {}: {e}", skills_root.display()))?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join("manifest.toml");
            if !manifest_path.exists() {
                continue;
            }
            match load_manifest(&path) {
                Ok(manifest) => {
                    loaded.push(manifest.name.clone());
                    self.skills.push(manifest);
                }
                Err(e) => {
                    errors.push(format!("{}: {e}", path.display()));
                }
            }
        }

        if !errors.is_empty() && loaded.is_empty() {
            return Err(anyhow!(
                "all manifests failed to load: {}",
                errors.join("; ")
            ));
        }

        Ok(loaded)
    }

    /// Get a skill by name.
    pub fn get(&self, name: &str) -> Option<&SkillManifest> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// List all registered skill names.
    pub fn list(&self) -> Vec<&str> {
        self.skills.iter().map(|s| s.name.as_str()).collect()
    }

    /// Detect which skills should be auto-loaded for a given context.
    ///
    /// Returns up to MAX_SKILLS_PER_EXECUTION skills, sorted by match priority
    /// (explicit > keyword > domain).
    pub fn auto_detect(&self, ctx: &AutoLoadContext) -> Vec<SkillMatch> {
        let text_lower = ctx.task_text.to_lowercase();
        let mut matches: Vec<SkillMatch> = Vec::new();

        for skill in &self.skills {
            // Check explicit invocation first
            if let Some(ref invocation) = skill.invocation {
                let explicit_patterns =
                    [format!("use {invocation}"), format!("skill:{invocation}")];
                if explicit_patterns
                    .iter()
                    .any(|pat| text_lower.contains(&pat.to_lowercase()))
                {
                    matches.push(SkillMatch {
                        manifest: skill.clone(),
                        match_kind: MatchKind::Explicit,
                    });
                    continue;
                }
            }

            // Check keyword match
            let keyword_match = skill
                .keywords
                .iter()
                .any(|kw| text_lower.contains(&kw.to_lowercase()));
            if keyword_match {
                matches.push(SkillMatch {
                    manifest: skill.clone(),
                    match_kind: MatchKind::Keyword,
                });
                continue;
            }

            // Check domain match
            if let Some(task_domain) = ctx.task_domain {
                let domain_lower = task_domain.to_lowercase();
                let domain_match = skill
                    .domains
                    .iter()
                    .any(|d| d.to_lowercase() == domain_lower);
                if domain_match {
                    matches.push(SkillMatch {
                        manifest: skill.clone(),
                        match_kind: MatchKind::Domain,
                    });
                }
            }
        }

        // Sort by priority (Explicit < Keyword < Domain in enum order = Explicit first)
        matches.sort_by(|a, b| a.match_kind.cmp(&b.match_kind));

        // Apply trust and capability gating, keeping only valid skills
        let mut result: Vec<SkillMatch> = Vec::new();
        for m in matches {
            if m.manifest.min_trust_level > ctx.agent_trust {
                continue; // Trust too low, skip
            }
            if !m
                .manifest
                .required_capabilities
                .is_subset(ctx.agent_capabilities)
            {
                continue; // Missing required capabilities, skip
            }
            result.push(m);
            if result.len() >= MAX_SKILLS_PER_EXECUTION {
                break;
            }
        }

        result
    }

    /// Check if a specific skill can be used by an agent with the given trust and capabilities.
    pub fn check_access(
        &self,
        skill_name: &str,
        agent_trust: AgentTrustLevel,
        agent_capabilities: &HashSet<String>,
    ) -> Result<&SkillManifest, RegistryError> {
        let skill = self
            .skills
            .iter()
            .find(|s| s.name == skill_name)
            .ok_or_else(|| RegistryError::SkillNotFound(skill_name.to_string()))?;

        if skill.min_trust_level > agent_trust {
            return Err(RegistryError::TrustTooLow {
                agent_level: agent_trust,
                required_level: skill.min_trust_level,
            });
        }

        let missing: Vec<String> = skill
            .required_capabilities
            .iter()
            .filter(|cap| !agent_capabilities.contains(*cap))
            .cloned()
            .collect();

        if !missing.is_empty() {
            return Err(RegistryError::MissingCapabilities { missing });
        }

        Ok(skill)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_manifest;
    use std::fs;
    use tempfile::TempDir;

    fn make_skill(name: &str, trust: &str, keywords: &[&str], domains: &[&str]) -> SkillManifest {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(name);
        fs::create_dir_all(&dir).unwrap();

        let kw_toml = keywords
            .iter()
            .map(|k| format!("\"{k}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let dom_toml = domains
            .iter()
            .map(|d| format!("\"{d}\""))
            .collect::<Vec<_>>()
            .join(", ");

        let toml = format!(
            r#"
[skill]
name = "{name}"
version = "1.0.0"
description = "Test skill {name}"
min_trust_level = "{trust}"

[triggers]
keywords = [{kw_toml}]
domains = [{dom_toml}]
invocation = "{name}"
"#
        );

        parse_manifest(&toml, &dir).unwrap()
    }

    fn make_skill_with_caps(name: &str, trust: &str, required_caps: &[&str]) -> SkillManifest {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(name);
        fs::create_dir_all(&dir).unwrap();

        let caps_toml = required_caps
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");

        let toml = format!(
            r#"
[skill]
name = "{name}"
version = "1.0.0"
description = "Test skill {name}"
min_trust_level = "{trust}"

[capabilities]
required = [{caps_toml}]

[triggers]
keywords = ["test"]
invocation = "{name}"
"#
        );

        parse_manifest(&toml, &dir).unwrap()
    }

    #[test]
    fn auto_detect_explicit_invocation() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill("code-review", "ReadOnly", &["review"], &[]));

        let caps: HashSet<String> = HashSet::new();
        let matches = reg.auto_detect(&AutoLoadContext {
            task_text: "Please use code-review on this PR",
            task_domain: None,
            agent_trust: AgentTrustLevel::ExternalAct,
            agent_capabilities: &caps,
        });

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_kind, MatchKind::Explicit);
        assert_eq!(matches[0].manifest.name, "code-review");
    }

    #[test]
    fn auto_detect_skill_colon_syntax() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill("research", "ReadOnly", &["research"], &[]));

        let caps: HashSet<String> = HashSet::new();
        let matches = reg.auto_detect(&AutoLoadContext {
            task_text: "task has skill:research tag",
            task_domain: None,
            agent_trust: AgentTrustLevel::ExternalAct,
            agent_capabilities: &caps,
        });

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_kind, MatchKind::Explicit);
    }

    #[test]
    fn auto_detect_keyword_match() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill(
            "code-review",
            "ReadOnly",
            &["review", "code review"],
            &[],
        ));

        let caps: HashSet<String> = HashSet::new();
        let matches = reg.auto_detect(&AutoLoadContext {
            task_text: "Please review the code changes",
            task_domain: None,
            agent_trust: AgentTrustLevel::ExternalAct,
            agent_capabilities: &caps,
        });

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_kind, MatchKind::Keyword);
    }

    #[test]
    fn auto_detect_domain_match() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill(
            "code-review",
            "ReadOnly",
            &[],
            &["software-engineering"],
        ));

        let caps: HashSet<String> = HashSet::new();
        let matches = reg.auto_detect(&AutoLoadContext {
            task_text: "fix the login bug",
            task_domain: Some("software-engineering"),
            agent_trust: AgentTrustLevel::ExternalAct,
            agent_capabilities: &caps,
        });

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_kind, MatchKind::Domain);
    }

    #[test]
    fn auto_detect_explicit_wins_over_keyword() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill(
            "code-review",
            "ReadOnly",
            &["review"],
            &["software-engineering"],
        ));
        reg.register(make_skill("linter", "ReadOnly", &["review"], &[]));

        let caps: HashSet<String> = HashSet::new();
        let matches = reg.auto_detect(&AutoLoadContext {
            task_text: "use code-review to review the code",
            task_domain: Some("software-engineering"),
            agent_trust: AgentTrustLevel::ExternalAct,
            agent_capabilities: &caps,
        });

        // code-review matched explicitly, linter matched by keyword
        assert!(!matches.is_empty());
        assert_eq!(matches[0].match_kind, MatchKind::Explicit);
        assert_eq!(matches[0].manifest.name, "code-review");
    }

    #[test]
    fn auto_detect_trust_gating() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill(
            "sensitive-skill",
            "CredentialAccess",
            &["sensitive"],
            &[],
        ));

        let caps: HashSet<String> = HashSet::new();

        // Agent with low trust: no match
        let matches = reg.auto_detect(&AutoLoadContext {
            task_text: "do something sensitive",
            task_domain: None,
            agent_trust: AgentTrustLevel::ReadOnly,
            agent_capabilities: &caps,
        });
        assert_eq!(matches.len(), 0);

        // Agent with sufficient trust: match
        let matches = reg.auto_detect(&AutoLoadContext {
            task_text: "do something sensitive",
            task_domain: None,
            agent_trust: AgentTrustLevel::CredentialAccess,
            agent_capabilities: &caps,
        });
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn auto_detect_capability_gating() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill_with_caps(
            "needs-caps",
            "ReadOnly",
            &["archive.read", "file.read"],
        ));

        // Agent missing capabilities: no match
        let caps: HashSet<String> = ["archive.read".to_string()].into_iter().collect();
        let matches = reg.auto_detect(&AutoLoadContext {
            task_text: "test this",
            task_domain: None,
            agent_trust: AgentTrustLevel::ExternalAct,
            agent_capabilities: &caps,
        });
        assert_eq!(matches.len(), 0);

        // Agent with all capabilities: match
        let caps: HashSet<String> = ["archive.read".to_string(), "file.read".to_string()]
            .into_iter()
            .collect();
        let matches = reg.auto_detect(&AutoLoadContext {
            task_text: "test this",
            task_domain: None,
            agent_trust: AgentTrustLevel::ExternalAct,
            agent_capabilities: &caps,
        });
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn auto_detect_max_three_skills() {
        let mut reg = SkillRegistry::new();
        for i in 0..5 {
            let name = format!("skill-{i}");
            reg.register(make_skill(&name, "ReadOnly", &[&format!("kw{i}")], &[]));
        }

        let caps: HashSet<String> = HashSet::new();
        let matches = reg.auto_detect(&AutoLoadContext {
            task_text: "kw0 kw1 kw2 kw3 kw4",
            task_domain: None,
            agent_trust: AgentTrustLevel::ExternalAct,
            agent_capabilities: &caps,
        });

        assert_eq!(matches.len(), MAX_SKILLS_PER_EXECUTION);
    }

    #[test]
    fn check_access_trust_denied() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill("high-trust", "ExternalAct", &["secret"], &[]));

        let caps: HashSet<String> = HashSet::new();
        let err = reg
            .check_access("high-trust", AgentTrustLevel::ReadOnly, &caps)
            .unwrap_err();
        assert!(err.to_string().contains("trust too low"));
    }

    #[test]
    fn check_access_missing_capabilities() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill_with_caps(
            "needs-caps",
            "ReadOnly",
            &["file.read"],
        ));

        let caps: HashSet<String> = HashSet::new();
        let err = reg
            .check_access("needs-caps", AgentTrustLevel::ExternalAct, &caps)
            .unwrap_err();
        assert!(err.to_string().contains("missing required capabilities"));
    }

    #[test]
    fn check_access_skill_not_found() {
        let reg = SkillRegistry::new();
        let caps: HashSet<String> = HashSet::new();
        let err = reg
            .check_access("nonexistent", AgentTrustLevel::ExternalAct, &caps)
            .unwrap_err();
        assert!(err.to_string().contains("skill not found"));
    }

    #[test]
    fn check_access_success() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill_with_caps(
            "good-skill",
            "ArchiveWrite",
            &["archive.read"],
        ));

        let caps: HashSet<String> = ["archive.read".to_string()].into_iter().collect();
        let skill = reg
            .check_access("good-skill", AgentTrustLevel::ArchiveWrite, &caps)
            .unwrap();
        assert_eq!(skill.name, "good-skill");
    }

    #[test]
    fn load_from_dir_discovers_skills() {
        let tmp = TempDir::new().unwrap();
        let skills_root = tmp.path();

        // Create two skill directories
        let skill_a = skills_root.join("skill-a");
        fs::create_dir_all(&skill_a).unwrap();
        fs::write(
            skill_a.join("manifest.toml"),
            r#"
[skill]
name = "skill-a"
version = "1.0.0"
description = "Skill A"
"#,
        )
        .unwrap();

        let skill_b = skills_root.join("skill-b");
        fs::create_dir_all(&skill_b).unwrap();
        fs::write(
            skill_b.join("manifest.toml"),
            r#"
[skill]
name = "skill-b"
version = "2.0.0"
description = "Skill B"
"#,
        )
        .unwrap();

        let mut reg = SkillRegistry::new();
        let loaded = reg.load_from_dir(skills_root).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains(&"skill-a".to_string()));
        assert!(loaded.contains(&"skill-b".to_string()));
        assert_eq!(reg.list().len(), 2);
    }

    #[test]
    fn load_from_dir_skips_invalid_and_non_skill_dirs() {
        let tmp = TempDir::new().unwrap();
        let skills_root = tmp.path();

        // Valid skill
        let valid = skills_root.join("valid-skill");
        fs::create_dir_all(&valid).unwrap();
        fs::write(
            valid.join("manifest.toml"),
            r#"
[skill]
name = "valid-skill"
version = "1.0.0"
description = "Valid"
"#,
        )
        .unwrap();

        // Directory without manifest.toml
        let no_manifest = skills_root.join("no-manifest");
        fs::create_dir_all(&no_manifest).unwrap();

        // File (not directory)
        fs::write(skills_root.join("random-file.txt"), "not a skill").unwrap();

        let mut reg = SkillRegistry::new();
        let loaded = reg.load_from_dir(skills_root).unwrap();
        assert_eq!(loaded, vec!["valid-skill"]);
    }

    #[test]
    fn load_from_nonexistent_dir_returns_empty() {
        let mut reg = SkillRegistry::new();
        let loaded = reg.load_from_dir(Path::new("/nonexistent/path")).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn no_match_returns_empty() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill("code-review", "ReadOnly", &["review"], &[]));

        let caps: HashSet<String> = HashSet::new();
        let matches = reg.auto_detect(&AutoLoadContext {
            task_text: "deploy the application",
            task_domain: None,
            agent_trust: AgentTrustLevel::ExternalAct,
            agent_capabilities: &caps,
        });
        assert!(matches.is_empty());
    }

    #[test]
    fn hot_register_new_skill() {
        let mut reg = SkillRegistry::new();
        let skill = make_skill("new-skill", "ReadOnly", &["test"], &[]);
        let old = reg.register_hot(skill);
        assert!(old.is_none());
        assert_eq!(reg.list().len(), 1);
        assert!(reg.get("new-skill").is_some());
    }

    #[test]
    fn hot_register_replaces_existing() {
        let mut reg = SkillRegistry::new();
        let v1 = make_skill("my-skill", "ReadOnly", &["v1"], &[]);
        reg.register(v1);
        assert_eq!(reg.list().len(), 1);

        let v2 = make_skill("my-skill", "ArchiveWrite", &["v2"], &[]);
        let old = reg.register_hot(v2);
        assert!(old.is_some());
        assert_eq!(old.unwrap().min_trust_level, AgentTrustLevel::ReadOnly);

        // Registry should still have exactly 1 skill, with updated trust
        assert_eq!(reg.list().len(), 1);
        let current = reg.get("my-skill").unwrap();
        assert_eq!(current.min_trust_level, AgentTrustLevel::ArchiveWrite);
        assert!(current.keywords.contains(&"v2".to_string()));
    }
}
