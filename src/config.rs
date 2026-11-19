//! Configuration models for gh-config-cli.
//!
//! This module defines the data structures used for representing repository, team, user, and webhook
//! configuration. All structs are serializable/deserializable for use with YAML and JSON configuration files.

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use serde_yaml::Value;
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
///
/// Configuration for a GitHub webhook.
///
/// Represents the configuration for a GitHub webhook, including the endpoint URL,
/// content type, and the list of events that trigger the webhook.
///
pub struct WebhookConfig {
    /// The webhook endpoint URL.
    pub url: String,
    /// The content type for webhook payloads (e.g., "json").
    pub content_type: String,
    /// List of events that trigger the webhook.
    pub events: Vec<String>,
}

// Extensible settings: arbitrary key-value pairs for repo settings
/// Arbitrary key-value pairs for repository settings (extensible).
pub type RepoSettings = BTreeMap<String, Value>;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
///
/// Branch protection rule for a repository.
///
/// Represents a single branch protection rule, including the branch pattern and
/// enforcement options for admins, deletions, and force pushes.
///
pub struct BranchProtectionRule {
    /// Branch name or glob pattern to match.
    pub pattern: String,
    /// Whether to enforce admin restrictions.
    #[serde(default)]
    pub enforce_admins: bool,
    /// Whether to allow branch deletions.
    #[serde(default)]
    pub allow_deletions: bool,
    /// Whether to allow force pushes.
    #[serde(default)]
    pub allow_force_pushes: bool,
}

impl Default for BranchProtectionRule {
    fn default() -> Self {
        BranchProtectionRule {
            pattern: "main".to_string(),
            enforce_admins: true,
            allow_deletions: false,
            allow_force_pushes: false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
///
/// Repository configuration.
///
/// Represents the configuration for a single repository, including its name,
/// settings, visibility, webhook and branch protections.
///
pub struct Repo {
    /// Name of the repository.
    pub name: String,
    #[serde(default = "RepoSettings::new")]
    pub settings: RepoSettings, // Now extensible
    #[serde(default)]
    pub visibility: Option<String>,
    /// Whether the repo gets the config's `default_webhook`. Being listed alone connects nothing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub use_default_webhook: bool,
    #[serde(default)]
    pub webhook: Option<WebhookConfig>,
    #[serde(default)]
    pub branch_protections: Vec<BranchProtectionRule>,
    /// Status checks that must pass before a PR merges to the default branch, each pinned to the app that must
    /// post it or to none, held in the repo's `Require CI` ruleset. Empty: `sync` leaves the repo's rulesets alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_checks: Vec<RequiredCheck>,
    /// The teams that may merge past `required_checks`, and how. Nobody else can: `sync` makes the ruleset's
    /// bypass list exactly this, so leaving it out removes every bypass. Needs `required_checks`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_checks_bypass: Vec<CheckBypass>,
}

/// A required status check: its name, and the GitHub App that must post it, if pinned (GitHub's `integration_id`).
/// Written as the name alone (any app may post it) or as `{context: <name>, app_id: <id>}`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RequiredCheck {
    pub context: String,
    pub app_id: Option<u64>,
}

/// The pinned form of a `RequiredCheck`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinnedCheck {
    context: String,
    app_id: u64,
}

impl Serialize for RequiredCheck {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.app_id {
            None => serializer.serialize_str(&self.context),
            Some(app_id) => PinnedCheck { context: self.context.clone(), app_id }.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for RequiredCheck {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Check;
        impl<'de> de::Visitor<'de> for Check {
            type Value = RequiredCheck;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a check name, or `{context: <name>, app_id: <id>}`")
            }
            fn visit_str<E: de::Error>(self, name: &str) -> Result<RequiredCheck, E> {
                Ok(RequiredCheck { context: name.to_string(), app_id: None })
            }
            fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<RequiredCheck, A::Error> {
                let pinned = PinnedCheck::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(RequiredCheck { context: pinned.context, app_id: Some(pinned.app_id) })
            }
        }
        deserializer.deserialize_any(Check)
    }
}

impl std::fmt::Display for RequiredCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self.app_id {
            None => f.write_str(&self.context),
            Some(app_id) => write!(f, "{} (app {app_id})", self.context),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
/// A team that may bypass a repo's `required_checks`.
pub struct CheckBypass {
    /// The team's slug, as in `teams`.
    pub team: String,
    pub mode: BypassMode,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
/// GitHub's ruleset `bypass_mode`.
pub enum BypassMode {
    /// Only by merging a PR; a direct push still has to pass the checks.
    PullRequest,
    /// By merging a PR or by pushing directly.
    Always,
}

impl BypassMode {
    pub fn as_str(self) -> &'static str {
        match self {
            BypassMode::PullRequest => "pull_request",
            BypassMode::Always => "always",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
/// The org's Actions workflow settings (GitHub's `/orgs/{org}/actions/permissions/workflow`).
pub struct OrgActions {
    /// What the `GITHUB_TOKEN` of a workflow run may do by default.
    pub default_workflow_permissions: WorkflowPermissions,
    /// Whether Actions may create and approve pull requests.
    pub can_approve_pull_request_reviews: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// GitHub's `default_workflow_permissions`.
pub enum WorkflowPermissions {
    /// Read the repo's contents and packages.
    Read,
    /// Read and write every permission.
    Write,
}

impl WorkflowPermissions {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkflowPermissions::Read => "read",
            WorkflowPermissions::Write => "write",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)] // Added Clone
#[serde(deny_unknown_fields)]
///
/// Represents a team within the organization.
///
/// Each team has a name and a list of member usernames.
///
pub struct Team {
    /// Name of the team.
    pub name: String,
    /// List of usernames belonging to the team.
    pub members: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)] // Added Clone
#[serde(deny_unknown_fields)]
///
/// Represents a user within the organization.
///
/// Each user has a login (username) and a role (e.g., admin, member).
///
pub struct User {
    /// The user's GitHub login/username.
    pub login: String,
    /// The user's role within the organization.
    pub role: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)] // Added Clone
#[serde(deny_unknown_fields)]
///
/// Represents a team assignment to a repository with a specific permission level.
///
/// Each assignment links a team to a repository and specifies the permission granted.
///
pub struct Assignment {
    /// The name of the repository.
    pub repo: String,
    /// The name of the team.
    pub team: String,
    /// The permission level (e.g., admin, write, read).
    pub permission: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
///
/// Top-level configuration for gh-config-cli.
///
/// This struct represents the full configuration for an organization, including
/// repositories, teams, users, assignments, default webhook, and branch protections.
///
pub struct Config {
    /// The name of the GitHub organization.
    pub org: String,
    /// The org's Actions workflow settings. Absent: `sync` leaves them alone.
    #[serde(default)]
    pub actions: Option<OrgActions>,
    /// List of repository configurations.
    #[serde(default)]
    pub repos: Vec<Repo>,
    /// List of team configurations.
    #[serde(default)]
    pub teams: Vec<Team>,
    /// List of user configurations.
    #[serde(default)]
    pub users: Vec<User>,
    /// List of team-to-repo assignments.
    #[serde(default)]
    pub assignments: Vec<Assignment>,
    /// The webhook of every repo that sets `use_default_webhook`.
    #[serde(default)]
    pub default_webhook: Option<WebhookConfig>,
    /// Default branch protection rules for all repositories (if not overridden).
    #[serde(default)]
    pub default_branch_protections: Vec<BranchProtectionRule>,
}

impl Config {
    /// Loads config from main file, optionally merging with defaults.config.yaml if present.
    pub fn from_file_with_defaults(main_path: &str, defaults_path: Option<&str>) -> crate::error::AppResult<Self> {
        let main_file = std::fs::File::open(main_path).map_err(crate::error::AppError::Io)?;
        let mut main_config: Value = serde_yaml::from_reader(main_file).map_err(crate::error::AppError::Serialization)?;

        if let Some(defaults_path) = defaults_path {
            if let Ok(defaults_file) = std::fs::File::open(defaults_path) {
                let defaults_config: Value = serde_yaml::from_reader(defaults_file).map_err(crate::error::AppError::Serialization)?;
                main_config = merge_with_defaults(main_config, defaults_config);
            }
        }

        Self::from_value(main_config, main_path)
    }

    /// Reads a config from YAML text; `main_path` names it in errors. Checked like a config file.
    pub fn from_yaml_str(text: &str, main_path: &str) -> crate::error::AppResult<Self> {
        Self::from_value(serde_yaml::from_str(text).map_err(crate::error::AppError::Serialization)?, main_path)
    }

    fn from_value(main_config: Value, main_path: &str) -> crate::error::AppResult<Self> {
        // Unknown or misplaced keys are refused, naming the file, where the key is and the keys allowed there.
        let config: Config = serde_path_to_error::deserialize(main_config)
            .map_err(|e| crate::error::AppError::Config(format!("{main_path}: {}: {}", e.path(), e.inner())))?;
        let allowed = crate::api_mapping::repo_settings();
        let all = crate::github_api_mapping_generated::get_github_api_mapping();
        for (i, repo) in config.repos.iter().enumerate() {
            let checks = &repo.required_checks;
            if let Some(dup) = checks.iter().enumerate().find_map(|(j, c)| checks.iter().take(j).any(|x| x.context == c.context).then_some(&c.context)) {
                return Err(crate::error::AppError::Config(format!(
                    "{main_path}: repos[{i}].required_checks (repo {}): `{dup}` is listed twice.",
                    repo.name
                )));
            }
            let bypass = &repo.required_checks_bypass;
            let bypass_problem = if !bypass.is_empty() && checks.is_empty() {
                Some("a bypass list needs `required_checks`: without them `sync` leaves the repo's rulesets alone.".to_string())
            } else if let Some(dup) = bypass.iter().enumerate().find_map(|(j, b)| bypass.iter().take(j).any(|x| x.team == b.team).then_some(&b.team)) {
                Some(format!("team `{dup}` is listed twice."))
            } else {
                let unknown = bypass.iter().find(|b| !config.teams.is_empty() && !config.teams.iter().any(|t| t.name == b.team));
                unknown.map(|b| format!("team `{}` isn't one of the config's `teams`.", b.team))
            };
            if let Some(problem) = bypass_problem {
                return Err(crate::error::AppError::Config(format!(
                    "{main_path}: repos[{i}].required_checks_bypass (repo {}): {problem}",
                    repo.name
                )));
            }
            if repo.use_default_webhook && (repo.webhook.is_some() || config.default_webhook.is_none()) {
                return Err(crate::error::AppError::Config(format!(
                    "{main_path}: repos[{i}] (repo {}): `use_default_webhook` needs a top-level `default_webhook` and no `webhook` of its own.",
                    repo.name
                )));
            }
            let Some(key) = repo.settings.keys().find(|k| !allowed.contains_key(k.as_str())) else { continue };
            let problem = match all.get(key.as_str()) {
                Some(m) => format!("`{key}` is not a repository setting: it would be sent to {} {}, which is not the repo.", m.method, m.endpoint),
                None => {
                    let mut close: Vec<_> = allowed.keys().filter(|k| !key.is_empty() && (k.starts_with(key.as_str()) || key.starts_with(**k))).collect();
                    close.sort();
                    let hint: Vec<_> = close.iter().take(5).map(|k| format!("`{k}`")).collect();
                    let hint = if hint.is_empty() { String::new() } else { format!(" Did you mean {}?", hint.join(" or ")) };
                    format!("unknown setting `{key}`.{hint}")
                }
            };
            return Err(crate::error::AppError::Config(format!(
                "{main_path}: repos[{i}].settings (repo {}): {problem} A repo's settings are fields of GitHub's repository endpoints, such as `allow_merge_commit` and `archived`.",
                repo.name
            )));
        }
        Ok(config)
    }

    /// Gives each repo the webhook it ends up with, events sorted, and drops the default: nothing after this needs it.
    pub fn resolve_webhooks(&mut self) {
        let default = self.default_webhook.take();
        for repo in &mut self.repos {
            if std::mem::take(&mut repo.use_default_webhook) {
                repo.webhook = default.clone();
            }
            if let Some(webhook) = repo.webhook.as_mut() {
                webhook.events.sort();
            }
        }
    }
}

/// Recursively merges defaults into main config (main config takes precedence).
fn merge_with_defaults(main: Value, defaults: Value) -> Value {
    match (main, defaults) {
        (Value::Mapping(mut main_map), Value::Mapping(defaults_map)) => {
            for (k, v_default) in defaults_map {
                if let Some(v_main) = main_map.remove(&k) {
                    // Special case: if v_main is an empty sequence and v_default is a sequence,
                    // use v_default directly
                    let merged_value = match (v_main, &v_default) {
                        (Value::Sequence(seq), Value::Sequence(default_seq)) if seq.is_empty() && !default_seq.is_empty() => {
                            v_default.clone()
                        }
                        (v_main, _) => {
                            merge_with_defaults(v_main, v_default.clone())
                        }
                    };
                    main_map.insert(k, merged_value);
                } else {
                    main_map.insert(k, v_default);
                }
            }
            Value::Mapping(main_map)
        }
        (Value::Sequence(main_seq), Value::Sequence(defaults_seq)) => {
            // If main_seq is empty, use defaults_seq directly
            if main_seq.is_empty() {
                return Value::Sequence(defaults_seq);
            }
            // Merge lists of mappings by "name" key (for repos, teams, etc.)
            let mut merged = vec![];
            let mut used = vec![false; main_seq.len()];
            for d in &defaults_seq {
                if let Value::Mapping(d_map) = d {
                    if let Some(Value::String(d_name)) = d_map.get(&Value::String("name".to_string())) {
                        // Try to find a matching item in main_seq
                        let mut found = false;
                        for (i, m) in main_seq.iter().enumerate() {
                            if let Value::Mapping(m_map) = m {
                                if let Some(Value::String(m_name)) = m_map.get(&Value::String("name".to_string())) {
                                    if m_name == d_name {
                                        // Merge recursively
                                        merged.push(merge_with_defaults(m.clone(), d.clone()));
                                        used[i] = true;
                                        found = true;
                                        break;
                                    }
                                }
                            }
                        }
                        if !found {
                            merged.push(d.clone());
                        }
                    } else {
                        merged.push(d.clone());
                    }
                } else {
                    merged.push(d.clone());
                }
            }
            // Add all main_seq items that were not matched
            for (i, m) in main_seq.into_iter().enumerate() {
                if !used[i] {
                    merged.push(m);
                }
            }
            Value::Sequence(merged)
        }
        (main, _) => main, // If not both mappings or sequences, main wins
    }
}

#[cfg(test)]
mod branch_protection_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_merge_with_defaults() {
        let defaults_yaml = r#"
org: test-org
repos:
  - name: repo1
    settings:
      allow_merge_commit: false
      allow_squash_merge: true
      allow_auto_merge: true
    visibility: public
"#;
        let main_yaml = r#"
org: test-org
repos:
  - name: repo1
    settings:
      allow_merge_commit: true
      allow_rebase_merge: true
    visibility: private
"#;
        let mut defaults_file = tempfile::NamedTempFile::new().expect("create temp file");
        write!(defaults_file, "{}", defaults_yaml).expect("write defaults yaml");
        let mut main_file = tempfile::NamedTempFile::new().expect("create temp file");
        write!(main_file, "{}", main_yaml).expect("write main yaml");

        let config = crate::config::Config::from_file_with_defaults(
            main_file.path().to_str().unwrap(),
            Some(defaults_file.path().to_str().unwrap())
        ).expect("parse merged config");

        // Main config takes precedence
        println!("config.repos.len() = {}", config.repos.len());
        let repo = &config.repos[0];
        assert_eq!(repo.settings.get("allow_merge_commit").unwrap().as_bool().unwrap(), true);
        assert_eq!(repo.settings.get("allow_squash_merge").unwrap().as_bool().unwrap(), true);
        assert_eq!(repo.settings.get("allow_rebase_merge").unwrap().as_bool().unwrap(), true);
        // A default is filled in
        assert!(repo.settings.get("allow_auto_merge").unwrap().as_bool().unwrap());
        // Main config wins
        assert_eq!(repo.visibility.as_deref(), Some("private"));
    }

    #[test]
    fn test_branch_protection_rule_deserialization() {
        let yaml = r#"
pattern: main
enforce_admins: true
allow_deletions: false
allow_force_pushes: false
"#;

        let rule: BranchProtectionRule = serde_yaml::from_str(yaml).expect("deserialize");
        assert_eq!(rule.pattern, "main");
        assert!(rule.enforce_admins);
        assert!(!rule.allow_deletions);
        assert!(!rule.allow_force_pushes);
    }

    #[test]
    fn a_check_listed_twice_is_refused() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, b"org: o\nrepos:\n- name: r\n  required_checks: [A, B, A]\n").unwrap();
        let err = Config::from_file_with_defaults(file.path().to_str().unwrap(), None).unwrap_err().to_string();
        assert!(err.contains("required_checks") && err.contains("`A` is listed twice"), "{err}");
    }

    #[test]
    fn a_check_is_a_name_or_a_name_pinned_to_an_app() {
        let load = |checks: &str| {
            let mut file = tempfile::NamedTempFile::new().unwrap();
            std::io::Write::write_all(&mut file, format!("org: o\nrepos:\n- name: r\n  required_checks: {checks}\n").as_bytes()).unwrap();
            Config::from_file_with_defaults(file.path().to_str().unwrap(), None).map_err(|e| e.to_string())
        };
        let ok = load(r#"[A, {context: "gate / CI gate", app_id: 15368}]"#).unwrap();
        assert_eq!(
            ok.repos[0].required_checks,
            [RequiredCheck { context: "A".into(), app_id: None }, RequiredCheck { context: "gate / CI gate".into(), app_id: Some(15368) }]
        );
        for (checks, expected) in [
            ("[{context: A, appid: 1}]", "unknown field `appid`"),
            ("[{context: A}]", "missing field `app_id`"),
            ("[{context: A, app_id: actions}]", "required_checks[0].app_id"),
            ("[A, {context: A, app_id: 1}]", "`A` is listed twice"),
        ] {
            let err = load(checks).unwrap_err();
            assert!(err.contains("required_checks") && err.contains(expected), "{checks}: {err}");
        }
    }

    #[test]
    fn org_actions_take_both_settings_and_nothing_else() {
        let load = |actions: &str| {
            let mut file = tempfile::NamedTempFile::new().unwrap();
            std::io::Write::write_all(&mut file, format!("org: o\nactions: {actions}\n").as_bytes()).unwrap();
            Config::from_file_with_defaults(file.path().to_str().unwrap(), None).map_err(|e| e.to_string())
        };
        let ok = load("{default_workflow_permissions: read, can_approve_pull_request_reviews: false}").unwrap();
        assert_eq!(ok.actions, Some(OrgActions { default_workflow_permissions: WorkflowPermissions::Read, can_approve_pull_request_reviews: false }));
        for (actions, expected) in [
            ("{default_workflow_permissions: admin, can_approve_pull_request_reviews: false}", "unknown variant `admin`"),
            ("{default_workflow_permissions: read, can_approve_pull_request_reviews: false, can_approve: true}", "unknown field `can_approve`"),
            ("{default_workflow_permissions: read}", "missing field `can_approve_pull_request_reviews`"),
        ] {
            let err = load(actions).unwrap_err();
            assert!(err.contains("actions") && err.contains(expected), "{actions}: {err}");
        }
    }

    #[test]
    fn a_bypass_list_names_known_teams_once_with_a_known_mode_and_needs_checks() {
        let load = |text: &str| {
            let mut file = tempfile::NamedTempFile::new().unwrap();
            std::io::Write::write_all(&mut file, text.as_bytes()).unwrap();
            Config::from_file_with_defaults(file.path().to_str().unwrap(), None).map_err(|e| e.to_string())
        };
        let teams = "teams:\n- name: core\n  members: []\n";
        let ok = load(&format!("org: o\nrepos:\n- name: r\n  required_checks: [A]\n  required_checks_bypass:\n  - team: core\n    mode: pull_request\n{teams}")).unwrap();
        assert_eq!(ok.repos[0].required_checks_bypass, [CheckBypass { team: "core".into(), mode: BypassMode::PullRequest }]);
        for (bypass, expected) in [
            ("[{team: core, mode: sometimes}]", "unknown variant `sometimes`"),
            ("[{team: core, mode: always, why: x}]", "unknown field `why`"),
            ("[{team: core, mode: always}, {team: core, mode: pull_request}]", "team `core` is listed twice"),
            ("[{team: ghosts, mode: always}]", "team `ghosts` isn't one of the config's `teams`"),
        ] {
            let err = load(&format!("org: o\nrepos:\n- name: r\n  required_checks: [A]\n  required_checks_bypass: {bypass}\n{teams}")).unwrap_err();
            assert!(err.contains("required_checks_bypass") && err.contains(expected), "{bypass}: {err}");
        }
        let err = load(&format!("org: o\nrepos:\n- name: r\n  required_checks_bypass: [{{team: core, mode: always}}]\n{teams}")).unwrap_err();
        assert!(err.contains("needs `required_checks`"), "{err}");
    }

    #[test]
    fn test_config_with_branch_protections() {
        let yaml = r#"
org: test-org
default_branch_protections:
  - pattern: main
    enforce_admins: true
    allow_deletions: false
    allow_force_pushes: false
repos:
  - name: repo1
    branch_protections:
      - pattern: release/*
        enforce_admins: true
        allow_deletions: false
        allow_force_pushes: false
    settings:
      allow_merge_commit: true
      allow_squash_merge: true
      allow_rebase_merge: true
teams: []
users: []
assignments: []
"#;

        let mut tmpfile = tempfile::NamedTempFile::new().expect("create temp file");
        write!(tmpfile, "{}", yaml).expect("write yaml");

        let config = crate::config::Config::from_file_with_defaults(tmpfile.path().to_str().unwrap(), None).expect("parse config");
        assert_eq!(config.org, "test-org");
        assert_eq!(config.default_branch_protections.len(), 1);
        assert_eq!(config.repos.len(), 1);
        let repo = &config.repos[0];
        assert_eq!(repo.branch_protections.len(), 1);
        assert_eq!(repo.branch_protections[0].pattern, "release/*");
    }
}


#[cfg(test)]
mod tests {
    use std::io::Write;

    #[test]
    fn unknown_or_misplaced_keys_are_refused_naming_file_place_key_and_allowed_keys() {
        for (yaml, place, key, allowed) in [
            // A typo under settings.
            ("org: o\nrepos:\n- name: r\n  settings:\n    archive: true\n", "repos[0].settings", "`archive`", "`archived`"),
            // `archived` belongs under settings, not on the repo.
            ("org: o\nrepos:\n- name: r\n  archived: true\n", "repos[0].archived", "`archived`", "`settings`"),
            ("org: o\nteam: []\n", "team", "`team`", "`teams`"),
            ("org: o\nusers:\n- login: a\n  role: admin\n  rol: member\n", "users[0].rol", "`rol`", "`role`"),
        ] {
            let mut file = tempfile::NamedTempFile::new().expect("create temp file");
            write!(file, "{}", yaml).expect("write yaml");
            let path = file.path().to_str().expect("utf-8 path");
            let err = crate::config::Config::from_file_with_defaults(path, None)
                .expect_err(&format!("accepted {key} in:\n{yaml}"))
                .to_string();
            for part in [path, place, key, allowed] {
                assert!(err.contains(part), "error for {key} doesn't name {part}: {err}");
            }
        }
    }

    #[test]
    fn test_from_file_deserializes_valid_yaml() {
        // Sample YAML content matching Config structure
        let yaml = r#"
org: test-org
repos:
  - name: repo1
    settings:
      allow_merge_commit: true
      allow_squash_merge: false
      allow_rebase_merge: true
    visibility: public
    webhook:
      url: "http://example.com"
      content_type: "json"
      events: ["push", "pull_request"]
teams:
  - name: team1
    members: ["alice", "bob"]
users:
  - login: alice
    role: admin
  - login: bob
    role: member
assignments:
  - repo: repo1
    team: team1
    permission: admin
default_webhook:
  url: "http://default.com"
  content_type: "json"
  events: ["push"]
"#;

        // Create a temporary file with the YAML content
        let mut tmpfile = tempfile::NamedTempFile::new().expect("create temp file");
        write!(tmpfile, "{}", yaml).expect("write yaml to temp file");

        // Call from_file
        let config = crate::config::Config::from_file_with_defaults(tmpfile.path().to_str().unwrap(), None).expect("parse config");

        // Basic assertions
        assert_eq!(config.org, "test-org");
        assert_eq!(config.repos.len(), 1);
        assert_eq!(config.teams.len(), 1);
        assert_eq!(config.users.len(), 2);
        assert_eq!(config.assignments.len(), 1);
        assert!(config.default_webhook.is_some());
    }
}