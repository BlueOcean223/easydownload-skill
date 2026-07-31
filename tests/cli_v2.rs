use std::{
    fs,
    process::{Command, Stdio},
};

use axum::{Router, routing::get};
use easydl::domain::{
    DownloadPlan, DownloadSource, DownloadTask, OutputSpec, PlanSource, SCHEMA_VERSION,
};
use tempfile::tempdir;
use time::OffsetDateTime;
use tokio::{net::TcpListener, task::JoinHandle};

#[test]
fn help_exposes_the_v2_command_model() {
    let output = Command::new(env!("CARGO_BIN_EXE_easydl"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    for command in ["inspect", "plan", "execute", "download"] {
        assert!(stdout.contains(command), "help should mention {command}");
    }
    assert!(stdout.contains("--cookies"));

    let execute = Command::new(env!("CARGO_BIN_EXE_easydl"))
        .args(["execute", "--help"])
        .output()
        .unwrap();
    assert!(execute.status.success());
    let execute_help = String::from_utf8(execute.stdout).unwrap();
    assert!(execute_help.contains("--ffmpeg"));
    assert!(execute_help.contains("--bilibili-auth"));
    assert!(execute_help.contains("--auth-session"));
    assert!(execute_help.contains("--config"));
    assert!(execute_help.contains("--config-key"));

    let plan = Command::new(env!("CARGO_BIN_EXE_easydl"))
        .args(["plan", "--help"])
        .output()
        .unwrap();
    assert!(plan.status.success());
    let plan_help = String::from_utf8(plan.stdout).unwrap();
    assert!(plan_help.contains("--images"));
    assert!(plan_help.contains("--album-output"));
    assert!(plan_help.contains("--part"));
    assert!(plan_help.contains("--all-parts"));
}

#[test]
fn bilibili_auth_actions_validate_action_specific_arguments() {
    let missing_session = Command::new(env!("CARGO_BIN_EXE_easydl"))
        .args(["execute", "--bilibili-auth", "poll", "--json"])
        .output()
        .unwrap();
    assert!(!missing_session.status.success());
    let error: serde_json::Value = serde_json::from_slice(&missing_session.stderr).unwrap();
    assert_eq!(error["error"]["code"], "cli.invalid_arguments");

    let download_option = Command::new(env!("CARGO_BIN_EXE_easydl"))
        .args([
            "execute",
            "--bilibili-auth",
            "status",
            "--output-dir",
            "ignored",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!download_option.status.success());
    let error: serde_json::Value = serde_json::from_slice(&download_option.stderr).unwrap();
    assert_eq!(error["error"]["code"], "cli.invalid_arguments");
}

#[test]
fn plan_rejects_invalid_image_selection_before_resolving_input() {
    let output = Command::new(env!("CARGO_BIN_EXE_easydl"))
        .args(["plan", "3xpic123", "--images", "0"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["code"], "selection.images_invalid");
}

#[test]
fn runtime_config_get_and_set_use_atomic_private_storage() {
    let workspace = tempdir().unwrap();
    let set = Command::new(env!("CARGO_BIN_EXE_easydl"))
        .args([
            "execute",
            "--config",
            "set",
            "--config-key",
            "output-dir",
            "--config-value",
            "configured-downloads",
            "--json",
        ])
        .env("EASYDL_CONFIG_DIR", workspace.path())
        .output()
        .unwrap();
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&set.stdout).unwrap();
    assert_eq!(value["outputDir"], "configured-downloads");

    let set_concurrency = Command::new(env!("CARGO_BIN_EXE_easydl"))
        .args([
            "execute",
            "--config",
            "set",
            "--config-key",
            "concurrency",
            "--config-value",
            "3",
            "--json",
        ])
        .env("EASYDL_CONFIG_DIR", workspace.path())
        .output()
        .unwrap();
    assert!(set_concurrency.status.success());
    let value: serde_json::Value = serde_json::from_slice(&set_concurrency.stdout).unwrap();
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/v2/runtime-config.json")).unwrap();
    assert_eq!(value, expected);

    let get = Command::new(env!("CARGO_BIN_EXE_easydl"))
        .args(["execute", "--config", "get", "--json"])
        .env("EASYDL_CONFIG_DIR", workspace.path())
        .output()
        .unwrap();
    assert!(get.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&get.stdout).unwrap(),
        value
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(workspace.path().join("config.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_config_uses_cli_then_env_then_file_precedence() {
    let server = FileServer::start().await;
    let workspace = tempdir().unwrap();
    let config_dir = workspace.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let config_output = workspace.path().join("from-config");
    fs::write(
        config_dir.join("config.json"),
        serde_json::to_vec(&serde_json::json!({
            "outputDir": config_output,
            "concurrency": 1,
            "chunkConcurrency": 1
        }))
        .unwrap(),
    )
    .unwrap();
    let plan_path = workspace.path().join("config-plan.json");
    fs::write(
        &plan_path,
        serde_json::to_vec(&fixture_plan(&server.base)).unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_easydl"))
        .args(["execute", plan_path.to_str().unwrap(), "--json"])
        .env("EASYDL_CONFIG_DIR", &config_dir)
        .env_remove("EASYDL_OUTPUT_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(config_output.join("hello.txt")).unwrap(), b"hello");

    let env_output = workspace.path().join("from-env");
    let output = Command::new(env!("CARGO_BIN_EXE_easydl"))
        .args(["execute", plan_path.to_str().unwrap(), "--json"])
        .env("EASYDL_CONFIG_DIR", &config_dir)
        .env("EASYDL_OUTPUT_DIR", &env_output)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(env_output.join("hello.txt")).unwrap(), b"hello");

    let cli_output = workspace.path().join("from-cli");
    let output = Command::new(env!("CARGO_BIN_EXE_easydl"))
        .args([
            "execute",
            plan_path.to_str().unwrap(),
            "--output-dir",
            cli_output.to_str().unwrap(),
            "--json",
        ])
        .env("EASYDL_CONFIG_DIR", &config_dir)
        .env("EASYDL_OUTPUT_DIR", &env_output)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(cli_output.join("hello.txt")).unwrap(), b"hello");

    fs::write(config_dir.join("config.json"), b"not-json").unwrap();
    let full_cli_output = workspace.path().join("all-cli-overrides");
    let output = Command::new(env!("CARGO_BIN_EXE_easydl"))
        .args([
            "execute",
            plan_path.to_str().unwrap(),
            "--output-dir",
            full_cli_output.to_str().unwrap(),
            "--ffmpeg",
            "unused-ffmpeg",
            "--concurrency",
            "1",
            "--chunk-concurrency",
            "1",
            "--json",
        ])
        .env("EASYDL_CONFIG_DIR", &config_dir)
        .env("EASYDL_OUTPUT_DIR", &env_output)
        .env("EASYDL_CONCURRENCY", "invalid-lower-precedence-value")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(full_cli_output.join("hello.txt")).unwrap(),
        b"hello"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_command_supports_json_and_ndjson() {
    let server = FileServer::start().await;
    let workspace = tempdir().unwrap();
    let first_output = workspace.path().join("first");
    let plan_path = workspace.path().join("plan.json");
    let plan = DownloadPlan {
        schema_version: SCHEMA_VERSION,
        created_at: OffsetDateTime::now_utc(),
        expires_at: None,
        source: PlanSource {
            platform: "fixture".to_owned(),
            media_id: "hello".to_owned(),
        },
        tasks: vec![DownloadTask::File {
            id: "hello".to_owned(),
            sources: vec![DownloadSource {
                url: format!("{}/hello.txt", server.base),
                headers: Default::default(),
                auth_ref: None,
            }],
            output: OutputSpec {
                relative_path: "hello.txt".to_owned(),
            },
        }],
    };
    fs::write(&plan_path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_easydl"))
        .args([
            "execute",
            plan_path.to_str().unwrap(),
            "--output-dir",
            first_output.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["schemaVersion"], 2);
    assert_eq!(summary["success"], true);
    assert_eq!(fs::read(first_output.join("hello.txt")).unwrap(), b"hello");

    let second_output = workspace.path().join("second");
    let output = Command::new(env!("CARGO_BIN_EXE_easydl"))
        .args([
            "execute",
            plan_path.to_str().unwrap(),
            "--output-dir",
            second_output.to_str().unwrap(),
            "--json-progress",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = String::from_utf8(output.stdout).unwrap();
    let events: Vec<serde_json::Value> = lines
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.first().unwrap()["type"], "start");
    assert_eq!(events.last().unwrap()["type"], "done");
    assert_eq!(events.last().unwrap()["success"], true);
}

fn fixture_plan(base: &str) -> DownloadPlan {
    DownloadPlan {
        schema_version: SCHEMA_VERSION,
        created_at: OffsetDateTime::now_utc(),
        expires_at: None,
        source: PlanSource {
            platform: "fixture".to_owned(),
            media_id: "hello".to_owned(),
        },
        tasks: vec![DownloadTask::File {
            id: "hello".to_owned(),
            sources: vec![DownloadSource {
                url: format!("{base}/hello.txt"),
                headers: Default::default(),
                auth_ref: None,
            }],
            output: OutputSpec {
                relative_path: "hello.txt".to_owned(),
            },
        }],
    }
}

struct FileServer {
    base: String,
    task: JoinHandle<()>,
}

impl FileServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/hello.txt", get(|| async { "hello" }));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, task }
    }
}

impl Drop for FileServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
