use std::sync::Arc;

use easydl::{
    domain::{
        DownloadEvent, DownloadPlan, DownloadSource, DownloadTask, OutputSpec, PlanSource,
        SCHEMA_VERSION,
    },
    downloader::{ExecuteOptions, execute_plan},
};
use tempfile::tempdir;
use time::OffsetDateTime;

#[tokio::test]
async fn reports_missing_local_credentials_without_contacting_the_origin() {
    let output = tempdir().unwrap();
    let plan = DownloadPlan {
        schema_version: SCHEMA_VERSION,
        created_at: OffsetDateTime::now_utc(),
        expires_at: None,
        source: PlanSource {
            platform: "bilibili".to_owned(),
            media_id: "auth".to_owned(),
        },
        tasks: vec![DownloadTask::File {
            id: "auth".to_owned(),
            sources: vec![
                authenticated_source("https://upos-sz-mirrorcos.bilivideo.com/video.m4s"),
                authenticated_source("https://upos-sz-mirrorali.bilivideo.com/video.m4s"),
            ],
            output: OutputSpec {
                relative_path: "video.m4s".to_owned(),
            },
        }],
    };

    let summary = execute_plan(
        plan,
        ExecuteOptions {
            output_root: output.path().to_path_buf(),
            config_dir: output.path().join("config"),
            ..ExecuteOptions::default()
        },
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();

    assert!(!summary.success);
    assert_eq!(summary.failures.len(), 1);
    assert_eq!(summary.failures[0].error.code, "auth.required");
    assert_eq!(
        summary.failures[0].error.context["authRef"],
        "bilibili:default"
    );
    assert_eq!(summary.failures[0].error.context["sourcesTried"], 2);
    assert!(!output.path().join("video.m4s").exists());
}

fn authenticated_source(url: &str) -> DownloadSource {
    DownloadSource {
        url: url.to_owned(),
        headers: Default::default(),
        auth_ref: Some("bilibili:default".to_owned()),
    }
}
