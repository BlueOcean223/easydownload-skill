use std::{collections::BTreeMap, error::Error, fmt};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ErrorInfo {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context: BTreeMap<String, Value>,
}

impl ErrorInfo {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
            context: BTreeMap::new(),
        }
    }

    pub fn with_context(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.context.insert(key.into(), value.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct AppError {
    pub info: ErrorInfo,
}

impl AppError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            info: ErrorInfo::new(code, message, retryable),
        }
    }

    pub fn with_context(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.info = self.info.with_context(key, value);
        self
    }

    pub fn invalid_plan(message: impl Into<String>) -> Self {
        Self::new("plan.invalid", message, false)
    }

    pub fn http_status(status: reqwest::StatusCode) -> Self {
        let (code, retryable) = match status.as_u16() {
            401 => ("http.unauthorized", false),
            403 => ("http.forbidden", false),
            404 => ("http.not_found", false),
            408 => ("http.timeout", true),
            429 => ("http.rate_limited", true),
            500..=599 => ("http.server_error", true),
            _ => ("http.status", false),
        };
        Self::new(code, format!("HTTP {}", status.as_u16()), retryable)
            .with_context("status", status.as_u16())
    }

    pub fn http_request(error: &reqwest::Error) -> Self {
        if error.is_timeout() {
            return Self::new("http.timeout", "HTTP request timed out", true);
        }
        if error.is_connect() {
            return Self::new(
                "http.connect_failed",
                "failed to connect to remote host",
                true,
            );
        }
        if error.is_body() || error.is_decode() {
            return Self::new(
                "http.response_failed",
                "failed while reading the HTTP response",
                true,
            );
        }
        Self::new("http.request_failed", "HTTP request failed", true)
    }

    pub fn filesystem(action: &str, path: &std::path::Path, error: &std::io::Error) -> Self {
        Self::new(
            "filesystem.io",
            format!("failed to {action} {}: {error}", path.display()),
            false,
        )
        .with_context("action", action)
        .with_context("path", path.to_string_lossy().to_string())
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.info.message)
    }
}

impl Error for AppError {}

impl From<ErrorInfo> for AppError {
    fn from(info: ErrorInfo) -> Self {
        Self { info }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ErrorDocument {
    pub schema_version: u32,
    pub error: ErrorInfo,
}

impl From<AppError> for ErrorDocument {
    fn from(error: AppError) -> Self {
        Self {
            schema_version: 2,
            error: error.info,
        }
    }
}
