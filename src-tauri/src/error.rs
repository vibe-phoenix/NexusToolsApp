use serde::Serialize;
use std::fmt;

pub type AppResult<T> = Result<T, AppError>;

/// Serializable command error. `code` is stable for the UI; `details` contains
/// actionable lines (for example paths that need approval or conflicting files).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    #[serde(default)]
    pub retryable: bool,
}

impl AppError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: Vec::new(),
            retryable: false,
        }
    }

    pub fn with_details(
        code: impl Into<String>,
        message: impl Into<String>,
        details: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: details.into_iter().map(Into::into).collect(),
            retryable: false,
        }
    }

    pub fn retryable(mut self, value: bool) -> Self {
        self.retryable = value;
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.details.push(detail.into());
        self
    }

    pub fn io(context: &str, error: impl fmt::Display) -> Self {
        Self::new("io_error", format!("{context}: {error}"))
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_data", message)
    }

    pub fn message_for(code: &str) -> &'static str {
        match code {
            "network" => "Could not reach GitHub.",
            "github_rate_limit" => {
                "GitHub's public API rate limit was reached. Try again later, or start NexusTools \
                 with NEXUSTOOLS_GITHUB_TOKEN set to a read-only token."
            }
            "repository_unavailable" => {
                "The repository is private, unavailable, or the referenced revision does not exist."
            }
            _ => "NexusTools could not complete that request.",
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self::new("io_error", value.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(value: serde_json::Error) -> Self {
        Self::new("invalid_json", format!("Invalid JSON: {value}"))
    }
}
