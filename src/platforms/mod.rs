pub mod bilibili;
pub mod douyin;
pub mod kuaishou;
pub mod xiaohongshu;

use std::{path::PathBuf, time::Duration};

use crate::{
    domain::{DownloadPlan, InspectDocument},
    errors::{AppError, Result},
};

use self::{
    bilibili::{BilibiliMedia, BilibiliResolver},
    douyin::{DouyinMedia, DouyinResolver},
    kuaishou::{KuaishouMedia, KuaishouResolver},
    xiaohongshu::{XiaohongshuMedia, XiaohongshuResolver},
};

#[derive(Debug, Clone)]
pub struct ResolveOptions {
    pub proxy: Option<String>,
    pub timeout: Duration,
    pub cookies_path: Option<PathBuf>,
    pub config_dir: PathBuf,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            proxy: None,
            timeout: Duration::from_secs(30),
            cookies_path: None,
            config_dir: crate::credentials::default_config_dir(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PlanSelection {
    pub quality: Option<String>,
    pub output: Option<String>,
    pub images: Option<Vec<u32>>,
    pub album_output: AlbumOutput,
    pub part: Option<u32>,
    pub all_parts: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AlbumOutput {
    #[default]
    Zip,
    Directory,
}

#[derive(Debug, Clone)]
pub enum ResolvedMedia {
    Bilibili(Box<BilibiliMedia>),
    Douyin(DouyinMedia),
    Kuaishou(KuaishouMedia),
    Xiaohongshu(XiaohongshuMedia),
}

impl ResolvedMedia {
    pub fn inspect(&self) -> InspectDocument {
        match self {
            Self::Bilibili(media) => media.inspect(),
            Self::Douyin(media) => media.inspect(),
            Self::Kuaishou(media) => media.inspect(),
            Self::Xiaohongshu(media) => media.inspect(),
        }
    }

    pub async fn create_plan(&self, selection: &PlanSelection) -> Result<DownloadPlan> {
        match self {
            Self::Bilibili(media) => media.create_plan(selection).await,
            Self::Douyin(media) => media.create_plan(selection),
            Self::Kuaishou(media) => media.create_plan(selection),
            Self::Xiaohongshu(media) => media.create_plan(selection),
        }
    }
}

pub async fn resolve(input: &str, options: ResolveOptions) -> Result<ResolvedMedia> {
    if xiaohongshu::matches(input) {
        return Ok(ResolvedMedia::Xiaohongshu(
            XiaohongshuResolver::new(options)?.resolve(input).await?,
        ));
    }
    if douyin::matches(input) {
        return Ok(ResolvedMedia::Douyin(
            DouyinResolver::new(options)?.resolve(input).await?,
        ));
    }
    if bilibili::matches(input) {
        return Ok(ResolvedMedia::Bilibili(Box::new(
            BilibiliResolver::new(options)?.resolve(input).await?,
        )));
    }
    if kuaishou::matches(input) || kuaishou::looks_like_bare_photo_id(input.trim()) {
        return Ok(ResolvedMedia::Kuaishou(
            KuaishouResolver::new(options)?.resolve(input).await?,
        ));
    }
    Err(AppError::new(
        "platform.unsupported",
        "could not detect a supported platform from the input",
        false,
    ))
}
