# Rust v0.2 development slice

本文档记录 `rs` 分支上的 Rust 实现。当前已完成快手、小红书、抖音与 B站的媒体解析/规划纵向链路，并补齐 file/archive/mux executor 的首批能力：

```text
快手单视频输入
  → inspect v2
  → DownloadPlan v2
  → 单文件顺序下载
  → 流式 events / summary
```

当前命令：

```bash
cargo run -- inspect '<快手视频链接>' --json
cargo run -- plan '<快手视频链接>' > plan.json
cargo run -- plan '<快手图集链接>' --images 1,3-5 --album-output zip > plan.json
cargo run -- download '<快手图集链接>' --album-output directory --json-progress
cargo run -- execute plan.json --json-progress
cargo run -- download '<快手视频链接>' --json-progress
cargo run -- plan '<B站多P链接>' --part 2 --quality 80 > plan.json
cargo run -- download '<B站合集链接>' --all-parts --ffmpeg /path/to/ffmpeg
cargo run -- execute --bilibili-auth create --json
cargo run -- execute --bilibili-auth wait --auth-session '<sessionId>' --json
cargo run -- execute --bilibili-auth status --json
cargo run -- execute --bilibili-auth logout --json
cargo run -- execute --config set --config-key ffmpeg --config-value /path/to/ffmpeg --json
cargo run -- execute --config get --json
```

当前 file executor 已支持：

- 有序 source fallback 与三次可重试错误重试。
- `.part`、0600 identity sidecar、ETag/Last-Modified/长度校验。
- Range + If-Range 安全顺序续传，以及 identity 变化时自动重启。
- `.parts` 多分块并发、已完成 chunk 恢复、严格 Content-Range 校验。
- chunk 流式拼接，不把完整媒体读入内存。
- 服务器忽略 Range 时自动回退顺序下载。
- 多 task 受限并发，最终 summary 保持 Plan 顺序。
- `archive` task 的 entry 并发、fallback、恢复和安全 ZIP entry path。
- 部分 entry 失败时生成 partial ZIP，保留 `.parts`/sidecar，并可在后续执行中补齐后原子替换为完整 ZIP。
- `mux` task 的 video/audio tracks 并发下载、失败恢复和受控 ffmpeg 参数。
- `--ffmpeg` / `EASYDL_FFMPEG` 运行时配置，以及 `ffmpeg.not_found` / `ffmpeg.failed` 稳定错误码。
- `authRef` 已进行已知 profile、HTTPS origin、标准端口和域名 allowlist 校验；缺失本地凭据时返回 `auth.required`。
- runtime HTTP proxy 已有真实 fake proxy 集成测试，proxy 仍只属于执行环境而不会进入 Plan。
- 快手 `SINGLE_PICTURE`、`HORIZONTAL_ATLAS`、`VERTICAL_ATLAS` 的安全 inspect 和 Plan v2 planner。
- `--images 1,3-5` 选择器；图集默认 ZIP，`--album-output directory` 生成独立 file tasks。
- 小红书分享文本、`xhslink.com` redirect/HTML fallback、完整 note URL 和裸 24 位 note id。
- 小红书视频 stream 排序/清晰度选择、图片 backup sources，以及 Live Photo sidecar video。
- 小红书 initial state 使用有大小上限的流式 HTML 读取，并兼容 `undefined`、`void 0` 和 trailing comma。
- 抖音分享文本、`v.douyin.com` redirect、video/note/share URL 和 `modal_id` / `aweme_id`。
- 抖音 share router data、临时 ttwid retry、web detail fallback，以及受源分辨率约束的 ratio probing。
- 抖音 direct CDN/backup sources、图集 sidecar video、ZIP/directory 和安全 inspect selectors。
- B站 BV/av/b23、festival、分P、番剧 ep/ss/md、合集/series/list、Cheese 课程和工房文件。
- B站 DASH codec/quality 选择、video/audio mux、字幕 sidecar、`--part` / `--all-parts` 和 DRM 明确拒绝。
- `--cookies` 支持 raw Cookie header 与 Netscape domain/path/secure/expiry 过滤，并兼容读取现有 `auth.json` 的 B站登录态；secret 不进入 Inspect/Plan。
- executor 支持 `bilibili:default` credential adapter；每次 redirect 都重新检查 origin scope，且只在本地请求阶段注入 Cookie。
- outputDir、ffmpeg、task/chunk concurrency 已支持 CLI > env > config.json > defaults 优先级。
- B站二维码登录支持 create/poll/wait/status/logout、真实 PNG 生成和本地 auth.json 持久化；通过现有 `execute` 动词的 `--bilibili-auth` control interface 暴露，不增加第五个顶层动词。
- config.json 支持 `execute --config get|set`，覆盖 outputDir、ffmpeg、concurrency 和 chunkConcurrency。
- auth、config 和 QR 文件统一通过 private filesystem module 原子替换；Unix 目录为 0700、文件为 0600，Windows 使用 `MoveFileExW` replace/write-through。
- B站 poll/wait 的公开结果只返回状态和 Cookie 名称，不回显 SESSDATA 或 Cookie 值。

四平台归一化 golden fixtures、v2 contract SHA256 freeze 和五目标 Rust release workflow 已建立。当前主要剩余工作是实际运行 Actions 五目标构建，以及不进入默认 CI 的真实链接 smoke。

验证：

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --release --locked
```

行为 golden fixtures 位于 `tests/fixtures/golden/`，v0.2 contract fixtures 位于 `tests/fixtures/v2/`。完整迁移决策记录位于本地 `.agent/rust-rewrite-plan.md`。
