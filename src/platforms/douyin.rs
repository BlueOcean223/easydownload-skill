use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

use futures_util::StreamExt;
use regex::Regex;
use reqwest::{
    Response, StatusCode,
    header::{CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE, SET_COOKIE},
    redirect::Policy,
};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use url::Url;

use crate::{
    credentials::CredentialStore,
    domain::{
        ArchiveEntry, DownloadPlan, DownloadSource, DownloadTask, ImageChoice, InspectDocument,
        MediaChoices, MediaKind, MediaRequirements, MediaView, OutputSpec, PlanSource,
        QualityChoice, SCHEMA_VERSION,
    },
    errors::{AppError, Result},
    platforms::{AlbumOutput, PlanSelection, ResolveOptions},
};

const MOBILE_USER_AGENT: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 16_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.5 Mobile/15E148 Safari/604.1";
const DESKTOP_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";
const DEFAULT_REFERER: &str = "https://www.douyin.com/";
const DEFAULT_SHARE_BASE: &str = "https://www.iesdouyin.com/share/";
const DEFAULT_WEB_DETAIL_URL: &str = "https://www.douyin.com/aweme/v1/web/aweme/detail/";
const DEFAULT_PLAY_BASE_URL: &str = "https://aweme.snssdk.com/aweme/v1/play/";
const DEFAULT_TTWID_URL: &str = "https://ttwid.bytedance.com/ttwid/union/register/";
const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
const MAX_IMAGES: usize = 10_000;

pub fn matches(input: &str) -> bool {
    if looks_like_aweme_id(input.trim()) {
        return true;
    }
    static HOST: OnceLock<Regex> = OnceLock::new();
    HOST.get_or_init(|| {
        Regex::new(
            r"(?i)(?:^|//)(?:[^/]+\.)?(?:douyin\.com|iesdouyin\.com)(?:[/:?#]|$)|(?:^|//)v\.douyin\.com(?:[/:?#]|$)",
        )
        .expect("valid douyin host regex")
    })
    .is_match(input)
}

pub fn looks_like_aweme_id(value: &str) -> bool {
    static AWEME_ID: OnceLock<Regex> = OnceLock::new();
    AWEME_ID
        .get_or_init(|| Regex::new(r"^\d{10,30}$").expect("valid douyin id regex"))
        .is_match(value)
}

#[derive(Clone)]
pub struct DouyinResolver {
    page_client: reqwest::Client,
    redirect_client: reqwest::Client,
    endpoints: Endpoints,
    credentials: CredentialStore,
    extra_douyin_hosts: Vec<String>,
    extra_short_hosts: Vec<String>,
}

#[derive(Clone)]
struct Endpoints {
    share_base: Url,
    web_detail: Url,
    play_base: Url,
    ttwid: Url,
}

impl DouyinResolver {
    pub fn new(options: ResolveOptions) -> Result<Self> {
        Self::from_endpoints(
            options,
            Endpoints {
                share_base: parse_endpoint(DEFAULT_SHARE_BASE)?,
                web_detail: parse_endpoint(DEFAULT_WEB_DETAIL_URL)?,
                play_base: parse_endpoint(DEFAULT_PLAY_BASE_URL)?,
                ttwid: parse_endpoint(DEFAULT_TTWID_URL)?,
            },
            Vec::new(),
            Vec::new(),
        )
    }

    pub fn with_base_url(
        options: ResolveOptions,
        base_url: &str,
        short_hosts: &[&str],
    ) -> Result<Self> {
        let base = parse_endpoint(base_url)?;
        let base_host = base.host_str().unwrap().to_ascii_lowercase();
        let endpoint = |path: &str| {
            let mut url = base.clone();
            url.set_path(path);
            url.set_query(None);
            url.set_fragment(None);
            url
        };
        Self::from_endpoints(
            options,
            Endpoints {
                share_base: endpoint("/share/"),
                web_detail: endpoint("/aweme/v1/web/aweme/detail/"),
                play_base: endpoint("/aweme/v1/play/"),
                ttwid: endpoint("/ttwid/union/register/"),
            },
            vec![base_host],
            short_hosts
                .iter()
                .map(|host| host.trim().to_ascii_lowercase())
                .filter(|host| !host.is_empty())
                .collect(),
        )
    }

    fn from_endpoints(
        options: ResolveOptions,
        endpoints: Endpoints,
        extra_douyin_hosts: Vec<String>,
        extra_short_hosts: Vec<String>,
    ) -> Result<Self> {
        let page_client = build_client(&options, Policy::limited(10))?;
        let redirect_client = build_client(&options, Policy::none())?;
        let credentials = CredentialStore::new(options.config_dir, options.cookies_path);
        Ok(Self {
            page_client,
            redirect_client,
            endpoints,
            credentials,
            extra_douyin_hosts,
            extra_short_hosts,
        })
    }

    pub async fn resolve(&self, input: &str) -> Result<DouyinMedia> {
        let aweme_id = self.parse_input(input).await?;
        self.fetch_media(&aweme_id).await
    }

    async fn parse_input(&self, input: &str) -> Result<String> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(AppError::new(
                "input.url_not_found",
                "no douyin URL found in input",
                false,
            ));
        }
        if looks_like_aweme_id(trimmed) {
            return Ok(trimmed.to_owned());
        }
        let mut current = self.extract_share_url(trimmed).ok_or_else(|| {
            AppError::new("input.url_not_found", "no douyin URL found in input", false)
        })?;
        for _ in 0..=5 {
            let host = current
                .host_str()
                .map(str::to_ascii_lowercase)
                .ok_or_else(|| AppError::new("input.invalid_url", "invalid URL host", false))?;
            if self.is_short_host(&host) {
                current = self.resolve_short_link(&current).await?;
                continue;
            }
            if !self.is_douyin_host(&host) {
                return Err(
                    AppError::new("input.invalid_host", "invalid douyin URL host", false)
                        .with_context("host", host),
                );
            }
            return extract_aweme_id(&current).ok_or_else(|| {
                AppError::new(
                    "platform.identifier_not_found",
                    "douyin aweme_id not found",
                    false,
                )
            });
        }
        Err(AppError::new(
            "platform.short_link_failed",
            "douyin short link exceeded the redirect limit",
            false,
        ))
    }

    fn extract_share_url(&self, input: &str) -> Option<Url> {
        if let Ok(url) = Url::parse(trim_url(input))
            && matches!(url.scheme(), "http" | "https")
            && url
                .host_str()
                .is_some_and(|host| self.is_acceptable_host(&host.to_ascii_lowercase()))
        {
            return Some(url);
        }
        url_regex().find_iter(input).find_map(|candidate| {
            let url = Url::parse(trim_url(candidate.as_str())).ok()?;
            url.host_str()
                .is_some_and(|host| self.is_acceptable_host(&host.to_ascii_lowercase()))
                .then_some(url)
        })
    }

    async fn resolve_short_link(&self, url: &Url) -> Result<Url> {
        let response = self
            .redirect_client
            .get(url.clone())
            .headers(page_headers(None))
            .send()
            .await
            .map_err(|error| AppError::http_request(&error))?;
        let status = response.status();
        if status.is_redirection()
            && let Some(location) = response.headers().get(LOCATION)
        {
            let location = location.to_str().map_err(|_| {
                AppError::new(
                    "platform.invalid_response",
                    "douyin redirect contains an invalid Location header",
                    false,
                )
            })?;
            return url.join(location).map_err(|_| {
                AppError::new(
                    "platform.invalid_response",
                    "douyin redirect contains an invalid URL",
                    false,
                )
            });
        }
        Err(AppError::new(
            "platform.short_link_failed",
            format!(
                "unexpected douyin short-link HTTP status {}",
                status.as_u16()
            ),
            status.is_server_error(),
        )
        .with_context("status", status.as_u16()))
    }

    async fn fetch_media(&self, aweme_id: &str) -> Result<DouyinMedia> {
        let mut first_error = None;
        for kind in ["video", "note"] {
            match self.fetch_share_page_item(aweme_id, kind).await {
                Ok(item) => match self.build_media(item, aweme_id).await {
                    Ok(media) if media.is_usable() => return Ok(media),
                    Ok(_) => remember_first(&mut first_error, no_playable_error()),
                    Err(error) => remember_first(&mut first_error, error),
                },
                Err(error) => remember_first(&mut first_error, error),
            }
        }
        match self.fetch_web_detail_item(aweme_id).await {
            Ok(item) => match self.build_media(item, aweme_id).await {
                Ok(media) if media.is_usable() => return Ok(media),
                Ok(_) => remember_first(&mut first_error, no_playable_error()),
                Err(error) => remember_first(&mut first_error, error),
            },
            Err(error) => remember_first(&mut first_error, error),
        }
        let first = first_error.unwrap_or_else(no_playable_error);
        Err(AppError::new(
            first.info.code,
            format!("douyin item fetch failed: {}", first.info.message),
            first.info.retryable,
        ))
    }

    async fn fetch_share_page_item(&self, aweme_id: &str, kind: &str) -> Result<Item> {
        match self.fetch_share_page_item_once(aweme_id, kind, None).await {
            Ok(item) => Ok(item),
            Err(error) if error.info.code == "platform.router_data_not_found" => {
                let Some(cookie) = self.create_ttwid_cookie().await.ok().flatten() else {
                    return Err(error);
                };
                self.fetch_share_page_item_once(aweme_id, kind, Some(&cookie))
                    .await
            }
            Err(error) => Err(error),
        }
    }

    async fn fetch_share_page_item_once(
        &self,
        aweme_id: &str,
        kind: &str,
        cookie: Option<&str>,
    ) -> Result<Item> {
        let url = self
            .endpoints
            .share_base
            .join(&format!("{kind}/{aweme_id}/?app=aweme"))
            .map_err(|_| endpoint_error())?;
        let explicit_cookie = self.credentials.explicit_cookie_header_for(&url).await?;
        let merged_cookie = merge_cookie_headers(explicit_cookie.as_deref(), cookie);
        let response = self
            .page_client
            .get(url)
            .headers(page_headers(merged_cookie.as_deref()))
            .send()
            .await
            .map_err(|error| AppError::http_request(&error))?;
        match response.status() {
            StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => {
                return Err(AppError::new(
                    "http.rate_limited",
                    "douyin request was rate limited",
                    true,
                ));
            }
            status if !status.is_success() => return Err(AppError::http_status(status)),
            _ => {}
        }
        let html = read_limited_text(response, MAX_RESPONSE_BYTES).await?;
        let router = extract_object_after_marker(&html, "window._ROUTER_DATA").map_err(|_| {
            AppError::new(
                "platform.router_data_not_found",
                "douyin router data not found",
                false,
            )
        })?;
        let payload: Value = serde_json::from_str(router).map_err(|error| {
            AppError::new(
                "platform.invalid_response",
                format!("invalid douyin router data: {error}"),
                false,
            )
        })?;
        let loader_data = payload
            .get("loaderData")
            .and_then(Value::as_object)
            .ok_or_else(router_item_error)?;
        for container in loader_data.values().filter_map(Value::as_object) {
            let Some(item) = container
                .get("videoInfoRes")
                .and_then(Value::as_object)
                .and_then(|result| result.get("item_list"))
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(Value::as_object)
            else {
                continue;
            };
            if !item.is_empty() {
                return Ok(item.clone());
            }
        }
        Err(router_item_error())
    }

    async fn fetch_web_detail_item(&self, aweme_id: &str) -> Result<Item> {
        let mut url = self.endpoints.web_detail.clone();
        url.query_pairs_mut()
            .append_pair("aweme_id", aweme_id)
            .append_pair("device_platform", "webapp")
            .append_pair("aid", "6383")
            .append_pair("channel", "channel_pc_web");
        let mut headers = web_detail_headers(aweme_id);
        if let Some(cookie) = self.credentials.explicit_cookie_header_for(&url).await?
            && let Ok(value) = reqwest::header::HeaderValue::from_str(&cookie)
        {
            headers.insert(reqwest::header::COOKIE, value);
        }
        let response = self
            .page_client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|error| AppError::http_request(&error))?;
        match response.status() {
            StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => {
                return Err(AppError::new(
                    "http.rate_limited",
                    "douyin web detail request was rate limited",
                    true,
                ));
            }
            status if !status.is_success() => return Err(AppError::http_status(status)),
            _ => {}
        }
        let text = read_limited_text(response, MAX_RESPONSE_BYTES).await?;
        if text.trim().is_empty() {
            return Err(AppError::new(
                "platform.invalid_response",
                "douyin web detail returned an empty response",
                false,
            ));
        }
        let payload: Value = serde_json::from_str(&text).map_err(|error| {
            AppError::new(
                "platform.invalid_response",
                format!("invalid douyin web detail response: {error}"),
                false,
            )
        })?;
        let status_code = flexible_i64(payload.get("status_code"));
        if status_code != 0 {
            return Err(AppError::new(
                "platform.remote_error",
                format!(
                    "douyin web detail status {status_code}: {}",
                    string_value(payload.get("status_msg"))
                        .unwrap_or_else(|| "unknown error".to_owned())
                ),
                false,
            ));
        }
        if let Some(item) = payload.get("aweme_detail").and_then(Value::as_object)
            && !item.is_empty()
        {
            return Ok(item.clone());
        }
        if let Some(item) = payload
            .get("item_list")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(Value::as_object)
            && !item.is_empty()
        {
            return Ok(item.clone());
        }
        Err(AppError::new(
            "platform.not_found",
            "douyin web detail item not found",
            false,
        ))
    }

    async fn create_ttwid_cookie(&self) -> Result<Option<String>> {
        let response = self
            .page_client
            .post(self.endpoints.ttwid.clone())
            .header("User-Agent", MOBILE_USER_AGENT)
            .header("Content-Type", "application/json")
            .json(&json!({
                "region": "cn",
                "aid": 1768,
                "needFid": false,
                "service": "www.ixigua.com",
                "migrate_info": { "ticket": "", "source": "node" },
                "cbUrlProtocol": "https",
                "union": true
            }))
            .send()
            .await
            .map_err(|error| AppError::http_request(&error))?;
        if !response.status().is_success() {
            return Err(AppError::http_status(response.status()));
        }
        for value in response.headers().get_all(SET_COOKIE) {
            let Ok(value) = value.to_str() else {
                continue;
            };
            if let Some(token) = ttwid_regex()
                .captures(value)
                .and_then(|capture| capture.get(1))
            {
                return Ok(Some(format!("ttwid={}", token.as_str())));
            }
        }
        Ok(None)
    }

    async fn build_media(&self, item: Item, fallback_id: &str) -> Result<DouyinMedia> {
        let id = first_non_empty([
            string_value(item.get("aweme_id")).as_deref(),
            Some(fallback_id),
        ]);
        let title = first_non_empty([string_value(item.get("desc")).as_deref(), Some(id.as_str())]);
        let author = item
            .get("author")
            .and_then(Value::as_object)
            .and_then(|author| string_value(author.get("nickname")));
        let video = item
            .get("video")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let images = parse_images(&item, &self.endpoints.play_base)?;
        let is_album = flexible_i64(item.get("aweme_type")) == 68 || !images.is_empty();
        if is_album {
            let cover = first_url(
                video
                    .get("cover")
                    .and_then(Value::as_object)
                    .and_then(|cover| cover.get("url_list")),
            );
            return Ok(DouyinMedia {
                id,
                title,
                author,
                cover,
                kind: DouyinMediaKind::Album { images },
            });
        }

        let mut streams = parse_streams(&video);
        if streams.is_empty() {
            let play_addr = video
                .get("play_addr")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let uri = string_value(play_addr.get("uri")).unwrap_or_default();
            let width = positive_u32(play_addr.get("width"))
                .or_else(|| positive_u32(video.get("width")))
                .unwrap_or_default();
            let height = positive_u32(play_addr.get("height"))
                .or_else(|| positive_u32(video.get("height")))
                .unwrap_or_default();
            streams = self.probe_ratio_streams(&uri, width, height).await;
        }
        let cover = first_url(
            video
                .get("cover")
                .and_then(Value::as_object)
                .and_then(|cover| cover.get("url_list")),
        );
        Ok(DouyinMedia {
            id,
            title,
            author,
            cover,
            kind: DouyinMediaKind::Video { streams },
        })
    }

    async fn probe_ratio_streams(
        &self,
        uri: &str,
        source_width: u32,
        source_height: u32,
    ) -> Vec<DouyinStream> {
        if uri.trim().is_empty() {
            return Vec::new();
        }
        let max_source_resolution = if source_width > 0 && source_height > 0 {
            source_width.min(source_height)
        } else {
            0
        };
        let mut seen_sizes = BTreeSet::new();
        let mut streams = Vec::new();
        for ratio in ["1080p", "720p", "540p", "480p", "360p"] {
            let resolution = ratio.trim_end_matches('p').parse::<u32>().unwrap();
            if max_source_resolution > 0 && resolution > max_source_resolution {
                continue;
            }
            let url = build_play_url(&self.endpoints.play_base, uri, Some(ratio));
            let Ok(response) = self
                .page_client
                .get(url.clone())
                .headers(download_headers())
                .header(RANGE, "bytes=0-1")
                .header("Accept", "*/*")
                .send()
                .await
            else {
                continue;
            };
            let total = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_content_range_total)
                .unwrap_or_default();
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            if response.status() != StatusCode::PARTIAL_CONTENT
                || total <= 2
                || !is_video_like_content_type(content_type)
                || !seen_sizes.insert(total)
            {
                continue;
            }
            let (width, height) = dimensions_for_ratio(resolution, source_width, source_height);
            streams.push(DouyinStream {
                id: ratio.to_owned(),
                label: ratio.to_owned(),
                width: positive_dimension(width),
                height: positive_dimension(height),
                size: Some(total),
                urls: vec![url.to_string()],
                bitrate: 0,
            });
        }
        sort_streams(streams)
    }

    fn is_acceptable_host(&self, host: &str) -> bool {
        self.is_short_host(host) || self.is_douyin_host(host)
    }

    fn is_douyin_host(&self, host: &str) -> bool {
        domain_matches(host, "douyin.com")
            || domain_matches(host, "iesdouyin.com")
            || self
                .extra_douyin_hosts
                .iter()
                .any(|allowed| host == allowed)
    }

    fn is_short_host(&self, host: &str) -> bool {
        host == "v.douyin.com" || self.extra_short_hosts.iter().any(|allowed| host == allowed)
    }
}

#[derive(Debug, Clone)]
pub struct DouyinMedia {
    id: String,
    title: String,
    author: Option<String>,
    cover: Option<String>,
    kind: DouyinMediaKind,
}

#[derive(Debug, Clone)]
enum DouyinMediaKind {
    Video { streams: Vec<DouyinStream> },
    Album { images: Vec<DouyinImage> },
}

#[derive(Debug, Clone)]
struct DouyinStream {
    id: String,
    label: String,
    width: Option<u32>,
    height: Option<u32>,
    size: Option<u64>,
    urls: Vec<String>,
    bitrate: u64,
}

#[derive(Debug, Clone)]
struct DouyinImage {
    index: usize,
    urls: Vec<String>,
    video_url: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
}

impl DouyinMedia {
    fn is_usable(&self) -> bool {
        match &self.kind {
            DouyinMediaKind::Video { streams } => !streams.is_empty(),
            DouyinMediaKind::Album { images } => !images.is_empty(),
        }
    }

    pub fn inspect(&self) -> InspectDocument {
        let (kind, qualities, images) = match &self.kind {
            DouyinMediaKind::Video { streams } => (
                MediaKind::Video,
                streams
                    .iter()
                    .enumerate()
                    .map(|(index, stream)| QualityChoice {
                        id: stream.id.clone(),
                        label: stream.label.clone(),
                        recommended: index == 0,
                        width: stream.width,
                        height: stream.height,
                        size: stream.size,
                    })
                    .collect(),
                Vec::new(),
            ),
            DouyinMediaKind::Album { images } => (
                MediaKind::Album,
                Vec::new(),
                images
                    .iter()
                    .map(|image| ImageChoice {
                        id: format!("image-{}", image.index + 1),
                        index: (image.index + 1) as u32,
                        width: image.width,
                        height: image.height,
                    })
                    .collect(),
            ),
        };
        InspectDocument {
            schema_version: SCHEMA_VERSION,
            platform: "douyin".to_owned(),
            media: MediaView {
                id: self.id.clone(),
                kind,
                title: self.title.clone(),
                author: self.author.clone(),
                cover: self.cover.clone(),
            },
            choices: MediaChoices {
                qualities,
                parts: Vec::new(),
                images,
            },
            requirements: MediaRequirements {
                login: false,
                ffmpeg: false,
            },
        }
    }

    pub fn create_plan(&self, selection: &PlanSelection) -> Result<DownloadPlan> {
        let tasks = match &self.kind {
            DouyinMediaKind::Video { streams } => {
                let stream = pick_stream(streams, selection.quality.as_deref().unwrap_or("best"))?;
                let relative_path = selection.output.clone().unwrap_or_else(|| {
                    format!(
                        "{}.{}",
                        self.file_stem(),
                        extension_from_url(&stream.urls[0], "mp4")
                    )
                });
                vec![DownloadTask::File {
                    id: self.id.clone(),
                    sources: download_sources(&stream.urls),
                    output: OutputSpec { relative_path },
                }]
            }
            DouyinMediaKind::Album { images } => self.create_album_tasks(images, selection)?,
        };
        let plan = DownloadPlan {
            schema_version: SCHEMA_VERSION,
            created_at: OffsetDateTime::now_utc(),
            expires_at: None,
            source: PlanSource {
                platform: "douyin".to_owned(),
                media_id: self.id.clone(),
            },
            tasks,
        };
        plan.validate()?;
        Ok(plan)
    }

    fn create_album_tasks(
        &self,
        images: &[DouyinImage],
        selection: &PlanSelection,
    ) -> Result<Vec<DownloadTask>> {
        let selected = select_images(images, selection.images.as_deref())?;
        match selection.album_output {
            AlbumOutput::Zip => {
                let mut entries = Vec::new();
                for image in selected {
                    append_archive_entries(&mut entries, image);
                }
                if entries.is_empty() {
                    return Err(AppError::new(
                        "platform.no_media",
                        "douyin album has no downloadable entries",
                        false,
                    ));
                }
                let relative_path = selection
                    .output
                    .clone()
                    .unwrap_or_else(|| format!("{}.zip", self.file_stem()));
                Ok(vec![DownloadTask::Archive {
                    id: self.id.clone(),
                    entries,
                    output: OutputSpec { relative_path },
                }])
            }
            AlbumOutput::Directory => {
                let directory = selection.output.clone().unwrap_or_else(|| self.file_stem());
                let mut tasks = Vec::new();
                for image in selected {
                    append_file_tasks(&mut tasks, &self.id, &directory, image);
                }
                if tasks.is_empty() {
                    return Err(AppError::new(
                        "platform.no_media",
                        "douyin album has no downloadable entries",
                        false,
                    ));
                }
                Ok(tasks)
            }
        }
    }

    fn file_stem(&self) -> String {
        safe_file_stem(
            &[
                self.author.as_deref(),
                Some(self.title.as_str()),
                Some(self.id.as_str()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("_"),
            &self.id,
        )
    }
}

fn append_archive_entries(entries: &mut Vec<ArchiveEntry>, image: &DouyinImage) {
    let prefix = format!("{:02}", image.index + 1);
    if let Some(url) = image.urls.first() {
        entries.push(ArchiveEntry {
            id: format!("image-{}", image.index + 1),
            path: format!("{prefix}.{}", extension_from_url(url, "jpg")),
            sources: download_sources(&image.urls),
        });
    }
    if let Some(url) = image.video_url.as_deref() {
        let suffix = if image.urls.is_empty() { "" } else { "_video" };
        entries.push(ArchiveEntry {
            id: format!("image-{}-video", image.index + 1),
            path: format!("{prefix}{suffix}.{}", extension_from_url(url, "mp4")),
            sources: single_download_source(url),
        });
    }
}

fn append_file_tasks(
    tasks: &mut Vec<DownloadTask>,
    media_id: &str,
    directory: &str,
    image: &DouyinImage,
) {
    let prefix = format!("{:02}", image.index + 1);
    if let Some(url) = image.urls.first() {
        tasks.push(DownloadTask::File {
            id: format!("{media_id}:image-{}", image.index + 1),
            sources: download_sources(&image.urls),
            output: OutputSpec {
                relative_path: format!("{directory}/{prefix}.{}", extension_from_url(url, "jpg")),
            },
        });
    }
    if let Some(url) = image.video_url.as_deref() {
        let suffix = if image.urls.is_empty() { "" } else { "_video" };
        tasks.push(DownloadTask::File {
            id: format!("{media_id}:image-{}-video", image.index + 1),
            sources: single_download_source(url),
            output: OutputSpec {
                relative_path: format!(
                    "{directory}/{prefix}{suffix}.{}",
                    extension_from_url(url, "mp4")
                ),
            },
        });
    }
}

fn parse_streams(video: &Item) -> Vec<DouyinStream> {
    let fallback_width = positive_u32(video.get("width"));
    let fallback_height = positive_u32(video.get("height"));
    let mut streams = Vec::new();
    if let Some(bit_rates) = video.get("bit_rate").and_then(Value::as_array) {
        for value in bit_rates {
            let Some(bit_rate) = value.as_object() else {
                continue;
            };
            let play_addr = bit_rate
                .get("play_addr")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let urls = pick_download_urls(string_array(play_addr.get("url_list")));
            if urls.is_empty() {
                continue;
            }
            let width = positive_u32(play_addr.get("width")).or(fallback_width);
            let height = positive_u32(play_addr.get("height")).or(fallback_height);
            let quality = string_value(bit_rate.get("gear_name"))
                .as_deref()
                .and_then(quality_from_gear)
                .or_else(|| resolution_key(width, height))
                .unwrap_or_else(|| "source".to_owned());
            streams.push(DouyinStream {
                id: quality.clone(),
                label: quality,
                width,
                height,
                size: positive_u64(bit_rate.get("size"))
                    .or_else(|| positive_u64(play_addr.get("data_size"))),
                urls,
                bitrate: positive_u64(bit_rate.get("bit_rate")).unwrap_or_default(),
            });
        }
    }
    if streams.is_empty() {
        let play_addr = video
            .get("play_addr")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let urls = direct_video_urls(string_array(play_addr.get("url_list")));
        if !urls.is_empty() {
            let width = positive_u32(play_addr.get("width")).or(fallback_width);
            let height = positive_u32(play_addr.get("height")).or(fallback_height);
            let quality = resolution_key(width, height).unwrap_or_else(|| "source".to_owned());
            streams.push(DouyinStream {
                id: quality.clone(),
                label: quality,
                width,
                height,
                size: positive_u64(play_addr.get("data_size")),
                urls,
                bitrate: 0,
            });
        }
    }
    sort_streams(streams)
}

fn parse_images(item: &Item, play_base: &Url) -> Result<Vec<DouyinImage>> {
    let values = item
        .get("images")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if values.len() > MAX_IMAGES {
        return Err(AppError::new(
            "platform.resource_limit",
            "douyin item contains too many images",
            false,
        )
        .with_context("images", values.len())
        .with_context("maximum", MAX_IMAGES));
    }
    let mut images = Vec::new();
    for value in values {
        let Some(image) = value.as_object() else {
            continue;
        };
        let mut urls = string_array(image.get("url_list"));
        urls.extend(
            string_array(image.get("download_url_list"))
                .into_iter()
                .filter(|url| !looks_video_url(url)),
        );
        let urls = unique_strings(urls);
        let video_url = extract_image_video_url(image, play_base);
        if urls.is_empty() && video_url.is_none() {
            continue;
        }
        images.push(DouyinImage {
            index: images.len(),
            urls,
            video_url,
            width: positive_u32(image.get("width")),
            height: positive_u32(image.get("height")),
        });
    }
    Ok(images)
}

fn extract_image_video_url(image: &Item, play_base: &Url) -> Option<String> {
    let play_addr = image
        .get("video")
        .and_then(Value::as_object)
        .and_then(|video| video.get("play_addr"))
        .and_then(Value::as_object);
    if let Some(url) = play_addr
        .and_then(|play_addr| play_addr.get("url_list"))
        .map(|value| string_array(Some(value)))
        .and_then(|urls| no_watermark_urls(urls).into_iter().next())
    {
        return Some(url);
    }
    if let Some(url) = no_watermark_urls(
        string_array(image.get("download_url_list"))
            .into_iter()
            .filter(|url| looks_video_url(url))
            .collect(),
    )
    .into_iter()
    .next()
    {
        return Some(url);
    }
    let uri = play_addr
        .and_then(|play_addr| string_value(play_addr.get("uri")))
        .unwrap_or_default();
    (!uri.is_empty()).then(|| {
        build_play_url(
            play_base,
            &uri,
            resolution_key(
                positive_u32(image.get("width")),
                positive_u32(image.get("height")),
            )
            .as_deref(),
        )
        .to_string()
    })
}

fn pick_stream<'a>(streams: &'a [DouyinStream], quality: &str) -> Result<&'a DouyinStream> {
    let wanted = quality.trim().to_ascii_lowercase();
    if wanted.is_empty() || wanted == "best" {
        return streams.first().ok_or_else(no_playable_error);
    }
    streams
        .iter()
        .find(|stream| {
            stream.id.eq_ignore_ascii_case(&wanted) || stream.label.eq_ignore_ascii_case(&wanted)
        })
        .ok_or_else(|| {
            AppError::new(
                "selection.quality_not_found",
                format!(
                    "douyin quality not found: {quality}; available: {}",
                    streams
                        .iter()
                        .map(|stream| stream.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                false,
            )
        })
}

fn select_images<'a>(
    images: &'a [DouyinImage],
    selection: Option<&[u32]>,
) -> Result<Vec<&'a DouyinImage>> {
    let Some(selection) = selection.filter(|selection| !selection.is_empty()) else {
        return Ok(images.iter().collect());
    };
    let indexes = selection
        .iter()
        .map(|index| *index as usize)
        .collect::<BTreeSet<_>>();
    indexes
        .into_iter()
        .map(|index| {
            images.get(index).ok_or_else(|| {
                AppError::new(
                    "selection.image_out_of_range",
                    format!(
                        "douyin image index out of range: {} (total {})",
                        index + 1,
                        images.len()
                    ),
                    false,
                )
                .with_context("requested", index + 1)
                .with_context("total", images.len())
            })
        })
        .collect()
}

fn sort_streams(mut streams: Vec<DouyinStream>) -> Vec<DouyinStream> {
    streams.sort_by_key(|stream| {
        std::cmp::Reverse((
            stream.height.unwrap_or_default(),
            stream.width.unwrap_or_default(),
            stream.bitrate,
        ))
    });
    let mut output = Vec::<DouyinStream>::new();
    for stream in streams {
        if let Some(existing) = output
            .iter_mut()
            .find(|existing| existing.id.eq_ignore_ascii_case(&stream.id))
        {
            existing.urls =
                unique_strings(existing.urls.iter().chain(&stream.urls).cloned().collect());
        } else {
            output.push(stream);
        }
    }
    output
}

fn pick_download_urls(urls: Vec<String>) -> Vec<String> {
    let direct = direct_video_urls(urls.clone());
    if direct.is_empty() {
        no_watermark_urls(urls)
    } else {
        direct
    }
}

fn direct_video_urls(urls: Vec<String>) -> Vec<String> {
    unique_strings(urls)
        .into_iter()
        .filter(|url| is_direct_video_url(url))
        .collect()
}

fn no_watermark_urls(urls: Vec<String>) -> Vec<String> {
    unique_strings(urls)
        .into_iter()
        .map(|url| url.replace("playwm", "play"))
        .collect()
}

fn is_direct_video_url(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if matches!(host.as_str(), "aweme.snssdk.com" | "www.douyin.com") {
        return false;
    }
    host.contains("douyinvod.com")
        || url
            .query_pairs()
            .any(|(key, value)| key == "mime_type" && value == "video_mp4")
        || url.path().contains("/video/")
}

fn looks_video_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("mime_type=video")
        || lower.contains("video_mp4")
        || media_extension_regex().is_match(&lower)
}

fn quality_from_gear(value: &str) -> Option<String> {
    quality_regex()
        .captures(&value.to_ascii_lowercase())
        .and_then(|capture| capture.get(1))
        .map(|value| format!("{}p", value.as_str()))
}

fn resolution_key(width: Option<u32>, height: Option<u32>) -> Option<String> {
    match (width, height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => {
            Some(format!("{}p", width.min(height)))
        }
        _ => None,
    }
}

fn dimensions_for_ratio(resolution: u32, source_width: u32, source_height: u32) -> (u32, u32) {
    if resolution == 0 || source_width == 0 || source_height == 0 {
        return (0, 0);
    }
    if source_width < source_height {
        (
            resolution,
            (f64::from(resolution) * f64::from(source_height) / f64::from(source_width)).round()
                as u32,
        )
    } else {
        (
            (f64::from(resolution) * f64::from(source_width) / f64::from(source_height)).round()
                as u32,
            resolution,
        )
    }
}

fn build_play_url(base: &Url, uri: &str, ratio: Option<&str>) -> Url {
    let mut url = base.clone();
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("video_id", uri);
        if let Some(ratio) = ratio.filter(|ratio| !ratio.is_empty()) {
            query.append_pair("ratio", ratio);
        }
        query.append_pair("line", "0");
    }
    url
}

fn parse_content_range_total(value: &str) -> Option<u64> {
    content_range_regex()
        .captures(value)
        .and_then(|capture| capture.get(1))
        .and_then(|value| value.as_str().parse().ok())
}

fn is_video_like_content_type(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.is_empty() || lower.contains("video") || lower.contains("octet-stream")
}

fn download_sources(urls: &[String]) -> Vec<DownloadSource> {
    let headers = download_header_map();
    urls.iter()
        .map(|url| DownloadSource {
            url: url.clone(),
            headers: headers.clone(),
            auth_ref: None,
        })
        .collect()
}

fn single_download_source(url: &str) -> Vec<DownloadSource> {
    vec![DownloadSource {
        url: url.to_owned(),
        headers: download_header_map(),
        auth_ref: None,
    }]
}

fn download_header_map() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("Referer".to_owned(), DEFAULT_REFERER.to_owned()),
        ("User-Agent".to_owned(), DESKTOP_USER_AGENT.to_owned()),
    ])
}

fn download_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        DESKTOP_USER_AGENT.parse().expect("valid user agent"),
    );
    headers.insert(
        reqwest::header::REFERER,
        DEFAULT_REFERER.parse().expect("valid referer"),
    );
    headers
}

fn page_headers(cookie: Option<&str>) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        MOBILE_USER_AGENT.parse().expect("valid user agent"),
    );
    headers.insert(
        reqwest::header::ACCEPT,
        "application/json, text/plain, */*"
            .parse()
            .expect("valid accept"),
    );
    headers.insert(
        reqwest::header::ACCEPT_LANGUAGE,
        "zh-CN,zh;q=0.9,en;q=0.8".parse().expect("valid language"),
    );
    headers.insert(
        reqwest::header::ORIGIN,
        DEFAULT_REFERER
            .trim_end_matches('/')
            .parse()
            .expect("valid origin"),
    );
    headers.insert(
        reqwest::header::REFERER,
        DEFAULT_REFERER.parse().expect("valid referer"),
    );
    if let Some(cookie) = cookie
        && let Ok(cookie) = cookie.parse()
    {
        headers.insert(reqwest::header::COOKIE, cookie);
    }
    headers
}

fn web_detail_headers(aweme_id: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        DESKTOP_USER_AGENT.parse().expect("valid user agent"),
    );
    headers.insert(
        reqwest::header::ACCEPT,
        "application/json, text/plain, */*"
            .parse()
            .expect("valid accept"),
    );
    headers.insert(
        reqwest::header::ACCEPT_LANGUAGE,
        "zh-CN,zh;q=0.9,en;q=0.8".parse().expect("valid language"),
    );
    headers.insert(
        reqwest::header::REFERER,
        format!("https://www.douyin.com/video/{aweme_id}")
            .parse()
            .expect("valid referer"),
    );
    headers
}

async fn read_limited_text(response: Response, limit: usize) -> Result<String> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(response_too_large(limit));
    }
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

fn extract_object_after_marker<'a>(html: &'a str, marker: &str) -> Result<&'a str> {
    let marker_index = html.find(marker).ok_or_else(router_item_error)?;
    let start = html[marker_index..]
        .find('{')
        .map(|index| marker_index + index)
        .ok_or_else(router_item_error)?;
    let bytes = html.as_bytes();
    let mut depth = 0_u32;
    let mut in_string = false;
    let mut escape = false;
    for index in start..bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escape {
                escape = false;
            } else if byte == b'\\' {
                escape = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(&html[start..=index]);
                }
            }
            _ => {}
        }
    }
    Err(router_item_error())
}

fn build_client(options: &ResolveOptions, redirect: Policy) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(options.timeout)
        .redirect(redirect);
    if let Some(proxy) = options
        .proxy
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy)
                .map_err(|_| AppError::new("proxy.invalid", "invalid proxy URL", false))?,
        );
    }
    builder
        .build()
        .map_err(|error| AppError::http_request(&error))
}

fn merge_cookie_headers(explicit: Option<&str>, ephemeral: Option<&str>) -> Option<String> {
    match (
        explicit.map(str::trim).filter(|value| !value.is_empty()),
        ephemeral.map(str::trim).filter(|value| !value.is_empty()),
    ) {
        (Some(explicit), Some(ephemeral)) => Some(format!("{explicit}; {ephemeral}")),
        (Some(explicit), None) => Some(explicit.to_owned()),
        (None, Some(ephemeral)) => Some(ephemeral.to_owned()),
        (None, None) => None,
    }
}

fn parse_endpoint(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|error| {
        AppError::new(
            "platform.configuration",
            format!("invalid douyin endpoint URL: {error}"),
            false,
        )
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(endpoint_error());
    }
    Ok(url)
}

fn extract_aweme_id(url: &Url) -> Option<String> {
    aweme_path_regex()
        .captures(url.path())
        .and_then(|capture| capture.get(1))
        .map(|value| value.as_str().to_owned())
        .or_else(|| {
            url.query_pairs()
                .find_map(|(key, value)| {
                    matches!(key.as_ref(), "modal_id" | "aweme_id").then(|| value.into_owned())
                })
                .filter(|value| looks_like_aweme_id(value))
        })
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn string_value(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(value)) => Some(value.trim().to_owned()),
        Some(Value::Number(value)) => Some(value.to_string()),
        Some(Value::Bool(value)) => Some(value.to_string()),
        _ => None,
    }
    .filter(|value| !value.is_empty())
}

fn first_url(value: Option<&Value>) -> Option<String> {
    string_array(value).into_iter().next()
}

fn positive_u32(value: Option<&Value>) -> Option<u32> {
    positive_u64(value).and_then(|value| u32::try_from(value).ok())
}

fn positive_u64(value: Option<&Value>) -> Option<u64> {
    let value = match value {
        Some(Value::Number(value)) => value.as_u64(),
        Some(Value::String(value)) => value.trim().parse().ok(),
        _ => None,
    }?;
    (value > 0).then_some(value)
}

fn flexible_i64(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(value)) => value.as_i64().unwrap_or_default(),
        Some(Value::String(value)) => value.trim().parse().unwrap_or_default(),
        _ => 0,
    }
}

fn positive_dimension(value: u32) -> Option<u32> {
    (value > 0).then_some(value)
}

fn unique_strings(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty() && seen.insert(value.clone()))
        .collect()
}

fn first_non_empty<const N: usize>(values: [Option<&str>; N]) -> String {
    values
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or_default()
        .to_owned()
}

fn safe_file_stem(value: &str, fallback: &str) -> String {
    let mut output = String::new();
    let mut previous_space = false;
    for ch in value.chars() {
        let replacement = if ch.is_control()
            || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        {
            '_'
        } else if ch.is_whitespace() {
            ' '
        } else {
            ch
        };
        if replacement == ' ' {
            if previous_space {
                continue;
            }
            previous_space = true;
        } else {
            previous_space = false;
        }
        output.push(replacement);
        if output.chars().count() >= 120 {
            break;
        }
    }
    let output = output.trim().trim_end_matches([' ', '.']).to_owned();
    if output.is_empty() {
        fallback.to_owned()
    } else {
        output
    }
}

fn extension_from_url(value: &str, fallback: &str) -> String {
    Url::parse(value)
        .ok()
        .and_then(|url| {
            std::path::Path::new(url.path())
                .extension()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty() && value.len() <= 8)
                .map(str::to_ascii_lowercase)
        })
        .unwrap_or_else(|| fallback.to_owned())
}

fn remember_first(slot: &mut Option<AppError>, error: AppError) {
    if slot.is_none() {
        *slot = Some(error);
    }
}

fn no_playable_error() -> AppError {
    AppError::new(
        "platform.no_streams",
        "douyin share item has metadata but no playable video URL; the item may be removed, restricted, app-only, or require browser-generated web detail cookies",
        false,
    )
}

fn router_item_error() -> AppError {
    AppError::new(
        "platform.router_data_not_found",
        "douyin item not found in router data",
        false,
    )
}

fn response_too_large(limit: usize) -> AppError {
    AppError::new(
        "platform.response_too_large",
        "platform response exceeded the configured size limit",
        false,
    )
    .with_context("maximum", limit)
}

fn endpoint_error() -> AppError {
    AppError::new(
        "platform.configuration",
        "douyin endpoint must be an HTTP URL with a host",
        false,
    )
}

fn trim_url(value: &str) -> &str {
    value.trim().trim_matches(|ch| {
        matches!(
            ch,
            '"' | '\''
                | '<'
                | '>'
                | '['
                | ']'
                | '{'
                | '}'
                | '('
                | ')'
                | ','
                | '.'
                | '!'
                | '?'
                | ';'
                | ':'
                | '，'
                | '。'
                | '！'
                | '？'
                | '；'
                | '：'
                | '）'
                | '】'
        )
    })
}

fn domain_matches(host: &str, allowed: &str) -> bool {
    host == allowed
        || host
            .strip_suffix(allowed)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn url_regex() -> &'static Regex {
    static URL: OnceLock<Regex> = OnceLock::new();
    URL.get_or_init(|| Regex::new(r#"https?://[^\s<>'"`，。；、）)】]+"#).expect("valid URL regex"))
}

fn aweme_path_regex() -> &'static Regex {
    static PATH: OnceLock<Regex> = OnceLock::new();
    PATH.get_or_init(|| {
        Regex::new(r"/(?:video|note|share/slides|share/video|share/note)/(\d{10,30})")
            .expect("valid douyin path regex")
    })
}

fn quality_regex() -> &'static Regex {
    static QUALITY: OnceLock<Regex> = OnceLock::new();
    QUALITY.get_or_init(|| Regex::new(r"(\d{3,4})p?").expect("valid quality regex"))
}

fn media_extension_regex() -> &'static Regex {
    static EXTENSION: OnceLock<Regex> = OnceLock::new();
    EXTENSION.get_or_init(|| {
        Regex::new(r"\.(?:mp4|mov|m4v)(?:[?#]|$)").expect("valid media extension regex")
    })
}

fn content_range_regex() -> &'static Regex {
    static RANGE: OnceLock<Regex> = OnceLock::new();
    RANGE.get_or_init(|| Regex::new(r"/(\d+)\s*$").expect("valid content range regex"))
}

fn ttwid_regex() -> &'static Regex {
    static TTWID: OnceLock<Regex> = OnceLock::new();
    TTWID.get_or_init(|| Regex::new(r"(?:^|[,\s])ttwid=([^;,\s]+)").expect("valid ttwid regex"))
}

type Item = Map<String, Value>;
