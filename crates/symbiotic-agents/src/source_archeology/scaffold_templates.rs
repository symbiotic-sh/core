//! Baseline scaffold templates for the Source Archeology Scaffold stage.
//!
//! Templates are embedded via `include_str!()` from
//! `crates/symbiotic-agents/templates/external-repo-scaffolds/docs-starter/`
//! so the agents crate ships them as static strings. Each template uses
//! `{{placeholder}}` tokens that `substitute_placeholders()` replaces at
//! render time.
//!
//! Scaffold's MVP is LLM-only generation; these templates are provided
//! so (a) T127 Auto-Provision can populate a freshly-created `{slug}-docs`
//! repo with a baseline layout before the LLM fills in the specifics,
//! and (b) the LLM prompt can include the template as a structural
//! reference rather than free-form generation.

use std::collections::HashMap;

/// One template file with its relative destination path within
/// `{slug}-docs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaffoldTemplate {
    /// Relative path in `{slug}-docs`, e.g. `"README.md"` or
    /// `"docs/architecture.md"`.
    pub rel_path: &'static str,
    /// Raw template body with `{{placeholder}}` tokens.
    pub body: &'static str,
}

/// All docs-starter templates, in dependency-free order. Consumers can
/// iterate this to materialize an entire `{slug}-docs` skeleton.
pub fn docs_starter_templates() -> Vec<ScaffoldTemplate> {
    vec![
        ScaffoldTemplate {
            rel_path: "README.md",
            body: include_str!("../../templates/external-repo-scaffolds/docs-starter/README.md"),
        },
        ScaffoldTemplate {
            rel_path: "CLAUDE.md",
            body: include_str!("../../templates/external-repo-scaffolds/docs-starter/CLAUDE.md"),
        },
        ScaffoldTemplate {
            rel_path: "AGENTS.md",
            body: include_str!("../../templates/external-repo-scaffolds/docs-starter/AGENTS.md"),
        },
        ScaffoldTemplate {
            rel_path: "docs/architecture.md",
            body: include_str!(
                "../../templates/external-repo-scaffolds/docs-starter/docs/architecture.md"
            ),
        },
        ScaffoldTemplate {
            rel_path: "docs/build.md",
            body: include_str!(
                "../../templates/external-repo-scaffolds/docs-starter/docs/build.md"
            ),
        },
        ScaffoldTemplate {
            rel_path: "docs/test.md",
            body: include_str!("../../templates/external-repo-scaffolds/docs-starter/docs/test.md"),
        },
        ScaffoldTemplate {
            rel_path: "docs/deploy.md",
            body: include_str!(
                "../../templates/external-repo-scaffolds/docs-starter/docs/deploy.md"
            ),
        },
    ]
}

/// Substitute `{{key}}` tokens in `body` using the `placeholders` map.
/// Unknown tokens are left as-is (visible in the output) so callers
/// notice missing context rather than ending up with silent blanks.
pub fn substitute_placeholders(body: &str, placeholders: &HashMap<&str, &str>) -> String {
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    let bytes = body.as_bytes();
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find the matching '}}'.
            if let Some(end_rel) = body[i + 2..].find("}}") {
                let key = body[i + 2..i + 2 + end_rel].trim();
                if let Some(val) = placeholders.get(key) {
                    out.push_str(val);
                    i += 2 + end_rel + 2;
                    continue;
                }
            }
        }
        out.push(body[i..=i].chars().next().unwrap());
        i += 1;
    }
    out
}

/// Convenience: substitute a template with standard project
/// placeholders.
pub fn render_with_project(
    template: &ScaffoldTemplate,
    project_name: &str,
    project_slug: &str,
    project_description: &str,
    project_clone_url: &str,
) -> String {
    let mut placeholders = HashMap::new();
    placeholders.insert("project_name", project_name);
    placeholders.insert("project_slug", project_slug);
    placeholders.insert("project_description", project_description);
    placeholders.insert("project_clone_url", project_clone_url);
    substitute_placeholders(template.body, &placeholders)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docs_starter_returns_expected_file_set() {
        let templates = docs_starter_templates();
        let paths: Vec<&str> = templates.iter().map(|t| t.rel_path).collect();
        assert!(paths.contains(&"README.md"));
        assert!(paths.contains(&"CLAUDE.md"));
        assert!(paths.contains(&"AGENTS.md"));
        assert!(paths.contains(&"docs/architecture.md"));
        assert!(paths.contains(&"docs/build.md"));
        assert!(paths.contains(&"docs/test.md"));
        assert!(paths.contains(&"docs/deploy.md"));
        assert_eq!(templates.len(), 7);
    }

    #[test]
    fn templates_have_nonempty_bodies() {
        for t in docs_starter_templates() {
            assert!(!t.body.is_empty(), "template {} has empty body", t.rel_path);
        }
    }

    #[test]
    fn substitute_replaces_known_placeholders() {
        let mut p = HashMap::new();
        p.insert("name", "Flux");
        p.insert("slug", "flux");
        let rendered = substitute_placeholders("# {{name}}\n\nSlug: {{slug}}\n", &p);
        assert_eq!(rendered, "# Flux\n\nSlug: flux\n");
    }

    #[test]
    fn substitute_leaves_unknown_placeholders_visible() {
        let p = HashMap::new();
        let rendered = substitute_placeholders("Value: {{missing}}", &p);
        assert_eq!(
            rendered, "Value: {{missing}}",
            "unknown placeholders stay visible so callers notice missing context"
        );
    }

    #[test]
    fn render_with_project_fills_standard_tokens() {
        let t = ScaffoldTemplate {
            rel_path: "README.md",
            body: "# {{project_name}}\n\nProject: {{project_slug}}\nClone: {{project_clone_url}}\n",
        };
        let rendered =
            render_with_project(&t, "Flux", "flux", "A test", "https://example.com/flux.git");
        assert!(rendered.contains("# Flux"));
        assert!(rendered.contains("Project: flux"));
        assert!(rendered.contains("Clone: https://example.com/flux.git"));
    }

    #[test]
    fn readme_template_references_project_name_token() {
        let templates = docs_starter_templates();
        let readme = templates
            .iter()
            .find(|t| t.rel_path == "README.md")
            .unwrap();
        assert!(
            readme.body.contains("{{project_name}}"),
            "README template should reference project_name"
        );
    }
}
