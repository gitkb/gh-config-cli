use std::collections::HashMap;

/// Describes how to map a config field to a GitHub API call.
#[derive(Debug, Clone)]
pub struct ApiFieldMapping {
    pub resource_type: &'static str, // e.g., "repo", "org", "team"
    pub config_key: &'static str,    // e.g., "allow_merge_commit"
    pub endpoint: &'static str,      // e.g., "/repos/{org}/{repo}"
    pub method: &'static str,        // "PATCH", "PUT", etc.
    pub json_path: &'static str,     // e.g., "allow_merge_commit"
    // Add more fields as needed for transformation, etc.
}

/// The mappings a repo's `settings:` may use: those that write the repo itself or something under it,
/// with no placeholder left once `{owner}` and `{repo}` are filled in. Any other key would write the
/// org, a user, or a URL that still holds a placeholder.
pub fn repo_settings() -> HashMap<&'static str, ApiFieldMapping> {
    crate::github_api_mapping_generated::get_github_api_mapping()
        .into_iter()
        .filter(|(_, m)| {
            m.endpoint
                .strip_prefix("/repos/{owner}/{repo}")
                .is_some_and(|rest| (rest.is_empty() || rest.starts_with('/')) && !rest.contains('{'))
        })
        .collect()
}
