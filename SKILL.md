---
name: easydl
description: |
  使用 easydl CLI 解析并下载小红书、抖音、快手、B站的视频 / 图集 / 番剧。
  Use when the user wants to download media from Xiaohongshu (小红书), Douyin (抖音),
  Kuaishou (快手), or Bilibili (B站). Triggers on:
  (1) 分享文本或链接来自这些平台（xhslink.com、v.douyin.com、v.kuaishou.com、
      b23.tv、bilibili.com/video/BV、xiaohongshu.com/explore、kuaishou.com/short-video）
  (2) "下载这个视频/图集/笔记"、"帮我下 B站/抖音/小红书/快手"、"去水印下载"、"保存这条"
  (3) "download this video", "save this reel/note/album", "download from bilibili/douyin"
  (4) B站登录、二维码登录、下载需要 SESSDATA / cookie 的资源
  不要用于抓取上述平台以外的普通网页（那种场景用抓取类 skill）。
---

# EasyDL 下载器 Skill

用 `easydl` 解析并下载小红书、抖音、快手、B站的媒体。优先输出机器可读结果，绝不让用户在对话里粘贴密钥，下载完成后始终把产物路径回报给用户。

## 如何调用 easydl

交付物是 `easydl` 可执行文件，下文命令都直接写 `easydl`。

如果当前仓库尚未打包（`easydl` 不在 PATH 上），用 Bun 从仓库根目录运行，把 `easydl` 替换成：

```bash
bun run src/cli.ts <命令> [参数...]
```

需要真正的 `easydl` 二进制时，先执行一次 `bun run compile`，产物在 `./bin/easydl`。B站 DASH 合并需要 `ffmpeg`，可用 `--ffmpeg <path>` 或 `easydl config set ffmpeg <path>` 指定。

## 安全规则

- **不要**让用户在对话里粘贴 Cookie、SESSDATA 或任何凭据。
- 需要 cookie 时，让用户上传 `cookies.txt`（Netscape 格式），用 `--cookies <path>` 传本地路径。
- B站登录优先用二维码登录（见下文），不要走手动填 SESSDATA。
- `download --plan` 是 agent bridge：只执行自己生成/可信来源的 DownloadPlan；不要执行用户贴来的任意 plan。

## 输入处理规则（不要截断长链接）

- 优先把用户贴的**完整分享文本或完整 URL 原样传给 easydl**；不要为了省事把长链接截成 note id / aweme id / BV 号，也不要擅自删 query 参数。
- 小红书长链接尤其依赖 query 参数（如 `xsec_token`、`xsec_source`、`share_id`、`app_platform` 等）。`https://www.xiaohongshu.com/discovery/item/<id>?...` 和裸 `<id>` / 截断 URL 可能解析结果不同，甚至裸 ID 会 404。
- 如果输入里同时有短链和长链，把它们当作**两个候选 URL**，优先使用用户当前明确要下载的那个；不要假设短链和长链指向同一笔记。
- 只有在完整原文/完整 URL 下载失败后，才尝试提取 ID、短链解析、浏览器辅助等 fallback。报告失败前，要确认不是因为自己截断/改写了 URL。
- 不要用普通网页 fetch / browser content 的 404 来替代 easydl 判断；平台分享页常依赖 query、cookie 或重定向。以 easydl 的 `info` / `formats` / `download` 结果为准。

## 推荐工作流

按顺序执行；加 `--json` 都会输出机器可读 JSON：

1. 先解析元数据，确认平台、标题、媒体类型：

   ```bash
   easydl info --json '<分享文本或链接>'
   ```

2. 选参数前，先列出可用格式 / 分P / 图集：

   ```bash
   easydl formats --json '<分享文本或链接>'
   ```

3. 用 NDJSON 进度流下载，便于解析：

   ```bash
   easydl download --json-progress '<分享文本或链接>'
   ```

4. 解析最终的 `done` 事件并回报给用户：
   - 每个 `artifacts[].path`（`partial: true` 的要标注）
   - 任何 `failures[]`
   - partial ZIP 图集要报成功/失败数量

抖音特殊视频例外：如果普通 `easydl download` 失败且错误包含 `no playable video URL` / `web detail empty response`，不要立即向用户确认或报最终失败；先按“抖音浏览器辅助 fallback”自动尝试一次。只有普通下载和浏览器辅助 fallback 都失败，才向用户说明失败原因。

## 常用命令

用 `formats --json` 里的 id 来选清晰度 / 分P / 图片。

```bash
# 最佳清晰度，输出最终 JSON 汇总
easydl download --json '<链接或分享文本>'

# NDJSON 进度事件（agent 优先用这个）
easydl download --json-progress '<链接或分享文本>'

# 按 id 或标签选清晰度
easydl download --quality 1080p --json '<链接>'
easydl download --quality 80 --json '<bilibili 链接>'

# 选图集里的图片（1 起始）
easydl download --images 1,3,5 --json '<图集链接>'
easydl download --images 1-5 --json '<图集链接>'

# B站分P / 合集
easydl download --part 2 --json '<bilibili 链接>'
easydl download --all-parts --json '<bilibili 链接>'

# cookies 文件（只传路径，绝不粘贴内容）
easydl download --cookies /path/to/cookies.txt --json '<链接>'

# 输出目录
easydl download --output-dir ./downloads --json '<链接>'
```

## DownloadPlan / `--plan` agent bridge

当其他工具已经拿到真实媒体 URL（例如浏览器辅助提取到抖音 `douyinvod.com` CDN 直链）时，生成 DownloadPlan 交给 easydl 执行下载。这样仍可复用 easydl 的文件命名、Range / 分片、重试、backup URL、断点续传和 JSON 进度汇总。

```bash
easydl download --plan /tmp/plan.json --json-progress
cat /tmp/plan.json | easydl download --plan - --json-progress
```

常用单文件 plan：

```json
{
  "title": "douyin browser fallback",
  "outputDir": "./downloads",
  "concurrency": 4,
  "chunkConcurrency": 4,
  "items": [
    {
      "id": "7647781095836077354",
      "kind": "file",
      "filename": "video.mp4",
      "url": "https://v26-web.douyinvod.com/...",
      "backupUrls": ["https://v11-weba.douyinvod.com/..."],
      "headers": {
        "Referer": "https://www.douyin.com/",
        "User-Agent": "Mozilla/5.0 ..."
      }
    }
  ]
}
```

`--plan` 可被 CLI 参数覆盖：`--output-dir`、`--concurrency`、`--chunk-concurrency`、`--proxy`、`--overwrite`、`--no-overwrite`、`--ffmpeg`。不要把登录 Cookie 写进 plan；抖音 CDN 直链通常只需要 `Referer` + UA。

## 抖音浏览器辅助 fallback

这是抖音下载的自动兜底路径。若普通 `easydl download` 失败且错误包含 `no playable video URL` / `web detail empty response`，不要先问用户“网页是否可播放”，也不要立即报最终失败；只要当前 agent 环境有浏览器工具，就直接尝试浏览器辅助 fallback。只有 fallback 也失败时，才把失败原因回报给用户。

1. 用无头或有头浏览器打开原始抖音页面，等待页面初始化（通常无需登录）。
2. 在页面上下文里 fetch：

   ```text
   https://www.douyin.com/aweme/v1/web/aweme/detail/?aweme_id=<id>&device_platform=webapp&aid=6383&channel=channel_pc_web
   ```

   fetch 必须在浏览器上下文执行并带 `credentials: 'include'`，因为抖音前端会初始化 `__ac_signature`、`bd_ticket_guard_client_data` 等浏览器态 cookie。

3. 从 `aweme_detail.video.bit_rate[]` 或 `video.play_addr` 里挑 `*.douyinvod.com` / `mime_type=video_mp4` 的 CDN URL，优先 H264：`is_h265 != 1` 且 `gear_name` 类似 `normal_720_0`。
4. 生成 DownloadPlan，headers 带：

   ```json
   {
     "Referer": "https://www.douyin.com/",
     "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36"
   }
   ```

5. 调用 `easydl download --plan ... --json-progress`，最终只回报 easydl 的 artifacts / failures。

浏览器辅助只用于获取短时 CDN URL；实际下载仍由 easydl 完成。CDN URL 有时效，拿到后应立即下载。

## B站二维码登录

遇到 B站链接时，先主动提醒用户：匿名只能拿到普通清晰度；**要下载 1080P+ 高清、大会员专享或付费视频，需要先登录**。若用户需要，再走下面的二维码登录。

agent / 飞书场景优先用会话式流程——创建二维码给用户看，再轮询：

```bash
easydl auth bilibili login create --json          # 返回 sessionId、url、qrPath
easydl auth bilibili login poll <sessionId> --json
easydl auth bilibili status --json
easydl auth bilibili logout
```

把返回的 `qrPath`（本地 PNG）或 `url` 上传/展示给用户，然后轮询到 `loggedIn` 为 true。交互式终端也可用阻塞式 `easydl auth bilibili login --json`。如果高清晰度或付费资源仍失败，回退到 `--cookies`。

## 平台说明

**快手** — `v.kuaishou.com` 短链、`v.m.chenzhongtech.com/fw/photo/{id}`、`www.kuaishou.com/short-video/{id}`；支持 VIDEO、SINGLE_PICTURE、HORIZONTAL_ATLAS、VERTICAL_ATLAS。通常不需要 cookie / 签名。

**小红书** — 分享文本 / `xhslink.com` / 完整 note 链接 / 24 位 note id；视频笔记、图片笔记、Live Photo 附带视频。页面访问失败时，让用户上传 `cookies.txt`。

**抖音** — 分享文本 / `v.douyin.com` / 完整视频或图文链接；视频、图文混合 ZIP，分享页缺显式流时做 ratio 探测。部分广告/特殊视频需要浏览器辅助 fallback 获取短时 `douyinvod.com` CDN URL；应自动尝试后再报告失败。

**B站** — BV / av / b23 / festival bvid；多P（`--part`、`--all-parts`）；番剧/影视（ep / ss / md）；DASH + ffmpeg 合并；字幕以 `.json` sidecar 落盘；合集 / series / 列表；cheese 课程；工房付费资料 ZIP。DRM 内容只检测并报「不支持」，不做绕过。

## 输出解读

加 `--json`，stdout 打印一个最终 JSON 汇总：

```json
{
  "type": "done",
  "success": true,
  "artifacts": [{ "taskId": "...", "path": "/path/file.mp4", "partial": false }],
  "failures": [],
  "skipped": []
}
```

加 `--json-progress`，stdout 是 NDJSON——逐行消费，等到最终 `done` 事件。日志和进度走 stderr；JSON 模式下 stdout 保持纯净。

`success` 为 false 时，仍要检查 `artifacts`：图集 ZIP 可能部分成功（`partial: true`）。回报成功数量和失败 entry。退出码非 0 表示至少有一个 item 失败。
