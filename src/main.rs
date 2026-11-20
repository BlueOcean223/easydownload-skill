use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    easydl::cli::entry().await
}
