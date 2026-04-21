use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use symbiotic_control_plane::manifest::ManifestParser;
use symbiotic_control_plane::types::{AvailabilityRuleManifest, PolicyScopeManifest};

pub(crate) fn load_availability_rule_by_subject(
    parser: &ManifestParser,
    archive_root: &Path,
    subject: &str,
) -> Result<Option<AvailabilityRuleManifest>> {
    Ok(load_availability_index(parser, archive_root)?
        .remove(subject)
        .filter(|rule| rule.enabled))
}

pub(crate) fn resolve_delivery_subject(
    parser: &ManifestParser,
    archive_root: &Path,
    audience: Option<&str>,
    policy_scope: Option<&PolicyScopeManifest>,
) -> Result<String> {
    if let Some(scope) = policy_scope {
        return Ok(scope
            .delivery_subject
            .clone()
            .unwrap_or_else(|| scope.id.clone()));
    }

    let subject = audience.unwrap_or("operator");
    if load_availability_rule_by_subject(parser, archive_root, subject)?.is_some() {
        return Ok(subject.to_string());
    }
    Ok("operator".to_string())
}

fn load_availability_index(
    parser: &ManifestParser,
    archive_root: &Path,
) -> Result<HashMap<String, AvailabilityRuleManifest>> {
    let Some(availability_dir) = parser.resolve_calendar_availability_dir(archive_root) else {
        return Ok(HashMap::new());
    };

    let mut rules = HashMap::new();
    for entry in std::fs::read_dir(availability_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        match parser.parse_availability_rule(&path) {
            Ok(rule) => {
                rules.insert(rule.subject.clone(), rule);
            }
            Err(error) => tracing::warn!(
                path = %path.display(),
                %error,
                "availability: failed to parse availability rule"
            ),
        }
    }
    Ok(rules)
}
