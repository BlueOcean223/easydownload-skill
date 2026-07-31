use std::sync::{Arc, Mutex};

use easydl::{
    domain::{
        DownloadEvent, DownloadPlan, DownloadSource, DownloadTask, OutputSpec, PlanSource,
        SCHEMA_VERSION,
    },
    downloader::{ExecuteOptions, execute_plan},
};
use tempfile::tempdir;
use time::OffsetDateTime;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

#[tokio::test]
async fn routes_http_downloads_through_the_runtime_proxy() {
    let proxy = FakeProxy::start(b"proxied-body").await;
    let output = tempdir().unwrap();

    let summary = execute_plan(
        file_plan("http://origin.invalid/media.bin"),
        ExecuteOptions {
            output_root: output.path().to_path_buf(),
            proxy: Some(proxy.url()),
            chunk_concurrency: 1,
            ..ExecuteOptions::default()
        },
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();

    assert!(summary.success);
    assert_eq!(
        tokio::fs::read(output.path().join("media.bin"))
            .await
            .unwrap(),
        b"proxied-body"
    );
    let request = proxy.request();
    assert!(request.starts_with("GET http://origin.invalid/media.bin HTTP/1.1\r\n"));
    assert!(!request.to_ascii_lowercase().contains("proxy-authorization"));
}

#[tokio::test]
async fn rejects_invalid_runtime_proxy_urls() {
    let output = tempdir().unwrap();
    let error = execute_plan(
        file_plan("http://origin.invalid/media.bin"),
        ExecuteOptions {
            output_root: output.path().to_path_buf(),
            proxy: Some("://invalid-proxy".to_owned()),
            ..ExecuteOptions::default()
        },
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap_err();

    assert_eq!(error.info.code, "proxy.invalid");
}

fn file_plan(url: &str) -> DownloadPlan {
    DownloadPlan {
        schema_version: SCHEMA_VERSION,
        created_at: OffsetDateTime::now_utc(),
        expires_at: None,
        source: PlanSource {
            platform: "fixture".to_owned(),
            media_id: "proxy".to_owned(),
        },
        tasks: vec![DownloadTask::File {
            id: "proxy".to_owned(),
            sources: vec![DownloadSource {
                url: url.to_owned(),
                headers: Default::default(),
                auth_ref: None,
            }],
            output: OutputSpec {
                relative_path: "media.bin".to_owned(),
            },
        }],
    }
}

struct FakeProxy {
    address: std::net::SocketAddr,
    request: Arc<Mutex<String>>,
    task: JoinHandle<()>,
}

impl FakeProxy {
    async fn start(body: &'static [u8]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let request = Arc::new(Mutex::new(String::new()));
        let captured = Arc::clone(&request);
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
            }
            *captured.lock().unwrap() = String::from_utf8_lossy(&bytes).into_owned();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        Self {
            address,
            request,
            task,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn request(&self) -> String {
        self.request.lock().unwrap().clone()
    }
}

impl Drop for FakeProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}
