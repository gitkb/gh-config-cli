//! Development tasks for gh-config. Run from anywhere in the workspace:
//!
//! - `cargo xtask gen-api-mappings`: regenerate `src/github_api_mapping_generated.rs`
//!   from the pinned schema in `mappings/github-openapi.json`.
//! - `--fetch` first downloads the latest schema from GitHub (needs `curl`).
//! - `--check` fails if the generated file differs from the pinned schema's output.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::{env, fs};

const SCHEMA_URL: &str = "https://raw.githubusercontent.com/github/rest-api-description/main/descriptions/api.github.com/api.github.com.json";
const SCHEMA: &str = "mappings/github-openapi.json";
const GENERATED: &str = "src/github_api_mapping_generated.rs";
const USAGE: &str = "usage: cargo xtask gen-api-mappings [--fetch | --check]";

type Result<T> = std::result::Result<T, String>;

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<()> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (fetch, check) = match args.as_slice() {
        ["gen-api-mappings"] => (false, false),
        ["gen-api-mappings", "--fetch"] => (true, false),
        ["gen-api-mappings", "--check"] => (false, true),
        _ => return Err(USAGE.into()),
    };
    let root = workspace_root()?;
    let schema = root.join(SCHEMA);
    let generated = root.join(GENERATED);
    if fetch {
        let tmp = schema.with_extension("json.download");
        let status = Command::new("curl")
            .args(["--fail", "--location", "--silent", "--show-error", "--output"])
            .arg(&tmp)
            .arg(SCHEMA_URL)
            .status()
            .map_err(|e| format!("running curl: {e}"))?;
        if !status.success() {
            // Best effort: a partial download is useless, and no file there is fine too.
            let _ = fs::remove_file(&tmp);
            return Err(format!("downloading {SCHEMA_URL} failed: curl {status}"));
        }
        rename(&tmp, &schema)?;
    }
    let text = fs::read_to_string(&schema).map_err(|e| format!("reading {}: {e}", schema.display()))?;
    let spec: Value = serde_json::from_str(&text).map_err(|e| format!("parsing {}: {e}", schema.display()))?;
    let output = render(&spec)?;
    if check {
        let current = fs::read_to_string(&generated).map_err(|e| format!("reading {}: {e}", generated.display()))?;
        return if current == output {
            Ok(())
        } else {
            Err(format!("{GENERATED} is stale; run `cargo xtask gen-api-mappings`"))
        };
    }
    let tmp = generated.with_extension("rs.tmp");
    fs::write(&tmp, output).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    rename(&tmp, &generated)
}

/// Read at run time, not build time, so a binary built from another checkout never writes here.
fn workspace_root() -> Result<PathBuf> {
    let xtask = env::var_os("CARGO_MANIFEST_DIR").ok_or("run this through `cargo xtask`")?;
    Path::new(&xtask)
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("{} has no parent directory", Path::new(&xtask).display()))
}

fn rename(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).map_err(|e| format!("moving {} to {}: {e}", from.display(), to.display()))
}

struct Mapping<'a> {
    resource_type: &'static str,
    endpoint: &'a str,
    method: String,
}

impl Mapping<'_> {
    /// PATCH /repos/{owner}/{repo} wins collisions, so repo settings map to the repo update call.
    fn is_repo_update(&self) -> bool {
        self.endpoint.starts_with("/repos/{owner}/{repo}") && self.method == "PATCH"
    }
}

fn render(spec: &Value) -> Result<String> {
    let paths = spec
        .get("paths")
        .and_then(Value::as_object)
        .ok_or("the schema has no `paths` object")?;
    let mut map: BTreeMap<&str, Mapping> = BTreeMap::new();
    for (endpoint, methods) in paths {
        let methods = methods
            .as_object()
            .ok_or_else(|| format!("path {endpoint} is not an object"))?;
        for (method, operation) in methods {
            let method = method.to_uppercase();
            if !["PATCH", "PUT", "POST"].contains(&method.as_str()) {
                continue;
            }
            let Some(schema) = operation.pointer("/requestBody/content/application~1json/schema") else {
                continue;
            };
            let schema = match schema.get("$ref").and_then(Value::as_str) {
                Some(reference) => resolve_ref(spec, reference)?,
                None => schema,
            };
            let Some(props) = schema.get("properties").and_then(Value::as_object) else {
                continue;
            };
            for field in props.keys() {
                let current = Mapping {
                    resource_type: infer_resource_type(endpoint),
                    endpoint,
                    method: method.clone(),
                };
                // Otherwise the last mapping seen wins, unless the existing one is the repo update.
                let replace = map
                    .get(field.as_str())
                    .is_none_or(|existing| !existing.is_repo_update());
                if replace {
                    map.insert(field, current);
                }
            }
        }
    }

    let mut out = String::from(
        "// AUTO-GENERATED FILE. DO NOT EDIT BY HAND.\n\
         // Please run `cargo xtask gen-api-mappings` to regenerate.\n\
         // This generator prioritizes PATCH /repos/{owner}/{repo} for repo settings.\n\
         // WARNING: Collisions for other keys might still use the *last processed* mapping.\n\
         use std::collections::HashMap;\n\
         use crate::api_mapping::ApiFieldMapping;\n\
         pub fn get_github_api_mapping() -> HashMap<&'static str, ApiFieldMapping> {\n    \
         let mut map = HashMap::new();\n",
    );
    for (key, m) in &map {
        let key = key.escape_default();
        out.push_str(&format!(
            "    map.insert(\"{key}\", ApiFieldMapping {{ resource_type: \"{}\", config_key: \"{key}\", endpoint: \"{}\", method: \"{}\", json_path: \"{key}\" }});\n",
            m.resource_type,
            m.endpoint.escape_default(),
            m.method.escape_default(),
        ));
    }
    out.push_str("    map\n}\n");
    Ok(out)
}

fn resolve_ref<'a>(spec: &'a Value, reference: &str) -> Result<&'a Value> {
    let pointer = reference
        .strip_prefix('#')
        .ok_or_else(|| format!("unsupported $ref {reference}"))?;
    spec.pointer(pointer)
        .ok_or_else(|| format!("unresolved $ref {reference}"))
}

fn infer_resource_type(endpoint: &str) -> &'static str {
    if endpoint.starts_with("/repos/{owner}/{repo}") || endpoint.starts_with("/repos/{org}/{repo}") {
        "repo"
    } else if endpoint.starts_with("/orgs/{org}") {
        "org"
    } else if endpoint == "/user/repos" {
        "repo_create_user"
    } else if endpoint.starts_with("/user") {
        "user"
    } else if endpoint.starts_with("/teams") || endpoint.contains("/teams/") {
        "team"
    } else if endpoint.starts_with("/projects") {
        "project"
    } else if endpoint.starts_with("/gists") {
        "gist"
    } else if endpoint.starts_with("/enterprises/{") {
        "enterprise"
    } else {
        "other"
    }
}
