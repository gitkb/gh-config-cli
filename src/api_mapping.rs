/// Describes how to map a config field to a GitHub API call.
#[derive(Debug, Clone)]
pub struct ApiFieldMapping {
    #[allow(dead_code)]
    pub resource_type: &'static str, // e.g., "repo", "org", "team"
    #[allow(dead_code)]
    pub config_key: &'static str,    // e.g., "allow_merge_commit"
    pub endpoint: &'static str,      // e.g., "/repos/{org}/{repo}"
    pub method: &'static str,        // "PATCH", "PUT", etc.
    pub json_path: &'static str,     // e.g., "allow_merge_commit"
    // Add more fields as needed for transformation, etc.
}
