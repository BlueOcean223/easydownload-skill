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

用 `easydl` 下载小红书、抖音、快手和 B站媒体。四条底线：

1. 保留用户的完整原始输入，一个字符都不改。
2. 始终使用机器可读输出（`--json` / `--json-progress`）并按稳定 `code` 判断结果。
3. 绝不要求用户在对话中粘贴 Cookie/SESSDATA。
4. 始终回报最终产物、partial 产物和失败项。

`easydl` 假定已在 PATH，下面的命令可直接执行。

## 强制安全规则

- **不要**让用户在对话里粘贴 Cookie、SESSDATA、Authorization 或其他凭据。
- **不要**把 Cookie/SESSDATA 写入 Plan、临时 JSON、日志或错误说明。
- 需要 Cookie 时，让用户提供本地文件路径，用全局 `--cookies PATH`。
- B站优先二维码登录，不要手工采集 SESSDATA。
- 只执行当前 agent 自己生成或明确可信的 Plan。Plan 会触发网络请求，不能把陌生 Plan 当作纯数据。
- 不修改用户 URL，不删除 query，不截断长链接。小红书 `xsec_token` 等 query 可能是解析必需信息。
- 不绕过 DRM。遇到 `platform.drm_unsupported` 时直接说明不支持。
- 不把 CDN URL 或 auth payload 当作 Inspect 结果回显给用户。

## CLI 模型

只有四个顶层动词：

```text
inspect   安全 metadata + selectors
plan      生成短期、无 secret 的 DownloadPlan v2
execute   执行 Plan，或管理本地 auth/config
download  plan + execute
```

选哪条路径：

| 场景 | 命令 |
|---|---|
| 先看清晰度 / 分P / 图片数量 | `inspect --json` |
| 常规下载（默认路径） | `download --json-progress` |
| 需要审查、延迟执行，或用浏览器补可信 CDN URL | `plan` → `execute` |
| B站登录、本地配置 | `execute --bilibili-auth …` / `execute --config …` |

默认走 `inspect` → `download`，不要无理由拆成 plan + execute。

B站 DASH 合并需要外部 ffmpeg；ffmpeg 不随 `easydl` 分发。

## 推荐工作流

### 1. Inspect

始终先用原始完整输入检查媒体类型和 selector：

```bash
easydl inspect '<完整分享文本或URL>' --json
```

读取：

- `platform`
- `media.id` / `media.kind`（`video` / `album` / `playlist` / `bangumi`）/ `media.title`
- `choices.qualities[].id`（作为 `--quality` 的值）
- `choices.parts[].index`（作为 `--part` 的值）
- `choices.images[].index`（作为 `--images` 的值）
- `requirements.login`：为 true 时先处理登录，别直接下载后再失败
- `requirements.ffmpeg`：为 true 时确认 ffmpeg 可用

Inspect 不包含下载直链或 secret。

### 2. 选择并下载

```bash
easydl download '<完整输入>' --json-progress
```

常用 selector：

```bash
# 清晰度，取 inspect 里的 id；默认 best
easydl download '<链接>' --quality 1080p --json-progress
easydl download '<B站链接>' --quality 80 --json-progress

# B站分P / 合集
easydl download '<B站链接>' --part 2 --json-progress
easydl download '<B站合集链接>' --all-parts --json-progress

# 图集，图片序号从 1 开始
easydl download '<图集链接>' --images 1,3-5 --json-progress
easydl download '<图集链接>' --images 1-5 --album-output directory --json-progress

# 输出位置 / ffmpeg
easydl download '<链接>' --output-dir ./downloads --json-progress
easydl download '<链接>' --output custom-name.mp4 --json-progress
easydl download '<B站链接>' --ffmpeg /path/to/ffmpeg --json-progress
```

图集默认打成 ZIP。`--album-output directory` 会生成独立文件 task。

### 3. 消费事件和汇总

`--json-progress` 的 stdout 是 NDJSON，每行一个事件，都带 `schemaVersion: 2`：

```text
start     task 开始，含 taskId 和 output
reset     已有 partial 数据不可复用，从头重下
progress  downloaded / total 字节
artifact  单个产物落盘
skipped   因 collision policy 跳过
error     单个 task 失败，含结构化 error
done      终止事件，含完整汇总
```

只有 `done` 是终止事件，逐行读取直到拿到它：

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

## 参数速查

Selector（`plan` / `download`）：

| 参数 | 说明 |
|---|---|
| `--quality ID` | 默认 `best`，取 `choices.qualities[].id` |
| `--part N` | 1-based，与 `--all-parts` 互斥 |
| `--all-parts` | 下载全部分P / 合集条目 |
| `--images LIST` | 1-based，支持 `1,3-5`，自动去重排序 |
| `--album-output zip\|directory` | 默认 `zip` |
| `--output RELATIVE_PATH` | 相对 output root 的输出路径 |

执行（`execute` / `download`）：

| 参数 | 说明 |
|---|---|
| `--output-dir DIR` | output root |
| `--overwrite` / `--rename` | 与默认 skip 三选一，两者互斥 |
| `--ffmpeg PATH` | mux task 使用的 ffmpeg |
| `--concurrency N` / `--chunk-concurrency N` | task 并发 / 分片并发 |
| `--multipart-threshold BYTES` / `--chunk-size BYTES` | 分片下载阈值和分片大小 |
| `--json` / `--json-progress` | 最终汇总 / NDJSON 事件流，两者互斥 |

全局（放在 `easydl` 之后）：

| 参数 | 说明 |
|---|---|
| `--cookies PATH` | raw Cookie Header 文件或 Netscape cookies.txt |
| `--proxy URL` | HTTP / SOCKS 代理 |

约束：

- 默认 collision policy 是 skip，适合重复执行和恢复；只有用户明确要求替换时才用 `--overwrite`。`--rename` 会选一个不冲突的新文件名。
- `--json-progress` 和上表的下载执行选项不能与 `--bilibili-auth` / `--config` 同用，否则返回 `cli.invalid_arguments`；auth 和 config 只用 `--json`。
- 不带 `--json` / `--json-progress` 时错误是人类文本；带上才是结构化 JSON。`plan` 始终输出结构化错误。

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

# 阻塞等待，适合已经把二维码展示给用户后调用（默认 2s 间隔、180s 超时）
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

```bash
easydl execute --config get --json

easydl execute --config set --config-key output-dir --config-value ./downloads --json
easydl execute --config set --config-key ffmpeg --config-value /path/to/ffmpeg --json
easydl execute --config set --config-key concurrency --config-value 4 --json
easydl execute --config set --config-key chunk-concurrency --config-value 4 --json
```

`--config get` 只反映已保存的 config.json，不含 CLI 参数和环境变量的覆盖结果。实际生效优先级：

```text
CLI > EASYDL_* environment > config.json > defaults
```

配置目录：

```text
EASYDL_CONFIG_DIR > XDG_CONFIG_HOME/easydl > ~/.config/easydl
```

在 agent 沙箱/容器中，把 `EASYDL_CONFIG_DIR` 指向 agent 自己的私有持久目录。

## 抖音浏览器辅助 fallback

`easydl` 内部已经包含 share router data、临时 ttwid 重试、web detail fallback 和受源分辨率约束的 ratio probing。只有普通 `download` 最终仍返回 `platform.no_streams` 一类错误，且当前 agent 有浏览器工具时，才自动尝试一次浏览器辅助。

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

- 完整分享文本、`xhslink.com` / `xhslink.cn`、note URL、24 位 note id
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

机器模式错误写入 stderr：

```json
{
  "schemaVersion": 2,
  "error": { "code": "platform.no_streams", "message": "...", "retryable": false }
}
```

不要通过解析英文 message 判断流程；只用稳定 `code` 和 `retryable` 分支。错误 context 不应包含 Cookie、SESSDATA 或 CDN credential。

按前缀处理：

| 前缀 | 含义 | 处理 |
|---|---|---|
| `input.*` | 不是可识别的链接或 ID | 让用户重发完整分享文本 |
| `platform.*` | 平台解析失败 | 见下面的高频 code |
| `selection.*` | selector 越界或非法 | 用 inspect 返回的合法 id 重试 |
| `auth.*` | 缺本地登录态或 scope 不允许 | 走二维码登录，不索取明文凭据 |
| `plan.*` | Plan 非法、过期或含 secret | 重新生成 Plan，别手改 |
| `http.*` | 网络或状态码 | 只在 `retryable: true` 时重试 |
| `download.*` / `resume.*` / `multipart.*` | 传输层 | 通常直接重跑，默认 skip 会保留已完成产物 |
| `ffmpeg.*` | 缺 ffmpeg 或合并失败 | 提示安装，或用 `--ffmpeg` 指定路径 |
| `filesystem.*` / `output.*` | 落盘路径问题 | 换 `--output-dir` 或加 `--rename` |
| `proxy.*` / `config.*` / `cli.*` | 调用方参数错误 | 修正命令后重试 |

高频具体 code：

- `auth.required`：缺少本地 credential
- `auth.scope_violation`：authRef origin 不允许
- `platform.drm_unsupported`：DRM，直接说明不支持绕过
- `platform.no_streams`：内容可能已删除、受限或仅 App 可看；抖音可考虑浏览器辅助 fallback
- `selection.quality_not_found` / `selection.part_out_of_range` / `selection.image_out_of_range`
- `ffmpeg.not_found` / `ffmpeg.failed`
- `plan.expired` / `plan.secret_header`
- `download.sources_exhausted`
- `http.forbidden` / `http.unauthorized` / `http.rate_limited`
- `proxy.invalid`
