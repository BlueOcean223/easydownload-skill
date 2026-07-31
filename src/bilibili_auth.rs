use std::{path::PathBuf, time::Duration};

use qrcode::{QrCode, types::Color};
use reqwest::header::SET_COOKIE;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use url::Url;

use crate::{
    credentials::{CredentialStore, default_config_dir},
    errors::{AppError, Result},
    private_fs,
};

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";
const DEFAULT_BASE_URL: &str = "https://passport.bilibili.com/";
const LOGIN_COOKIE_NAMES: &[&str] = &[
    "SESSDATA",
    "DedeUserID",
    "DedeUserID__ckMd5",
    "bili_jct",
    "sid",
    "buvid3",
    "b_nut",
];
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct BilibiliAuthClient {
    client: reqwest::Client,
    base_url: Url,
    store: CredentialStore,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BilibiliQrSession {
    pub session_id: String,
    pub qrcode_key: String,
    pub url: String,
    pub qr_path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BilibiliQrPollResult {
    pub session_id: String,
    pub code: i64,
    pub message: String,
    pub logged_in: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cookie_names: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BilibiliLoginStatus {
    pub platform: String,
    pub logged_in: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BilibiliLogoutResult {
    pub platform: String,
    pub logged_in: bool,
    pub cleared: bool,
}

impl BilibiliAuthClient {
    pub fn new(
        config_dir: Option<PathBuf>,
        proxy: Option<String>,
        timeout: Duration,
    ) -> Result<Self> {
        Self::with_base_url(config_dir, proxy, timeout, DEFAULT_BASE_URL)
    }

    pub fn with_base_url(
        config_dir: Option<PathBuf>,
        proxy: Option<String>,
        timeout: Duration,
        base_url: &str,
    ) -> Result<Self> {
        let base_url = Url::parse(base_url).map_err(|error| {
            AppError::new(
                "platform.configuration",
                format!("invalid bilibili auth URL: {error}"),
                false,
            )
        })?;
        if !matches!(base_url.scheme(), "http" | "https") || base_url.host_str().is_none() {
            return Err(AppError::new(
                "platform.configuration",
                "bilibili auth URL must be an HTTP URL with a host",
                false,
            ));
        }
        let mut builder = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::limited(5));
        if let Some(proxy) = proxy
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy)
                    .map_err(|_| AppError::new("proxy.invalid", "invalid proxy URL", false))?,
            );
        }
        let client = builder
            .build()
            .map_err(|error| AppError::http_request(&error))?;
        Ok(Self {
            client,
            base_url,
            store: CredentialStore::new(config_dir.unwrap_or_else(default_config_dir), None),
        })
    }

    pub async fn create_session(&self) -> Result<BilibiliQrSession> {
        let payload: QrGenerateResponse = self
            .get_json(self.url("x/passport-login/web/qrcode/generate")?)
            .await?;
        let data = payload
            .data
            .ok_or_else(|| qr_create_error(&payload.message, payload.code))?;
        let url = data
            .url
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| qr_create_error(&payload.message, payload.code))?;
        let key = data
            .qrcode_key
            .filter(|value| valid_session_id(value))
            .ok_or_else(|| qr_create_error(&payload.message, payload.code))?;
        if payload.code != 0 {
            return Err(qr_create_error(&payload.message, payload.code));
        }
        let config_dir = self.store.config_dir().to_path_buf();
        let qr_dir = config_dir.join("qrcode");
        private_fs::create_private_dir(&qr_dir).await?;
        let qr_path = qr_dir.join(format!("{key}.png"));
        let png = render_qr_png(&url)?;
        private_fs::atomic_write_private(&qr_path, &png).await?;
        Ok(BilibiliQrSession {
            session_id: key.clone(),
            qrcode_key: key,
            url,
            qr_path: qr_path.to_string_lossy().to_string(),
        })
    }

    pub async fn poll_session(&self, session_id: &str) -> Result<BilibiliQrPollResult> {
        let session_id = session_id.trim();
        if !valid_session_id(session_id) {
            return Err(AppError::new(
                "auth.invalid_session",
                "invalid bilibili QR sessionId",
                false,
            ));
        }
        let mut url = self.url("x/passport-login/web/qrcode/poll")?;
        url.query_pairs_mut().append_pair("qrcode_key", session_id);
        let response = self
            .client
            .get(url)
            .header("User-Agent", USER_AGENT)
            .header("Referer", DEFAULT_BASE_URL)
            .send()
            .await
            .map_err(|error| AppError::http_request(&error))?;
        if !response.status().is_success() {
            return Err(AppError::http_status(response.status()));
        }
        let set_cookies = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let text = read_limited_text(response, MAX_RESPONSE_BYTES).await?;
        let payload: QrPollResponse = serde_json::from_str(&text).map_err(|error| {
            AppError::new(
                "platform.invalid_response",
                format!("invalid bilibili QR poll response: {error}"),
                false,
            )
        })?;
        if payload.code != 0 {
            return Err(AppError::new(
                "auth.remote_error",
                format!(
                    "bilibili QR poll failed: {}",
                    payload.message.unwrap_or_else(|| payload.code.to_string())
                ),
                false,
            ));
        }
        let data = payload.data.unwrap_or_default();
        let code = data.code.unwrap_or(-1);
        let message = data
            .message
            .unwrap_or_else(|| message_for_status(code).to_owned());
        let cookies = extract_login_cookies(&set_cookies, data.url.as_deref().unwrap_or_default());
        let cookie_header = (!cookies.is_empty()).then(|| {
            cookies
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; ")
        });
        let sess_data = cookies
            .iter()
            .find(|(name, _)| name == "SESSDATA")
            .map(|(_, value)| percent_decode_to_string(value));
        if code == 0 {
            let sess_data = sess_data.as_deref().ok_or_else(|| {
                AppError::new(
                    "auth.invalid_response",
                    "bilibili QR login succeeded but SESSDATA was not found",
                    false,
                )
            })?;
            self.store
                .save_bilibili_auth(sess_data, cookie_header.as_deref())
                .await?;
        }
        Ok(BilibiliQrPollResult {
            session_id: session_id.to_owned(),
            code,
            message,
            logged_in: code == 0,
            cookie_names: (code == 0)
                .then(|| cookies.iter().map(|(name, _)| name.clone()).collect()),
        })
    }

    pub async fn wait_for_login(
        &self,
        session_id: &str,
        interval: Duration,
        timeout: Duration,
    ) -> Result<BilibiliQrPollResult> {
        let started = std::time::Instant::now();
        loop {
            let result = self.poll_session(session_id).await?;
            if result.logged_in {
                return Ok(result);
            }
            if result.code == 86038 {
                return Err(AppError::new(
                    "auth.qr_expired",
                    "bilibili QR code expired",
                    false,
                ));
            }
            if started.elapsed() >= timeout {
                return Err(AppError::new(
                    "auth.timeout",
                    "bilibili QR login timed out",
                    true,
                ));
            }
            sleep(interval).await;
        }
    }

    pub async fn status(&self) -> Result<BilibiliLoginStatus> {
        let status = self.store.bilibili_status().await?;
        Ok(BilibiliLoginStatus {
            platform: "bilibili".to_owned(),
            logged_in: status.logged_in,
            updated_at: status.updated_at,
        })
    }

    pub async fn logout(&self) -> Result<BilibiliLogoutResult> {
        Ok(BilibiliLogoutResult {
            platform: "bilibili".to_owned(),
            logged_in: false,
            cleared: self.store.clear_bilibili_auth().await?,
        })
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: Url) -> Result<T> {
        let response = self
            .client
            .get(url)
            .header("User-Agent", USER_AGENT)
            .header("Referer", DEFAULT_BASE_URL)
            .send()
            .await
            .map_err(|error| AppError::http_request(&error))?;
        if !response.status().is_success() {
            return Err(AppError::http_status(response.status()));
        }
        let text = read_limited_text(response, MAX_RESPONSE_BYTES).await?;
        serde_json::from_str(&text).map_err(|error| {
            AppError::new(
                "platform.invalid_response",
                format!("invalid bilibili auth response: {error}"),
                false,
            )
        })
    }

    fn url(&self, path: &str) -> Result<Url> {
        self.base_url.join(path).map_err(|_| {
            AppError::new(
                "platform.configuration",
                "invalid bilibili auth endpoint",
                false,
            )
        })
    }
}

fn render_qr_png(value: &str) -> Result<Vec<u8>> {
    let code = QrCode::new(value.as_bytes()).map_err(|error| {
        AppError::new(
            "auth.qr_render_failed",
            format!("failed to encode bilibili QR code: {error}"),
            false,
        )
    })?;
    let modules = code.width();
    let margin = 4_usize;
    let scale = (320 / (modules + margin * 2)).max(1);
    let width = (modules + margin * 2) * scale;
    let colors = code.to_colors();
    let mut pixels = vec![255_u8; width * width];
    for y in 0..modules {
        for x in 0..modules {
            if colors[y * modules + x] != Color::Dark {
                continue;
            }
            let start_x = (x + margin) * scale;
            let start_y = (y + margin) * scale;
            for pixel_y in start_y..start_y + scale {
                for pixel_x in start_x..start_x + scale {
                    pixels[pixel_y * width + pixel_x] = 0;
                }
            }
        }
    }
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, width as u32, width as u32);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|error| {
            AppError::new(
                "auth.qr_render_failed",
                format!("failed to create bilibili QR PNG: {error}"),
                false,
            )
        })?;
        writer.write_image_data(&pixels).map_err(|error| {
            AppError::new(
                "auth.qr_render_failed",
                format!("failed to encode bilibili QR PNG: {error}"),
                false,
            )
        })?;
    }
    Ok(output)
}

fn extract_login_cookies(set_cookies: &[String], redirect_url: &str) -> Vec<(String, String)> {
    let mut output = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for name in LOGIN_COOKIE_NAMES {
        if let Some(value) = raw_query_value(redirect_url, name)
            && seen.insert((*name).to_owned())
        {
            output.push(((*name).to_owned(), value));
        }
    }
    for name in LOGIN_COOKIE_NAMES {
        if seen.contains(*name) {
            continue;
        }
        let marker = format!("{name}=");
        let value = set_cookies.iter().find_map(|header| {
            header
                .split(|ch: char| ch == ',' || ch == ';' || ch.is_whitespace())
                .find_map(|part| part.strip_prefix(&marker))
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        });
        if let Some(value) = value {
            seen.insert((*name).to_owned());
            output.push(((*name).to_owned(), value));
        }
    }
    output
}

fn raw_query_value(value: &str, name: &str) -> Option<String> {
    let query = value.split_once('?')?.1.split('#').next()?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        key.eq_ignore_ascii_case(name).then(|| value.to_owned())
    })
}

fn percent_decode_to_string(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (unhex(bytes[index + 1]), unhex(bytes[index + 2]))
        {
            output.push((high << 4) | low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn unhex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn message_for_status(code: i64) -> &'static str {
    match code {
        0 => "login successful",
        86038 => "QR code expired",
        86090 => "QR code scanned; waiting for confirmation",
        86101 => "QR code not scanned",
        _ => "unknown QR status",
    }
}

fn qr_create_error(message: &Option<String>, code: i64) -> AppError {
    AppError::new(
        "auth.qr_create_failed",
        format!(
            "bilibili QR create failed: {}",
            message.as_deref().unwrap_or(if code == 0 {
                "invalid response"
            } else {
                "remote error"
            })
        ),
        false,
    )
    .with_context("remoteCode", code)
}

async fn read_limited_text(response: reqwest::Response, limit: usize) -> Result<String> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(response_too_large(limit));
    }
    use futures_util::StreamExt;
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| AppError::http_request(&error))?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(response_too_large(limit));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn response_too_large(limit: usize) -> AppError {
    AppError::new(
        "platform.response_too_large",
        "platform response exceeded the configured size limit",
        false,
    )
    .with_context("maximum", limit)
}

#[derive(Deserialize)]
struct QrGenerateResponse {
    code: i64,
    message: Option<String>,
    data: Option<QrGenerateData>,
}

#[derive(Deserialize)]
struct QrGenerateData {
    url: Option<String>,
    qrcode_key: Option<String>,
}

#[derive(Deserialize)]
struct QrPollResponse {
    code: i64,
    message: Option<String>,
    data: Option<QrPollData>,
}

#[derive(Default, Deserialize)]
struct QrPollData {
    code: Option<i64>,
    message: Option<String>,
    url: Option<String>,
    #[allow(dead_code)]
    refresh_token: Option<String>,
    #[allow(dead_code)]
    timestamp: Option<i64>,
}
