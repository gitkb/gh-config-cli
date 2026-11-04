use clap::{Parser, Subcommand};
use gh_config::config::Config;
use gh_config::error::{self, AppResult};
use gh_config::github::GitHubClient;
use gh_config::github_app;
use log::{error, info};
use std::process;

#[derive(Parser, Debug)]
#[command(version, about = "Manage GitHub org settings declaratively")]
struct Args {
    /// GitHub Personal Access Token
    #[arg(short, long, env = "GITHUB_TOKEN")]
    token: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Register a GitHub App using a versioned manifest and browser approval
    App {
        #[command(subcommand)]
        command: AppCommand,
    },
    #[command(flatten)]
    Org(OrgCommand),
}

/// Commands that read or change an organization and need a token.
#[derive(Subcommand, Debug)]
enum OrgCommand {
    /// Show a line-numbered diff between GitHub state and local config file
    Diff {
        /// Path to the config file
        config: String,
    },
    /// Sync local config to GitHub
    Sync {
        /// Path to the config file
        config: String,
        /// Dry run mode (no changes applied, only validation)
        #[arg(long)]
        dry_run: bool,
    },
    /// Generate config from a GitHub org and write to file
    SyncFromOrg {
        /// Path to the config file
        config: String,
        /// Dry run mode (no changes applied, only validation)
        #[arg(long)]
        dry_run: bool,
        /// GitHub organization name to sync from
        #[arg(long)]
        org: String,
    },
}

#[derive(Subcommand, Debug)]
enum AppCommand {
    /// Check a manifest without authenticating or creating an App
    Validate {
        #[arg(long)]
        manifest: std::path::PathBuf,
    },
    /// Start the official manifest flow; approve the org-owned App in your browser
    Create {
        #[arg(long)]
        org: String,
        #[arg(long)]
        manifest: std::path::PathBuf,
        /// New private directory outside Git for the generated credentials (Unix only)
        #[arg(long)]
        credentials_dir: std::path::PathBuf,
    },
}

#[tokio::main]
async fn main() {
    env_logger::init();
    match run().await {
        Ok(false) => process::exit(0), // No diffs or successful sync/sync-from-org
        Ok(true) => {
            info!("Exiting with code 1 due to differences found.");
            process::exit(1); // Diffs found
        }
        Err(e) => {
            error!("Application error: {}", e);
            process::exit(1); // Error occurred
        }
    }
}

async fn run() -> AppResult<bool> {
    let args = Args::parse();
    match args.command {
        Command::App { command } => run_app(command)
            .await
            .map(|()| false)
            .map_err(|e| error::AppError::GitHubApi(e.to_string())),
        Command::Org(command) => {
            let token = args
                .token
                .as_deref()
                .filter(|token| !token.trim().is_empty())
                .ok_or_else(|| {
                    error::AppError::GitHubApi(
                        "Set GITHUB_TOKEN or --token for organization sync commands".into(),
                    )
                })?;
            run_org(token, command).await
        }
    }
}

async fn run_app(command: AppCommand) -> anyhow::Result<()> {
    match command {
        AppCommand::Validate { manifest } => {
            github_app::load_manifest(&manifest)?;
            println!("App manifest is valid");
        }
        AppCommand::Create {
            org,
            manifest,
            credentials_dir,
        } => {
            let app = github_app::create(&org, &manifest, &credentials_dir).await?;
            println!("Created {} (App ID {})", app.slug, app.id);
            println!(
                "Install: https://github.com/apps/{}/installations/new",
                app.slug
            );
            println!(
                "Credentials saved privately in {}",
                credentials_dir.display()
            );
        }
    }
    Ok(())
}

/// Returns whether differences were found (only `diff` reports any).
async fn run_org(token: &str, command: OrgCommand) -> AppResult<bool> {
    match command {
        OrgCommand::Diff { config } => {
            info!("Starting gh-config-cli with command: diff, config: {}", config);
            client_for_config(token, &config)?.diff(&config).await
        }
        OrgCommand::Sync { config, dry_run } => {
            info!("Starting gh-config-cli with command: sync, config: {}", config);
            client_for_config(token, &config)?
                .sync(&config, dry_run)
                .await?;
            Ok(false)
        }
        OrgCommand::SyncFromOrg {
            config,
            dry_run,
            org,
        } => {
            info!(
                "Starting gh-config-cli with command: sync-from-org, config: {}",
                config
            );
            GitHubClient::new(token, &org)
                .generate_config_and_write(&config, dry_run)
                .await?;
            Ok(false)
        }
    }
}

/// A client for the org named in the config file, which must name one.
fn client_for_config(token: &str, config_path: &str) -> AppResult<GitHubClient> {
    let config = Config::from_file_with_defaults(config_path, None)?;
    if config.org.trim().is_empty() {
        error!(
            "Configuration file '{}' is missing the required 'org' key or its value is empty.",
            config_path
        );
        return Err(error::AppError::GitHubApi(
            "Missing or empty 'org' key in configuration file.".to_string(),
        ));
    }
    Ok(GitHubClient::new(token, &config.org))
}
