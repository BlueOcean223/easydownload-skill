use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
};

use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::{
    auth,
    errors::{AppError, ErrorInfo, Result},
};

pub const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InspectDocument {
    pub schema_version: u32,
    pub platform: String,
    pub media: MediaView,
    pub choices: MediaChoices,
    pub requirements: MediaRequirements,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MediaView {
    pub id: String,
    pub kind: MediaKind,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Video,
    Album,
    Playlist,
    Bangumi,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MediaChoices {
    #[serde(default)]
    pub qualities: Vec<QualityChoice>,
    #[serde(default)]
    pub parts: Vec<PartChoice>,
    #[serde(default)]
    pub images: Vec<ImageChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QualityChoice {
    pub id: String,
    pub label: String,
    pub recommended: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PartChoice {
    pub id: String,
    pub index: u32,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImageChoice {
    pub id: String,
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MediaRequirements {
    pub login: bool,
    pub ffmpeg: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DownloadPlan {
    pub schema_version: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub expires_at: Option<OffsetDateTime>,
    pub source: PlanSource,
    pub tasks: Vec<DownloadTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlanSource {
    pub platform: String,
    pub media_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DownloadTask {
    File {
        id: String,
        sources: Vec<DownloadSource>,
        output: OutputSpec,
    },
    Archive {
        id: String,
        entries: Vec<ArchiveEntry>,
        output: OutputSpec,
    },
    Mux {
        id: String,
        tracks: Vec<MuxTrack>,
        format: MuxFormat,
        output: OutputSpec,
    },
}

impl DownloadTask {
    pub fn id(&self) -> &str {
        match self {
            Self::File { id, .. } | Self::Archive { id, .. } | Self::Mux { id, .. } => id,
        }
    }

    pub fn output(&self) -> &OutputSpec {
        match self {
            Self::File { output, .. } | Self::Archive { output, .. } | Self::Mux { output, .. } => {
                output
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveEntry {
    pub id: String,
    pub path: String,
    pub sources: Vec<DownloadSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MuxTrack {
    pub id: String,
    pub kind: MuxTrackKind,
    pub sources: Vec<DownloadSource>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MuxTrackKind {
    Video,
    Audio,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MuxFormat {
    Mp4,
    Matroska,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DownloadSource {
    pub url: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OutputSpec {
    pub relative_path: String,
}

impl DownloadPlan {
    pub fn validate(&self) -> Result<()> {
        self.validate_at(OffsetDateTime::now_utc())
    }

    pub fn validate_at(&self, now: OffsetDateTime) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(AppError::new(
                "plan.unsupported_version",
                format!(
                    "unsupported plan schemaVersion {}; expected {}",
                    self.schema_version, SCHEMA_VERSION
                ),
                false,
            )
            .with_context("actual", self.schema_version)
            .with_context("expected", SCHEMA_VERSION));
        }
        if let Some(expires_at) = self.expires_at
            && expires_at <= now
        {
            return Err(AppError::new(
                "plan.expired",
                "download plan has expired; generate a fresh plan",
                false,
            )
            .with_context("expiresAt", expires_at.to_string()));
        }
        if self.source.platform.trim().is_empty() || self.source.media_id.trim().is_empty() {
            return Err(AppError::invalid_plan(
                "plan source platform and mediaId must be non-empty",
            ));
        }
        if self.tasks.is_empty() {
            return Err(AppError::invalid_plan(
                "plan must contain at least one task",
            ));
        }

        let mut ids = HashSet::new();
        let mut output_paths = Vec::<String>::new();
        for task in &self.tasks {
            let id = task.id().trim();
            if id.is_empty() {
                return Err(AppError::invalid_plan("task id must be non-empty"));
            }
            if !ids.insert(id.to_owned()) {
                return Err(AppError::invalid_plan(format!("duplicate task id: {id}")));
            }
            let output_path = &task.output().relative_path;
            validate_relative_path(output_path)?;
            if let Some(existing) = output_paths
                .iter()
                .find(|existing| output_paths_conflict(existing, output_path))
            {
                return Err(AppError::invalid_plan("task output paths must not overlap")
                    .with_context("firstPath", existing.clone())
                    .with_context("secondPath", output_path.clone()));
            }
            output_paths.push(output_path.clone());

            match task {
                DownloadTask::File { sources, .. } => {
                    validate_sources(sources, format!("file task {id}"))?;
                }
                DownloadTask::Archive { entries, .. } => {
                    if entries.is_empty() {
                        return Err(AppError::invalid_plan(format!(
                            "archive task {id} must contain at least one entry"
                        )));
                    }
                    let mut entry_ids = HashSet::new();
                    let mut entry_paths = Vec::<String>::new();
                    for entry in entries {
                        let entry_id = entry.id.trim();
                        if entry_id.is_empty() {
                            return Err(AppError::invalid_plan(format!(
                                "archive task {id} contains an entry with an empty id"
                            )));
                        }
                        if !entry_ids.insert(entry_id.to_owned()) {
                            return Err(AppError::invalid_plan(format!(
                                "duplicate archive entry id in task {id}: {entry_id}"
                            )));
                        }
                        validate_relative_path(&entry.path)?;
                        if let Some(existing) = entry_paths
                            .iter()
                            .find(|existing| output_paths_conflict(existing, &entry.path))
                        {
                            return Err(AppError::invalid_plan(
                                "archive entry paths must not overlap",
                            )
                            .with_context("taskId", id)
                            .with_context("firstPath", existing.clone())
                            .with_context("secondPath", entry.path.clone()));
                        }
                        entry_paths.push(entry.path.clone());
                        validate_sources(&entry.sources, format!("archive entry {id}:{entry_id}"))?;
                    }
                }
                DownloadTask::Mux { tracks, .. } => {
                    if tracks.len() < 2 {
                        return Err(AppError::invalid_plan(format!(
                            "mux task {id} must contain at least two tracks"
                        )));
                    }
                    let mut track_ids = HashSet::new();
                    let mut has_video = false;
                    let mut has_audio = false;
                    for track in tracks {
                        let track_id = track.id.trim();
                        if track_id.is_empty() {
                            return Err(AppError::invalid_plan(format!(
                                "mux task {id} contains a track with an empty id"
                            )));
                        }
                        if !track_ids.insert(track_id.to_owned()) {
                            return Err(AppError::invalid_plan(format!(
                                "duplicate mux track id in task {id}: {track_id}"
                            )));
                        }
                        match track.kind {
                            MuxTrackKind::Video => has_video = true,
                            MuxTrackKind::Audio => has_audio = true,
                        }
                        validate_sources(&track.sources, format!("mux track {id}:{track_id}"))?;
                    }
                    if !has_video || !has_audio {
                        return Err(AppError::invalid_plan(format!(
                            "mux task {id} must contain video and audio tracks"
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

fn output_paths_conflict(first: &str, second: &str) -> bool {
    const INTERNAL_SUFFIXES: &[&str] = &[
        ".part",
        ".parts",
        ".easydl.json",
        ".part.concat",
        ".part.ziptmp",
        ".easydl.json.tmp",
    ];

    paths_overlap(first, second)
        || INTERNAL_SUFFIXES.iter().any(|suffix| {
            paths_overlap(&format!("{first}{suffix}"), second)
                || paths_overlap(first, &format!("{second}{suffix}"))
        })
}

fn paths_overlap(first: &str, second: &str) -> bool {
    first == second
        || first
            .strip_prefix(second)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || second
            .strip_prefix(first)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub fn validate_relative_path(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(AppError::new(
            "output.unsafe_path",
            "output relativePath must be non-empty",
            false,
        ));
    }
    if value.contains('\\') {
        return Err(AppError::new(
            "output.unsafe_path",
            "output relativePath must use forward slashes",
            false,
        )
        .with_context("path", value));
    }
    if value.starts_with('/')
        || value.starts_with("//")
        || value.as_bytes().get(1) == Some(&b':')
        || Path::new(value).is_absolute()
    {
        return Err(AppError::new(
            "output.unsafe_path",
            "absolute output paths are not allowed",
            false,
        )
        .with_context("path", value));
    }
    if value.len() > 1_000 {
        return Err(AppError::new(
            "output.unsafe_path",
            "output relativePath is too long",
            false,
        ));
    }

    for component in value.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(AppError::new(
                "output.unsafe_path",
                "output relativePath contains an unsafe component",
                false,
            )
            .with_context("path", value));
        }
        if component.len() > 240
            || component
                .chars()
                .any(|ch| ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        {
            return Err(AppError::new(
                "output.unsafe_path",
                "output relativePath contains characters that are not portable",
                false,
            )
            .with_context("path", value));
        }
        let trimmed = component.trim_end_matches([' ', '.']);
        if trimmed != component || trimmed.is_empty() || is_windows_reserved_name(trimmed) {
            return Err(AppError::new(
                "output.unsafe_path",
                "output relativePath contains a reserved filename",
                false,
            )
            .with_context("path", value));
        }
    }
    Ok(())
}

fn validate_sources(sources: &[DownloadSource], owner: String) -> Result<()> {
    if sources.is_empty() {
        return Err(AppError::invalid_plan(format!(
            "{owner} must contain at least one source"
        )));
    }
    for source in sources {
        validate_download_source(source)?;
    }
    Ok(())
}

fn validate_download_source(source: &DownloadSource) -> Result<()> {
    let url = Url::parse(&source.url).map_err(|_| AppError::invalid_plan("invalid source URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(
            AppError::invalid_plan("download source URL must use http or https")
                .with_context("scheme", url.scheme()),
        );
    }
    if url.host_str().is_none() {
        return Err(AppError::invalid_plan(
            "download source URL must contain a host",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::new(
            "plan.secret_url",
            "credentials embedded in source URLs are not allowed",
            false,
        ));
    }
    if let Some(auth_ref) = source.auth_ref.as_deref() {
        auth::validate_reference(auth_ref, &url)?;
    }
    for (name, value) in &source.headers {
        let normalized = name.trim().to_ascii_lowercase();
        if is_sensitive_header(&normalized) {
            return Err(AppError::new(
                "plan.secret_header",
                format!("sensitive header {name} is not allowed in a plan"),
                false,
            )
            .with_context("header", name.clone()));
        }
        if is_executor_owned_header(&normalized) {
            return Err(AppError::new(
                "plan.reserved_header",
                format!("executor-owned header {name} is not allowed in a plan"),
                false,
            )
            .with_context("header", name.clone()));
        }
        HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| AppError::invalid_plan(format!("invalid HTTP header name: {name}")))?;
        HeaderValue::from_str(value)
            .map_err(|_| AppError::invalid_plan(format!("invalid HTTP header value for {name}")))?;
    }
    Ok(())
}

fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name,
        "authorization"
            | "cookie"
            | "proxy-authorization"
            | "set-cookie"
            | "x-api-key"
            | "x-auth-token"
    )
}

fn is_executor_owned_header(name: &str) -> bool {
    matches!(
        name,
        "connection" | "content-length" | "host" | "if-range" | "range" | "transfer-encoding"
    )
}

fn is_windows_reserved_name(component: &str) -> bool {
    let stem = component
        .split_once('.')
        .map_or(component, |(stem, _)| stem)
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRecord {
    pub task_id: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub partial: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FailureRecord {
    pub task_id: String,
    pub error: ErrorInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkipInfo {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkippedRecord {
    pub task_id: String,
    pub path: String,
    pub reason: SkipInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DownloadSummary {
    pub schema_version: u32,
    pub success: bool,
    pub artifacts: Vec<ArtifactRecord>,
    pub failures: Vec<FailureRecord>,
    pub skipped: Vec<SkippedRecord>,
}

impl DownloadSummary {
    pub fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            success: true,
            artifacts: Vec::new(),
            failures: Vec::new(),
            skipped: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DownloadEvent {
    pub schema_version: u32,
    #[serde(flatten)]
    pub event: DownloadEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DownloadEventKind {
    Start {
        #[serde(rename = "taskId")]
        task_id: String,
        output: String,
    },
    Reset {
        #[serde(rename = "taskId")]
        task_id: String,
        path: String,
        reason: SkipInfo,
    },
    Progress {
        #[serde(rename = "taskId")]
        task_id: String,
        downloaded: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        total: Option<u64>,
    },
    Artifact {
        #[serde(rename = "taskId")]
        task_id: String,
        path: String,
        #[serde(default, skip_serializing_if = "is_false")]
        partial: bool,
    },
    Skipped {
        #[serde(rename = "taskId")]
        task_id: String,
        path: String,
        reason: SkipInfo,
    },
    Error {
        #[serde(rename = "taskId")]
        task_id: String,
        error: ErrorInfo,
    },
    Done {
        success: bool,
        artifacts: Vec<ArtifactRecord>,
        failures: Vec<FailureRecord>,
        skipped: Vec<SkippedRecord>,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl DownloadEvent {
    pub fn new(event: DownloadEventKind) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            event,
        }
    }

    pub fn done(summary: &DownloadSummary) -> Self {
        Self::new(DownloadEventKind::Done {
            success: summary.success,
            artifacts: summary.artifacts.clone(),
            failures: summary.failures.clone(),
            skipped: summary.skipped.clone(),
        })
    }
}
