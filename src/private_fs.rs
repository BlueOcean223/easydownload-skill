use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use tokio::io::AsyncWriteExt;

use crate::errors::{AppError, Result};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) async fn create_private_dir(path: &Path) -> Result<()> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|error| AppError::filesystem("create private directory", path, &error))?;
    set_private_directory_permissions(path).await
}

pub(crate) async fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_private_dir(parent).await?;
    let temporary = temporary_path(path);
    let result = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary).await.map_err(|error| {
            AppError::filesystem("create temporary private file", &temporary, &error)
        })?;
        set_private_file_permissions(&temporary).await?;
        file.write_all(bytes)
            .await
            .map_err(|error| AppError::filesystem("write private file", &temporary, &error))?;
        file.flush()
            .await
            .map_err(|error| AppError::filesystem("flush private file", &temporary, &error))?;
        file.sync_all()
            .await
            .map_err(|error| AppError::filesystem("sync private file", &temporary, &error))?;
        drop(file);
        atomic_replace(&temporary, path).await?;
        sync_parent_directory(parent).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

fn temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_else(|| "private".into());
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), sequence))
}

#[cfg(unix)]
async fn atomic_replace(from: &Path, to: &Path) -> Result<()> {
    tokio::fs::rename(from, to)
        .await
        .map_err(|error| AppError::filesystem("commit private file", to, &error))
}

#[cfg(windows)]
async fn atomic_replace(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let from = from
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let to_path = to.to_path_buf();
    let to = to
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        let result = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if result == 0 {
            Err(AppError::filesystem(
                "commit private file",
                &to_path,
                &std::io::Error::last_os_error(),
            ))
        } else {
            Ok(())
        }
    })
    .await
    .map_err(|error| {
        AppError::new(
            "filesystem.task_failed",
            format!("private file commit task failed: {error}"),
            true,
        )
    })?
}

#[cfg(not(any(unix, windows)))]
async fn atomic_replace(from: &Path, to: &Path) -> Result<()> {
    tokio::fs::rename(from, to)
        .await
        .map_err(|error| AppError::filesystem("commit private file", to, &error))
}

#[cfg(unix)]
async fn sync_parent_directory(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::fs::File::open(&path)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| AppError::filesystem("sync private directory", &path, &error))
    })
    .await
    .map_err(|error| {
        AppError::new(
            "filesystem.task_failed",
            format!("private directory sync task failed: {error}"),
            true,
        )
    })?
}

#[cfg(not(unix))]
async fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
async fn set_private_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|error| AppError::filesystem("secure private directory", path, &error))
}

#[cfg(not(unix))]
async fn set_private_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
async fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(|error| AppError::filesystem("secure private file", path, &error))
}

#[cfg(not(unix))]
async fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}
