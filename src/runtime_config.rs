use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    errors::{AppError, Result},
    private_fs,
};

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ffmpeg: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_concurrency: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeConfigKey {
    OutputDir,
    Ffmpeg,
    Concurrency,
    ChunkConcurrency,
}

impl RuntimeConfig {
    pub(crate) async fn load(config_dir: &Path) -> Result<Self> {
        let path = config_dir.join("config.json");
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(AppError::filesystem("read runtime config", &path, &error)),
        };
        let config: Self = serde_json::from_str(&text).map_err(|error| {
            AppError::new(
                "config.invalid",
                format!("invalid runtime config: {error}"),
                false,
            )
            .with_context("path", path.to_string_lossy().to_string())
        })?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) async fn set(config_dir: &Path, key: RuntimeConfigKey, value: &str) -> Result<Self> {
        let mut config = Self::load(config_dir).await?;
        match key {
            RuntimeConfigKey::OutputDir => {
                config.output_dir = Some(parse_path(value, "outputDir")?);
            }
            RuntimeConfigKey::Ffmpeg => {
                config.ffmpeg = Some(parse_path(value, "ffmpeg")?);
            }
            RuntimeConfigKey::Concurrency => {
                config.concurrency = Some(parse_positive_usize(value, "concurrency")?);
            }
            RuntimeConfigKey::ChunkConcurrency => {
                config.chunk_concurrency = Some(parse_positive_usize(value, "chunkConcurrency")?);
            }
        }
        config.write(config_dir).await?;
        Ok(config)
    }

    async fn write(&self, config_dir: &Path) -> Result<()> {
        self.validate()?;
        let path = config_dir.join("config.json");
        let mut bytes = serde_json::to_vec_pretty(self).map_err(|error| {
            AppError::new(
                "config.invalid",
                format!("failed to serialize runtime config: {error}"),
                false,
            )
        })?;
        bytes.push(b'\n');
        private_fs::atomic_write_private(&path, &bytes).await
    }

    fn validate(&self) -> Result<()> {
        if self.concurrency == Some(0) || self.chunk_concurrency == Some(0) {
            return Err(AppError::new(
                "config.invalid",
                "configured concurrency values must be positive",
                false,
            ));
        }
        if self
            .output_dir
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
            || self
                .ffmpeg
                .as_ref()
                .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(AppError::new(
                "config.invalid",
                "configured paths must not be empty",
                false,
            ));
        }
        Ok(())
    }
}

fn parse_path(value: &str, key: &str) -> Result<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AppError::new(
            "config.invalid",
            format!("{key} must not be empty"),
            false,
        ));
    }
    Ok(PathBuf::from(value))
}

fn parse_positive_usize(value: &str, key: &str) -> Result<usize> {
    value
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            AppError::new(
                "config.invalid",
                format!("{key} must be a positive integer"),
                false,
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{RuntimeConfig, RuntimeConfigKey};
    use tempfile::tempdir;

    #[tokio::test]
    async fn atomically_updates_legacy_runtime_config_with_private_permissions() {
        let directory = tempdir().unwrap();
        tokio::fs::write(
            directory.path().join("config.json"),
            r#"{"outputDir":"downloads","concurrency":2}"#,
        )
        .await
        .unwrap();
        let config = RuntimeConfig::set(directory.path(), RuntimeConfigKey::ChunkConcurrency, "3")
            .await
            .unwrap();
        assert_eq!(config.output_dir.unwrap().to_string_lossy(), "downloads");
        assert_eq!(config.concurrency, Some(2));
        assert_eq!(config.chunk_concurrency, Some(3));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                tokio::fs::metadata(directory.path().join("config.json"))
                    .await
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(std::fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[tokio::test]
    async fn rejects_zero_concurrency_without_replacing_existing_config() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.json");
        tokio::fs::write(&path, r#"{"concurrency":2}"#)
            .await
            .unwrap();
        assert_eq!(
            RuntimeConfig::set(directory.path(), RuntimeConfigKey::Concurrency, "0")
                .await
                .unwrap_err()
                .info
                .code,
            "config.invalid"
        );
        assert_eq!(
            RuntimeConfig::load(directory.path())
                .await
                .unwrap()
                .concurrency,
            Some(2)
        );
    }
}
