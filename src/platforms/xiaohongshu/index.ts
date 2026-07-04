import { loadCookieHeader } from '../../core/cookies.js'
import type {
  DownloadEntry,
  DownloadOptions,
  DownloadPlan,
  MediaImage,
  MediaInfo,
  MediaStream,
  Platform,
  ResolveContext,
} from '../../core/types.js'

const defaultUserAgent = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36'
const defaultBaseUrl = 'https://www.xiaohongshu.com'
const defaultReferer = 'https://www.xiaohongshu.com/'
const requestTimeoutMs = 30_000
const maxHtmlChars = 10 * 1024 * 1024

const xhsHostPattern = /(?:^|\/\/)(?:[^/]+\.)?(?:xiaohongshu\.com|xhslink\.com)(?:[/:?#]|$)/i
const noteIdExactPattern = /^[0-9a-f]{24}$/i
const noteIdPattern = /[0-9a-f]{24}/i
const urlPattern = /https?:\/\/[^\s<>'"`，。；、）)】]+/gi

export const xiaohongshuPlatform: Platform = {
  id: 'xhs',
  displayName: '小红书',
  match(input: string): boolean {
    return xhsHostPattern.test(input) || noteIdExactPattern.test(input.trim())
  },
  async resolve(input: string, ctx: ResolveContext): Promise<MediaInfo> {
    const parsed = await parseXhsInput(input)
    const info = await fetchNoteInfo(parsed, ctx)
    return info
  },
  async createDownloadPlan(info: MediaInfo, options: DownloadOptions): Promise<DownloadPlan> {
    if (info.platform !== 'xhs') throw new Error(`expected xhs MediaInfo, got ${info.platform}`)

    const basePlan = {
      title: info.title,
      outputDir: options.outputDir ?? './downloads',
      concurrency: options.concurrency ?? 4,
      chunkConcurrency: options.chunkConcurrency ?? 4,
      multipartThresholdBytes: 32 * 1024 * 1024,
      timeoutMs: 30_000,
      ffmpeg: 'ffmpeg',
      overwrite: Boolean(options.overwrite),
      noOverwrite: Boolean(options.noOverwrite),
    }

    const headers = xhsDownloadHeaders(info)
    if (info.type === 'video') {
      const stream = pickStream(info, options.quality)
      return {
        ...basePlan,
        items: [
          {
            id: info.id,
            kind: 'file',
            filename: `${xhsFileStem(info)}.${extensionFromUrl(stream.url, 'mp4')}`,
            url: stream.url,
            backupUrls: stream.backupUrls ?? [],
            headers: stream.headers ?? headers,
          },
        ],
      }
    }

    if (info.type === 'album') {
      const selectedImages = selectImages(info.images ?? [], options.images)
      const entries: DownloadEntry[] = []
      for (const image of selectedImages) {
        if (!image.url) throw new Error(`xhs image ${image.index + 1} has no URL`)
        const prefix = String(image.index + 1).padStart(2, '0')
        entries.push({
          name: `${prefix}.${extensionFromUrl(image.url, 'jpg')}`,
          url: image.url,
          backupUrls: image.backupUrls ?? [],
          headers: image.headers ?? headers,
        })
        if (image.livePhoto && image.livePhotoUrl) {
          entries.push({
            name: `${prefix}_live.${extensionFromUrl(image.livePhotoUrl, 'mp4')}`,
            url: image.livePhotoUrl,
            backupUrls: [],
            headers,
          })
        }
      }
      if (entries.length === 0) throw new Error('xhs image note has no downloadable entries')
      return {
        ...basePlan,
        items: [
          {
            id: info.id,
            kind: 'zip',
            filename: `${xhsFileStem(info)}.zip`,
            headers,
            entries,
          },
        ],
      }
    }

    throw new Error(`unsupported xhs media type: ${info.type}`)
  },
}

type ParsedInput = {
  noteId: string
  fullUrl?: string
  xsecToken?: string
}

async function parseXhsInput(input: string): Promise<ParsedInput> {
  const trimmed = input.trim()
  if (!trimmed) throw new Error('no xiaohongshu url found in input')
  if (noteIdExactPattern.test(trimmed)) return { noteId: trimmed.toLowerCase() }

  const url = extractShareUrl(trimmed)
  if (!url) throw new Error('no xiaohongshu url found in input')
  return parseXhsUrl(url, 0)
}

async function parseXhsUrl(rawUrl: string, depth: number): Promise<ParsedInput> {
  if (depth > 5) throw new Error('xiaohongshu short link resolution failed: too many redirects')

  const url = new URL(trimUrl(rawUrl))
  if (url.protocol !== 'http:' && url.protocol !== 'https:') throw new Error('invalid xiaohongshu url scheme')

  const host = url.hostname.toLowerCase()
  if (isShortHost(host)) {
    const resolved = await resolveShortLink(url.href)
    return parseXhsUrl(resolved, depth + 1)
  }
  if (!isXhsHost(host)) throw new Error(`invalid xiaohongshu host: ${host}`)

  const fromPath = url.pathname.match(noteIdPattern)?.[0]
  const fromQuery = [...url.searchParams.values()].map((value) => value.match(noteIdPattern)?.[0]).find(Boolean)
  const noteId = (fromPath || fromQuery)?.toLowerCase()
  if (!noteId) throw new Error('xiaohongshu note id not found')

  return {
    noteId,
    fullUrl: url.href,
    xsecToken: url.searchParams.get('xsec_token') ?? undefined,
  }
}

async function resolveShortLink(shortUrl: string): Promise<string> {
  const candidates = [shortUrl]
  const parsed = new URL(shortUrl)
  if (parsed.protocol === 'http:') {
    parsed.protocol = 'https:'
    candidates.push(parsed.href)
  }

  let lastErr: unknown
  for (const candidate of candidates) {
    try {
      return await resolveShortLinkOnce(candidate)
    } catch (err) {
      lastErr = err
    }
  }
  throw new Error(`xiaohongshu short link resolution failed: ${lastErr instanceof Error ? lastErr.message : String(lastErr)}`)
}

async function resolveShortLinkOnce(shortUrl: string): Promise<string> {
  const response = await fetchWithTimeout(shortUrl, {
    redirect: 'manual',
    headers: {
      'User-Agent': defaultUserAgent,
      Accept: 'text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8',
      'Accept-Language': 'zh-CN,zh;q=0.9,en;q=0.8',
    },
  })

  const location = response.headers.get('location')
  if (location && response.status >= 300 && response.status < 400) {
    await response.body?.cancel().catch(() => undefined)
    return new URL(location, shortUrl).href
  }

  const body = htmlUnescape((await response.text()).slice(0, 256 * 1024))
  for (const candidate of body.match(urlPattern) ?? []) {
    try {
      const url = new URL(trimUrl(candidate))
      if (isXhsHost(url.hostname.toLowerCase())) return url.href
    } catch {
      // Ignore malformed embedded URLs.
    }
  }

  throw new Error(`unexpected status ${response.status}`)
}

function extractShareUrl(input: string): string | undefined {
  if (/^https?:\/\//i.test(input)) {
    try {
      const url = new URL(trimUrl(input))
      if (isAcceptableHost(url.hostname.toLowerCase())) return url.href
    } catch {
      // Fall through to embedded URL extraction.
    }
  }

  for (const match of input.match(urlPattern) ?? []) {
    try {
      const url = new URL(trimUrl(match))
      if (isAcceptableHost(url.hostname.toLowerCase())) return url.href
    } catch {
      // Ignore malformed URLs in share text.
    }
  }
  return undefined
}

async function fetchNoteInfo(input: ParsedInput, ctx: ResolveContext): Promise<MediaInfo> {
  const noteUrl = buildExploreUrl(input.noteId, input.xsecToken)
  const headers = await xhsPageHeaders(ctx.cookiesPath, noteUrl)
  const response = await fetchWithTimeout(noteUrl, { headers })

  if (response.status === 404) throw new Error('xhs note not found')
  if (response.status === 429) throw new Error('xhs request rate limited')
  if (response.status === 401 || response.status === 403) throw new Error('xhs note blocked or login required')
  if (!response.ok) throw new Error(`xhs page HTTP ${response.status}`)

  const html = await response.text()
  if (html.length > maxHtmlChars) throw new Error(`xhs response too large: ${html.length} chars`)
  if (html.includes('当前笔记暂时无法浏览') || html.includes('error_code')) throw new Error('xhs note blocked or login required')

  const stateJson = jsToJson(extractJSONObjectAfterMarker(html, 'window.__INITIAL_STATE__'))
  const state = JSON.parse(stateJson) as Record<string, unknown>
  const note = lookupNote(state, input.noteId)
  if (!note) throw new Error('xhs note not found in initial state')

  return buildMediaInfo(input, note, noteUrl)
}

function buildExploreUrl(noteId: string, xsecToken: string | undefined): string {
  const url = new URL(`/explore/${noteId}`, defaultBaseUrl)
  if (xsecToken?.trim()) url.searchParams.set('xsec_token', xsecToken.trim())
  return url.href
}

async function xhsPageHeaders(cookiesPath: string | undefined, noteUrl: string): Promise<Record<string, string>> {
  const headers: Record<string, string> = {
    'User-Agent': defaultUserAgent,
    Referer: defaultReferer,
    Accept: 'text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8',
    'Accept-Language': 'zh-CN,zh;q=0.9,en;q=0.8',
    Connection: 'keep-alive',
    'Upgrade-Insecure-Requests': '1',
    'Sec-Fetch-Dest': 'document',
    'Sec-Fetch-Mode': 'navigate',
    'Sec-Fetch-Site': 'none',
    'Sec-Fetch-User': '?1',
    'Cache-Control': 'max-age=0',
  }
  const cookie = await loadCookieHeader(cookiesPath, noteUrl)
  if (cookie) headers.Cookie = cookie
  return headers
}

function xhsDownloadHeaders(info: MediaInfo): Record<string, string> {
  const raw = getRaw(info)
  return {
    'User-Agent': defaultUserAgent,
    Referer: raw.noteUrl ?? defaultReferer,
  }
}

function buildMediaInfo(input: ParsedInput, note: Record<string, unknown>, noteUrl: string): MediaInfo {
  const id = input.noteId
  const title = firstNonEmpty(getString(note, 'title'), getString(note, 'desc'), id)
  const user = getRecord(note.user)
  const author = firstNonEmpty(getString(user, 'nickname'), getString(user, 'name')) || undefined
  const images = parseImages(note)
  const streams = parseVideoStreams(note)
  const noteType = getString(note, 'type').toLowerCase()

  if (noteType === 'video' || streams.length > 0) {
    return {
      platform: 'xhs',
      id,
      type: 'video',
      title,
      author,
      cover: images[0]?.url || getString(note, 'cover') || undefined,
      streams,
      raw: { note, noteUrl, xsecToken: input.xsecToken } satisfies XhsRaw,
    }
  }

  if (images.length > 0) {
    return {
      platform: 'xhs',
      id,
      type: 'album',
      title,
      author,
      cover: images[0]?.url,
      images,
      raw: { note, noteUrl, xsecToken: input.xsecToken } satisfies XhsRaw,
    }
  }

  throw new Error('xhs note has no downloadable video or images')
}

function lookupNote(state: Record<string, unknown>, noteId: string): Record<string, unknown> | undefined {
  const detailMap = getRecord(getRecord(state.note).noteDetailMap)
  const direct = getRecord(detailMap[noteId])
  if (direct.note) return getRecord(direct.note)

  for (const value of Object.values(detailMap)) {
    const entry = getRecord(value)
    const note = getRecord(entry.note)
    if (getString(note, 'noteId').toLowerCase() === noteId || getString(note, 'id').toLowerCase() === noteId) return note
  }
  return undefined
}

function parseImages(note: Record<string, unknown>): MediaImage[] {
  const list = Array.isArray(note.imageList) ? note.imageList : []
  const out: MediaImage[] = []
  for (const [index, value] of list.entries()) {
    const image = getRecord(value)
    const urls = uniqueStrings([
      getString(image, 'urlDefault'),
      getString(image, 'urlPre'),
      getString(image, 'url'),
      ...urlsFromInfoList(image.infoList),
    ])
    if (urls.length === 0) continue
    out.push({
      index,
      url: urls[0],
      backupUrls: urls.slice(1),
      width: getNumber(image, 'width') || undefined,
      height: getNumber(image, 'height') || undefined,
      livePhoto: getBool(image, 'livePhoto'),
      livePhotoUrl: firstMediaUrl(image.stream),
      headers: { 'User-Agent': defaultUserAgent, Referer: defaultReferer },
    })
  }
  return out
}

function parseVideoStreams(note: Record<string, unknown>): ParsedStream[] {
  const media = getRecord(getRecord(note.video).media)
  const streamValue = media.stream
  const candidates: Array<{ data: Record<string, unknown>; codecHint?: string }> = []

  if (Array.isArray(streamValue)) {
    for (const item of streamValue) {
      const data = getRecord(item)
      if (Object.keys(data).length) candidates.push({ data })
    }
  }

  const streamMap = getRecord(streamValue)
  for (const [codecHint, value] of Object.entries(streamMap)) {
    if (!Array.isArray(value)) continue
    for (const item of value) {
      const data = getRecord(item)
      if (Object.keys(data).length) candidates.push({ data, codecHint })
    }
  }

  const bestByKey = new Map<string, ParsedStream>()
  for (const candidate of candidates) {
    const stream = parseStreamCandidate(candidate.data, candidate.codecHint)
    if (!stream) continue
    const key = stream.id.toLowerCase()
    const existing = bestByKey.get(key)
    if (!existing || streamBetterThan(stream, existing)) bestByKey.set(key, stream)
  }

  return [...bestByKey.values()].sort((a, b) => (streamBetterThan(b, a) ? 1 : streamBetterThan(a, b) ? -1 : 0))
}

function parseStreamCandidate(data: Record<string, unknown>, codecHint: string | undefined): ParsedStream | undefined {
  const urls = uniqueStrings([getString(data, 'masterUrl'), ...collectStringSlice(data.backupUrls)])
  if (urls.length === 0) return undefined

  const streamType = getNumber(data, 'streamType')
  const qualityName = firstNonEmpty(getString(data, 'qualityType'), getString(data, 'quality'), streamType ? `Stream ${streamType}` : '')
  const videoCodec = firstNonEmpty(getString(data, 'videoCodec'), codecHint ?? '')
  const id = buildStreamQualityKey(qualityName, streamType)
  return {
    id,
    quality: id,
    qualityName: [qualityName, videoCodec].filter(Boolean).join(' '),
    width: getNumber(data, 'width') || undefined,
    height: getNumber(data, 'height') || undefined,
    size: getNumber(data, 'size') || undefined,
    url: urls[0],
    backupUrls: urls.slice(1),
    headers: { 'User-Agent': defaultUserAgent, Referer: defaultReferer },
    streamType,
    weight: getNumber(data, 'weight'),
    defaultStream: getNumber(data, 'defaultStream'),
    videoCodec,
    videoBitrate: getNumber(data, 'videoBitrate'),
  }
}

function pickStream(info: MediaInfo, quality = 'best'): ParsedStream {
  const streams = (info.streams ?? []).filter((stream): stream is ParsedStream => Boolean(stream.url))
  if (streams.length === 0) throw new Error('xhs video has no downloadable stream')

  const wanted = quality.trim().toLowerCase()
  if (wanted && wanted !== 'best') {
    const match = streams.find((stream) =>
      [stream.id, stream.quality, stream.qualityName, stream.streamType ? String(stream.streamType) : '']
        .filter((value): value is string => Boolean(value))
        .some((value) => value.toLowerCase() === wanted),
    )
    if (match) return match
    throw new Error(`xhs quality not found: ${quality}; available: ${streams.map((stream) => stream.id).join(', ')}`)
  }

  return streams[0]
}

function selectImages(images: MediaImage[], selection: number[] | undefined): MediaImage[] {
  if (!selection?.length) return images
  return selection.map((index) => {
    const image = images[index]
    if (!image) throw new Error(`xhs image index out of range: ${index + 1} (total ${images.length})`)
    return image
  })
}

function extractJSONObjectAfterMarker(html: string, marker: string): string {
  const markerIndex = html.indexOf(marker)
  if (markerIndex < 0) throw new Error('xhs initial state not found')
  const start = html.indexOf('{', markerIndex)
  if (start < 0) throw new Error('xhs initial state not found')

  let depth = 0
  let inString = false
  let escape = false
  for (let i = start; i < html.length; i++) {
    const ch = html[i]
    if (inString) {
      if (escape) {
        escape = false
      } else if (ch === '\\') {
        escape = true
      } else if (ch === '"') {
        inString = false
      }
      continue
    }
    if (ch === '"') inString = true
    else if (ch === '{') depth++
    else if (ch === '}') {
      depth--
      if (depth === 0) return html.slice(start, i + 1)
    }
  }
  throw new Error('xhs initial state not found')
}

function jsToJson(js: string): string {
  let result = js
  const patterns: Array<[RegExp, string]> = [
    [/([:,\[{]\s*)undefined(\s*[,}\]])/g, '$1null$2'],
    [/([:,\[{]\s*)undefined(\s*)$/g, '$1null$2'],
    [/([:,\[{]\s*)void\s+0(\s*[,}\]])/g, '$1null$2'],
    [/([:,\[{]\s*)void\s+0(\s*)$/g, '$1null$2'],
    [/,\s*([}\]])/g, '$1'],
  ]
  for (const [pattern, replacement] of patterns) {
    for (let i = 0; i < 10; i++) {
      const next = result.replace(pattern, replacement)
      if (next === result) break
      result = next
    }
  }
  return result
}

async function fetchWithTimeout(url: string, init: RequestInit): Promise<Response> {
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(), requestTimeoutMs)
  try {
    return await fetch(url, { ...init, signal: controller.signal })
  } catch (err) {
    if (err instanceof Error && err.name === 'AbortError') throw new Error(`xhs request timed out after ${requestTimeoutMs}ms`)
    throw err
  } finally {
    clearTimeout(timer)
  }
}

function urlsFromInfoList(value: unknown): string[] {
  if (!Array.isArray(value)) return []
  return value.map((item) => getString(getRecord(item), 'url')).filter(Boolean)
}

function firstMediaUrl(value: unknown): string | undefined {
  if (typeof value === 'string') return value.trim() || undefined
  if (Array.isArray(value)) {
    for (const item of value) {
      const found = firstMediaUrl(item)
      if (found) return found
    }
    return undefined
  }
  if (!value || typeof value !== 'object') return undefined
  const record = getRecord(value)
  const urls = uniqueStrings([firstNonEmpty(getString(record, 'masterUrl'), getString(record, 'url')), ...collectStringSlice(record.backupUrls)])
  if (urls.length > 0) return urls[0]

  for (const key of ['h265', 'h264', 'av1', 'h266', 'hevc', 'avc']) {
    const found = firstMediaUrl(record[key])
    if (found) return found
  }

  for (const key of Object.keys(record).sort()) {
    if (['masterUrl', 'url', 'backupUrls', 'h265', 'h264', 'av1', 'h266', 'hevc', 'avc'].includes(key)) continue
    const found = firstMediaUrl(record[key])
    if (found) return found
  }
  return undefined
}

function buildStreamQualityKey(qualityName: string, streamType: number): string {
  const base = qualityName.trim().toLowerCase().replace(/[\s/\\:]+/g, '_')
  if (streamType > 0) {
    if (!base || base.startsWith('stream_')) return `stream_${streamType}`
    return `${base}_${streamType}`
  }
  return base || 'default'
}

function streamBetterThan(a: ParsedStream, b: ParsedStream): boolean {
  if (codecRank(a.videoCodec) !== codecRank(b.videoCodec)) return codecRank(a.videoCodec) > codecRank(b.videoCodec)
  if ((a.weight ?? 0) !== (b.weight ?? 0)) return (a.weight ?? 0) > (b.weight ?? 0)
  if (streamTypeRank(a.streamType) !== streamTypeRank(b.streamType)) return streamTypeRank(a.streamType) > streamTypeRank(b.streamType)
  if ((a.defaultStream ?? 0) !== (b.defaultStream ?? 0)) return (a.defaultStream ?? 0) > (b.defaultStream ?? 0)
  const aPixels = (a.width ?? 0) * (a.height ?? 0)
  const bPixels = (b.width ?? 0) * (b.height ?? 0)
  if (aPixels !== bPixels) return aPixels > bPixels
  if ((a.videoBitrate ?? 0) !== (b.videoBitrate ?? 0)) return (a.videoBitrate ?? 0) > (b.videoBitrate ?? 0)
  return (a.size ?? 0) > (b.size ?? 0)
}

function codecRank(codec: string | undefined): number {
  const normalized = (codec ?? '').toLowerCase().replace(/[._-]/g, '')
  if (normalized === 'h265' || normalized === 'hevc') return 4
  if (normalized === 'h264' || normalized === 'avc') return 3
  if (normalized === 'av1' || normalized === 'h266' || normalized === 'vvc') return 2
  return 1
}

function streamTypeRank(streamType: number | undefined): number {
  if (streamType === 115) return 300
  if (streamType === 114) return 200
  if (streamType === 259) return 100
  return 0
}

function collectStringSlice(value: unknown): string[] {
  if (typeof value === 'string') return value.trim() ? [value.trim()] : []
  if (!Array.isArray(value)) return []
  return value.filter((item): item is string => typeof item === 'string' && item.trim() !== '').map((item) => item.trim())
}

function uniqueStrings(values: string[]): string[] {
  const out: string[] = []
  const seen = new Set<string>()
  for (const value of values) {
    const trimmed = value.trim()
    if (!trimmed || seen.has(trimmed)) continue
    seen.add(trimmed)
    out.push(trimmed)
  }
  return out
}

function getRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, unknown>) : {}
}

function getString(record: Record<string, unknown>, key: string): string {
  const value = record[key]
  if (typeof value === 'string') return value.trim()
  if (typeof value === 'number' || typeof value === 'boolean') return String(value)
  return ''
}

function getNumber(record: Record<string, unknown>, key: string): number {
  const value = record[key]
  if (typeof value === 'number' && Number.isFinite(value)) return value
  if (typeof value === 'string') {
    const parsed = Number.parseInt(value, 10)
    return Number.isFinite(parsed) ? parsed : 0
  }
  return 0
}

function getBool(record: Record<string, unknown>, key: string): boolean {
  const value = record[key]
  if (typeof value === 'boolean') return value
  if (typeof value === 'number') return value !== 0
  if (typeof value === 'string') return value === '1' || value.toLowerCase() === 'true'
  return false
}

function firstNonEmpty(...values: string[]): string {
  return values.find((value) => value.trim() !== '')?.trim() ?? ''
}

function trimUrl(value: string): string {
  return value.trim().replace(/^["'<>\[\]{}()]+|["'<>\[\]{}(),.!?;:，。！？；：）】]+$/g, '')
}

function isAcceptableHost(host: string): boolean {
  return isXhsHost(host) || isShortHost(host)
}

function isXhsHost(host: string): boolean {
  return host === 'xiaohongshu.com' || host.endsWith('.xiaohongshu.com')
}

function isShortHost(host: string): boolean {
  return host === 'xhslink.com' || host === 'www.xhslink.com'
}

function htmlUnescape(value: string): string {
  return value
    .replace(/&amp;/g, '&')
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
}

function getRaw(info: MediaInfo): XhsRaw {
  return (info.raw && typeof info.raw === 'object' ? (info.raw as XhsRaw) : {})
}

function xhsFileStem(info: MediaInfo): string {
  return safeStem([info.author, info.title, info.id].filter(Boolean).join('_'), info.id)
}

function safeStem(value: string, fallback: string): string {
  return value.replace(/[<>:"/\\|?*\x00-\x1f]+/g, '_').replace(/\s+/g, ' ').trim().slice(0, 120) || fallback
}

function extensionFromUrl(url: string | undefined, fallback: string): string {
  if (!url) return fallback
  try {
    const ext = new URL(url).pathname.split('/').pop()?.match(/\.([a-zA-Z0-9]{1,8})$/)?.[1]
    return ext?.toLowerCase() ?? fallback
  } catch {
    return fallback
  }
}

type ParsedStream = MediaStream & {
  url: string
  streamType?: number
  weight?: number
  defaultStream?: number
  videoCodec?: string
  videoBitrate?: number
}

type XhsRaw = {
  note?: Record<string, unknown>
  noteUrl?: string
  xsecToken?: string
}
