# easydl v2 contract fixtures

这些文件是 Rust v0.2 机器 interface 的首批可评审基线：

- `inspect-kuaishou-video.json`：安全视频 inspect 输出，不含媒体下载直链或凭据。
- `inspect-kuaishou-album.json`：安全图集 inspect 输出，只公开 image selectors 和尺寸。
- `inspect-xhs-video.json`：小红书视频的安全 metadata 和排序后 quality selectors。
- `inspect-xhs-album.json`：不含图片/Live Photo 下载直链的小红书图集 selectors。
- `inspect-douyin-video.json`：抖音视频的安全 metadata 和 quality selectors。
- `inspect-douyin-album.json`：不含图片/sidecar video 下载直链的抖音图集 selectors。
- `inspect-bilibili-playlist.json`：B站 DASH qualities、分P selectors 和 ffmpeg requirement。
- `inspect-bilibili-gongfang.json`：不暴露工房下载 URL 的登录要求 inspect。
- `plan-file.json`：短期、低层、无 secret 的 Plan v2 file task。
- `plan-archive.json`：带稳定 entry id、安全 ZIP path 和 fallback sources 的 archive task。
- `plan-mux.json`：video/audio tracks、显式容器格式和本地 `authRef` 的 mux task。
- `events-file-success.json`：流式事件与最终 done 汇总。
- `auth-bilibili-session.json`：二维码 create control 输出。
- `auth-bilibili-poll.json`：不回显 SESSDATA/Cookie 值的 poll 成功输出。
- `auth-bilibili-status.json`：本地登录状态输出。
- `auth-bilibili-logout.json`：credential 清除结果。
- `runtime-config.json`：`execute --config get|set --json` 的公开 config 形状。
- `error-http-forbidden.json`：稳定机器错误码。

## Freeze policy

这些 fixtures 现已进入 v0.2 release-candidate freeze，并由 `SHA256SUMS` 和 Rust contract test 覆盖：

- 不得静默修改、删除或新增 JSON contract fixture。
- release candidate 阶段若必须调整，必须同时更新实现、测试、README、`SHA256SUMS`，并在评审中明确说明兼容性影响。
- v0.2 stable 发布后，破坏现有机器消费者的字段删除、重命名、类型变化或语义变化必须提升 schemaVersion 或进入下一 breaking release。
- 新增可选字段仍需 fixture 和文档；稳定错误 code 不得复用为不同含义。
