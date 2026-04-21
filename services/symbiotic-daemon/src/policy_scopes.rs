use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use symbiotic_control_plane::manifest::ManifestParser;
use symbiotic_control_plane::types::{GoalManifest, PolicyScopeManifest};

pub(crate) fn load_goal_policy_scopes(
    parser: &ManifestParser,
    archive_root: &Path,
    goal: &GoalManifest,
) -> Result<Vec<PolicyScopeManifest>> {
    let scope_index = load_policy_scope_index(parser, archive_root)?;
    let mut resolved = goal
        .policy_scopes
        .iter()
        .enumerate()
        .filter_map(|(declared_index, scope_id)| {
            scope_index
                .get(scope_id)
                .cloned()
                .filter(|scope| scope.enabled)
                .map(|scope| (declared_index, scope))
        })
        .collect::<Vec<_>>();
    resolved.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then(left.1.priority.cmp(&right.1.priority))
            .then(left.1.id.cmp(&right.1.id))
    });
    Ok(resolved.into_iter().map(|(_, scope)| scope).collect())
}

pub(crate) fn load_policy_scope_by_id(
    parser: &ManifestParser,
    archive_root: &Path,
    scope_id: &str,
) -> Result<Option<PolicyScopeManifest>> {
    Ok(load_policy_scope_index(parser, archive_root)?
        .remove(scope_id)
        .filter(|scope| scope.enabled))
}

fn load_policy_scope_index(
    parser: &ManifestParser,
    archive_root: &Path,
) -> Result<HashMap<String, PolicyScopeManifest>> {
    let Some(scopes_dir) = parser.resolve_policy_scopes_dir(archive_root) else {
        return Ok(HashMap::new());
    };

    let mut scopes = HashMap::new();
    for entry in std::fs::read_dir(scopes_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        match parser.parse_policy_scope(&path) {
            Ok(scope) => {
                scopes.insert(scope.id.clone(), scope);
            }
            Err(error) => tracing::warn!(
                path = %path.display(),
                %error,
                "policy_scopes: failed to parse policy scope"
            ),
        }
    }
    Ok(scopes)
}
