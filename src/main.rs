mod api_mapping;
mod config;
mod error;
mod github;
mod github_api_mapping_generated;

use clap::{Parser, Subcommand};
use config::Config;
use error::AppResult;
use gh_config::github_app;
use github::GitHubClient;
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

    let (command, config_path, _dry_run, _org) = match &args.command {
        Command::App { command } => {
            let result = match command {
                AppCommand::Validate { manifest } => {
                    github_app::load_manifest(manifest).map(|_| println!("App manifest is valid"))
                }
                AppCommand::Create {
                    org,
                    manifest,
                    credentials_dir,
                } => github_app::create(org, manifest, credentials_dir)
                    .await
                    .map(|app| {
                        println!("Created {} (App ID {})", app.slug, app.id);
                        println!(
                            "Install: https://github.com/apps/{}/installations/new",
                            app.slug
                        );
                        println!(
                            "Credentials saved privately in {}",
                            credentials_dir.display()
                        );
                    }),
            };
            return result
                .map(|_| false)
                .map_err(|e| error::AppError::GitHubApi(e.to_string()));
        }
        Command::Diff { config } => ("diff", config, false, None),
        Command::Sync { config, dry_run } => ("sync", config, *dry_run, None),
        Command::SyncFromOrg {
            config,
            dry_run,
            org,
        } => ("sync-from-org", config, *dry_run, Some(org)),
    };

    let token = args
        .token
        .as_deref()
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| {
            error::AppError::GitHubApi(
                "Set GITHUB_TOKEN or --token for organization sync commands".into(),
            )
        })?;

    info!(
        "Starting gh-config-cli with command: {}, config: {}",
        command, config_path
    );

    let mut client = match &args.command {
        Command::SyncFromOrg {
            config: _,
            dry_run: _,
            org,
        } => GitHubClient::new(token, org),
        _ => {
            // Load config first for Diff and Sync
            let config = Config::from_file_with_defaults(config_path, None)?;

            // --- Add Org Check ---
            if config.org.trim().is_empty() {
                error!("Configuration file '{}' is missing the required 'org' key or its value is empty.", config_path);
                // Use a specific error type if available, or a general one.
                // Using GitHubApi might be slightly misleading, but fits AppError enum.
                return Err(error::AppError::GitHubApi(
                    "Missing or empty 'org' key in configuration file.".to_string(),
                ));
            }
            // --- End Org Check ---

            GitHubClient::new(token, &config.org)
        }
    };

    match args.command {
        Command::App { .. } => unreachable!("App commands return before organization sync"),
        Command::Diff { config: _ } => client.diff(config_path).await,
        Command::Sync { config: _, dry_run } => {
            client.sync(config_path, dry_run).await?;
            Ok(false) // Sync completed, no diffs to report
        }
        Command::SyncFromOrg {
            config: _,
            dry_run,
            org: _,
        } => {
            client
                .generate_config_and_write(config_path, dry_run)
                .await?;
            Ok(false) // Sync-from-org completed, no diffs to report
        }
    }
}
