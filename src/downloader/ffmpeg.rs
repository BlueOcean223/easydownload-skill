use std::{
    env, io,
    path::{Path, PathBuf},
    process::Stdio,
};

use tokio::{
    fs,
    io::AsyncReadExt,
    process::{ChildStderr, Command},
};

use crate::{
    domain::{MuxFormat, MuxTrackKind},
    errors::{AppError, Result},
};

const STDERR_LIMIT: usize = 8 * 1024;

#[derive(Debug)]
pub(super) struct MuxInput {
    pub kind: MuxTrackKind,
    pub path: PathBuf,
}

pub(super) async fn ensure_available(ffmpeg: &Path) -> Result<()> {
    if is_executable_present(ffmpeg).await {
        return Ok(());
    }
    Err(
        AppError::new("ffmpeg.not_found", "ffmpeg executable was not found", false)
            .with_context("path", ffmpeg.to_string_lossy().to_string()),
    )
}

pub(super) async fn mux(
    ffmpeg: &Path,
    format: MuxFormat,
    target: &Path,
    inputs: Vec<MuxInput>,
) -> Result<()> {
    let part_path = append_suffix(target, ".part");
    remove_file_if_exists(&part_path).await?;
    for input in &inputs {
        let metadata = fs::symlink_metadata(&input.path)
            .await
            .map_err(|error| AppError::filesystem("inspect mux track", &input.path, &error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(AppError::new(
                "mux.invalid_track_state",
                "mux track is not a regular file",
                false,
            )
            .with_context("path", input.path.to_string_lossy().to_string()));
        }
    }

    let mut command = Command::new(ffmpeg);
    command.arg("-hide_banner").arg("-nostdin").arg("-y");
    for input in &inputs {
        command.arg("-i").arg(&input.path);
    }
    for (index, input) in inputs.iter().enumerate() {
        let stream = match input.kind {
            MuxTrackKind::Video => "v:0",
            MuxTrackKind::Audio => "a:0",
        };
        command.arg("-map").arg(format!("{index}:{stream}"));
    }
    command.arg("-c").arg("copy");
    if format == MuxFormat::Mp4 {
        command.arg("-movflags").arg("+faststart");
    }
    command
        .arg("-f")
        .arg(format_name(format))
        .arg(&part_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().map_err(|error| {
        let code = if error.kind() == io::ErrorKind::NotFound {
            "ffmpeg.not_found"
        } else {
            "ffmpeg.start_failed"
        };
        AppError::new(
            code,
            if error.kind() == io::ErrorKind::NotFound {
                "ffmpeg executable was not found"
            } else {
                "failed to start ffmpeg"
            },
            false,
        )
        .with_context("path", ffmpeg.to_string_lossy().to_string())
    })?;
    let stderr = child.stderr.take();
    let stderr_task = stderr.map(|stderr| tokio::spawn(collect_stderr(stderr)));
    let status = child.wait().await.map_err(|error| {
        AppError::new(
            "ffmpeg.wait_failed",
            "failed while waiting for ffmpeg",
            false,
        )
        .with_context("path", ffmpeg.to_string_lossy().to_string())
        .with_context("detail", error.to_string())
    })?;
    let stderr = match stderr_task {
        Some(task) => task
            .await
            .map_err(|error| {
                AppError::new(
                    "ffmpeg.stderr_failed",
                    "failed while reading ffmpeg diagnostics",
                    false,
                )
                .with_context("detail", error.to_string())
            })?
            .map_err(|error| {
                AppError::new(
                    "ffmpeg.stderr_failed",
                    "failed while reading ffmpeg diagnostics",
                    false,
                )
                .with_context("detail", error.to_string())
            })?,
        None => String::new(),
    };

    if !status.success() {
        let mut error = AppError::new(
            "ffmpeg.failed",
            if stderr.is_empty() {
                "ffmpeg failed".to_owned()
            } else {
                format!("ffmpeg failed: {stderr}")
            },
            false,
        )
        .with_context("path", ffmpeg.to_string_lossy().to_string());
        if let Some(code) = status.code() {
            error = error.with_context("exitCode", code);
        }
        #[cfg(unix)]
        if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
            error = error.with_context("signal", signal);
        }
        let _ = remove_file_if_exists(&part_path).await;
        return Err(error);
    }

    let metadata = fs::symlink_metadata(&part_path)
        .await
        .map_err(|error| AppError::filesystem("inspect ffmpeg output", &part_path, &error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        let _ = remove_file_if_exists(&part_path).await;
        return Err(AppError::new(
            "ffmpeg.output_missing",
            "ffmpeg exited successfully but did not produce a regular output file",
            false,
        )
        .with_context("path", part_path.to_string_lossy().to_string()));
    }
    let output = fs::File::open(&part_path)
        .await
        .map_err(|error| AppError::filesystem("open ffmpeg output", &part_path, &error))?;
    output
        .sync_data()
        .await
        .map_err(|error| AppError::filesystem("sync ffmpeg output", &part_path, &error))?;
    drop(output);

    remove_file_if_exists(target).await?;
    fs::rename(&part_path, target)
        .await
        .map_err(|error| AppError::filesystem("finalize muxed output", target, &error))?;
    Ok(())
}

async fn collect_stderr(mut stderr: ChildStderr) -> io::Result<String> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stderr.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..read]);
        if output.len() > STDERR_LIMIT {
            let trim = output.len() - STDERR_LIMIT;
            output.drain(..trim);
        }
    }
    Ok(String::from_utf8_lossy(&output).trim().to_owned())
}

fn format_name(format: MuxFormat) -> &'static str {
    match format {
        MuxFormat::Mp4 => "mp4",
        MuxFormat::Matroska => "matroska",
    }
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
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

async fn is_executable_present(ffmpeg: &Path) -> bool {
    if ffmpeg.components().count() > 1 {
        return file_exists_with_pathext(ffmpeg, false).await;
    }
    let Some(path_env) = env::var_os("PATH") else {
        return false;
    };
    for directory in env::split_paths(&path_env) {
        if file_exists_with_pathext(&directory.join(ffmpeg), true).await {
            return true;
        }
    }
    false
}

async fn file_exists_with_pathext(candidate: &Path, require_executable: bool) -> bool {
    if is_usable_file(candidate, require_executable).await {
        return true;
    }
    #[cfg(windows)]
    if candidate.extension().is_none() {
        let pathext = env::var_os("PATHEXT")
            .unwrap_or_else(|| std::ffi::OsString::from(".COM;.EXE;.BAT;.CMD"));
        for ext in pathext.to_string_lossy().split(';') {
            let ext = ext.trim();
            if ext.is_empty() {
                continue;
            }
            let with_ext = append_suffix(candidate, ext);
            if is_usable_file(&with_ext, require_executable).await {
                return true;
            }
        }
    }
    false
}

async fn is_usable_file(path: &Path, require_executable: bool) -> bool {
    let Ok(metadata) = fs::metadata(path).await else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    if require_executable {
        use std::os::unix::fs::PermissionsExt;
        return metadata.permissions().mode() & 0o111 != 0;
    }
    let _ = require_executable;
    true
}
