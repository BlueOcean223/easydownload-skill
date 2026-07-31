use std::{
    env,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::{
    auth,
    errors::{AppError, Result},
    private_fs,
};

#[derive(Debug, Clone)]
pub(crate) struct CredentialStore {
    config_dir: PathBuf,
    cookies_path: Option<PathBuf>,
}

impl CredentialStore {
    pub(crate) fn new(config_dir: PathBuf, cookies_path: Option<PathBuf>) -> Self {
        Self {
            config_dir,
            cookies_path,
        }
    }

    pub(crate) fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub(crate) async fn cookie_for_reference(&self, auth_ref: &str, url: &Url) -> Result<String> {
        auth::validate_reference(auth_ref, url)?;
        self.cookie_header_for(url).await?.ok_or_else(|| {
            AppError::new(
                "auth.required",
                "the download source requires a local credential profile",
                false,
            )
            .with_context("authRef", auth_ref)
        })
    }

    pub(crate) async fn cookie_header_for(&self, url: &Url) -> Result<Option<String>> {
        if let Some(path) = self.cookies_path.as_deref()
            && let Ok(Some(header)) = load_cookie_file(path, url).await
        {
            return Ok(Some(header));
        }
        self.load_stored_bilibili_cookie().await
    }

    pub(crate) async fn bilibili_status(&self) -> Result<BilibiliAuthStatus> {
        let auth = self.load_auth_state().await?;
        Ok(BilibiliAuthStatus {
            logged_in: auth
                .bilibili
                .as_ref()
                .and_then(|auth| auth.sess_data.as_deref())
                .is_some_and(|value| !value.trim().is_empty()),
            updated_at: auth.bilibili.and_then(|auth| auth.updated_at),
        })
    }

    pub(crate) async fn save_bilibili_auth(
        &self,
        sess_data: &str,
        cookie_header: Option<&str>,
    ) -> Result<()> {
        let mut auth = self.load_auth_state().await?;
        auth.bilibili = Some(BilibiliAuth {
            sess_data: Some(sess_data.to_owned()),
            cookie_header: cookie_header.map(str::to_owned),
            updated_at: Some(
                OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .expect("UTC timestamp is RFC3339 compatible"),
            ),
        });
        self.write_auth_state(&auth).await
    }

    pub(crate) async fn clear_bilibili_auth(&self) -> Result<bool> {
        let mut auth = self.load_auth_state().await?;
        let cleared = auth.bilibili.take().is_some();
        self.write_auth_state(&auth).await?;
        Ok(cleared)
    }

    async fn load_stored_bilibili_cookie(&self) -> Result<Option<String>> {
        let auth = self.load_auth_state().await?;
        let Some(bilibili) = auth.bilibili else {
            return Ok(None);
        };
        if let Some(header) = bilibili
            .cookie_header
            .as_deref()
            .map(str::trim)
            .filter(|header| !header.is_empty())
        {
            return Ok(Some(normalize_bilibili_cookie_header(header)));
        }
        Ok(bilibili
            .sess_data
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!("SESSDATA={}", canonical_cookie_value(value))))
    }

    async fn load_auth_state(&self) -> Result<AuthState> {
        let path = self.config_dir.join("auth.json");
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AuthState::default());
            }
            Err(error) => return Err(AppError::filesystem("read credential store", &path, &error)),
        };
        serde_json::from_str(&text).map_err(|error| {
            AppError::new(
                "auth.store_invalid",
                format!("invalid local credential store: {error}"),
                false,
            )
            .with_context("path", path.to_string_lossy().to_string())
        })
    }

    async fn write_auth_state(&self, auth: &AuthState) -> Result<()> {
        let path = self.config_dir.join("auth.json");
        let mut bytes = serde_json::to_vec_pretty(auth).map_err(|error| {
            AppError::new(
                "auth.store_invalid",
                format!("failed to serialize local credential store: {error}"),
                false,
            )
        })?;
        bytes.push(b'\n');
        private_fs::atomic_write_private(&path, &bytes).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BilibiliAuthStatus {
    pub logged_in: bool,
    pub updated_at: Option<String>,
}

pub(crate) fn default_config_dir() -> PathBuf {
    if let Some(path) = env::var_os("EASYDL_CONFIG_DIR").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(path) = env::var_os("XDG_CONFIG_HOME").filter(|path| !path.is_empty()) {
        return PathBuf::from(path).join("easydl");
    }
    env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/easydl")
}

async fn load_cookie_file(path: &Path, url: &Url) -> Result<Option<String>> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|error| AppError::filesystem("read cookies file", path, &error))?;
    if let Some(header) = parse_raw_cookie_header(&text) {
        return Ok(Some(header));
    }

    let host = url.host_str().unwrap_or_default();
    let request_path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    let secure_request = url.scheme() == "https";
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let mut pairs = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || (trimmed.starts_with('#') && !trimmed.starts_with("#HttpOnly_")) {
            continue;
        }
        let fields = trimmed.split('\t').collect::<Vec<_>>();
        if fields.len() < 7 {
            continue;
        }
        let domain = fields[0].strip_prefix("#HttpOnly_").unwrap_or(fields[0]);
        let include_subdomains = fields[1].eq_ignore_ascii_case("true") || domain.starts_with('.');
        let cookie_path = if fields[2].is_empty() { "/" } else { fields[2] };
        let secure = fields[3].eq_ignore_ascii_case("true");
        let expiry = fields[4].parse::<i64>().ok();
        let name = fields[5];
        let value = fields[6..].join("\t");
        if name.is_empty()
            || (secure && !secure_request)
            || expiry.is_some_and(|expiry| expiry > 0 && expiry < now)
            || !domain_matches(host, domain, include_subdomains)
            || !request_path.starts_with(cookie_path)
        {
            continue;
        }
        pairs.push(format!("{name}={value}"));
    }
    Ok((!pairs.is_empty()).then(|| pairs.join("; ")))
}

fn parse_raw_cookie_header(text: &str) -> Option<String> {
    let lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.len() != 1 {
        return None;
    }
    let line = lines[0];
    if line.starts_with('#') || line.contains('\t') {
        return None;
    }
    let valid = line.split(';').all(|pair| {
        let pair = pair.trim();
        let Some((name, _)) = pair.split_once('=') else {
            return false;
        };
        !name.is_empty() && !name.chars().any(char::is_whitespace)
    });
    valid.then(|| line.to_owned())
}

fn normalize_bilibili_cookie_header(header: &str) -> String {
    header
        .split(';')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let Some((name, value)) = pair.split_once('=') else {
                return pair.to_owned();
            };
            if name == "SESSDATA" {
                format!("{name}={}", canonical_cookie_value(value))
            } else {
                pair.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn canonical_cookie_value(value: &str) -> String {
    let decoded = percent_decode(value.as_bytes());
    let mut output = String::new();
    for byte in decoded {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            output.push(char::from(byte));
        } else {
            output.push('%');
            output.push(hex(byte >> 4));
            output.push(hex(byte & 0x0f));
        }
    }
    output
}

fn percent_decode(value: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'%'
            && index + 2 < value.len()
            && let (Some(high), Some(low)) = (unhex(value[index + 1]), unhex(value[index + 2]))
        {
            output.push((high << 4) | low);
            index += 3;
            continue;
        }
        output.push(value[index]);
        index += 1;
    }
    output
}

fn hex(value: u8) -> char {
    char::from(if value < 10 {
        b'0' + value
    } else {
        b'A' + value - 10
    })
}

fn unhex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn domain_matches(hostname: &str, domain: &str, include_subdomains: bool) -> bool {
    let host = hostname.to_ascii_lowercase();
    let normalized = domain
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    !normalized.is_empty()
        && (host == normalized || (include_subdomains && host.ends_with(&format!(".{normalized}"))))
}

#[derive(Default, Deserialize, Serialize)]
struct AuthState {
    #[serde(skip_serializing_if = "Option::is_none")]
    bilibili: Option<BilibiliAuth>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct BilibiliAuth {
    sess_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cookie_header: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{CredentialStore, canonical_cookie_value, parse_raw_cookie_header};
    use tempfile::tempdir;
    use url::Url;

    #[test]
    fn canonicalizes_sessdata_without_double_encoding() {
        assert_eq!(canonical_cookie_value("sess%2Fdata"), "sess%2Fdata");
        assert_eq!(canonical_cookie_value("sess/data"), "sess%2Fdata");
    }

    #[test]
    fn accepts_a_single_raw_cookie_header() {
        assert_eq!(
            parse_raw_cookie_header("SESSDATA=abc; bili_jct=def\n"),
            Some("SESSDATA=abc; bili_jct=def".to_owned())
        );
        assert_eq!(parse_raw_cookie_header("# Netscape\n"), None);
    }

    #[tokio::test]
    async fn resolves_scoped_auth_references_from_the_existing_store() {
        let config = tempdir().unwrap();
        tokio::fs::write(
            config.path().join("auth.json"),
            r#"{"bilibili":{"sessData":"sess/data"}}"#,
        )
        .await
        .unwrap();
        let store = CredentialStore::new(config.path().to_path_buf(), None);
        assert_eq!(
            store
                .cookie_for_reference(
                    "bilibili:default",
                    &Url::parse("https://api.bilibili.com/file").unwrap(),
                )
                .await
                .unwrap(),
            "SESSDATA=sess%2Fdata"
        );
        assert_eq!(
            store
                .cookie_for_reference(
                    "bilibili:default",
                    &Url::parse("https://evil.example/file").unwrap(),
                )
                .await
                .unwrap_err()
                .info
                .code,
            "auth.scope_violation"
        );
    }

    #[tokio::test]
    async fn atomically_saves_reports_and_clears_bilibili_auth() {
        let config = tempdir().unwrap();
        let store = CredentialStore::new(config.path().to_path_buf(), None);
        store
            .save_bilibili_auth("sess/data", Some("SESSDATA=sess%2Fdata; sid=abc"))
            .await
            .unwrap();
        let status = store.bilibili_status().await.unwrap();
        assert!(status.logged_in);
        assert!(status.updated_at.is_some());
        let text = tokio::fs::read_to_string(config.path().join("auth.json"))
            .await
            .unwrap();
        assert!(text.contains("sess/data"));
        assert!(!text.contains(".tmp"));
        assert!(store.clear_bilibili_auth().await.unwrap());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &tokio::fs::read_to_string(config.path().join("auth.json"))
                    .await
                    .unwrap()
            )
            .unwrap(),
            serde_json::json!({})
        );
    }
}
