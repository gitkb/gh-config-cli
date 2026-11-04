/*!
    GitHub API client module.

    This module provides the `GitHubClient` struct and related types for interacting with the GitHub REST API.
    It supports operations such as updating repository settings, managing teams, users, and webhooks, and
    generating configuration from a GitHub organization. All API interactions are authenticated and support
    dry-run mode for previewing changes.

    Public APIs are documented for maintainability. Internal response structs are used for deserialization.
*/

use crate::config::{Assignment, Repo, RepoSettings, Team, User, WebhookConfig, Config};
use crate::github_api_mapping_generated::get_github_api_mapping;
use crate::error::{AppError, AppResult};
use colored::*;
use log::{debug, info, error};
use reqwest::Client;
use serde_json::json;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet}; // Added HashSet
use similar::{ChangeTag, TextDiff};
use std::fs::File;
use std::io::Write;

const GITHUB_API_BASE_URL: &str = "https://api.github.com";

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct RepoResponse {
    allow_merge_commit: bool,
    allow_squash_merge: bool,
    allow_rebase_merge: bool,
    private: bool,
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
}

#[derive(Debug, Deserialize, Serialize)]
pub struct WebhookResponse {
    id: Option<i64>,
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
    /// Reqwest HTTP client for making API requests.
    client: Client,
    /// GitHub personal access token for authentication.
    token: String,
    /// Name of the GitHub organization to operate on.
    pub org: String,
}

impl GitHubClient {
    /// Create a new GitHubClient for the given organization and token.
    ///
    /// # Arguments
    /// * `token` - GitHub personal access token.
    /// * `org` - Name of the GitHub organization.
    pub fn new(token: &str, org: &str) -> Self {
        GitHubClient {
            client: Client::new(),
            token: token.to_string(),
            org: org.to_string(),
        }
    }

    async fn send_patch(&self, url: &str, body: serde_json::Value) -> AppResult<()> {
        // --- Add Enhanced Logging ---
        debug!("Attempting to build PATCH request for URL: '{}'", url);
        if url.trim().is_empty() {
            error!("URL passed to send_patch() is empty!");
            return Err(AppError::GitHubApi("Internal error: Attempted PATCH with empty URL".to_string()));
        }
        if self.token.trim().is_empty() {
            error!("GitHub token is empty!");
            return Err(AppError::GitHubApi("GitHub token is empty".to_string()));
        } else {
            debug!("Token length: {}", self.token.len());
        }
        debug!("PATCH body: {:?}", body); // Log the body being sent
        // --- End Enhanced Logging ---

        // Try building step-by-step again
        let client_ref = &self.client;
        let builder_result = client_ref.patch(url); // Initial builder

        // --- Log after initial build ---
        debug!("reqwest::Client::patch succeeded for URL: '{}'", url); // Check if this logs
        // ---

        let auth_header_value = format!("Bearer {}", self.token);
        if auth_header_value.contains('\n') || auth_header_value.contains('\r') {
             error!("Authorization header value contains invalid characters (newline/CR)");
             return Err(AppError::GitHubApi("Invalid characters in token for Authorization header".to_string()));
        }

        // Chain headers
        let request_builder_headers = builder_result
            .header(reqwest::header::AUTHORIZATION, auth_header_value)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(reqwest::header::USER_AGENT, "gh-config");

        // --- Log after headers added ---
        debug!("Request builder headers configured."); // Check if this logs
        // ---

        // Add JSON body
        let request_builder_final = request_builder_headers.json(&body);

        // --- Log after body added ---
        debug!("Request builder body configured (using .json())."); // Check if this logs
        // ----

        // Send the request
        debug!("Attempting final send..."); // Check if this logs
        let response_result = request_builder_final.send().await; // Store result before unwrapping

        match response_result {
             Ok(response) => {
                 // --- Original status check logic ---
                 debug!("Request send successful."); // Log success before status check
                 let status = response.status();
                 debug!("PATCH {} returned status: {}", url, status);
                 if status.is_success() {
                     Ok(())
                 } else {
                     let text = response.text().await?;
                     error!("PATCH {} failed with status {}: {}", url, status, text);
                     Err(AppError::GitHubApi(text))
                 }
             }
             Err(e) => {
                 error!("Request builder send() failed: {}", e);
                 // Check if the error source is the builder error we saw
                 if e.is_builder() {
                     error!("Confirmed: Failure is a builder error during send().");
                 } else if e.is_connect() {
                     error!("Confirmed: Failure is a connection error during send().");
                 } else if e.is_timeout() {
                     error!("Confirmed: Failure is a timeout error during send().");
                 } // Add other checks from reqwest::Error if needed
                 Err(AppError::from(e)) // Propagate the reqwest error
             }
         }

    }

    async fn send_post(&self, url: &str, body: serde_json::Value) -> AppResult<()> {
        // --- Add Similar Enhanced Logging ---
        debug!("Attempting to build POST request for URL: '{}'", url);
        if url.trim().is_empty() { /* ... */ }
        if self.token.trim().is_empty() { /* ... */ } else { debug!("Token length: {}", self.token.len()); }
        debug!("POST body: {:?}", body);

        let client_ref = &self.client;
        let builder_result = client_ref.post(url); // Initial builder
        debug!("reqwest::Client::post succeeded for URL: '{}'", url);

        let auth_header_value = format!("Bearer {}", self.token);
        if auth_header_value.contains('\n') || auth_header_value.contains('\r') { /* ... */ }

        let request_builder_headers = builder_result
            .header(reqwest::header::AUTHORIZATION, auth_header_value)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(reqwest::header::USER_AGENT, "gh-config");
        debug!("Request builder headers configured.");

        let request_builder_final = request_builder_headers.json(&body);
        debug!("Request builder body configured (using .json()).");

        debug!("Attempting final send...");
        let response_result = request_builder_final.send().await; // Store result

        match response_result {
            Ok(response) => {
                debug!("Request send successful.");
                let status = response.status();
                debug!("POST {} returned status: {}", url, status);
                if status.is_success() {
                    Ok(())
                } else {
                    let text = response.text().await?;
                    error!("POST {} failed with status {}: {}", url, status, text);
                    Err(AppError::GitHubApi(text))
                }
            }
            Err(e) => {
                error!("Request builder send() failed: {}", e);
                if e.is_builder() { error!("Confirmed: Failure is a builder error during send()."); }
                // ... other error checks ...
                Err(AppError::from(e))
            }
        }
    }

    async fn send_put(&self, url: &str, body: Option<serde_json::Value>) -> AppResult<()> {
        let mut request = self
            .client
            .put(url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "gh-config");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let text = response.text().await?;
            error!("PUT {} failed with status {}: {}", url, status, text);
            Err(AppError::GitHubApi(text))
        }
    }

    async fn get(&self, url: &str) -> AppResult<reqwest::Response> {
        // --- Add Enhanced Logging ---
        debug!("Attempting to build GET request for URL: '{}'", url);
        if url.trim().is_empty() {
             error!("URL passed to get() is empty!");
             return Err(AppError::GitHubApi("Internal error: Attempted GET with empty URL".to_string()));
        }
        // Check token validity superficially (e.g., not empty)
        if self.token.trim().is_empty() {
            error!("GitHub token is empty!");
            // Although this usually causes 401 later, an empty Bearer token might fail earlier.
            return Err(AppError::GitHubApi("GitHub token is empty".to_string()));
        } else {
             // Log length as a non-sensitive indicator
             debug!("Token length: {}", self.token.len());
        }
        // --- End Enhanced Logging ---

        // Try building the request step-by-step to isolate the failure
        let client_ref = &self.client; // Borrow client
        let builder_result = client_ref.get(url); // Create builder

        // --- Log after potential URL parsing failure ---
        debug!("reqwest::Client::get succeeded for URL: '{}'", url);
        // ---

        let auth_header_value = format!("Bearer {}", self.token);
        // Validate auth header value doesn't contain obviously invalid chars like newline
        if auth_header_value.contains('\n') || auth_header_value.contains('\r') {
             error!("Authorization header value contains invalid characters (newline/CR)");
             return Err(AppError::GitHubApi("Invalid characters in token for Authorization header".to_string()));
        }

        // Chain the rest - if it fails here, it's likely header related
        let request_builder = builder_result
            .header(reqwest::header::AUTHORIZATION, auth_header_value) // Use constant for clarity
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(reqwest::header::USER_AGENT, "gh-config");

        // --- Log before send ---
        debug!("Request builder fully configured, attempting send...");
        // ---

        let response = request_builder.send().await?;

        // --- Original status check logic ---
        let status = response.status();
        debug!("GET {} returned status: {}", url, status);
        if status.is_success() {
            Ok(response)
        } else {
            let text = response.text().await?;
            error!("GET {} failed with status {}: {}", url, status, text);
            Err(AppError::GitHubApi(text))
        }
    }

    /// Fetch all settings for a repo as a HashMap<String, serde_yaml::Value>
    async fn get_repo_settings(&self, repo_name: &str) -> AppResult<RepoSettings> {
        let full_url = format!("{}/repos/{}/{}", GITHUB_API_BASE_URL, self.org, repo_name);
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

    #[allow(dead_code)]
    async fn get_repo_visibility(&self, repo_name: &str) -> AppResult<String> {
        let full_url = format!("{}/repos/{}/{}", GITHUB_API_BASE_URL, self.org, repo_name);
        let response = self.get(&full_url).await?;
        let text = response.text().await?;
        if text.is_empty() {
            error!("Empty response body from GET {}", full_url);
            return Err(AppError::GitHubApi("Empty response body".to_string()));
        }
        let repo: RepoResponse = serde_json::from_str(&text)
            .map_err(|e| AppError::GitHubApi(format!("Failed to parse response from {}: {}", full_url, e)))?;
        Ok(if repo.private { "private" } else { "public" }.to_string())
    }

    async fn get_team(&self, team_name: &str) -> AppResult<Option<TeamResponse>> {
        let full_url = format!("{}/orgs/{}/teams/{}", GITHUB_API_BASE_URL, self.org, team_name);
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
            Err(AppError::GitHubApi(e)) if e.contains("404") => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn get_user_membership(&self, login: &str) -> AppResult<Option<String>> {
        let full_url = format!("{}/orgs/{}/memberships/{}", GITHUB_API_BASE_URL, self.org, login);
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
            Err(AppError::GitHubApi(e)) if e.contains("404") => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn get_team_repos(&self, team_name: &str) -> AppResult<Vec<TeamRepoResponse>> {
        let full_url = format!("{}/orgs/{}/teams/{}/repos?per_page=100", GITHUB_API_BASE_URL, self.org, team_name);
        let response = self.get(&full_url).await?;
        let text = response.text().await?;
        let repos: Vec<TeamRepoResponse> = serde_json::from_str(&text)
            .map_err(|e| AppError::GitHubApi(format!("Failed to parse team repos: {}", e)))?;
        Ok(repos)
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
        let full_url = format!("{}/repos/{}/{}/hooks", GITHUB_API_BASE_URL, self.org, repo_name);
        let response = self.get(&full_url).await?;
        let text = response.text().await?;
        let webhooks: Vec<WebhookResponse> = serde_json::from_str(&text)
            .map_err(|e| AppError::GitHubApi(format!("Failed to parse webhooks: {}", e)))?;
        Ok(webhooks)
    }

    async fn create_webhook(&self, repo_name: &str, webhook: &WebhookConfig) -> AppResult<()> {
        let url = format!("{}/repos/{}/{}/hooks", GITHUB_API_BASE_URL, self.org, repo_name);
    
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
        let url = format!("{}/repos/{}/{}/hooks/{}", GITHUB_API_BASE_URL, self.org, repo_name, hook_id);
    
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

        if dry_run {
            match existing {
                Some(hook) if hook.events != webhook.events || hook.config.content_type != webhook.content_type => {
                    info!(
                        "[Dry Run] Would update webhook for {}/{}: events {:?} -> {:?}, content_type {} -> {}",
                        self.org, repo_name, hook.events, webhook.events, hook.config.content_type, webhook.content_type
                    );
                }
                Some(_) => debug!("[Dry Run] Webhook for {}/{} already matches desired config", self.org, repo_name),
                None => info!(
                    "[Dry Run] Would create webhook for {}/{} with config: {:?}",
                    self.org, repo_name, webhook
                ),
            }
        } else {
            match existing {
                Some(hook) if hook.events != webhook.events || hook.config.content_type != webhook.content_type => {
                    self.update_webhook(repo_name, hook.id.unwrap(), webhook).await?;
                }
                Some(_) => debug!("Webhook for {}/{} already up to date", self.org, repo_name),
                None => self.create_webhook(repo_name, webhook).await?,
            }
        }
        Ok(())
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
    pub async fn update_repo_settings(&self, repo: &Repo, dry_run: bool) -> AppResult<()> {
        let current = self.get_repo_settings(&repo.name).await?;
        let desired = &repo.settings;
        let mapping = get_github_api_mapping();

        // --- Optimization suggestion (Apply after fixing URL): ---
        // Instead of sending one PATCH per setting, collect all PATCHes for the same endpoint.
        // Create a HashMap<String, serde_json::Map<String, serde_json::Value>> where the key is the endpoint URL.
        let mut pending_updates: HashMap<String, (String, serde_json::Map<String, serde_json::Value>)> = HashMap::new();
        // The tuple stores (HTTP Method, Body Map)

        for (k, v_desired) in desired.iter() {
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
                    let full_url = format!("{}{}", GITHUB_API_BASE_URL, relative_path);
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


        // If a webhook is defined in the repo config, manage it as well.
        // Ensure manage_webhooks also uses full URLs if it calls send_* methods directly.
         if let Some(webhook) = repo.webhook.as_ref() {
             self.manage_webhooks(&repo.name, webhook, dry_run).await?;
         }

        Ok(())
    }

        // Add a helper for DELETE
    async fn send_delete(&self, url: &str) -> AppResult<()> {
        debug!("Attempting to build DELETE request for URL: '{}'", url);
        // Add token/URL checks if desired
        let response = self
            .client
            .delete(url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", self.token))
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(reqwest::header::USER_AGENT, "gh-config")
            .send()
            .await?;

        let status = response.status();
        debug!("DELETE {} returned status: {}", url, status);
        // DELETE for membership returns 204 No Content on success
        if status.is_success() || status == reqwest::StatusCode::NO_CONTENT {
            Ok(())
        } else {
            let text = response.text().await?;
            error!("DELETE {} failed with status {}: {}", url, status, text);
            Err(AppError::GitHubApi(text))
        }
    }

    // Add a helper to get team members
    async fn get_team_members(&self, team_slug: &str) -> AppResult<HashSet<String>> {
        let url = format!("https://api.github.com/orgs/{}/teams/{}/members?per_page=100", self.org, team_slug);
        let response = self.get(&url).await?;
        let members_json: Vec<serde_json::Value> = response.json().await.map_err(AppError::Http)?;
        let members = members_json.iter()
            .filter_map(|m| m["login"].as_str().map(String::from))
            .collect::<HashSet<String>>();
        Ok(members)
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
        let url_create = format!("https://api.github.com/orgs/{}/teams", self.org); // For POST
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
                    let member_url = format!("https://api.github.com/orgs/{}/teams/{}/memberships/{}", self.org, team_slug, member);
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
                 let member_url = format!("https://api.github.com/orgs/{}/teams/{}/memberships/{}", self.org, team_slug, member);
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
                  let member_url = format!("https://api.github.com/orgs/{}/teams/{}/memberships/{}", self.org, team_slug, member);
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
        if dry_run {
            let current_role = self.get_user_membership(&user.login).await?;
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
                GITHUB_API_BASE_URL, self.org, user.login
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
        if dry_run {
            let current_perm = self
                .get_team_repo_permission(&assignment.team, &assignment.repo)
                .await?;
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
                GITHUB_API_BASE_URL, self.org, assignment.team, self.org, assignment.repo
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
            GITHUB_API_BASE_URL, self.org, team, self.org, repo
        );
        match self.get(&full_url).await {
            Ok(response) => {
                let text = response.text().await?;
                if text.is_empty() {
                    debug!("Empty response body from GET {}, assuming permission exists but not detailed", full_url);
                    return Ok(Some("push".to_string()));
                }
                let perms: TeamRepoResponse = serde_json::from_str(&text)
                    .map_err(|e| AppError::GitHubApi(format!("Failed to parse response from {}: {}", full_url, e)))?;
                let permission = if perms.permissions.admin {
                    "admin"
                } else if perms.permissions.push {
                    "push"
                } else if perms.permissions.pull {
                    "pull"
                } else {
                    "none"
                };
                Ok(Some(permission.to_string()))
            }
            Err(AppError::GitHubApi(e)) if e.contains("404") => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn generate_config_and_write(&self, config_path: &str, dry_run: bool) -> AppResult<()> {
        info!("Generating config from GitHub org: {}", self.org);
        let config = self.generate_unfiltered_config_from_org().await?;  // Make sure to call an unfiltered version

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
             let mut events = default_webhook.events.clone();
             events.sort();
             for event in &events {
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
                       let keys_to_write = ["allow_merge_commit", "allow_squash_merge", "allow_rebase_merge"];
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
                 // Write webhook only if different from default (if default exists)
                  if let Some(webhook) = &repo.webhook {
                     if config.default_webhook.as_ref() != Some(webhook) {
                        yaml_content.push_str("  webhook:\n");
                        yaml_content.push_str(&format!("    url: {}\n", webhook.url));
                        yaml_content.push_str(&format!("    content_type: {}\n", webhook.content_type));
                        yaml_content.push_str("    events:\n");
                        let mut events = webhook.events.clone();
                        events.sort();
                        for event in &events {
                         yaml_content.push_str(&format!("    - {}\n", event));
                         }
                     }
                  } else if config.default_webhook.is_none() {
                     // If no default, explicitly state no webhook? Or omit? Omit for cleaner.
                     // yaml_content.push_str("  webhook: null\n");
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

    async fn generate_unfiltered_config_from_org(&self) -> AppResult<Config> {
        // This is essentially the original logic of generate_config_from_org
         let mut repos = Vec::new();
        let repo_url = format!("https://api.github.com/orgs/{}/repos?per_page=100", self.org);
        let repo_response = self.get(&repo_url).await?;
        let repo_json: Vec<serde_json::Value> = repo_response.json().await.map_err(AppError::Http)?;

        for repo in repo_json {
            let name = repo["name"].as_str().ok_or_else(|| AppError::GitHubApi("Missing repo name".to_string()))?.to_string();
            // Fetch *limited* settings relevant for generating a *manageable* config
            let settings = match self.get_repo_settings(&name).await {
                Ok(full_settings) => {
                    let mut manageable_settings = RepoSettings::new();
                    // Only include settings we typically manage
                    let managed_keys = ["allow_merge_commit", "allow_squash_merge", "allow_rebase_merge"];
                    for key in managed_keys {
                         if let Some(value) = full_settings.get(key) {
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
            let webhooks = self.get_webhooks(&name).await.unwrap_or_default(); // Handle potential error
            let webhook = webhooks.first().map(|wh| WebhookConfig {
                url: wh.config.url.clone(),
                content_type: wh.config.content_type.clone(),
                events: wh.events.clone(),
            });

            repos.push(Repo {
                name,
                settings,
                visibility,
                webhook,
                branch_protections: vec![], // Add logic to fetch these if needed
                extra: std::collections::HashMap::new(),
            });
        }

        // Fetch teams, users, assignments as before (full state needed for generation)
        let mut teams = Vec::new();
        let team_url = format!("https://api.github.com/orgs/{}/teams?per_page=100", self.org);
        let team_response = self.get(&team_url).await?;
        let team_json: Vec<serde_json::Value> = team_response.json().await.map_err(AppError::Http)?;

        for team in team_json {
            // Fetch full team data including members
             let name = team["slug"].as_str().ok_or_else(|| AppError::GitHubApi("Missing team slug".to_string()))?.to_string();
            let members_url = format!("https://api.github.com/orgs/{}/teams/{}/members?per_page=100", self.org, name);
            let members_response = self.get(&members_url).await?;
            let members_json: Vec<serde_json::Value> = members_response.json().await.map_err(AppError::Http)?;
            let mut members = members_json.iter()
                .filter_map(|m| m["login"].as_str().map(String::from))
                .collect::<Vec<String>>();
            members.sort();

            teams.push(Team { name, members });
        }

        let mut users = Vec::new();
        let members_url = format!("https://api.github.com/orgs/{}/members?per_page=100", self.org);
        let members_response = self.get(&members_url).await?;
        let members_json: Vec<serde_json::Value> = members_response.json().await.map_err(AppError::Http)?;

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
                 let permission = if repo.permissions.admin {
                    "admin"
                } else if repo.permissions.push {
                    "push" // Use push/pull
                } else if repo.permissions.pull {
                    "pull"
                } else {
                    "none"
                };
                if permission != "none" {
                    assignments.push(Assignment {
                        repo: repo.name.clone(),
                        team: team.name.clone(),
                        permission: permission.to_string(),
                    });
                }
            }
        }

         // Determine default webhook - find the most common one perhaps?
         // Or just pick the first one found for simplicity? Let's pick first.
        let default_webhook = repos.iter().find_map(|r| r.webhook.clone());

        Ok(Config {
            org: self.org.clone(),
            repos,
            teams,
            users,
            assignments,
            default_webhook,
            default_branch_protections: vec![], // Add logic if needed
            extra: std::collections::HashMap::new(),
        })
    }

    pub async fn sync(&mut self, config_path: &str, dry_run: bool) -> AppResult<()> {
        let config = crate::config::Config::from_file_with_defaults(config_path, None)?;
        self.org = config.org.clone(); // Set org from config

        if dry_run {
            info!("Running in dry-run mode; validating changes without applying.");
        } else {
            info!("Running in apply mode; changes will be applied.");
        }

        let mut config = config;
        // Apply default webhook *before* iterating repos for sync
        if let Some(default_webhook) = &config.default_webhook {
            for repo in &mut config.repos {
                if repo.webhook.is_none() {
                    repo.webhook = Some(default_webhook.clone());
                }
            }
        } else {
             // If no default webhook, ensure all repos have one explicitly defined
             if config.repos.iter().any(|r| r.webhook.is_none()) {
                  return Err(AppError::GitHubApi(
                     "Sync requires either a 'default_webhook' or explicit 'webhook' definition for every repo in the config.".to_string()
                 ));
             }
        }

        // Sync resources
        for repo in &config.repos {
            // --- Add logging just before the failing call ---
            info!("Processing repo: {}/{}", self.org, repo.name);
            if repo.name.trim().is_empty() {
                 error!("Found repo with empty name in config file '{}'.", config_path);
                 return Err(AppError::GitHubApi("Invalid empty repo name found in config.".to_string()));
            }
            // You might add more validation for repo.name characters here if needed
            // --- End logging ---

            self.update_repo_settings(repo, dry_run).await?; // This is the first call in the loop
        }

        // Teams
        for team in &config.teams {
            info!("Processing team: {}", team.name); // Add similar logging for other resources
          self.create_team(team, dry_run).await?;
        }

        // Users
        for user in &config.users {
            info!("Processing user: {}", user.login); // Add similar logging
            self.add_user_to_org(user, dry_run).await?;
        }

        // Assignments
        for assignment in &config.assignments {
            info!("Processing assignment: Team '{}' on Repo '{}'", assignment.team, assignment.repo); // Add similar logging
            self.assign_team_to_repo(assignment, dry_run).await?;
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

        // --- Step 1: Load original local config & track explicit webhooks ---
        let local_config = crate::config::Config::from_file_with_defaults(config_path, None)?;
        let local_default_webhook = local_config.default_webhook.clone();
        // Keep track of original explicit webhooks
        let originally_explicit_webhooks: HashSet<String> = local_config.repos.iter()
            .filter(|r| r.webhook.is_some())
            .map(|r| r.name.clone())
            .collect();

        // --- Step 2: Fetch FULL GitHub org state (unfiltered, as sync-from-github would write) ---
        let github_config = self.generate_unfiltered_config_from_org().await?;

        // --- Step 3: Prepare final versions for diffing ---
        let mut diff_local_config = local_config.clone(); // Clone original local config
        let mut diff_github_config = github_config;      // Use the fetched FULL GitHub org state

        // --- Step 4: Apply local default webhook logic to the local config *copy* ---
        let mut sorted_local_default_webhook = local_default_webhook.clone(); // Clone to sort optional default
        if let Some(ref mut wh) = sorted_local_default_webhook {
            wh.events.sort(); // Sort events in the default webhook we'll use for comparison
        }

        if let Some(ref default_webhook) = local_default_webhook { // Use original for application logic
            for repo in &mut diff_local_config.repos {
                if repo.webhook.is_none() {
                    repo.webhook = Some(default_webhook.clone());
                }
                // Sort events for consistent comparison (both applied default and explicit)
                if let Some(wh) = repo.webhook.as_mut() {
                    wh.events.sort();
                }
            }
        }
        // Events from generate_unfiltered_config_from_org are sorted by normalize_webhooks below

        // --- Step 5: Normalization - Remove matching default webhooks for both configs ---
        // Helper closure to normalize webhooks and default_webhook
        let normalize_webhooks = |config: &mut crate::config::Config| {
            // Sort default webhook events for comparison
            let mut sorted_default = config.default_webhook.clone();
            if let Some(ref mut wh) = sorted_default { wh.events.sort(); }

            // Track repos that actually use the default
            let mut used_default = false;
            for repo in &mut config.repos {
                // Sort events for explicit webhooks
                if let Some(ref mut wh) = repo.webhook { wh.events.sort(); }
                // If repo webhook matches default, set to None for diffing
                if let (Some(ref wh), Some(ref def)) = (&repo.webhook, &sorted_default) {
                    if wh == def {
                        repo.webhook = None;
                        used_default = true;
                    }
                } else if repo.webhook.is_none() && sorted_default.is_some() {
                    // If repo has no webhook, it implicitly uses the default
                    used_default = true;
                }
            }
            // If no repo uses the default, set it to None for diffing
            if !used_default {
                config.default_webhook = None;
            } else {
                // Always store sorted version for diffing
                config.default_webhook = sorted_default;
            }
        };
        normalize_webhooks(&mut diff_local_config);
        normalize_webhooks(&mut diff_github_config);

        // --- Step 6: Remove top-level default key if not used by any repo ---
        // (Handled above in normalization)

        // --- Step 7: Sort both configs... (ensure BTreeMap logic is kept) ---
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

        // --- Step 8: Serialize and Diff ---
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

        if !has_diffs {
            println!("No differences found between full GitHub org state and local config.");
        } else {
            println!("Differences found between full GitHub org state and local config:");
            println!("--- GitHub Org (Normalized)"); // Adjusted header
            println!("+++ Local Config (Normalized)"); // Adjusted header
            print!("{}", output_buffer);
        }
        Ok(has_diffs)
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use mockito;
    use tokio;

    #[tokio::test]
    async fn test_github_client_new() {
        let client = GitHubClient::new("dummy_token", "dummy_org");
        assert_eq!(client.token, "dummy_token");
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
            assert_eq!(hook.id, Some(123));
            assert_eq!(hook.url, "http://api.github.com/hook/123");
            assert_eq!(hook.config.url, "http://example.com");
            assert_eq!(hook.config.content_type, "json");
            assert_eq!(hook.events, vec!["push", "pull_request"]);
        });
    }
}