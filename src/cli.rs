use std::{
    collections::BTreeSet,
    env,
    io::{self, Read, Write},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::{
    bilibili_auth::BilibiliAuthClient,
    domain::{DownloadEvent, DownloadPlan, DownloadSummary},
    downloader::{CollisionPolicy, EventSink, ExecuteOptions, NoopEventSink, execute_plan},
    errors::{AppError, ErrorDocument, Result},
    platforms::{
        AlbumOutput, PlanSelection, ResolveOptions, ResolvedMedia,
        kuaishou::{self, KuaishouResolver},
    },
};

#[derive(Debug, Parser)]
#[command(
    name = "easydl",
    version,
    about = "Agent-friendly media downloader CLI",
    long_about = None
)]
pub struct Cli {
    #[arg(long, global = true, value_name = "URL", help = "HTTP/SOCKS proxy URL")]
    pub proxy: Option<String>,
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Raw Cookie header or Netscape cookies.txt path"
    )]
    pub cookies: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(about = "Inspect safe media metadata and available selectors")]
    Inspect(InspectArgs),
    #[command(about = "Resolve input into a short-lived DownloadPlan v2 JSON document")]
    Plan(PlanArgs),
    #[command(about = "Execute a DownloadPlan v2 or manage local auth/config")]
    Execute(ExecuteArgs),
    #[command(about = "Resolve and download input in one command")]
    Download(DownloadArgs),
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    #[arg(value_name = "INPUT")]
    pub input: String,
    #[arg(long, help = "Print Inspect v2 JSON")]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct PlanArgs {
    #[arg(value_name = "INPUT")]
    pub input: String,
    #[arg(long, default_value = "best")]
    pub quality: String,
    #[arg(long, value_name = "N", value_parser = parse_positive_u32)]
    pub part: Option<u32>,
    #[arg(long, conflicts_with = "part")]
    pub all_parts: bool,
    #[arg(long, value_name = "SELECTION", help = "Images, e.g. 1,3-5")]
    pub images: Option<String>,
    #[arg(long, value_enum, default_value = "zip")]
    pub album_output: AlbumOutputArg,
    #[arg(long, value_name = "RELATIVE_PATH")]
    pub output: Option<String>,
}

#[derive(Debug, Args)]
pub struct ExecuteArgs {
    #[arg(
        value_name = "PLAN",
        help = "Plan JSON path, or - for stdin",
        required_unless_present_any = ["bilibili_auth", "runtime_config"],
        conflicts_with_all = ["bilibili_auth", "runtime_config"]
    )]
    pub plan: Option<String>,
    #[arg(
        long,
        value_name = "ACTION",
        value_enum,
        help = "Bilibili credential action: create, poll, wait, status, or logout",
        conflicts_with = "runtime_config"
    )]
    pub bilibili_auth: Option<BilibiliAuthActionArg>,
    #[arg(
        long,
        value_name = "ID",
        requires = "bilibili_auth",
        help = "QR session id for Bilibili poll/wait"
    )]
    pub auth_session: Option<String>,
    #[arg(
        long,
        value_name = "MILLISECONDS",
        requires = "bilibili_auth",
        help = "Polling interval for Bilibili wait (default: 2000)"
    )]
    pub auth_interval_ms: Option<u64>,
    #[arg(
        long,
        value_name = "SECONDS",
        requires = "bilibili_auth",
        help = "Timeout for Bilibili wait (default: 180)"
    )]
    pub auth_timeout_seconds: Option<u64>,
    #[arg(
        long = "config",
        value_name = "ACTION",
        value_enum,
        help = "Runtime config action: get or set",
        conflicts_with = "bilibili_auth"
    )]
    pub runtime_config: Option<RuntimeConfigActionArg>,
    #[arg(
        long,
        value_name = "KEY",
        value_enum,
        requires = "runtime_config",
        help = "Runtime config key to set"
    )]
    pub config_key: Option<RuntimeConfigKeyArg>,
    #[arg(
        long,
        value_name = "VALUE",
        requires = "runtime_config",
        help = "Runtime config value to set"
    )]
    pub config_value: Option<String>,
    #[command(flatten)]
    pub execution: ExecutionArgs,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum BilibiliAuthActionArg {
    Create,
    Poll,
    Wait,
    Status,
    Logout,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum RuntimeConfigActionArg {
    Get,
    Set,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum RuntimeConfigKeyArg {
    OutputDir,
    Ffmpeg,
    Concurrency,
    ChunkConcurrency,
}

impl From<RuntimeConfigKeyArg> for crate::runtime_config::RuntimeConfigKey {
    fn from(value: RuntimeConfigKeyArg) -> Self {
        match value {
            RuntimeConfigKeyArg::OutputDir => Self::OutputDir,
            RuntimeConfigKeyArg::Ffmpeg => Self::Ffmpeg,
            RuntimeConfigKeyArg::Concurrency => Self::Concurrency,
            RuntimeConfigKeyArg::ChunkConcurrency => Self::ChunkConcurrency,
        }
    }
}

#[derive(Debug, Args)]
pub struct DownloadArgs {
    #[arg(value_name = "INPUT")]
    pub input: String,
    #[arg(long, default_value = "best")]
    pub quality: String,
    #[arg(long, value_name = "N", value_parser = parse_positive_u32)]
    pub part: Option<u32>,
    #[arg(long, conflicts_with = "part")]
    pub all_parts: bool,
    #[arg(long, value_name = "SELECTION", help = "Images, e.g. 1,3-5")]
    pub images: Option<String>,
    #[arg(long, value_enum, default_value = "zip")]
    pub album_output: AlbumOutputArg,
    #[arg(long, value_name = "RELATIVE_PATH")]
    pub output: Option<String>,
    #[command(flatten)]
    pub execution: ExecutionArgs,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum AlbumOutputArg {
    Zip,
    Directory,
}

impl From<AlbumOutputArg> for AlbumOutput {
    fn from(value: AlbumOutputArg) -> Self {
        match value {
            AlbumOutputArg::Zip => Self::Zip,
            AlbumOutputArg::Directory => Self::Directory,
        }
    }
}

#[derive(Debug, Args)]
pub struct ExecutionArgs {
    #[arg(long, value_name = "DIR", help = "Output root for all task paths")]
    pub output_dir: Option<PathBuf>,
    #[arg(
        long,
        conflicts_with = "rename",
        help = "Replace complete existing outputs"
    )]
    pub overwrite: bool,
    #[arg(
        long,
        conflicts_with = "overwrite",
        help = "Choose a non-conflicting output name"
    )]
    pub rename: bool,
    #[arg(long, value_name = "PATH", help = "ffmpeg executable for mux tasks")]
    pub ffmpeg: Option<PathBuf>,
    #[arg(long, value_name = "N", help = "Concurrent download tasks")]
    pub concurrency: Option<usize>,
    #[arg(long, value_name = "N", help = "Concurrent multipart chunks")]
    pub chunk_concurrency: Option<usize>,
    #[arg(
        long,
        value_name = "BYTES",
        help = "Minimum size for multipart download"
    )]
    pub multipart_threshold: Option<u64>,
    #[arg(long, value_name = "BYTES", help = "Multipart chunk size")]
    pub chunk_size: Option<u64>,
    #[arg(
        long,
        conflicts_with = "json_progress",
        help = "Print the final result as JSON"
    )]
    pub json: bool,
    #[arg(
        long,
        conflicts_with = "json",
        help = "Print NDJSON progress and the final done event"
    )]
    pub json_progress: bool,
}

impl ExecutionArgs {
    fn has_download_overrides(&self) -> bool {
        self.output_dir.is_some()
            || self.overwrite
            || self.rename
            || self.ffmpeg.is_some()
            || self.concurrency.is_some()
            || self.chunk_concurrency.is_some()
            || self.multipart_threshold.is_some()
            || self.chunk_size.is_some()
    }
}

#[derive(Debug, Clone, Default)]
pub struct AppContext {
    pub kuaishou_api_url: Option<String>,
    pub bilibili_auth_base_url: Option<String>,
    pub config_dir: Option<PathBuf>,
}

pub async fn entry() -> ExitCode {
    let cli = Cli::parse();
    let error_mode = cli.error_mode();
    match run(cli, AppContext::default()).await {
        Ok(success) if success => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(error) => {
            render_error(error, error_mode);
            ExitCode::FAILURE
        }
    }
}

pub async fn run(cli: Cli, context: AppContext) -> Result<bool> {
    let proxy = runtime_proxy(cli.proxy);
    let cookies_path = cli.cookies;
    match cli.command {
        Command::Inspect(args) => {
            let media = resolve_input(&args.input, proxy, cookies_path, &context).await?;
            let inspect = media.inspect();
            if args.json {
                write_pretty_json(io::stdout().lock(), &inspect)?;
            } else {
                println!("{}: {}", inspect.platform, inspect.media.title);
                for quality in inspect.choices.qualities {
                    println!(
                        "- {} ({}){}",
                        quality.id,
                        quality.label,
                        if quality.recommended {
                            " [recommended]"
                        } else {
                            ""
                        }
                    );
                }
                if !inspect.choices.parts.is_empty() {
                    println!("parts: {}", inspect.choices.parts.len());
                }
                if !inspect.choices.images.is_empty() {
                    println!("images: {}", inspect.choices.images.len());
                }
            }
            Ok(true)
        }
        Command::Plan(args) => {
            let images = parse_image_selection(args.images.as_deref())?;
            let media = resolve_input(&args.input, proxy, cookies_path, &context).await?;
            let plan = media
                .create_plan(&PlanSelection {
                    quality: Some(args.quality),
                    output: args.output,
                    images,
                    album_output: args.album_output.into(),
                    part: args.part,
                    all_parts: args.all_parts,
                })
                .await?;
            write_pretty_json(io::stdout().lock(), &plan)?;
            Ok(true)
        }
        Command::Execute(args) => {
            if let Some(action) = args.bilibili_auth {
                run_bilibili_auth(action, args, proxy, &context).await
            } else if let Some(action) = args.runtime_config {
                run_runtime_config(action, args, &context).await
            } else {
                let plan = read_plan(args.plan.as_deref().expect("clap requires a plan")).await?;
                run_execution(plan, args.execution, proxy, cookies_path, &context).await
            }
        }
        Command::Download(args) => {
            let images = parse_image_selection(args.images.as_deref())?;
            let media =
                resolve_input(&args.input, proxy.clone(), cookies_path.clone(), &context).await?;
            let plan = media
                .create_plan(&PlanSelection {
                    quality: Some(args.quality),
                    output: args.output,
                    images,
                    album_output: args.album_output.into(),
                    part: args.part,
                    all_parts: args.all_parts,
                })
                .await?;
            run_execution(plan, args.execution, proxy, cookies_path, &context).await
        }
    }
}

async fn resolve_input(
    input: &str,
    proxy: Option<String>,
    cookies_path: Option<PathBuf>,
    context: &AppContext,
) -> Result<ResolvedMedia> {
    let options = ResolveOptions {
        proxy,
        timeout: Duration::from_secs(30),
        cookies_path,
        config_dir: context
            .config_dir
            .clone()
            .unwrap_or_else(crate::credentials::default_config_dir),
    };
    if let Some(api_url) = context.kuaishou_api_url.as_deref()
        && !crate::platforms::xiaohongshu::matches(input)
        && !crate::platforms::douyin::matches(input)
        && !crate::platforms::bilibili::matches(input)
        && (kuaishou::matches(input) || kuaishou::looks_like_bare_photo_id(input.trim()))
    {
        return Ok(ResolvedMedia::Kuaishou(
            KuaishouResolver::with_api_url(options, api_url)?
                .resolve(input)
                .await?,
        ));
    }
    crate::platforms::resolve(input, options).await
}

async fn run_runtime_config(
    action: RuntimeConfigActionArg,
    args: ExecuteArgs,
    context: &AppContext,
) -> Result<bool> {
    validate_runtime_config_args(action, &args)?;
    let config_dir = context
        .config_dir
        .clone()
        .unwrap_or_else(crate::credentials::default_config_dir);
    let config = match action {
        RuntimeConfigActionArg::Get => {
            crate::runtime_config::RuntimeConfig::load(&config_dir).await?
        }
        RuntimeConfigActionArg::Set => {
            crate::runtime_config::RuntimeConfig::set(
                &config_dir,
                args.config_key.expect("validated config key").into(),
                args.config_value
                    .as_deref()
                    .expect("validated config value"),
            )
            .await?
        }
    };
    write_control_output(&config, &args.execution, || {
        let mut printed = false;
        if let Some(value) = config.output_dir.as_deref() {
            println!("outputDir={}", value.to_string_lossy());
            printed = true;
        }
        if let Some(value) = config.ffmpeg.as_deref() {
            println!("ffmpeg={}", value.to_string_lossy());
            printed = true;
        }
        if let Some(value) = config.concurrency {
            println!("concurrency={value}");
            printed = true;
        }
        if let Some(value) = config.chunk_concurrency {
            println!("chunkConcurrency={value}");
            printed = true;
        }
        if !printed {
            println!("No runtime config values set");
        }
    })?;
    Ok(true)
}

fn validate_runtime_config_args(action: RuntimeConfigActionArg, args: &ExecuteArgs) -> Result<()> {
    match action {
        RuntimeConfigActionArg::Get => {
            if args.config_key.is_some() || args.config_value.is_some() {
                return Err(AppError::new(
                    "cli.invalid_arguments",
                    "--config-key and --config-value are only valid for config set",
                    false,
                ));
            }
        }
        RuntimeConfigActionArg::Set => {
            if args.config_key.is_none() || args.config_value.is_none() {
                return Err(AppError::new(
                    "cli.invalid_arguments",
                    "config set requires --config-key and --config-value",
                    false,
                ));
            }
        }
    }
    if args.execution.json_progress {
        return Err(AppError::new(
            "cli.invalid_arguments",
            "--json-progress is only valid when executing a DownloadPlan",
            false,
        ));
    }
    if args.execution.has_download_overrides() {
        return Err(AppError::new(
            "cli.invalid_arguments",
            "download execution options cannot be used with --config",
            false,
        ));
    }
    Ok(())
}

async fn run_bilibili_auth(
    action: BilibiliAuthActionArg,
    args: ExecuteArgs,
    proxy: Option<String>,
    context: &AppContext,
) -> Result<bool> {
    validate_bilibili_auth_args(action, &args)?;
    let config_dir = context
        .config_dir
        .clone()
        .unwrap_or_else(crate::credentials::default_config_dir);
    let client = if let Some(base_url) = context.bilibili_auth_base_url.as_deref() {
        BilibiliAuthClient::with_base_url(
            Some(config_dir),
            proxy,
            Duration::from_secs(30),
            base_url,
        )?
    } else {
        BilibiliAuthClient::new(Some(config_dir), proxy, Duration::from_secs(30))?
    };
    match action {
        BilibiliAuthActionArg::Create => {
            let result = client.create_session().await?;
            write_control_output(&result, &args.execution, || {
                println!("Bilibili QR login session created");
                println!("sessionId: {}", result.session_id);
                println!("QR image: {}", result.qr_path);
                println!("QR URL: {}", result.url);
            })?;
        }
        BilibiliAuthActionArg::Poll => {
            let result = client
                .poll_session(args.auth_session.as_deref().expect("validated session id"))
                .await?;
            write_control_output(&result, &args.execution, || {
                println!("Bilibili QR status: {} ({})", result.message, result.code);
            })?;
        }
        BilibiliAuthActionArg::Wait => {
            let result = client
                .wait_for_login(
                    args.auth_session.as_deref().expect("validated session id"),
                    Duration::from_millis(args.auth_interval_ms.unwrap_or(2_000)),
                    Duration::from_secs(args.auth_timeout_seconds.unwrap_or(180)),
                )
                .await?;
            write_control_output(&result, &args.execution, || {
                println!("Bilibili login completed");
            })?;
        }
        BilibiliAuthActionArg::Status => {
            let result = client.status().await?;
            write_control_output(&result, &args.execution, || {
                println!(
                    "Bilibili login: {}",
                    if result.logged_in {
                        "available"
                    } else {
                        "not configured"
                    }
                );
                if let Some(updated_at) = result.updated_at.as_deref() {
                    println!("updatedAt: {updated_at}");
                }
            })?;
        }
        BilibiliAuthActionArg::Logout => {
            let result = client.logout().await?;
            write_control_output(&result, &args.execution, || {
                println!(
                    "Bilibili login cleared: {}",
                    if result.cleared { "yes" } else { "no" }
                );
            })?;
        }
    }
    Ok(true)
}

fn validate_bilibili_auth_args(action: BilibiliAuthActionArg, args: &ExecuteArgs) -> Result<()> {
    let requires_session = matches!(
        action,
        BilibiliAuthActionArg::Poll | BilibiliAuthActionArg::Wait
    );
    let session = args
        .auth_session
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if requires_session && session.is_none() {
        return Err(AppError::new(
            "cli.invalid_arguments",
            "--auth-session is required for bilibili auth poll and wait",
            false,
        ));
    }
    if !requires_session && session.is_some() {
        return Err(AppError::new(
            "cli.invalid_arguments",
            "--auth-session is only valid for bilibili auth poll and wait",
            false,
        ));
    }
    if action != BilibiliAuthActionArg::Wait
        && (args.auth_interval_ms.is_some() || args.auth_timeout_seconds.is_some())
    {
        return Err(AppError::new(
            "cli.invalid_arguments",
            "auth interval and timeout are only valid for bilibili auth wait",
            false,
        ));
    }
    if args.auth_interval_ms == Some(0) || args.auth_timeout_seconds == Some(0) {
        return Err(AppError::new(
            "cli.invalid_arguments",
            "auth interval and timeout must be positive",
            false,
        ));
    }
    if args.execution.json_progress {
        return Err(AppError::new(
            "cli.invalid_arguments",
            "--json-progress is only valid when executing a DownloadPlan",
            false,
        ));
    }
    if args.execution.has_download_overrides() {
        return Err(AppError::new(
            "cli.invalid_arguments",
            "download execution options cannot be used with --bilibili-auth",
            false,
        ));
    }
    Ok(())
}

fn write_control_output(
    value: &impl serde::Serialize,
    args: &ExecutionArgs,
    human: impl FnOnce(),
) -> Result<()> {
    if args.json {
        write_pretty_json(io::stdout().lock(), value)
    } else {
        human();
        Ok(())
    }
}

async fn run_execution(
    plan: DownloadPlan,
    args: ExecutionArgs,
    proxy: Option<String>,
    cookies_path: Option<PathBuf>,
    context: &AppContext,
) -> Result<bool> {
    let sink: Arc<dyn EventSink> = if args.json_progress {
        Arc::new(JsonProgressSink)
    } else {
        Arc::new(NoopEventSink)
    };
    let defaults = ExecuteOptions::default();
    let config_dir = context
        .config_dir
        .clone()
        .unwrap_or_else(crate::credentials::default_config_dir);
    let env_output = if args.output_dir.is_none() {
        env::var_os("EASYDL_OUTPUT_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    } else {
        None
    };
    let env_ffmpeg = if args.ffmpeg.is_none() {
        env::var("EASYDL_FFMPEG")
            .ok()
            .and_then(non_empty)
            .map(PathBuf::from)
    } else {
        None
    };
    let env_concurrency = if args.concurrency.is_none() {
        runtime_positive_usize("EASYDL_CONCURRENCY")?
    } else {
        None
    };
    let env_chunk_concurrency = if args.chunk_concurrency.is_none() {
        runtime_positive_usize("EASYDL_CHUNK_CONCURRENCY")?
    } else {
        None
    };
    let needs_config = (args.output_dir.is_none() && env_output.is_none())
        || (args.ffmpeg.is_none() && env_ffmpeg.is_none())
        || (args.concurrency.is_none() && env_concurrency.is_none())
        || (args.chunk_concurrency.is_none() && env_chunk_concurrency.is_none());
    let config = if needs_config {
        crate::runtime_config::RuntimeConfig::load(&config_dir).await?
    } else {
        crate::runtime_config::RuntimeConfig::default()
    };
    let summary = execute_plan(
        plan,
        ExecuteOptions {
            output_root: args
                .output_dir
                .or(env_output)
                .or(config.output_dir)
                .unwrap_or_else(|| defaults.output_root.clone()),
            collision: if args.overwrite {
                CollisionPolicy::Overwrite
            } else if args.rename {
                CollisionPolicy::Rename
            } else {
                CollisionPolicy::Skip
            },
            proxy,
            ffmpeg: args
                .ffmpeg
                .or(env_ffmpeg)
                .or(config.ffmpeg)
                .unwrap_or_else(|| defaults.ffmpeg.clone()),
            timeout: defaults.timeout,
            item_concurrency: args
                .concurrency
                .or(env_concurrency)
                .or(config.concurrency)
                .unwrap_or(defaults.item_concurrency),
            chunk_concurrency: args
                .chunk_concurrency
                .or(env_chunk_concurrency)
                .or(config.chunk_concurrency)
                .unwrap_or(defaults.chunk_concurrency),
            multipart_threshold: args
                .multipart_threshold
                .unwrap_or(defaults.multipart_threshold),
            chunk_size: args.chunk_size.unwrap_or(defaults.chunk_size),
            cookies_path,
            config_dir,
        },
        sink,
    )
    .await?;

    if args.json {
        write_pretty_json(io::stdout().lock(), &summary)?;
    } else if !args.json_progress {
        print_human_summary(&summary);
    }
    Ok(summary.success)
}

async fn read_plan(path: &str) -> Result<DownloadPlan> {
    let text = if path == "-" {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text).map_err(|error| {
            AppError::new(
                "plan.read_failed",
                format!("failed to read plan from stdin: {error}"),
                false,
            )
        })?;
        text
    } else {
        tokio::fs::read_to_string(path).await.map_err(|error| {
            AppError::new(
                "plan.read_failed",
                format!("failed to read plan file {path}: {error}"),
                false,
            )
            .with_context("path", path)
        })?
    };
    let plan: DownloadPlan = serde_json::from_str(&text).map_err(|error| {
        AppError::new(
            "plan.invalid_json",
            format!("invalid DownloadPlan JSON: {error}"),
            false,
        )
    })?;
    plan.validate()?;
    Ok(plan)
}

fn runtime_proxy(cli_proxy: Option<String>) -> Option<String> {
    cli_proxy
        .and_then(non_empty)
        .or_else(|| env::var("EASYDL_PROXY").ok().and_then(non_empty))
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn runtime_positive_usize(name: &str) -> Result<Option<usize>> {
    let Some(value) = env::var(name).ok().and_then(non_empty) else {
        return Ok(None);
    };
    value
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .map(Some)
        .ok_or_else(|| {
            AppError::new(
                "config.invalid",
                format!("{name} must be a positive integer"),
                false,
            )
        })
}

fn parse_positive_u32(value: &str) -> std::result::Result<u32, String> {
    value
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("expected a positive integer, got {value}"))
}

fn parse_image_selection(value: Option<&str>) -> Result<Option<Vec<u32>>> {
    const MAX_SELECTED_IMAGES: usize = 10_000;
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let mut selected = BTreeSet::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start = parse_image_index(start.trim(), part)?;
            let end = parse_image_index(end.trim(), part)?;
            if end < start || (end - start) as usize >= MAX_SELECTED_IMAGES {
                return Err(AppError::new(
                    "selection.images_invalid",
                    format!("invalid or excessive image range: {part}"),
                    false,
                ));
            }
            for index in start..=end {
                selected.insert(index - 1);
            }
        } else {
            selected.insert(parse_image_index(part, part)? - 1);
        }
        if selected.len() > MAX_SELECTED_IMAGES {
            return Err(AppError::new(
                "selection.images_invalid",
                "too many images selected",
                false,
            ));
        }
    }
    if selected.is_empty() {
        return Err(AppError::new(
            "selection.images_invalid",
            "image selection is empty",
            false,
        ));
    }
    Ok(Some(selected.into_iter().collect()))
}

fn parse_image_index(value: &str, selection: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            AppError::new(
                "selection.images_invalid",
                format!("invalid image selection: {selection}"),
                false,
            )
        })
}

fn print_human_summary(summary: &DownloadSummary) {
    if summary.success {
        println!("Download completed");
    } else {
        println!("Download completed with failures");
    }
    for artifact in &summary.artifacts {
        println!(
            "artifact: {}{}",
            artifact.path,
            if artifact.partial { " (partial)" } else { "" }
        );
    }
    for skipped in &summary.skipped {
        println!("skipped: {} ({})", skipped.path, skipped.reason.message);
    }
    for failure in &summary.failures {
        println!(
            "failure: {}: {} ({})",
            failure.task_id, failure.error.message, failure.error.code
        );
    }
}

fn write_pretty_json(mut writer: impl Write, value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer_pretty(&mut writer, value).map_err(|error| {
        AppError::new(
            "output.serialize_failed",
            format!("failed to serialize JSON output: {error}"),
            false,
        )
    })?;
    writeln!(writer).map_err(|error| {
        AppError::new(
            "output.write_failed",
            format!("failed to write output: {error}"),
            false,
        )
    })?;
    Ok(())
}

fn render_error(error: AppError, mode: ErrorMode) {
    match mode {
        ErrorMode::Human => eprintln!("Error [{}]: {}", error.info.code, error.info.message),
        ErrorMode::Json => {
            let document = ErrorDocument::from(error);
            let mut stderr = io::stderr().lock();
            let _ = serde_json::to_writer(&mut stderr, &document);
            let _ = writeln!(stderr);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ErrorMode {
    Human,
    Json,
}

impl Cli {
    fn error_mode(&self) -> ErrorMode {
        match &self.command {
            Command::Inspect(args) if args.json => ErrorMode::Json,
            Command::Plan(_) => ErrorMode::Json,
            Command::Execute(args) if args.execution.json || args.execution.json_progress => {
                ErrorMode::Json
            }
            Command::Download(args) if args.execution.json || args.execution.json_progress => {
                ErrorMode::Json
            }
            _ => ErrorMode::Human,
        }
    }
}

struct JsonProgressSink;

impl EventSink for JsonProgressSink {
    fn emit(&self, event: &DownloadEvent) {
        let mut stdout = io::stdout().lock();
        if serde_json::to_writer(&mut stdout, event).is_ok() {
            let _ = writeln!(stdout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_image_selection;

    #[test]
    fn parses_one_based_image_lists_and_ranges() {
        assert_eq!(
            parse_image_selection(Some("1,3-5,3")).unwrap(),
            Some(vec![0, 2, 3, 4])
        );
        assert_eq!(parse_image_selection(None).unwrap(), None);
    }

    #[test]
    fn rejects_invalid_or_excessive_image_ranges() {
        for value in ["0", "5-3", "1-10001", "a"] {
            assert_eq!(
                parse_image_selection(Some(value)).unwrap_err().info.code,
                "selection.images_invalid"
            );
        }
    }
}
