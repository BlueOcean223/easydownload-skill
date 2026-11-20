use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

use futures_util::{StreamExt, stream};
use regex::Regex;
use reqwest::{
    Response, StatusCode,
    header::{CONTENT_TYPE, LOCATION},
    redirect::Policy,
};
use serde::Deserialize;
use serde_json::{Map, Value};
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

const DEFAULT_BASE_URL: &str = "https://www.xiaohongshu.com/";
const DEFAULT_REFERER: &str = "https://www.xiaohongshu.com/";
const DEFAULT_ORIGINAL_IMAGE_BASE: &str = "https://sns-img-bd.xhscdn.com/";
const DEFAULT_ORIGIN_VIDEO_BASE: &str = "https://sns-video-bd.xhscdn.com/";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";
const MAX_NOTE_HTML_BYTES: usize = 10 * 1024 * 1024;
const MAX_SHORT_HTML_BYTES: usize = 256 * 1024;
const MAX_IMAGES: usize = 10_000;
const ORIGINAL_IMAGE_PROBE_CONCURRENCY: usize = 6;

pub fn matches(input: &str) -> bool {
    static HOST: OnceLock<Regex> = OnceLock::new();
    HOST.get_or_init(|| {
        Regex::new(
            r"(?i)(?:^|//)(?:[^/]+\.)?(?:xiaohongshu\.com|rednote\.com|xhslink\.(?:com|cn))(?:[/:?#]|$)",
        )
        .expect("valid xiaohongshu host regex")
    })
    .is_match(input)
        || looks_like_note_id(input.trim())
}

pub fn looks_like_note_id(value: &str) -> bool {
    static NOTE_ID: OnceLock<Regex> = OnceLock::new();
    NOTE_ID
        .get_or_init(|| Regex::new(r"(?i)^[0-9a-f]{24}$").expect("valid xhs note id regex"))
        .is_match(value)
}

#[derive(Clone)]
pub struct XiaohongshuResolver {
    page_client: reqwest::Client,
    redirect_client: reqwest::Client,
    base_url: Url,
    original_image_base: Option<Url>,
    credentials: CredentialStore,
    extra_xhs_hosts: Vec<String>,
    extra_short_hosts: Vec<String>,
}

impl XiaohongshuResolver {
    pub fn new(options: ResolveOptions) -> Result<Self> {
        let mut resolver = Self::with_endpoints(options, DEFAULT_BASE_URL, &[])?;
        resolver.original_image_base = Some(
            Url::parse(DEFAULT_ORIGINAL_IMAGE_BASE)
                .expect("valid default xiaohongshu original image base"),
        );
        Ok(resolver)
    }

    pub fn with_endpoints(
        options: ResolveOptions,
        base_url: &str,
        short_hosts: &[&str],
    ) -> Result<Self> {
        let base_url = Url::parse(base_url).map_err(|error| {
            AppError::new(
                "platform.configuration",
                format!("invalid xiaohongshu base URL: {error}"),
                false,
            )
        })?;
        if !matches!(base_url.scheme(), "http" | "https") || base_url.host_str().is_none() {
            return Err(AppError::new(
                "platform.configuration",
                "xiaohongshu base URL must be an HTTP URL with a host",
                false,
            ));
        }
        let page_client = build_client(&options, Policy::limited(10))?;
        let redirect_client = build_client(&options, Policy::none())?;
        let credentials = CredentialStore::new(options.config_dir, options.cookies_path);
        Ok(Self {
            page_client,
            redirect_client,
            original_image_base: None,
            credentials,
            extra_xhs_hosts: vec![base_url.host_str().unwrap().to_ascii_lowercase()],
            extra_short_hosts: short_hosts
                .iter()
                .map(|host| host.trim().to_ascii_lowercase())
                .filter(|host| !host.is_empty())
                .collect(),
            base_url,
        })
    }

    pub fn with_original_image_base(mut self, base_url: &str) -> Result<Self> {
        let mut parsed = Url::parse(base_url).map_err(|error| {
            AppError::new(
                "platform.configuration",
                format!("invalid xiaohongshu original image base URL: {error}"),
                false,
            )
        })?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(AppError::new(
                "platform.configuration",
                "xiaohongshu original image base URL must be an HTTP URL with a host",
                false,
            ));
        }
        if !parsed.path().ends_with('/') {
            parsed.set_path(&format!("{}/", parsed.path()));
        }
        self.original_image_base = Some(parsed);
        Ok(self)
    }

    pub async fn resolve(&self, input: &str) -> Result<XiaohongshuMedia> {
        let parsed = self.parse_input(input).await?;
        self.fetch_note(parsed).await
    }

    async fn parse_input(&self, input: &str) -> Result<ParsedInput> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(AppError::new(
                "input.url_not_found",
                "no xiaohongshu URL found in input",
                false,
            ));
        }
        if looks_like_note_id(trimmed) {
            return Ok(ParsedInput {
                note_id: trimmed.to_ascii_lowercase(),
                xsec_token: None,
            });
        }
        let mut current = self.extract_share_url(trimmed).ok_or_else(|| {
            AppError::new(
                "input.url_not_found",
                "no xiaohongshu URL found in input",
                false,
            )
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
            if !self.is_xhs_host(&host) {
                return Err(AppError::new(
                    "input.invalid_host",
                    "invalid xiaohongshu URL host",
                    false,
                )
                .with_context("host", host));
            }
            let note_id = note_id_from_url(&current).ok_or_else(|| {
                AppError::new(
                    "platform.identifier_not_found",
                    "xiaohongshu note id not found",
                    false,
                )
            })?;
            return Ok(ParsedInput {
                note_id,
                xsec_token: current
                    .query_pairs()
                    .find_map(|(key, value)| (key == "xsec_token").then(|| value.into_owned()))
                    .filter(|value| !value.trim().is_empty()),
            });
        }
        Err(AppError::new(
            "platform.short_link_failed",
            "xiaohongshu short link exceeded the redirect limit",
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
        url_regex().find_iter(input).find_map(|value| {
            let url = Url::parse(trim_url(value.as_str())).ok()?;
            url.host_str()
                .is_some_and(|host| self.is_acceptable_host(&host.to_ascii_lowercase()))
                .then_some(url)
        })
    }

    async fn resolve_short_link(&self, url: &Url) -> Result<Url> {
        let mut candidates = vec![url.clone()];
        if url.scheme() == "http" {
            let mut secure = url.clone();
            if secure.set_scheme("https").is_ok() {
                candidates.push(secure);
            }
        }
        let mut last_error = None;
        for candidate in candidates {
            match self.resolve_short_link_once(&candidate).await {
                Ok(url) => return Ok(url),
                Err(error) => last_error = Some(error),
            }
        }
        let last = last_error.unwrap_or_else(|| {
            AppError::new(
                "platform.short_link_failed",
                "xiaohongshu short link resolution failed",
                false,
            )
        });
        Err(AppError::new(
            "platform.short_link_failed",
            format!("xiaohongshu short link resolution failed: {last}"),
            last.info.retryable,
        )
        .with_context("lastCode", last.info.code))
    }

    async fn resolve_short_link_once(&self, url: &Url) -> Result<Url> {
        let mut request = self
            .redirect_client
            .get(url.clone())
            .header("User-Agent", USER_AGENT)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8");
        if let Some(cookie) = self.credentials.explicit_cookie_header_for(url).await? {
            request = request.header("Cookie", cookie);
        }
        let response = request
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
                    "xiaohongshu redirect contains an invalid Location header",
                    false,
                )
            })?;
            return url.join(location).map_err(|_| {
                AppError::new(
                    "platform.invalid_response",
                    "xiaohongshu redirect contains an invalid URL",
                    false,
                )
            });
        }

        let html = html_unescape(&read_limited_text(response, MAX_SHORT_HTML_BYTES).await?);
        for candidate in url_regex().find_iter(&html) {
            let Ok(url) = Url::parse(trim_url(candidate.as_str())) else {
                continue;
            };
            if url
                .host_str()
                .is_some_and(|host| self.is_xhs_host(&host.to_ascii_lowercase()))
            {
                return Ok(url);
            }
        }
        Err(AppError::new(
            "platform.short_link_failed",
            format!("unexpected short-link HTTP status {}", status.as_u16()),
            status.is_server_error(),
        )
        .with_context("status", status.as_u16()))
    }

    async fn fetch_note(&self, input: ParsedInput) -> Result<XiaohongshuMedia> {
        let mut note_url = self.base_url.clone();
        note_url.set_path(&format!("/explore/{}", input.note_id));
        note_url.set_query(None);
        if let Some(token) = input.xsec_token.as_deref() {
            note_url.query_pairs_mut().append_pair("xsec_token", token);
        }
        let mut request = self
            .page_client
            .get(note_url.clone())
            .header("User-Agent", USER_AGENT)
            .header("Referer", DEFAULT_REFERER)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
            )
            .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
            .header("Cache-Control", "max-age=0");
        if let Some(cookie) = self
            .credentials
            .explicit_cookie_header_for(&note_url)
            .await?
        {
            request = request.header("Cookie", cookie);
        }
        let response = request
            .send()
            .await
            .map_err(|error| AppError::http_request(&error))?;
        match response.status() {
            StatusCode::NOT_FOUND => {
                return Err(AppError::new(
                    "platform.not_found",
                    "xiaohongshu note was not found",
                    false,
                ));
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(AppError::new(
                    "auth.required",
                    "xiaohongshu note is blocked or requires login",
                    false,
                ));
            }
            status if !status.is_success() => return Err(AppError::http_status(status)),
            _ => {}
        }
        let html = read_limited_text(response, MAX_NOTE_HTML_BYTES).await?;
        if html.contains("当前笔记暂时无法浏览") || html.contains("error_code") {
            return Err(AppError::new(
                "auth.required",
                "xiaohongshu note is blocked or requires login",
                false,
            ));
        }
        let state = extract_object_after_marker(&html, "window.__INITIAL_STATE__")?;
        let state: InitialState =
            serde_json::from_str(&normalize_js_json(state)).map_err(|error| {
                AppError::new(
                    "platform.invalid_response",
                    format!("invalid xiaohongshu initial state: {error}"),
                    false,
                )
            })?;
        let note = lookup_note(state, &input.note_id).ok_or_else(|| {
            AppError::new(
                "platform.not_found",
                "xiaohongshu note was not found in the initial state",
                false,
            )
        })?;
        XiaohongshuMedia::from_note(
            input.note_id,
            note,
            note_url.to_string(),
            self.page_client.clone(),
            self.original_image_base.clone(),
        )
    }

    fn is_acceptable_host(&self, host: &str) -> bool {
        self.is_xhs_host(host) || self.is_short_host(host)
    }

    fn is_xhs_host(&self, host: &str) -> bool {
        domain_matches(host, "xiaohongshu.com")
            || domain_matches(host, "rednote.com")
            || self.extra_xhs_hosts.iter().any(|allowed| host == allowed)
    }

    fn is_short_host(&self, host: &str) -> bool {
        domain_matches(host, "xhslink.com")
            || domain_matches(host, "xhslink.cn")
            || self.extra_short_hosts.iter().any(|allowed| host == allowed)
    }
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

#[derive(Debug, Clone)]
pub struct XiaohongshuMedia {
    id: String,
    title: String,
    author: Option<String>,
    cover: Option<String>,
    note_url: String,
    probe_client: reqwest::Client,
    original_image_base: Option<Url>,
    kind: XiaohongshuMediaKind,
}

#[derive(Debug, Clone)]
enum XiaohongshuMediaKind {
    Video { streams: Vec<XhsStream> },
    Album { images: Vec<XhsImage> },
}

#[derive(Debug, Clone)]
struct XhsImage {
    index: usize,
    urls: Vec<String>,
    token: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    live_photo_url: Option<String>,
}

#[derive(Debug, Clone)]
struct XhsStream {
    id: String,
    label: String,
    width: Option<u32>,
    height: Option<u32>,
    size: Option<u64>,
    urls: Vec<String>,
    is_origin: bool,
    stream_type: Option<u32>,
    weight: i64,
    default_stream: i64,
    video_codec: String,
    video_bitrate: u64,
}

impl XhsStream {
    fn rank(&self) -> (bool, u8, i64, i32, i64, u64, u64, u64) {
        (
            self.is_origin,
            codec_rank(&self.video_codec),
            self.weight,
            stream_type_rank(self.stream_type),
            self.default_stream,
            u64::from(self.width.unwrap_or_default()) * u64::from(self.height.unwrap_or_default()),
            self.video_bitrate,
            self.size.unwrap_or_default(),
        )
    }
}

impl XiaohongshuMedia {
    fn from_note(
        id: String,
        note: XhsNote,
        note_url: String,
        probe_client: reqwest::Client,
        original_image_base: Option<Url>,
    ) -> Result<Self> {
        let title = first_non_empty([note.title.as_deref(), note.desc.as_deref(), Some(&id)]);
        let author = note.user.as_ref().and_then(|user| {
            let value = first_non_empty([user.nickname.as_deref(), user.name.as_deref()]);
            (!value.is_empty()).then_some(value)
        });
        let images = parse_images(&note)?;
        let streams = parse_video_streams(&note);
        let image_cover = images.first().and_then(|image| image.urls.first().cloned());
        let note_type = note
            .note_type
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let (kind, cover) = if note_type == "video" || !streams.is_empty() {
            if streams.is_empty() {
                return Err(AppError::new(
                    "platform.no_streams",
                    "xiaohongshu video has no downloadable stream",
                    false,
                ));
            }
            (
                XiaohongshuMediaKind::Video { streams },
                note.cover.clone().or(image_cover),
            )
        } else if !images.is_empty() {
            // imageList URLs are the downloadable originals, so they stay private to the planner.
            (XiaohongshuMediaKind::Album { images }, note.cover.clone())
        } else {
            return Err(AppError::new(
                "platform.no_media",
                "xiaohongshu note has no downloadable media",
                false,
            ));
        };
        Ok(Self {
            id,
            title,
            author,
            cover,
            note_url,
            probe_client,
            original_image_base,
            kind,
        })
    }

    pub fn inspect(&self) -> InspectDocument {
        let (media_kind, qualities, images) = match &self.kind {
            XiaohongshuMediaKind::Video { streams } => (
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
            XiaohongshuMediaKind::Album { images } => (
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
            platform: "xhs".to_owned(),
            media: MediaView {
                id: self.id.clone(),
                kind: media_kind,
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

    pub async fn create_plan(&self, selection: &PlanSelection) -> Result<DownloadPlan> {
        let tasks = match &self.kind {
            XiaohongshuMediaKind::Video { streams } => {
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
                    sources: self.download_sources(&stream.urls),
                    output: OutputSpec { relative_path },
                }]
            }
            XiaohongshuMediaKind::Album { images } => {
                self.create_album_tasks(images, selection).await?
            }
        };
        let plan = DownloadPlan {
            schema_version: SCHEMA_VERSION,
            created_at: OffsetDateTime::now_utc(),
            expires_at: None,
            source: PlanSource {
                platform: "xhs".to_owned(),
                media_id: self.id.clone(),
            },
            tasks,
        };
        plan.validate()?;
        Ok(plan)
    }

    async fn create_album_tasks(
        &self,
        images: &[XhsImage],
        selection: &PlanSelection,
    ) -> Result<Vec<DownloadTask>> {
        let selected = select_images(images, selection.images.as_deref())?;
        let mut resolved = stream::iter(selected.into_iter().enumerate().map(
            |(order, image)| async move {
                let (urls, ext) = self.resolve_image_sources(image).await;
                (order, image, urls, ext)
            },
        ))
        .buffer_unordered(ORIGINAL_IMAGE_PROBE_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
        resolved.sort_by_key(|(order, _, _, _)| *order);

        match selection.album_output {
            AlbumOutput::Zip => {
                let mut entries = Vec::new();
                for (_, image, urls, ext) in resolved {
                    let prefix = format!("{:02}", image.index + 1);
                    entries.push(ArchiveEntry {
                        id: format!("image-{}", image.index + 1),
                        path: format!("{prefix}.{ext}"),
                        sources: self.download_sources(&urls),
                    });
                    if let Some(url) = image.live_photo_url.as_deref() {
                        entries.push(ArchiveEntry {
                            id: format!("image-{}-live", image.index + 1),
                            path: format!("{prefix}_live.{}", extension_from_url(url, "mp4")),
                            sources: self.single_download_source(url),
                        });
                    }
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
                for (_, image, urls, ext) in resolved {
                    let prefix = format!("{:02}", image.index + 1);
                    tasks.push(DownloadTask::File {
                        id: format!("{}:image-{}", self.id, image.index + 1),
                        sources: self.download_sources(&urls),
                        output: OutputSpec {
                            relative_path: format!("{directory}/{prefix}.{ext}"),
                        },
                    });
                    if let Some(url) = image.live_photo_url.as_deref() {
                        tasks.push(DownloadTask::File {
                            id: format!("{}:image-{}-live", self.id, image.index + 1),
                            sources: self.single_download_source(url),
                            output: OutputSpec {
                                relative_path: format!(
                                    "{directory}/{prefix}_live.{}",
                                    extension_from_url(url, "mp4")
                                ),
                            },
                        });
                    }
                }
                Ok(tasks)
            }
        }
    }

    async fn resolve_image_sources(&self, image: &XhsImage) -> (Vec<String>, String) {
        if let (Some(base), Some(token)) =
            (self.original_image_base.as_ref(), image.token.as_deref())
            && let Ok(candidate) = base.join(token)
            && let Some(ext) = self.probe_original_image(&candidate).await
        {
            let mut urls = vec![candidate.to_string()];
            urls.extend(image.urls.iter().cloned());
            return (unique_strings(urls), ext.to_owned());
        }
        (
            image.urls.clone(),
            extension_from_url(&image.urls[0], "jpg"),
        )
    }

    async fn probe_original_image(&self, url: &Url) -> Option<&'static str> {
        let response = self
            .probe_client
            .head(url.clone())
            .header("User-Agent", USER_AGENT)
            .header("Referer", &self.note_url)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())?;
        image_extension_from_content_type(content_type)
    }

    fn single_download_source(&self, url: &str) -> Vec<DownloadSource> {
        vec![DownloadSource {
            url: url.to_owned(),
            headers: self.download_headers(),
            auth_ref: None,
        }]
    }

    fn download_sources(&self, urls: &[String]) -> Vec<DownloadSource> {
        let headers = self.download_headers();
        urls.iter()
            .map(|url| DownloadSource {
                url: url.clone(),
                headers: headers.clone(),
                auth_ref: None,
            })
            .collect()
    }

    fn download_headers(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("Referer".to_owned(), self.note_url.clone()),
            ("User-Agent".to_owned(), USER_AGENT.to_owned()),
        ])
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

fn parse_images(note: &XhsNote) -> Result<Vec<XhsImage>> {
    if note.image_list.len() > MAX_IMAGES {
        return Err(AppError::new(
            "platform.resource_limit",
            "xiaohongshu note contains too many images",
            false,
        )
        .with_context("images", note.image_list.len())
        .with_context("maximum", MAX_IMAGES));
    }
    let mut images = Vec::new();
    for image in &note.image_list {
        let mut urls = vec![
            image.url_default.clone().unwrap_or_default(),
            image.url_pre.clone().unwrap_or_default(),
            image.url.clone().unwrap_or_default(),
        ];
        urls.extend(image.info_list.iter().filter_map(|info| info.url.clone()));
        let urls = unique_strings(urls);
        if urls.is_empty() {
            continue;
        }
        let live_photo = flexible_bool(image.live_photo.as_ref());
        let token = extract_image_token(image, &urls);
        images.push(XhsImage {
            index: images.len(),
            urls,
            token,
            width: flexible_u32(image.width.as_ref()),
            height: flexible_u32(image.height.as_ref()),
            live_photo_url: live_photo
                .then(|| {
                    image
                        .stream
                        .as_ref()
                        .and_then(|value| first_media_url(value, 0))
                })
                .flatten(),
        });
    }
    Ok(images)
}

fn parse_video_streams(note: &XhsNote) -> Vec<XhsStream> {
    let mut best = BTreeMap::<String, XhsStream>::new();
    if let Some(stream_value) = note
        .video
        .as_ref()
        .and_then(|video| video.media.as_ref())
        .and_then(|media| media.stream.as_ref())
    {
        let mut candidates = Vec::<(&Map<String, Value>, Option<&str>)>::new();
        if let Some(items) = stream_value.as_array() {
            candidates.extend(
                items
                    .iter()
                    .filter_map(Value::as_object)
                    .map(|item| (item, None)),
            );
        }
        if let Some(map) = stream_value.as_object() {
            for (codec, value) in map {
                let Some(items) = value.as_array() else {
                    continue;
                };
                candidates.extend(
                    items
                        .iter()
                        .filter_map(Value::as_object)
                        .map(|item| (item, Some(codec.as_str()))),
                );
            }
        }

        for (data, codec_hint) in candidates {
            let Some(stream) = parse_stream_candidate(data, codec_hint) else {
                continue;
            };
            let key = stream.id.to_ascii_lowercase();
            if best
                .get(&key)
                .is_none_or(|existing| stream.rank() > existing.rank())
            {
                best.insert(key, stream);
            }
        }
    }
    let mut streams = best.into_values().collect::<Vec<_>>();
    streams.sort_by_key(|stream| std::cmp::Reverse(stream.rank()));

    if let Some(origin_url) = note
        .video
        .as_ref()
        .and_then(|video| video.consumer.as_ref())
        .and_then(|consumer| consumer.origin_video_key.as_deref())
        .and_then(build_origin_video_url)
    {
        let mut urls = vec![origin_url];
        if let Some(top) = streams.first() {
            urls.extend(top.urls.iter().cloned());
        }
        let top = streams.first();
        let origin = XhsStream {
            id: "origin".to_owned(),
            label: "Origin".to_owned(),
            width: top.and_then(|stream| stream.width),
            height: top.and_then(|stream| stream.height),
            size: None,
            urls: unique_strings(urls),
            is_origin: true,
            stream_type: None,
            weight: 0,
            default_stream: 0,
            video_codec: String::new(),
            video_bitrate: 0,
        };
        streams.insert(0, origin);
    }

    streams
}

fn parse_stream_candidate(
    data: &Map<String, Value>,
    codec_hint: Option<&str>,
) -> Option<XhsStream> {
    let mut urls = vec![string_value(data.get("masterUrl"))];
    urls.extend(string_list(data.get("backupUrls")));
    let urls = unique_strings(urls);
    if urls.is_empty() {
        return None;
    }
    let stream_type = flexible_u32(data.get("streamType"));
    let quality_name = first_non_empty([
        optional_string(data.get("qualityType")).as_deref(),
        optional_string(data.get("quality")).as_deref(),
    ]);
    let quality_name = if quality_name.is_empty() {
        stream_type
            .map(|value| format!("Stream {value}"))
            .unwrap_or_default()
    } else {
        quality_name
    };
    let video_codec = first_non_empty([
        optional_string(data.get("videoCodec")).as_deref(),
        codec_hint,
    ]);
    let id = stream_quality_key(&quality_name, stream_type);
    Some(XhsStream {
        id: id.clone(),
        label: [quality_name.as_str(), video_codec.as_str()]
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        width: flexible_u32(data.get("width")),
        height: flexible_u32(data.get("height")),
        size: flexible_u64(data.get("size")),
        urls,
        is_origin: false,
        stream_type,
        weight: flexible_i64(data.get("weight")),
        default_stream: flexible_i64(data.get("defaultStream")),
        video_codec,
        video_bitrate: flexible_u64(data.get("videoBitrate")).unwrap_or_default(),
    })
}

fn pick_stream<'a>(streams: &'a [XhsStream], quality: &str) -> Result<&'a XhsStream> {
    let wanted = quality.trim().to_ascii_lowercase();
    if wanted.is_empty() || wanted == "best" {
        return streams.first().ok_or_else(|| {
            AppError::new(
                "platform.no_streams",
                "xiaohongshu video has no downloadable stream",
                false,
            )
        });
    }
    streams
        .iter()
        .find(|stream| {
            stream.id.eq_ignore_ascii_case(&wanted)
                || stream.label.eq_ignore_ascii_case(&wanted)
                || stream
                    .stream_type
                    .is_some_and(|value| value.to_string() == wanted)
        })
        .ok_or_else(|| {
            AppError::new(
                "selection.quality_not_found",
                format!(
                    "xiaohongshu quality not found: {quality}; available: {}",
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
    images: &'a [XhsImage],
    selection: Option<&[u32]>,
) -> Result<Vec<&'a XhsImage>> {
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
                        "xiaohongshu image index out of range: {} (total {})",
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

async fn read_limited_text(response: Response, limit: usize) -> Result<String> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(AppError::new(
            "platform.response_too_large",
            "platform response exceeded the configured size limit",
            false,
        )
        .with_context("maximum", limit));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| AppError::http_request(&error))?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(AppError::new(
                "platform.response_too_large",
                "platform response exceeded the configured size limit",
                false,
            )
            .with_context("maximum", limit));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn extract_object_after_marker<'a>(html: &'a str, marker: &str) -> Result<&'a str> {
    let marker_index = html.find(marker).ok_or_else(initial_state_error)?;
    let start = html[marker_index..]
        .find('{')
        .map(|index| marker_index + index)
        .ok_or_else(initial_state_error)?;
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
    Err(initial_state_error())
}

fn initial_state_error() -> AppError {
    AppError::new(
        "platform.invalid_response",
        "xiaohongshu initial state was not found",
        false,
    )
}

fn normalize_js_json(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_string = false;
    let mut prev_sig: Option<u8> = None;

    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if byte == b'\\' {
                output.push(byte);
                index += 1;
                if index < bytes.len() {
                    output.push(bytes[index]);
                    index += 1;
                }
                continue;
            }
            if byte == b'"' {
                output.push(byte);
                in_string = false;
                prev_sig = Some(b'"');
                index += 1;
                continue;
            }
            if byte < 0x20 {
                match byte {
                    b'\n' => output.extend_from_slice(b"\\n"),
                    b'\r' => output.extend_from_slice(b"\\r"),
                    b'\t' => output.extend_from_slice(b"\\t"),
                    other => output.extend_from_slice(format!("\\u{other:04x}").as_bytes()),
                }
                index += 1;
                continue;
            }
            output.push(byte);
            index += 1;
            continue;
        }

        if byte == b'"' {
            output.push(byte);
            in_string = true;
            index += 1;
            continue;
        }

        if byte == b',' {
            let mut lookahead = index + 1;
            while lookahead < bytes.len() && bytes[lookahead].is_ascii_whitespace() {
                lookahead += 1;
            }
            if lookahead < bytes.len() && matches!(bytes[lookahead], b'}' | b']') {
                index += 1;
                continue;
            }
            output.push(byte);
            prev_sig = Some(b',');
            index += 1;
            continue;
        }

        if matches!(prev_sig, Some(b':' | b',' | b'[')) {
            if let Some(end) = skip_js_collection_ctor(bytes, index) {
                output.extend_from_slice(b"[]");
                prev_sig = Some(b']');
                index = end;
                continue;
            }
            if bytes[index..].starts_with(b"undefined")
                && !bytes.get(index + 9).copied().is_some_and(is_js_ident_byte)
            {
                output.extend_from_slice(b"null");
                prev_sig = Some(b'l');
                index += 9;
                continue;
            }
            if let Some(end) = skip_void_zero(bytes, index) {
                output.extend_from_slice(b"null");
                prev_sig = Some(b'l');
                index = end;
                continue;
            }
        }

        output.push(byte);
        if !byte.is_ascii_whitespace() {
            prev_sig = Some(byte);
        }
        index += 1;
    }

    String::from_utf8(output).expect("sanitized JSON preserves valid UTF-8")
}

fn is_js_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn skip_void_zero(bytes: &[u8], start: usize) -> Option<usize> {
    if !bytes[start..].starts_with(b"void") {
        return None;
    }
    let mut index = start + 4;
    let ws_start = index;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if index == ws_start || bytes.get(index) != Some(&b'0') {
        return None;
    }
    index += 1;
    if bytes.get(index).copied().is_some_and(is_js_ident_byte) {
        return None;
    }
    Some(index)
}

fn skip_js_collection_ctor(bytes: &[u8], start: usize) -> Option<usize> {
    if !bytes[start..].starts_with(b"new") {
        return None;
    }
    let mut index = start + 3;
    let ws_start = index;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if index == ws_start {
        return None;
    }
    if bytes[index..].starts_with(b"Map") || bytes[index..].starts_with(b"Set") {
        index += 3;
    } else {
        return None;
    }
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if bytes.get(index) != Some(&b'(') {
        return None;
    }
    let mut depth = 0_u32;
    let mut quote: Option<u8> = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(q) = quote {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == q {
                quote = None;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' => quote = Some(byte),
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn lookup_note(mut state: InitialState, note_id: &str) -> Option<XhsNote> {
    if let Some(entry) = state.note.note_detail_map.remove(note_id)
        && let Some(note) = entry.note
    {
        return Some(note);
    }
    state.note.note_detail_map.into_values().find_map(|entry| {
        let note = entry.note?;
        let id = note
            .note_id
            .as_deref()
            .or(note.id.as_deref())
            .unwrap_or_default();
        id.eq_ignore_ascii_case(note_id).then_some(note)
    })
}

fn note_id_from_url(url: &Url) -> Option<String> {
    if let Some(value) = note_id_regex().find(url.path()) {
        return Some(value.as_str().to_ascii_lowercase());
    }
    url.query_pairs().find_map(|(_, value)| {
        note_id_regex()
            .find(&value)
            .map(|value| value.as_str().to_ascii_lowercase())
    })
}

fn note_id_regex() -> &'static Regex {
    static NOTE_ID: OnceLock<Regex> = OnceLock::new();
    NOTE_ID.get_or_init(|| Regex::new(r"(?i)[0-9a-f]{24}").expect("valid xhs note id regex"))
}

fn url_regex() -> &'static Regex {
    static URL: OnceLock<Regex> = OnceLock::new();
    URL.get_or_init(|| Regex::new(r#"https?://[^\s<>'"`，。；、）)】]+"#).expect("valid URL regex"))
}

fn first_media_url(value: &Value, depth: usize) -> Option<String> {
    if depth > 32 {
        return None;
    }
    if let Some(value) = value.as_str() {
        return (!value.trim().is_empty()).then(|| value.trim().to_owned());
    }
    if let Some(values) = value.as_array() {
        return values
            .iter()
            .find_map(|value| first_media_url(value, depth + 1));
    }
    let record = value.as_object()?;
    let mut direct = vec![
        string_value(record.get("masterUrl")),
        string_value(record.get("url")),
    ];
    direct.extend(string_list(record.get("backupUrls")));
    let direct = unique_strings(direct);
    if let Some(url) = direct.into_iter().next() {
        return Some(url);
    }
    for key in ["h265", "h264", "av1", "h266", "hevc", "avc"] {
        if let Some(url) = record
            .get(key)
            .and_then(|value| first_media_url(value, depth + 1))
        {
            return Some(url);
        }
    }
    let mut keys = record.keys().collect::<Vec<_>>();
    keys.sort();
    keys.into_iter()
        .filter(|key| {
            !matches!(
                key.as_str(),
                "masterUrl"
                    | "url"
                    | "backupUrls"
                    | "h265"
                    | "h264"
                    | "av1"
                    | "h266"
                    | "hevc"
                    | "avc"
            )
        })
        .find_map(|key| first_media_url(&record[key], depth + 1))
}

fn stream_quality_key(quality_name: &str, stream_type: Option<u32>) -> String {
    let base = quality_name
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_whitespace() || matches!(ch, '/' | '\\' | ':') {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>();
    match stream_type {
        Some(stream_type) if base.is_empty() || base.starts_with("stream_") => {
            format!("stream_{stream_type}")
        }
        Some(stream_type) => format!("{base}_{stream_type}"),
        None if base.is_empty() => "default".to_owned(),
        None => base,
    }
}

fn codec_rank(value: &str) -> u8 {
    let normalized = value.to_ascii_lowercase().replace(['.', '_', '-'], "");
    match normalized.as_str() {
        "h265" | "hevc" => 4,
        "h264" | "avc" => 3,
        "av1" | "h266" | "vvc" => 2,
        _ => 1,
    }
}

fn stream_type_rank(value: Option<u32>) -> i32 {
    match value {
        Some(115) => 300,
        Some(114) => 200,
        Some(259) => 100,
        _ => 0,
    }
}

fn extension_from_url(value: &str, fallback: &str) -> String {
    let Ok(url) = Url::parse(value) else {
        return fallback.to_owned();
    };
    let path = url.path();
    let (base_path, directive) = match path.rsplit_once('!') {
        Some((base, directive)) => (base, Some(directive)),
        None => (path, None),
    };
    if let Some(ext) = directive.and_then(format_hint_from_token_str) {
        return ext.to_owned();
    }
    if let Some(ext) = std::path::Path::new(base_path)
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 8
                && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
    {
        return ext.to_ascii_lowercase();
    }
    if let Some(ext) = url.query().and_then(format_hint_from_query) {
        return ext.to_owned();
    }
    fallback.to_owned()
}

fn format_hint_from_token_str(value: &str) -> Option<&'static str> {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .find_map(known_media_extension)
}

fn format_hint_from_query(query: &str) -> Option<&'static str> {
    let mut segments = query.split(['&', '/', '=']);
    while let Some(segment) = segments.next() {
        if segment.eq_ignore_ascii_case("format")
            && let Some(ext) = segments.next().and_then(known_media_extension)
        {
            return Some(ext);
        }
    }
    None
}

fn known_media_extension(token: &str) -> Option<&'static str> {
    match token.to_ascii_lowercase().as_str() {
        "webp" => Some("webp"),
        "jpg" => Some("jpg"),
        "jpeg" => Some("jpeg"),
        "png" => Some("png"),
        "gif" => Some("gif"),
        "heic" => Some("heic"),
        "heif" => Some("heif"),
        "avif" => Some("avif"),
        "bmp" => Some("bmp"),
        "mp4" => Some("mp4"),
        "mov" => Some("mov"),
        "webm" => Some("webm"),
        _ => None,
    }
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

fn unique_strings(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty() && seen.insert(value.clone()))
        .collect()
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(value)) => vec![value.clone()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn optional_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(value)) => Some(value.trim().to_owned()),
        Some(Value::Number(value)) => Some(value.to_string()),
        Some(Value::Bool(value)) => Some(value.to_string()),
        _ => None,
    }
    .filter(|value| !value.is_empty())
}

fn string_value(value: Option<&Value>) -> String {
    optional_string(value).unwrap_or_default()
}

fn flexible_u32(value: Option<&Value>) -> Option<u32> {
    flexible_u64(value).and_then(|value| u32::try_from(value).ok())
}

fn flexible_u64(value: Option<&Value>) -> Option<u64> {
    match value {
        Some(Value::Number(value)) => value.as_u64(),
        Some(Value::String(value)) => value.trim().parse().ok(),
        _ => None,
    }
}

fn flexible_i64(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(value)) => value.as_i64().unwrap_or_default(),
        Some(Value::String(value)) => value.trim().parse().unwrap_or_default(),
        _ => 0,
    }
}

fn flexible_bool(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_i64().is_some_and(|value| value != 0),
        Some(Value::String(value)) => value == "1" || value.eq_ignore_ascii_case("true"),
        _ => false,
    }
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

fn html_unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn extract_image_token(image: &XhsImageData, urls: &[String]) -> Option<String> {
    if let Some(file_id) = image.file_id.as_deref().and_then(sanitize_xhs_media_token) {
        return Some(file_id);
    }
    for raw in urls {
        let Ok(url) = Url::parse(raw) else {
            continue;
        };
        let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
            continue;
        };
        if !domain_matches(&host, "xhscdn.com") && !domain_matches(&host, "xiaohongshu.com") {
            continue;
        }
        let segments = url
            .path()
            .trim_start_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        if segments.len() >= 3 {
            let joined = segments[2..].join("/");
            let without_directive = joined.split('!').next().unwrap_or_default();
            if let Some(token) = sanitize_xhs_media_token(without_directive) {
                return Some(token);
            }
        }
    }
    None
}

fn sanitize_xhs_media_token(raw: &str) -> Option<String> {
    let token = raw.trim().trim_start_matches('/');
    if token.is_empty()
        || token.contains("://")
        || token.contains("..")
        || token.contains('?')
        || token.contains('#')
        || token.contains('!')
        || token.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some(token.to_owned())
}

fn build_origin_video_url(raw_key: &str) -> Option<String> {
    let trimmed = raw_key.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(url) = Url::parse(trimmed)
        && matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
    {
        return Some(url.to_string());
    }
    let token = sanitize_xhs_media_token(trimmed)?;
    let base = Url::parse(DEFAULT_ORIGIN_VIDEO_BASE).ok()?;
    base.join(&token).ok().map(Into::into)
}

fn image_extension_from_content_type(content_type: &str) -> Option<&'static str> {
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match media_type.as_str() {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/heic" => Some("heic"),
        "image/heif" => Some("heif"),
        "image/avif" => Some("avif"),
        "image/gif" => Some("gif"),
        "image/bmp" => Some("bmp"),
        _ => None,
    }
}

fn domain_matches(host: &str, allowed: &str) -> bool {
    host == allowed
        || host
            .strip_suffix(allowed)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

#[derive(Debug, Deserialize, Default)]
struct InitialState {
    #[serde(default)]
    note: InitialNoteState,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct InitialNoteState {
    #[serde(default)]
    note_detail_map: BTreeMap<String, InitialNoteEntry>,
}

#[derive(Debug, Deserialize, Default)]
struct InitialNoteEntry {
    note: Option<XhsNote>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct XhsNote {
    note_id: Option<String>,
    id: Option<String>,
    #[serde(rename = "type")]
    note_type: Option<String>,
    title: Option<String>,
    desc: Option<String>,
    user: Option<XhsUser>,
    #[serde(default)]
    image_list: Vec<XhsImageData>,
    video: Option<XhsVideo>,
    cover: Option<String>,
}

#[derive(Debug, Deserialize)]
struct XhsUser {
    nickname: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct XhsImageData {
    file_id: Option<String>,
    url_default: Option<String>,
    url_pre: Option<String>,
    url: Option<String>,
    #[serde(default)]
    info_list: Vec<XhsUrlInfo>,
    width: Option<Value>,
    height: Option<Value>,
    live_photo: Option<Value>,
    stream: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct XhsUrlInfo {
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct XhsVideo {
    media: Option<XhsVideoMedia>,
    consumer: Option<XhsVideoConsumer>,
}

#[derive(Debug, Deserialize)]
struct XhsVideoMedia {
    stream: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct XhsVideoConsumer {
    origin_video_key: Option<String>,
}

#[derive(Debug)]
struct ParsedInput {
    note_id: String,
    xsec_token: Option<String>,
}
