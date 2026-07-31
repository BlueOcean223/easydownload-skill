---
name: easydl
description: |
  使用 easydl CLI 解析并下载小红书、抖音、快手、B站的视频、图集、番剧、合集、课程和资料。
  Use when the user wants to download media from Xiaohongshu, Douyin, Kuaishou, or Bilibili,
  including share text, short links, note/video IDs, Bilibili BV/av/ep/ss/md, QR login,
  quality/part/image selection, resumable download, or a safe DownloadPlan v2 execution.
  不要用于上述平台之外的普通网页抓取，也不要用于 DRM 绕过。
---

# EasyDL 下载器 Skill

用 `easydl` 下载小红书、抖音、快手和 B站媒体。保留用户的完整输入，优先使用机器输出，绝不要求用户在对话中粘贴 Cookie/SESSDATA，并始终回报最终产物和失败项。

## CLI 模型

v0.2 只有四个顶层动词：

```text
inspect   安全 metadata + selectors
plan      生成短期、无 secret 的 DownloadPlan v2
execute   执行 Plan，或管理本地 auth/config
download  plan + execute
```

如果 `easydl` 已在 PATH，直接调用。仓库开发环境中可替换为：

```bash
cargo run -- <命令> [参数...]
```

B站 DASH 合并需要外部 ffmpeg；ffmpeg 不包含在 easydl 二进制中。

## 强制安全规则

- **不要**让用户在对话里粘贴 Cookie、SESSDATA、Authorization 或其他凭据。
- **不要**把 Cookie/SESSDATA 写入 Plan、临时 JSON、日志或错误说明。
- 需要 Cookie 时，让用户提供本地文件路径，用全局 `--cookies PATH`。支持 raw Cookie Header 文件和 Netscape cookies.txt。
- B站优先二维码登录，不要手工采集 SESSDATA。
- 只执行当前 agent 自己生成或明确可信的 Plan。Plan 会触发网络请求，不能把陌生 Plan 当作纯数据。
- 不修改用户 URL，不删除 query，不截断长链接。小红书 `xsec_token` 等 query 可能是解析必需信息。
- 不绕过 DRM。遇到 `platform.drm_unsupported` 时直接说明不支持。
- 不把 CDN URL 或 auth payload 当作 Inspect 结果回显给用户。

## 推荐工作流

### 1. Inspect

始终先用原始完整输入检查媒体类型和 selector：

```bash
easydl inspect '<完整分享文本或URL>' --json
```

读取：

- `platform`
- `media.id` / `media.kind` / `media.title`
- `choices.qualities[]`
- `choices.parts[]`
- `choices.images[]`
- `requirements.login`
- `requirements.ffmpeg`

Inspect 不包含下载直链或 secret。

### 2. 选择并下载

普通情况直接走统一 planner/executor：

```bash
easydl download '<完整输入>' --json-progress
```

常用 selector：

```bash
# 清晰度
easydl download '<链接>' --quality 1080p --json-progress
easydl download '<B站链接>' --quality 80 --json-progress

# B站分P / 合集
easydl download '<B站链接>' --part 2 --json-progress
easydl download '<B站合集链接>' --all-parts --json-progress

# 图集，图片序号从 1 开始
easydl download '<图集链接>' --images 1,3-5 --json-progress
easydl download '<图集链接>' --images 1-5 --album-output directory --json-progress

# 输出目录 / ffmpeg
easydl download '<链接>' --output-dir ./downloads --json-progress
easydl download '<B站链接>' --ffmpeg /path/to/ffmpeg --json-progress
```

图集默认 ZIP。`--album-output directory` 会生成独立文件 task。

### 3. 消费事件和汇总

`--json-progress` 的 stdout 是 NDJSON。逐行读取，等待最终 `type: "done"`：

```json
{
  "schemaVersion": 2,
  "type": "done",
  "success": true,
  "artifacts": [
    { "taskId": "video", "path": "downloads/video.mp4", "partial": false }
  ],
  "failures": [],
  "skipped": []
}
```

向用户报告：

- 每个 `artifacts[].path`
- `partial: true` 的产物及缺失内容
- 每个 `failures[].error.code` 和可理解的失败原因
- `skipped[]`，尤其默认 collision skip

任一 task/entry 失败时进程会非零退出，但仍可能有可用 artifact；不能只看退出码就丢弃 partial 结果。

## Plan v2 工作流

需要审查、延迟执行或由浏览器辅助提供可信 CDN URL 时，显式使用 Plan：

```bash
easydl plan '<链接>' --quality 1080p > /tmp/plan.json
easydl execute /tmp/plan.json --json-progress
cat /tmp/plan.json | easydl execute - --json-progress
```

Plan v2 的核心形状：

```json
{
  "schemaVersion": 2,
  "createdAt": "2026-07-30T00:00:00Z",
  "expiresAt": "2026-07-30T00:10:00Z",
  "source": {
    "platform": "douyin",
    "mediaId": "1234567890123456789"
  },
  "tasks": [
    {
      "kind": "file",
      "id": "video",
      "sources": [
        {
          "url": "https://trusted-cdn.example/video.mp4",
          "headers": {
            "Referer": "https://www.douyin.com/",
            "User-Agent": "Mozilla/5.0 ..."
          }
        }
      ],
      "output": {
        "relativePath": "video.mp4"
      }
    }
  ]
}
```

规则：

- `createdAt` 用当前 RFC3339 时间；短时 CDN 应设置合理 `expiresAt`。
- 输出只能是相对 output root 的安全路径。
- 可放非敏感 Referer/User-Agent；禁止 Cookie/Authorization。
- 登录资源只用 resolver 生成的 `authRef`，不要自行伪造 credential header。
- proxy、ffmpeg、output root 和并发属于执行环境，不进入 Plan。

## B站二维码登录

遇到 B站高清、大会员、课程或付费资料权限不足时，先告知用户需要扫码登录，再使用下面的会话式流程。

### 创建二维码

```bash
easydl execute --bilibili-auth create --json
```

结果包含：

- `sessionId`
- `qrPath`：本地 PNG，优先上传/展示给用户
- `url`：无法发送图片时的备用二维码 payload

### 查询或等待

```bash
# 单次 poll
easydl execute --bilibili-auth poll --auth-session '<sessionId>' --json

# 阻塞等待，适合已经把二维码展示给用户后调用
easydl execute --bilibili-auth wait --auth-session '<sessionId>' --json
```

需要更长等待时间：

```bash
easydl execute --bilibili-auth wait \
  --auth-session '<sessionId>' \
  --auth-interval-ms 1000 \
  --auth-timeout-seconds 300 \
  --json
```

成功后本地 `auth.json` 自动更新。poll/wait 不回显 SESSDATA 或 Cookie 值，只返回登录状态和已保存 Cookie 的名称。

### 状态和退出

```bash
easydl execute --bilibili-auth status --json
easydl execute --bilibili-auth logout --json
```

不要读取并展示 `auth.json` 内容。

## Cookie 文件

全局参数可放在子命令前或后；推荐放在 `easydl` 后：

```bash
easydl --cookies /secure/path/cookies.txt inspect '<链接>' --json
easydl --cookies /secure/path/cookies.txt download '<链接>' --json-progress
```

Netscape 文件会按请求 URL 的 domain、path、secure 和 expiry 过滤。显式 cookies 文件优先于已有 B站 auth.json。

## Runtime config

查看配置：

```bash
easydl execute --config get --json
```

设置支持的 key：

```bash
easydl execute --config set --config-key output-dir --config-value ./downloads --json
easydl execute --config set --config-key ffmpeg --config-value /path/to/ffmpeg --json
easydl execute --config set --config-key concurrency --config-value 4 --json
easydl execute --config set --config-key chunk-concurrency --config-value 4 --json
```

优先级：

```text
CLI > EASYDL_* environment > config.json > defaults
```

配置目录：

```text
EASYDL_CONFIG_DIR > XDG_CONFIG_HOME/easydl > ~/.config/easydl
```

在 agent 沙箱/容器中，把 `EASYDL_CONFIG_DIR` 指向 agent 自己的私有持久目录。

## 下载执行选项

```bash
--output-dir DIR
--overwrite
--rename
--ffmpeg PATH
--concurrency N
--chunk-concurrency N
--multipart-threshold BYTES
--chunk-size BYTES
--json
--json-progress
```

默认 collision policy 是 skip，适合重复执行和恢复；只有用户明确要求替换时使用 `--overwrite`。`--rename` 会选择不冲突的新文件名。

## 抖音浏览器辅助 fallback

Rust resolver 已内置 share router data、临时 ttwid retry、web detail fallback 和受源分辨率约束的 ratio probing。只有普通 `download` 最终仍返回“no playable video URL”一类错误，且当前 agent 有浏览器工具时，才自动尝试一次浏览器辅助。

1. 打开用户的原始完整抖音页面。
2. 在浏览器页面上下文、带 `credentials: 'include'` 请求 web detail。
3. 只选可信的 `douyinvod.com` 等媒体 CDN URL，优先可验证的 MP4/H.264 流。
4. 立即生成短时 Plan v2，headers 仅包含 Referer/User-Agent。
5. 调用 `easydl execute plan.json --json-progress`。

浏览器只负责发现短时媒体 URL；实际下载仍必须通过 Plan v2/executor seam。普通下载和该 fallback 都失败后，才报告最终失败。

## 平台说明

**快手**

- `v.kuaishou.com`、`v.m.chenzhongtech.com/fw/photo/...`、`kuaishou.com/short-video/...`
- VIDEO、SINGLE_PICTURE、HORIZONTAL_ATLAS、VERTICAL_ATLAS
- 图集可选图片、ZIP/directory，并可包含背景音乐

**小红书**

- 完整分享文本、`xhslink.com`、note URL、24 位 note id
- 视频、图片、Live Photo sidecar
- 完整 URL 的 `xsec_token` 等 query 不得删除

**抖音**

- 分享文本、`v.douyin.com`、video/note/share URL、裸 aweme id
- 视频、图文 ZIP/directory、sidecar video
- 不可信 play endpoint 必须经过 Range probing 才能进入 Plan

**B站**

- BV、av、b23、festival
- 多P、ep/ss/md 番剧、collection/series/list
- Cheese 课程、已购课程、工房 archive
- DASH mux、字幕 JSON sidecar
- 会员/付费权限通过本地 credential adapter
- DRM 只检测并拒绝

## 错误处理

机器模式错误写入 stderr，结构为稳定 `code`、`message`、`retryable` 和可选安全 context。常见错误：

- `auth.required`：缺少本地 credential
- `auth.scope_violation`：authRef origin 不允许
- `platform.drm_unsupported`：DRM，不支持绕过
- `ffmpeg.not_found` / `ffmpeg.failed`
- `plan.expired`
- `plan.secret_header`
- `proxy.invalid`
- `download.sources_exhausted`

不要通过解析英文 message 判断流程；优先使用稳定错误 code。错误 context 不应包含 Cookie、SESSDATA 或 CDN credential。
