/*!
    GitHub API client module.

    This module provides the `GitHubClient` struct and related types for interacting with the GitHub REST API.
    It supports operations such as updating repository settings, managing teams, users, and webhooks, and
    generating configuration from a GitHub organization. All API interactions are authenticated and support
    dry-run mode for previewing changes.

    Public APIs are documented for maintainability. Internal response structs are used for deserialization.
*/

use crate::config::{Assignment, BypassMode, CheckBypass, Repo, RepoSettings, Team, User, WebhookConfig, Config};
use crate::error::{AppError, AppResult};
use colored::*;
use log::{debug, info, error};
use crate::github_http::GitHubHttp;
use reqwest::Method;
use serde_json::json;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::HashSet;
use similar::{ChangeTag, TextDiff};
use std::fs::File;
use std::io::Write;

const GITHUB_API_BASE_URL: &str = "https://api.github.com";
/// The repo setting that archives (true) or unarchives (false) a repo.
const ARCHIVED: &str = "archived";
/// The repo setting that lets PRs merge themselves once their requirements are met.
const ALLOW_AUTO_MERGE: &str = "allow_auto_merge";
/// Who outside a private repo may use its Actions workflows: `none`, `user` or `organization`.
const ACCESS_LEVEL: &str = "access_level";
/// The repo ruleset `required_checks` is kept in. gh-config owns every rule in it.
const REQUIRE_CI: &str = "Require CI";
const REQUIRED_STATUS_CHECKS: &str = "required_status_checks";
/// The repo ruleset `required_approvals` is kept in. gh-config owns every rule in it.
const REQUIRE_REVIEW: &str = "Require review";
const PULL_REQUEST: &str = "pull_request";

/// A repo's `Require review` ruleset as GitHub holds it.
struct RequireReview {
    id: u64,
    approvals: u32,
    /// Whether everything but the count is what `sync` writes.
    standard: bool,
}

/// The `Require review` ruleset `sync` writes: PRs to the default branch need `approvals` approving reviews. The other
/// review settings are off here; a stricter one in another ruleset (the org's) still applies, GitHub takes the strictest.
fn require_review_body(approvals: u32) -> serde_json::Value {
    json!({
        "name": REQUIRE_REVIEW,
        "target": "branch",
        "enforcement": "active",
        "bypass_actors": [],
        "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
        "rules": [{
            "type": PULL_REQUEST,
            "parameters": {
                "required_approving_review_count": approvals,
                "dismiss_stale_reviews_on_push": false,
                "require_code_owner_review": false,
                "require_last_push_approval": false,
                "required_review_thread_resolution": false,
            },
        }],
    })
}

/// Reads a `Require review` ruleset. Any rule but one pull request rule is refused: `sync` would drop it.
fn parse_require_review(repo: &str, ruleset: &serde_json::Value) -> AppResult<RequireReview> {
    let id = ruleset["id"].as_u64().ok_or_else(|| AppError::GitHubApi(format!("Repo {repo}: the `{REQUIRE_REVIEW}` ruleset has no id")))?;
    let rules = ruleset["rules"].as_array().map(Vec::as_slice).unwrap_or_default();
    let [rule] = rules else {
        return Err(AppError::GitHubApi(format!(
            "Repo {repo}: its `{REQUIRE_REVIEW}` ruleset holds {} rules; gh-config owns it and writes one pull_request rule: move the others to another ruleset first.",
            rules.len()
        )));
    };
    if rule["type"] != PULL_REQUEST {
        return Err(AppError::GitHubApi(format!(
            "Repo {repo}: its `{REQUIRE_REVIEW}` ruleset holds a `{}` rule; gh-config owns it and would drop the rule: move it to another ruleset first.",
            rule["type"].as_str().unwrap_or("?")
        )));
    }
    let p = &rule["parameters"];
    let approvals = p["required_approving_review_count"].as_u64().and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| AppError::GitHubApi(format!("Repo {repo}: its `{REQUIRE_REVIEW}` ruleset has no review count")))?;
    let ref_name = &ruleset["conditions"]["ref_name"];
    let standard = ruleset["target"] == "branch"
        && ruleset["enforcement"] == "active"
        && ruleset["bypass_actors"].as_array().is_some_and(Vec::is_empty)
        && ref_name["include"] == json!(["~DEFAULT_BRANCH"])
        && ref_name["exclude"].as_array().is_some_and(Vec::is_empty)
        && ["dismiss_stale_reviews_on_push", "require_code_owner_review", "require_last_push_approval", "required_review_thread_resolution"]
            .iter()
            .all(|k| p[*k] == false);
    Ok(RequireReview { id, approvals, standard })
}

/// Settings a generated config writes only when they aren't GitHub's default, with that default. `diff` gives
/// GitHub's side the default of each one a config names, so writing the default out is no difference.
fn omitted_defaults() -> [(&'static str, serde_yaml::Value); 3] {
    [
        (ARCHIVED, serde_yaml::Value::Bool(false)),
        (ALLOW_AUTO_MERGE, serde_yaml::Value::Bool(false)),
        (ACCESS_LEVEL, serde_yaml::Value::String("none".into())),
    ]
}

/// One required status check. `integration_id`, when GitHub has one, pins the check to the app that must post it.
#[derive(Debug, Clone, PartialEq)]
struct RequiredCheck {
    context: String,
    integration_id: Option<u64>,
}

/// One entry of a ruleset's bypass list, as GitHub holds it: a team, an app, a role, the org's admins...
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct BypassActor {
    actor_type: String,
    actor_id: Option<u64>,
    mode: String,
}

/// The org's teams, `(slug, id)`: a config names a team by slug, a ruleset by id.
type TeamIds = [(String, u64)];

impl BypassActor {
    fn team(id: u64, mode: BypassMode) -> Self {
        BypassActor { actor_type: "Team".into(), actor_id: Some(id), mode: mode.as_str().into() }
    }

    /// The config entry for this actor, if a config can name it: a team the org has, with a known mode.
    fn as_config(&self, teams: &TeamIds) -> Option<CheckBypass> {
        let team = teams.iter().find(|(_, id)| self.actor_type == "Team" && Some(*id) == self.actor_id)?.0.clone();
        let mode = serde_yaml::from_value(serde_yaml::Value::String(self.mode.clone())).ok()?;
        Some(CheckBypass { team, mode })
    }

    /// For people to read.
    fn describe(&self, teams: &TeamIds) -> String {
        match (self.as_config(teams), self.actor_id) {
            (Some(b), _) => format!("{} ({})", b.team, self.mode),
            (None, Some(id)) => format!("{} #{id} ({})", self.actor_type, self.mode),
            (None, None) => format!("{} ({})", self.actor_type, self.mode),
        }
    }
}

/// The bypass list a config's entries stand for, sorted. A team the org doesn't have is refused.
fn bypass_actors(org: &str, repo: &Repo, teams: &TeamIds) -> AppResult<Vec<BypassActor>> {
    let mut actors = repo
        .required_checks_bypass
        .iter()
        .map(|b| {
            let id = teams.iter().find(|(slug, _)| *slug == b.team).map(|(_, id)| *id).ok_or_else(|| {
                AppError::Config(format!(
                    "Repo {}: `required_checks_bypass` names team `{}`, which neither {org} nor the config's `teams` has.",
                    repo.name, b.team
                ))
            })?;
            Ok(BypassActor::team(id, b.mode))
        })
        .collect::<AppResult<Vec<_>>>()?;
    actors.sort();
    Ok(actors)
}

/// The bypass part of a dry run's line: who may bypass after the write, and who no longer may.
fn bypass_change(bypass: &[BypassActor], held: Option<&[BypassActor]>, exists: bool, teams: &TeamIds) -> String {
    let named = |actors: &[&BypassActor]| actors.iter().map(|a| a.describe(teams)).collect::<Vec<_>>().join(", ");
    let allowed = if bypass.is_empty() { "nobody".to_string() } else { named(&bypass.iter().collect::<Vec<_>>()) };
    let dropped = match held {
        Some(held) => {
            let gone: Vec<_> = held.iter().filter(|a| !bypass.contains(a)).collect();
            if gone.is_empty() { String::new() } else { format!("; no longer bypassing: {}", named(&gone)) }
        }
        None if exists => "; its current bypass list can't be read with this token".to_string(),
        None => String::new(),
    };
    format!("bypass: {allowed}{dropped}")
}

/// A repo's `Require CI` ruleset as GitHub holds it.
struct RequireCi {
    id: u64,
    checks: Vec<RequiredCheck>,
    /// Sorted. `None` when GitHub didn't say: it shows a ruleset's bypass list only to tokens that can administer the repo.
    bypass: Option<Vec<BypassActor>>,
    /// Whether everything but the checks is what `sync` writes: active, on the default branch, not strict.
    standard: bool,
}

impl RequireCi {
    fn contexts(&self) -> Vec<String> {
        self.checks.iter().map(|c| c.context.clone()).collect()
    }
}

/// The `Require CI` ruleset `sync` writes for `checks`, which only `bypass` may merge past.
fn require_ci_body(checks: &[RequiredCheck], bypass: &[BypassActor]) -> serde_json::Value {
    let bypass: Vec<_> = bypass
        .iter()
        .map(|a| match a.actor_id {
            Some(id) => json!({ "actor_id": id, "actor_type": a.actor_type, "bypass_mode": a.mode }),
            None => json!({ "actor_type": a.actor_type, "bypass_mode": a.mode }),
        })
        .collect();
    let checks: Vec<_> = checks
        .iter()
        .map(|c| match c.integration_id {
            Some(id) => json!({ "context": c.context, "integration_id": id }),
            None => json!({ "context": c.context }),
        })
        .collect();
    json!({
        "name": REQUIRE_CI,
        "target": "branch",
        "enforcement": "active",
        "bypass_actors": bypass,
        "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
        "rules": [{
            "type": REQUIRED_STATUS_CHECKS,
            "parameters": {
                "required_status_checks": checks,
                "strict_required_status_checks_policy": false,
                "do_not_enforce_on_create": false,
            },
        }],
    })
}

/// Reads a `Require CI` ruleset. A rule other than required status checks is refused: `sync` would drop it.
fn parse_require_ci(repo: &str, ruleset: &serde_json::Value) -> AppResult<RequireCi> {
    let id = ruleset["id"].as_u64().ok_or_else(|| AppError::GitHubApi(format!("Repo {repo}: the `{REQUIRE_CI}` ruleset has no id")))?;
    let rules = ruleset["rules"].as_array().map(Vec::as_slice).unwrap_or_default();
    if let Some(other) = rules.iter().map(|r| r["type"].as_str().unwrap_or("?")).find(|t| *t != REQUIRED_STATUS_CHECKS) {
        return Err(AppError::GitHubApi(format!(
            "Repo {repo}: its `{REQUIRE_CI}` ruleset also holds a `{other}` rule. gh-config owns that ruleset and would drop the rule: move the rule to another ruleset first."
        )));
    }
    let params = rules.first().map(|r| &r["parameters"]);
    let checks = params
        .and_then(|p| p["required_status_checks"].as_array())
        .map(|checks| {
            checks
                .iter()
                .filter_map(|c| {
                    let context = c["context"].as_str()?.to_string();
                    Some(RequiredCheck { context, integration_id: c["integration_id"].as_u64() })
                })
                .collect()
        })
        .unwrap_or_default();
    let checks: Vec<RequiredCheck> = checks;
    if let Some(dup) = checks.iter().enumerate().find_map(|(j, c)| checks.iter().take(j).any(|x| x.context == c.context).then_some(&c.context)) {
        return Err(AppError::GitHubApi(format!(
            "Repo {repo}: its `{REQUIRE_CI}` ruleset requires `{dup}` twice (from different apps). A config names a check once, so gh-config would drop one: change the ruleset first."
        )));
    }
    let ref_name = &ruleset["conditions"]["ref_name"];
    let standard = rules.len() == 1
        && ruleset["target"] == "branch"
        && ruleset["enforcement"] == "active"
        && ref_name["include"] == json!(["~DEFAULT_BRANCH"])
        && ref_name["exclude"].as_array().is_some_and(Vec::is_empty)
        && params.is_some_and(|p| p["strict_required_status_checks_policy"] == false && p["do_not_enforce_on_create"] == false);
    let bypass = ruleset["bypass_actors"].as_array().map(|actors| {
        let mut actors: Vec<_> = actors
            .iter()
            .map(|a| BypassActor {
                actor_type: a["actor_type"].as_str().unwrap_or("?").to_string(),
                actor_id: a["actor_id"].as_u64(),
                mode: a["bypass_mode"].as_str().unwrap_or("?").to_string(),
            })
            .collect();
        actors.sort();
        actors
    });
    Ok(RequireCi { id, checks, bypass, standard })
}

/// Whether two check lists hold the same names, in any order.
fn same_checks(a: &[String], b: &[String]) -> bool {
    let mut a = a.to_vec();
    let mut b = b.to_vec();
    a.sort();
    b.sort();
    a == b
}

#[derive(Debug, Deserialize)]
struct RulesetSummary {
    id: u64,
    name: String,
}

/// Whether `sync` makes a repo public (`Some(true)`) or private, from the only fields it writes for that: the
/// settings' `visibility` and `private`, PATCHed onto the repo. `None` when neither is given. Settings that
/// contradict each other, or aren't the type GitHub takes, are refused.
fn written_public(repo: &Repo) -> AppResult<Option<bool>> {
    let refuse = |problem: String| AppError::Config(format!("Repo {}: {problem}", repo.name));
    let by_visibility = match repo.settings.get("visibility") {
        None => None,
        Some(v) => Some(v.as_str().ok_or_else(|| refuse(format!("`settings.visibility` must be a string, got {v:?}")))? == "public"),
    };
    let by_private = match repo.settings.get("private") {
        None => None,
        Some(v) => Some(!v.as_bool().ok_or_else(|| refuse(format!("`settings.private` must be true or false, got {v:?}")))?),
    };
    match (by_visibility, by_private) {
        (Some(a), Some(b)) if a != b => Err(refuse("`settings.visibility` and `settings.private` contradict each other.".into())),
        (a, b) => Ok(a.or(b)),
    }
}

/// Whether a repo is public once `sync` is done with it: what its settings write, else what it is now. That is
/// GitHub's visibility (`current`) for a repo that exists, and the top-level `visibility` it is created with for
/// one that doesn't (top-level `visibility` is used only to create). `None` only for a missing repo with no
/// visibility, which `sync` refuses to create.
fn ends_public(repo: &Repo, current: Option<&RepoSettings>) -> AppResult<Option<bool>> {
    Ok(written_public(repo)?.or_else(|| match current {
        Some(current) => Some(current.get("visibility").and_then(|v| v.as_str()) == Some("public")),
        None => repo.visibility.as_deref().map(|v| v == "public"),
    }))
}

/// Whether an `access_level` asks for more than `none`, the only level a public repo can have.
fn grants_access(repo: &Repo) -> bool {
    repo.settings.get(ACCESS_LEVEL).is_some_and(|v| v.as_str() != Some("none"))
}

/// Refuses an `access_level` other than `none` on a repo that ends up public (`ends_public`): GitHub has no access
/// level for public repos.
fn check_access_level(repo: &Repo, current: Option<&RepoSettings>) -> AppResult<()> {
    if grants_access(repo) && ends_public(repo, current)? == Some(true) {
        return Err(AppError::Config(format!(
            "Repo {}: `{ACCESS_LEVEL}` other than `none` applies only to private repos, and it ends up public.",
            repo.name
        )));
    }
    Ok(())
}

/// What the config alone decides, for every repo before anything reads or writes GitHub (dry runs included):
/// visibility settings that contradict each other, and an access level on a repo its settings make public.
/// Cases that turn on GitHub's current visibility are refused when `sync` reaches the repo, before its first write.
fn check_access_levels(config: &Config) -> AppResult<()> {
    config.repos.iter().try_for_each(|repo| match written_public(repo)? {
        Some(true) if grants_access(repo) => check_access_level(repo, None),
        _ => Ok(()),
    })
}

/// Rejects a config whose `archived` values aren't booleans, before anything reads or writes GitHub.
fn check_archived(config: &Config) -> AppResult<()> {
    config.repos.iter().try_for_each(|r| archived(&r.settings, &r.name).map(|_| ()))
}

/// The `archived` setting as a bool, or `None` when the settings don't name it.
fn archived(settings: &RepoSettings, repo: &str) -> AppResult<Option<bool>> {
    settings
        .get(ARCHIVED)
        .map(|v| {
            v.as_bool().ok_or_else(|| {
                AppError::GitHubApi(format!("Repo {repo}: `{ARCHIVED}` must be true or false, got {v:?}"))
            })
        })
        .transpose()
}

#[derive(Debug, Deserialize)]
struct TeamResponse {
    name: String,
}

#[derive(Debug, Deserialize)]
struct MembershipResponse {
    role: String,
}

#[derive(Debug, Deserialize)]
struct TeamRepoResponse {
    name: String,
    permissions: PermissionDetails,
}

#[derive(Debug, Deserialize)]
struct PermissionDetails {
    pull: bool,
    push: bool,
    admin: bool,
    // Finer levels GitHub reports alongside the three above.
    #[serde(default)]
    triage: bool,
    #[serde(default)]
    maintain: bool,
}

impl PermissionDetails {
    /// The highest level granted, in the config's names (`pull`, `triage`, `push`, `maintain`, `admin`).
    /// `None` when no level is granted.
    fn level(&self) -> Option<&'static str> {
        [(self.admin, "admin"), (self.maintain, "maintain"), (self.push, "push"), (self.triage, "triage"), (self.pull, "pull")]
            .into_iter()
            .find_map(|(granted, name)| granted.then_some(name))
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct WebhookResponse {
    id: i64,
    url: String,
    config: WebhookConfigResponse,
    events: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct WebhookConfigResponse {
    url: String,
    content_type: String,
}

///
/// Client for interacting with the GitHub API for organization management.
///
/// The `GitHubClient` provides methods to manage repositories, teams, users, webhooks,
/// and configuration synchronization for a GitHub organization. It handles authentication,
/// API requests, and high-level orchestration of organization state.
///
pub struct GitHubClient {
    /// HTTP transport that paces writes and handles rate limits.
    client: GitHubHttp,
    /// Name of the GitHub organization to operate on.
    pub org: String,
    /// Base URL of the GitHub REST API (tests point it at a mock server).
    api_url: String,
}

impl GitHubClient {
    /// Create a new GitHubClient for the given organization and token.
    ///
    /// # Arguments
    /// * `token` - GitHub personal access token.
    /// * `org` - Name of the GitHub organization.
    pub fn new(token: &str, org: &str) -> Self {
        GitHubClient {
            client: GitHubHttp::new(token),
            org: org.to_string(),
            api_url: GITHUB_API_BASE_URL.to_string(),
        }
    }

    async fn send_patch(&self, url: &str, body: serde_json::Value) -> AppResult<()> {
        self.client.request(Method::PATCH, url, "application/vnd.github+json", Some(body)).await?;
        Ok(())
    }

    async fn send_post(&self, url: &str, body: serde_json::Value) -> AppResult<()> {
        self.client.request(Method::POST, url, "application/vnd.github+json", Some(body)).await?;
        Ok(())
    }

    async fn send_put(&self, url: &str, body: Option<serde_json::Value>) -> AppResult<()> {
        self.client.request(Method::PUT, url, "application/vnd.github+json", body).await?;
        Ok(())
    }

    async fn get(&self, url: &str) -> AppResult<reqwest::Response> {
        self.get_as(url, "application/vnd.github+json").await
    }

    /// A GET that asks for the `accept` media type.
    async fn get_as(&self, url: &str, accept: &str) -> AppResult<reqwest::Response> {
        self.client.request(Method::GET, url, accept, None).await
    }

    /// Every item of a GitHub list, following `Link: rel="next"` from page to page.
    async fn get_all<T: DeserializeOwned>(&self, url: &str) -> AppResult<Vec<T>> {
        let mut items = Vec::new();
        let mut next = Some(url.to_string());
        while let Some(url) = next {
            let response = self.get(&url).await?;
            next = match next_page(response.headers()) {
                None => None,
                Some(target) => {
                    // A relative target (RFC 8288) resolves against this page's URL.
                    let next = reqwest::Url::parse(&url)
                        .and_then(|base| base.join(&target))
                        .map_err(|e| AppError::GitHubApi(format!("Bad next page {target} of {url}: {e}")))?
                        .to_string();
                    // The token goes only to the API itself, never to another host a `Link` header names.
                    if !next.starts_with(&format!("{}/", self.api_url)) {
                        return Err(AppError::GitHubApi(format!("Refusing next page {next} of {url}: it is outside {}", self.api_url)));
                    }
                    Some(next)
                }
            };
            let text = response.text().await?;
            let page: Vec<T> = serde_json::from_str(&text)
                .map_err(|e| AppError::GitHubApi(format!("Failed to parse response from {}: {}", url, e)))?;
            items.extend(page);
        }
        Ok(items)
    }

    /// Fetch all settings for a repo as a HashMap<String, serde_yaml::Value>
    async fn get_repo_settings(&self, repo_name: &str) -> AppResult<RepoSettings> {
        let full_url = format!("{}/repos/{}/{}", self.api_url, self.org, repo_name);
        let response = self.get(&full_url).await?;
        let text = response.text().await?;
        if text.is_empty() {
            error!("Empty response body from GET {}", full_url);
            return Err(AppError::GitHubApi("Empty response body".to_string()));
        }
        debug!("Raw response for {}: {}", full_url, text);
        let repo_json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| AppError::GitHubApi(format!("Failed to parse response from {}: {}", full_url, e)))?;
        // Convert to serde_yaml::Value for consistency with config
        let repo_yaml: serde_yaml::Value = serde_yaml::to_value(repo_json)
            .map_err(|e| AppError::GitHubApi(format!("Failed to convert repo JSON to YAML: {}", e)))?;
        // Flatten to a HashMap<String, Value>
        let mut settings = RepoSettings::new();
        if let serde_yaml::Value::Mapping(map) = repo_yaml {
            for (k, v) in map {
                if let serde_yaml::Value::String(key) = k {
                    settings.insert(key, v);
                }
            }
        }
        Ok(settings)
    }

    /// A private repo's Actions `access_level`. Every failure, a 404 included, is a `GitHubApi` error, so a
    /// caller can't take it for the repo itself missing.
    async fn get_access_level(&self, repo_name: &str) -> AppResult<String> {
        let url = format!("{}/repos/{}/{}/actions/permissions/access", self.api_url, self.org, repo_name);
        let read = async {
            let text = self.get(&url).await?.text().await?;
            let access: serde_json::Value = serde_json::from_str(&text)?;
            access[ACCESS_LEVEL]
                .as_str()
                .map(String::from)
                .ok_or_else(|| AppError::GitHubApi(format!("no `{ACCESS_LEVEL}` in {access}")))
        };
        read.await.map_err(|e| AppError::GitHubApi(format!("Reading the Actions access level of {}/{} from {url}: {e}", self.org, repo_name)))
    }

    /// Creates a repo the config lists and GitHub doesn't have. Its `visibility` must be given: a guess could
    /// publish a private repo.
    async fn create_repo(&self, repo: &Repo, dry_run: bool) -> AppResult<()> {
        let visibility = repo.visibility.as_deref().ok_or_else(|| {
            AppError::Config(format!("Repo {} doesn't exist on GitHub. Give it a `visibility` (public or private) to create it.", repo.name))
        })?;
        if dry_run {
            println!("[Dry Run] Would create {} repo {}/{}, then apply its settings", visibility, self.org, repo.name);
            return Ok(());
        }
        let url = format!("{}/orgs/{}/repos", self.api_url, self.org);
        self.send_post(&url, json!({ "name": repo.name, "visibility": visibility })).await?;
        info!("Created {} repo {}/{}", visibility, self.org, repo.name);
        Ok(())
    }

    /// The repo's own rulesets, by name and id: one list request.
    async fn list_rulesets(&self, repo: &str) -> AppResult<Vec<RulesetSummary>> {
        self.get_all(&format!("{}/repos/{}/{}/rulesets?includes_parents=false&per_page=100", self.api_url, self.org, repo)).await
    }

    /// The repo's own ruleset named `name` among `rulesets`, read in full: one request when it exists.
    async fn read_ruleset(&self, repo: &str, rulesets: &[RulesetSummary], name: &str) -> AppResult<Option<serde_json::Value>> {
        let Some(found) = rulesets.iter().find(|r| r.name == name) else { return Ok(None) };
        let url = format!("{}/repos/{}/{}/rulesets/{}", self.api_url, self.org, repo, found.id);
        serde_json::from_str(&self.get(&url).await?.text().await?)
            .map(Some)
            .map_err(|e| AppError::GitHubApi(format!("Failed to parse response from {}: {}", url, e)))
    }

    /// The repo's own `Require CI` ruleset, if it has one: one list request, plus one for its rules when it exists.
    async fn read_require_ci(&self, repo: &str) -> AppResult<Option<RequireCi>> {
        let rulesets = self.list_rulesets(repo).await?;
        self.read_ruleset(repo, &rulesets, REQUIRE_CI).await?.map(|r| parse_require_ci(repo, &r)).transpose()
    }

    /// The repo's `Require CI` and `Require review` rulesets, raw, from one list request.
    async fn read_owned_rulesets(&self, repo: &str) -> AppResult<(Option<serde_json::Value>, Option<serde_json::Value>)> {
        let rulesets = self.list_rulesets(repo).await?;
        Ok((self.read_ruleset(repo, &rulesets, REQUIRE_CI).await?, self.read_ruleset(repo, &rulesets, REQUIRE_REVIEW).await?))
    }

    /// The repo's own `Require review` ruleset, if it has one.
    async fn read_require_review(&self, repo: &str) -> AppResult<Option<RequireReview>> {
        let rulesets = self.list_rulesets(repo).await?;
        self.read_ruleset(repo, &rulesets, REQUIRE_REVIEW).await?.map(|r| parse_require_review(repo, &r)).transpose()
    }

    /// Makes the repo's `Require review` ruleset require exactly `repo.required_approvals`. A repo without it is left alone.
    async fn sync_required_approvals(&self, repo: &Repo, dry_run: bool) -> AppResult<()> {
        let Some(approvals) = repo.required_approvals else { return Ok(()) };
        let current = self.read_require_review(&repo.name).await?;
        let held = current.as_ref().map(|c| c.approvals);
        let (method, url) = match current {
            Some(current) if current.standard && current.approvals == approvals => return Ok(()),
            Some(current) => (Method::PUT, format!("{}/repos/{}/{}/rulesets/{}", self.api_url, self.org, repo.name, current.id)),
            None => (Method::POST, format!("{}/repos/{}/{}/rulesets", self.api_url, self.org, repo.name)),
        };
        if dry_run {
            let verb = if method == Method::POST { "create" } else { "update" };
            let was = held.map(|n| format!(" (now {n})")).unwrap_or_default();
            println!("[Dry Run] Would {verb} ruleset `{REQUIRE_REVIEW}` on {}/{} requiring {approvals} approving review(s){was}", self.org, repo.name);
            return Ok(());
        }
        self.client.request(method, &url, "application/vnd.github+json", Some(require_review_body(approvals))).await?;
        info!("Ruleset `{REQUIRE_REVIEW}` on {}/{} now requires {approvals} approving review(s)", self.org, repo.name);
        Ok(())
    }

    /// The org's teams as `(slug, id)`.
    async fn team_ids(&self) -> AppResult<Vec<(String, u64)>> {
        let teams: Vec<serde_json::Value> = self.get_all(&format!("{}/orgs/{}/teams?per_page=100", self.api_url, self.org)).await?;
        teams
            .iter()
            .map(|t| match (t["slug"].as_str(), t["id"].as_u64()) {
                (Some(slug), Some(id)) => Ok((slug.to_string(), id)),
                _ => Err(AppError::GitHubApi("Missing team slug or id".to_string())),
            })
            .collect()
    }

    /// Makes the repo's `Require CI` ruleset require exactly `repo.required_checks`, with exactly
    /// `repo.required_checks_bypass` allowed past them. A repo with no checks is left alone.
    async fn sync_required_checks(&self, repo: &Repo, teams: &TeamIds, dry_run: bool) -> AppResult<()> {
        let checks = &repo.required_checks;
        if checks.is_empty() {
            return Ok(());
        }
        let bypass = bypass_actors(&self.org, repo, teams)?;
        let current = self.read_require_ci(&repo.name).await?;
        let held = current.as_ref().map(|c| c.checks.clone()).unwrap_or_default();
        // A check kept from the ruleset keeps the app it's pinned to; a new one has none.
        let desired: Vec<RequiredCheck> = checks
            .iter()
            .map(|name| held.iter().find(|c| &c.context == name).cloned().unwrap_or(RequiredCheck { context: name.clone(), integration_id: None }))
            .collect();
        let held_bypass = current.as_ref().and_then(|c| c.bypass.clone());
        let (method, url) = match current {
            Some(current) if current.standard && same_checks(&current.contexts(), checks) && current.bypass.as_ref() == Some(&bypass) => return Ok(()),
            Some(current) => (Method::PUT, format!("{}/repos/{}/{}/rulesets/{}", self.api_url, self.org, repo.name, current.id)),
            None => (Method::POST, format!("{}/repos/{}/{}/rulesets", self.api_url, self.org, repo.name)),
        };
        let verb = if method == Method::POST { "create" } else { "update" };
        if dry_run {
            let removed: Vec<_> = held.iter().map(|c| c.context.as_str()).filter(|c| !checks.iter().any(|n| n == c)).collect();
            let removed = if removed.is_empty() { String::new() } else { format!("; no longer requiring: {}", removed.join(", ")) };
            let bypass = bypass_change(&bypass, held_bypass.as_deref(), method == Method::PUT, teams);
            println!("[Dry Run] Would {verb} ruleset `{REQUIRE_CI}` on {}/{} requiring: {}{removed}; {bypass}", self.org, repo.name, checks.join(", "));
            return Ok(());
        }
        self.client.request(method, &url, "application/vnd.github+json", Some(require_ci_body(&desired, &bypass))).await?;
        info!("Ruleset `{REQUIRE_CI}` on {}/{} now requires: {}; bypass: {}", self.org, repo.name, checks.join(", "), bypass.len());
        Ok(())
    }

    async fn get_team(&self, team_name: &str) -> AppResult<Option<TeamResponse>> {
        let full_url = format!("{}/orgs/{}/teams/{}", self.api_url, self.org, team_name);
        match self.get(&full_url).await {
            Ok(response) => {
                let text = response.text().await?;
                if text.is_empty() {
                    error!("Empty response body from GET {}", full_url);
                    return Err(AppError::GitHubApi("Empty response body".to_string()));
                }
                let team: TeamResponse = serde_json::from_str(&text)
                    .map_err(|e| AppError::GitHubApi(format!("Failed to parse response from {}: {}", full_url, e)))?;
                if team.name == team_name {
                    Ok(Some(team))
                } else {
                    Ok(None)
                }
            }
            Err(AppError::GitHubResponse { status: reqwest::StatusCode::NOT_FOUND, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn get_user_membership(&self, login: &str) -> AppResult<Option<String>> {
        let full_url = format!("{}/orgs/{}/memberships/{}", self.api_url, self.org, login);
        match self.get(&full_url).await {
            Ok(response) => {
                let text = response.text().await?;
                if text.is_empty() {
                    error!("Empty response body from GET {}", full_url);
                    return Err(AppError::GitHubApi("Empty response body".to_string()));
                }
                let membership: MembershipResponse = serde_json::from_str(&text)
                    .map_err(|e| AppError::GitHubApi(format!("Failed to parse response from {}: {}", full_url, e)))?;
                Ok(Some(membership.role))
            }
            Err(AppError::GitHubResponse { status: reqwest::StatusCode::NOT_FOUND, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn get_team_repos(&self, team_name: &str) -> AppResult<Vec<TeamRepoResponse>> {
        self.get_all(&format!("{}/orgs/{}/teams/{}/repos?per_page=100", self.api_url, self.org, team_name)).await
    }

    ///
    /// Retrieve all webhooks configured for a given repository.
    ///
    /// # Arguments
    /// * `repo_name` - The name of the repository.
    ///
    /// # Returns
    /// * `Ok(Vec<WebhookResponse>)` containing the list of webhooks if successful.
    /// * `Err(AppError)` if the API call or parsing fails.
    ///
    pub async fn get_webhooks(&self, repo_name: &str) -> AppResult<Vec<WebhookResponse>> {
        self.get_all(&format!("{}/repos/{}/{}/hooks?per_page=100", self.api_url, self.org, repo_name)).await
    }

    async fn create_webhook(&self, repo_name: &str, webhook: &WebhookConfig) -> AppResult<()> {
        let url = format!("{}/repos/{}/{}/hooks", self.api_url, self.org, repo_name);
    
        let body = json!({
            "name": "web", // Standard name for webhooks
            "active": true,
            "events": webhook.events, // Use the events from the parameter
            "config": {
                "url": webhook.url, // Use the URL from the parameter
                "content_type": webhook.content_type, // Use the content_type from the parameter
                "insecure_ssl": "0" // Standard setting
            }
        });
    
        debug!("Webhook create payload: {}", serde_json::to_string(&body)?);
        info!("Creating webhook for {}/{}", self.org, repo_name);
        self.send_post(&url, body).await?;
        Ok(())
    }

    async fn update_webhook(&self, repo_name: &str, hook_id: i64, webhook: &WebhookConfig) -> AppResult<()> {
        let url = format!("{}/repos/{}/{}/hooks/{}", self.api_url, self.org, repo_name, hook_id);
    
        let body = json!({
            // Note: Do not include "name" or "active" when updating
            // according to some GitHub API docs, only config/events/add_events/remove_events
            // Let's stick to config and events for simplicity here. Re-add 'active' if needed.
            // "active": true, // Usually controlled separately if needed
            "events": webhook.events, // Use the events from the parameter
            "config": {
                "url": webhook.url, // Use the URL from the parameter
                "content_type": webhook.content_type, // Use the content_type from the parameter
                "insecure_ssl": "0" // Standard setting
            }
            // "add_events": [], // Optional: Specific events to add without replacing all
            // "remove_events": [] // Optional: Specific events to remove
        });
    
        debug!("Webhook update payload: {}", serde_json::to_string(&body)?);
        info!("Updating webhook {} for {}/{}", hook_id, self.org, repo_name); // Log hook_id
        self.send_patch(&url, body).await?;
        Ok(())
    }

    async fn manage_webhooks(&self, repo_name: &str, webhook: &WebhookConfig, dry_run: bool) -> AppResult<()> {
        let current_hooks = self.get_webhooks(repo_name).await?;
        let existing = current_hooks.iter().find(|h| h.config.url == webhook.url);
        let Some(change) = webhook_change(&self.org, repo_name, existing, webhook) else {
            debug!("Webhook for {}/{} already up to date", self.org, repo_name);
            return Ok(());
        };
        // Printed, not logged: a dry run must show this whatever RUST_LOG says.
        if dry_run {
            writeln!(std::io::stdout(), "[Dry Run] Would {change}")?;
            return Ok(());
        }
        info!("Applying: {change}");
        match existing {
            Some(hook) => self.update_webhook(repo_name, hook.id, webhook).await,
            None => self.create_webhook(repo_name, webhook).await,
        }
    }

    ///
    /// Update repository settings on GitHub to match the desired configuration.
    ///
    /// This method compares the current repository settings with the desired settings and applies only the
    /// necessary changes using the appropriate GitHub API endpoints. The mapping table determines which
    /// settings correspond to which API endpoints and JSON fields.
    ///
    /// # Arguments
    /// * `repo` - The repository whose settings should be updated.
    /// * `dry_run` - If true, no changes are made; actions are logged for preview.
    ///
    /// # Returns
    /// * `Ok(())` if all updates succeed or are skipped in dry-run mode.
    /// * `Err(AppError)` if any API call fails.
    ///
    /// # Behavior
    /// - Only settings that differ from the current state are updated.
    /// - Supports PATCH, PUT, and POST methods as defined in the mapping.
    /// - Logs actions in dry-run mode instead of performing them.
    /// - Also manages webhooks if defined in the repo configuration.
    ///
    /// `archived` is skipped here: `sync` unarchives before and archives after everything else.
    /// Writes the settings `only` accepts that differ from `current`. Writes go out in URL order, so the repo's own
    /// PATCH (visibility included) comes before anything under it, such as the Actions access level.
    pub async fn update_repo_settings(&self, repo: &Repo, current: &RepoSettings, only: impl Fn(&str) -> bool, dry_run: bool) -> AppResult<()> {
        let desired = &repo.settings;
        let mapping = crate::api_mapping::repo_settings();

        // --- Optimization suggestion (Apply after fixing URL): ---
        // Instead of sending one PATCH per setting, collect all PATCHes for the same endpoint.
        // Create a HashMap<String, serde_json::Map<String, serde_json::Value>> where the key is the endpoint URL.
        let mut pending_updates: std::collections::BTreeMap<String, (String, serde_json::Map<String, serde_json::Value>)> = Default::default();
        // The tuple stores (HTTP Method, Body Map)

        for (k, v_desired) in desired.iter().filter(|(k, _)| k.as_str() != ARCHIVED && only(k)) {
            if let Some(field_map) = mapping.get(k.as_str()) {
                let v_current = current.get(k);
                if v_current != Some(v_desired) {
                    // Construct the *full* URL for this *specific* setting's mapped endpoint
                     let relative_path = field_map
                        .endpoint
                        .replace("{org}", &self.org)
                        .replace("{owner}", &self.org)
                        .replace("{repo}", &repo.name); // Add other replacements if needed (e.g., {team_slug})

                    // ****** THIS IS THE KEY FIX ******
                    let full_url = format!("{}{}", self.api_url, relative_path);
                    // ****** END KEY FIX ******

                    // Optimization: Group updates by full_url and method
                    let (_, body_map) = pending_updates
                         .entry(full_url.clone()) // Group by the calculated full URL
                         .or_insert_with(|| (field_map.method.to_string(), serde_json::Map::new())); // Store method and init body map

                    // Add the current setting change to the body map for this URL
                     body_map.insert(
                        field_map.json_path.to_string(), // Use the key expected by the API
                        serde_json::to_value(v_desired).unwrap_or(serde_json::Value::Null),
                    );
                }
            } else {
                debug!("No API mapping for repo setting '{}', skipping.", k);
            }
        }


        // --- Apply Grouped Updates ---
        for (full_url, (method, body_map)) in pending_updates {
             if body_map.is_empty() { continue; } // Skip if no changes ended up for this group

             let body = serde_json::Value::Object(body_map);

             if dry_run {
                 info!(
                     "[Dry Run] Would {} {} with body: {:?}",
                     method, full_url, body // Log the full URL
                 );
             } else {
                 debug!("{} {} with body: {:?}", method, full_url, body); // Log the full URL
                 match method.as_str() { // Use the stored method string
                     "PATCH" => self.send_patch(&full_url, body).await?, // Pass the FULL URL
                     "PUT" => self.send_put(&full_url, Some(body)).await?, // Pass the FULL URL
                     "POST" => self.send_post(&full_url, body).await?, // Pass the FULL URL
                     _ => error!("Unsupported HTTP method: {}", method),
                 }
                 info!("Applied relevant settings changes for repo {} via {}", repo.name, full_url); // Add success log
             }
        }


        Ok(())
    }

    async fn set_archived(&self, repo: &str, archived: bool, dry_run: bool) -> AppResult<()> {
        let (verb, done) = if archived { ("archive", "Archived") } else { ("unarchive", "Unarchived") };
        if dry_run {
            info!("[Dry Run] Would {} {}/{}", verb, self.org, repo);
            return Ok(());
        }
        let url = format!("{}/repos/{}/{}", self.api_url, self.org, repo);
        self.send_patch(&url, json!({ ARCHIVED: archived })).await?;
        info!("{} {}/{}", done, self.org, repo);
        Ok(())
    }

    async fn send_delete(&self, url: &str) -> AppResult<()> {
        self.client.request(Method::DELETE, url, "application/vnd.github+json", None).await?;
        Ok(())
    }

    // Add a helper to get team members
    async fn get_team_members(&self, team_slug: &str) -> AppResult<HashSet<String>> {
        let url = format!("{}/orgs/{}/teams/{}/members?per_page=100", self.api_url, self.org, team_slug);
        let members: Vec<serde_json::Value> = self.get_all(&url).await?;
        Ok(members.iter().filter_map(|m| m["login"].as_str().map(String::from)).collect())
    }

    ///
    /// Create a team in the GitHub organization and add members.
    ///
    /// If the team already exists, ensures all specified members are present.
    ///
    /// # Arguments
    /// * `team` - The team to create or update.
    /// * `dry_run` - If true, no changes are made; actions are logged for preview.
    ///
    /// # Returns
    /// * `Ok(())` if the team is created/updated successfully or in dry-run mode.
    /// * `Err(AppError)` if any API call fails.
    ///
    pub async fn create_team(&self, team: &Team, dry_run: bool) -> AppResult<()> {
        let url_create = format!("{}/orgs/{}/teams", self.api_url, self.org); // For POST
        let team_slug = &team.name; // Assuming name is slug for now
    
        let existing_team = self.get_team(team_slug).await?;
    
        if existing_team.is_none() {
            // --- Create Team ---
            if dry_run {
                info!("[Dry Run] Would create team: {}", team_slug);
                for member in &team.members {
                   info!("[Dry Run] Would add {} to team {}", member, team_slug);
                }
            } else {
                info!("Creating team: {}", team_slug);
                let body = json!({"name": team.name, "privacy": "closed"}); // Use team.name for creation
                self.send_post(&url_create, body).await?;
                for member in &team.members {
                    let member_url = format!("{}/orgs/{}/teams/{}/memberships/{}", self.api_url, self.org, team_slug, member);
                    // Use PUT for initial add too, it works as add/update
                    match self.send_put(&member_url, Some(json!({"role": "member"}))).await { // Specify role maybe? GitHub default is member
                        Ok(()) => info!("Added {} to new team {}", member, team_slug),
                        Err(e) => error!("Failed to add {} to new team {}: {}", member, team_slug, e),
                    }
                }
            }
        } else {
             // --- Update Existing Team Members ---
             info!("Team {} already exists, syncing members", team_slug);
    
             let config_members: HashSet<String> = team.members.iter().cloned().collect();
             let github_members = self.get_team_members(team_slug).await?;
    
             let members_to_add = config_members.difference(&github_members);
             let members_to_remove = github_members.difference(&config_members);
    
             for member in members_to_add {
                 let member_url = format!("{}/orgs/{}/teams/{}/memberships/{}", self.api_url, self.org, team_slug, member);
                if dry_run {
                    info!("[Dry Run] Would add {} to team {}", member, team_slug);
                } else {
                    match self.send_put(&member_url, Some(json!({"role": "member"}))).await {
                         Ok(()) => info!("Added {} to team {}", member, team_slug),
                         Err(e) => error!("Failed to add {} to team {}: {}", member, team_slug, e),
                     }
                }
             }
    
             for member in members_to_remove {
                  let member_url = format!("{}/orgs/{}/teams/{}/memberships/{}", self.api_url, self.org, team_slug, member);
                 if dry_run {
                     info!("[Dry Run] Would remove {} from team {}", member, team_slug);
                 } else {
                     match self.send_delete(&member_url).await {
                         Ok(()) => info!("Removed {} from team {}", member, team_slug),
                         Err(e) => error!("Failed to remove {} from team {}: {}", member, team_slug, e),
                     }
                 }
             }
    
              if config_members == github_members {
                   debug!("Team {} members already match config.", team_slug);
              }
        }
        Ok(())
    }

    ///
    /// Add a user to the GitHub organization or update their role.
    ///
    /// If the user is already a member, updates their role if necessary.
    ///
    /// # Arguments
    /// * `user` - The user to add or update.
    /// * `dry_run` - If true, no changes are made; actions are logged for preview.
    ///
    /// # Returns
    /// * `Ok(())` if the user is added/updated successfully or in dry-run mode.
    /// * `Err(AppError)` if any API call fails.
    ///
    pub async fn add_user_to_org(&self, user: &User, dry_run: bool) -> AppResult<()> {
        let current_role = self.get_user_membership(&user.login).await?;
        if current_role.as_deref() == Some(user.role.as_str()) {
            debug!("No changes needed for user {}", user.login);
            return Ok(());
        }
        if dry_run {
            match current_role {
                Some(role) if role != user.role => {
                    info!(
                        "[Dry Run] Would update {} role: {} -> {}",
                        user.login, role, user.role
                    );
                }
                None => {
                    info!("[Dry Run] Would add {} with role {}", user.login, user.role);
                }
                _ => debug!("[Dry Run] No changes needed for user {}", user.login),
            }
            Ok(())
        } else {
            let full_url = format!(
                "{}/orgs/{}/memberships/{}",
                self.api_url, self.org, user.login
            );
            let body = json!({
                "role": user.role
            });
            info!("Adding {} to org with role {}", user.login, user.role);
            self.send_put(&full_url, Some(body)).await?;
            Ok(())
        }
    }

    ///
    /// Assign a team to a repository with a specific permission level.
    ///
    /// If the team already has a different permission, updates it.
    ///
    /// # Arguments
    /// * `assignment` - The assignment specifying team, repo, and permission.
    /// * `dry_run` - If true, no changes are made; actions are logged for preview.
    ///
    /// # Returns
    /// * `Ok(())` if the assignment is applied successfully or in dry-run mode.
    /// * `Err(AppError)` if any API call fails.
    ///
    pub async fn assign_team_to_repo(&self, assignment: &Assignment, dry_run: bool) -> AppResult<()> {
        let current_perm = self.get_team_repo_permission(&assignment.team, &assignment.repo).await?;
        if current_perm.as_deref() == Some(assignment.permission.as_str()) {
            debug!("No changes needed for team {} on {}/{}", assignment.team, self.org, assignment.repo);
            return Ok(());
        }
        if dry_run {
            match current_perm {
                Some(perm) if perm != assignment.permission => {
                    info!(
                        "[Dry Run] Would update {}/{} permission for team {}: {} -> {}",
                        self.org, assignment.repo, assignment.team, perm, assignment.permission
                    );
                }
                None => {
                    info!(
                        "[Dry Run] Would assign team {} to {}/{} with permission {}",
                        assignment.team, self.org, assignment.repo, assignment.permission
                    );
                }
                _ => debug!(
                    "[Dry Run] No changes needed for team {} on {}/{}",
                    assignment.team, self.org, assignment.repo
                ),
            }
            Ok(())
        } else {
            let full_url = format!(
                "{}/orgs/{}/teams/{}/repos/{}/{}",
                self.api_url, self.org, assignment.team, self.org, assignment.repo
            );
            let body = json!({
                "permission": assignment.permission
            });
            info!(
                "Assigning team {} to repo {} with permission {}",
                assignment.team, assignment.repo, assignment.permission
            );
            self.send_put(&full_url, Some(body)).await?;
            Ok(())
        }
    }

    async fn get_team_repo_permission(&self, team: &str, repo: &str) -> AppResult<Option<String>> {
        let full_url = format!(
            "{}/orgs/{}/teams/{}/repos/{}/{}",
            self.api_url, self.org, team, self.org, repo
        );
        // Without this media type GitHub answers a bare 204, which says nothing about the level.
        match self.get_as(&full_url, "application/vnd.github.v3.repository+json").await {
            Ok(response) => {
                let text = response.text().await?;
                if text.is_empty() {
                    return Err(AppError::GitHubApi(format!("GET {} returned an empty body instead of the team's permissions", full_url)));
                }
                let perms: TeamRepoResponse = serde_json::from_str(&text)
                    .map_err(|e| AppError::GitHubApi(format!("Failed to parse response from {}: {}", full_url, e)))?;
                Ok(perms.permissions.level().map(String::from))
            }
            Err(AppError::GitHubResponse { status: reqwest::StatusCode::NOT_FOUND, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn generate_config_and_write(&self, config_path: &str, dry_run: bool) -> AppResult<()> {
        info!("Generating config from GitHub org: {}", self.org);
        // What the file records for this org stays, so regenerating never changes who is on the default webhook.
        let file = match Config::from_file_with_defaults(config_path, None) {
            Ok(file) => Some(file).filter(|file| file.org.eq_ignore_ascii_case(&self.org)),
            Err(AppError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let (config, unread) = self.generate_unfiltered_config_from_org(file.as_ref()).await?;  // Make sure to call an unfiltered version
        if !unread.is_empty() {
            println!("Not read, so these keep what the file has for them: {}", unread.join(", "));
        }

        // ... rest of the YAML writing logic ...
        // (Ensure this part uses the full, unfiltered config)

         let mut yaml_content = String::new();
         yaml_content.push_str(&format!("org: {}\n\n", config.org));

         // Add assignments (sorted)
         let mut assignments = config.assignments;
         assignments.sort_by(|a, b| a.team.cmp(&b.team).then(a.repo.cmp(&b.repo)));
         if !assignments.is_empty() {
              yaml_content.push_str("assignments:\n");
              for assignment in &assignments {
                  yaml_content.push_str(&format!(
                      "- repo: {}\n  team: {}\n  permission: {}\n",
                      assignment.repo, assignment.team, assignment.permission
                  ));
              }
              yaml_content.push_str("\n");
         } else {
             yaml_content.push_str("assignments: []\n\n");
         }

         // Add default webhook if present
         if let Some(default_webhook) = &config.default_webhook {
             yaml_content.push_str("default_webhook:\n");
             yaml_content.push_str(&format!("  url: {}\n", default_webhook.url));
             yaml_content.push_str(&format!("  content_type: {}\n", default_webhook.content_type));
             yaml_content.push_str("  events:\n");
             for event in &default_webhook.events {
                 yaml_content.push_str(&format!("  - {}\n", event));
             }
             yaml_content.push_str("\n");
         }

         // Add repos (sorted)
         let mut repos = config.repos;
         repos.sort_by(|a, b| a.name.cmp(&b.name));
         if !repos.is_empty() {
             yaml_content.push_str("repos:\n");
             for repo in &repos {
                 yaml_content.push_str(&format!("- name: {}\n", repo.name));

                 // Only write settings actually fetched/relevant (potentially limited set here)
                 if !repo.settings.is_empty() {
                       yaml_content.push_str("  settings:\n");
                       // Example: Write only specific known settings for cleaner output
                       let keys_to_write = ["allow_merge_commit", "allow_squash_merge", "allow_rebase_merge", ARCHIVED, ALLOW_AUTO_MERGE, ACCESS_LEVEL];
                       let mut setting_keys: Vec<_> = repo.settings.keys().collect();
                       setting_keys.sort(); // Sort keys within settings
                       for key in setting_keys {
                           if keys_to_write.contains(&key.as_str()) {
                             if let Some(value) = repo.settings.get(key) {
                                let val_str = serde_yaml::to_string(value).unwrap_or_default().trim().to_string();
                                // Basic indentation and handling for simple values
                                yaml_content.push_str(&format!("    {}: {}\n", key, val_str));
                              }
                           }
                       }
                 } else {
                      // Still ensure settings key exists if empty
                      yaml_content.push_str("  settings: {}\n");
                 }


                 if let Some(visibility) = &repo.visibility {
                     yaml_content.push_str(&format!("  visibility: {}\n", visibility));
                 }
                 if repo.use_default_webhook {
                     yaml_content.push_str("  use_default_webhook: true\n");
                 }
                 if let Some(webhook) = &repo.webhook {
                     yaml_content.push_str("  webhook:\n");
                     yaml_content.push_str(&format!("    url: {}\n", webhook.url));
                     yaml_content.push_str(&format!("    content_type: {}\n", webhook.content_type));
                     yaml_content.push_str("    events:\n");
                     for event in &webhook.events {
                         yaml_content.push_str(&format!("    - {}\n", event));
                     }
                 }
                if !repo.required_checks.is_empty() {
                    yaml_content.push_str("  required_checks:\n");
                    for check in &repo.required_checks {
                        yaml_content.push_str(&format!("  - {}\n", serde_yaml::to_string(check)?.trim()));
                    }
                }
                if let Some(approvals) = repo.required_approvals {
                    yaml_content.push_str(&format!("  required_approvals: {approvals}\n"));
                }
                if !repo.required_checks_bypass.is_empty() {
                    yaml_content.push_str("  required_checks_bypass:\n");
                    for bypass in &repo.required_checks_bypass {
                        yaml_content.push_str(&format!("  - team: {}\n", serde_yaml::to_string(&bypass.team)?.trim()));
                        yaml_content.push_str(&format!("    mode: {}\n", bypass.mode.as_str()));
                    }
                }
                // Add branch protections if needed
                if !repo.branch_protections.is_empty() {
                    // Serialize properly
                }
             }
             yaml_content.push_str("\n");
         }

         // Add teams (sorted)
         let mut teams = config.teams;
         teams.sort_by(|a, b| a.name.cmp(&b.name));
         if !teams.is_empty() {
             yaml_content.push_str("teams:\n");
             for team in &teams {
                 yaml_content.push_str(&format!("- name: {}\n", team.name));
                 yaml_content.push_str("  members:\n");
                 let mut members = team.members.clone();
                 members.sort();
                 for member in &members {
                     yaml_content.push_str(&format!("  - {}\n", member));
                 }
             }
             yaml_content.push_str("\n");
         }

         // Add users (sorted)
         let mut users = config.users;
         users.sort_by(|a, b| a.login.cmp(&b.login));
         if !users.is_empty() {
             yaml_content.push_str("users:\n");
             for user in &users {
                 yaml_content.push_str(&format!("- login: {}\n  role: {}\n", user.login, user.role));
             }
             yaml_content.push_str("\n");
         }

         // Never replace a config with one that wouldn't load: every later command, this one included, would refuse it.
         Config::from_yaml_str(&yaml_content, config_path).map_err(|e| {
             AppError::Config(format!("The config generated from the org doesn't load, so {config_path} is left as it is: {e}"))
         })?;
         if dry_run {
             println!("Dry run: Would write the following config to {}:\n{}", config_path, yaml_content);
         } else {
             println!("Writing generated config to {}", config_path);
             let mut file = File::create(config_path).map_err(AppError::Io)?;
             file.write_all(yaml_content.as_bytes()).map_err(AppError::Io)?;
             println!("Config generation completed successfully.");
         }
        Ok(())
    }

    /// The org as a config, reading `file` for what GitHub can't say: which default to keep and, as a config holds
    /// one webhook per repo, which of a repo's webhooks to record. Without a default, the first webhook found is it.
    /// Also returns the repos whose webhooks couldn't be read: their webhook is the file's, not GitHub's.
    async fn generate_unfiltered_config_from_org(&self, file: Option<&Config>) -> AppResult<(Config, Vec<String>)> {
        let mut default_webhook = file
            .and_then(|file| file.default_webhook.clone())
            .map(|wh| WebhookConfig { events: sorted(&wh.events), ..wh });
        // This is essentially the original logic of generate_config_from_org
         let mut repos = Vec::new();
        let mut unread = Vec::new();
        let team_ids = self.team_ids().await?;
        let repo_url = format!("{}/orgs/{}/repos?per_page=100", self.api_url, self.org);
        let repo_json: Vec<serde_json::Value> = self.get_all(&repo_url).await?;

        for repo in repo_json {
            let name = repo["name"].as_str().ok_or_else(|| AppError::GitHubApi("Missing repo name".to_string()))?.to_string();
            // Fetch *limited* settings relevant for generating a *manageable* config
            let mut settings = match self.get_repo_settings(&name).await {
                Ok(full_settings) => {
                    let mut manageable_settings = RepoSettings::new();
                    // Only include settings we typically manage
                    let managed_keys = ["allow_merge_commit", "allow_squash_merge", "allow_rebase_merge"];
                    for key in managed_keys {
                         if let Some(value) = full_settings.get(key) {
                             manageable_settings.insert(key.to_string(), value.clone());
                         }
                    }
                    // Like `archived: false`, a default isn't written.
                    for (key, default) in omitted_defaults() {
                        if let Some(value) = full_settings.get(key).filter(|v| **v != default) {
                            manageable_settings.insert(key.to_string(), value.clone());
                        }
                    }
                     manageable_settings
                }
                Err(e) => {
                   error!("Failed to fetch settings for repo {}: {}", name, e);
                    RepoSettings::new() // Return empty settings on error
                }
            };

            let visibility = if repo["private"].as_bool().unwrap_or(false) { Some("private".to_string()) } else { Some("public".to_string()) };
            let in_file = file.and_then(|file| file.repos.iter().find(|r| r.name == name));
            // Public repos have no access level: their workflows are open to everyone.
            if repo["private"].as_bool() == Some(true) {
                match self.get_access_level(&name).await {
                    Ok(level) if level != "none" => {
                        settings.insert(ACCESS_LEVEL.to_string(), serde_yaml::Value::String(level));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        error!("{e}. Its access level stays as the file has it.");
                        unread.push(format!("{name} (access level)"));
                        if let Some(level) = in_file.and_then(|r| r.settings.get(ACCESS_LEVEL)) {
                            settings.insert(ACCESS_LEVEL.to_string(), level.clone());
                        }
                    }
                }
            }
            // Like settings above: report the repo and carry on, so one unreadable repo doesn't hide the rest.
            let (use_default_webhook, webhook) = match self.get_webhooks(&name).await {
                Ok(hooks) => {
                    let with_url = |webhook: Option<&WebhookConfig>| hooks.iter().find(|wh| webhook.is_some_and(|w| w.url == wh.config.url));
                    let hook = with_url(in_file.and_then(|r| r.webhook.as_ref())).or(with_url(default_webhook.as_ref())).or(hooks.first());
                    let webhook = hook.map(|wh| WebhookConfig {
                        url: wh.config.url.clone(),
                        content_type: wh.config.content_type.clone(),
                        events: sorted(&wh.events),
                    });
                    if default_webhook.is_none() {
                        default_webhook.clone_from(&webhook);
                    }
                    let is_default = webhook.is_some() && webhook == default_webhook;
                    (is_default, webhook.filter(|_| !is_default))
                }
                Err(e) => {
                    error!("Failed to fetch webhooks for repo {}: {}. Its webhook stays as the file has it.", name, e);
                    unread.push(format!("{name} (webhooks)"));
                    in_file.map_or((false, None), |r| (r.use_default_webhook, r.webhook.clone()))
                }
            };
            let file_checks = in_file.map(|r| r.required_checks.clone()).unwrap_or_default();
            let file_bypass = in_file.map(|r| r.required_checks_bypass.clone()).unwrap_or_default();
            // One list of the repo's rulesets for both: a read that fails leaves both as the file has them.
            let (ci, review) = match self.read_owned_rulesets(&name).await {
                Ok((ci, review)) => (Ok(ci), Ok(review)),
                Err(e) => (Err(AppError::GitHubApi(e.to_string())), Err(AppError::GitHubApi(e.to_string()))),
            };
            let ci = ci.and_then(|r| r.map(|r| parse_require_ci(&name, &r)).transpose());
            let review = review.and_then(|r| r.map(|r| parse_require_review(&name, &r)).transpose());
            let (required_checks, required_checks_bypass) = match ci {
                Ok(found) => {
                    // The file's order when it names the same checks, so regenerating doesn't reorder them.
                    let checks = match found.as_ref().map(RequireCi::contexts).unwrap_or_default() {
                        checks if same_checks(&checks, &file_checks) => file_checks,
                        checks => checks,
                    };
                    let bypass = match found.as_ref().map(|r| r.bypass.as_deref()) {
                        None => Ok(Vec::new()),
                        Some(None) => Err("GitHub didn't show its bypass list (it does only to tokens that can administer the repo)".to_string()),
                        Some(Some(actors)) => actors
                            .iter()
                            .map(|a| a.as_config(&team_ids).ok_or_else(|| format!("its bypass list holds {}, which a config can't name", a.describe(&team_ids))))
                            .collect::<Result<Vec<_>, _>>(),
                    };
                    let bypass = match bypass {
                        Ok(mut bypass) => {
                            bypass.sort();
                            bypass
                        }
                        Err(why) => {
                            error!("The `{REQUIRE_CI}` ruleset of repo {name}: {why}. Its bypass list stays as the file has it.");
                            unread.push(format!("{name} (required-check bypass)"));
                            file_bypass
                        }
                    };
                    // A config without checks leaves the ruleset alone, so a bypass list there would mean nothing:
                    // the loader refuses one.
                    let bypass = if checks.is_empty() { Vec::new() } else { bypass };
                    (checks, bypass)
                }
                Err(e) => {
                    error!("Failed to read the `{}` ruleset of repo {}: {}. Its required checks stay as the file has them.", REQUIRE_CI, name, e);
                    unread.push(format!("{name} (required checks)"));
                    (file_checks, file_bypass)
                }
            };

            let required_approvals = match review {
                Ok(found) => found.map(|r| r.approvals),
                Err(e) => {
                    error!("Failed to read the `{}` ruleset of repo {}: {}. Its required approvals stay as the file has them.", REQUIRE_REVIEW, name, e);
                    unread.push(format!("{name} (required approvals)"));
                    in_file.and_then(|r| r.required_approvals)
                }
            };
            repos.push(Repo {
                name,
                settings,
                visibility,
                use_default_webhook,
                webhook,
                branch_protections: vec![], // Add logic to fetch these if needed
                required_checks,
                required_checks_bypass,
                required_approvals,
            });
        }

        // Fetch teams, users, assignments as before (full state needed for generation)
        let mut teams = Vec::new();
        for (name, _) in team_ids {
            // Fetch full team data including members
            let mut members: Vec<String> = self.get_team_members(&name).await?.into_iter().collect();
            members.sort();

            teams.push(Team { name, members });
        }

        let mut users = Vec::new();
        let members_url = format!("{}/orgs/{}/members?per_page=100", self.api_url, self.org);
        let members_json: Vec<serde_json::Value> = self.get_all(&members_url).await?;

        for member in members_json {
            let login = member["login"].as_str().ok_or_else(|| AppError::GitHubApi("Missing member login".to_string()))?.to_string();
            let role_response = self.get_user_membership(&login).await?;
            let role = role_response.unwrap_or("member".to_string()); // Default to member if fetch fails? Or error?
            users.push(User { login, role });
        }

        let mut assignments = Vec::new();
        for team in &teams {
            let team_repos = self.get_team_repos(&team.name).await?;
            for repo in team_repos {
                if let Some(permission) = repo.permissions.level() {
                    assignments.push(Assignment {
                        repo: repo.name.clone(),
                        team: team.name.clone(),
                        permission: permission.to_string(),
                    });
                }
            }
        }

        let config = Config {
            org: self.org.clone(),
            repos,
            teams,
            users,
            assignments,
            default_webhook,
            default_branch_protections: vec![], // Add logic if needed
        };
        Ok((config, unread))
    }

    pub async fn sync(&mut self, config_path: &str, dry_run: bool) -> AppResult<()> {
        let config = crate::config::Config::from_file_with_defaults(config_path, None)?;
        self.org = config.org.clone(); // Set org from config
        check_archived(&config)?;
        check_access_levels(&config)?;

        if dry_run {
            info!("Running in dry-run mode; validating changes without applying.");
        } else {
            info!("Running in apply mode; changes will be applied.");
        }

        let mut config = config;
        config.resolve_webhooks();

        // An archived repo is read-only: unarchive before its other changes, archive after all of them,
        // and leave the settings and webhook of a repo that stays archived alone.
        let mut to_archive = Vec::new();
        // Teams come before repos, so a repo's bypass list may name a team this config adds. Every bypass list is
        // checked before the first write; a team still to be created stands in with id 0 until it exists.
        let gated = config.repos.iter().any(|r| !r.required_checks.is_empty());
        let mut teams = if gated { self.team_ids().await? } else { Vec::new() };
        let adds: Vec<(String, u64)> = config.teams.iter().filter(|t| !teams.iter().any(|(slug, _)| *slug == t.name)).map(|t| (t.name.clone(), 0)).collect();
        teams.extend(adds.iter().cloned());
        for repo in &config.repos {
            bypass_actors(&self.org, repo, &teams)?;
        }
        for team in &config.teams {
            info!("Processing team: {}", team.name);
            self.create_team(team, dry_run).await?;
        }
        if gated && !adds.is_empty() && !dry_run {
            teams = self.team_ids().await?;
        }
        for repo in &config.repos {
            // --- Add logging just before the failing call ---
            info!("Processing repo: {}/{}", self.org, repo.name);
            if repo.name.trim().is_empty() {
                 error!("Found repo with empty name in config file '{}'.", config_path);
                 return Err(AppError::GitHubApi("Invalid empty repo name found in config.".to_string()));
            }
            // You might add more validation for repo.name characters here if needed
            // --- End logging ---

            let found = match self.get_repo_settings(&repo.name).await {
                Ok(current) => Some(current),
                Err(AppError::GitHubResponse { status: reqwest::StatusCode::NOT_FOUND, .. }) => None,
                Err(e) => return Err(e),
            };
            // Before this repo's first write, created or not.
            check_access_level(repo, found.as_ref())?;
            let mut current = match found {
                Some(current) => current,
                None => {
                    self.create_repo(repo, dry_run).await?;
                    if dry_run {
                        // Nothing to read until it exists; the real run applies its settings right after creating it.
                        continue;
                    }
                    self.get_repo_settings(&repo.name).await?
                }
            };
            // The access level exists only on private repos. Read it only when the config names it, so it's written
            // only when it differs. A repo the settings make private now has none to read: it's written after the
            // visibility PATCH (URL order, see `update_repo_settings`).
            let now_public = current.get("visibility").and_then(|v| v.as_str()) == Some("public");
            let skip_access = ends_public(repo, Some(&current))? == Some(true);
            if repo.settings.contains_key(ACCESS_LEVEL) && !now_public {
                let level = self.get_access_level(&repo.name).await?;
                current.insert(ACCESS_LEVEL.to_string(), serde_yaml::Value::String(level));
            }
            match (archived(&current, &repo.name)? == Some(true), archived(&repo.settings, &repo.name)?) {
                (true, Some(false)) => self.set_archived(&repo.name, false, dry_run).await?,
                (true, desired) => {
                    let hint = if desired.is_none() { format!(" Set `{ARCHIVED}: false` to unarchive it.") } else { String::new() };
                    info!("Skipping {}/{}: it is archived, so its settings and webhook are left as they are.{}", self.org, repo.name, hint);
                    continue;
                }
                (false, Some(true)) => to_archive.push(repo.name.as_str()),
                (false, _) => {}
            }
            // Auto-merge goes on only after the ruleset that gates it, so a failed ruleset write never leaves PRs
            // merging with no checks. A public repo's `access_level: none` has nothing to write.
            self.update_repo_settings(repo, &current, |k| k != ALLOW_AUTO_MERGE && !(skip_access && k == ACCESS_LEVEL), dry_run).await?;
            if let Some(webhook) = repo.webhook.as_ref() {
                self.manage_webhooks(&repo.name, webhook, dry_run).await?;
            }
            self.sync_required_checks(repo, &teams, dry_run).await?;
            self.sync_required_approvals(repo, dry_run).await?;
            self.update_repo_settings(repo, &current, |k| k == ALLOW_AUTO_MERGE, dry_run).await?;
        }

        // Users
        for user in &config.users {
            info!("Processing user: {}", user.login); // Add similar logging
            self.add_user_to_org(user, dry_run).await?;
        }

        // Assignments, archived repos included: GitHub decides whether their team access can change,
        // so sync never skips a change that diff reports.
        for assignment in &config.assignments {
            info!("Processing assignment: Team '{}' on Repo '{}'", assignment.team, assignment.repo); // Add similar logging
            self.assign_team_to_repo(assignment, dry_run).await?;
        }

        for repo in to_archive {
            self.set_archived(repo, true, dry_run).await?;
        }

        if dry_run {
            println!("Dry run completed successfully. No changes were applied.");
        } else {
            println!("Sync completed successfully. All changes applied.");
        }
        Ok(())
    }

    /// Diffs the local configuration against the full GitHub org state (as written by sync-from-github).
    pub async fn diff(&self, config_path: &str) -> AppResult<bool> {
        info!("Generating diff between full GitHub org state and local config: {} (this matches what sync-from-github would write)", config_path);

        // --- Step 1: Load original local config ---
        let local_config = crate::config::Config::from_file_with_defaults(config_path, None)?;
        check_archived(&local_config)?;

        // --- Step 2: Fetch FULL GitHub org state (unfiltered, as sync-from-github would write) ---
        let (github_config, unread) = self.generate_unfiltered_config_from_org(Some(&local_config)).await?;

        // --- Step 3: Prepare final versions for diffing ---
        let mut diff_local_config = local_config.clone(); // Clone original local config
        let mut diff_github_config = github_config;      // Use the fetched FULL GitHub org state

        // GitHub's side leaves out defaults such as `archived: false`; where the local config names one, show the
        // real value. A repo archived on GitHub whose config leaves `archived` out still shows `- archived: true`:
        // like any setting, that's what sync-from-org would add.
        for local in &diff_local_config.repos {
            if let Some(remote) = diff_github_config.repos.iter_mut().find(|r| r.name == local.name) {
                for (key, default) in omitted_defaults().into_iter().filter(|(k, _)| local.settings.contains_key(*k)) {
                    remote.settings.entry(key.to_string()).or_insert(default);
                }
            }
        }
        // Required checks and their bypass list are sets: their order means nothing.
        for repo in diff_local_config.repos.iter_mut().chain(diff_github_config.repos.iter_mut()) {
            repo.required_checks.sort();
            repo.required_checks_bypass.sort();
        }

        // --- Step 4: Compare the webhook each repo ends up with ---
        diff_local_config.resolve_webhooks();
        diff_github_config.resolve_webhooks();

        // --- Step 5: Sort both configs... (ensure BTreeMap logic is kept) ---
        // Repos
        diff_local_config.repos.sort_by(|a, b| a.name.cmp(&b.name));
        diff_github_config.repos.sort_by(|a, b| a.name.cmp(&b.name));

        // Teams (and members within teams)
        diff_local_config.teams.sort_by(|a, b| a.name.cmp(&b.name));
        diff_github_config.teams.sort_by(|a, b| a.name.cmp(&b.name));
        for team in &mut diff_local_config.teams { team.members.sort(); }
        for team in &mut diff_github_config.teams { team.members.sort(); }

        // Users
        diff_local_config.users.sort_by(|a, b| a.login.cmp(&b.login));
        diff_github_config.users.sort_by(|a, b| a.login.cmp(&b.login));

        // Assignments
        diff_local_config.assignments.sort_by(|a, b| a.team.cmp(&b.team).then(a.repo.cmp(&b.repo)));
        diff_github_config.assignments.sort_by(|a, b| a.team.cmp(&b.team).then(a.repo.cmp(&b.repo)));

        // --- Step 6: Serialize and Diff ---
        let github_yaml = serde_yaml::to_string(&diff_github_config)?;
        let local_yaml = serde_yaml::to_string(&diff_local_config)?;

        // Compute and print diff
        let diff = TextDiff::from_lines(&github_yaml, &local_yaml);

        // Corrected: Bind the configured builder to ensure it lives long enough
        let mut unified_diff = diff.unified_diff();
        let unified_diff_builder = unified_diff // Create the builder
            .context_radius(3)                           // Configure it
            .header("GitHub (Normalized)", "Local Config (Normalized)"); // Configure it further

        let mut has_diffs = false;
        let mut output_buffer = String::new();

        // Iterate using the named builder instance
        for hunk in unified_diff_builder.iter_hunks() { // Now using the variable that lives long enough
            has_diffs = true;
            output_buffer.push_str(&format!(
                "@@ {} @@\n",
                hunk.header()
            ));
            for change in hunk.iter_changes() {
                match change.tag() {
                    ChangeTag::Delete => output_buffer.push_str(&format!("{}\n", format!("- {}", change.value().trim_end()).red())),
                    ChangeTag::Insert => output_buffer.push_str(&format!("{}\n", format!("+ {}", change.value().trim_end()).green())),
                    ChangeTag::Equal => output_buffer.push_str(&format!("  {}\n", change.value().trim_end())),
                }
            }
        }

        if !has_diffs && unread.is_empty() {
            println!("No differences found between full GitHub org state and local config.");
        } else if has_diffs {
            println!("Differences found between full GitHub org state and local config:");
            println!("--- GitHub Org (Normalized)"); // Adjusted header
            println!("+++ Local Config (Normalized)"); // Adjusted header
            print!("{}", output_buffer);
        }
        // Those repos match above only because their webhook came from the file: that says nothing about GitHub.
        if !unread.is_empty() {
            println!("Not compared, because they couldn't be read: {}", unread.join(", "));
        }
        Ok(has_diffs || !unread.is_empty())
    }
}


/// `events` in the order configs and comparisons use: GitHub's own order means nothing.
fn sorted(events: &[String]) -> Vec<String> {
    let mut events = events.to_vec();
    events.sort();
    events
}

/// What sync would do to a repo's webhook, for people to read; `None` if it already matches.
/// It names the URL's host only: the rest of a webhook URL is often a credential, and this reaches CI logs.
fn webhook_change(org: &str, repo: &str, existing: Option<&WebhookResponse>, webhook: &WebhookConfig) -> Option<String> {
    let parsed = reqwest::Url::parse(&webhook.url).ok();
    let host = parsed.as_ref().and_then(|url| url.host_str()).unwrap_or("an unparsable URL");
    let events = sorted(&webhook.events);
    let Some(hook) = existing else {
        return Some(format!("create webhook on {org}/{repo}: {host}, content_type {}, events {events:?}", webhook.content_type));
    };
    let current = sorted(&hook.events);
    (current != events || hook.config.content_type != webhook.content_type).then(|| {
        format!("update webhook on {org}/{repo}: {host}, events {current:?} -> {events:?}, content_type {} -> {}", hook.config.content_type, webhook.content_type)
    })
}

/// The `rel="next"` URL of GitHub's `Link` header, if there is a next page.
fn next_page(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let link = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        params
            .split(';')
            .any(|p| p.trim() == r#"rel="next""#)
            .then(|| url.trim().trim_start_matches('<').trim_end_matches('>').to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito;
    use tokio;

    #[tokio::test]
    async fn test_github_client_new() {
        let client = GitHubClient::new("dummy_token", "dummy_org");
        assert_eq!(client.org, "dummy_org");
    }

    #[test]
    fn test_get_webhooks_parses_response() {
        let mut server = mockito::Server::new();

        let _m = server
            .mock("GET", "/repos/dummy_org/dummy_repo/hooks")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"
[
  {
    "id": 123,
    "url": "http://api.github.com/hook/123",
    "config": {
      "url": "http://example.com",
      "content_type": "json"
    },
    "events": ["push", "pull_request"]
  }
]
"#,
            )
            .create();

        let rt = tokio::runtime::Runtime::new().expect("create runtime");
        rt.block_on(async {
            let mut client = GitHubClient::new("dummy_token", "dummy_org");
            client.org = "dummy_org".to_string();

            let url = format!("{}/repos/dummy_org/dummy_repo/hooks", server.url());
            let response = client.get(&url).await.expect("HTTP GET failed");
            let text = response.text().await.expect("read response text");
            let hooks: Vec<WebhookResponse> = serde_json::from_str(&text).expect("parse JSON");

            assert_eq!(hooks.len(), 1);
            let hook = &hooks[0];
            assert_eq!(hook.id, 123);
            assert_eq!(hook.url, "http://api.github.com/hook/123");
            assert_eq!(hook.config.url, "http://example.com");
            assert_eq!(hook.config.content_type, "json");
            assert_eq!(hook.events, vec!["push", "pull_request"]);
        });
    }

    /// Mocks the GET side of org `o`: each `(name, archived)` repo with one shared webhook, no teams, no members.
    /// Like GitHub, the repo list comes in pages linked by `Link: rel="next"`, here one repo per page.
    async fn mock_org(server: &mut mockito::ServerGuard, repos: &[(&str, bool)]) -> Vec<mockito::Mock> {
        let hooks = json!([{"id": 1, "url": "u", "config": {"url": "https://hooks.example/h", "content_type": "json"}, "events": ["push"]}]);
        mock_org_with_hooks(server, repos, |_| hooks.clone()).await
    }

    /// `mock_org` where `hooks` gives each repo's webhooks.
    async fn mock_org_with_hooks(server: &mut mockito::ServerGuard, repos: &[(&str, bool)], hooks: impl Fn(&str) -> serde_json::Value) -> Vec<mockito::Mock> {
        let repos: Vec<_> = repos.iter().map(|(name, archived)| (*name, json!({"archived": archived}))).collect();
        mock_org_full(server, &repos, hooks, |_| Some(vec![])).await
    }

    /// The general `mock_org`: each repo's fields as GitHub reports them on top of the defaults below (its
    /// `access_level`, if given, is served by the Actions access endpoint), its webhooks, and its own rulesets
    /// (`None`: not mocked, so reading them fails).
    async fn mock_org_full(
        server: &mut mockito::ServerGuard,
        repos: &[(&str, serde_json::Value)],
        hooks: impl Fn(&str) -> serde_json::Value,
        rulesets: impl Fn(&str) -> Option<Vec<serde_json::Value>>,
    ) -> Vec<mockito::Mock> {
        mock_org_with_teams(server, &[], repos, hooks, rulesets).await
    }

    /// `mock_org_full` with teams `(slug, id)`, each with no members and no repos.
    async fn mock_org_with_teams(
        server: &mut mockito::ServerGuard,
        teams: &[(&str, u64)],
        repos: &[(&str, serde_json::Value)],
        hooks: impl Fn(&str) -> serde_json::Value,
        rulesets: impl Fn(&str) -> Option<Vec<serde_json::Value>>,
    ) -> Vec<mockito::Mock> {
        let team_list: Vec<_> = teams.iter().map(|(slug, id)| json!({"slug": slug, "name": slug, "id": id})).collect();
        let mut mocks = vec![
            server.mock("GET", "/orgs/o/teams?per_page=100").with_body(json!(team_list).to_string()).create_async().await,
            server.mock("GET", "/orgs/o/members?per_page=100").with_body("[]").create_async().await,
        ];
        for (slug, _) in teams {
            let url = format!("/orgs/o/teams/{slug}");
            mocks.push(server.mock("GET", url.as_str()).with_body(json!({"name": slug}).to_string()).create_async().await);
            for what in ["members", "repos"] {
                let url = format!("/orgs/o/teams/{slug}/{what}?per_page=100");
                mocks.push(server.mock("GET", url.as_str()).with_body("[]").create_async().await);
            }
        }
        let page = |n: usize| if n == 1 { "/orgs/o/repos?per_page=100".to_string() } else { format!("/orgs/o/repos?per_page=100&page={n}") };
        for (i, (name, fields)) in repos.iter().enumerate() {
            let mut repo = json!({"name": name, "private": false, "visibility": "public", "archived": false,
                "allow_merge_commit": false, "allow_squash_merge": true, "allow_rebase_merge": true});
            for (key, value) in fields.as_object().expect("repo fields") {
                repo[key] = value.clone();
            }
            if let Some(level) = repo.as_object_mut().expect("repo").remove(ACCESS_LEVEL) {
                let url = format!("/repos/o/{name}/actions/permissions/access");
                mocks.push(server.mock("GET", url.as_str()).with_body(json!({ ACCESS_LEVEL: level }).to_string()).create_async().await);
            }
            let mut list = server.mock("GET", page(i + 1).as_str()).with_body(json!([{"name": name, "private": repo["private"]}]).to_string());
            if i + 1 < repos.len() {
                list = list.with_header("link", &format!(r#"<{0}{1}>; rel="next", <{0}{2}>; rel="last""#, server.url(), page(i + 2), page(repos.len())));
            }
            mocks.push(list.create_async().await);
            mocks.push(server.mock("GET", format!("/repos/o/{name}").as_str()).with_body(repo.to_string()).create_async().await);
            mocks.push(server.mock("GET", format!("/repos/o/{name}/hooks?per_page=100").as_str()).with_body(hooks(name).to_string()).create_async().await);
            if let Some(rulesets) = rulesets(name) {
                mocks.extend(mock_rulesets(server, name, rulesets).await);
            }
        }
        mocks
    }

    /// A repo's rulesets: the list `read_require_ci` reads, and each one by id.
    async fn mock_rulesets(server: &mut mockito::ServerGuard, repo: &str, rulesets: Vec<serde_json::Value>) -> Vec<mockito::Mock> {
        let list: Vec<_> = rulesets.iter().map(|r| json!({"id": r["id"], "name": r["name"]})).collect();
        let url = format!("/repos/o/{repo}/rulesets?includes_parents=false&per_page=100");
        let mut mocks = vec![server.mock("GET", url.as_str()).with_body(json!(list).to_string()).create_async().await];
        for ruleset in rulesets {
            let url = format!("/repos/o/{repo}/rulesets/{}", ruleset["id"]);
            mocks.push(server.mock("GET", url.as_str()).with_body(ruleset.to_string()).create_async().await);
        }
        mocks
    }

    /// `checks` pinned to no app.
    fn unpinned(checks: &[&str]) -> Vec<RequiredCheck> {
        checks.iter().map(|c| RequiredCheck { context: c.to_string(), integration_id: None }).collect()
    }

    /// A `Require CI` ruleset with id `id` requiring `checks`, as `sync` writes it.
    fn require_ci(id: u64, checks: &[&str]) -> serde_json::Value {
        let mut ruleset = require_ci_body(&unpinned(checks), &[]);
        ruleset["id"] = json!(id);
        ruleset
    }

    /// Mocks `method path`, recording each request as `"METHOD path body"` in the returned list.
    async fn record(server: &mut mockito::ServerGuard, writes: &std::sync::Arc<std::sync::Mutex<Vec<String>>>, method: &str, path: &str) -> mockito::Mock {
        let writes = writes.clone();
        let label = format!("{method} {path}");
        server
            .mock(method, path)
            .with_body_from_request(move |req| {
                let body = req.utf8_lossy_body().expect("request body").to_string();
                writes.lock().expect("lock").push(format!("{label} {body}"));
                b"{}".to_vec()
            })
            .create_async()
            .await
    }

    fn client(server: &mockito::ServerGuard) -> GitHubClient {
        let mut client = GitHubClient::new("t", "o");
        client.api_url = server.url();
        client
    }

    fn write_file(text: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("create temp file");
        file.write_all(text.as_bytes()).expect("write temp file");
        file
    }

    /// The `- name: <repo>` block of a generated config.
    fn repo_block<'a>(yaml: &'a str, name: &str) -> &'a str {
        let start = yaml.find(&format!("- name: {name}\n")).expect("repo in output");
        let rest = &yaml[start + 1..];
        &yaml[start..start + 1 + rest.find("\n- ").unwrap_or(rest.len())]
    }

    #[tokio::test]
    async fn sync_from_org_records_archived_repos_only() {
        let mut server = mockito::Server::new_async().await;
        let _mocks = mock_org(&mut server, &[("live", false), ("old", true)]).await;
        let out = write_file("org: o\n");
        let path = out.path().to_str().expect("utf-8 path");
        client(&server).generate_config_and_write(path, false).await.expect("sync-from-org");
        let yaml = std::fs::read_to_string(path).expect("read output");
        assert!(repo_block(&yaml, "old").contains("    archived: true\n"), "{yaml}");
        assert!(!repo_block(&yaml, "live").contains("archived"), "{yaml}");
    }

    #[tokio::test]
    async fn sync_from_org_reads_every_page_of_repos() {
        let mut server = mockito::Server::new_async().await;
        let names = ["a", "b", "c"];
        let _mocks = mock_org(&mut server, &names.map(|n| (n, false))).await;
        let out = write_file("org: o\n");
        let path = out.path().to_str().expect("utf-8 path");
        client(&server).generate_config_and_write(path, false).await.expect("sync-from-org");
        let yaml = std::fs::read_to_string(path).expect("read output");
        for name in names {
            assert!(yaml.contains(&format!("- name: {name}\n")), "repo {name} on a later page is missing:\n{yaml}");
        }
    }

    #[tokio::test]
    async fn sync_finds_its_webhook_on_a_later_page_instead_of_adding_a_duplicate() {
        let mut server = mockito::Server::new_async().await;
        let hook = |id: i64, url: &str| json!([{"id": id, "url": "u", "config": {"url": url, "content_type": "json"}, "events": ["push"]}]).to_string();
        let page2 = "/repos/o/r/hooks?per_page=100&page=2";
        let _mocks = [
            server.mock("GET", "/repos/o/r").with_body(r#"{"name":"r","archived":false}"#).create_async().await,
            server
                .mock("GET", mockito::Matcher::Regex(r"^/repos/o/r/hooks(\?per_page=100)?$".into()))
                // A relative target (RFC 8288) resolves against the request URL; mock_org covers absolute ones.
                .with_header("link", &format!(r#"<{page2}>; rel="next""#))
                .with_body(hook(1, "https://other.example/x"))
                .create_async()
                .await,
            server.mock("GET", page2).with_body(hook(31, "https://hooks.example/h")).create_async().await,
        ];
        let create = server.mock("POST", "/repos/o/r/hooks").expect(0).create_async().await;
        let config = write_file("org: o\nrepos:\n- name: r\n  webhook: {url: https://hooks.example/h, content_type: json, events: [push]}\n");
        client(&server).sync(config.path().to_str().expect("utf-8 path"), false).await.expect("sync");
        create.assert_async().await;
    }

    #[tokio::test]
    async fn settings_that_write_outside_the_repo_are_refused_before_any_request() {
        let mut server = mockito::Server::new_async().await;
        let mut any = Vec::new();
        for method in ["GET", "POST", "PATCH", "PUT", "DELETE"] {
            any.push(server.mock(method, mockito::Matcher::Any).expect(0).create_async().await);
        }
        for (key, writes) in [
            ("armored_public_key", "POST /user/gpg_keys"),
            ("default_repository_permission", "PATCH /orgs/{org}"),
            ("actions", "PATCH /repos/{owner}/{repo}/check-runs/{check_run_id}"),
        ] {
            let config = write_file(&format!(
                "org: o\ndefault_webhook: {{url: https://h.example/x, content_type: json, events: [push]}}\nrepos:\n- name: r\n  settings:\n    {key}: x\n"
            ));
            let path = config.path().to_str().expect("utf-8 path");
            for dry_run in [true, false] {
                let err = client(&server).sync(path, dry_run).await.expect_err(&format!("accepted `{key}` under a repo's settings"));
                let err = err.to_string();
                for part in [path, "repos[0].settings", key, writes] {
                    assert!(err.contains(part), "error for {key} doesn't name {part}: {err}");
                }
            }
        }
        for mock in any {
            mock.assert_async().await;
        }
    }

    #[tokio::test]
    async fn a_next_page_on_another_host_is_refused() {
        let mut server = mockito::Server::new_async().await;
        let _page = server
            .mock("GET", "/repos/o/r/hooks?per_page=100")
            .with_header("link", r#"<https://elsewhere.example/repos/o/r/hooks?page=2>; rel="next""#)
            .with_body("[]")
            .create_async()
            .await;
        let err = client(&server).get_webhooks("r").await.expect_err("followed a Link to another host");
        assert!(err.to_string().contains("Refusing next page https://elsewhere.example/"), "{err}");
    }

    #[tokio::test]
    async fn team_permission_is_read_from_the_repository_media_type_never_guessed() {
        let mut server = mockito::Server::new_async().await;
        let check = |repo: &str| format!("/orgs/o/teams/devs/repos/o/{repo}");
        let repository = "application/vnd.github.v3.repository+json";
        let perms = |p: &str| format!(r#"{{"name":"x","role_name":"x","permissions":{p}}}"#);
        let mut mocks = Vec::new();
        for (repo, body) in [
            ("read", Some(perms(r#"{"admin":false,"maintain":false,"push":false,"triage":false,"pull":true}"#))),
            ("maintained", Some(perms(r#"{"admin":false,"maintain":true,"push":true,"triage":true,"pull":true}"#))),
            ("triaged", Some(perms(r#"{"admin":false,"maintain":false,"push":false,"triage":true,"pull":true}"#))),
            ("empty", None),
        ] {
            // Like GitHub: without the repository media type the check is a bare 204.
            mocks.push(server.mock("GET", check(repo).as_str()).match_header("accept", "application/vnd.github+json").with_status(204).create_async().await);
            let with_type = server.mock("GET", check(repo).as_str()).match_header("accept", repository);
            mocks.push(match body {
                Some(body) => with_type.with_body(body),
                None => with_type.with_status(204),
            }.create_async().await);
        }
        let client = client(&server);
        for (repo, want) in [("read", "pull"), ("maintained", "maintain"), ("triaged", "triage")] {
            let got = client.get_team_repo_permission("devs", repo).await.expect("permission");
            assert_eq!(got.as_deref(), Some(want), "team permission on {repo}");
        }
        let err = client.get_team_repo_permission("devs", "empty").await.expect_err("guessed a permission from an empty body");
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[tokio::test]
    async fn diff_round_trips_archived() {
        let mut server = mockito::Server::new_async().await;
        let _mocks = mock_org(&mut server, &[("live", false), ("old", true)]).await;
        let out = write_file("org: o\n");
        let path = out.path().to_str().expect("utf-8 path");
        let client = client(&server);
        client.generate_config_and_write(path, false).await.expect("sync-from-org");
        let generated = std::fs::read_to_string(path).expect("read output");
        assert!(!client.diff(path).await.expect("diff"), "sync-from-org output differs from the org it came from");

        let live = repo_block(&generated, "live");
        let explicit = generated.replace(live, &live.replace("    allow_squash_merge: true\n", "    allow_squash_merge: true\n    archived: false\n"));
        std::fs::write(path, &explicit).expect("write config");
        assert!(!client.diff(path).await.expect("diff"), "`archived: false` on an unarchived repo is a difference:\n{explicit}");

        std::fs::write(path, generated.replace("    archived: true\n", "")).expect("write config");
        assert!(client.diff(path).await.expect("diff"), "an archived repo whose config leaves out `archived` is not shown, though sync-from-org would add it");

        std::fs::write(path, generated.replace("    archived: true\n", "    archived: false\n")).expect("write config");
        assert!(client.diff(path).await.expect("diff"), "an unarchive is not shown as a difference");

        std::fs::write(path, explicit.replace("    archived: false\n", "    archived: true\n")).expect("write config");
        assert!(client.diff(path).await.expect("diff"), "an archive is not shown as a difference");
    }

    #[tokio::test]
    async fn only_repos_that_opt_in_get_the_default_webhook() {
        let mut server = mockito::Server::new_async().await;
        let _mocks = mock_org(&mut server, &[("listed", false), ("joined", false)]).await;
        let listed = server.mock("POST", "/repos/o/listed/hooks").expect(0).create_async().await;
        let joined = server.mock("POST", "/repos/o/joined/hooks").with_status(201).with_body("{}").expect(1).create_async().await;
        let default = "org: o\ndefault_webhook: {url: https://hooks.example/new, content_type: json, events: [push]}\nrepos:\n- name: listed\n";

        // Listing a repo connects nothing, and having no default isn't an error.
        let config = write_file(default);
        client(&server).sync(config.path().to_str().expect("utf-8 path"), false).await.expect("sync");
        listed.assert_async().await;
        let config = write_file("org: o\nrepos:\n- name: listed\n");
        client(&server).sync(config.path().to_str().expect("utf-8 path"), false).await.expect("sync without a default");

        let config = write_file(&format!("{default}- name: joined\n  use_default_webhook: true\n"));
        let path = config.path().to_str().expect("utf-8 path");
        client(&server).sync(path, true).await.expect("dry run");
        client(&server).sync(path, false).await.expect("sync");
        listed.assert_async().await;
        joined.assert_async().await;
    }

    #[tokio::test]
    async fn opting_in_to_a_missing_default_or_alongside_a_webhook_is_refused() {
        let server = mockito::Server::new_async().await;
        let hook = "{url: https://hooks.example/h, content_type: json, events: [push]}";
        for yaml in [
            "org: o\nrepos:\n- name: r\n  use_default_webhook: true\n".to_string(),
            format!("org: o\ndefault_webhook: {hook}\nrepos:\n- name: r\n  use_default_webhook: true\n  webhook: {hook}\n"),
        ] {
            let config = write_file(&yaml);
            let err = client(&server).sync(config.path().to_str().expect("utf-8 path"), true).await.expect_err("accepted a contradictory opt-in");
            assert!(err.to_string().contains("repos[0]"), "error doesn't say where: {err}");
        }
    }

    #[tokio::test]
    async fn sync_from_org_round_trips_who_is_on_the_default_webhook() {
        let mut server = mockito::Server::new_async().await;
        let hook = |url: &str| json!({"id": 1, "url": "u", "config": {"url": url, "content_type": "json"}, "events": ["push", "issues"]});
        // `first` makes another hook the first one found in the org.
        let hooks = |repo: &str| match repo {
            "first" => json!([hook("https://hooks.example/other")]),
            "off" => json!([]),
            "both" => json!([hook("https://hooks.example/other"), hook("https://hooks.example/h")]),
            _ => json!([hook("https://hooks.example/h")]),
        };
        let _mocks = mock_org_with_hooks(&mut server, &[("first", false), ("off", false), ("both", false), ("on", false)], hooks).await;
        let config = write_file("org: o\ndefault_webhook:\n  url: https://hooks.example/h\n  content_type: json\n  events: [push, issues]\n");
        let path = config.path().to_str().expect("utf-8 path");

        client(&server).generate_config_and_write(path, false).await.expect("generate");
        let yaml = std::fs::read_to_string(path).expect("read generated config");
        let generated = crate::config::Config::from_file_with_defaults(path, None).expect("generated config parses");
        assert_eq!(generated.default_webhook.expect("default kept").url, "https://hooks.example/h");
        for repo in ["on", "both"] {
            assert!(repo_block(&yaml, repo).contains("use_default_webhook: true"), "{yaml}");
        }
        for repo in ["first", "off"] {
            assert!(!repo_block(&yaml, repo).contains("use_default_webhook"), "regenerating put {repo} on the default webhook:\n{yaml}");
        }
        assert!(repo_block(&yaml, "first").contains("https://hooks.example/other"), "{yaml}");
        assert!(!repo_block(&yaml, "off").contains("webhook"), "{yaml}");
        assert!(!client(&server).diff(path).await.expect("diff"), "the regenerated config differs from GitHub");
    }

    #[tokio::test]
    async fn the_webhook_recorded_for_a_repo_is_the_same_whatever_the_file_holds_and_github_returns() {
        let hook = |url: &str| json!({"id": 1, "url": "u", "config": {"url": url, "content_type": "json"}, "events": ["push"]});
        let (c, d) = (hook("https://hooks.example/c"), hook("https://hooks.example/d"));
        let default = "default_webhook:\n  url: https://hooks.example/d\n  content_type: json\n  events: [push]\n";
        let own = "repos:\n- name: b\n  webhook:\n    url: https://hooks.example/c\n    content_type: json\n    events: [push]\n";
        for file in [None, Some("org: o\n".to_string()), Some(format!("org: o\n{default}")), Some(format!("org: o\n{default}{own}"))] {
            for b_hooks in [json!([c, d]), json!([d, c])] {
                for repos in [[("a", false), ("b", false)], [("b", false), ("a", false)]] {
                    let mut server = mockito::Server::new_async().await;
                    let _mocks = mock_org_with_hooks(&mut server, &repos, |repo| if repo == "a" { json!([d]) } else { b_hooks.clone() }).await;
                    let dir = tempfile::tempdir().expect("create temp dir");
                    let path = dir.path().join("config.yaml");
                    if let Some(file) = &file {
                        std::fs::write(&path, file).expect("write config");
                    }
                    let path = path.to_str().expect("utf-8 path");

                    client(&server).generate_config_and_write(path, false).await.expect("export");
                    let first = std::fs::read_to_string(path).expect("read export");
                    assert!(!client(&server).diff(path).await.expect("diff"), "false difference from {file:?} with {b_hooks} and {repos:?}:\n{first}");
                    client(&server).generate_config_and_write(path, false).await.expect("second export");
                    assert_eq!(first, std::fs::read_to_string(path).expect("read export"), "a second export changed the file");
                    if file.as_deref().is_some_and(|f| f.contains("- name: b")) {
                        assert!(repo_block(&first, "b").contains("https://hooks.example/c"), "b lost its own webhook:\n{first}");
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn webhooks_that_cant_be_read_stay_in_the_export_and_are_never_a_clean_diff() {
        let mut server = mockito::Server::new_async().await;
        // Not a list, so reading the hooks fails.
        let _mocks = mock_org_with_hooks(&mut server, &[("r", false)], |_| json!(null)).await;
        let file = write_file("org: o\ndefault_webhook:\n  url: https://hooks.example/h\n  content_type: json\n  events: [push]\nrepos:\n- name: r\n  use_default_webhook: true\n");
        let path = file.path().to_str().expect("utf-8 path");
        client(&server).generate_config_and_write(path, false).await.expect("export");
        let yaml = std::fs::read_to_string(path).expect("read export");
        assert!(repo_block(&yaml, "r").contains("use_default_webhook: true"), "{yaml}");
        assert!(client(&server).diff(path).await.expect("diff"), "diff called webhooks it never read a match");
    }

    #[tokio::test]
    async fn sync_from_org_keeps_nothing_from_another_orgs_file() {
        let mut server = mockito::Server::new_async().await;
        let _mocks = mock_org(&mut server, &[("r", false)]).await;
        let file = write_file("org: other\ndefault_webhook:\n  url: https://other.example/h\n  content_type: json\n  events: [push]\n");
        let path = file.path().to_str().expect("utf-8 path");
        client(&server).generate_config_and_write(path, false).await.expect("export");
        assert!(!std::fs::read_to_string(path).expect("read export").contains("other.example"));
    }

    #[test]
    fn a_dry_run_line_names_the_repo_and_keeps_the_webhook_url_out() {
        let webhook = WebhookConfig { url: "https://hooks.example/api/webhooks/1/token".into(), content_type: "json".into(), events: vec!["push".into(), "issues".into()] };
        let line = webhook_change("o", "r", None, &webhook).expect("a missing webhook is a change");
        for part in ["create", "o/r", "hooks.example", "push"] {
            assert!(line.contains(part), "{line} doesn't name {part}");
        }
        assert!(!line.contains("token"), "the URL's path reached the output: {line}");

        let same = WebhookResponse {
            id: 1,
            url: "u".into(),
            config: WebhookConfigResponse { url: webhook.url.clone(), content_type: "json".into() },
            events: vec!["issues".into(), "push".into()],
        };
        assert_eq!(webhook_change("o", "r", Some(&same), &webhook), None, "the order of events isn't a change");
        let changed = WebhookResponse { events: vec!["push".into()], ..same };
        let line = webhook_change("o", "r", Some(&changed), &webhook).expect("other events are a change");
        assert!(line.contains("hooks.example") && !line.contains("token"), "{line}");
    }

    #[tokio::test]
    async fn sync_unarchives_first_archives_last_and_leaves_archived_repo_settings_alone() {
        let mut server = mockito::Server::new_async().await;
        // kept stays archived, going gets archived, back gets unarchived; hidden is archived and only has team access.
        let _mocks = mock_org(&mut server, &[("kept", true), ("going", false), ("back", true), ("hidden", true)]).await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut write_mocks = Vec::new();
        for (method, path) in [
            ("PATCH", "/repos/o/going"),
            ("PATCH", "/repos/o/back"),
            ("PUT", "/orgs/o/teams/devs/repos/o/going"),
            ("PUT", "/orgs/o/teams/devs/repos/o/kept"),
            ("PUT", "/orgs/o/teams/devs/repos/o/hidden"),
        ] {
            let writes = writes.clone();
            let label = format!("{method} {path}");
            write_mocks.push(
                server
                    .mock(method, path)
                    .with_body_from_request(move |req| {
                        let body = req.utf8_lossy_body().expect("request body").to_string();
                        writes.lock().expect("lock").push(format!("{label} {body}"));
                        b"{}".to_vec()
                    })
                    .create_async()
                    .await,
            );
        }
        // Nothing else is mocked: any settings or webhook write to `kept` gets mockito's 501 and fails the sync.
        let mut no_access = Vec::new();
        for repo in ["going", "kept", "hidden"] {
            no_access.push(
                server
                    .mock("GET", format!("/orgs/o/teams/devs/repos/o/{repo}").as_str())
                    .with_status(404)
                    .with_body(r#"{"message":"Not Found","status":"404"}"#)
                    .create_async()
                    .await,
            );
        }
        let config = write_file(
            "org: o
default_webhook:
  url: https://hooks.example/h
  content_type: json
  events: [push]
repos:
- name: kept
  settings: {allow_merge_commit: true}
  webhook: {url: https://hooks.example/other, content_type: json, events: [push]}
- name: going
  settings: {allow_merge_commit: true, archived: true}
- name: back
  settings: {allow_merge_commit: true, archived: false}
assignments:
- {repo: going, team: devs, permission: push}
- {repo: kept, team: devs, permission: push}
- {repo: hidden, team: devs, permission: push}
",
        );
        let path = config.path().to_str().expect("utf-8 path");
        let text = std::fs::read_to_string(path).expect("read config");
        let bad = write_file(&text.replace("archived: false}", "archived: yes}"));
        let bad = bad.path().to_str().expect("utf-8 path");
        assert!(client(&server).sync(bad, false).await.is_err(), "a non-boolean `archived` was accepted");
        assert!(client(&server).diff(bad).await.is_err(), "diff accepted a non-boolean `archived`");
        assert!(writes.lock().expect("lock").is_empty(), "sync wrote to GitHub before rejecting the config");
        client(&server).sync(path, true).await.expect("dry run");
        assert!(writes.lock().expect("lock").is_empty(), "a dry run wrote to GitHub");
        client(&server).sync(path, false).await.expect("sync");
        assert_eq!(
            *writes.lock().expect("lock"),
            [
                r#"PATCH /repos/o/going {"allow_merge_commit":true}"#,
                r#"PATCH /repos/o/back {"archived":false}"#,
                r#"PATCH /repos/o/back {"allow_merge_commit":true}"#,
                r#"PUT /orgs/o/teams/devs/repos/o/going {"permission":"push"}"#,
                // Team access isn't skipped on archived repos: GitHub decides, so sync and diff agree.
                r#"PUT /orgs/o/teams/devs/repos/o/kept {"permission":"push"}"#,
                r#"PUT /orgs/o/teams/devs/repos/o/hidden {"permission":"push"}"#,
                r#"PATCH /repos/o/going {"archived":true}"#,
            ]
        );
    }


    #[tokio::test]
    async fn sync_creates_a_missing_repo_and_a_dry_run_only_says_so() {
        let mut server = mockito::Server::new_async().await;
        // Missing for the dry run and for the real run's first read; there once created.
        let _missing = server.mock("GET", "/repos/o/new").with_status(404).with_body(r#"{"message":"Not Found"}"#).expect(2).create_async().await;
        let _created = server.mock("GET", "/repos/o/new")
            .with_body(json!({"name": "new", "private": true, "visibility": "private", "allow_auto_merge": false}).to_string())
            .create_async().await;
        let _nameless = server.mock("GET", "/repos/o/nameless").with_status(404).with_body(r#"{"message":"Not Found"}"#).create_async().await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let _post = record(&mut server, &writes, "POST", "/orgs/o/repos").await;
        let _patch = record(&mut server, &writes, "PATCH", "/repos/o/new").await;
        let config = write_file("org: o\nrepos:\n- name: new\n  visibility: private\n  settings: {allow_auto_merge: true}\n");
        let path = config.path().to_str().expect("utf-8 path");

        client(&server).sync(path, true).await.expect("dry run");
        assert!(writes.lock().expect("lock").is_empty(), "a dry run wrote to GitHub");
        client(&server).sync(path, false).await.expect("sync");
        assert_eq!(
            *writes.lock().expect("lock"),
            [r#"POST /orgs/o/repos {"name":"new","visibility":"private"}"#, r#"PATCH /repos/o/new {"allow_auto_merge":true}"#]
        );

        // Without a visibility nothing is created: guessing could publish a private repo.
        let config = write_file("org: o\nrepos:\n- name: nameless\n");
        let err = client(&server).sync(config.path().to_str().expect("utf-8 path"), false).await.expect_err("created a repo with no visibility");
        assert!(err.to_string().contains("nameless") && err.to_string().contains("visibility"), "{err}");
        assert_eq!(writes.lock().expect("lock").len(), 2, "a repo with no visibility was created");
    }

    #[tokio::test]
    async fn diff_round_trips_auto_merge_and_access_level() {
        let mut server = mockito::Server::new_async().await;
        let private = |auto: bool, level: &str| json!({"private": true, "visibility": "private", "allow_auto_merge": auto, ACCESS_LEVEL: level});
        let repos = [("auto", private(true, "organization")), ("plain", private(false, "none")), ("pub", json!({"allow_auto_merge": false}))];
        let _mocks = mock_org_full(&mut server, &repos, |_| json!([]), |_| Some(vec![])).await;
        // A public repo has no access level to read.
        let _pub_access = server.mock("GET", "/repos/o/pub/actions/permissions/access").expect(0).create_async().await;
        let file = write_file("org: o\n");
        let path = file.path().to_str().expect("utf-8 path");
        let client = client(&server);
        client.generate_config_and_write(path, false).await.expect("sync-from-org");
        let generated = std::fs::read_to_string(path).expect("read output");
        let auto = repo_block(&generated, "auto");
        assert!(auto.contains("    allow_auto_merge: true\n") && auto.contains("    access_level: organization\n"), "{generated}");
        for repo in ["plain", "pub"] {
            let block = repo_block(&generated, repo);
            assert!(!block.contains(ALLOW_AUTO_MERGE) && !block.contains(ACCESS_LEVEL), "a default was written:\n{generated}");
        }
        assert!(!client.diff(path).await.expect("diff"), "sync-from-org output differs from the org it came from:\n{generated}");

        let plain = repo_block(&generated, "plain");
        let explicit = generated.replace(plain, &plain.replace("    allow_squash_merge: true\n", "    allow_squash_merge: true\n    allow_auto_merge: false\n    access_level: none\n"));
        std::fs::write(path, &explicit).expect("write config");
        assert!(!client.diff(path).await.expect("diff"), "naming the defaults is a difference:\n{explicit}");

        for changed in [generated.replace("    allow_auto_merge: true\n", ""), generated.replace("access_level: organization", "access_level: none")] {
            std::fs::write(path, &changed).expect("write config");
            assert!(client.diff(path).await.expect("diff"), "a change is not shown:\n{changed}");
        }
        _pub_access.assert_async().await;
    }

    #[tokio::test]
    async fn sync_reads_the_access_level_and_writes_it_only_when_it_differs() {
        let mut server = mockito::Server::new_async().await;
        let private = |level: &str| json!({"private": true, "visibility": "private", ACCESS_LEVEL: level});
        let repos = [
            ("closed", private("none")),
            ("open", private("organization")),
            ("unnamed", private("none")),
            ("broken", json!({"private": true, "visibility": "private"})),
            ("pub", json!({})),
        ];
        let _mocks = mock_org_full(&mut server, &repos, |_| json!([]), |_| Some(vec![])).await;
        // An existing repo whose access level can't be read, e.g. with Actions off.
        let _broken = server.mock("GET", "/repos/o/broken/actions/permissions/access").with_status(404).with_body(r#"{"message":"Not Found"}"#).create_async().await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut puts = Vec::new();
        for repo in ["closed", "open"] {
            puts.push(record(&mut server, &writes, "PUT", &format!("/repos/o/{repo}/actions/permissions/access")).await);
        }
        let config = write_file(
            "org: o\nrepos:\n- name: closed\n  settings: {access_level: organization}\n- name: open\n  settings: {access_level: organization}\n- name: unnamed\n  settings: {allow_squash_merge: true}\n",
        );
        client(&server).sync(config.path().to_str().expect("utf-8 path"), false).await.expect("sync");
        assert_eq!(*writes.lock().expect("lock"), [r#"PUT /repos/o/closed/actions/permissions/access {"access_level":"organization"}"#]);

        // Neither is taken for a missing repo (no create is mocked), and nothing is written, dry run or not. `pub`
        // stays public: top-level `visibility` is used only to create a repo, so with or without it nothing makes
        // `pub` private.
        let cases = [("broken", "\n  visibility: private", "access level"), ("pub", "\n  visibility: private", "public"), ("pub", "", "public")];
        for ((repo, visibility, says), dry_run) in cases.into_iter().flat_map(|case| [(case, true), (case, false)]) {
            let config = write_file(&format!("org: o\nrepos:\n- name: {repo}{visibility}\n  settings: {{access_level: organization}}\n"));
            let err = client(&server).sync(config.path().to_str().expect("utf-8 path"), dry_run).await.expect_err("accepted");
            assert!(err.to_string().contains(repo) && err.to_string().contains(says), "{err}");
        }
        assert_eq!(writes.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn an_access_level_that_cant_be_read_keeps_the_files_and_the_other_settings() {
        let mut server = mockito::Server::new_async().await;
        let _mocks = mock_org_full(&mut server, &[("r", json!({"private": true, "visibility": "private"}))], |_| json!([]), |_| Some(vec![])).await;
        let _denied = server.mock("GET", "/repos/o/r/actions/permissions/access").with_status(403).with_body(r#"{"message":"Forbidden"}"#).create_async().await;
        let file = write_file("org: o\nrepos:\n- name: r\n  settings: {access_level: organization}\n");
        let path = file.path().to_str().expect("utf-8 path");
        client(&server).generate_config_and_write(path, false).await.expect("export");
        let yaml = std::fs::read_to_string(path).expect("read export");
        let block = repo_block(&yaml, "r");
        assert!(block.contains("    access_level: organization\n") && block.contains("    allow_squash_merge: true\n"), "{yaml}");
        assert!(client(&server).diff(path).await.expect("diff"), "diff called an access level it never read a match");
    }

    #[tokio::test]
    async fn sync_keeps_require_ci_to_the_configured_checks() {
        let mut server = mockito::Server::new_async().await;
        let mut adopted = require_ci(7, &["A", "B"]);
        adopted["conditions"]["ref_name"]["include"] = json!(["refs/heads/main"]);
        let mut mixed = require_ci(9, &["A"]);
        mixed["rules"].as_array_mut().expect("rules").push(json!({"type": "pull_request", "parameters": {}}));
        let repos = ["fresh", "adopt", "same", "mixed", "untouched"].map(|n| (n, json!({})));
        // `untouched` names no checks, so its rulesets are never read: they aren't mocked.
        let _mocks = mock_org_full(&mut server, &repos, |_| json!([]), |name| match name {
            "adopt" => Some(vec![adopted.clone()]),
            "same" => Some(vec![json!({"id": 1, "name": "Other"}), require_ci(8, &["B", "A"])]),
            "mixed" => Some(vec![mixed.clone()]),
            "untouched" => None,
            _ => Some(vec![]),
        }).await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let _post = record(&mut server, &writes, "POST", "/repos/o/fresh/rulesets").await;
        let _put = record(&mut server, &writes, "PUT", "/repos/o/adopt/rulesets/7").await;
        let config = write_file(
            "org: o\nrepos:\n- name: fresh\n  required_checks: [A, Clippy & Docs]\n- name: adopt\n  required_checks: [A, B]\n- name: same\n  required_checks: [A, B]\n- name: untouched\n",
        );
        let path = config.path().to_str().expect("utf-8 path");
        client(&server).sync(path, true).await.expect("dry run");
        assert!(writes.lock().expect("lock").is_empty(), "a dry run wrote to GitHub");
        client(&server).sync(path, false).await.expect("sync");
        let body = |checks: &[&str]| require_ci_body(&unpinned(checks), &[]).to_string();
        assert_eq!(
            *writes.lock().expect("lock"),
            [format!("POST /repos/o/fresh/rulesets {}", body(&["A", "Clippy & Docs"])), format!("PUT /repos/o/adopt/rulesets/7 {}", body(&["A", "B"]))]
        );

        let config = write_file("org: o\nrepos:\n- name: mixed\n  required_checks: [A]\n");
        let err = client(&server).sync(config.path().to_str().expect("utf-8 path"), false).await.expect_err("dropped a rule");
        assert!(err.to_string().contains("mixed") && err.to_string().contains("pull_request"), "{err}");
        assert_eq!(writes.lock().expect("lock").len(), 2, "a ruleset with another rule was rewritten");
    }

    #[tokio::test]
    async fn sync_from_org_records_required_checks_in_the_files_order() {
        let mut server = mockito::Server::new_async().await;
        let repos = ["gated", "open"].map(|n| (n, json!({})));
        let _mocks = mock_org_full(&mut server, &repos, |_| json!([]), |name| {
            Some(if name == "gated" { vec![require_ci(3, &["A", "Clippy & Docs"])] } else { vec![] })
        }).await;
        let file = write_file("org: o\nrepos:\n- name: gated\n  required_checks: [Clippy & Docs, A]\n");
        let path = file.path().to_str().expect("utf-8 path");
        let client = client(&server);
        client.generate_config_and_write(path, false).await.expect("sync-from-org");
        let generated = std::fs::read_to_string(path).expect("read output");
        assert!(repo_block(&generated, "gated").contains("  required_checks:\n  - Clippy & Docs\n  - A"), "{generated}");
        assert!(!repo_block(&generated, "open").contains("required_checks"), "{generated}");
        assert!(!client.diff(path).await.expect("diff"), "sync-from-org output differs from the org it came from:\n{generated}");
        std::fs::write(path, generated.replace("  - A\n", "")).expect("write config");
        assert!(client.diff(path).await.expect("diff"), "a removed check is not shown");
    }


    #[tokio::test]
    async fn visibility_changes_are_checked_up_front_and_ordered_before_the_access_level() {
        let mut server = mockito::Server::new_async().await;
        let private = |level: &str| json!({"private": true, "visibility": "private", ACCESS_LEVEL: level});
        // `by_private` and `by_visibility` are public, so they have no access level to read: none is mocked.
        let repos = [("by_private", json!({})), ("by_visibility", json!({})), ("closing", private("organization")), ("quiet", private("none"))];
        let _mocks = mock_org_full(&mut server, &repos, |_| json!([]), |_| Some(vec![])).await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut write_mocks = Vec::new();
        for (method, path) in [
            ("PATCH", "/repos/o/by_private"),
            ("PUT", "/repos/o/by_private/actions/permissions/access"),
            ("PATCH", "/repos/o/by_visibility"),
            ("PUT", "/repos/o/by_visibility/actions/permissions/access"),
            ("PATCH", "/repos/o/closing"),
            ("PATCH", "/repos/o/quiet"),
        ] {
            write_mocks.push(record(&mut server, &writes, method, path).await);
        }
        let opening = "- name: by_private\n  settings: {private: true, access_level: organization}\n";

        // `closing` is private with an access level. Settings that make it public while granting access, or that
        // contradict each other, are refused before any repo is touched, the one listed before it included.
        for bad in ["{private: false, access_level: organization}", "{visibility: public, access_level: organization}", "{visibility: public, private: true}"] {
            let config = write_file(&format!("org: o\nrepos:\n{opening}- name: closing\n  settings: {bad}\n"));
            for dry_run in [true, false] {
                let err = client(&server).sync(config.path().to_str().expect("utf-8 path"), dry_run).await.expect_err(bad);
                assert!(err.to_string().contains("closing"), "{err}");
            }
        }
        assert!(writes.lock().expect("lock").is_empty(), "wrote before refusing the config");

        // Going private by either setting: the visibility PATCH, then the access level the private repo can now have.
        // Going public with access `none` writes nothing for the access level.
        let good = write_file(&format!(
            "org: o\nrepos:\n{opening}- name: by_visibility\n  settings: {{visibility: private, access_level: user}}\n- name: quiet\n  settings: {{private: false, access_level: none}}\n"
        ));
        client(&server).sync(good.path().to_str().expect("utf-8 path"), false).await.expect("sync");
        assert_eq!(
            *writes.lock().expect("lock"),
            [
                r#"PATCH /repos/o/by_private {"private":true}"#,
                r#"PUT /repos/o/by_private/actions/permissions/access {"access_level":"organization"}"#,
                r#"PATCH /repos/o/by_visibility {"visibility":"private"}"#,
                r#"PUT /repos/o/by_visibility/actions/permissions/access {"access_level":"user"}"#,
                r#"PATCH /repos/o/quiet {"private":false}"#,
            ]
        );
    }

    #[tokio::test]
    async fn a_ruleset_requiring_one_check_from_two_apps_is_refused_and_never_exported() {
        let mut server = mockito::Server::new_async().await;
        let _mocks = mock_org_full(&mut server, &[("r", json!({}))], |_| json!([]), |_| None).await;
        let _list = server.mock("GET", "/repos/o/r/rulesets?includes_parents=false&per_page=100")
            .with_body(r#"[{"id": 6, "name": "Require CI"}]"#).create_async().await;
        let _detail = server.mock("GET", "/repos/o/r/rulesets/6").with_body(r#"{
            "id": 6, "name": "Require CI", "target": "branch", "enforcement": "active",
            "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
            "rules": [{"type": "required_status_checks", "parameters": {
                "strict_required_status_checks_policy": false, "do_not_enforce_on_create": false,
                "required_status_checks": [{"context": "Unit Tests", "integration_id": 1}, {"context": "Unit Tests", "integration_id": 2}]}}]
        }"#).create_async().await;
        let file = write_file("org: o\nrepos:\n- name: r\n  required_checks: [Unit Tests]\n");
        let path = file.path().to_str().expect("utf-8 path");
        let err = client(&server).sync(path, true).await.expect_err("accepted");
        assert!(err.to_string().contains("`Unit Tests` twice"), "{err}");
        client(&server).generate_config_and_write(path, false).await.expect("export");
        let yaml = std::fs::read_to_string(path).expect("read export");
        assert!(repo_block(&yaml, "r").contains("  required_checks:\n  - Unit Tests") && !yaml.contains("Unit Tests\n  - Unit Tests"), "{yaml}");
        assert!(client(&server).diff(path).await.expect("diff"), "a ruleset that can't be represented was called a match");
    }

    #[tokio::test]
    async fn auto_merge_goes_on_only_after_its_ruleset_is_written() {
        let mut server = mockito::Server::new_async().await;
        let _mocks = mock_org_full(&mut server, &[("r", json!({}))], |_| json!([]), |_| Some(vec![])).await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let _patch = record(&mut server, &writes, "PATCH", "/repos/o/r").await;
        let refused = {
            let writes = writes.clone();
            server.mock("POST", "/repos/o/r/rulesets").with_status(422)
                .with_body_from_request(move |_| { writes.lock().expect("lock").push("POST refused".into()); b"{}".to_vec() })
                .expect(1).create_async().await
        };
        let config = write_file("org: o\nrepos:\n- name: r\n  settings: {allow_auto_merge: true, allow_merge_commit: true}\n  required_checks: [A]\n");
        let path = config.path().to_str().expect("utf-8 path");
        assert!(client(&server).sync(path, false).await.is_err(), "a refused ruleset write passed");
        assert_eq!(*writes.lock().expect("lock"), [r#"PATCH /repos/o/r {"allow_merge_commit":true}"#, "POST refused"]);
        refused.assert_async().await;

        let _post = record(&mut server, &writes, "POST", "/repos/o/r/rulesets").await;
        writes.lock().expect("lock").clear();
        client(&server).sync(path, false).await.expect("sync");
        let writes = writes.lock().expect("lock");
        assert_eq!(writes.len(), 3, "{writes:?}");
        assert!(writes[1].starts_with("POST /repos/o/r/rulesets ") && writes[2] == r#"PATCH /repos/o/r {"allow_auto_merge":true}"#, "{writes:?}");
    }

    #[tokio::test]
    async fn rewriting_require_ci_keeps_the_app_each_kept_check_is_pinned_to() {
        let mut server = mockito::Server::new_async().await;
        let _mocks = mock_org_full(&mut server, &[("r", json!({}))], |_| json!([]), |_| None).await;
        // As GitHub returns it, written out by hand.
        let held = r#"{
            "id": 42, "name": "Require CI", "target": "branch", "source_type": "Repository", "source": "o/r",
            "enforcement": "active", "bypass_actors": [],
            "conditions": {"ref_name": {"include": ["refs/heads/main"], "exclude": []}},
            "rules": [{"type": "required_status_checks", "parameters": {
                "strict_required_status_checks_policy": false, "do_not_enforce_on_create": false,
                "required_status_checks": [{"context": "Unit Tests", "integration_id": 15368}, {"context": "Old"}]}}],
            "node_id": "RRS_x", "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        }"#;
        let _list = server.mock("GET", "/repos/o/r/rulesets?includes_parents=false&per_page=100")
            .with_body(r#"[{"id": 42, "name": "Require CI"}]"#).create_async().await;
        let _detail = server.mock("GET", "/repos/o/r/rulesets/42").with_body(held).create_async().await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let _put = record(&mut server, &writes, "PUT", "/repos/o/r/rulesets/42").await;
        let config = write_file("org: o\nrepos:\n- name: r\n  required_checks: [Unit Tests, Lint]\n");
        client(&server).sync(config.path().to_str().expect("utf-8 path"), false).await.expect("sync");
        let writes = writes.lock().expect("lock");
        let [put] = writes.as_slice() else { panic!("{writes:?}") };
        let sent: serde_json::Value = serde_json::from_str(put.trim_start_matches("PUT /repos/o/r/rulesets/42 ")).expect("json body");
        let expected: serde_json::Value = serde_json::from_str(r#"{
            "name": "Require CI", "target": "branch", "enforcement": "active", "bypass_actors": [],
            "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
            "rules": [{"type": "required_status_checks", "parameters": {
                "strict_required_status_checks_policy": false, "do_not_enforce_on_create": false,
                "required_status_checks": [{"context": "Unit Tests", "integration_id": 15368}, {"context": "Lint"}]}}]
        }"#).expect("expected json");
        assert_eq!(sent, expected);
    }

    /// A `Require CI` ruleset requiring `A`, with `bypass_actors` as given (`None`: GitHub didn't show it).
    fn require_a_with(id: u64, bypass: Option<serde_json::Value>) -> serde_json::Value {
        let mut ruleset = require_ci(id, &["A"]);
        match bypass {
            Some(bypass) => ruleset["bypass_actors"] = bypass,
            None => drop(ruleset.as_object_mut().expect("object").remove("bypass_actors")),
        }
        ruleset
    }

    #[tokio::test]
    async fn an_adopted_ruleset_bypass_list_becomes_exactly_the_configs() {
        let mut server = mockito::Server::new_async().await;
        // Held: the core team always, the org's admins, an app. Wanted: the core team by PR only, nobody else.
        let held = json!([
            {"actor_id": 9, "actor_type": "Team", "bypass_mode": "always"},
            {"actor_id": 1, "actor_type": "OrganizationAdmin", "bypass_mode": "always"},
            {"actor_id": 77, "actor_type": "Integration", "bypass_mode": "pull_request"},
        ]);
        let ruleset = require_a_with(42, Some(held.clone()));
        let _mocks = mock_org_with_teams(&mut server, &[("core", 9), ("other", 10)], &[("r", json!({}))], |_| json!([]), |_| Some(vec![ruleset.clone()])).await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let _put = record(&mut server, &writes, "PUT", "/repos/o/r/rulesets/42").await;
        let file = write_file("org: o\nrepos:\n- name: r\n  required_checks: [A]\n  required_checks_bypass:\n  - team: core\n    mode: pull_request\n");
        let path = file.path().to_str().expect("utf-8 path");

        // What GitHub holds can't all be written in a config: diff says so instead of calling it a match.
        assert!(client(&server).diff(path).await.expect("diff"), "a bypass list a config can't name was called a match");
        client(&server).sync(path, true).await.expect("dry run");
        assert!(writes.lock().expect("lock").is_empty(), "a dry run wrote to GitHub");
        client(&server).sync(path, false).await.expect("sync");
        let writes = writes.lock().expect("lock");
        let [put] = writes.as_slice() else { panic!("{writes:?}") };
        let sent: serde_json::Value = serde_json::from_str(put.trim_start_matches("PUT /repos/o/r/rulesets/42 ")).expect("json body");
        assert_eq!(sent["bypass_actors"], json!([{"actor_id": 9, "actor_type": "Team", "bypass_mode": "pull_request"}]));
        assert_eq!(sent["rules"], ruleset["rules"], "the checks changed");
    }

    #[test]
    fn a_dry_run_names_who_may_bypass_and_who_no_longer_may() {
        let teams = [("core".to_string(), 9)];
        let core_pr = [BypassActor::team(9, BypassMode::PullRequest)];
        let held = [
            BypassActor::team(9, BypassMode::Always),
            BypassActor { actor_type: "OrganizationAdmin".into(), actor_id: Some(1), mode: "always".into() },
        ];
        assert_eq!(
            bypass_change(&core_pr, Some(&held), true, &teams),
            "bypass: core (pull_request); no longer bypassing: core (always), OrganizationAdmin #1 (always)"
        );
        assert_eq!(bypass_change(&[], None, false, &teams), "bypass: nobody");
        assert_eq!(bypass_change(&core_pr, None, true, &teams), "bypass: core (pull_request); its current bypass list can't be read with this token");
    }

    #[tokio::test]
    async fn a_teams_bypass_round_trips_and_a_matching_ruleset_is_not_rewritten() {
        let mut server = mockito::Server::new_async().await;
        let ruleset = require_a_with(5, Some(json!([
            {"actor_id": 10, "actor_type": "Team", "bypass_mode": "always"},
            {"actor_id": 9, "actor_type": "Team", "bypass_mode": "pull_request"},
        ])));
        let _mocks = mock_org_with_teams(&mut server, &[("core", 9), ("other", 10)], &[("r", json!({}))], |_| json!([]), |_| Some(vec![ruleset.clone()])).await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let _put = record(&mut server, &writes, "PUT", "/repos/o/r/rulesets/5").await;
        let file = write_file("org: o\nrepos:\n- name: r\n  required_checks: [A]\n");
        let path = file.path().to_str().expect("utf-8 path");
        client(&server).generate_config_and_write(path, false).await.expect("export");
        let yaml = std::fs::read_to_string(path).expect("read export");
        assert!(
            repo_block(&yaml, "r").contains("  required_checks_bypass:\n  - team: core\n    mode: pull_request\n  - team: other\n    mode: always\n"),
            "{yaml}"
        );
        assert!(!client(&server).diff(path).await.expect("diff"), "the exported config doesn't match what it was read from");
        client(&server).sync(path, false).await.expect("sync");
        assert!(writes.lock().expect("lock").is_empty(), "a matching ruleset was rewritten");
    }

    #[tokio::test]
    async fn a_bypass_list_the_token_cant_see_is_kept_from_the_file_rewritten_by_sync_and_never_a_clean_diff() {
        let mut server = mockito::Server::new_async().await;
        let ruleset = require_a_with(5, None);
        let _mocks = mock_org_with_teams(&mut server, &[("core", 9)], &[("r", json!({}))], |_| json!([]), |_| Some(vec![ruleset.clone()])).await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let _put = record(&mut server, &writes, "PUT", "/repos/o/r/rulesets/5").await;
        let file = write_file("org: o\nrepos:\n- name: r\n  required_checks: [A]\n  required_checks_bypass: [{team: core, mode: pull_request}]\n");
        let path = file.path().to_str().expect("utf-8 path");
        assert!(client(&server).diff(path).await.expect("diff"), "diff called a bypass list it never read a match");
        client(&server).generate_config_and_write(path, false).await.expect("export");
        let yaml = std::fs::read_to_string(path).expect("read export");
        assert!(repo_block(&yaml, "r").contains("  required_checks_bypass:\n  - team: core\n    mode: pull_request\n"), "{yaml}");
        client(&server).sync(path, false).await.expect("sync");
        assert_eq!(writes.lock().expect("lock").len(), 1, "an unseen bypass list was assumed to match");
    }

    #[tokio::test]
    async fn a_bypass_team_the_org_doesnt_have_is_refused_before_any_ruleset_write() {
        let mut server = mockito::Server::new_async().await;
        let _mocks = mock_org_with_teams(&mut server, &[("core", 9)], &[("r", json!({}))], |_| json!([]), |_| Some(vec![])).await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let _post = record(&mut server, &writes, "POST", "/repos/o/r/rulesets").await;
        let file = write_file("org: o\nrepos:\n- name: r\n  required_checks: [A]\n  required_checks_bypass: [{team: ghosts, mode: always}]\n");
        let err = client(&server).sync(file.path().to_str().expect("utf-8 path"), false).await.expect_err("accepted");
        assert!(err.to_string().contains("team `ghosts`"), "{err}");
        assert!(writes.lock().expect("lock").is_empty(), "wrote a ruleset for an unknown team");
    }

    /// Org `o` with repo `r` and no rulesets, where team `release` exists only once `POST /orgs/o/teams` was sent.
    /// Records every write.
    async fn mock_org_creating_release(server: &mut mockito::ServerGuard, writes: &std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> Vec<mockito::Mock> {
        let created = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let is_created = created.clone();
        let repo = json!({"name": "r", "private": false, "visibility": "public", "archived": false,
            "allow_merge_commit": false, "allow_squash_merge": true, "allow_rebase_merge": true});
        let teams = server.mock("GET", "/orgs/o/teams?per_page=100").with_body_from_request(move |_| {
            let list = if is_created.load(std::sync::atomic::Ordering::SeqCst) { r#"[{"slug": "release", "name": "release", "id": 31}]"# } else { "[]" };
            list.as_bytes().to_vec()
        });
        let create = {
            let writes = writes.clone();
            server.mock("POST", "/orgs/o/teams").with_body_from_request(move |req| {
                created.store(true, std::sync::atomic::Ordering::SeqCst);
                writes.lock().expect("lock").push(format!("POST /orgs/o/teams {}", req.utf8_lossy_body().expect("body")));
                b"{}".to_vec()
            })
        };
        vec![
            teams.create_async().await,
            create.create_async().await,
            server.mock("GET", "/orgs/o/teams/release").with_status(404).create_async().await,
            server.mock("GET", "/repos/o/r").with_body(repo.to_string()).create_async().await,
            server.mock("GET", "/repos/o/r/rulesets?includes_parents=false&per_page=100").with_body("[]").create_async().await,
            record(server, writes, "POST", "/repos/o/r/rulesets").await,
        ]
    }

    #[tokio::test]
    async fn a_bypass_team_the_same_config_adds_is_created_first_and_then_used() {
        let mut server = mockito::Server::new_async().await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let _mocks = mock_org_creating_release(&mut server, &writes).await;
        let file = write_file("org: o\nteams:\n- name: release\n  members: []\nrepos:\n- name: r\n  required_checks: [A]\n  required_checks_bypass: [{team: release, mode: pull_request}]\n");
        let path = file.path().to_str().expect("utf-8 path");
        client(&server).sync(path, true).await.expect("dry run");
        assert!(writes.lock().expect("lock").is_empty(), "a dry run wrote to GitHub");
        client(&server).sync(path, false).await.expect("sync");
        let writes = writes.lock().expect("lock");
        let [team, ruleset] = writes.as_slice() else { panic!("{writes:?}") };
        assert!(team.starts_with("POST /orgs/o/teams "), "{writes:?}");
        let sent: serde_json::Value = serde_json::from_str(ruleset.trim_start_matches("POST /repos/o/r/rulesets ")).expect("json body");
        assert_eq!(sent["bypass_actors"], json!([{"actor_id": 31, "actor_type": "Team", "bypass_mode": "pull_request"}]));
    }

    #[tokio::test]
    async fn a_bypass_team_nobody_creates_is_refused_before_any_write() {
        let mut server = mockito::Server::new_async().await;
        let _mocks = mock_org_with_teams(&mut server, &[("core", 9)], &[("a", json!({})), ("r", json!({}))], |_| json!([]), |_| Some(vec![])).await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let _patch = record(&mut server, &writes, "PATCH", "/repos/o/a").await;
        let _post = record(&mut server, &writes, "POST", "/repos/o/a/rulesets").await;
        // `a` comes first and is fine; `r` names a team neither the org nor the config has.
        let file = write_file("org: o\nrepos:\n- name: a\n  settings: {allow_merge_commit: true}\n  required_checks: [A]\n- name: r\n  required_checks: [A]\n  required_checks_bypass: [{team: ghosts, mode: always}]\n");
        let err = client(&server).sync(file.path().to_str().expect("utf-8 path"), false).await.expect_err("accepted");
        assert!(err.to_string().contains("team `ghosts`") && err.to_string().contains("Repo r"), "{err}");
        assert!(writes.lock().expect("lock").is_empty(), "wrote before refusing: {:?}", writes.lock().expect("lock"));
    }

    #[tokio::test]
    async fn a_ruleset_requiring_no_checks_exports_a_config_that_loads() {
        let mut server = mockito::Server::new_async().await;
        // Someone removed the checks rule but the bypass list stayed: a config can't hold a bypass without checks.
        let mut ruleset = require_a_with(5, Some(json!([{"actor_id": 9, "actor_type": "Team", "bypass_mode": "pull_request"}])));
        ruleset["rules"] = json!([]);
        let _mocks = mock_org_with_teams(&mut server, &[("core", 9)], &[("r", json!({}))], |_| json!([]), |_| Some(vec![ruleset.clone()])).await;
        let file = write_file("org: o\nrepos:\n- name: r\n  required_checks: [A]\n  required_checks_bypass: [{team: core, mode: pull_request}]\n");
        let path = file.path().to_str().expect("utf-8 path");
        client(&server).generate_config_and_write(path, false).await.expect("export");
        let config = Config::from_file_with_defaults(path, None).expect("the exported config doesn't load");
        let repo = config.repos.iter().find(|r| r.name == "r").expect("repo r");
        assert!(repo.required_checks.is_empty() && repo.required_checks_bypass.is_empty(), "{repo:?}");
    }

    fn require_review(id: u64, approvals: u32) -> serde_json::Value {
        let mut ruleset = require_review_body(approvals);
        ruleset["id"] = json!(id);
        ruleset
    }

    #[tokio::test]
    async fn required_approvals_creates_require_review_and_leaves_require_ci_alone() {
        let mut server = mockito::Server::new_async().await;
        let _mocks = mock_org_full(&mut server, &[("r", json!({}))], |_| json!([]), |_| Some(vec![])).await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let _post = record(&mut server, &writes, "POST", "/repos/o/r/rulesets").await;
        let file = write_file("org: o\nrepos:\n- name: r\n  required_approvals: 1\n");
        let path = file.path().to_str().expect("utf-8 path");
        client(&server).sync(path, true).await.expect("dry run");
        assert!(writes.lock().expect("lock").is_empty(), "a dry run wrote to GitHub");
        client(&server).sync(path, false).await.expect("sync");
        let writes = writes.lock().expect("lock");
        let [post] = writes.as_slice() else { panic!("{writes:?}") };
        let sent: serde_json::Value = serde_json::from_str(post.trim_start_matches("POST /repos/o/r/rulesets ")).expect("json body");
        assert_eq!(sent["name"], "Require review");
        assert_eq!(sent["rules"].as_array().map(Vec::len), Some(1));
        assert_eq!(sent["rules"][0]["type"], "pull_request");
        assert_eq!(sent["rules"][0]["parameters"]["required_approving_review_count"], 1);
        assert_eq!(sent["bypass_actors"], json!([]));
    }

    #[tokio::test]
    async fn required_approvals_round_trip_and_a_matching_ruleset_is_not_rewritten() {
        let mut server = mockito::Server::new_async().await;
        let _mocks = mock_org_full(&mut server, &[("r", json!({})), ("open", json!({}))], |_| json!([]), |name| {
            Some(if name == "r" { vec![json!({"id": 1, "name": "Other"}), require_review(4, 2)] } else { vec![] })
        }).await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let _put = record(&mut server, &writes, "PUT", "/repos/o/r/rulesets/4").await;
        let file = write_file("org: o\nrepos:\n- name: r\n");
        let path = file.path().to_str().expect("utf-8 path");
        client(&server).generate_config_and_write(path, false).await.expect("export");
        let yaml = std::fs::read_to_string(path).expect("read export");
        assert!(repo_block(&yaml, "r").contains("  required_approvals: 2\n"), "{yaml}");
        assert!(!repo_block(&yaml, "open").contains("required_approvals"), "{yaml}");
        assert!(!client(&server).diff(path).await.expect("diff"), "the exported config doesn't match what it was read from");
        client(&server).sync(path, false).await.expect("sync");
        assert!(writes.lock().expect("lock").is_empty(), "a matching ruleset was rewritten");
    }

    #[tokio::test]
    async fn an_adopted_require_review_is_rewritten_to_the_config_and_other_rules_are_refused() {
        let mut server = mockito::Server::new_async().await;
        let mut adopted = require_review(4, 0);
        adopted["rules"][0]["parameters"]["require_code_owner_review"] = json!(true);
        let mut mixed = require_review(5, 1);
        mixed["rules"].as_array_mut().expect("rules").push(json!({"type": "deletion"}));
        let _mocks = mock_org_full(&mut server, &[("adopt", json!({})), ("mixed", json!({}))], |_| json!([]), |name| {
            Some(vec![if name == "adopt" { adopted.clone() } else { mixed.clone() }])
        }).await;
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let _put = record(&mut server, &writes, "PUT", "/repos/o/adopt/rulesets/4").await;
        let file = write_file("org: o\nrepos:\n- name: adopt\n  required_approvals: 1\n");
        client(&server).sync(file.path().to_str().expect("utf-8 path"), false).await.expect("sync");
        assert_eq!(*writes.lock().expect("lock"), [format!("PUT /repos/o/adopt/rulesets/4 {}", require_review_body(1))]);
        let file = write_file("org: o\nrepos:\n- name: mixed\n  required_approvals: 1\n");
        let err = client(&server).sync(file.path().to_str().expect("utf-8 path"), false).await.expect_err("dropped a rule");
        assert!(err.to_string().contains("mixed") && err.to_string().contains("2 rules"), "{err}");
    }

    #[tokio::test]
    async fn required_approvals_that_cant_be_read_keep_the_files_and_are_never_a_clean_diff() {
        let mut server = mockito::Server::new_async().await;
        let _mocks = mock_org_full(&mut server, &[("r", json!({}))], |_| json!([]), |_| None).await;
        let _list = server.mock("GET", "/repos/o/r/rulesets?includes_parents=false&per_page=100")
            .with_body(r#"[{"id": 5, "name": "Require review"}]"#).create_async().await;
        let file = write_file("org: o\nrepos:\n- name: r\n  required_approvals: 1\n");
        let path = file.path().to_str().expect("utf-8 path");
        client(&server).generate_config_and_write(path, false).await.expect("export");
        assert!(repo_block(&std::fs::read_to_string(path).expect("read export"), "r").contains("  required_approvals: 1\n"));
        assert!(client(&server).diff(path).await.expect("diff"), "diff called approvals it never read a match");
    }

    #[tokio::test]
    async fn required_checks_that_cant_be_read_keep_the_files_and_are_never_a_clean_diff() {
        let mut server = mockito::Server::new_async().await;
        // `list` fails at the rulesets list; `detail` lists `Require CI` but its rules can't be read.
        let _mocks = mock_org_full(&mut server, &[("list", json!({})), ("detail", json!({}))], |_| json!([]), |_| None).await;
        let _detail = server.mock("GET", "/repos/o/detail/rulesets?includes_parents=false&per_page=100")
            .with_body(r#"[{"id": 5, "name": "Require CI"}]"#).create_async().await;
        let file = write_file("org: o\nrepos:\n- name: list\n  required_checks: [A]\n- name: detail\n  required_checks: [B]\n");
        let path = file.path().to_str().expect("utf-8 path");
        client(&server).generate_config_and_write(path, false).await.expect("export");
        let yaml = std::fs::read_to_string(path).expect("read export");
        assert!(repo_block(&yaml, "list").contains("  required_checks:\n  - A"), "{yaml}");
        assert!(repo_block(&yaml, "detail").contains("  required_checks:\n  - B"), "{yaml}");
        assert!(client(&server).diff(path).await.expect("diff"), "diff called checks it never read a match");
    }

    #[tokio::test]
    async fn matching_memberships_and_assignments_do_not_write() {
        let mut server = mockito::Server::new_async().await;
        let membership = server.mock("GET", "/orgs/o/memberships/alice")
            .with_body(r#"{"role":"admin"}"#).expect(2).create_async().await;
        let permission = server.mock("GET", "/orgs/o/teams/devs/repos/o/r")
            .match_header("accept", "application/vnd.github.v3.repository+json")
            .with_body(r#"{"name":"r","permissions":{"pull":true,"push":true,"admin":false}}"#)
            .expect(2).create_async().await;
        let writes = server.mock("PUT", mockito::Matcher::Any).expect(0).create_async().await;
        let client = client(&server);
        let user = User { login: "alice".into(), role: "admin".into() };
        let assignment = Assignment { repo: "r".into(), team: "devs".into(), permission: "push".into() };
        for dry_run in [true, false] {
            client.add_user_to_org(&user, dry_run).await.expect("membership");
            client.assign_team_to_repo(&assignment, dry_run).await.expect("assignment");
        }
        membership.assert_async().await;
        permission.assert_async().await;
        writes.assert_async().await;
    }

    #[tokio::test]
    async fn missing_or_changed_roles_and_permissions_are_applied_but_not_by_dry_runs() {
        for missing in [false, true] {
            let mut server = mockito::Server::new_async().await;
            let status = if missing { 404 } else { 200 };
            // GitHub's Not Found response need not repeat the numeric status in its body.
            let membership = server.mock("GET", "/orgs/o/memberships/alice").with_status(status)
                .with_body(if missing { r#"{"message":"Not Found"}"# } else { r#"{"role":"member"}"# })
                .expect(2).create_async().await;
            let permission = server.mock("GET", "/orgs/o/teams/devs/repos/o/r").with_status(status)
                .with_body(if missing { r#"{"message":"Not Found"}"# } else { r#"{"name":"r","permissions":{"pull":true,"push":false,"admin":false}}"# })
                .expect(2).create_async().await;
            let role_write = server.mock("PUT", "/orgs/o/memberships/alice")
                .match_body(mockito::Matcher::Json(json!({"role":"admin"})))
                .with_status(200).expect(1).create_async().await;
            let permission_write = server.mock("PUT", "/orgs/o/teams/devs/repos/o/r")
                .match_body(mockito::Matcher::Json(json!({"permission":"push"})))
                .with_status(204).expect(1).create_async().await;
            let client = client(&server);
            let user = User { login: "alice".into(), role: "admin".into() };
            let assignment = Assignment { repo: "r".into(), team: "devs".into(), permission: "push".into() };
            client.add_user_to_org(&user, true).await.expect("dry membership");
            client.assign_team_to_repo(&assignment, true).await.expect("dry assignment");
            assert!(!role_write.matched_async().await && !permission_write.matched_async().await, "dry run wrote");
            client.add_user_to_org(&user, false).await.expect("membership");
            client.assign_team_to_repo(&assignment, false).await.expect("assignment");
            for mock in [membership, permission, role_write, permission_write] {
                mock.assert_async().await;
            }
        }
    }

    #[tokio::test]
    async fn permission_reads_that_fail_do_not_trigger_writes() {
        let mut server = mockito::Server::new_async().await;
        let _membership = server.mock("GET", "/orgs/o/memberships/alice404").with_status(403)
            .with_body(r#"{"message":"Resource not accessible by integration"}"#).create_async().await;
        let _permission = server.mock("GET", "/orgs/o/teams/devs/repos/o/r404").with_status(403)
            .with_body(r#"{"message":"Resource not accessible by integration"}"#).create_async().await;
        let writes = server.mock("PUT", mockito::Matcher::Any).expect(0).create_async().await;
        let client = client(&server);
        assert!(client.add_user_to_org(&User { login: "alice404".into(), role: "admin".into() }, false).await.is_err());
        assert!(client.assign_team_to_repo(&Assignment { repo: "r404".into(), team: "devs".into(), permission: "push".into() }, false).await.is_err());
        writes.assert_async().await;
    }

    #[test]
    fn webhook_without_id_is_rejected() {
        let body = r#"[{"url": "u", "config": {"url": "http://example.com", "content_type": "json"}, "events": []}]"#;
        let err = serde_json::from_str::<Vec<WebhookResponse>>(body).expect_err("a hook without an id must not parse");
        assert!(err.to_string().contains("missing field `id`"), "{err}");
    }
}
