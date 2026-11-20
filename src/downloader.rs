mod archive;
mod ffmpeg;

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::{StreamExt, stream};
use reqwest::{
    Response, StatusCode,
    header::{
        ACCEPT_RANGES, CONTENT_RANGE, COOKIE, ETAG, HeaderMap, HeaderName, HeaderValue, IF_RANGE,
        LAST_MODIFIED, LOCATION, RANGE,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    time::sleep,
};
use url::Url;

use crate::{
    credentials::CredentialStore,
    domain::{
        ArchiveEntry, ArtifactRecord, DownloadEvent, DownloadEventKind, DownloadPlan,
        DownloadSource, DownloadSummary, DownloadTask, FailureRecord, MuxFormat, MuxTrack,
        MuxTrackKind, SkipInfo, SkippedRecord,
    },
    errors::{AppError, Result},
};

const DEFAULT_RETRIES: usize = 3;
const PROGRESS_INTERVAL_BYTES: u64 = 512 * 1024;
const RESUME_METADATA_VERSION: u32 = 2;

pub trait EventSink: Send + Sync {
    fn emit(&self, event: &DownloadEvent);
}

impl<F> EventSink for F
where
    F: Fn(&DownloadEvent) + Send + Sync,
{
    fn emit(&self, event: &DownloadEvent) {
        self(event);
    }
}

#[derive(Default)]
pub struct NoopEventSink;

impl EventSink for NoopEventSink {
    fn emit(&self, _event: &DownloadEvent) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollisionPolicy {
    Skip,
    Overwrite,
    Rename,
}

#[derive(Debug, Clone)]
pub struct ExecuteOptions {
    pub output_root: PathBuf,
    pub collision: CollisionPolicy,
    pub proxy: Option<String>,
    pub ffmpeg: PathBuf,
    pub timeout: Duration,
    pub item_concurrency: usize,
    pub chunk_concurrency: usize,
    pub multipart_threshold: u64,
    pub chunk_size: u64,
    pub cookies_path: Option<PathBuf>,
    pub config_dir: PathBuf,
}

impl Default for ExecuteOptions {
    fn default() -> Self {
        Self {
            output_root: PathBuf::from("./downloads"),
            collision: CollisionPolicy::Skip,
            proxy: None,
            ffmpeg: PathBuf::from("ffmpeg"),
            timeout: Duration::from_secs(30),
            item_concurrency: 4,
            chunk_concurrency: 4,
            multipart_threshold: 32 * 1024 * 1024,
            chunk_size: 8 * 1024 * 1024,
            cookies_path: None,
            config_dir: crate::credentials::default_config_dir(),
        }
    }
}

pub async fn execute_plan(
    plan: DownloadPlan,
    options: ExecuteOptions,
    sink: Arc<dyn EventSink>,
) -> Result<DownloadSummary> {
    plan.validate()?;
    let executor = DownloadExecutor::new(options, sink)?;
    executor.run(plan).await
}

struct DownloadExecutor {
    options: ExecuteOptions,
    client: reqwest::Client,
    credentials: CredentialStore,
    sink: Arc<dyn EventSink>,
}

impl DownloadExecutor {
    fn new(options: ExecuteOptions, sink: Arc<dyn EventSink>) -> Result<Self> {
        if options.item_concurrency == 0
            || options.chunk_concurrency == 0
            || options.chunk_size == 0
            || options.ffmpeg.as_os_str().is_empty()
        {
            return Err(AppError::new(
                "config.invalid",
                "concurrency, chunkConcurrency, chunkSize and ffmpeg must be configured",
                false,
            ));
        }
        let mut builder = reqwest::Client::builder()
            .connect_timeout(options.timeout)
            .read_timeout(options.timeout)
            .redirect(reqwest::redirect::Policy::none());
        if let Some(proxy) = options
            .proxy
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let configured = reqwest::Proxy::all(proxy)
                .map_err(|_| AppError::new("proxy.invalid", "invalid proxy URL", false))?;
            builder = builder.proxy(configured);
        }
        let client = builder
            .build()
            .map_err(|error| AppError::http_request(&error))?;
        let credentials =
            CredentialStore::new(options.config_dir.clone(), options.cookies_path.clone());
        Ok(Self {
            options,
            client,
            credentials,
            sink,
        })
    }

    async fn run(&self, plan: DownloadPlan) -> Result<DownloadSummary> {
        fs::create_dir_all(&self.options.output_root)
            .await
            .map_err(|error| {
                AppError::filesystem("create output directory", &self.options.output_root, &error)
            })?;
        let output_root = fs::canonicalize(&self.options.output_root)
            .await
            .map_err(|error| {
                AppError::filesystem(
                    "resolve output directory",
                    &self.options.output_root,
                    &error,
                )
            })?;

        let futures = plan.tasks.into_iter().enumerate().map(|(index, task)| {
            let output_root = output_root.clone();
            async move { (index, self.process_task(task, &output_root).await) }
        });
        let mut outcomes: Vec<(usize, TaskOutcome)> = stream::iter(futures)
            .buffer_unordered(self.options.item_concurrency)
            .collect()
            .await;
        outcomes.sort_by_key(|(index, _)| *index);

        let mut summary = DownloadSummary::empty();
        for (_, outcome) in outcomes {
            summary.artifacts.extend(outcome.artifacts);
            summary.failures.extend(outcome.failures);
            summary.skipped.extend(outcome.skipped);
        }
        summary.success = summary.failures.is_empty();
        self.emit(DownloadEvent::done(&summary));
        Ok(summary)
    }

    async fn process_task(&self, task: DownloadTask, output_root: &Path) -> TaskOutcome {
        match task {
            DownloadTask::File {
                id,
                sources,
                output,
            } => {
                self.process_file_task(id, sources, output.relative_path, output_root)
                    .await
            }
            DownloadTask::Archive {
                id,
                entries,
                output,
            } => {
                self.process_archive_task(id, entries, output.relative_path, output_root)
                    .await
            }
            DownloadTask::Mux {
                id,
                tracks,
                format,
                output,
            } => {
                self.process_mux_task(id, tracks, format, output.relative_path, output_root)
                    .await
            }
        }
    }

    async fn process_file_task(
        &self,
        id: String,
        sources: Vec<DownloadSource>,
        relative_path: String,
        output_root: &Path,
    ) -> TaskOutcome {
        self.emit(DownloadEvent::new(DownloadEventKind::Start {
            task_id: id.clone(),
            output: relative_path.clone(),
        }));
        let mut outcome = TaskOutcome::default();
        match self
            .process_file(
                &id,
                &sources,
                &relative_path,
                output_root,
                self.options.collision,
            )
            .await
        {
            Ok(FileOutcome::Artifact(artifact)) => self.record_artifact(&mut outcome, artifact),
            Ok(FileOutcome::Skipped(skipped)) => self.record_skipped(&mut outcome, skipped),
            Err(error) => self.record_failure(&mut outcome, id, error),
        }
        outcome
    }

    async fn process_archive_task(
        &self,
        id: String,
        entries: Vec<ArchiveEntry>,
        relative_path: String,
        output_root: &Path,
    ) -> TaskOutcome {
        self.emit(DownloadEvent::new(DownloadEventKind::Start {
            task_id: id.clone(),
            output: relative_path.clone(),
        }));
        let mut outcome = TaskOutcome::default();
        if let Err(error) = self
            .process_archive(&id, entries, &relative_path, output_root, &mut outcome)
            .await
        {
            self.record_failure(&mut outcome, id, error);
        }
        outcome
    }

    async fn process_archive(
        &self,
        task_id: &str,
        entries: Vec<ArchiveEntry>,
        relative_path: &str,
        output_root: &Path,
        outcome: &mut TaskOutcome,
    ) -> Result<()> {
        let fingerprint = archive_fingerprint(task_id, relative_path, &entries)?;
        let prepared = prepare_target(output_root, relative_path, self.options.collision).await?;
        let target = match prepared {
            PreparedTarget::Ready(path) => path,
            PreparedTarget::Skipped(path) => {
                let sidecar_path = append_suffix(&path, ".easydl.json");
                if archive::partial_state_matches(&sidecar_path, task_id, &fingerprint).await? {
                    path
                } else {
                    self.record_skipped(
                        outcome,
                        SkippedRecord {
                            task_id: task_id.to_owned(),
                            path: path.to_string_lossy().to_string(),
                            reason: SkipInfo {
                                code: "filesystem.exists".to_owned(),
                                message: "file already exists".to_owned(),
                            },
                        },
                    );
                    return Ok(());
                }
            }
        };

        let parts_dir = append_suffix(&target, ".parts");
        let sidecar_path = append_suffix(&target, ".easydl.json");
        ensure_parts_directory(&parts_dir).await?;
        let parts_root = fs::canonicalize(&parts_dir)
            .await
            .map_err(|error| AppError::filesystem("resolve archive state", &parts_dir, &error))?;

        let futures = entries.into_iter().enumerate().map(|(index, entry)| {
            let parts_root = parts_root.clone();
            let entry_task_id = archive_entry_task_id(task_id, &entry.id);
            let staging_path = format!("entry_{index:05}_{fingerprint}");
            async move {
                let result = self
                    .process_file(
                        &entry_task_id,
                        &entry.sources,
                        &staging_path,
                        &parts_root,
                        CollisionPolicy::Skip,
                    )
                    .await;
                (index, entry, entry_task_id, result)
            }
        });
        let mut entry_outcomes: Vec<(usize, ArchiveEntry, String, Result<FileOutcome>)> =
            stream::iter(futures)
                .buffer_unordered(self.options.item_concurrency)
                .collect()
                .await;
        entry_outcomes.sort_by_key(|(index, _, _, _)| *index);

        let mut files = Vec::new();
        let mut failed_entries = Vec::new();
        for (_, entry, entry_task_id, result) in entry_outcomes {
            match result {
                Ok(FileOutcome::Artifact(artifact)) => files.push(archive::ArchiveFile {
                    name: entry.path,
                    path: PathBuf::from(artifact.path),
                }),
                Ok(FileOutcome::Skipped(skipped)) => files.push(archive::ArchiveFile {
                    name: entry.path,
                    path: PathBuf::from(skipped.path),
                }),
                Err(error) => {
                    failed_entries.push(entry.id.clone());
                    self.record_failure(
                        outcome,
                        entry_task_id,
                        error
                            .with_context("archiveTaskId", task_id)
                            .with_context("entryId", entry.id)
                            .with_context("entryPath", entry.path),
                    );
                }
            }
        }

        if files.is_empty() {
            return Ok(());
        }

        let partial = !failed_entries.is_empty();
        if partial {
            archive::mark_partial(&sidecar_path, task_id, &fingerprint, failed_entries).await?;
        }
        archive::write_zip_archive(&target, files).await?;
        if !partial {
            remove_file_if_exists(&sidecar_path).await?;
            remove_dir_if_exists(&parts_dir).await?;
        }
        self.record_artifact(
            outcome,
            ArtifactRecord {
                task_id: task_id.to_owned(),
                path: target.to_string_lossy().to_string(),
                partial,
            },
        );
        Ok(())
    }

    async fn process_mux_task(
        &self,
        id: String,
        tracks: Vec<MuxTrack>,
        format: MuxFormat,
        relative_path: String,
        output_root: &Path,
    ) -> TaskOutcome {
        self.emit(DownloadEvent::new(DownloadEventKind::Start {
            task_id: id.clone(),
            output: relative_path.clone(),
        }));
        let mut outcome = TaskOutcome::default();
        if let Err(error) = self
            .process_mux(
                &id,
                tracks,
                format,
                &relative_path,
                output_root,
                &mut outcome,
            )
            .await
        {
            self.record_failure(&mut outcome, id, error);
        }
        outcome
    }

    async fn process_mux(
        &self,
        task_id: &str,
        tracks: Vec<MuxTrack>,
        format: MuxFormat,
        relative_path: &str,
        output_root: &Path,
        outcome: &mut TaskOutcome,
    ) -> Result<()> {
        let fingerprint = mux_fingerprint(task_id, relative_path, format, &tracks)?;
        let prepared = prepare_target(output_root, relative_path, self.options.collision).await?;
        let target = match prepared {
            PreparedTarget::Ready(path) => path,
            PreparedTarget::Skipped(path) => {
                self.record_skipped(
                    outcome,
                    SkippedRecord {
                        task_id: task_id.to_owned(),
                        path: path.to_string_lossy().to_string(),
                        reason: SkipInfo {
                            code: "filesystem.exists".to_owned(),
                            message: "file already exists".to_owned(),
                        },
                    },
                );
                return Ok(());
            }
        };

        let parts_dir = append_suffix(&target, ".parts");
        ensure_parts_directory(&parts_dir).await?;
        let parts_root = fs::canonicalize(&parts_dir)
            .await
            .map_err(|error| AppError::filesystem("resolve mux state", &parts_dir, &error))?;

        let futures = tracks.into_iter().enumerate().map(|(index, track)| {
            let parts_root = parts_root.clone();
            let track_task_id = mux_track_task_id(task_id, &track.id);
            let staging_path = format!("track_{index:05}_{fingerprint}");
            async move {
                let result = self
                    .process_file(
                        &track_task_id,
                        &track.sources,
                        &staging_path,
                        &parts_root,
                        CollisionPolicy::Skip,
                    )
                    .await;
                (index, track, track_task_id, result)
            }
        });
        let mut track_outcomes: Vec<(usize, MuxTrack, String, Result<FileOutcome>)> =
            stream::iter(futures)
                .buffer_unordered(self.options.item_concurrency)
                .collect()
                .await;
        track_outcomes.sort_by_key(|(index, _, _, _)| *index);

        let mut inputs = Vec::new();
        let mut failed = false;
        for (_, track, track_task_id, result) in track_outcomes {
            match result {
                Ok(FileOutcome::Artifact(artifact)) => inputs.push(ffmpeg::MuxInput {
                    kind: track.kind,
                    path: PathBuf::from(artifact.path),
                }),
                Ok(FileOutcome::Skipped(skipped)) => inputs.push(ffmpeg::MuxInput {
                    kind: track.kind,
                    path: PathBuf::from(skipped.path),
                }),
                Err(error) => {
                    failed = true;
                    self.record_failure(
                        outcome,
                        track_task_id,
                        error
                            .with_context("muxTaskId", task_id)
                            .with_context("trackId", track.id)
                            .with_context(
                                "trackKind",
                                match track.kind {
                                    MuxTrackKind::Video => "video",
                                    MuxTrackKind::Audio => "audio",
                                },
                            ),
                    );
                }
            }
        }
        if failed {
            return Ok(());
        }

        ffmpeg::mux(&self.options.ffmpeg, format, &target, inputs).await?;
        remove_dir_if_exists(&parts_dir).await?;
        self.record_artifact(
            outcome,
            ArtifactRecord {
                task_id: task_id.to_owned(),
                path: target.to_string_lossy().to_string(),
                partial: false,
            },
        );
        Ok(())
    }

    fn record_artifact(&self, outcome: &mut TaskOutcome, artifact: ArtifactRecord) {
        self.emit(DownloadEvent::new(DownloadEventKind::Artifact {
            task_id: artifact.task_id.clone(),
            path: artifact.path.clone(),
            partial: artifact.partial,
        }));
        outcome.artifacts.push(artifact);
    }

    fn record_skipped(&self, outcome: &mut TaskOutcome, skipped: SkippedRecord) {
        self.emit(DownloadEvent::new(DownloadEventKind::Skipped {
            task_id: skipped.task_id.clone(),
            path: skipped.path.clone(),
            reason: skipped.reason.clone(),
        }));
        outcome.skipped.push(skipped);
    }

    fn record_failure(&self, outcome: &mut TaskOutcome, task_id: String, error: AppError) {
        let failure = FailureRecord {
            task_id,
            error: error.info,
        };
        self.emit(DownloadEvent::new(DownloadEventKind::Error {
            task_id: failure.task_id.clone(),
            error: failure.error.clone(),
        }));
        outcome.failures.push(failure);
    }

    async fn process_file(
        &self,
        task_id: &str,
        sources: &[DownloadSource],
        relative_path: &str,
        output_root: &Path,
        collision: CollisionPolicy,
    ) -> Result<FileOutcome> {
        let prepared = prepare_target(output_root, relative_path, collision).await?;
        let target = match prepared {
            PreparedTarget::Ready(path) => path,
            PreparedTarget::Skipped(path) => {
                return Ok(FileOutcome::Skipped(SkippedRecord {
                    task_id: task_id.to_owned(),
                    path: path.to_string_lossy().to_string(),
                    reason: SkipInfo {
                        code: "filesystem.exists".to_owned(),
                        message: "file already exists".to_owned(),
                    },
                }));
            }
        };

        let part_path = append_suffix(&target, ".part");
        let parts_dir = append_suffix(&target, ".parts");
        let concat_path = append_suffix(&part_path, ".concat");
        let sidecar_path = append_suffix(&target, ".easydl.json");
        let mut last_error: Option<AppError> = None;

        for (source_index, source) in sources.iter().enumerate() {
            if source_index > 0 {
                self.reset_partial_state(
                    task_id,
                    &part_path,
                    &parts_dir,
                    &concat_path,
                    &sidecar_path,
                    "download.source_changed",
                    "switching to a fallback download source",
                )
                .await?;
            }

            let fingerprint = source_fingerprint(task_id, relative_path, source)?;
            for attempt in 1..=DEFAULT_RETRIES {
                match self
                    .download_source(
                        task_id,
                        source,
                        &fingerprint,
                        &part_path,
                        &parts_dir,
                        &concat_path,
                        &sidecar_path,
                    )
                    .await
                {
                    Ok(()) => {
                        fs::rename(&part_path, &target).await.map_err(|error| {
                            AppError::filesystem("finalize download", &target, &error)
                        })?;
                        // Stale internal state is harmless once the completed target exists. Do
                        // not turn a completed artifact into a failure solely because cleanup failed.
                        let _ = remove_file_if_exists(&sidecar_path).await;
                        let _ = remove_file_if_exists(&concat_path).await;
                        let _ = remove_dir_if_exists(&parts_dir).await;
                        return Ok(FileOutcome::Artifact(ArtifactRecord {
                            task_id: task_id.to_owned(),
                            path: target.to_string_lossy().to_string(),
                            partial: false,
                        }));
                    }
                    Err(error) => {
                        let retry = error.info.retryable && attempt < DEFAULT_RETRIES;
                        last_error = Some(error);
                        if retry {
                            sleep(Duration::from_secs(attempt as u64)).await;
                        } else {
                            break;
                        }
                    }
                }
            }
        }

        let last = last_error.unwrap_or_else(|| {
            AppError::new(
                "download.no_sources",
                "file task has no usable download source",
                false,
            )
        });
        if last.info.code.starts_with("auth.") {
            return Err(last.with_context("sourcesTried", sources.len()));
        }
        Err(AppError::new(
            "download.sources_exhausted",
            format!("all download sources failed: {}", last.info.message),
            last.info.retryable,
        )
        .with_context("lastCode", last.info.code))
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_source(
        &self,
        task_id: &str,
        source: &DownloadSource,
        fingerprint: &str,
        part_path: &Path,
        parts_dir: &Path,
        concat_path: &Path,
        sidecar_path: &Path,
    ) -> Result<()> {
        let part_size = inspect_partial_file(part_path).await?;
        let parts_present = inspect_parts_directory(parts_dir).await?;
        let concat_present = path_exists(concat_path).await?;
        let has_transfer_state = part_size > 0 || parts_present || concat_present;
        if !has_transfer_state {
            remove_file_if_exists(part_path).await?;
            remove_file_if_exists(concat_path).await?;
            remove_dir_if_exists(parts_dir).await?;
            remove_file_if_exists(sidecar_path).await?;
            return self
                .start_new_transfer(
                    task_id,
                    source,
                    fingerprint,
                    part_path,
                    parts_dir,
                    concat_path,
                    sidecar_path,
                    None,
                )
                .await;
        }

        let metadata = match load_resume_metadata(sidecar_path).await? {
            ResumeMetadataState::Valid(metadata) => metadata,
            ResumeMetadataState::Missing => {
                self.reset_partial_state(
                    task_id,
                    part_path,
                    parts_dir,
                    concat_path,
                    sidecar_path,
                    "resume.metadata_missing",
                    "partial download has no identity metadata; restarting safely",
                )
                .await?;
                return self
                    .start_new_transfer(
                        task_id,
                        source,
                        fingerprint,
                        part_path,
                        parts_dir,
                        concat_path,
                        sidecar_path,
                        None,
                    )
                    .await;
            }
            ResumeMetadataState::Invalid => {
                self.reset_partial_state(
                    task_id,
                    part_path,
                    parts_dir,
                    concat_path,
                    sidecar_path,
                    "resume.metadata_invalid",
                    "partial download metadata is invalid; restarting safely",
                )
                .await?;
                return self
                    .start_new_transfer(
                        task_id,
                        source,
                        fingerprint,
                        part_path,
                        parts_dir,
                        concat_path,
                        sidecar_path,
                        None,
                    )
                    .await;
            }
        };

        let remote = match self.probe_source(source).await {
            Ok(remote) => remote,
            Err(_) => {
                self.reset_partial_state(
                    task_id,
                    part_path,
                    parts_dir,
                    concat_path,
                    sidecar_path,
                    "resume.probe_failed",
                    "could not validate the partial download; restarting safely",
                )
                .await?;
                return self
                    .start_new_transfer(
                        task_id,
                        source,
                        fingerprint,
                        part_path,
                        parts_dir,
                        concat_path,
                        sidecar_path,
                        None,
                    )
                    .await;
            }
        };

        if let Err(reason) = validate_resume_identity(
            &metadata,
            task_id,
            fingerprint,
            &source.url,
            &remote,
            part_size,
        ) {
            self.reset_partial_state(
                task_id,
                part_path,
                parts_dir,
                concat_path,
                sidecar_path,
                &reason.code,
                &reason.message,
            )
            .await?;
            return self
                .start_new_transfer(
                    task_id,
                    source,
                    fingerprint,
                    part_path,
                    parts_dir,
                    concat_path,
                    sidecar_path,
                    Some(remote),
                )
                .await;
        }

        let total = remote.total.expect("validated resume identity has a total");
        match metadata.layout {
            ResumeLayout::Sequential => {
                if parts_present || concat_present || part_size == 0 {
                    self.reset_partial_state(
                        task_id,
                        part_path,
                        parts_dir,
                        concat_path,
                        sidecar_path,
                        "resume.layout_mismatch",
                        "partial download state does not match its sequential layout",
                    )
                    .await?;
                    return self
                        .start_new_transfer(
                            task_id,
                            source,
                            fingerprint,
                            part_path,
                            parts_dir,
                            concat_path,
                            sidecar_path,
                            Some(remote),
                        )
                        .await;
                }
                if part_size == total {
                    self.emit(DownloadEvent::new(DownloadEventKind::Progress {
                        task_id: task_id.to_owned(),
                        downloaded: total,
                        total: Some(total),
                    }));
                    return Ok(());
                }
                if !remote.supports_ranges {
                    self.reset_partial_state(
                        task_id,
                        part_path,
                        parts_dir,
                        concat_path,
                        sidecar_path,
                        "resume.range_unsupported",
                        "remote server no longer supports range requests; restarting safely",
                    )
                    .await?;
                    return self
                        .download_fresh(task_id, source, fingerprint, part_path, sidecar_path)
                        .await;
                }
                self.download_resumed(
                    task_id,
                    source,
                    fingerprint,
                    part_path,
                    parts_dir,
                    concat_path,
                    sidecar_path,
                    part_size,
                    remote,
                )
                .await
            }
            ResumeLayout::Multipart { chunk_size } => {
                if part_size > 0 {
                    if part_size == total {
                        remove_file_if_exists(concat_path).await?;
                        remove_dir_if_exists(parts_dir).await?;
                        self.emit(DownloadEvent::new(DownloadEventKind::Progress {
                            task_id: task_id.to_owned(),
                            downloaded: total,
                            total: Some(total),
                        }));
                        return Ok(());
                    }
                    self.reset_partial_state(
                        task_id,
                        part_path,
                        parts_dir,
                        concat_path,
                        sidecar_path,
                        "resume.layout_mismatch",
                        "multipart state contains an incomplete final part; restarting safely",
                    )
                    .await?;
                    return self
                        .start_new_transfer(
                            task_id,
                            source,
                            fingerprint,
                            part_path,
                            parts_dir,
                            concat_path,
                            sidecar_path,
                            Some(remote),
                        )
                        .await;
                }
                if !parts_present || chunk_size == 0 {
                    self.reset_partial_state(
                        task_id,
                        part_path,
                        parts_dir,
                        concat_path,
                        sidecar_path,
                        "resume.layout_mismatch",
                        "multipart download state is incomplete; restarting safely",
                    )
                    .await?;
                    return self
                        .start_new_transfer(
                            task_id,
                            source,
                            fingerprint,
                            part_path,
                            parts_dir,
                            concat_path,
                            sidecar_path,
                            Some(remote),
                        )
                        .await;
                }
                if !remote.supports_ranges {
                    self.reset_partial_state(
                        task_id,
                        part_path,
                        parts_dir,
                        concat_path,
                        sidecar_path,
                        "resume.range_unsupported",
                        "remote server no longer supports multipart ranges; restarting sequentially",
                    )
                    .await?;
                    return self
                        .download_fresh(task_id, source, fingerprint, part_path, sidecar_path)
                        .await;
                }
                self.download_multipart(
                    task_id,
                    source,
                    fingerprint,
                    part_path,
                    parts_dir,
                    concat_path,
                    sidecar_path,
                    remote,
                    chunk_size,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_new_transfer(
        &self,
        task_id: &str,
        source: &DownloadSource,
        fingerprint: &str,
        part_path: &Path,
        parts_dir: &Path,
        concat_path: &Path,
        sidecar_path: &Path,
        remote_hint: Option<RemoteIdentity>,
    ) -> Result<()> {
        let remote = match remote_hint {
            Some(remote) => Some(remote),
            None if self.options.chunk_concurrency > 1 => self.probe_source(source).await.ok(),
            None => None,
        };
        if let Some(remote) = remote
            && should_use_multipart(&remote, &self.options)
        {
            return self
                .download_multipart(
                    task_id,
                    source,
                    fingerprint,
                    part_path,
                    parts_dir,
                    concat_path,
                    sidecar_path,
                    remote,
                    self.options.chunk_size,
                )
                .await;
        }
        self.download_fresh(task_id, source, fingerprint, part_path, sidecar_path)
            .await
    }

    async fn probe_source(&self, source: &DownloadSource) -> Result<RemoteIdentity> {
        let mut headers = build_headers(&source.headers)?;
        headers.insert(RANGE, HeaderValue::from_static("bytes=0-0"));
        let response = self.send_source_request(source, headers).await?;
        let status = response.status();

        if status == StatusCode::PARTIAL_CONTENT {
            let range = content_range(&response).ok_or_else(|| {
                AppError::new(
                    "http.invalid_content_range",
                    "range probe returned an invalid Content-Range",
                    true,
                )
            })?;
            if range.start != 0 || range.end != 0 {
                return Err(AppError::new(
                    "http.invalid_content_range",
                    "range probe returned an unexpected byte range",
                    true,
                ));
            }
            return Ok(RemoteIdentity::from_response(
                &response,
                Some(range.total),
                true,
            ));
        }
        if status == StatusCode::RANGE_NOT_SATISFIABLE && unsatisfied_total(&response) == Some(0) {
            return Ok(RemoteIdentity::from_response(&response, Some(0), true));
        }
        if status.is_success() {
            let total = response.content_length();
            return Ok(RemoteIdentity::from_response(&response, total, false));
        }
        Err(AppError::http_status(status))
    }

    async fn download_fresh(
        &self,
        task_id: &str,
        source: &DownloadSource,
        fingerprint: &str,
        part_path: &Path,
        sidecar_path: &Path,
    ) -> Result<()> {
        let response = self
            .send_source_request(source, build_headers(&source.headers)?)
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::http_status(status));
        }

        let identity = if status == StatusCode::PARTIAL_CONTENT {
            let range = content_range(&response).ok_or_else(|| {
                AppError::new(
                    "http.invalid_content_range",
                    "fresh download returned an invalid Content-Range",
                    true,
                )
            })?;
            if range.start != 0 {
                return Err(AppError::new(
                    "http.invalid_content_range",
                    "fresh download did not begin at byte zero",
                    true,
                ));
            }
            RemoteIdentity::from_response(&response, Some(range.total), true)
        } else {
            let total = response.content_length();
            RemoteIdentity::from_response(&response, total, accepts_ranges(&response))
        };

        self.write_response(
            task_id,
            source,
            fingerprint,
            response,
            part_path,
            sidecar_path,
            0,
            false,
            identity,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_resumed(
        &self,
        task_id: &str,
        source: &DownloadSource,
        fingerprint: &str,
        part_path: &Path,
        parts_dir: &Path,
        concat_path: &Path,
        sidecar_path: &Path,
        initial: u64,
        identity: RemoteIdentity,
    ) -> Result<()> {
        let mut headers = build_headers(&source.headers)?;
        let range_value = HeaderValue::from_str(&format!("bytes={initial}-")).map_err(|_| {
            AppError::new(
                "http.invalid_range",
                "could not construct resume Range header",
                false,
            )
        })?;
        headers.insert(RANGE, range_value);
        if let Some(if_range) = identity.if_range_value()
            && let Ok(value) = HeaderValue::from_str(if_range)
        {
            headers.insert(IF_RANGE, value);
        }

        let response = self.send_source_request(source, headers).await?;
        let status = response.status();
        if status == StatusCode::OK {
            self.reset_partial_state(
                task_id,
                part_path,
                parts_dir,
                concat_path,
                sidecar_path,
                "resume.server_restarted",
                "remote server restarted the response; replacing the partial download",
            )
            .await?;
            let fresh_identity = RemoteIdentity::from_response(
                &response,
                response.content_length(),
                accepts_ranges(&response),
            );
            return self
                .write_response(
                    task_id,
                    source,
                    fingerprint,
                    response,
                    part_path,
                    sidecar_path,
                    0,
                    false,
                    fresh_identity,
                )
                .await;
        }
        if status != StatusCode::PARTIAL_CONTENT {
            return Err(AppError::http_status(status));
        }

        let range = content_range(&response).ok_or_else(|| {
            AppError::new(
                "http.invalid_content_range",
                "resume response returned an invalid Content-Range",
                true,
            )
        })?;
        if range.start != initial || Some(range.total) != identity.total {
            self.reset_partial_state(
                task_id,
                part_path,
                parts_dir,
                concat_path,
                sidecar_path,
                "resume.range_mismatch",
                "resume response does not match the validated remote object",
            )
            .await?;
            return self
                .download_fresh(task_id, source, fingerprint, part_path, sidecar_path)
                .await;
        }
        if header_text(response.headers(), ETAG)
            .is_some_and(|etag| identity.etag.as_deref().is_some_and(|saved| saved != etag))
            || header_text(response.headers(), LAST_MODIFIED).is_some_and(|last_modified| {
                identity
                    .last_modified
                    .as_deref()
                    .is_some_and(|saved| saved != last_modified)
            })
        {
            self.reset_partial_state(
                task_id,
                part_path,
                parts_dir,
                concat_path,
                sidecar_path,
                "resume.identity_mismatch",
                "resume response identity changed; restarting safely",
            )
            .await?;
            return self
                .download_fresh(task_id, source, fingerprint, part_path, sidecar_path)
                .await;
        }

        self.emit(DownloadEvent::new(DownloadEventKind::Progress {
            task_id: task_id.to_owned(),
            downloaded: initial,
            total: identity.total,
        }));
        self.write_response(
            task_id,
            source,
            fingerprint,
            response,
            part_path,
            sidecar_path,
            initial,
            true,
            identity,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_response(
        &self,
        task_id: &str,
        source: &DownloadSource,
        fingerprint: &str,
        response: Response,
        part_path: &Path,
        sidecar_path: &Path,
        initial: u64,
        append: bool,
        identity: RemoteIdentity,
    ) -> Result<()> {
        let parent = part_path.parent().ok_or_else(|| {
            AppError::new(
                "output.unsafe_path",
                "download target has no parent directory",
                false,
            )
        })?;
        fs::create_dir_all(parent)
            .await
            .map_err(|error| AppError::filesystem("create parent directory", parent, &error))?;

        let metadata = ResumeMetadata {
            schema_version: RESUME_METADATA_VERSION,
            task_id: task_id.to_owned(),
            fingerprint: fingerprint.to_owned(),
            source_url: source.url.clone(),
            total: identity.total,
            etag: identity.etag.clone(),
            last_modified: identity.last_modified.clone(),
            layout: ResumeLayout::Sequential,
            created_at: OffsetDateTime::now_utc(),
        };
        write_resume_metadata(sidecar_path, &metadata).await?;

        let mut options = OpenOptions::new();
        options.create(true).write(true);
        if append {
            options.append(true);
        } else {
            options.truncate(true);
        }
        let mut file = options
            .open(part_path)
            .await
            .map_err(|error| AppError::filesystem("open partial file", part_path, &error))?;

        if append {
            let actual = inspect_partial_file(part_path).await?;
            if actual != initial {
                return Err(AppError::new(
                    "resume.local_size_changed",
                    "partial file size changed while preparing the resume request",
                    true,
                )
                .with_context("expected", initial)
                .with_context("actual", actual));
            }
        }

        let total = identity.total;
        let mut downloaded = initial;
        let mut last_emitted = initial;
        let mut next_progress = initial.saturating_add(PROGRESS_INTERVAL_BYTES);
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| AppError::http_request(&error))?;
            file.write_all(&chunk)
                .await
                .map_err(|error| AppError::filesystem("write partial file", part_path, &error))?;
            downloaded += chunk.len() as u64;
            if downloaded >= next_progress || total.is_some_and(|value| downloaded == value) {
                self.emit(DownloadEvent::new(DownloadEventKind::Progress {
                    task_id: task_id.to_owned(),
                    downloaded,
                    total,
                }));
                last_emitted = downloaded;
                next_progress = downloaded.saturating_add(PROGRESS_INTERVAL_BYTES);
            }
        }
        file.flush()
            .await
            .map_err(|error| AppError::filesystem("flush partial file", part_path, &error))?;
        file.sync_data()
            .await
            .map_err(|error| AppError::filesystem("sync partial file", part_path, &error))?;

        if let Some(total) = total
            && downloaded != total
        {
            return Err(AppError::new(
                "http.short_read",
                format!("short download: got {downloaded} of {total} bytes"),
                true,
            )
            .with_context("downloaded", downloaded)
            .with_context("total", total));
        }
        if downloaded != last_emitted || (downloaded == 0 && last_emitted == 0) {
            self.emit(DownloadEvent::new(DownloadEventKind::Progress {
                task_id: task_id.to_owned(),
                downloaded,
                total,
            }));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_multipart(
        &self,
        task_id: &str,
        source: &DownloadSource,
        fingerprint: &str,
        part_path: &Path,
        parts_dir: &Path,
        concat_path: &Path,
        sidecar_path: &Path,
        identity: RemoteIdentity,
        chunk_size: u64,
    ) -> Result<()> {
        let total = identity.total.ok_or_else(|| {
            AppError::new(
                "multipart.unknown_size",
                "multipart download requires a known content length",
                false,
            )
        })?;
        let chunks = build_chunks(total, chunk_size);
        if chunks.len() <= 1 {
            return self
                .download_fresh(task_id, source, fingerprint, part_path, sidecar_path)
                .await;
        }

        ensure_parts_directory(parts_dir).await?;
        remove_file_if_exists(concat_path).await?;
        let metadata = ResumeMetadata {
            schema_version: RESUME_METADATA_VERSION,
            task_id: task_id.to_owned(),
            fingerprint: fingerprint.to_owned(),
            source_url: source.url.clone(),
            total: Some(total),
            etag: identity.etag.clone(),
            last_modified: identity.last_modified.clone(),
            layout: ResumeLayout::Multipart { chunk_size },
            created_at: OffsetDateTime::now_utc(),
        };
        write_resume_metadata(sidecar_path, &metadata).await?;

        let mut completed = 0_u64;
        let mut pending = Vec::new();
        for chunk in &chunks {
            if chunk_is_complete(parts_dir, *chunk).await? {
                completed += chunk.len();
            } else {
                pending.push(*chunk);
            }
        }
        self.emit(DownloadEvent::new(DownloadEventKind::Progress {
            task_id: task_id.to_owned(),
            downloaded: completed,
            total: Some(total),
        }));

        let progress = Arc::new(Mutex::new(completed));
        let futures = pending.into_iter().map(|chunk| {
            let progress = Arc::clone(&progress);
            let identity = identity.clone();
            async move {
                self.download_chunk(source, parts_dir, chunk, &identity)
                    .await?;
                let mut downloaded = progress.lock().expect("multipart progress lock poisoned");
                *downloaded += chunk.len();
                self.emit(DownloadEvent::new(DownloadEventKind::Progress {
                    task_id: task_id.to_owned(),
                    downloaded: *downloaded,
                    total: Some(total),
                }));
                Ok::<(), AppError>(())
            }
        });
        let results: Vec<Result<()>> = stream::iter(futures)
            .buffer_unordered(self.options.chunk_concurrency)
            .collect()
            .await;
        if let Some(error) = results.into_iter().find_map(std::result::Result::err) {
            if matches!(
                error.info.code.as_str(),
                "multipart.range_ignored" | "multipart.range_mismatch" | "resume.identity_mismatch"
            ) {
                self.reset_partial_state(
                    task_id,
                    part_path,
                    parts_dir,
                    concat_path,
                    sidecar_path,
                    "multipart.range_unsupported",
                    "remote server rejected multipart ranges; restarting sequentially",
                )
                .await?;
                return self
                    .download_fresh(task_id, source, fingerprint, part_path, sidecar_path)
                    .await;
            }
            return Err(error);
        }

        concatenate_chunks(part_path, concat_path, parts_dir, &chunks, total).await?;
        remove_dir_if_exists(parts_dir).await?;
        self.emit(DownloadEvent::new(DownloadEventKind::Progress {
            task_id: task_id.to_owned(),
            downloaded: total,
            total: Some(total),
        }));
        Ok(())
    }

    async fn download_chunk(
        &self,
        source: &DownloadSource,
        parts_dir: &Path,
        chunk: Chunk,
        identity: &RemoteIdentity,
    ) -> Result<()> {
        let mut headers = build_headers(&source.headers)?;
        headers.insert(
            RANGE,
            HeaderValue::from_str(&format!("bytes={}-{}", chunk.start, chunk.end)).map_err(
                |_| {
                    AppError::new(
                        "http.invalid_range",
                        "could not construct multipart Range header",
                        false,
                    )
                },
            )?,
        );
        if let Some(if_range) = identity.if_range_value()
            && let Ok(value) = HeaderValue::from_str(if_range)
        {
            headers.insert(IF_RANGE, value);
        }
        let response = self.send_source_request(source, headers).await?;
        if response.status() == StatusCode::OK {
            return Err(AppError::new(
                "multipart.range_ignored",
                "remote server ignored a multipart Range request",
                false,
            ));
        }
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(AppError::http_status(response.status()));
        }
        let range = content_range(&response).ok_or_else(|| {
            AppError::new(
                "multipart.range_mismatch",
                "multipart response returned an invalid Content-Range",
                false,
            )
        })?;
        if range.start != chunk.start
            || range.end != chunk.end
            || Some(range.total) != identity.total
        {
            return Err(AppError::new(
                "multipart.range_mismatch",
                "multipart response range does not match the requested chunk",
                false,
            ));
        }
        if header_text(response.headers(), ETAG)
            .is_some_and(|etag| identity.etag.as_deref().is_some_and(|saved| saved != etag))
            || header_text(response.headers(), LAST_MODIFIED).is_some_and(|last_modified| {
                identity
                    .last_modified
                    .as_deref()
                    .is_some_and(|saved| saved != last_modified)
            })
        {
            return Err(AppError::new(
                "resume.identity_mismatch",
                "multipart response identity changed",
                false,
            ));
        }

        let path = chunk_path(parts_dir, chunk);
        let temporary = append_suffix(&path, ".tmp");
        remove_file_if_exists(&temporary).await?;
        remove_file_if_exists(&path).await?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .await
            .map_err(|error| AppError::filesystem("open multipart chunk", &temporary, &error))?;
        let mut written = 0_u64;
        let mut body = response.bytes_stream();
        while let Some(bytes) = body.next().await {
            let bytes = bytes.map_err(|error| AppError::http_request(&error))?;
            file.write_all(&bytes).await.map_err(|error| {
                AppError::filesystem("write multipart chunk", &temporary, &error)
            })?;
            written += bytes.len() as u64;
        }
        file.flush()
            .await
            .map_err(|error| AppError::filesystem("flush multipart chunk", &temporary, &error))?;
        file.sync_data()
            .await
            .map_err(|error| AppError::filesystem("sync multipart chunk", &temporary, &error))?;
        drop(file);
        if written != chunk.len() {
            return Err(AppError::new(
                "http.short_read",
                format!(
                    "multipart chunk {} was short: got {written} of {} bytes",
                    chunk.index,
                    chunk.len()
                ),
                true,
            ));
        }
        fs::rename(&temporary, &path)
            .await
            .map_err(|error| AppError::filesystem("commit multipart chunk", &path, &error))?;
        Ok(())
    }

    async fn send_source_request(
        &self,
        source: &DownloadSource,
        headers: HeaderMap,
    ) -> Result<Response> {
        const MAX_REDIRECTS: usize = 10;
        let mut url =
            Url::parse(&source.url).map_err(|_| AppError::invalid_plan("invalid source URL"))?;
        for redirect_count in 0..=MAX_REDIRECTS {
            let mut request_headers = headers.clone();
            if let Some(auth_ref) = source.auth_ref.as_deref() {
                let cookie = self
                    .credentials
                    .cookie_for_reference(auth_ref, &url)
                    .await?;
                request_headers.insert(
                    COOKIE,
                    HeaderValue::from_str(&cookie).map_err(|_| {
                        AppError::new(
                            "auth.invalid_credential",
                            "the local credential profile contains an invalid Cookie header",
                            false,
                        )
                        .with_context("authRef", auth_ref)
                    })?,
                );
            }
            let response = self
                .client
                .get(url.clone())
                .headers(request_headers)
                .send()
                .await
                .map_err(|error| AppError::http_request(&error))?;
            if !response.status().is_redirection() {
                return Ok(response);
            }
            let Some(location) = response.headers().get(LOCATION) else {
                return Ok(response);
            };
            if redirect_count == MAX_REDIRECTS {
                return Err(AppError::new(
                    "http.redirect_limit",
                    "download source exceeded the redirect limit",
                    true,
                ));
            }
            let location = location.to_str().map_err(|_| {
                AppError::new(
                    "http.invalid_redirect",
                    "download source returned an invalid Location header",
                    false,
                )
            })?;
            let next = url.join(location).map_err(|_| {
                AppError::new(
                    "http.invalid_redirect",
                    "download source returned an invalid redirect URL",
                    false,
                )
            })?;
            if !matches!(next.scheme(), "http" | "https")
                || next.host_str().is_none()
                || !next.username().is_empty()
                || next.password().is_some()
            {
                return Err(AppError::new(
                    "http.unsafe_redirect",
                    "download source redirected to an unsafe URL",
                    false,
                ));
            }
            url = next;
        }
        unreachable!("redirect loop returns at its configured limit")
    }

    #[allow(clippy::too_many_arguments)]
    async fn reset_partial_state(
        &self,
        task_id: &str,
        part_path: &Path,
        parts_dir: &Path,
        concat_path: &Path,
        sidecar_path: &Path,
        code: &str,
        message: &str,
    ) -> Result<()> {
        let existed = path_exists(part_path).await?
            || path_exists(parts_dir).await?
            || path_exists(concat_path).await?
            || path_exists(sidecar_path).await?;
        remove_file_if_exists(part_path).await?;
        remove_file_if_exists(concat_path).await?;
        remove_dir_if_exists(parts_dir).await?;
        remove_file_if_exists(sidecar_path).await?;
        if existed {
            self.emit(DownloadEvent::new(DownloadEventKind::Reset {
                task_id: task_id.to_owned(),
                path: part_path.to_string_lossy().to_string(),
                reason: SkipInfo {
                    code: code.to_owned(),
                    message: message.to_owned(),
                },
            }));
        }
        Ok(())
    }

    fn emit(&self, event: DownloadEvent) {
        self.sink.emit(&event);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResumeMetadata {
    schema_version: u32,
    task_id: String,
    fingerprint: String,
    source_url: String,
    total: Option<u64>,
    etag: Option<String>,
    last_modified: Option<String>,
    layout: ResumeLayout,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum ResumeLayout {
    Sequential,
    Multipart {
        #[serde(rename = "chunkSize")]
        chunk_size: u64,
    },
}

enum ResumeMetadataState {
    Missing,
    Invalid,
    Valid(ResumeMetadata),
}

#[derive(Debug, Clone, Copy)]
struct Chunk {
    index: usize,
    start: u64,
    end: u64,
}

impl Chunk {
    fn len(self) -> u64 {
        self.end - self.start + 1
    }
}

#[derive(Debug, Clone)]
struct RemoteIdentity {
    total: Option<u64>,
    etag: Option<String>,
    last_modified: Option<String>,
    supports_ranges: bool,
}

impl RemoteIdentity {
    fn from_response(response: &Response, total: Option<u64>, supports_ranges: bool) -> Self {
        Self {
            total,
            etag: header_text(response.headers(), ETAG).map(str::to_owned),
            last_modified: header_text(response.headers(), LAST_MODIFIED).map(str::to_owned),
            supports_ranges,
        }
    }

    fn if_range_value(&self) -> Option<&str> {
        self.etag
            .as_deref()
            .filter(|etag| !etag.trim_start().starts_with("W/"))
            .or(self.last_modified.as_deref())
    }
}

#[derive(Debug, Clone, Copy)]
struct ParsedContentRange {
    start: u64,
    end: u64,
    total: u64,
}

#[derive(Default)]
struct TaskOutcome {
    artifacts: Vec<ArtifactRecord>,
    failures: Vec<FailureRecord>,
    skipped: Vec<SkippedRecord>,
}

enum FileOutcome {
    Artifact(ArtifactRecord),
    Skipped(SkippedRecord),
}

enum PreparedTarget {
    Ready(PathBuf),
    Skipped(PathBuf),
}

async fn prepare_target(
    output_root: &Path,
    relative_path: &str,
    collision: CollisionPolicy,
) -> Result<PreparedTarget> {
    let target = join_portable_path(output_root, relative_path);
    let parent = target.parent().ok_or_else(|| {
        AppError::new(
            "output.unsafe_path",
            "output relativePath has no parent directory",
            false,
        )
    })?;
    ensure_safe_parent(output_root, parent).await?;

    match fs::symlink_metadata(&target).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(AppError::new(
                    "output.symlink",
                    "refusing to write through an output symlink",
                    false,
                )
                .with_context("path", target.to_string_lossy().to_string()));
            }
            if !metadata.is_file() {
                return Err(AppError::new(
                    "filesystem.not_file",
                    "output target exists and is not a regular file",
                    false,
                )
                .with_context("path", target.to_string_lossy().to_string()));
            }
            match collision {
                CollisionPolicy::Skip => return Ok(PreparedTarget::Skipped(target)),
                CollisionPolicy::Overwrite => {}
                CollisionPolicy::Rename => {
                    return Ok(PreparedTarget::Ready(
                        allocate_renamed_target(&target).await?,
                    ));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AppError::filesystem(
                "inspect output target",
                &target,
                &error,
            ));
        }
    }
    if collision == CollisionPolicy::Overwrite {
        remove_download_state(&target).await?;
    }
    Ok(PreparedTarget::Ready(target))
}

async fn remove_download_state(target: &Path) -> Result<()> {
    let part_path = append_suffix(target, ".part");
    remove_file_if_exists(target).await?;
    remove_file_if_exists(&part_path).await?;
    remove_file_if_exists(&append_suffix(&part_path, ".concat")).await?;
    remove_dir_if_exists(&append_suffix(target, ".parts")).await?;
    remove_file_if_exists(&append_suffix(target, ".easydl.json")).await?;
    Ok(())
}

async fn ensure_safe_parent(output_root: &Path, parent: &Path) -> Result<()> {
    let relative = parent.strip_prefix(output_root).map_err(|_| {
        AppError::new(
            "output.unsafe_path",
            "output target escapes output root",
            false,
        )
    })?;
    let mut current = output_root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current).await {
            Ok(metadata) => validate_output_directory(&current, &metadata)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&current).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let metadata = fs::symlink_metadata(&current).await.map_err(|error| {
                            AppError::filesystem("inspect output directory", &current, &error)
                        })?;
                        validate_output_directory(&current, &metadata)?;
                    }
                    Err(error) => {
                        return Err(AppError::filesystem(
                            "create output directory",
                            &current,
                            &error,
                        ));
                    }
                }
            }
            Err(error) => {
                return Err(AppError::filesystem(
                    "inspect output directory",
                    &current,
                    &error,
                ));
            }
        }
    }
    Ok(())
}

fn validate_output_directory(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() {
        return Err(AppError::new(
            "output.symlink",
            "refusing to traverse an output symlink",
            false,
        )
        .with_context("path", path.to_string_lossy().to_string()));
    }
    if !metadata.is_dir() {
        return Err(AppError::new(
            "filesystem.not_directory",
            "an output path ancestor is not a directory",
            false,
        )
        .with_context("path", path.to_string_lossy().to_string()));
    }
    Ok(())
}

async fn allocate_renamed_target(target: &Path) -> Result<PathBuf> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let filename = target
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            AppError::new("output.invalid_filename", "invalid output filename", false)
        })?;
    let path = Path::new(filename);
    let extension = path.extension().and_then(|value| value.to_str());
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(filename);

    for index in 1..10_000 {
        let candidate_name = match extension {
            Some(extension) => format!("{stem} ({index}).{extension}"),
            None => format!("{stem} ({index})"),
        };
        let candidate = parent.join(candidate_name);
        match fs::symlink_metadata(&candidate).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => continue,
            Err(error) => {
                return Err(AppError::filesystem(
                    "inspect renamed output target",
                    &candidate,
                    &error,
                ));
            }
        }
    }
    Err(AppError::new(
        "output.name_exhausted",
        "could not allocate a unique output filename",
        false,
    ))
}

fn should_use_multipart(identity: &RemoteIdentity, options: &ExecuteOptions) -> bool {
    identity.supports_ranges
        && options.chunk_concurrency > 1
        && identity
            .total
            .is_some_and(|total| total >= options.multipart_threshold && total > options.chunk_size)
}

fn build_chunks(total: u64, chunk_size: u64) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut start = 0_u64;
    let mut index = 0_usize;
    while start < total {
        let end = start.saturating_add(chunk_size - 1).min(total - 1);
        chunks.push(Chunk { index, start, end });
        start = end + 1;
        index += 1;
    }
    chunks
}

fn chunk_path(parts_dir: &Path, chunk: Chunk) -> PathBuf {
    parts_dir.join(format!(
        "chunk_{:05}_{}_{}.part",
        chunk.index, chunk.start, chunk.end
    ))
}

async fn inspect_parts_directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AppError::filesystem(
            "inspect multipart directory",
            path,
            &error,
        )),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(AppError::new(
            "output.symlink",
            "refusing to use a symlink as multipart download state",
            false,
        )
        .with_context("path", path.to_string_lossy().to_string())),
        Ok(metadata) if !metadata.is_dir() => Err(AppError::new(
            "filesystem.invalid_state",
            "multipart state path is not a directory",
            false,
        )
        .with_context("path", path.to_string_lossy().to_string())),
        Ok(_) => Ok(true),
    }
}

async fn ensure_parts_directory(path: &Path) -> Result<()> {
    if inspect_parts_directory(path).await? {
        return Ok(());
    }
    fs::create_dir(path)
        .await
        .map_err(|error| AppError::filesystem("create multipart directory", path, &error))
}

async fn chunk_is_complete(parts_dir: &Path, chunk: Chunk) -> Result<bool> {
    let path = chunk_path(parts_dir, chunk);
    match fs::symlink_metadata(&path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AppError::filesystem(
            "inspect multipart chunk",
            &path,
            &error,
        )),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            remove_file_if_exists(&path).await?;
            Ok(false)
        }
        Ok(metadata) if !metadata.is_file() => Err(AppError::new(
            "filesystem.invalid_state",
            "multipart chunk path is not a regular file",
            false,
        )
        .with_context("path", path.to_string_lossy().to_string())),
        Ok(metadata) if metadata.len() == chunk.len() => Ok(true),
        Ok(_) => {
            remove_file_if_exists(&path).await?;
            Ok(false)
        }
    }
}

async fn concatenate_chunks(
    part_path: &Path,
    concat_path: &Path,
    parts_dir: &Path,
    chunks: &[Chunk],
    total: u64,
) -> Result<()> {
    remove_file_if_exists(concat_path).await?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(concat_path)
        .await
        .map_err(|error| AppError::filesystem("open multipart concat file", concat_path, &error))?;
    for chunk in chunks {
        let path = chunk_path(parts_dir, *chunk);
        let mut input = fs::File::open(&path)
            .await
            .map_err(|error| AppError::filesystem("open multipart chunk", &path, &error))?;
        let copied = tokio::io::copy(&mut input, &mut output)
            .await
            .map_err(|error| AppError::filesystem("concatenate multipart chunk", &path, &error))?;
        if copied != chunk.len() {
            return Err(AppError::new(
                "multipart.concat_mismatch",
                "multipart chunk changed during concatenation",
                true,
            ));
        }
    }
    output.flush().await.map_err(|error| {
        AppError::filesystem("flush multipart concat file", concat_path, &error)
    })?;
    output
        .sync_data()
        .await
        .map_err(|error| AppError::filesystem("sync multipart concat file", concat_path, &error))?;
    drop(output);
    let size = inspect_partial_file(concat_path).await?;
    if size != total {
        return Err(AppError::new(
            "multipart.concat_mismatch",
            format!("multipart concat size mismatch: got {size} of {total} bytes"),
            true,
        ));
    }
    remove_file_if_exists(part_path).await?;
    fs::rename(concat_path, part_path)
        .await
        .map_err(|error| AppError::filesystem("commit multipart download", part_path, &error))?;
    Ok(())
}

fn validate_resume_identity(
    metadata: &ResumeMetadata,
    task_id: &str,
    fingerprint: &str,
    source_url: &str,
    remote: &RemoteIdentity,
    part_size: u64,
) -> std::result::Result<(), SkipInfo> {
    let mismatch = || SkipInfo {
        code: "resume.identity_mismatch".to_owned(),
        message: "partial download does not match the current remote object".to_owned(),
    };
    if metadata.schema_version != RESUME_METADATA_VERSION
        || metadata.task_id != task_id
        || metadata.fingerprint != fingerprint
        || metadata.source_url != source_url
        || metadata.total.is_none()
        || metadata.total != remote.total
        || metadata.etag != remote.etag
        || metadata.last_modified != remote.last_modified
        || remote.total.is_some_and(|total| part_size > total)
    {
        return Err(mismatch());
    }
    Ok(())
}

fn archive_entry_task_id(task_id: &str, entry_id: &str) -> String {
    format!("{task_id}:{entry_id}")
}

fn archive_fingerprint(
    task_id: &str,
    relative_path: &str,
    entries: &[ArchiveEntry],
) -> Result<String> {
    fingerprint(&(task_id, relative_path, entries))
}

fn mux_track_task_id(task_id: &str, track_id: &str) -> String {
    format!("{task_id}:{track_id}")
}

fn mux_fingerprint(
    task_id: &str,
    relative_path: &str,
    format: MuxFormat,
    tracks: &[MuxTrack],
) -> Result<String> {
    fingerprint(&(task_id, relative_path, format, tracks))
}

fn source_fingerprint(
    task_id: &str,
    relative_path: &str,
    source: &DownloadSource,
) -> Result<String> {
    fingerprint(&(task_id, relative_path, source))
}

fn fingerprint(value: &impl Serialize) -> Result<String> {
    let encoded = serde_json::to_vec(value).map_err(|_| {
        AppError::new(
            "internal.fingerprint_failed",
            "failed to fingerprint download task",
            false,
        )
    })?;
    let digest = Sha256::digest(encoded);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(output)
}

async fn load_resume_metadata(path: &Path) -> Result<ResumeMetadataState> {
    match fs::symlink_metadata(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ResumeMetadataState::Missing);
        }
        Err(error) => {
            return Err(AppError::filesystem(
                "inspect resume metadata",
                path,
                &error,
            ));
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Ok(ResumeMetadataState::Invalid);
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(AppError::new(
                "filesystem.invalid_state",
                "resume metadata path is not a regular file",
                false,
            )
            .with_context("path", path.to_string_lossy().to_string()));
        }
        Ok(_) => {}
    }

    let bytes = fs::read(path)
        .await
        .map_err(|error| AppError::filesystem("read resume metadata", path, &error))?;
    Ok(match serde_json::from_slice(&bytes) {
        Ok(metadata) => ResumeMetadataState::Valid(metadata),
        Err(_) => ResumeMetadataState::Invalid,
    })
}

async fn write_resume_metadata(path: &Path, metadata: &ResumeMetadata) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(metadata).map_err(|_| {
        AppError::new(
            "internal.metadata_serialize_failed",
            "failed to serialize resume metadata",
            false,
        )
    })?;
    let temporary = append_suffix(path, ".tmp");
    remove_file_if_exists(&temporary).await?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .await
        .map_err(|error| AppError::filesystem("open resume metadata", &temporary, &error))?;
    file.write_all(&bytes)
        .await
        .map_err(|error| AppError::filesystem("write resume metadata", &temporary, &error))?;
    file.write_all(b"\n")
        .await
        .map_err(|error| AppError::filesystem("write resume metadata", &temporary, &error))?;
    file.flush()
        .await
        .map_err(|error| AppError::filesystem("flush resume metadata", &temporary, &error))?;
    file.sync_data()
        .await
        .map_err(|error| AppError::filesystem("sync resume metadata", &temporary, &error))?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|error| AppError::filesystem("secure resume metadata", &temporary, &error))?;
    }
    #[cfg(windows)]
    remove_file_if_exists(path).await?;

    fs::rename(&temporary, path)
        .await
        .map_err(|error| AppError::filesystem("commit resume metadata", path, &error))?;
    Ok(())
}

fn content_range(response: &Response) -> Option<ParsedContentRange> {
    parse_content_range(response.headers().get(CONTENT_RANGE)?.to_str().ok()?)
}

fn parse_content_range(value: &str) -> Option<ParsedContentRange> {
    let value = value.trim().strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    if total == "*" {
        return None;
    }
    let (start, end) = range.split_once('-')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    let total = total.parse().ok()?;
    if end < start || total == 0 || end >= total {
        return None;
    }
    Some(ParsedContentRange { start, end, total })
}

fn unsatisfied_total(response: &Response) -> Option<u64> {
    let value = response.headers().get(CONTENT_RANGE)?.to_str().ok()?.trim();
    value.strip_prefix("bytes */")?.parse().ok()
}

fn accepts_ranges(response: &Response) -> bool {
    response
        .headers()
        .get(ACCEPT_RANGES)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("bytes"))
}

fn header_text(headers: &HeaderMap, name: HeaderName) -> Option<&str> {
    headers.get(name)?.to_str().ok()
}

async fn inspect_partial_file(path: &Path) -> Result<u64> {
    match fs::symlink_metadata(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(AppError::filesystem("inspect partial file", path, &error)),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(AppError::new(
            "output.symlink",
            "refusing to use a symlink as partial download state",
            false,
        )
        .with_context("path", path.to_string_lossy().to_string())),
        Ok(metadata) if !metadata.is_file() => Err(AppError::new(
            "filesystem.invalid_state",
            "partial download path is not a regular file",
            false,
        )
        .with_context("path", path.to_string_lossy().to_string())),
        Ok(metadata) => Ok(metadata.len()),
    }
}

async fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AppError::filesystem("inspect download state", path, &error)),
    }
}

fn join_portable_path(root: &Path, relative_path: &str) -> PathBuf {
    relative_path
        .split('/')
        .fold(root.to_path_buf(), |path, component| path.join(component))
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value: OsString = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}

async fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::filesystem("remove directory", path, &error)),
    }
}

async fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::filesystem("remove file", path, &error)),
    }
}

fn build_headers(values: &std::collections::BTreeMap<String, String>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in values {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| AppError::invalid_plan(format!("invalid HTTP header name: {name}")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| AppError::invalid_plan(format!("invalid HTTP header value for {name}")))?;
        headers.insert(name, value);
    }
    Ok(headers)
}
