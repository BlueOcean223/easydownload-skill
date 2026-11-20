# easydownload-skill

一个面向 agent 的单文件媒体下载 CLI，支持小红书、抖音、快手和 B站的视频、图集、番剧、合集、课程与付费资料。

`easydl` 的 v0.2 实现使用 Rust，核心流程固定为：

```text
input → safe inspect → short-lived Plan v2 → executor → NDJSON events / summary
```

Rust v0.2 已是本项目唯一实现，提供 macOS、Linux（静态 musl）和 Windows 五目标单文件二进制分发。

## 能力

- 自动识别分享文本、短链、完整链接和平台 ID。
- 快手视频、单图、横版/竖版图集。
- 小红书视频、图集和 Live Photo sidecar。
- 抖音视频、图文、临时 ttwid retry、web detail fallback 和受控清晰度探测。
- B站普通视频、多P、番剧、合集、series/list、Cheese 课程、字幕和工房 ZIP。
- DASH video/audio 下载及 ffmpeg 无转码合并。
- file、archive、mux 三类 Plan v2 task。
- Range/If-Range 续传、multipart、fallback source、task/chunk 并发。
- partial ZIP/mux 恢复、原子落盘和安全输出路径。
- `--json` 最终汇总及 `--json-progress` NDJSON 事件流。
- B站二维码登录、本地 credential adapter 和逐跳 origin scope 检查。

不支持 DRM 绕过。检测到受保护内容时会返回稳定错误 `platform.drm_unsupported`。

## 四个 CLI 动词

v0.2 只保留四个顶层动词：

```text
inspect   查看安全 metadata 和 selectors
plan      生成短期、无 secret 的 DownloadPlan v2
execute   执行 Plan，或管理本地 auth/config 执行环境
download  plan + execute 的便捷组合
```

### Inspect

```bash
easydl inspect '<完整分享文本或链接>'
easydl inspect '<完整分享文本或链接>' --json
```

Inspect 只包含标题、作者、媒体类型、清晰度、分P和图片 selector，不包含 CDN URL、Cookie 或平台 raw payload。

### Plan

```bash
easydl plan '<链接>' --quality 1080p > plan.json
easydl plan '<B站多P链接>' --part 2 --quality 80 > plan.json
easydl plan '<B站合集链接>' --all-parts > plan.json
easydl plan '<图集链接>' --images 1,3-5 --album-output zip > plan.json
```

Plan v2 是低层、短期执行计划。Cookie、SESSDATA、proxy、ffmpeg 路径和输出根目录不会写入 Plan；需要本地登录态时只记录受 scope 约束的 `authRef`。

### Execute

```bash
easydl execute plan.json --json
easydl execute plan.json --json-progress
cat plan.json | easydl execute - --json-progress
```

不要执行来源不可信的 Plan。虽然 executor 会校验路径、敏感 header、时效和 authRef scope，Plan 仍可触发网络请求。

### Download

```bash
easydl download '<链接>' --json-progress
easydl download '<链接>' --quality 1080p --output-dir ./downloads --json
easydl download '<图集链接>' --images 1-5 --album-output directory --json
easydl download '<B站合集链接>' --all-parts --ffmpeg /path/to/ffmpeg --json-progress
```

`download` 内部仍经过同一个 Plan v2 和 executor seam，不维护旁路下载实现。

## B站二维码登录

认证操作通过现有 `execute` 动词暴露，不增加第五个顶层命令：

```bash
# 创建二维码；返回 sessionId、url 和本地 PNG qrPath
easydl execute --bilibili-auth create --json

# 单次查询
easydl execute --bilibili-auth poll --auth-session '<sessionId>' --json

# 阻塞等待扫码完成，默认每 2 秒查询、180 秒超时
easydl execute --bilibili-auth wait --auth-session '<sessionId>' --json

# 可覆盖等待参数
easydl execute --bilibili-auth wait \
  --auth-session '<sessionId>' \
  --auth-interval-ms 1000 \
  --auth-timeout-seconds 300 \
  --json

# 查看或清除本地登录态
easydl execute --bilibili-auth status --json
easydl execute --bilibili-auth logout --json
```

poll/wait 输出只包含状态和已保存 Cookie 的名称，不回显 SESSDATA 或 Cookie 值。登录成功后 credential 写入本地 `auth.json`，Plan 中只出现 `bilibili:default` 引用。

## 配置

执行配置优先级：

```text
CLI > environment > config.json > defaults
```

支持的 config key：

- `output-dir`
- `ffmpeg`
- `concurrency`
- `chunk-concurrency`

```bash
easydl execute --config set --config-key output-dir --config-value ./downloads --json
easydl execute --config set --config-key ffmpeg --config-value /opt/homebrew/bin/ffmpeg --json
easydl execute --config set --config-key concurrency --config-value 4 --json
easydl execute --config get --json
```

对应环境变量：

- `EASYDL_OUTPUT_DIR`
- `EASYDL_FFMPEG`
- `EASYDL_CONCURRENCY`
- `EASYDL_CHUNK_CONCURRENCY`
- `EASYDL_PROXY`

配置目录优先级：

```text
EASYDL_CONFIG_DIR → XDG_CONFIG_HOME/easydl → ~/.config/easydl
```

`config.json`、`auth.json` 和二维码 PNG 使用原子替换。Unix 配置目录权限为 0700，私有文件为 0600；Windows 替换使用 `MoveFileExW` replace/write-through。

## Cookie 与隐私

- 不要在对话、Plan 或命令行参数中粘贴 Cookie/SESSDATA。
- 需要外部 Cookie 时，让用户提供本地文件，并用全局 `--cookies PATH`：

  ```bash
  easydl --cookies /secure/path/cookies.txt download '<链接>' --json
  ```

- 支持 raw Cookie Header 文件和 Netscape cookies.txt。
- Netscape Cookie 会按 domain、path、secure 和 expiry 过滤。
- B站 credential 只会发送到明确 allowlist 中的 HTTPS 标准端口 origin；每次 redirect 都重新检查 scope。
- Inspect、Plan、progress、summary 和结构化错误不得包含 secret。

## ffmpeg

ffmpeg 不打入主二进制。B站 DASH、番剧和课程通常需要 ffmpeg 合并音视频：

```bash
brew install ffmpeg             # macOS
sudo apt-get install ffmpeg     # Debian / Ubuntu
winget install Gyan.FFmpeg      # Windows
```

然后确保 `ffmpeg` 在 PATH 中，或使用 `--ffmpeg`、`EASYDL_FFMPEG`、config.json 指定。

## 从源码验证

需要 Rust 1.97.1：

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --release --locked
```

四个平台 resolver/planner 测试会读取 `tests/fixtures/golden/*.json`，比较归一化后的 platform、media kind、quality、part 和 task/output 语义。Rust contract fixtures 位于 `tests/fixtures/v2/`：

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --release --locked
```

当前本机 release 二进制为 5,053,504 bytes（gzip 2,466,609 bytes），ffmpeg 不包含在内。

## Release

- `.github/workflows/rust.yml`：Rust 日常验证。
- `.github/workflows/rust-release.yml`：手动构建 artifact，或在 `v0.2.*` tag 上通过全部验证后发布 GitHub Release：
  - macOS arm64
  - macOS x64
  - Linux x64 musl
  - Linux arm64 musl
  - Windows x64 MSVC

Linux 产物会检查不存在动态 ELF interpreter。每个平台还会实际执行两次 config replacement，用于覆盖原子私有文件写入路径。

workflow 会先运行完整 Rust 测试，再构建和 smoke 五目标；任一目标失败都不会进入 bundle/publish。v2 contract fixtures 通过 SHA256 manifest 保持冻结。

真实链接 smoke 不进入默认 CI，使用本地 harness，并通过环境变量提供不会提交到仓库的测试链接：

```bash
EASYDL_SMOKE_KUAISHOU='<real share URL>' \
EASYDL_SMOKE_XHS='<real share URL>' \
EASYDL_SMOKE_DOUYIN='<real share URL>' \
EASYDL_SMOKE_BILIBILI='<real share URL>' \
EASYDL_SMOKE_MODE=inspect \
./scripts/smoke-real.sh
```

可选 mode 为 `inspect`、`plan` 或 `download`；需要凭据时只传 `EASYDL_SMOKE_COOKIES=/secure/path/cookies.txt`，不要把链接或 credential 写入 workflow/repository fixture。

## Agent 接入

把当前平台的二进制命名为 `easydl` 并放入 PATH，然后把 `SKILL.md` 提供给 agent。Agent 应：

1. 保留用户的完整分享文本/URL。
2. 先运行 `inspect --json`。
3. 根据 selector 运行 `download --json-progress`，或先生成/评审 Plan 再 `execute`。
4. 消费最终 `done` 事件，报告所有 artifact、partial artifact 和 failure。
5. 需要 B站会员能力时使用二维码登录，不索取明文凭据。

详细运行规则见 [`SKILL.md`](SKILL.md)，Rust 内部开发说明见 [`docs/rust-development.md`](docs/rust-development.md)。
