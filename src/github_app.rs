//! GitHub's manifest handshake. Registration still requires browser approval;
//! an organization PAT cannot create an App registration by itself.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{anyhow, bail, ensure, Context, Result};
use axum::{
    extract::{rejection::QueryRejection, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use reqwest::{redirect::Policy, Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{net::TcpListener, sync::oneshot};

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_CREDENTIAL_BYTES: usize = 256 * 1024;

/// Nonsecret registration metadata, safe to display or track separately.
#[derive(Debug, Deserialize, Serialize)]
pub struct AppSummary {
    pub id: u64,
    pub slug: String,
    pub name: String,
    pub html_url: String,
    pub owner: AppOwner,
    pub permissions: Value,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AppOwner {
    pub login: String,
}

pub fn load_manifest(path: &Path) -> Result<Value> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_MANIFEST_BYTES,
        "Manifest exceeds 64 KiB"
    );
    let manifest: Value = serde_json::from_slice(&bytes).context("Invalid manifest JSON")?;
    ensure!(manifest.is_object(), "Manifest must be a JSON object");
    ensure!(
        manifest
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty()),
        "Manifest requires a nonempty name"
    );
    let homepage = manifest
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Manifest requires a homepage URL"))?;
    let url = Url::parse(homepage).context("Invalid manifest homepage URL")?;
    ensure!(
        matches!(url.scheme(), "https" | "http") && url.host_str().is_some(),
        "Manifest homepage must be an HTTP(S) URL"
    );
    if let Some(permissions) = manifest.get("default_permissions") {
        let permissions = permissions
            .as_object()
            .ok_or_else(|| anyhow!("default_permissions must be an object"))?;
        ensure!(
            permissions
                .values()
                .all(|p| matches!(p.as_str(), Some("read" | "write"))),
            "Manifest permissions must use read or write"
        );
    }
    Ok(manifest)
}

pub async fn create(org: &str, manifest_path: &Path, credentials_dir: &Path) -> Result<AppSummary> {
    ensure!(valid_org(org), "Invalid GitHub organization name");
    let mut manifest = load_manifest(manifest_path)?;
    prepare_directory(credentials_dir)?;
    let result = register(org, &mut manifest, credentials_dir).await;
    if result.is_err() {
        // Only remove our newly reserved directory if it is still empty. A
        // partially written credential is retained for recovery, never deleted.
        let _ = fs::remove_dir(credentials_dir);
    }
    result
}

async fn register(org: &str, manifest: &mut Value, credentials_dir: &Path) -> Result<AppSummary> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let host = listener.local_addr()?.to_string();
    let mut entropy = [0; 32];
    getrandom::fill(&mut entropy).map_err(|_| anyhow!("System random source unavailable"))?;
    let state = entropy
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    manifest["redirect_url"] = format!("http://{host}/callback").into();
    let (callback_tx, callback_rx) = oneshot::channel();
    let app = registration_router(org, manifest, &host, &state, callback_tx);
    let (stop_tx, stop_rx) = oneshot::channel();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stop_rx.await;
            })
            .await
    });
    println!("Open http://{host}/setup/{state} in your browser.");
    println!("Approve creation under {org}; personal-account fallback will be rejected.");
    let result = async {
        let code = tokio::select! {
            received = tokio::time::timeout(Duration::from_secs(900), callback_rx) => {
                received.context("App registration timed out after 15 minutes")?
                    .context("App registration callback closed")?
            }
            _ = tokio::signal::ctrl_c() => bail!("App registration cancelled"),
        };
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(Policy::none())
            .user_agent("gh-config-app-manifest")
            .build()?;
        let credentials = exchange_manifest(&client, "https://api.github.com", &code).await?;
        save_credentials(credentials_dir, org, &credentials)
    }
    .await;
    let _ = stop_tx.send(());
    // A browser keepalive must not keep the CLI alive after the handshake.
    if tokio::time::timeout(Duration::from_secs(2), &mut server)
        .await
        .is_err()
    {
        server.abort();
    }
    result
}

struct Registration {
    host: String,
    state: String,
    form: String,
    callback: Mutex<Option<oneshot::Sender<String>>>,
}

#[derive(Deserialize)]
struct Callback {
    state: String,
    code: String,
}

fn registration_router(
    org: &str,
    manifest: &Value,
    host: &str,
    state: &str,
    callback: oneshot::Sender<String>,
) -> Router {
    let form = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Register GitHub App</title>\
         <h1>Register GitHub App for {org}</h1>\
         <form method=\"post\" action=\"https://github.com/organizations/{org}/settings/apps/new?state={state}\">\
         <input type=\"hidden\" name=\"manifest\" value=\"{}\">\
         <button type=\"submit\">Continue on GitHub</button></form>",
        html_escape(&manifest.to_string()),
    );
    Router::new()
        .route(&format!("/setup/{state}"), get(registration_form))
        .route("/callback", get(registration_callback))
        .with_state(Arc::new(Registration {
            host: host.into(),
            state: state.into(),
            form,
            callback: Mutex::new(Some(callback)),
        }))
}

async fn registration_form(State(s): State<Arc<Registration>>, headers: HeaderMap) -> Response {
    if !valid_host(&headers, &s.host) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    private_response(Html(s.form.clone()).into_response())
}

async fn registration_callback(
    State(s): State<Arc<Registration>>,
    headers: HeaderMap,
    query: std::result::Result<Query<Callback>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !valid_host(&headers, &s.host) || query.state != s.state || !valid_code(&query.code) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let sender = s.callback.lock().expect("callback lock poisoned").take();
    if sender.is_some_and(|sender| sender.send(query.code).is_ok()) {
        private_response(
            Html("Authorization received. Return to the terminal for the registration result.")
                .into_response(),
        )
    } else {
        StatusCode::CONFLICT.into_response()
    }
}

fn valid_host(headers: &HeaderMap, expected: &str) -> bool {
    headers.get(header::HOST).and_then(|h| h.to_str().ok()) == Some(expected)
}

fn private_response(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert(header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; form-action https://github.com; frame-ancestors 'none'; base-uri 'none'".parse().unwrap());
    headers.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    response
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn valid_org(org: &str) -> bool {
    !org.is_empty()
        && org.len() <= 39
        && !org.starts_with('-')
        && !org.ends_with('-')
        && org.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

fn valid_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 512
        && code
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

async fn exchange_manifest(client: &Client, api: &str, code: &str) -> Result<Value> {
    ensure!(valid_code(code), "Invalid manifest conversion code");
    let mut response = client
        .post(format!("{api}/app-manifests/{code}/conversions"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|_| anyhow!("GitHub manifest exchange request failed"))?;
    // Never include the response body or request URL: either can contain secrets.
    ensure!(
        response.status().is_success(),
        "GitHub manifest exchange failed (HTTP {})",
        response.status()
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("Reading GitHub manifest exchange failed"))?
    {
        ensure!(
            bytes.len() + chunk.len() <= MAX_CREDENTIAL_BYTES,
            "GitHub manifest response exceeds size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow!("Invalid GitHub manifest exchange response"))
}

fn save_credentials(dir: &Path, org: &str, credentials: &Value) -> Result<AppSummary> {
    let summary: AppSummary = serde_json::from_value(credentials.clone())
        .map_err(|_| anyhow!("GitHub returned incomplete App metadata"))?;
    ensure!(summary.owner.login.eq_ignore_ascii_case(org),
        "GitHub registered the App under a different owner; no credentials saved. Inspect the registration on GitHub.");
    ensure!(
        !summary.slug.is_empty()
            && summary
                .slug
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-'),
        "GitHub returned an invalid App slug"
    );
    let pem = credentials
        .get("pem")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("GitHub returned no App private key"))?;
    ensure!(
        pem.starts_with("-----BEGIN ") && pem.contains("PRIVATE KEY-----"),
        "GitHub returned an invalid App private key"
    );
    write_private(
        &dir.join("credentials.json"),
        &serde_json::to_vec_pretty(credentials)?,
    )?;
    write_private(&dir.join("private-key.pem"), pem.as_bytes())?;
    write_private(&dir.join("app.json"), &serde_json::to_vec_pretty(&summary)?)?;
    Ok(summary)
}

#[cfg(unix)]
fn prepare_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).context("Credentials directory parent must exist")?;
    for ancestor in parent.ancestors() {
        ensure!(
            !ancestor.join(".git").exists(),
            "Credentials must be saved outside Git worktrees"
        );
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .context("Choose a new credentials directory with an existing parent")
}

#[cfg(not(unix))]
fn prepare_directory(_: &Path) -> Result<()> {
    bail!("App credential output currently requires Unix file permissions")
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn credentials(owner: &str) -> Value {
        json!({
            "id":123, "slug":"gitkb-rabbit", "name":"GitKB Rabbit 🐇",
            "html_url":"https://github.com/apps/gitkb-rabbit",
            "owner":{"login":owner}, "permissions":{"contents":"read","pull_requests":"write"},
            "pem":"-----BEGIN RSA PRIVATE KEY-----\nsynthetic-test-key\n-----END RSA PRIVATE KEY-----",
            "client_secret":"synthetic-client-secret", "webhook_secret":"synthetic-webhook-secret"
        })
    }

    #[tokio::test]
    async fn callback_rejects_wrong_state_host_and_duplicates_without_consuming_the_request() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let base = format!("http://{host}");
        let (tx, mut rx) = oneshot::channel();
        let app = registration_router(
            "gitkb",
            &json!({"name":"<script>'\"&"}),
            &host,
            "expected",
            tx,
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client::new();
        let form = client
            .get(format!("{base}/setup/expected"))
            .send()
            .await
            .unwrap();
        assert_eq!(form.status(), StatusCode::OK);
        assert_eq!(form.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(form.headers()[header::REFERRER_POLICY], "no-referrer");
        let body = form.text().await.unwrap();
        assert!(body.contains("&lt;script&gt;&#39;"));
        assert!(!body.contains("<script>"));
        for query in [
            "state=wrong&code=secret-code",
            "state=expected",
            "state=expected&state=wrong&code=secret-code",
            "state=expected&code=..%2Fbad",
        ] {
            let response = client
                .get(format!("{base}/callback?{query}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert!(matches!(
                rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
        }
        let callback = format!("{base}/callback?state=expected&code=secret-code");
        let rejected = client
            .get(&callback)
            .header(header::HOST, "attacker.example")
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        assert!(matches!(
            rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(
            client.get(&callback).send().await.unwrap().status(),
            StatusCode::OK
        );
        assert_eq!(rx.await.unwrap(), "secret-code");
        assert_eq!(
            client.get(&callback).send().await.unwrap().status(),
            StatusCode::CONFLICT
        );
        server.abort();
    }

    #[tokio::test]
    async fn conversion_uses_the_official_endpoint_without_a_pat_and_hides_failure_secrets() {
        let mut server = mockito::Server::new_async().await;
        let success = server
            .mock("POST", "/app-manifests/temporary-code/conversions")
            .match_header("authorization", mockito::Matcher::Missing)
            .with_status(201)
            .with_body(credentials("gitkb").to_string())
            .create_async()
            .await;
        let client = Client::builder().redirect(Policy::none()).build().unwrap();
        let converted = exchange_manifest(&client, &server.url(), "temporary-code")
            .await
            .unwrap();
        assert_eq!(converted["owner"]["login"], "gitkb");
        success.assert_async().await;

        let failure = server
            .mock("POST", "/app-manifests/secret-code/conversions")
            .with_status(422)
            .with_body("synthetic-client-secret secret-code")
            .create_async()
            .await;
        let error = exchange_manifest(&client, &server.url(), "secret-code")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("422"));
        assert!(!error.contains("secret"));
        failure.assert_async().await;
        let redirect = server
            .mock("POST", "/app-manifests/redirect-code/conversions")
            .with_status(302)
            .with_header("location", "https://attacker.example/")
            .create_async()
            .await;
        assert!(exchange_manifest(&client, &server.url(), "redirect-code")
            .await
            .is_err());
        redirect.assert_async().await;
    }

    #[cfg(unix)]
    #[test]
    fn credentials_are_private_metadata_is_redacted_and_existing_paths_are_never_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("rabbit");
        prepare_directory(&dir).unwrap();
        save_credentials(&dir, "gitkb", &credentials("gitkb")).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for name in ["credentials.json", "private-key.pem", "app.json"] {
            assert_eq!(
                fs::metadata(dir.join(name)).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let metadata = fs::read_to_string(dir.join("app.json")).unwrap();
        assert!(!metadata.contains("secret"));
        assert!(!metadata.contains("PRIVATE KEY"));
        assert!(prepare_directory(&dir).is_err());
        let original = fs::read(dir.join("credentials.json")).unwrap();
        let mut replacement = credentials("gitkb");
        replacement["client_secret"] = "replacement".into();
        assert!(save_credentials(&dir, "gitkb", &replacement).is_err());
        assert_eq!(fs::read(dir.join("credentials.json")).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_personal_fallback_git_worktrees_and_symlink_destinations() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("rabbit");
        prepare_directory(&dir).unwrap();
        assert!(save_credentials(&dir, "gitkb", &credentials("Gonzih")).is_err());
        assert!(fs::read_dir(&dir).unwrap().next().is_none());
        let linked = temp.path().join("linked");
        symlink(&dir, &linked).unwrap();
        assert!(prepare_directory(&linked).is_err());
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        fs::write(repo.join(".git"), "gitdir: /elsewhere").unwrap();
        assert!(prepare_directory(&repo.join("credentials")).is_err());
    }

    #[test]
    fn manifest_validation_rejects_invalid_permissions_and_oversized_input() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("manifest.json");
        fs::write(
            &file,
            json!({"name":"Rabbit 🐇", "url":"https://gitkb.com",
            "default_permissions":{"contents":"read","pull_requests":"write"}})
            .to_string(),
        )
        .unwrap();
        assert!(load_manifest(&file).is_ok());
        for invalid in [
            json!([]),
            json!({"name":"", "url":"https://gitkb.com"}),
            json!({"name":"Rabbit", "url":"file:///tmp/app"}),
            json!({"name":"Rabbit", "url":"https://gitkb.com", "default_permissions":{"contents":"root"}}),
        ] {
            fs::write(&file, invalid.to_string()).unwrap();
            assert!(load_manifest(&file).is_err());
        }
        fs::write(&file, vec![b' '; MAX_MANIFEST_BYTES as usize + 1]).unwrap();
        assert!(load_manifest(&file)
            .unwrap_err()
            .to_string()
            .contains("64 KiB"));
        assert!(!valid_org("../gitkb"));
        assert!(!valid_org("gitkb?state=bad"));
    }
}
