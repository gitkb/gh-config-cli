use anyhow::Result;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Config error: {0}")]
    Config(String),
    #[error("GitHub API error: {0}")]
    GitHubApi(String),
    #[error("GitHub API error: {message}")]
    GitHubResponse { status: reqwest::StatusCode, message: String },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_yaml::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error), // Added for serde_json errors
}

pub type AppResult<T> = Result<T, AppError>;
