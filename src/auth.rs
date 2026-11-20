use url::Url;

use crate::errors::{AppError, Result};

const BILIBILI_DEFAULT: &str = "bilibili:default";
const BILIBILI_ORIGINS: &[&str] = &["bilibili.com", "bilivideo.com", "bilivideo.cn"];

pub(crate) fn validate_reference(auth_ref: &str, url: &Url) -> Result<()> {
    let auth_ref = auth_ref.trim();
    if auth_ref.is_empty() || !auth_ref.contains(':') {
        return Err(AppError::new(
            "auth.invalid_ref",
            "authRef must identify a credential adapter and profile",
            false,
        ));
    }
    match auth_ref {
        BILIBILI_DEFAULT => validate_bilibili_scope(auth_ref, url),
        _ => Err(AppError::new(
            "auth.unknown_ref",
            "authRef does not identify a supported credential profile",
            false,
        )
        .with_context("authRef", auth_ref)),
    }
}

fn validate_bilibili_scope(auth_ref: &str, url: &Url) -> Result<()> {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let secure = url.scheme() == "https";
    let standard_port = url.port().is_none() || url.port() == Some(443);
    let allowed_host = BILIBILI_ORIGINS
        .iter()
        .any(|allowed| domain_matches(&host, allowed));
    if secure && standard_port && allowed_host {
        return Ok(());
    }

    let mut error = AppError::new(
        "auth.scope_violation",
        "authRef is not allowed for the download source origin",
        false,
    )
    .with_context("authRef", auth_ref)
    .with_context("scheme", url.scheme())
    .with_context("host", host);
    if let Some(port) = url.port() {
        error = error.with_context("port", port);
    }
    Err(error)
}

fn domain_matches(host: &str, allowed: &str) -> bool {
    host == allowed
        || host
            .strip_suffix(allowed)
            .is_some_and(|prefix| prefix.ends_with('.'))
}
