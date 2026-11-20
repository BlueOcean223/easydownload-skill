use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

use futures_util::StreamExt;
use regex::Regex;
use reqwest::{Response, header::LOCATION, redirect::Policy};
use serde_json::{Map, Value, json};
use time::{Duration as TimeDuration, OffsetDateTime};
use url::Url;

use crate::{
    credentials::CredentialStore,
    domain::{
        ArchiveEntry, DownloadPlan, DownloadSource, DownloadTask, InspectDocument, MediaChoices,
        MediaKind, MediaRequirements, MediaView, MuxFormat, MuxTrack, MuxTrackKind, OutputSpec,
        PartChoice, PlanSource, QualityChoice, SCHEMA_VERSION,
    },
    errors::{AppError, Result},
    platforms::{PlanSelection, ResolveOptions},
};

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";
const REFERER: &str = "https://www.bilibili.com/";
const DEFAULT_API_BASE: &str = "https://api.bilibili.com/";
const DEFAULT_MALL_BASE: &str = "https://mall.bilibili.com/";
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PARTS: usize = 10_000;

pub fn matches(input: &str) -> bool {
    host_regex().is_match(input) || parse_bare_identifier(input.trim()).is_some()
}

#[derive(Debug, Clone)]
pub struct BilibiliResolver {
    client: reqwest::Client,
    redirect_client: reqwest::Client,
    endpoints: Endpoints,
    credentials: CredentialStore,
    extra_bilibili_hosts: Vec<String>,
    extra_short_hosts: Vec<String>,
}

#[derive(Debug, Clone)]
struct Endpoints {
    api_base: Url,
    mall_base: Url,
}

impl BilibiliResolver {
    pub fn new(options: ResolveOptions) -> Result<Self> {
        Self::from_endpoints(
            options,
            Endpoints {
                api_base: parse_endpoint(DEFAULT_API_BASE)?,
                mall_base: parse_endpoint(DEFAULT_MALL_BASE)?,
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
        let mut base = parse_endpoint(base_url)?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let host = base.host_str().unwrap().to_ascii_lowercase();
        Self::from_endpoints(
            options,
            Endpoints {
                api_base: base.clone(),
                mall_base: base,
            },
            vec![host],
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
        extra_bilibili_hosts: Vec<String>,
        extra_short_hosts: Vec<String>,
    ) -> Result<Self> {
        let credentials =
            CredentialStore::new(options.config_dir.clone(), options.cookies_path.clone());
        Ok(Self {
            client: build_client(&options, Policy::limited(10))?,
            redirect_client: build_client(&options, Policy::none())?,
            endpoints,
            credentials,
            extra_bilibili_hosts,
            extra_short_hosts,
        })
    }

    pub async fn resolve(&self, input: &str) -> Result<BilibiliMedia> {
        let parsed = self.parse_input(input).await?;
        match parsed {
            ParsedInput::Video { id, page } => self.resolve_video(&id, page).await,
            ParsedInput::Bangumi { id_kind, id } => self.resolve_bangumi(id_kind, &id).await,
            ParsedInput::Collection { mid, season_id } => {
                self.resolve_playlist(PlaylistInput::Collection { mid, season_id })
                    .await
            }
            ParsedInput::Series { mid, series_id } => {
                self.resolve_playlist(PlaylistInput::Series { mid, series_id })
                    .await
            }
            ParsedInput::MediaList { raw_url, up_id } => {
                self.resolve_playlist(PlaylistInput::MediaList { raw_url, up_id })
                    .await
            }
            ParsedInput::Cheese { id_kind, id } => {
                self.resolve_cheese(id_kind, id.as_deref()).await
            }
            ParsedInput::Gongfang { order_id } => self.resolve_gongfang(&order_id).await,
        }
    }

    async fn parse_input(&self, input: &str) -> Result<ParsedInput> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(AppError::new(
                "input.url_not_found",
                "empty bilibili input",
                false,
            ));
        }
        if let Some(parsed) = parse_bare_identifier(trimmed) {
            return Ok(parsed);
        }
        let mut url = self.extract_share_url(trimmed).ok_or_else(|| {
            AppError::new(
                "input.url_not_found",
                "could not extract bilibili URL or identifier",
                false,
            )
        })?;
        for _ in 0..5 {
            let host = url
                .host_str()
                .map(str::to_ascii_lowercase)
                .ok_or_else(|| AppError::new("input.invalid_url", "invalid URL host", false))?;
            if !self.is_short_host(&host) {
                break;
            }
            url = self.resolve_short_once(&url).await?;
        }
        if url
            .host_str()
            .is_some_and(|host| self.is_short_host(&host.to_ascii_lowercase()))
        {
            return Err(AppError::new(
                "platform.short_link_failed",
                "b23 short link exceeded the redirect limit",
                false,
            ));
        }
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        if !self.is_bilibili_host(&host) {
            return Err(
                AppError::new("input.invalid_host", "invalid bilibili URL host", false)
                    .with_context("host", host),
            );
        }
        parse_url_identifier(url).ok_or_else(|| {
            AppError::new(
                "platform.identifier_not_found",
                "could not extract bilibili video or bangumi identifier",
                false,
            )
        })
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

    async fn resolve_short_once(&self, url: &Url) -> Result<Url> {
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
                    "b23 redirect contains an invalid Location header",
                    false,
                )
            })?;
            return url.join(location).map_err(|_| {
                AppError::new(
                    "platform.invalid_response",
                    "b23 redirect contains an invalid URL",
                    false,
                )
            });
        }
        Err(AppError::new(
            "platform.short_link_failed",
            format!("unexpected b23 HTTP status {}", status.as_u16()),
            status.is_server_error(),
        )
        .with_context("status", status.as_u16()))
    }

    async fn resolve_video(&self, id: &str, page: Option<u32>) -> Result<BilibiliMedia> {
        let view = self.fetch_video_view(id).await?;
        if let Some(redirect_url) = string_value(view.get("redirect_url"))
            && let Ok(url) = Url::parse(&redirect_url)
            && let Some(ParsedInput::Bangumi { id_kind, id }) = parse_url_identifier(url)
        {
            return self.resolve_bangumi(id_kind, &id).await;
        }
        let bvid = string_value(view.get("bvid")).unwrap_or_default();
        let aid = positive_u64(view.get("aid"));
        let title = string_value(view.get("title")).unwrap_or_else(|| id.to_owned());
        let owner = view
            .get("owner")
            .and_then(Value::as_object)
            .and_then(|owner| string_value(owner.get("name")));
        let cover = string_value(view.get("pic"));
        let mut parts = Vec::new();
        if let Some(pages) = view.get("pages").and_then(Value::as_array) {
            for (index, value) in pages.iter().enumerate() {
                let Some(page_data) = value.as_object() else {
                    continue;
                };
                let Some(cid) = positive_u64(page_data.get("cid")) else {
                    continue;
                };
                parts.push(BiliPart {
                    index: parts.len(),
                    page: positive_u32(page_data.get("page")).unwrap_or((index + 1) as u32),
                    title: string_value(page_data.get("part")).unwrap_or_else(|| title.clone()),
                    cid,
                    bvid: (!bvid.is_empty()).then(|| bvid.clone()),
                    aid,
                    ep_id: None,
                    _duration: positive_u64(page_data.get("duration")),
                    subtitles: Vec::new(),
                });
            }
        }
        if parts.is_empty() {
            let cid = positive_u64(view.get("cid")).ok_or_else(|| {
                AppError::new(
                    "platform.invalid_response",
                    "bilibili view response is missing cid",
                    false,
                )
            })?;
            parts.push(BiliPart {
                index: 0,
                page: 1,
                title: title.clone(),
                cid,
                bvid: (!bvid.is_empty()).then(|| bvid.clone()),
                aid,
                ep_id: None,
                _duration: positive_u64(view.get("duration")),
                subtitles: Vec::new(),
            });
        }
        ensure_part_limit(&parts)?;
        for part in &mut parts {
            if let Some(bvid) = part.bvid.as_deref() {
                part.subtitles = self
                    .fetch_subtitles(bvid, part.cid)
                    .await
                    .unwrap_or_default();
            }
        }
        let current_index = page
            .map(|page| page.saturating_sub(1) as usize)
            .unwrap_or_default()
            .min(parts.len() - 1);
        let streams = self
            .fetch_part_streams(BiliSourceKind::Video, &parts[current_index])
            .await?;
        let mut cached_streams = BTreeMap::new();
        cached_streams.insert(current_index, streams.clone());
        Ok(BilibiliMedia {
            resolver: self.clone(),
            id: if bvid.is_empty() {
                aid.map(|aid| format!("av{aid}"))
                    .unwrap_or_else(|| id.to_owned())
            } else {
                bvid
            },
            title,
            author: owner,
            cover,
            media_kind: if parts.len() > 1 {
                MediaKind::Playlist
            } else {
                MediaKind::Video
            },
            requires_login: false,
            kind: BilibiliMediaKind::Playable {
                source_kind: BiliSourceKind::Video,
                parts,
                current_index,
                cached_streams,
            },
        })
    }

    async fn resolve_bangumi(&self, id_kind: BangumiIdKind, id: &str) -> Result<BilibiliMedia> {
        let season = self.fetch_bangumi_season(id_kind, id).await?;
        let episodes = season
            .get("episodes")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                AppError::new(
                    "platform.not_found",
                    "bilibili season has no episodes",
                    false,
                )
            })?;
        let mut parts = Vec::new();
        for episode in episodes {
            let Some(episode) = episode.as_object() else {
                continue;
            };
            let Some(cid) = positive_u64(episode.get("cid")) else {
                continue;
            };
            let episode_title = [
                string_value(episode.get("title")),
                string_value(episode.get("long_title")),
            ]
            .into_iter()
            .flatten()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
            parts.push(BiliPart {
                index: parts.len(),
                page: parts.len() as u32 + 1,
                title: if episode_title.is_empty() {
                    format!("P{}", parts.len() + 1)
                } else {
                    episode_title
                },
                cid,
                bvid: string_value(episode.get("bvid")),
                aid: positive_u64(episode.get("aid")),
                ep_id: positive_u64(episode.get("ep_id"))
                    .or_else(|| positive_u64(episode.get("id"))),
                _duration: positive_u64(episode.get("duration")),
                subtitles: Vec::new(),
            });
        }
        if parts.is_empty() {
            return Err(AppError::new(
                "platform.not_found",
                "bilibili season has no episodes",
                false,
            ));
        }
        ensure_part_limit(&parts)?;
        let current_index = match id_kind {
            BangumiIdKind::Episode => parts
                .iter()
                .position(|part| part.ep_id.is_some_and(|episode| episode.to_string() == id))
                .unwrap_or_default(),
            _ => episodes
                .iter()
                .position(|episode| {
                    episode
                        .get("section_type")
                        .map_or(0, |value| flexible_i64(Some(value)))
                        == 0
                })
                .unwrap_or_default()
                .min(parts.len() - 1),
        };
        let streams = self
            .fetch_part_streams(BiliSourceKind::Bangumi, &parts[current_index])
            .await?;
        let mut cached_streams = BTreeMap::new();
        cached_streams.insert(current_index, streams);
        let title = string_value(season.get("season_title"))
            .or_else(|| string_value(season.get("title")))
            .unwrap_or_else(|| format!("bangumi_{id}"));
        let author = string_value(season.get("title"));
        let current = &parts[current_index];
        Ok(BilibiliMedia {
            resolver: self.clone(),
            id: current
                .bvid
                .clone()
                .or_else(|| current.ep_id.map(|id| format!("ep{id}")))
                .unwrap_or_else(|| format!("bangumi-{id}")),
            title,
            author,
            cover: string_value(season.get("cover")),
            media_kind: MediaKind::Bangumi,
            requires_login: false,
            kind: BilibiliMediaKind::Playable {
                source_kind: BiliSourceKind::Bangumi,
                parts,
                current_index,
                cached_streams,
            },
        })
    }

    async fn resolve_playlist(&self, input: PlaylistInput) -> Result<BilibiliMedia> {
        let media_id = match &input {
            PlaylistInput::Collection { .. } => "bili-collection",
            PlaylistInput::Series { .. } => "bili-series",
            PlaylistInput::MediaList { .. } => "bili-mediaList",
        };
        let (title, bvids) = self.fetch_playlist_bvids(input).await?;
        let mut parts = Vec::new();
        for (video_index, bvid) in bvids.iter().enumerate() {
            let view = self.fetch_video_view(bvid).await?;
            let view_bvid = string_value(view.get("bvid")).unwrap_or_else(|| bvid.clone());
            let aid = positive_u64(view.get("aid"));
            let video_title = string_value(view.get("title")).unwrap_or_else(|| bvid.clone());
            let pages = view
                .get("pages")
                .and_then(Value::as_array)
                .filter(|pages| !pages.is_empty())
                .cloned()
                .unwrap_or_else(|| {
                    vec![json!({
                        "cid": positive_u64(view.get("cid")),
                        "page": 1,
                        "part": video_title,
                        "duration": positive_u64(view.get("duration"))
                    })]
                });
            for (page_index, page) in pages.iter().enumerate() {
                let Some(page) = page.as_object() else {
                    continue;
                };
                let Some(cid) = positive_u64(page.get("cid")) else {
                    continue;
                };
                let page_title = string_value(page.get("part")).unwrap_or_default();
                let title = if pages.len() > 1 {
                    format!(
                        "[{:02}] {} P{} {}",
                        video_index + 1,
                        video_title,
                        page_index + 1,
                        page_title
                    )
                    .trim()
                    .to_owned()
                } else {
                    format!("[{:02}] {}", video_index + 1, video_title)
                };
                let subtitles = self
                    .fetch_subtitles(&view_bvid, cid)
                    .await
                    .unwrap_or_default();
                parts.push(BiliPart {
                    index: parts.len(),
                    page: parts.len() as u32 + 1,
                    title,
                    cid,
                    bvid: Some(view_bvid.clone()),
                    aid,
                    ep_id: None,
                    _duration: positive_u64(page.get("duration")),
                    subtitles,
                });
                ensure_part_limit(&parts)?;
            }
        }
        if parts.is_empty() {
            return Err(AppError::new(
                "platform.not_found",
                "bilibili playlist has no videos",
                false,
            ));
        }
        let streams = self
            .fetch_part_streams(BiliSourceKind::Video, &parts[0])
            .await?;
        let mut cached_streams = BTreeMap::new();
        cached_streams.insert(0, streams);
        Ok(BilibiliMedia {
            resolver: self.clone(),
            id: media_id.to_owned(),
            title,
            author: None,
            cover: None,
            media_kind: MediaKind::Playlist,
            requires_login: false,
            kind: BilibiliMediaKind::Playable {
                source_kind: BiliSourceKind::Video,
                parts,
                current_index: 0,
                cached_streams,
            },
        })
    }

    async fn resolve_cheese(
        &self,
        id_kind: CheeseIdKind,
        id: Option<&str>,
    ) -> Result<BilibiliMedia> {
        if id_kind == CheeseIdKind::Paid {
            return self.resolve_paid_cheese().await;
        }
        let season = self
            .fetch_cheese_season(id_kind, id.unwrap_or_default())
            .await?;
        let title = string_value(season.get("title"))
            .unwrap_or_else(|| format!("cheese_{}", id.unwrap_or_default()));
        let parts = cheese_parts(&season)?;
        self.build_cheese_media(title, parts).await
    }

    async fn resolve_paid_cheese(&self) -> Result<BilibiliMedia> {
        let mut courses = Vec::<(String, String)>::new();
        let mut seen = BTreeSet::new();
        for page in 1..=500 {
            let mut url = self.api_url("pugv/pay/web/my/paid")?;
            url.query_pairs_mut()
                .append_pair("ps", "10")
                .append_pair("pn", &page.to_string());
            let data = self.fetch_api_data(url).await?;
            let mut values = Vec::new();
            if let Some(items) = data.get("data").and_then(Value::as_array) {
                values.extend(items);
            }
            if let Some(items) = data.get("list").and_then(Value::as_array) {
                values.extend(items);
            }
            if values.is_empty() {
                break;
            }
            for value in values {
                let Some(value) = value.as_object() else {
                    continue;
                };
                let course_id =
                    string_id(value.get("id")).or_else(|| string_id(value.get("season_id")));
                let Some(course_id) = course_id.filter(|id| seen.insert(id.clone())) else {
                    continue;
                };
                courses.push((
                    course_id.clone(),
                    string_value(value.get("title"))
                        .unwrap_or_else(|| format!("course_{course_id}")),
                ));
            }
            let total = positive_u64(
                data.get("page")
                    .and_then(Value::as_object)
                    .and_then(|page| page.get("total")),
            )
            .or_else(|| positive_u64(data.get("total")))
            .unwrap_or_default();
            if total > 0 && courses.len() as u64 >= total {
                break;
            }
        }
        if courses.is_empty() {
            return Err(AppError::new(
                "auth.required",
                "bilibili cheese paid list is empty or requires login",
                false,
            ));
        }
        let mut all_parts = Vec::new();
        for (course_id, course_title) in courses {
            let Ok(season) = self
                .fetch_cheese_season(CheeseIdKind::Season, &course_id)
                .await
            else {
                continue;
            };
            let Ok(parts) = cheese_parts(&season) else {
                continue;
            };
            for mut part in parts {
                part.index = all_parts.len();
                part.page = all_parts.len() as u32 + 1;
                part.title = format!("{course_title} -- {}", part.title);
                all_parts.push(part);
                ensure_part_limit(&all_parts)?;
            }
        }
        self.build_cheese_media("Bilibili Cheese Paid Courses".to_owned(), all_parts)
            .await
    }

    async fn build_cheese_media(
        &self,
        title: String,
        parts: Vec<BiliPart>,
    ) -> Result<BilibiliMedia> {
        if parts.is_empty() {
            return Err(AppError::new(
                "platform.not_found",
                "bilibili cheese has no episodes",
                false,
            ));
        }
        let streams = self
            .fetch_part_streams(BiliSourceKind::Cheese, &parts[0])
            .await?;
        let mut cached_streams = BTreeMap::new();
        cached_streams.insert(0, streams);
        Ok(BilibiliMedia {
            resolver: self.clone(),
            id: format!("cheese-{}", parts[0].ep_id.unwrap_or_default()),
            title,
            author: None,
            cover: None,
            media_kind: MediaKind::Playlist,
            requires_login: true,
            kind: BilibiliMediaKind::Playable {
                source_kind: BiliSourceKind::Cheese,
                parts,
                current_index: 0,
                cached_streams,
            },
        })
    }

    async fn resolve_gongfang(&self, order_id: &str) -> Result<BilibiliMedia> {
        let title = self
            .fetch_gongfang_title(order_id)
            .await
            .unwrap_or_else(|_| format!("gongfang_{order_id}"));
        let detail_url = self.mall_url("mall-c/ship/orderdetails/query")?;
        let detail = self
            .post_json(detail_url, json!({ "orderId": order_id }))
            .await?;
        ensure_api_code(&detail, "bilibili gongfang detail")?;
        let items = detail
            .get("data")
            .and_then(Value::as_object)
            .and_then(|data| data.get("shipOrderDetails"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut entries = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let Some(item) = item.as_object() else {
                continue;
            };
            let Some(source_id) = string_id(item.get("shipOrderDetailsId")) else {
                continue;
            };
            let Some(content_type) = string_value(item.get("fileContentType")) else {
                continue;
            };
            let download_url = self.mall_url("mall-c/ship/orderdetails/querydownloadurl")?;
            let Ok(payload) = self
                .post_json(
                    download_url,
                    json!({ "orderId": order_id, "shipOrderDetailsId": source_id }),
                )
                .await
            else {
                continue;
            };
            if ensure_api_code(&payload, "bilibili gongfang download").is_err() {
                continue;
            }
            let url = payload.get("data").and_then(|data| {
                string_value(Some(data)).or_else(|| {
                    data.as_object()
                        .and_then(|data| string_value(data.get("url")))
                })
            });
            let Some(url) = url else {
                continue;
            };
            let file_name = string_value(item.get("fileName")).unwrap_or_else(|| source_id.clone());
            let stem = strip_extension(&file_name, &content_type);
            entries.push(ArchiveEntry {
                id: format!("file-{}", index + 1),
                path: format!(
                    "{:02}_{}.{}",
                    index + 1,
                    safe_file_stem(&stem, &source_id),
                    extension_from_content_type(&content_type, &url)
                ),
                sources: download_sources(std::slice::from_ref(&url)),
            });
        }
        if entries.is_empty() {
            return Err(AppError::new(
                "auth.required",
                "bilibili gongfang has no downloadable files; login may be required",
                false,
            ));
        }
        Ok(BilibiliMedia {
            resolver: self.clone(),
            id: format!("gongfang-{order_id}"),
            title,
            author: None,
            cover: None,
            media_kind: MediaKind::Playlist,
            requires_login: true,
            kind: BilibiliMediaKind::Gongfang { entries },
        })
    }

    async fn fetch_gongfang_title(&self, order_id: &str) -> Result<String> {
        let mut url = self.mall_url("mall-c/order/detail")?;
        url.query_pairs_mut().append_pair("orderId", order_id);
        let html = self.get_text(url).await?;
        let raw = items_name_regex()
            .captures(&html)
            .and_then(|capture| capture.get(1))
            .ok_or_else(|| {
                AppError::new(
                    "platform.invalid_response",
                    "bilibili gongfang title was not found",
                    false,
                )
            })?;
        serde_json::from_str::<String>(&format!("\"{}\"", raw.as_str())).map_err(|error| {
            AppError::new(
                "platform.invalid_response",
                format!("invalid bilibili gongfang title: {error}"),
                false,
            )
        })
    }

    async fn fetch_video_view(&self, video_id: &str) -> Result<Item> {
        let mut url = self.api_url("x/web-interface/view")?;
        if let Some(aid) = video_id
            .strip_prefix("av")
            .or_else(|| video_id.strip_prefix("AV"))
        {
            url.query_pairs_mut().append_pair("aid", aid);
        } else {
            url.query_pairs_mut().append_pair("bvid", video_id);
        }
        self.fetch_api_data(url).await
    }

    async fn fetch_bangumi_season(&self, id_kind: BangumiIdKind, id: &str) -> Result<Item> {
        let mut url = self.api_url("pgc/view/web/season")?;
        let key = match id_kind {
            BangumiIdKind::Episode => "ep_id",
            BangumiIdKind::Season => "season_id",
            BangumiIdKind::Media => "media_id",
        };
        url.query_pairs_mut().append_pair(key, id);
        let payload = self.get_json(url).await?;
        if api_code(&payload) != 0 && id_kind == BangumiIdKind::Media {
            let season_id = self.resolve_media_season_id(id).await?;
            let mut fallback = self.api_url("pgc/view/web/season")?;
            fallback
                .query_pairs_mut()
                .append_pair("season_id", &season_id.to_string());
            return self.fetch_pgc_result(fallback).await;
        }
        pgc_result(payload, "bilibili season")
    }

    async fn resolve_media_season_id(&self, media_id: &str) -> Result<u64> {
        let mut url = self.api_url("pgc/review/user")?;
        url.query_pairs_mut().append_pair("media_id", media_id);
        let result = self.fetch_pgc_result(url).await?;
        positive_u64(
            result
                .get("media")
                .and_then(Value::as_object)
                .and_then(|media| media.get("season_id")),
        )
        .or_else(|| positive_u64(result.get("season_id")))
        .ok_or_else(|| {
            AppError::new(
                "platform.invalid_response",
                "season_id not found for bilibili media_id",
                false,
            )
        })
    }

    async fn fetch_cheese_season(&self, id_kind: CheeseIdKind, id: &str) -> Result<Item> {
        let (path, key) = match id_kind {
            CheeseIdKind::Episode => ("pugv/view/web/season", "ep_id"),
            CheeseIdKind::Season => ("pugv/view/web/season/v2", "season_id"),
            CheeseIdKind::Paid => {
                return Err(AppError::new(
                    "platform.invalid_input",
                    "paid cheese list does not map to one season",
                    false,
                ));
            }
        };
        let mut url = self.api_url(path)?;
        url.query_pairs_mut().append_pair(key, id);
        self.fetch_api_data(url).await
    }

    async fn fetch_playlist_bvids(&self, input: PlaylistInput) -> Result<(String, Vec<String>)> {
        match input {
            PlaylistInput::Collection { mid, season_id } => {
                self.fetch_collection_bvids(&mid, &season_id).await
            }
            PlaylistInput::Series { mid, series_id } => {
                self.fetch_series_bvids(&mid, &series_id).await
            }
            PlaylistInput::MediaList { raw_url, up_id } => {
                self.fetch_media_list_bvids(raw_url, &up_id).await
            }
        }
    }

    async fn fetch_collection_bvids(
        &self,
        mid: &str,
        season_id: &str,
    ) -> Result<(String, Vec<String>)> {
        let mut title = format!("bilibili_collection_{season_id}");
        let mut bvids = Vec::new();
        for page in 1..=99 {
            let mut url = self.api_url("x/polymer/web-space/seasons_archives_list")?;
            url.query_pairs_mut()
                .append_pair("mid", mid)
                .append_pair("season_id", season_id)
                .append_pair("sort_reverse", "false")
                .append_pair("page_num", &page.to_string())
                .append_pair("page_size", "99");
            let payload = self.get_json(url).await?;
            if api_code(&payload) != 0 {
                if page == 1 {
                    return Err(api_response_error(&payload, "bilibili collection"));
                }
                break;
            }
            let data = payload
                .get("data")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    AppError::new(
                        "platform.invalid_response",
                        "bilibili collection response is missing data",
                        false,
                    )
                })?;
            if let Some(name) = data
                .get("meta")
                .and_then(Value::as_object)
                .and_then(|meta| string_value(meta.get("name")))
            {
                title = name;
            }
            let page_bvids = data
                .get("archives")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_object)
                .filter_map(|archive| string_value(archive.get("bvid")))
                .collect::<Vec<_>>();
            if page_bvids.is_empty() {
                break;
            }
            bvids.extend(page_bvids);
            if bvids.len() > MAX_PARTS {
                return Err(resource_limit_error());
            }
        }
        finish_bvids(title, bvids, "bilibili collection has no videos")
    }

    async fn fetch_series_bvids(
        &self,
        mid: &str,
        series_id: &str,
    ) -> Result<(String, Vec<String>)> {
        let mut title = format!("bilibili_series_{series_id}");
        let mut meta_url = self.api_url("x/series/series")?;
        meta_url
            .query_pairs_mut()
            .append_pair("series_id", series_id);
        if let Ok(data) = self.fetch_api_data(meta_url).await {
            title = data
                .get("meta")
                .and_then(Value::as_object)
                .and_then(|meta| string_value(meta.get("name")))
                .or_else(|| string_value(data.get("name")))
                .unwrap_or(title);
        }
        let mut bvids = Vec::new();
        for page in 1..=99 {
            let mut url = self.api_url("x/series/archives")?;
            url.query_pairs_mut()
                .append_pair("mid", mid)
                .append_pair("series_id", series_id)
                .append_pair("only_normal", "true")
                .append_pair("sort", "desc")
                .append_pair("pn", &page.to_string())
                .append_pair("ps", "99");
            let payload = self.get_json(url).await?;
            if api_code(&payload) != 0 {
                if page == 1 {
                    return Err(api_response_error(&payload, "bilibili series"));
                }
                break;
            }
            let page_bvids = payload
                .get("data")
                .and_then(Value::as_object)
                .and_then(|data| data.get("archives"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_object)
                .filter_map(|archive| string_value(archive.get("bvid")))
                .collect::<Vec<_>>();
            if page_bvids.is_empty() {
                break;
            }
            bvids.extend(page_bvids);
            if bvids.len() > MAX_PARTS {
                return Err(resource_limit_error());
            }
        }
        finish_bvids(title, bvids, "bilibili series has no videos")
    }

    async fn fetch_media_list_bvids(
        &self,
        raw_url: Url,
        up_id: &str,
    ) -> Result<(String, Vec<String>)> {
        if let Ok(html) = self.get_text(raw_url).await
            && let Some(capture) = media_list_redirect_regex().captures(&html)
        {
            let mid = capture.get(1).unwrap().as_str();
            let kind = capture.get(2).unwrap().as_str();
            let sid = capture.get(3).unwrap().as_str();
            if kind.eq_ignore_ascii_case("collection") {
                return self.fetch_collection_bvids(mid, sid).await;
            }
            return self.fetch_series_bvids(mid, sid).await;
        }
        let mut url = self.api_url("x/v2/medialist/resource/list")?;
        url.query_pairs_mut()
            .append_pair("mobi_app", "web")
            .append_pair("type", "1")
            .append_pair("biz_id", up_id)
            .append_pair("oid", "0")
            .append_pair("otype", "2")
            .append_pair("ps", "99")
            .append_pair("direction", "false")
            .append_pair("desc", "false")
            .append_pair("sort_field", "1")
            .append_pair("tid", "0")
            .append_pair("with_current", "false");
        let data = self.fetch_api_data(url).await?;
        let bvids = data
            .get("media_list")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
            .filter_map(|item| string_value(item.get("bv_id")))
            .collect::<Vec<_>>();
        let title = data
            .get("upper")
            .and_then(Value::as_object)
            .and_then(|upper| string_value(upper.get("name")))
            .unwrap_or_else(|| format!("bilibili_list_{up_id}"));
        finish_bvids(title, bvids, "bilibili media list has no videos")
    }

    async fn fetch_subtitles(&self, bvid: &str, cid: u64) -> Result<Vec<BiliSubtitle>> {
        let mut url = self.api_url("x/player/v2")?;
        url.query_pairs_mut()
            .append_pair("bvid", bvid)
            .append_pair("cid", &cid.to_string());
        let payload = self.get_json(url).await?;
        if api_code(&payload) != 0 {
            return Ok(Vec::new());
        }
        let subtitles = payload
            .get("data")
            .and_then(Value::as_object)
            .and_then(|data| data.get("subtitle"))
            .and_then(Value::as_object)
            .and_then(|subtitle| subtitle.get("subtitles"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(subtitles
            .iter()
            .filter_map(Value::as_object)
            .filter_map(|subtitle| {
                let mut url = string_value(subtitle.get("subtitle_url"))?;
                if url.starts_with("//") {
                    url = format!("https:{url}");
                }
                Some(BiliSubtitle {
                    language: string_value(subtitle.get("lan_doc"))
                        .or_else(|| string_value(subtitle.get("lan")))
                        .unwrap_or_else(|| "unknown".to_owned()),
                    url,
                })
            })
            .collect())
    }

    async fn fetch_part_streams(
        &self,
        kind: BiliSourceKind,
        part: &BiliPart,
    ) -> Result<Vec<BiliStream>> {
        let mut url = match kind {
            BiliSourceKind::Video => {
                let mut url = self.api_url("x/player/playurl")?;
                if let Some(bvid) = part.bvid.as_deref() {
                    url.query_pairs_mut().append_pair("bvid", bvid);
                } else if let Some(aid) = part.aid {
                    url.query_pairs_mut().append_pair("avid", &aid.to_string());
                }
                url.query_pairs_mut()
                    .append_pair("cid", &part.cid.to_string())
                    .append_pair("fnval", "4048")
                    .append_pair("fnver", "0")
                    .append_pair("fourk", "1");
                url
            }
            BiliSourceKind::Bangumi => {
                let mut url = self.api_url("pgc/player/web/playurl")?;
                if let Some(bvid) = part.bvid.as_deref() {
                    url.query_pairs_mut().append_pair("bvid", bvid);
                }
                if let Some(ep_id) = part.ep_id {
                    url.query_pairs_mut()
                        .append_pair("ep_id", &ep_id.to_string());
                }
                url.query_pairs_mut()
                    .append_pair("cid", &part.cid.to_string())
                    .append_pair("qn", "127")
                    .append_pair("fnval", "4048")
                    .append_pair("fnver", "0")
                    .append_pair("fourk", "1")
                    .append_pair("drm_tech_type", "2")
                    .append_pair("otype", "json")
                    .append_pair("platform", "web");
                url
            }
            BiliSourceKind::Cheese => {
                let ep_id = part.ep_id.ok_or_else(|| {
                    AppError::new(
                        "platform.invalid_response",
                        "bilibili cheese part is missing ep_id",
                        false,
                    )
                })?;
                let mut url = self.api_url("pugv/player/web/playurl")?;
                url.query_pairs_mut()
                    .append_pair("fnval", "16")
                    .append_pair("fourk", "1")
                    .append_pair("ep_id", &ep_id.to_string());
                url
            }
        };
        url.set_fragment(None);
        let payload = self.get_json(url).await?;
        if api_code(&payload) != 0 {
            return Err(api_response_error(&payload, "bilibili playurl"));
        }
        let data = payload
            .get("data")
            .or_else(|| payload.get("result"))
            .and_then(Value::as_object)
            .ok_or_else(|| {
                AppError::new(
                    "platform.invalid_response",
                    "bilibili playurl response is missing data",
                    false,
                )
            })?;
        if serde_json::to_string(data).is_ok_and(|value| value.contains("bilidrm_uri")) {
            return Err(AppError::new(
                "platform.drm_unsupported",
                "bilibili DRM protected content is not supported",
                false,
            ));
        }
        let streams = build_streams(data);
        if streams.is_empty() {
            return Err(AppError::new(
                "auth.required",
                "bilibili playurl has no DASH video/audio streams; login or VIP permission may be required",
                false,
            ));
        }
        Ok(streams)
    }

    async fn fetch_api_data(&self, url: Url) -> Result<Item> {
        let payload = self.get_json(url).await?;
        if api_code(&payload) != 0 {
            return Err(api_response_error(&payload, "bilibili API"));
        }
        payload
            .get("data")
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| {
                AppError::new(
                    "platform.invalid_response",
                    "bilibili API response is missing data",
                    false,
                )
            })
    }

    async fn fetch_pgc_result(&self, url: Url) -> Result<Item> {
        let payload = self.get_json(url).await?;
        pgc_result(payload, "bilibili PGC API")
    }

    async fn get_json(&self, url: Url) -> Result<Value> {
        let response = self
            .client
            .get(url.clone())
            .headers(self.api_headers(&url).await?)
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
                format!("invalid bilibili JSON response: {error}"),
                false,
            )
        })
    }

    async fn get_text(&self, url: Url) -> Result<String> {
        let response = self
            .client
            .get(url.clone())
            .headers(self.api_headers(&url).await?)
            .send()
            .await
            .map_err(|error| AppError::http_request(&error))?;
        if !response.status().is_success() {
            return Err(AppError::http_status(response.status()));
        }
        read_limited_text(response, MAX_RESPONSE_BYTES).await
    }

    async fn post_json(&self, url: Url, body: Value) -> Result<Value> {
        let response = self
            .client
            .post(url.clone())
            .headers(self.api_headers(&url).await?)
            .header("Content-Type", "application/json;charset=utf-8")
            .json(&body)
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
                format!("invalid bilibili JSON response: {error}"),
                false,
            )
        })
    }

    async fn api_headers(&self, url: &Url) -> Result<reqwest::header::HeaderMap> {
        let cookie = self.credentials.cookie_header_for(url).await?;
        Ok(page_headers(cookie.as_deref()))
    }

    fn api_url(&self, path: &str) -> Result<Url> {
        self.endpoints
            .api_base
            .join(path)
            .map_err(|_| endpoint_error())
    }

    fn mall_url(&self, path: &str) -> Result<Url> {
        self.endpoints
            .mall_base
            .join(path)
            .map_err(|_| endpoint_error())
    }

    fn is_acceptable_host(&self, host: &str) -> bool {
        self.is_short_host(host) || self.is_bilibili_host(host)
    }

    fn is_bilibili_host(&self, host: &str) -> bool {
        domain_matches(host, "bilibili.com")
            || self
                .extra_bilibili_hosts
                .iter()
                .any(|allowed| host == allowed)
    }

    fn is_short_host(&self, host: &str) -> bool {
        host == "b23.tv" || self.extra_short_hosts.iter().any(|allowed| host == allowed)
    }
}

#[derive(Debug, Clone)]
pub struct BilibiliMedia {
    resolver: BilibiliResolver,
    id: String,
    title: String,
    author: Option<String>,
    cover: Option<String>,
    media_kind: MediaKind,
    requires_login: bool,
    kind: BilibiliMediaKind,
}

#[derive(Debug, Clone)]
enum BilibiliMediaKind {
    Playable {
        source_kind: BiliSourceKind,
        parts: Vec<BiliPart>,
        current_index: usize,
        cached_streams: BTreeMap<usize, Vec<BiliStream>>,
    },
    Gongfang {
        entries: Vec<ArchiveEntry>,
    },
}

#[derive(Debug, Clone, Copy)]
enum BiliSourceKind {
    Video,
    Bangumi,
    Cheese,
}

#[derive(Debug, Clone)]
struct BiliPart {
    index: usize,
    page: u32,
    title: String,
    cid: u64,
    bvid: Option<String>,
    aid: Option<u64>,
    ep_id: Option<u64>,
    _duration: Option<u64>,
    subtitles: Vec<BiliSubtitle>,
}

#[derive(Debug, Clone)]
struct BiliSubtitle {
    language: String,
    url: String,
}

#[derive(Debug, Clone)]
struct BiliStream {
    id: String,
    label: String,
    width: Option<u32>,
    height: Option<u32>,
    size: Option<u64>,
    video_urls: Vec<String>,
    audio_urls: Vec<String>,
}

impl BilibiliMedia {
    pub fn inspect(&self) -> InspectDocument {
        let (qualities, parts, ffmpeg) = match &self.kind {
            BilibiliMediaKind::Playable {
                parts,
                current_index,
                cached_streams,
                ..
            } => (
                cached_streams
                    .get(current_index)
                    .into_iter()
                    .flatten()
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
                parts
                    .iter()
                    .map(|part| PartChoice {
                        id: format!("part-{}", part.index + 1),
                        index: (part.index + 1) as u32,
                        title: part.title.clone(),
                    })
                    .collect(),
                true,
            ),
            BilibiliMediaKind::Gongfang { .. } => (Vec::new(), Vec::new(), false),
        };
        InspectDocument {
            schema_version: SCHEMA_VERSION,
            platform: "bilibili".to_owned(),
            media: MediaView {
                id: self.id.clone(),
                kind: self.media_kind,
                title: self.title.clone(),
                author: self.author.clone(),
                cover: self.cover.clone(),
            },
            choices: MediaChoices {
                qualities,
                parts,
                images: Vec::new(),
            },
            requirements: MediaRequirements {
                login: self.requires_login,
                ffmpeg,
            },
        }
    }

    pub async fn create_plan(&self, selection: &PlanSelection) -> Result<DownloadPlan> {
        let created_at = OffsetDateTime::now_utc();
        let tasks = match &self.kind {
            BilibiliMediaKind::Gongfang { entries } => {
                let relative_path = selection
                    .output
                    .clone()
                    .unwrap_or_else(|| format!("{}.zip", safe_file_stem(&self.title, &self.id)));
                vec![DownloadTask::Archive {
                    id: self.id.clone(),
                    entries: entries.clone(),
                    output: OutputSpec { relative_path },
                }]
            }
            BilibiliMediaKind::Playable {
                source_kind,
                parts,
                current_index,
                cached_streams,
            } => {
                let selected = select_parts(parts, *current_index, selection)?;
                if selected.len() > 1 && selection.output.is_some() {
                    return Err(AppError::new(
                        "selection.output_ambiguous",
                        "--output cannot name multiple bilibili part outputs",
                        false,
                    ));
                }
                let mut tasks = Vec::new();
                for part in &selected {
                    let streams = if let Some(streams) = cached_streams.get(&part.index) {
                        streams.clone()
                    } else {
                        self.resolver.fetch_part_streams(*source_kind, part).await?
                    };
                    let stream =
                        pick_stream(&streams, selection.quality.as_deref().unwrap_or("best"))?;
                    let default_stem = self.file_stem((selected.len() > 1).then_some(*part));
                    let relative_path = selection
                        .output
                        .clone()
                        .unwrap_or_else(|| format!("{default_stem}.mp4"));
                    let task_id = if selected.len() == 1 {
                        self.id.clone()
                    } else {
                        format!("{}-p{}", self.id, part.page)
                    };
                    tasks.push(DownloadTask::Mux {
                        id: task_id,
                        tracks: vec![
                            MuxTrack {
                                id: "video".to_owned(),
                                kind: MuxTrackKind::Video,
                                sources: download_sources(&stream.video_urls),
                            },
                            MuxTrack {
                                id: "audio".to_owned(),
                                kind: MuxTrackKind::Audio,
                                sources: download_sources(&stream.audio_urls),
                            },
                        ],
                        format: MuxFormat::Mp4,
                        output: OutputSpec {
                            relative_path: relative_path.clone(),
                        },
                    });
                    let subtitle_stem = strip_output_extension(&relative_path);
                    for (subtitle_index, subtitle) in part.subtitles.iter().enumerate() {
                        let language = safe_file_stem(&subtitle.language, "subtitle");
                        tasks.push(DownloadTask::File {
                            id: format!(
                                "{}-p{}-sub-{}-{}",
                                self.id,
                                part.page,
                                subtitle_index + 1,
                                language
                            ),
                            sources: download_sources(std::slice::from_ref(&subtitle.url)),
                            output: OutputSpec {
                                relative_path: format!("{subtitle_stem}.{language}.json"),
                            },
                        });
                    }
                }
                tasks
            }
        };
        let plan = DownloadPlan {
            schema_version: SCHEMA_VERSION,
            created_at,
            expires_at: Some(created_at + TimeDuration::minutes(30)),
            source: PlanSource {
                platform: "bilibili".to_owned(),
                media_id: self.id.clone(),
            },
            tasks,
        };
        plan.validate()?;
        Ok(plan)
    }

    fn file_stem(&self, part: Option<&BiliPart>) -> String {
        if let Some(part) = part {
            return safe_file_stem(
                &format!("{}_P{:02}_{}", self.title, part.page, part.title),
                &self.id,
            );
        }
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

fn build_streams(data: &Item) -> Vec<BiliStream> {
    let Some(dash) = data.get("dash").and_then(Value::as_object) else {
        return Vec::new();
    };
    let Some(audio) = best_dash_audio(dash) else {
        return Vec::new();
    };
    let audio_urls = dash_urls(audio);
    let audio_bandwidth = positive_u64(audio.get("bandwidth")).unwrap_or_default();
    let mut by_quality = BTreeMap::<u32, Vec<&Item>>::new();
    for video in dash
        .get("video")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
    {
        if dash_urls(video).is_empty() {
            continue;
        }
        by_quality
            .entry(positive_u32(video.get("id")).unwrap_or_default())
            .or_default()
            .push(video);
    }
    let mut streams = Vec::new();
    for (quality, videos) in by_quality {
        let Some(video) = videos.into_iter().max_by_key(|video| {
            (
                codec_rank(positive_u32(video.get("codecid"))),
                positive_u64(video.get("bandwidth")).unwrap_or_default(),
            )
        }) else {
            continue;
        };
        let total_bandwidth = positive_u64(video.get("bandwidth"))
            .unwrap_or_default()
            .saturating_add(audio_bandwidth);
        streams.push(BiliStream {
            id: quality.to_string(),
            label: quality_name(data, quality),
            width: positive_u32(video.get("width")),
            height: positive_u32(video.get("height")),
            size: estimate_dash_size(data, dash, total_bandwidth),
            video_urls: dash_urls(video),
            audio_urls: audio_urls.clone(),
        });
    }
    streams.sort_by_key(|stream| std::cmp::Reverse(stream.id.parse::<u32>().unwrap_or_default()));
    streams
}

fn best_dash_audio(dash: &Item) -> Option<&Item> {
    if let Some(audio) = pick_best_audio_from_array(dash.get("audio")) {
        return Some(audio);
    }
    if let Some(audio) =
        pick_best_audio_from_array(dash.get("dolby").and_then(|value| value.get("audio")))
    {
        return Some(audio);
    }
    match dash.get("flac").and_then(|value| value.get("audio")) {
        Some(Value::Object(audio)) if !dash_urls(audio).is_empty() => Some(audio),
        other => pick_best_audio_from_array(other),
    }
}

fn pick_best_audio_from_array(value: Option<&Value>) -> Option<&Item> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter(|audio| !dash_urls(audio).is_empty())
        .max_by_key(|audio| positive_u64(audio.get("bandwidth")).unwrap_or_default())
}

fn estimate_dash_size(data: &Item, dash: &Item, total_bandwidth: u64) -> Option<u64> {
    if total_bandwidth == 0 {
        return None;
    }
    if let Some(timelength_ms) = positive_u64(data.get("timelength")) {
        let bytes = (u128::from(total_bandwidth) * u128::from(timelength_ms)) / 8_000;
        return u64::try_from(bytes).ok().filter(|&value| value > 0);
    }
    let duration_secs = positive_u64(dash.get("duration"))?;
    let bytes = (u128::from(total_bandwidth) * u128::from(duration_secs)) / 8;
    u64::try_from(bytes).ok().filter(|&value| value > 0)
}

fn dash_urls(item: &Item) -> Vec<String> {
    let mut urls = vec![
        string_value(item.get("baseUrl")).unwrap_or_default(),
        string_value(item.get("base_url")).unwrap_or_default(),
    ];
    urls.extend(string_array(item.get("backupUrl")));
    urls.extend(string_array(item.get("backup_url")));
    let mut urls = unique_strings(urls);
    if urls.iter().any(|url| is_bilibili_media_cdn_url(url)) {
        urls.sort_by_key(|url| bilibili_cdn_priority(url));
    }
    urls
}

fn is_bilibili_media_cdn_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    host.starts_with("upos-")
        || domain_matches(&host, "bilivideo.com")
        || domain_matches(&host, "bilivideo.cn")
        || domain_matches(&host, "acgvideo.com")
}

fn bilibili_cdn_priority(raw: &str) -> u8 {
    let Ok(url) = Url::parse(raw) else {
        return 2;
    };
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return 2;
    };
    if host.contains("mcdn.") || domain_matches(&host, "szbdyd.com") {
        return 3;
    }
    if host.starts_with("upos-") {
        return 0;
    }
    if domain_matches(&host, "bilivideo.com") || domain_matches(&host, "acgvideo.com") {
        return 1;
    }
    if url.port().is_some_and(|port| port != 80 && port != 443) {
        return 3;
    }
    2
}

fn quality_name(data: &Item, quality: u32) -> String {
    if let Some(format) = data
        .get("support_formats")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .find(|format| positive_u32(format.get("quality")) == Some(quality))
        && let Some(value) = ["new_description", "display_desc", "format"]
            .into_iter()
            .find_map(|key| string_value(format.get(key)))
    {
        return value;
    }
    let index = data
        .get("accept_quality")
        .and_then(Value::as_array)
        .and_then(|qualities| {
            qualities
                .iter()
                .position(|value| positive_u32(Some(value)) == Some(quality))
        });
    if let Some(index) = index
        && let Some(value) = data
            .get("accept_description")
            .and_then(Value::as_array)
            .and_then(|descriptions| descriptions.get(index))
            .and_then(|value| string_value(Some(value)))
    {
        return value;
    }
    default_quality_name(quality)
}

fn default_quality_name(quality: u32) -> String {
    match quality {
        127 => "8K".to_owned(),
        126 => "Dolby Vision".to_owned(),
        125 => "HDR".to_owned(),
        120 => "4K".to_owned(),
        116 => "1080P60".to_owned(),
        112 => "1080P+".to_owned(),
        80 => "1080P".to_owned(),
        74 => "720P60".to_owned(),
        64 => "720P".to_owned(),
        32 => "480P".to_owned(),
        16 => "360P".to_owned(),
        _ => format!("未知({quality})"),
    }
}

fn codec_rank(codec: Option<u32>) -> u8 {
    match codec {
        Some(13) => 3,
        Some(12) => 2,
        Some(7) => 1,
        _ => 0,
    }
}

fn pick_stream<'a>(streams: &'a [BiliStream], quality: &str) -> Result<&'a BiliStream> {
    let wanted = quality.trim().to_ascii_lowercase();
    if wanted.is_empty() || wanted == "best" {
        return streams.first().ok_or_else(no_dash_error);
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
                    "bilibili quality not found: {quality}; available: {}",
                    streams
                        .iter()
                        .map(|stream| format!("{}({})", stream.id, stream.label))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                false,
            )
        })
}

fn select_parts<'a>(
    parts: &'a [BiliPart],
    current_index: usize,
    selection: &PlanSelection,
) -> Result<Vec<&'a BiliPart>> {
    if selection.all_parts {
        return Ok(parts.iter().collect());
    }
    if let Some(page) = selection.part {
        if page == 0 {
            return Err(part_out_of_range(page, parts.len()));
        }
        return parts
            .get(page as usize - 1)
            .map(|part| vec![part])
            .ok_or_else(|| part_out_of_range(page, parts.len()));
    }
    Ok(vec![&parts[current_index.min(parts.len() - 1)]])
}

fn part_out_of_range(page: u32, total: usize) -> AppError {
    AppError::new(
        "selection.part_out_of_range",
        format!("bilibili part out of range: {page} (total {total})"),
        false,
    )
    .with_context("requested", page)
    .with_context("total", total)
}

fn cheese_parts(season: &Item) -> Result<Vec<BiliPart>> {
    let mut episodes = Vec::<(Option<String>, &Item)>::new();
    if let Some(values) = season.get("episodes").and_then(Value::as_array) {
        episodes.extend(
            values
                .iter()
                .filter_map(Value::as_object)
                .map(|ep| (None, ep)),
        );
    }
    if let Some(sections) = season.get("sections").and_then(Value::as_array) {
        for section in sections.iter().filter_map(Value::as_object) {
            let section_title = string_value(section.get("title"));
            if let Some(values) = section.get("episodes").and_then(Value::as_array) {
                episodes.extend(
                    values
                        .iter()
                        .filter_map(Value::as_object)
                        .map(|episode| (section_title.clone(), episode)),
                );
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut parts = Vec::new();
    for (section, episode) in episodes {
        let id = string_id(episode.get("id"))
            .or_else(|| string_id(episode.get("ep_id")))
            .or_else(|| string_id(episode.get("episode_id")));
        let Some(id) = id.filter(|id| seen.insert(id.clone())) else {
            continue;
        };
        let title = string_value(episode.get("title"))
            .or_else(|| string_value(episode.get("long_title")))
            .unwrap_or_else(|| format!("episode_{id}"));
        parts.push(BiliPart {
            index: parts.len(),
            page: parts.len() as u32 + 1,
            title: section
                .filter(|section| !section.is_empty())
                .map_or(title.clone(), |section| format!("{section} -- {title}")),
            cid: 0,
            bvid: None,
            aid: None,
            ep_id: id.parse().ok(),
            _duration: positive_u64(episode.get("duration")),
            subtitles: Vec::new(),
        });
        ensure_part_limit(&parts)?;
    }
    if parts.is_empty() {
        return Err(AppError::new(
            "platform.not_found",
            "bilibili cheese season has no episodes",
            false,
        ));
    }
    Ok(parts)
}

fn parse_bare_identifier(input: &str) -> Option<ParsedInput> {
    if input.to_ascii_lowercase().starts_with("http://")
        || input.to_ascii_lowercase().starts_with("https://")
    {
        return None;
    }
    if let Some(id) = cheese_ep_regex()
        .captures(input)
        .and_then(|capture| capture.get(1))
    {
        return Some(ParsedInput::Cheese {
            id_kind: CheeseIdKind::Episode,
            id: Some(id.as_str().to_owned()),
        });
    }
    if let Some(id) = cheese_ss_regex()
        .captures(input)
        .and_then(|capture| capture.get(1))
    {
        return Some(ParsedInput::Cheese {
            id_kind: CheeseIdKind::Season,
            id: Some(id.as_str().to_owned()),
        });
    }
    if let Some(id) = ep_regex()
        .captures(input)
        .and_then(|capture| capture.get(1))
    {
        return Some(ParsedInput::Bangumi {
            id_kind: BangumiIdKind::Episode,
            id: id.as_str().to_owned(),
        });
    }
    if let Some(id) = ss_regex()
        .captures(input)
        .and_then(|capture| capture.get(1))
    {
        return Some(ParsedInput::Bangumi {
            id_kind: BangumiIdKind::Season,
            id: id.as_str().to_owned(),
        });
    }
    if let Some(id) = md_regex()
        .captures(input)
        .and_then(|capture| capture.get(1))
    {
        return Some(ParsedInput::Bangumi {
            id_kind: BangumiIdKind::Media,
            id: id.as_str().to_owned(),
        });
    }
    if let Some(id) = bv_regex().find(input) {
        return Some(ParsedInput::Video {
            id: id.as_str().to_owned(),
            page: None,
        });
    }
    av_regex()
        .captures(input)
        .and_then(|capture| capture.get(1))
        .map(|id| ParsedInput::Video {
            id: format!("av{}", id.as_str()),
            page: None,
        })
}

fn parse_url_identifier(mut url: Url) -> Option<ParsedInput> {
    let host = url.host_str()?.to_ascii_lowercase();
    if host == "m.bilibili.com"
        && let Some(capture) = mobile_space_regex().captures(url.path())
    {
        let mid = capture.get(1)?.as_str().to_owned();
        let suffix = capture
            .get(2)
            .map_or_else(String::new, |value| value.as_str().to_owned());
        url.set_host(Some("space.bilibili.com")).ok()?;
        url.set_path(&format!("/{mid}{suffix}"));
        return parse_url_identifier(url);
    }
    if let Some(bvid) = url
        .query_pairs()
        .find_map(|(key, value)| (key == "bvid").then(|| value.into_owned()))
        .filter(|value| bv_regex().is_match(value))
        && url.path().contains("/festival/")
    {
        return Some(ParsedInput::Video {
            id: bvid,
            page: None,
        });
    }
    let path_with_query = format!(
        "{}{}",
        url.path(),
        url.query()
            .map_or(String::new(), |query| format!("?{query}"))
    );
    if let Some(id) = cheese_ep_regex()
        .captures(&path_with_query)
        .and_then(|capture| capture.get(1))
    {
        return Some(ParsedInput::Cheese {
            id_kind: CheeseIdKind::Episode,
            id: Some(id.as_str().to_owned()),
        });
    }
    if let Some(id) = cheese_ss_regex()
        .captures(&path_with_query)
        .and_then(|capture| capture.get(1))
    {
        return Some(ParsedInput::Cheese {
            id_kind: CheeseIdKind::Season,
            id: Some(id.as_str().to_owned()),
        });
    }
    if domain_matches(&host, "bilibili.com") && url.path().trim_end_matches('/') == "/cheese" {
        return Some(ParsedInput::Cheese {
            id_kind: CheeseIdKind::Paid,
            id: None,
        });
    }
    if let Some(id) = gongfang_regex()
        .captures(&format!("{host}{}", url.path()))
        .and_then(|capture| capture.get(1))
    {
        return Some(ParsedInput::Gongfang {
            order_id: id.as_str().to_owned(),
        });
    }
    if host == "space.bilibili.com" {
        if let Some(capture) = collection_path_regex().captures(url.path()) {
            let sid = url
                .query_pairs()
                .find_map(|(key, value)| (key == "sid").then(|| value.into_owned()))?;
            return Some(ParsedInput::Collection {
                mid: capture.get(1)?.as_str().to_owned(),
                season_id: sid,
            });
        }
        if let Some(capture) = series_path_regex().captures(url.path()) {
            let sid = url
                .query_pairs()
                .find_map(|(key, value)| (key == "sid").then(|| value.into_owned()))?;
            return Some(ParsedInput::Series {
                mid: capture.get(1)?.as_str().to_owned(),
                series_id: sid,
            });
        }
        if let Some(capture) = lists_path_regex().captures(url.path()) {
            let sid = capture
                .get(2)
                .map(|value| value.as_str().to_owned())
                .or_else(|| {
                    url.query_pairs()
                        .find_map(|(key, value)| (key == "sid").then(|| value.into_owned()))
                })?;
            return Some(ParsedInput::Collection {
                mid: capture.get(1)?.as_str().to_owned(),
                season_id: sid,
            });
        }
    }
    if matches!(host.as_str(), "www.bilibili.com" | "bilibili.com")
        && let Some(capture) = web_list_regex().captures(url.path())
    {
        let up_id = capture.get(1)?.as_str().to_owned();
        if let Some(sid) = url
            .query_pairs()
            .find_map(|(key, value)| (key == "sid").then(|| value.into_owned()))
        {
            return Some(ParsedInput::Collection {
                mid: up_id,
                season_id: sid,
            });
        }
        return Some(ParsedInput::MediaList {
            raw_url: url,
            up_id,
        });
    }
    if let Some(id) = ep_regex()
        .captures(&path_with_query)
        .and_then(|capture| capture.get(1))
    {
        return Some(ParsedInput::Bangumi {
            id_kind: BangumiIdKind::Episode,
            id: id.as_str().to_owned(),
        });
    }
    if let Some(id) = ss_regex()
        .captures(&path_with_query)
        .and_then(|capture| capture.get(1))
    {
        return Some(ParsedInput::Bangumi {
            id_kind: BangumiIdKind::Season,
            id: id.as_str().to_owned(),
        });
    }
    if let Some(id) = md_regex()
        .captures(&path_with_query)
        .and_then(|capture| capture.get(1))
    {
        return Some(ParsedInput::Bangumi {
            id_kind: BangumiIdKind::Media,
            id: id.as_str().to_owned(),
        });
    }
    if let Some(id) = bv_regex().find(&path_with_query) {
        return Some(ParsedInput::Video {
            id: id.as_str().to_owned(),
            page: url
                .query_pairs()
                .find_map(|(key, value)| (key == "p").then(|| value.parse().ok()).flatten()),
        });
    }
    av_regex()
        .captures(&path_with_query)
        .and_then(|capture| capture.get(1))
        .map(|id| ParsedInput::Video {
            id: format!("av{}", id.as_str()),
            page: url
                .query_pairs()
                .find_map(|(key, value)| (key == "p").then(|| value.parse().ok()).flatten()),
        })
}

#[derive(Debug)]
enum ParsedInput {
    Video {
        id: String,
        page: Option<u32>,
    },
    Bangumi {
        id_kind: BangumiIdKind,
        id: String,
    },
    Collection {
        mid: String,
        season_id: String,
    },
    Series {
        mid: String,
        series_id: String,
    },
    MediaList {
        raw_url: Url,
        up_id: String,
    },
    Cheese {
        id_kind: CheeseIdKind,
        id: Option<String>,
    },
    Gongfang {
        order_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BangumiIdKind {
    Episode,
    Season,
    Media,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheeseIdKind {
    Episode,
    Season,
    Paid,
}

enum PlaylistInput {
    Collection { mid: String, season_id: String },
    Series { mid: String, series_id: String },
    MediaList { raw_url: Url, up_id: String },
}

fn finish_bvids(
    title: String,
    bvids: Vec<String>,
    empty_message: &str,
) -> Result<(String, Vec<String>)> {
    let bvids = unique_strings(bvids);
    if bvids.is_empty() {
        Err(AppError::new("platform.not_found", empty_message, false))
    } else {
        Ok((title, bvids))
    }
}

fn ensure_part_limit(parts: &[BiliPart]) -> Result<()> {
    if parts.len() > MAX_PARTS {
        Err(resource_limit_error())
    } else {
        Ok(())
    }
}

fn resource_limit_error() -> AppError {
    AppError::new(
        "platform.resource_limit",
        "bilibili media contains too many parts",
        false,
    )
    .with_context("maximum", MAX_PARTS)
}

fn pgc_result(payload: Value, owner: &str) -> Result<Item> {
    if api_code(&payload) != 0 {
        return Err(api_response_error(&payload, owner));
    }
    payload
        .get("result")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| {
            AppError::new(
                "platform.invalid_response",
                format!("{owner} response is missing result"),
                false,
            )
        })
}

fn ensure_api_code(payload: &Value, owner: &str) -> Result<()> {
    if api_code(payload) == 0 {
        Ok(())
    } else {
        Err(api_response_error(payload, owner))
    }
}

fn api_code(payload: &Value) -> i64 {
    payload
        .get("code")
        .map_or(-1, |value| flexible_i64(Some(value)))
}

fn api_response_error(payload: &Value, owner: &str) -> AppError {
    let code = api_code(payload);
    let message = string_value(payload.get("message")).unwrap_or_default();
    let error_code = if matches!(code, -101 | -10403 | -403) {
        "auth.required"
    } else {
        "platform.remote_error"
    };
    AppError::new(
        error_code,
        format!("{owner} API error {code}: {message}")
            .trim()
            .to_owned(),
        false,
    )
    .with_context("remoteCode", code)
}

fn no_dash_error() -> AppError {
    AppError::new(
        "platform.no_streams",
        "bilibili has no downloadable DASH stream",
        false,
    )
}

fn endpoint_error() -> AppError {
    AppError::new(
        "platform.configuration",
        "bilibili endpoint must be an HTTP URL with a host",
        false,
    )
}

fn parse_endpoint(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|error| {
        AppError::new(
            "platform.configuration",
            format!("invalid bilibili endpoint URL: {error}"),
            false,
        )
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(endpoint_error());
    }
    Ok(url)
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

fn response_too_large(limit: usize) -> AppError {
    AppError::new(
        "platform.response_too_large",
        "platform response exceeded the configured size limit",
        false,
    )
    .with_context("maximum", limit)
}

fn page_headers(cookie: Option<&str>) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        USER_AGENT.parse().expect("valid user agent"),
    );
    headers.insert(
        reqwest::header::REFERER,
        REFERER.parse().expect("valid referer"),
    );
    headers.insert(
        reqwest::header::ACCEPT,
        "application/json, text/plain, */*"
            .parse()
            .expect("valid accept"),
    );
    if let Some(cookie) = cookie
        && let Ok(cookie) = cookie.parse()
    {
        headers.insert(reqwest::header::COOKIE, cookie);
    }
    headers
}

fn download_sources(urls: &[String]) -> Vec<DownloadSource> {
    let headers = BTreeMap::from([
        ("Referer".to_owned(), REFERER.to_owned()),
        ("User-Agent".to_owned(), USER_AGENT.to_owned()),
    ]);
    urls.iter()
        .map(|url| DownloadSource {
            url: url.clone(),
            headers: headers.clone(),
            auth_ref: None,
        })
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

fn string_id(value: Option<&Value>) -> Option<String> {
    string_value(value)
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

fn unique_strings(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty() && seen.insert(value.clone()))
        .collect()
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

fn strip_extension(name: &str, extension: &str) -> String {
    let normalized = format!(".{}", extension.trim().trim_start_matches('.'));
    if name
        .to_ascii_lowercase()
        .ends_with(&normalized.to_ascii_lowercase())
    {
        name[..name.len() - normalized.len()].trim().to_owned()
    } else {
        name.trim().to_owned()
    }
}

fn extension_from_content_type(content_type: &str, url: &str) -> String {
    let normalized = content_type
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if !normalized.is_empty()
        && normalized.len() <= 8
        && normalized.chars().all(|ch| ch.is_ascii_alphanumeric())
    {
        return normalized;
    }
    Url::parse(url)
        .ok()
        .and_then(|url| {
            std::path::Path::new(url.path())
                .extension()
                .and_then(|extension| extension.to_str())
                .filter(|extension| !extension.is_empty() && extension.len() <= 8)
                .map(str::to_ascii_lowercase)
        })
        .unwrap_or_else(|| "bin".to_owned())
}

fn strip_output_extension(path: &str) -> String {
    path.rsplit_once('.')
        .filter(|(stem, extension)| !stem.is_empty() && !extension.contains('/'))
        .map_or_else(|| path.to_owned(), |(stem, _)| stem.to_owned())
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

fn host_regex() -> &'static Regex {
    static HOST: OnceLock<Regex> = OnceLock::new();
    HOST.get_or_init(|| {
        Regex::new(
            r"(?i)(?:^|//)(?:[^/]+\.)?(?:bilibili\.com|b23\.tv)(?:[/:?#]|$)|\b(?:BV[0-9A-Za-z]+|av\d+|ep\d+|ss\d+|md\d+)\b",
        )
        .expect("valid bilibili matcher regex")
    })
}

fn url_regex() -> &'static Regex {
    static URL: OnceLock<Regex> = OnceLock::new();
    URL.get_or_init(|| Regex::new(r#"https?://[^\s<>'"`，。；、）)】]+"#).expect("valid URL regex"))
}

macro_rules! regex_fn {
    ($name:ident, $pattern:literal) => {
        fn $name() -> &'static Regex {
            static VALUE: OnceLock<Regex> = OnceLock::new();
            VALUE.get_or_init(|| Regex::new($pattern).expect(concat!("valid ", stringify!($name))))
        }
    };
}

regex_fn!(bv_regex, r"(?i)\bBV[0-9A-Za-z]+\b");
regex_fn!(av_regex, r"(?i)\bav(\d+)\b");
regex_fn!(ep_regex, r"(?i)(?:^|/bangumi/play/|\b)ep(\d+)\b");
regex_fn!(ss_regex, r"(?i)(?:^|/bangumi/play/|\b)ss(\d+)\b");
regex_fn!(md_regex, r"(?i)(?:^|/bangumi/media/|\b)md(\d+)\b");
regex_fn!(cheese_ep_regex, r"(?i)(?:^|/cheese/play/)ep(\d+)\b");
regex_fn!(cheese_ss_regex, r"(?i)(?:^|/cheese/play/)ss(\d+)\b");
regex_fn!(
    gongfang_regex,
    r"(?i)gf\.bilibili\.com/order/hyg-download/(\d+)"
);
regex_fn!(
    collection_path_regex,
    r"(?i)^/(\d+)/channel/collectiondetail/?$"
);
regex_fn!(series_path_regex, r"(?i)^/(\d+)/channel/seriesdetail/?$");
regex_fn!(lists_path_regex, r"(?i)^/(\d+)/lists(?:/(\d+))?/?$");
regex_fn!(web_list_regex, r"(?i)^/list/([A-Za-z0-9_]+)/?$");
regex_fn!(mobile_space_regex, r"(?i)^/space/(\d+)(/.*)?$");
regex_fn!(
    media_list_redirect_regex,
    r"(?i)space\.bilibili\.com/(\d+)/channel/(collection|series)detail\?sid=(\d+)"
);
regex_fn!(items_name_regex, r#""itemsName"\s*:\s*"((?:\\.|[^"\\])*)""#);

type Item = Map<String, Value>;
