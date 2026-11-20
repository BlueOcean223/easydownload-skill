use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::{fs, io::AsyncWriteExt, task};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::errors::{AppError, Result};

const ARCHIVE_METADATA_VERSION: u32 = 2;

#[derive(Debug)]
pub(super) struct ArchiveFile {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveMetadata {
    schema_version: u32,
    task_id: String,
    fingerprint: String,
    failed_entries: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

pub(super) async fn partial_state_matches(
    path: &Path,
    task_id: &str,
    fingerprint: &str,
) -> Result<bool> {
    match fs::symlink_metadata(path).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(AppError::filesystem(
                "inspect archive metadata",
                path,
                &error,
            ));
        }
        Ok(metadata) if metadata.file_type().is_symlink() => return Ok(false),
        Ok(metadata) if !metadata.is_file() => {
            return Err(AppError::new(
                "filesystem.invalid_state",
                "archive metadata path is not a regular file",
                false,
            )
            .with_context("path", path.to_string_lossy().to_string()));
        }
        Ok(_) => {}
    };
    let bytes = fs::read(path)
        .await
        .map_err(|error| AppError::filesystem("read archive metadata", path, &error))?;
    let metadata: ArchiveMetadata = match serde_json::from_slice(&bytes) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(false),
    };
    Ok(metadata.schema_version == ARCHIVE_METADATA_VERSION
        && metadata.task_id == task_id
        && metadata.fingerprint == fingerprint
        && !metadata.failed_entries.is_empty())
}

pub(super) async fn mark_partial(
    path: &Path,
    task_id: &str,
    fingerprint: &str,
    failed_entries: Vec<String>,
) -> Result<()> {
    let metadata = ArchiveMetadata {
        schema_version: ARCHIVE_METADATA_VERSION,
        task_id: task_id.to_owned(),
        fingerprint: fingerprint.to_owned(),
        failed_entries,
        updated_at: OffsetDateTime::now_utc(),
    };
    let bytes = serde_json::to_vec_pretty(&metadata).map_err(|_| {
        AppError::new(
            "internal.metadata_serialize_failed",
            "failed to serialize archive metadata",
            false,
        )
    })?;
    let temporary = append_suffix(path, ".tmp");
    remove_file_if_exists(&temporary).await?;

    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .await
        .map_err(|error| AppError::filesystem("open archive metadata", &temporary, &error))?;
    file.write_all(&bytes)
        .await
        .map_err(|error| AppError::filesystem("write archive metadata", &temporary, &error))?;
    file.write_all(b"\n")
        .await
        .map_err(|error| AppError::filesystem("write archive metadata", &temporary, &error))?;
    file.flush()
        .await
        .map_err(|error| AppError::filesystem("flush archive metadata", &temporary, &error))?;
    file.sync_data()
        .await
        .map_err(|error| AppError::filesystem("sync archive metadata", &temporary, &error))?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|error| AppError::filesystem("secure archive metadata", &temporary, &error))?;
    }
    #[cfg(windows)]
    remove_file_if_exists(path).await?;

    fs::rename(&temporary, path)
        .await
        .map_err(|error| AppError::filesystem("commit archive metadata", path, &error))?;
    Ok(())
}

pub(super) async fn write_zip_archive(target: &Path, entries: Vec<ArchiveFile>) -> Result<()> {
    let part_path = append_suffix(target, ".part");
    let temporary = append_suffix(&part_path, ".ziptmp");
    remove_file_if_exists(&temporary).await?;

    let temporary_for_writer = temporary.clone();
    task::spawn_blocking(move || write_zip_sync(&temporary_for_writer, entries))
        .await
        .map_err(|error| {
            AppError::new(
                "archive.worker_failed",
                format!("archive writer task failed: {error}"),
                false,
            )
        })??;

    remove_file_if_exists(&part_path).await?;
    fs::rename(&temporary, &part_path)
        .await
        .map_err(|error| AppError::filesystem("commit partial archive", &part_path, &error))?;
    remove_file_if_exists(target).await?;
    fs::rename(&part_path, target)
        .await
        .map_err(|error| AppError::filesystem("finalize archive", target, &error))?;
    Ok(())
}

fn write_zip_sync(path: &Path, entries: Vec<ArchiveFile>) -> Result<()> {
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| AppError::filesystem("open archive output", path, &error))?;
    let mut archive = ZipWriter::new(output);

    for entry in entries {
        let metadata = std::fs::symlink_metadata(&entry.path)
            .map_err(|error| AppError::filesystem("inspect archive entry", &entry.path, &error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(AppError::new(
                "archive.invalid_entry_state",
                "archive entry source is not a regular file",
                false,
            )
            .with_context("entryPath", entry.name)
            .with_context("path", entry.path.to_string_lossy().to_string()));
        }
        let mut input = File::open(&entry.path)
            .map_err(|error| AppError::filesystem("open archive entry", &entry.path, &error))?;
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .unix_permissions(0o644)
            .large_file(metadata.len() > u32::MAX as u64);
        archive
            .start_file(&entry.name, options)
            .map_err(|error| archive_error("start archive entry", Some(&entry.name), error))?;
        io::copy(&mut input, &mut archive).map_err(|error| {
            AppError::filesystem("write archive entry", &entry.path, &error)
                .with_context("entryPath", entry.name.clone())
        })?;
    }

    let output = archive
        .finish()
        .map_err(|error| archive_error("finish archive", None, error))?;
    output
        .sync_all()
        .map_err(|error| AppError::filesystem("sync archive output", path, &error))?;
    Ok(())
}

fn archive_error(action: &str, entry: Option<&str>, error: zip::result::ZipError) -> AppError {
    let mut error = AppError::new(
        "archive.write_failed",
        format!("failed to {action}: {error}"),
        false,
    )
    .with_context("action", action);
    if let Some(entry) = entry {
        error = error.with_context("entryPath", entry);
    }
    error
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value: OsString = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}

async fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::filesystem("remove file", path, &error)),
    }
}
