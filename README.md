# easydownload-skill

一个 **agent 友好** 的媒体下载工具：解析并下载小红书、抖音、快手、B站的视频 / 图集 / 番剧。

核心是 CLI `easydl`，配套一份 `SKILL.md`——把它接入你的 agent（如 OpenClaw、Claude Code），agent 就能读懂「先看信息、再选清晰度、然后下载、最后回报产物路径」这套工作流，并自动遵守安全约定（绝不让你在对话里粘贴 Cookie）。

## 能做什么

- **四个平台**：小红书、抖音、快手、B站。给一段分享文本或链接即可，无需手动指定平台。
- **多种媒体**：单个视频、图集（打包 ZIP）、B站分P / 番剧 / 合集 / 课程，图集可按序号选图。
- **去水印 / 选清晰度**：抖音去水印直链；B站可选 1080P+、指定分P、下字幕。
- **B站登录**：二维码登录，用于高清、大会员或付费内容——全程不需要你手填 SESSDATA。
- **机器可读输出**：`--json` / `--json-progress`，专为 agent 解析设计；断点续传、多线程分块、失败自动汇总。

## 安装 / 分发包

面向 agent 分发的 zip 只包含三类内容：

```text
bin/        # 各平台预编译 easydl 二进制
SKILL.md    # 给 agent 看的 skill 手册
README.md   # 本说明
```

预编译二进制已经内置 Bun runtime 和下载器代码，**使用者不需要安装 Bun，也不需要自行打包**。选择自己的平台二进制，放到 PATH 里并命名为 `easydl` 即可：

```text
bin/easydl-darwin-arm64       # macOS Apple Silicon
bin/easydl-linux-x64          # Linux x64
bin/easydl-linux-arm64        # Linux arm64/aarch64
bin/easydl-windows-x64.exe    # Windows x64
```

Linux / macOS 示例：

```bash
chmod +x bin/easydl-darwin-arm64
cp bin/easydl-darwin-arm64 ~/.local/bin/easydl
easydl --help
```

Windows PowerShell 示例：

```powershell
.\bin\easydl-windows-x64.exe --help
```

### ffmpeg 是否包含？

**不包含。** `ffmpeg` 体积大、平台差异多，当前分发包不内置。只有 B站 DASH 音视频合并需要它，通常包括 B站高画质、1080P+、4K、HDR、番剧 / PGC 等资源。

请使用者自行安装 ffmpeg，并确保它在 PATH 上，或显式配置：

```bash
easydl config set ffmpeg /path/to/ffmpeg
```

常见安装方式：

```bash
brew install ffmpeg             # macOS
sudo apt-get install ffmpeg     # Debian / Ubuntu
winget install Gyan.FFmpeg      # Windows
```

### 从源码开发

源码仓库才需要 Bun：

```bash
bun install
bun run typecheck
bun run test
bun run compile      # 产出当前平台 ./bin/easydl
```

## 快速上手

```bash
# 看看这是什么
./bin/easydl info '<分享文本或链接>'

# 列出可选清晰度 / 分P / 图集
./bin/easydl formats '<链接>'

# 下载（默认最佳清晰度，落到当前目录）
./bin/easydl download '<链接>'

# 指定清晰度和输出目录
./bin/easydl download --quality 1080p --output-dir ~/Downloads '<链接>'
```

完整参数（清晰度、分P、选图、cookies、代理、覆盖策略等）见 `./bin/easydl download --help`。

## 配置给 Agent 使用


### 接入 OpenClaw

1. **准备可执行文件**：从 `bin/` 里选择当前平台二进制，放到 OpenClaw 容器 / 会话能执行到的路径（或加进 PATH），并命名为 `easydl`。运行时还需要 `ffmpeg`（B站 DASH 合并用，不随包内置）。
2. **喂入 SKILL.md**：把 `SKILL.md` 的内容作为这项下载能力的工具说明 / 系统提示交给 agent。它写成了工作流文档——命令用法、`--json` / `--json-progress` 输出契约、安全规则——不是单纯的命令列表。
3. **让 agent 按工作流跑**：先 `info` / `formats` 看清楚是什么、有哪些清晰度，再 `download --json-progress`，最后解析 `done` 事件把产物路径回给用户。
4. **指定安全工作目录**（下一节）：让登录态和配置落到 OpenClaw 自己的目录，而不是散落在系统用户目录。

OpenClaw 一次运行下一个/一组链接即可；几百个 URL 的批量调度由 OpenClaw 侧分批完成，`easydl` 不承担长期队列。

> 遇到 B站链接时，agent 应主动提醒用户：匿名只能拿普通清晰度，1080P+ / 大会员 / 付费内容需要先扫码登录。

### 也支持 Claude Code 等 Skill 类 agent

`SKILL.md` 是标准 skill 格式。把本仓库放进工作目录（项目 skill 自动发现），或软链到全局 skills 目录：

```bash
ln -s "$(pwd)" ~/.claude/skills/easydl-downloader
```

之后用户贴链接或说「帮我下这个 B站视频」，agent 就会加载 skill 并调用 `easydl`。

### 配置目录（登录态 / 默认值放哪）

`easydl` 的 config 和 B站登录态按以下优先级存放。OpenClaw 应把 `EASYDL_CONFIG_DIR` 指到自己的安全工作目录：

```
$EASYDL_CONFIG_DIR  →  $XDG_CONFIG_HOME/easydl  →  ~/.config/easydl
```

- `config.json`：默认输出目录、ffmpeg 路径、并发数等（`easydl config set <key> <value>`）。
- `auth.json`：B站 SESSDATA，由二维码登录写入。

## 关于登录与隐私

- **绝不在对话或命令行里粘贴 Cookie / SESSDATA。** agent 被明确要求不索取这些。
- B站高清 / 会员 / 付费内容需要登录：用二维码登录（`easydl auth bilibili login create --json`），agent 会把二维码发给你扫。
- 若某平台确实需要 cookie，上传一个 `cookies.txt` 文件，用 `--cookies <路径>` 传，路径优先级高于登录态。
- 本工具只做**解析 + 下载**，不做 DRM 绕过——受保护内容会明确报「不支持」。

## 文档

- **`SKILL.md`** — 给 agent 看的运行手册：命令、工作流、输出契约、安全规则。接入 agent 时用这份。
- 想改代码 / 加平台，从 `SKILL.md` 顶部结构和 `src/core/types.ts` 的 `Platform` 接口入手。
