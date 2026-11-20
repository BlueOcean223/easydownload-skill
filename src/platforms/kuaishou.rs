use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

use regex::Regex;
use reqwest::{header::LOCATION, redirect::Policy};
use serde::Deserialize;
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

const DEFAULT_API_URL: &str = "https://m.gifshow.com/rest/wd/ugH5App/photo/simple/info";
const MOBILE_USER_AGENT: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 16_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.0 Mobile/15E148 Safari/604.1";
const REFERER: &str = "https://v.chenzhongtech.com/";

pub fn matches(input: &str) -> bool {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN
        .get_or_init(|| {
            Regex::new(r"(?i)(?:^|//)(?:[^/]+\.)?(?:kuaishou\.(?:com|cn)|kwai\.com|chenzhongtech\.(?:com|cn)|gifshow\.com)(?:[/:?#]|$)")
                .expect("valid kuaishou host regex")
        })
        .is_match(input)
}

pub fn looks_like_bare_photo_id(value: &str) -> bool {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN
        .get_or_init(|| Regex::new(r"^[A-Za-z0-9_]{8,32}$").expect("valid photo id regex"))
        .is_match(value)
        && !value.contains('.')
}

#[derive(Clone)]
pub struct KuaishouResolver {
    client: reqwest::Client,
    api_url: Url,
    credentials: CredentialStore,
}

impl KuaishouResolver {
    pub fn new(options: ResolveOptions) -> Result<Self> {
        Self::with_api_url(options, DEFAULT_API_URL)
    }

    pub fn with_api_url(options: ResolveOptions, api_url: &str) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(options.timeout)
            .redirect(Policy::none());
        if let Some(proxy) = options
            .proxy
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            let configured = reqwest::Proxy::all(proxy)
                .map_err(|_| AppError::new("proxy.invalid", "invalid proxy URL", false))?;
            builder = builder.proxy(configured);
        }
        let client = builder
            .build()
            .map_err(|error| AppError::http_request(&error))?;
        let api_url = Url::parse(api_url).map_err(|error| {
            AppError::new(
                "platform.configuration",
                format!("invalid kuaishou API URL: {error}"),
                false,
            )
        })?;
        let credentials = CredentialStore::new(options.config_dir, options.cookies_path);
        Ok(Self {
            client,
            api_url,
            credentials,
        })
    }

    pub async fn resolve(&self, input: &str) -> Result<KuaishouMedia> {
        let photo_id = self.resolve_photo_id(input).await?;
        let response = self.fetch_photo_info(&photo_id).await?;
        KuaishouMedia::from_response(response, &photo_id)
    }

    async fn resolve_photo_id(&self, input: &str) -> Result<String> {
        if let Some(photo_id) = extract_photo_id(input) {
            return Ok(photo_id);
        }
        let trimmed = input.trim();
        if looks_like_bare_photo_id(trimmed) {
            return Ok(trimmed.to_owned());
        }
        let mut current = extract_first_url(input).ok_or_else(|| {
            AppError::new(
                "input.url_not_found",
                "no kuaishou URL found in input",
                false,
            )
        })?;

        for _ in 0..6 {
            let mut request = self
                .client
                .get(current.clone())
                .header("User-Agent", MOBILE_USER_AGENT)
                .header("Referer", REFERER);
            if let Some(cookie) = self
                .credentials
                .explicit_cookie_header_for(&current)
                .await?
            {
                request = request.header("Cookie", cookie);
            }
            let response = request
                .send()
                .await
                .map_err(|error| AppError::http_request(&error))?;
            if let Some(photo_id) = extract_photo_id(response.url().as_str()) {
                return Ok(photo_id);
            }
            if response.status().is_redirection()
                && let Some(location) = response.headers().get(LOCATION)
            {
                let location = location.to_str().map_err(|_| {
                    AppError::new(
                        "platform.invalid_response",
                        "kuaishou redirect contains an invalid Location header",
                        false,
                    )
                })?;
                current = response.url().join(location).map_err(|error| {
                    AppError::new(
                        "platform.invalid_response",
                        format!("invalid kuaishou redirect: {error}"),
                        false,
                    )
                })?;
                if let Some(photo_id) = extract_photo_id(current.as_str()) {
                    return Ok(photo_id);
                }
                continue;
            }
            if !response.status().is_success() {
                return Err(AppError::http_status(response.status()));
            }
            break;
        }

        extract_photo_id(current.as_str()).ok_or_else(|| {
            AppError::new(
                "platform.identifier_not_found",
                "kuaishou photoId not found",
                false,
            )
        })
    }

    async fn fetch_photo_info(&self, photo_id: &str) -> Result<KuaishouApiResponse> {
        let mut request = self
            .client
            .post(self.api_url.clone())
            .header("User-Agent", MOBILE_USER_AGENT)
            .header("Referer", REFERER)
            .header("Content-Type", "application/json");
        if let Some(cookie) = self
            .credentials
            .explicit_cookie_header_for(&self.api_url)
            .await?
        {
            request = request.header("Cookie", cookie);
        }
        let response = request
            .json(&serde_json::json!({
                "photoId": photo_id,
                "subBiz": "BROWSE_SLIDE_PHOTO",
                "kpn": "KUAISHOU"
            }))
            .send()
            .await
            .map_err(|error| AppError::http_request(&error))?;
        if !response.status().is_success() {
            return Err(AppError::http_status(response.status()));
        }
        let data: KuaishouApiResponse = response.json().await.map_err(|error| {
            AppError::new(
                "platform.invalid_response",
                format!("invalid kuaishou API JSON: {error}"),
                false,
            )
        })?;
        if data.result.is_some_and(|result| result != 1) {
            return Err(AppError::new(
                "platform.api_error",
                data.message.as_deref().map_or_else(
                    || "kuaishou API returned an error".to_owned(),
                    str::to_owned,
                ),
                false,
            )
            .with_context("result", data.result.unwrap_or_default()));
        }
        if data.photo.is_none() {
            return Err(AppError::new(
                "platform.invalid_response",
                "kuaishou API response is missing photo",
                false,
            ));
        }
        Ok(data)
    }
}

#[derive(Debug, Clone)]
pub struct KuaishouMedia {
    id: String,
    title: String,
    author: Option<String>,
    cover: Option<String>,
    kind: KuaishouMediaKind,
}

#[derive(Debug, Clone)]
enum KuaishouMediaKind {
    Video {
        streams: Vec<KuaishouStream>,
    },
    SinglePicture {
        image: KuaishouImage,
    },
    Atlas {
        images: Vec<KuaishouImage>,
        music_url: Option<String>,
    },
}

#[derive(Debug, Clone)]
struct KuaishouImage {
    index: usize,
    url: String,
    width: Option<u32>,
    height: Option<u32>,
}

impl KuaishouMedia {
    fn from_response(response: KuaishouApiResponse, fallback_id: &str) -> Result<Self> {
        let KuaishouApiResponse { photo, atlas, .. } = response;
        let photo = photo.ok_or_else(|| {
            AppError::new(
                "platform.invalid_response",
                "kuaishou API response is missing photo",
                false,
            )
        })?;
        let id = photo
            .photo_id
            .clone()
            .unwrap_or_else(|| fallback_id.to_owned());
        let title = photo
            .caption
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| id.clone());
        let cover = first_url(photo.cover_urls.as_deref());
        let photo_type = photo.photo_type.as_deref().unwrap_or("unknown").to_owned();
        let kind = match photo_type.as_str() {
            "VIDEO" | "PAY_COURSE_VIDEO" | "SPHERICAL_VIDEO" => {
                let mut streams = build_video_streams(&photo)?;
                streams.sort_by(|a, b| {
                    b.height
                        .unwrap_or_default()
                        .cmp(&a.height.unwrap_or_default())
                        .then_with(|| {
                            b.width
                                .unwrap_or_default()
                                .cmp(&a.width.unwrap_or_default())
                        })
                        .then_with(|| b.size.unwrap_or_default().cmp(&a.size.unwrap_or_default()))
                });
                KuaishouMediaKind::Video { streams }
            }
            "SINGLE_PICTURE" => {
                let url = cover.clone().ok_or_else(|| {
                    AppError::new(
                        "platform.no_images",
                        "kuaishou single picture has no cover URL",
                        false,
                    )
                })?;
                KuaishouMediaKind::SinglePicture {
                    image: KuaishouImage {
                        index: 0,
                        url,
                        width: None,
                        height: None,
                    },
                }
            }
            "HORIZONTAL_ATLAS" | "VERTICAL_ATLAS" => KuaishouMediaKind::Atlas {
                images: build_atlas_images(atlas.as_ref())?,
                music_url: build_music_url(atlas.as_ref())?,
            },
            _ => {
                return Err(AppError::new(
                    "platform.unsupported_media",
                    format!("unsupported kuaishou media type: {photo_type}"),
                    false,
                )
                .with_context("photoType", photo_type));
            }
        };

        Ok(Self {
            id,
            title,
            author: photo.user_name,
            cover,
            kind,
        })
    }

    pub fn inspect(&self) -> InspectDocument {
        let (media_kind, qualities, images) = match &self.kind {
            KuaishouMediaKind::Video { streams } => (
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
            KuaishouMediaKind::SinglePicture { image } => {
                (MediaKind::Album, Vec::new(), vec![image_choice(image)])
            }
            KuaishouMediaKind::Atlas { images, .. } => (
                MediaKind::Album,
                Vec::new(),
                images.iter().map(image_choice).collect(),
            ),
        };
        InspectDocument {
            schema_version: SCHEMA_VERSION,
            platform: "kuaishou".to_owned(),
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

    pub fn create_plan(&self, selection: &PlanSelection) -> Result<DownloadPlan> {
        let tasks = match &self.kind {
            KuaishouMediaKind::Video { streams } => {
                vec![self.create_video_task(streams, selection)?]
            }
            KuaishouMediaKind::SinglePicture { image } => {
                let selected =
                    select_images(std::slice::from_ref(image), selection.images.as_deref())?;
                let image = selected[0];
                let relative_path = selection.output.clone().unwrap_or_else(|| {
                    format!(
                        "{}.{}",
                        safe_file_stem(&self.title, &self.id),
                        extension_from_url(&image.url, "jpg")
                    )
                });
                vec![file_task(self.id.clone(), &image.url, relative_path)]
            }
            KuaishouMediaKind::Atlas { images, music_url } => {
                self.create_atlas_tasks(images, music_url.as_deref(), selection)?
            }
        };
        let plan = DownloadPlan {
            schema_version: SCHEMA_VERSION,
            created_at: OffsetDateTime::now_utc(),
            expires_at: None,
            source: PlanSource {
                platform: "kuaishou".to_owned(),
                media_id: self.id.clone(),
            },
            tasks,
        };
        plan.validate()?;
        Ok(plan)
    }

    fn create_video_task(
        &self,
        streams: &[KuaishouStream],
        selection: &PlanSelection,
    ) -> Result<DownloadTask> {
        let stream = pick_stream(streams, selection.quality.as_deref().unwrap_or("best"))?;
        let relative_path = selection.output.clone().unwrap_or_else(|| {
            format!(
                "{}.{}",
                safe_file_stem(&self.title, &self.id),
                extension_from_url(&stream.url, "mp4")
            )
        });
        Ok(DownloadTask::File {
            id: self.id.clone(),
            sources: download_sources(&stream.url, &stream.backup_urls),
            output: OutputSpec { relative_path },
        })
    }

    fn create_atlas_tasks(
        &self,
        images: &[KuaishouImage],
        music_url: Option<&str>,
        selection: &PlanSelection,
    ) -> Result<Vec<DownloadTask>> {
        let selected = select_images(images, selection.images.as_deref())?;
        match selection.album_output {
            AlbumOutput::Zip => {
                let mut entries = selected
                    .iter()
                    .map(|image| ArchiveEntry {
                        id: format!("image-{}", image.index + 1),
                        path: image_filename(image),
                        sources: download_sources(&image.url, &[]),
                    })
                    .collect::<Vec<_>>();
                if let Some(url) = music_url {
                    entries.push(ArchiveEntry {
                        id: "music".to_owned(),
                        path: format!("music.{}", extension_from_url(url, "m4a")),
                        sources: download_sources(url, &[]),
                    });
                }
                let relative_path = selection
                    .output
                    .clone()
                    .unwrap_or_else(|| format!("{}.zip", safe_file_stem(&self.title, &self.id)));
                Ok(vec![DownloadTask::Archive {
                    id: self.id.clone(),
                    entries,
                    output: OutputSpec { relative_path },
                }])
            }
            AlbumOutput::Directory => {
                let directory = selection
                    .output
                    .clone()
                    .unwrap_or_else(|| safe_file_stem(&self.title, &self.id));
                let mut tasks = selected
                    .iter()
                    .map(|image| {
                        file_task(
                            format!("{}:image-{}", self.id, image.index + 1),
                            &image.url,
                            format!("{directory}/{}", image_filename(image)),
                        )
                    })
                    .collect::<Vec<_>>();
                if let Some(url) = music_url {
                    tasks.push(file_task(
                        format!("{}:music", self.id),
                        url,
                        format!("{directory}/music.{}", extension_from_url(url, "m4a")),
                    ));
                }
                Ok(tasks)
            }
        }
    }
}

#[derive(Debug, Clone)]
struct KuaishouStream {
    id: String,
    quality: String,
    label: String,
    width: Option<u32>,
    height: Option<u32>,
    size: Option<u64>,
    url: String,
    backup_urls: Vec<String>,
}

fn pick_stream<'a>(streams: &'a [KuaishouStream], quality: &str) -> Result<&'a KuaishouStream> {
    let wanted = quality.trim().to_ascii_lowercase();
    if wanted.is_empty() || wanted == "best" {
        return streams.first().ok_or_else(no_stream_error);
    }
    streams
        .iter()
        .find(|stream| stream.id.eq_ignore_ascii_case(&wanted))
        .or_else(|| {
            streams
                .iter()
                .find(|stream| stream.quality.eq_ignore_ascii_case(&wanted))
        })
        .or_else(|| {
            streams
                .iter()
                .find(|stream| stream.label.to_ascii_lowercase().contains(&wanted))
        })
        .ok_or_else(|| {
            AppError::new(
                "selection.quality_not_found",
                format!(
                    "kuaishou quality not found: {quality}; available: {}",
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

fn image_choice(image: &KuaishouImage) -> ImageChoice {
    ImageChoice {
        id: format!("image-{}", image.index + 1),
        index: (image.index + 1) as u32,
        width: image.width,
        height: image.height,
    }
}

fn select_images<'a>(
    images: &'a [KuaishouImage],
    selection: Option<&[u32]>,
) -> Result<Vec<&'a KuaishouImage>> {
    let Some(selection) = selection.filter(|selection| !selection.is_empty()) else {
        return Ok(images.iter().collect());
    };
    let mut indexes = BTreeSet::new();
    for index in selection {
        indexes.insert(*index as usize);
    }
    indexes
        .into_iter()
        .map(|index| {
            images.get(index).ok_or_else(|| {
                AppError::new(
                    "selection.image_out_of_range",
                    format!(
                        "kuaishou image index out of range: {} (total {})",
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

fn image_filename(image: &KuaishouImage) -> String {
    format!(
        "{:02}.{}",
        image.index + 1,
        extension_from_url(&image.url, "jpg")
    )
}

fn file_task(id: String, url: &str, relative_path: String) -> DownloadTask {
    DownloadTask::File {
        id,
        sources: download_sources(url, &[]),
        output: OutputSpec { relative_path },
    }
}

fn download_sources(primary: &str, backups: &[String]) -> Vec<DownloadSource> {
    let headers = download_headers();
    std::iter::once(primary)
        .chain(backups.iter().map(String::as_str))
        .map(|url| DownloadSource {
            url: url.to_owned(),
            headers: headers.clone(),
            auth_ref: None,
        })
        .collect()
}

fn build_video_streams(photo: &KuaishouPhoto) -> Result<Vec<KuaishouStream>> {
    let mut streams = Vec::new();
    for set in photo
        .manifest
        .as_ref()
        .and_then(|manifest| manifest.adaptation_set.as_ref())
        .into_iter()
        .flatten()
    {
        for representation in set.representation.iter().flatten() {
            let Some(url) = representation.url.as_ref() else {
                continue;
            };
            let quality = representation
                .quality_type
                .clone()
                .unwrap_or_else(|| format!("{}p", representation.height.unwrap_or_default()));
            let id = [
                Some(quality.as_str()),
                representation.video_codec.as_deref(),
            ]
            .into_iter()
            .flatten()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("_");
            let id = if id.is_empty() {
                format!("stream_{}", streams.len() + 1)
            } else {
                id
            };
            let label = [
                representation.quality_type.as_deref(),
                representation.video_codec.as_deref(),
            ]
            .into_iter()
            .flatten()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
            streams.push(KuaishouStream {
                id: id.clone(),
                quality,
                label: if label.is_empty() { id } else { label },
                width: representation.width,
                height: representation.height,
                size: representation.file_size,
                url: url.clone(),
                backup_urls: representation.backup_url.clone().unwrap_or_default(),
            });
        }
    }

    if streams.is_empty() {
        for (index, item) in photo.main_mv_urls.iter().flatten().enumerate() {
            let Some(url) = item.url.as_ref() else {
                continue;
            };
            let id = if index == 0 {
                "main".to_owned()
            } else {
                format!("main_{}", index + 1)
            };
            streams.push(KuaishouStream {
                id: id.clone(),
                quality: "main".to_owned(),
                label: item.cdn.as_deref().map_or_else(
                    || "mainMvUrls".to_owned(),
                    |cdn| format!("mainMvUrls {cdn}"),
                ),
                width: None,
                height: None,
                size: None,
                url: url.clone(),
                backup_urls: Vec::new(),
            });
        }
    }
    if streams.is_empty() {
        return Err(no_stream_error());
    }
    Ok(streams)
}

fn build_atlas_images(atlas: Option<&KuaishouAtlas>) -> Result<Vec<KuaishouImage>> {
    const MAX_ATLAS_IMAGES: usize = 10_000;
    let atlas = atlas.ok_or_else(|| {
        AppError::new(
            "platform.no_images",
            "kuaishou atlas payload is missing",
            false,
        )
    })?;
    let paths = atlas
        .list
        .as_deref()
        .filter(|paths| !paths.is_empty())
        .ok_or_else(|| {
            AppError::new("platform.no_images", "kuaishou atlas has no images", false)
        })?;
    if paths.len() > MAX_ATLAS_IMAGES {
        return Err(AppError::new(
            "platform.resource_limit",
            "kuaishou atlas contains too many images",
            false,
        )
        .with_context("images", paths.len())
        .with_context("maximum", MAX_ATLAS_IMAGES));
    }
    let cdn = atlas
        .cdn_list
        .iter()
        .flatten()
        .find_map(|item| item.cdn.as_deref())
        .filter(|cdn| !cdn.trim().is_empty())
        .ok_or_else(|| {
            AppError::new(
                "platform.no_images",
                "kuaishou atlas has no image CDN",
                false,
            )
        })?;

    paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let size = atlas.size.as_deref().and_then(|sizes| sizes.get(index));
            Ok(KuaishouImage {
                index,
                url: absolute_cdn_url(cdn, path)?,
                width: size.and_then(|size| size.w),
                height: size.and_then(|size| size.h),
            })
        })
        .collect()
}

fn build_music_url(atlas: Option<&KuaishouAtlas>) -> Result<Option<String>> {
    let Some(atlas) = atlas else {
        return Ok(None);
    };
    let Some(path) = atlas
        .music
        .as_deref()
        .filter(|path| !path.trim().is_empty())
    else {
        return Ok(None);
    };
    let Some(cdn) = atlas
        .music_cdn_list
        .iter()
        .flatten()
        .find_map(|item| item.cdn.as_deref())
        .filter(|cdn| !cdn.trim().is_empty())
    else {
        return Ok(None);
    };
    Ok(Some(absolute_cdn_url(cdn, path)?))
}

fn absolute_cdn_url(cdn: &str, path: &str) -> Result<String> {
    if let Ok(url) = Url::parse(path)
        && matches!(url.scheme(), "http" | "https")
    {
        return Ok(url.into());
    }
    let base = if cdn.starts_with("http://") || cdn.starts_with("https://") {
        cdn.to_owned()
    } else {
        format!("https://{cdn}")
    };
    let base = Url::parse(&base).map_err(|_| {
        AppError::new(
            "platform.invalid_response",
            "kuaishou atlas contains an invalid CDN",
            false,
        )
    })?;
    base.join(path).map(Into::into).map_err(|_| {
        AppError::new(
            "platform.invalid_response",
            "kuaishou atlas contains an invalid media path",
            false,
        )
    })
}

fn first_url(values: Option<&[KuaishouUrl]>) -> Option<String> {
    values?.iter().find_map(|item| item.url.clone())
}

fn no_stream_error() -> AppError {
    AppError::new(
        "platform.no_streams",
        "kuaishou video has no downloadable stream",
        false,
    )
}

fn extract_photo_id(input: &str) -> Option<String> {
    static DIRECT: OnceLock<Regex> = OnceLock::new();
    let direct = DIRECT.get_or_init(|| {
        Regex::new(r"(?:/fw/photo/|/short-video/)([A-Za-z0-9_]+)")
            .expect("valid direct photo id regex")
    });
    if let Some(value) = direct
        .captures(input)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_owned())
    {
        return Some(value);
    }
    if let Ok(url) = Url::parse(input)
        && let Some(value) = url.query_pairs().find_map(|(key, value)| {
            (key == "photoId" && looks_like_bare_photo_id(&value)).then(|| value.into_owned())
        })
    {
        return Some(value);
    }
    static QUERY: OnceLock<Regex> = OnceLock::new();
    QUERY
        .get_or_init(|| {
            Regex::new(r"[?&]photoId=([A-Za-z0-9_]+)").expect("valid query photo id regex")
        })
        .captures(input)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_owned())
}

fn extract_first_url(input: &str) -> Option<Url> {
    static URL_PATTERN: OnceLock<Regex> = OnceLock::new();
    let raw = URL_PATTERN
        .get_or_init(|| {
            Regex::new(r#"https?://[^\s<>'"`，。；、）)】]+"#).expect("valid URL regex")
        })
        .find(input)?
        .as_str()
        .trim_end_matches([
            '.', ',', '!', '?', ';', ':', '，', '。', '！', '？', '；', '：',
        ]);
    Url::parse(raw).ok()
}

fn download_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("Referer".to_owned(), REFERER.to_owned()),
        ("User-Agent".to_owned(), MOBILE_USER_AGENT.to_owned()),
    ])
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
        if output.chars().count() >= 80 {
            break;
        }
    }
    let output = output.trim().trim_end_matches([' ', '.']).to_owned();
    let output = if output.is_empty() {
        fallback.to_owned()
    } else {
        output
    };
    if is_windows_reserved_stem(&output) {
        format!("{output}_")
    } else {
        output
    }
}

fn is_windows_reserved_stem(value: &str) -> bool {
    let upper = value.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit()
            && upper.as_bytes()[3] != b'0')
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KuaishouApiResponse {
    result: Option<i64>,
    message: Option<String>,
    photo: Option<KuaishouPhoto>,
    atlas: Option<KuaishouAtlas>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KuaishouPhoto {
    photo_id: Option<String>,
    photo_type: Option<String>,
    caption: Option<String>,
    user_name: Option<String>,
    #[serde(default)]
    cover_urls: Option<Vec<KuaishouUrl>>,
    #[serde(default)]
    main_mv_urls: Option<Vec<KuaishouUrl>>,
    manifest: Option<KuaishouManifest>,
}

#[derive(Debug, Clone, Deserialize)]
struct KuaishouManifest {
    #[serde(rename = "adaptationSet")]
    adaptation_set: Option<Vec<KuaishouAdaptationSet>>,
}

#[derive(Debug, Clone, Deserialize)]
struct KuaishouAdaptationSet {
    representation: Option<Vec<KuaishouRepresentation>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KuaishouRepresentation {
    quality_type: Option<String>,
    video_codec: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    file_size: Option<u64>,
    url: Option<String>,
    backup_url: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KuaishouAtlas {
    cdn_list: Option<Vec<KuaishouUrl>>,
    list: Option<Vec<String>>,
    size: Option<Vec<KuaishouImageSize>>,
    music: Option<String>,
    music_cdn_list: Option<Vec<KuaishouUrl>>,
}

#[derive(Debug, Clone, Deserialize)]
struct KuaishouImageSize {
    w: Option<u32>,
    h: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
struct KuaishouUrl {
    cdn: Option<String>,
    url: Option<String>,
}
