# EasyDL Downloader Skill

Use `easydl` to resolve and download media from 小红书、抖音、快手、B站. This skill is agent-friendly: prefer machine-readable output, never ask users to paste secrets into chat, and always report artifact paths after downloads.

## Safety rules

- Do **not** ask the user to paste Cookie, SESSDATA, or other credentials in chat.
- If authentication/cookies are needed, ask the user to upload a `cookies.txt` file and pass its local path with `--cookies <path>`.
- For B站 login, prefer QR login:
  - `easydl auth bilibili login create --json`
  - show/upload the returned `qrPath` or `url`
  - poll with `easydl auth bilibili login poll <sessionId> --json`
- For B站 DASH downloads, `ffmpeg` must be available. If needed, set `--ffmpeg <path>` or `easydl config set ffmpeg <path>`.

## Recommended workflow

1. Identify metadata first:

   ```bash
   easydl info --platform auto --json '<share text or url>'
   ```

2. List formats/parts/images before choosing options:

   ```bash
   easydl formats --platform auto --json '<share text or url>'
   ```

3. Download with JSON progress for agent parsing:

   ```bash
   easydl download --platform auto --json-progress '<share text or url>'
   ```

4. Parse the final `done` event and report:
   - `artifacts[].path`
   - `failures[]`, if any
   - whether any artifact is `partial`

## Common commands

### Download best quality

```bash
easydl download --json '<url or share text>'
```

### Download with progress events

```bash
easydl download --json-progress '<url or share text>'
```

### Select quality

Use IDs from `formats --json`:

```bash
easydl download --quality 1080p --json '<url>'
easydl download --quality 80 --json '<bilibili url>'
```

### Select images

```bash
easydl download --images 1,3,5 --json '<album url>'
easydl download --images 1-5 --json '<album url>'
```

### B站 multi-part / season

```bash
easydl download --part 2 --json '<bilibili url>'
easydl download --all-parts --json '<bilibili url>'
```

### Cookies file

```bash
easydl download --cookies /path/to/cookies.txt --json '<url>'
```

### Output directory

```bash
easydl download --output-dir ./downloads --json '<url>'
```

## Platform notes

### 快手

Supported:

- `v.kuaishou.com` short links
- `v.m.chenzhongtech.com/fw/photo/{photoId}`
- `www.kuaishou.com/short-video/{photoId}`
- VIDEO, SINGLE_PICTURE, HORIZONTAL_ATLAS, VERTICAL_ATLAS

No cookie/signature is normally required.

### 小红书

Supported:

- share text / `xhslink.com` / full note URL / 24-char note id
- video notes
- image notes
- Live Photo sidecar video files

If page access fails, try a user-uploaded `cookies.txt`.

### 抖音

Supported:

- share text / `v.douyin.com` / full video or note URL
- video
- image/video album ZIP
- ratio probing when share page lacks explicit streams

### B站

Supported:

- BV / av / b23 / festival bvid
- multi-P: `--part`, `--all-parts`
- bangumi/PGC: ep / ss / md
- DASH + ffmpeg merge
- subtitles as `.json` sidecar files when exposed by B站
- collections / series / lists
- cheese course episodes
- gongfang order file ZIPs

If higher qualities or paid resources fail, use QR login or `--cookies`.

## Output interpretation

With `--json`, the command prints one final JSON summary:

```json
{
  "type": "done",
  "success": true,
  "artifacts": [{ "taskId": "...", "path": "/path/file.mp4" }],
  "failures": [],
  "skipped": []
}
```

With `--json-progress`, stdout is NDJSON. Consume line-by-line and wait for the final `done` event.

If `success` is false, still inspect `artifacts`: ZIP downloads can be partial when some entries fail.
