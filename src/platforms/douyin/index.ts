import type { DownloadEntry, DownloadOptions, DownloadPlan, MediaImage, MediaInfo, MediaStream, Platform, ResolveContext } from '../../core/types.js'

const defaultUserAgent = 'Mozilla/5.0 (iPhone; CPU iPhone OS 16_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.5 Mobile/15E148 Safari/604.1'
const defaultReferer = 'https://www.douyin.com/'
const shareBase = 'https://www.iesdouyin.com/share'
const playBaseUrl = 'https://aweme.snssdk.com/aweme/v1/play/'
const ttwidRegisterUrl = 'https://ttwid.bytedance.com/ttwid/union/register/'
const requestTimeoutMs = 30_000

const douyinHostPattern = /(?:^|\/\/)(?:[^/]+\.)?(?:douyin\.com|iesdouyin\.com)(?:[/:?#]|$)|(?:^|\/\/)v\.douyin\.com(?:[/:?#]|$)/i
const urlPattern = /https?:\/\/[^\s<>'"`，。；、）)】]+/gi
const numericIdPattern = /^\d{10,30}$/

export const douyinPlatform: Platform = {
  id: 'douyin',
  displayName: '抖音',
  match(input: string): boolean {
    return douyinHostPattern.test(input)
  },
  async resolve(input: string, ctx: ResolveContext): Promise<MediaInfo> {
    const awemeId = await parseDouyinInput(input, ctx)
    return fetchShareInfo(awemeId, ctx)
  },
  async createDownloadPlan(info: MediaInfo, options: DownloadOptions): Promise<DownloadPlan> {
    if (info.platform !== 'douyin') throw new Error(`expected douyin MediaInfo, got ${info.platform}`)

    const basePlan = {
      title: info.title,
      outputDir: options.outputDir ?? './downloads',
      concurrency: options.concurrency ?? 4,
      chunkConcurrency: options.chunkConcurrency ?? 4,
      multipartThresholdBytes: 32 * 1024 * 1024,
      timeoutMs: 30_000,
      ...(options.proxy ? { proxy: options.proxy } : {}),
      ffmpeg: 'ffmpeg',
      overwrite: Boolean(options.overwrite),
      noOverwrite: Boolean(options.noOverwrite),
    }
    const headers = douyinDownloadHeaders()

    if (info.type === 'video') {
      const stream = pickStream(info, options.quality)
      return {
        ...basePlan,
        items: [
          {
            id: info.id,
            kind: 'file',
            filename: `${douyinFileStem(info)}.${extensionFromUrl(stream.url, 'mp4')}`,
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
        const prefix = String(image.index + 1).padStart(2, '0')
        if (image.url) {
          entries.push({
            name: `${prefix}.${extensionFromUrl(image.url, 'jpg')}`,
            url: image.url,
            backupUrls: image.backupUrls ?? [],
            headers: image.headers ?? headers,
          })
        }
        if (image.videoUrl) {
          entries.push({
            name: `${prefix}${image.url ? '_video' : ''}.${extensionFromUrl(image.videoUrl, 'mp4')}`,
            url: image.videoUrl,
            backupUrls: [],
            headers,
          })
        }
      }
      if (entries.length === 0) throw new Error('douyin album has no downloadable entries')
      return {
        ...basePlan,
        items: [
          {
            id: info.id,
            kind: 'zip',
            filename: `${douyinFileStem(info)}.zip`,
            headers,
            entries,
          },
        ],
      }
    }

    throw new Error(`unsupported douyin media type: ${info.type}`)
  },
}

async function parseDouyinInput(input: string, ctx: ResolveContext): Promise<string> {
  const trimmed = input.trim()
  if (!trimmed) throw new Error('no douyin url found in input')
  if (numericIdPattern.test(trimmed)) return trimmed

  const url = extractShareUrl(trimmed)
  if (!url) throw new Error('no douyin url found in input')
  return parseDouyinUrl(url, 0, ctx)
}

async function parseDouyinUrl(rawUrl: string, depth: number, ctx: ResolveContext): Promise<string> {
  if (depth > 5) throw new Error('douyin short link resolution failed: too many redirects')
  const url = new URL(trimUrl(rawUrl))
  if (url.protocol !== 'http:' && url.protocol !== 'https:') throw new Error('invalid douyin url scheme')
  const host = url.hostname.toLowerCase()

  if (host === 'v.douyin.com') {
    const next = await resolveShortLink(url.href, ctx)
    return parseDouyinUrl(next, depth + 1, ctx)
  }
  if (!isDouyinHost(host)) throw new Error(`invalid douyin host: ${host}`)

  const id = extractAwemeId(url)
  if (!id) throw new Error('douyin aweme_id not found')
  return id
}

async function resolveShortLink(shortUrl: string, ctx: ResolveContext): Promise<string> {
  const response = await fetchWithTimeout(shortUrl, { redirect: 'manual', headers: douyinPageHeaders() }, ctx.proxy)
  await response.body?.cancel().catch(() => undefined)
  const location = response.headers.get('location')
  if (location && response.status >= 300 && response.status < 400) return new URL(location, shortUrl).href
  throw new Error(`douyin short link resolution failed: unexpected status ${response.status}`)
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

function extractAwemeId(url: URL): string | undefined {
  const pathMatch = url.pathname.match(/\/(?:video|note|share\/slides|share\/video|share\/note)\/(\d{10,30})/)
  if (pathMatch) return pathMatch[1]
  return url.searchParams.get('modal_id') ?? url.searchParams.get('aweme_id') ?? undefined
}

async function fetchShareInfo(awemeId: string, ctx: ResolveContext): Promise<MediaInfo> {
  let firstErr: unknown
  for (const kind of ['video', 'note']) {
    try {
      const item = await fetchSharePageItem(`${shareBase}/${kind}/${awemeId}/?app=aweme`, ctx)
      const info = await buildMediaInfo(item, awemeId, ctx)
      if (isUsable(info)) return info
      firstErr ??= new Error('douyin share item has metadata but no playable video URL; play endpoint returned no usable video stream, so the item may be removed, restricted, or app-only')
    } catch (err) {
      firstErr ??= err
    }
  }
  throw new Error(`douyin item fetch failed: ${firstErr instanceof Error ? firstErr.message : String(firstErr)}`)
}

async function fetchSharePageItem(url: string, ctx: ResolveContext): Promise<Record<string, unknown>> {
  try {
    return await fetchSharePageItemOnce(url, ctx)
  } catch (err) {
    if (!shouldRetrySharePageWithTtwid(err)) throw err
    const cookie = await createDouyinTtwidCookie(ctx).catch(() => undefined)
    if (!cookie) throw err
    return fetchSharePageItemOnce(url, ctx, cookie)
  }
}

async function fetchSharePageItemOnce(url: string, ctx: ResolveContext, cookieHeader?: string): Promise<Record<string, unknown>> {
  const response = await fetchWithTimeout(url, { headers: douyinPageHeaders(cookieHeader) }, ctx.proxy)
  if (response.status === 403 || response.status === 429) throw new Error('douyin request rate limited')
  if (!response.ok) throw new Error(`douyin share page HTTP ${response.status}`)

  const html = await response.text()
  const routerJson = extractJSONObjectAfterMarker(html, 'window._ROUTER_DATA')
  const payload = JSON.parse(routerJson) as Record<string, unknown>
  const loaderData = getRecord(payload.loaderData)
  for (const rawContainer of Object.values(loaderData)) {
    const container = getRecord(rawContainer)
    const videoInfoRes = getRecord(container.videoInfoRes)
    const itemList = Array.isArray(videoInfoRes.item_list) ? videoInfoRes.item_list : []
    const item = getRecord(itemList[0])
    if (Object.keys(item).length > 0) return item
  }
  throw new Error('douyin item not found in router data')
}

function shouldRetrySharePageWithTtwid(err: unknown): boolean {
  const message = err instanceof Error ? err.message : String(err)
  return message.includes('router data not found') || message.includes('item not found in router data')
}

async function createDouyinTtwidCookie(ctx: ResolveContext): Promise<string | undefined> {
  const body = {
    region: 'cn',
    aid: 1768,
    needFid: false,
    service: 'www.ixigua.com',
    migrate_info: { ticket: '', source: 'node' },
    cbUrlProtocol: 'https',
    union: true,
  }
  const response = await fetchWithTimeout(ttwidRegisterUrl, {
    method: 'POST',
    headers: { 'User-Agent': defaultUserAgent, 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  }, ctx.proxy)
  await response.body?.cancel().catch(() => undefined)
  const value = response.headers.get('set-cookie')?.match(/(?:^|[,\s])ttwid=([^;,\s]+)/)?.[1]
  return value ? `ttwid=${value}` : undefined
}

async function buildMediaInfo(item: Record<string, unknown>, fallbackId: string, ctx: ResolveContext): Promise<MediaInfo> {
  const id = firstNonEmpty(getString(item, 'aweme_id'), fallbackId)
  const desc = firstNonEmpty(getString(item, 'desc'), id)
  const author = getRecord(item.author)
  const video = getRecord(item.video)
  const images = parseImages(item)
  const isAlbum = getNumber(item, 'aweme_type') === 68 || images.length > 0

  if (isAlbum) {
    return {
      platform: 'douyin',
      id,
      type: 'album',
      title: desc,
      author: getString(author, 'nickname') || undefined,
      cover: firstUrl(getArray(getRecord(video.cover).url_list)) || images[0]?.url || images[0]?.videoUrl,
      images,
      raw: { item } satisfies DouyinRaw,
    }
  }

  let streams = parseStreams(video)
  if (streams.length === 0) {
    streams = await probeRatioStreams(getString(getRecord(video.play_addr), 'uri'), getNumber(getRecord(video.play_addr), 'width') || getNumber(video, 'width'), getNumber(getRecord(video.play_addr), 'height') || getNumber(video, 'height'), ctx)
  }

  return {
    platform: 'douyin',
    id,
    type: 'video',
    title: desc,
    author: getString(author, 'nickname') || undefined,
    cover: firstUrl(getArray(getRecord(video.cover).url_list)),
    streams,
    raw: { item } satisfies DouyinRaw,
  }
}

function parseStreams(video: Record<string, unknown>): ParsedStream[] {
  const bitRates = Array.isArray(video.bit_rate) ? video.bit_rate : []
  const fallbackWidth = getNumber(video, 'width')
  const fallbackHeight = getNumber(video, 'height')
  const streams: ParsedStream[] = []

  for (const value of bitRates) {
    const br = getRecord(value)
    const brPlay = getRecord(br.play_addr)
    const url = pickNoWatermarkUrl(getArray(brPlay.url_list))
    if (!url) continue
    const width = getNumber(brPlay, 'width') || fallbackWidth
    const height = getNumber(brPlay, 'height') || fallbackHeight
    const quality = qualityFromGear(getString(br, 'gear_name')) || resolutionKey(width, height) || 'source'
    streams.push({
      id: quality,
      quality,
      qualityName: quality,
      width: width || undefined,
      height: height || undefined,
      size: getNumber(br, 'size') || undefined,
      url,
      backupUrls: [],
      headers: douyinDownloadHeaders(),
      bitrate: getNumber(br, 'bit_rate'),
    })
  }

  return sortStreams(streams)
}

function parseImages(item: Record<string, unknown>): MediaImage[] {
  const images = Array.isArray(item.images) ? item.images : []
  return images.map((value, index) => {
    const image = getRecord(value)
    const imageUrls = uniqueStrings([...getArray(image.url_list), ...getArray(image.download_url_list).filter((url) => !looksVideoUrl(url))])
    const videoUrl = extractImageVideoUrl(image)
    return {
      index,
      url: imageUrls[0],
      backupUrls: imageUrls.slice(1),
      videoUrl,
      width: getNumber(image, 'width') || undefined,
      height: getNumber(image, 'height') || undefined,
      headers: douyinDownloadHeaders(),
    }
  }).filter((image) => image.url || image.videoUrl)
}

function extractImageVideoUrl(image: Record<string, unknown>): string | undefined {
  const videoPlay = getRecord(getRecord(image.video).play_addr)
  const explicit = pickNoWatermarkUrl(getArray(videoPlay.url_list))
  if (explicit) return explicit
  const fromDownloadList = pickNoWatermarkUrl(getArray(image.download_url_list).filter(looksVideoUrl))
  if (fromDownloadList) return fromDownloadList
  const uri = getString(videoPlay, 'uri')
  return uri ? buildPlayUrl(uri, resolutionKey(getNumber(image, 'width'), getNumber(image, 'height'))) : undefined
}

async function probeRatioStreams(uri: string, sourceW: number, sourceH: number, ctx: ResolveContext): Promise<ParsedStream[]> {
  if (!uri) return []
  const maxSourceResolution = sourceW > 0 && sourceH > 0 ? Math.min(sourceW, sourceH) : 0
  const ratios = ['1080p', '720p', '540p', '480p', '360p'].filter((ratio) => !maxSourceResolution || Number.parseInt(ratio, 10) <= maxSourceResolution)
  const seenSizes = new Set<number>()
  const streams: ParsedStream[] = []
  for (const ratio of ratios) {
    const url = buildPlayUrl(uri, ratio)
    const response = await fetchWithTimeout(url, { headers: { ...douyinDownloadHeaders(), Range: 'bytes=0-1', Accept: '*/*' } }, ctx.proxy).catch(() => undefined)
    if (!response) continue
    await response.body?.cancel().catch(() => undefined)
    const total = parseTotalFromContentRange(response.headers.get('content-range'))
    const contentType = response.headers.get('content-type') ?? ''
    if (response.status !== 206 || total <= 2 || !isVideoLikeContentType(contentType) || seenSizes.has(total)) continue
    seenSizes.add(total)
    const [width, height] = dimensionsForRatio(ratio, sourceW, sourceH)
    streams.push({ id: ratio, quality: ratio, qualityName: ratio, width: width || undefined, height: height || undefined, size: total, url, backupUrls: [], headers: douyinDownloadHeaders() })
  }
  return sortStreams(streams)
}

function pickStream(info: MediaInfo, quality = 'best'): ParsedStream {
  const streams = (info.streams ?? []).filter((stream): stream is ParsedStream => Boolean(stream.url))
  if (streams.length === 0) throw new Error('douyin video has no downloadable stream')
  const wanted = quality.trim().toLowerCase()
  if (wanted && wanted !== 'best') {
    const match = streams.find((stream) => stream.id.toLowerCase() === wanted || stream.quality.toLowerCase() === wanted || stream.qualityName?.toLowerCase() === wanted)
    if (match) return match
    throw new Error(`douyin quality not found: ${quality}; available: ${streams.map((stream) => stream.id).join(', ')}`)
  }
  return streams[0]
}

function selectImages(images: MediaImage[], selection: number[] | undefined): MediaImage[] {
  if (!selection?.length) return images
  return selection.map((index) => {
    const image = images[index]
    if (!image) throw new Error(`douyin image index out of range: ${index + 1} (total ${images.length})`)
    return image
  })
}

function extractJSONObjectAfterMarker(html: string, marker: string): string {
  const markerIndex = html.indexOf(marker)
  if (markerIndex < 0) throw new Error('douyin router data not found')
  const start = html.indexOf('{', markerIndex)
  if (start < 0) throw new Error('douyin router data not found')

  let depth = 0
  let inString = false
  let escape = false
  for (let i = start; i < html.length; i++) {
    const ch = html[i]
    if (inString) {
      if (escape) escape = false
      else if (ch === '\\') escape = true
      else if (ch === '"') inString = false
      continue
    }
    if (ch === '"') inString = true
    else if (ch === '{') depth++
    else if (ch === '}') {
      depth--
      if (depth === 0) return html.slice(start, i + 1)
    }
  }
  throw new Error('douyin router data not found')
}

async function fetchWithTimeout(url: string, init: RequestInit, proxy?: string): Promise<Response> {
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(), requestTimeoutMs)
  const requestInit = withProxy({ ...init, signal: controller.signal }, proxy)
  try {
    return await fetch(url, requestInit)
  } catch (err) {
    if (err instanceof Error && err.name === 'AbortError') throw new Error(`douyin request timed out after ${requestTimeoutMs}ms`)
    throw err
  } finally {
    clearTimeout(timer)
  }
}

function withProxy(init: RequestInit, proxy: string | undefined): RequestInit {
  const trimmed = proxy?.trim()
  return trimmed ? ({ ...init, proxy: trimmed } as RequestInit) : init
}

function douyinPageHeaders(cookieHeader?: string): Record<string, string> {
  return {
    'User-Agent': defaultUserAgent,
    Accept: 'application/json, text/plain, */*',
    'Accept-Language': 'zh-CN,zh;q=0.9,en;q=0.8',
    Origin: 'https://www.douyin.com',
    Referer: defaultReferer,
    ...(cookieHeader ? { Cookie: cookieHeader } : {}),
  }
}

function douyinDownloadHeaders(): Record<string, string> {
  return { 'User-Agent': defaultUserAgent, Referer: defaultReferer }
}

function isUsable(info: MediaInfo): boolean {
  return info.type === 'video' ? Boolean(info.streams?.length) : info.type === 'album' ? Boolean(info.images?.length) : false
}

function pickNoWatermarkUrl(urls: string[]): string | undefined {
  const cleaned = uniqueStrings(urls)
  const selected = cleaned.find((url) => !url.includes('playwm')) ?? cleaned[0]
  return selected?.replaceAll('playwm', 'play')
}

function firstUrl(value: unknown[]): string | undefined {
  return value.find((item): item is string => typeof item === 'string' && item.trim() !== '')?.trim()
}

function getArray(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === 'string' && item.trim() !== '').map((item) => item.trim()) : []
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

function firstNonEmpty(...values: string[]): string {
  return values.find((value) => value.trim() !== '')?.trim() ?? ''
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

function sortStreams<T extends MediaStream>(streams: T[]): T[] {
  return [...streams].sort((a, b) => (b.height ?? 0) - (a.height ?? 0) || (b.width ?? 0) - (a.width ?? 0) || ((b as ParsedStream).bitrate ?? 0) - ((a as ParsedStream).bitrate ?? 0))
}

function qualityFromGear(gear: string): string {
  const match = gear.toLowerCase().match(/(\d{3,4})p?/)
  return match ? `${match[1]}p` : ''
}

function resolutionKey(width: number, height: number): string {
  if (width <= 0 || height <= 0) return ''
  return `${Math.min(width, height)}p`
}

function dimensionsForRatio(ratio: string, sourceW: number, sourceH: number): [number, number] {
  const res = Number.parseInt(ratio, 10)
  if (!res || sourceW <= 0 || sourceH <= 0) return [0, 0]
  return sourceW < sourceH ? [res, Math.round((res * sourceH) / sourceW)] : [Math.round((res * sourceW) / sourceH), res]
}

function buildPlayUrl(uri: string, ratio = ''): string {
  const url = new URL(playBaseUrl)
  url.searchParams.set('video_id', uri)
  if (ratio) url.searchParams.set('ratio', ratio)
  url.searchParams.set('line', '0')
  return url.href
}

function parseTotalFromContentRange(value: string | null): number {
  const total = value?.match(/\/(\d+)\s*$/)?.[1]
  return total ? Number.parseInt(total, 10) : 0
}

function isVideoLikeContentType(value: string): boolean {
  const lower = value.toLowerCase()
  return !lower || lower.includes('video') || lower.includes('octet-stream')
}

function looksVideoUrl(url: string): boolean {
  const lower = url.toLowerCase()
  return lower.includes('mime_type=video') || lower.includes('video_mp4') || /\.(mp4|mov|m4v)(?:[?#]|$)/.test(lower)
}

function trimUrl(value: string): string {
  return value.trim().replace(/^["'<>\[\]{}()]+|["'<>\[\]{}(),.!?;:，。！？；：）】]+$/g, '')
}

function isAcceptableHost(host: string): boolean {
  return host === 'v.douyin.com' || isDouyinHost(host)
}

function isDouyinHost(host: string): boolean {
  return host === 'douyin.com' || host.endsWith('.douyin.com') || host === 'iesdouyin.com' || host.endsWith('.iesdouyin.com')
}

function douyinFileStem(info: MediaInfo): string {
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

type ParsedStream = MediaStream & { url: string; bitrate?: number }

type DouyinRaw = { item?: Record<string, unknown> }
