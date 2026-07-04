import { loadAuth } from '../../core/auth-store.js'
import { loadCookieHeader } from '../../core/cookies.js'
import type { DownloadOptions, DownloadPlan, MediaInfo, MediaPart, MediaStream, Platform, ResolveContext } from '../../core/types.js'

const defaultUserAgent = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36'
const defaultReferer = 'https://www.bilibili.com/'
const requestTimeoutMs = 30_000

const bilibiliPattern = /(?:^|\/\/)(?:[^/]+\.)?(?:bilibili\.com|b23\.tv)(?:[/:?#]|$)|\b(?:BV[0-9A-Za-z]+|av\d+|ep\d+|ss\d+|md\d+)\b/i
const urlPattern = /https?:\/\/[^\s<>'"`，。；、）)】]+/gi
const bvPattern = /\bBV[0-9A-Za-z]+\b/i
const avPattern = /\bav(\d+)\b/i
const epPattern = /(?:^|\/bangumi\/play\/|\b)ep(\d+)\b/i
const ssPattern = /(?:^|\/bangumi\/play\/|\b)ss(\d+)\b/i
const mdPattern = /(?:^|\/bangumi\/media\/|\b)md(\d+)\b/i

type ParsedBiliInput =
  | { kind: 'video'; id: string; page?: number }
  | { kind: 'bangumi'; idKind: 'ep' | 'season' | 'media'; id: string }

export const bilibiliPlatform: Platform = {
  id: 'bilibili',
  displayName: 'B站',
  match(input: string): boolean {
    return bilibiliPattern.test(input)
  },
  async resolve(input: string, ctx: ResolveContext): Promise<MediaInfo> {
    const parsed = await parseBilibiliInput(input)
    if (parsed.kind === 'bangumi') return resolveBangumi(parsed, ctx)
    return resolveOrdinaryVideo(parsed, ctx)
  },
  async createDownloadPlan(info: MediaInfo, options: DownloadOptions): Promise<DownloadPlan> {
    if (info.platform !== 'bilibili') throw new Error(`expected bilibili MediaInfo, got ${info.platform}`)
    const raw = getRaw(info)
    const parts = raw.parts ?? []
    if (parts.length === 0) throw new Error('bilibili media has no parts')

    const selected = selectParts(parts, options, raw.currentIndex)
    const items = []
    for (const part of selected) {
      const streams = raw.streamsByPart?.[part.index] ?? await fetchPartStreams(raw.kind, part, raw.ctx)
      const stream = pickStream(streams, options.quality)
      items.push({
        id: selected.length === 1 ? info.id : `${info.id}-p${part.page}`,
        kind: 'dash' as const,
        filename: `${biliFileStem(info, selected.length > 1 ? part : undefined)}.mp4`,
        videoUrl: stream.url,
        audioUrl: stream.audioUrl,
        videoBackupUrls: stream.videoBackupUrls ?? stream.backupUrls ?? [],
        audioBackupUrls: stream.audioBackupUrls ?? [],
        headers: biliDownloadHeaders(),
      })
    }

    return {
      title: info.title,
      outputDir: options.outputDir ?? './downloads',
      concurrency: options.concurrency ?? 4,
      chunkConcurrency: options.chunkConcurrency ?? 4,
      multipartThresholdBytes: 32 * 1024 * 1024,
      timeoutMs: 30_000,
      ffmpeg: 'ffmpeg',
      overwrite: Boolean(options.overwrite),
      noOverwrite: Boolean(options.noOverwrite),
      items,
    }
  },
}

async function parseBilibiliInput(input: string): Promise<ParsedBiliInput> {
  const trimmed = input.trim()
  if (!trimmed) throw new Error('empty bilibili input')

  const bare = parseBiliIdentifier(trimmed)
  if (bare) return bare

  const rawUrl = extractShareUrl(trimmed)
  if (!rawUrl) throw new Error('could not extract bilibili URL or identifier')
  const resolvedUrl = await resolveB23IfNeeded(rawUrl)
  const normalized = normalizeBiliUrl(resolvedUrl)
  const parsed = parseBiliIdentifier(normalized)
  if (parsed) return parsed
  throw new Error('could not extract bilibili video/bangumi identifier')
}

function parseBiliIdentifier(input: string): ParsedBiliInput | undefined {
  try {
    const url = new URL(input)
    const page = Number.parseInt(url.searchParams.get('p') ?? '', 10)
    const withSearch = `${url.pathname}${url.search}`
    const ep = withSearch.match(epPattern)?.[1]
    if (ep) return { kind: 'bangumi', idKind: 'ep', id: ep }
    const ss = withSearch.match(ssPattern)?.[1]
    if (ss) return { kind: 'bangumi', idKind: 'season', id: ss }
    const md = withSearch.match(mdPattern)?.[1]
    if (md) return { kind: 'bangumi', idKind: 'media', id: md }
    const pathBv = withSearch.match(bvPattern)?.[0]
    if (pathBv) return { kind: 'video', id: pathBv, page: Number.isFinite(page) && page > 0 ? page : undefined }
    const pathAv = withSearch.match(avPattern)?.[1]
    if (pathAv) return { kind: 'video', id: `av${pathAv}`, page: Number.isFinite(page) && page > 0 ? page : undefined }
  } catch {
    // Not a URL; try bare identifiers below.
  }

  const ep = input.match(epPattern)?.[1]
  if (ep) return { kind: 'bangumi', idKind: 'ep', id: ep }
  const ss = input.match(ssPattern)?.[1]
  if (ss) return { kind: 'bangumi', idKind: 'season', id: ss }
  const md = input.match(mdPattern)?.[1]
  if (md) return { kind: 'bangumi', idKind: 'media', id: md }
  const bv = input.match(bvPattern)?.[0]
  if (bv) return { kind: 'video', id: bv }
  const av = input.match(avPattern)?.[1]
  if (av) return { kind: 'video', id: `av${av}` }
  return undefined
}

async function resolveB23IfNeeded(rawUrl: string): Promise<string> {
  const url = new URL(rawUrl)
  if (url.hostname.toLowerCase() !== 'b23.tv') return rawUrl
  let current = rawUrl
  for (let i = 0; i < 5; i++) {
    const response = await fetchWithTimeout(current, { redirect: 'manual', headers: biliPageHeaders() })
    await response.body?.cancel().catch(() => undefined)
    const location = response.headers.get('location')
    if (location && response.status >= 300 && response.status < 400) {
      current = new URL(location, current).href
      if (!new URL(current).hostname.toLowerCase().endsWith('b23.tv')) return current
      continue
    }
    return response.url || current
  }
  throw new Error('b23 short link resolution failed: too many redirects')
}

function normalizeBiliUrl(rawUrl: string): string {
  const festival = rawUrl.match(/bilibili\.com\/festival\/.*?[?&]bvid=([A-Za-z0-9]+)/i)?.[1]
  if (festival) return `https://www.bilibili.com/video/${festival}`
  return rawUrl
}

async function resolveOrdinaryVideo(parsed: Extract<ParsedBiliInput, { kind: 'video' }>, ctx: ResolveContext): Promise<MediaInfo> {
  const view = await fetchVideoView(parsed.id, ctx)
  const parts: BiliPart[] = view.pages.map((page, index) => ({
    index,
    page: page.page || index + 1,
    title: page.part || view.title || `P${index + 1}`,
    cid: page.cid,
    bvid: view.bvid,
    aid: view.aid,
    duration: page.duration,
  }))
  if (parts.length === 0) parts.push({ index: 0, page: 1, title: view.title, cid: view.cid, bvid: view.bvid, aid: view.aid, duration: view.duration })

  const currentIndex = parsed.page ? Math.max(0, Math.min(parts.length - 1, parsed.page - 1)) : 0
  const current = parts[currentIndex]
  const streams = await fetchPartStreams('video', current, ctx)
  const streamsByPart: Record<number, BiliStream[]> = { [current.index]: streams }

  return {
    platform: 'bilibili',
    id: view.bvid || `av${view.aid}`,
    type: parts.length > 1 ? 'playlist' : 'video',
    title: view.title,
    author: view.owner?.name,
    cover: view.pic,
    streams,
    parts: parts.map(toMediaPart),
    raw: { kind: 'video', ctx, parts, currentIndex, streamsByPart } satisfies BiliRaw,
  }
}

async function resolveBangumi(parsed: Extract<ParsedBiliInput, { kind: 'bangumi' }>, ctx: ResolveContext): Promise<MediaInfo> {
  const season = await fetchBangumiSeason(parsed, ctx)
  const selectedIndex = selectCurrentEpisode(season.episodes, parsed)
  const parts: BiliPart[] = season.episodes.map((ep, index) => ({
    index,
    page: index + 1,
    title: [ep.title, ep.long_title].filter(Boolean).join(' '),
    cid: ep.cid,
    bvid: ep.bvid,
    aid: ep.aid,
    epId: ep.ep_id || ep.id,
    duration: ep.duration,
    badge: ep.badge,
  }))
  const current = parts[selectedIndex]
  const streams = await fetchPartStreams('bangumi', current, ctx)
  const streamsByPart: Record<number, BiliStream[]> = { [current.index]: streams }

  return {
    platform: 'bilibili',
    id: current.bvid || `ep${current.epId}`,
    type: 'bangumi',
    title: season.season_title || season.title,
    author: season.title,
    cover: season.cover || current.cover,
    streams,
    parts: parts.map(toMediaPart),
    raw: { kind: 'bangumi', ctx, parts, currentIndex: selectedIndex, streamsByPart } satisfies BiliRaw,
  }
}

async function fetchVideoView(videoId: string, ctx: ResolveContext): Promise<BiliViewData> {
  const url = new URL('https://api.bilibili.com/x/web-interface/view')
  if (videoId.toLowerCase().startsWith('av')) url.searchParams.set('aid', videoId.slice(2))
  else url.searchParams.set('bvid', videoId)
  const payload = await fetchJson<BiliAPIResponse<BiliViewData>>(url.href, ctx)
  if (payload.code !== 0) throw new Error(`bilibili view API error ${payload.code}: ${payload.message ?? ''}`.trim())
  return payload.data
}

async function fetchBangumiSeason(parsed: Extract<ParsedBiliInput, { kind: 'bangumi' }>, ctx: ResolveContext): Promise<BiliSeasonData> {
  const params = new URLSearchParams()
  if (parsed.idKind === 'ep') params.set('ep_id', parsed.id)
  else if (parsed.idKind === 'season') params.set('season_id', parsed.id)
  else params.set('media_id', parsed.id)

  let payload = await fetchJson<BiliPGCResponse<BiliSeasonData>>(`https://api.bilibili.com/pgc/view/web/season?${params}`, ctx)
  if (payload.code !== 0 && parsed.idKind === 'media') {
    const seasonId = await resolveMediaSeasonId(parsed.id, ctx)
    payload = await fetchJson<BiliPGCResponse<BiliSeasonData>>(`https://api.bilibili.com/pgc/view/web/season?season_id=${seasonId}`, ctx)
  }
  if (payload.code !== 0) throw new Error(`bilibili season API error ${payload.code}: ${payload.message ?? ''}`.trim())
  if (!payload.result.episodes?.length) throw new Error('bilibili season has no episodes')
  return payload.result
}

async function resolveMediaSeasonId(mediaId: string, ctx: ResolveContext): Promise<number> {
  const payload = await fetchJson<BiliPGCResponse<{ season_id?: number; media?: { season_id?: number } }>>(`https://api.bilibili.com/pgc/review/user?media_id=${encodeURIComponent(mediaId)}`, ctx)
  if (payload.code !== 0) throw new Error(`bilibili media API error ${payload.code}: ${payload.message ?? ''}`.trim())
  const id = payload.result.media?.season_id || payload.result.season_id
  if (!id) throw new Error(`season_id not found for media_id=${mediaId}`)
  return id
}

async function fetchPartStreams(kind: 'video' | 'bangumi', part: BiliPart, ctx: ResolveContext): Promise<BiliStream[]> {
  const url = kind === 'bangumi' ? buildBangumiPlayUrl(part) : buildVideoPlayUrl(part)
  const payload = await fetchJson<BiliPlayResponse>(url, ctx)
  const data = payload.data ?? payload.result
  if (payload.code !== 0) throw new Error(`bilibili playurl API error ${payload.code}: ${payload.message ?? ''}`.trim())
  if (!data) throw new Error('bilibili playurl response missing data')
  if (hasBiliDrm(data)) throw new Error('bilibili DRM protected content is not supported')
  const streams = buildBiliStreams(data)
  if (streams.length === 0) throw new Error('bilibili playurl has no DASH video/audio streams; login or VIP permission may be required')
  return streams
}

function buildVideoPlayUrl(part: BiliPart): string {
  const url = new URL('https://api.bilibili.com/x/player/playurl')
  if (part.bvid) url.searchParams.set('bvid', part.bvid)
  else if (part.aid) url.searchParams.set('avid', String(part.aid))
  url.searchParams.set('cid', String(part.cid))
  url.searchParams.set('fnval', '4048')
  url.searchParams.set('fnver', '0')
  url.searchParams.set('fourk', '1')
  return url.href
}

function buildBangumiPlayUrl(part: BiliPart): string {
  const url = new URL('https://api.bilibili.com/pgc/player/web/playurl')
  if (part.bvid) url.searchParams.set('bvid', part.bvid)
  if (part.epId) url.searchParams.set('ep_id', String(part.epId))
  url.searchParams.set('cid', String(part.cid))
  url.searchParams.set('qn', '127')
  url.searchParams.set('fnval', '4048')
  url.searchParams.set('fnver', '0')
  url.searchParams.set('fourk', '1')
  url.searchParams.set('drm_tech_type', '2')
  url.searchParams.set('otype', 'json')
  url.searchParams.set('platform', 'web')
  return url.href
}

function buildBiliStreams(data: BiliPlayData): BiliStream[] {
  const audio = [...(data.dash?.audio ?? [])].sort((a, b) => (b.bandwidth ?? 0) - (a.bandwidth ?? 0))[0]
  if (!audio) return []

  const byQuality = new Map<number, BiliDashVideo[]>()
  for (const video of data.dash?.video ?? []) {
    if (!dashUrl(video)) continue
    const id = video.id ?? 0
    const list = byQuality.get(id) ?? []
    list.push(video)
    byQuality.set(id, list)
  }

  const streams: BiliStream[] = []
  for (const [quality, videos] of byQuality) {
    const video = [...videos].sort(compareDashVideo)[0]
    const videoUrl = dashUrl(video)
    const audioUrl = dashUrl(audio)
    if (!videoUrl || !audioUrl) continue
    streams.push({
      id: String(quality),
      quality: String(quality),
      qualityName: qualityName(data, quality),
      width: video.width,
      height: video.height,
      size: (video.bandwidth ?? 0) + (audio.bandwidth ?? 0),
      url: videoUrl,
      audioUrl,
      backupUrls: dashBackups(video),
      videoBackupUrls: dashBackups(video),
      audioBackupUrls: dashBackups(audio),
      headers: biliDownloadHeaders(),
      codecId: video.codecid,
      bandwidth: video.bandwidth,
    })
  }
  return streams.sort((a, b) => Number(b.quality) - Number(a.quality))
}

function compareDashVideo(a: BiliDashVideo, b: BiliDashVideo): number {
  return codecRank(b.codecid) - codecRank(a.codecid) || (b.bandwidth ?? 0) - (a.bandwidth ?? 0)
}

function codecRank(codecid: number | undefined): number {
  if (codecid === 13) return 3
  if (codecid === 12) return 2
  if (codecid === 7) return 1
  return 0
}

function qualityName(data: BiliPlayData, quality: number): string {
  const fmt = data.support_formats?.find((item) => item.quality === quality)
  return fmt?.new_description || fmt?.display_desc || fmt?.format || data.accept_description?.[data.accept_quality?.indexOf(quality) ?? -1] || defaultQualityName(quality)
}

function defaultQualityName(quality: number): string {
  return ({ 127: '8K', 126: 'Dolby Vision', 125: 'HDR', 120: '4K', 116: '1080P60', 112: '1080P+', 80: '1080P', 74: '720P60', 64: '720P', 32: '480P', 16: '360P' } as Record<number, string>)[quality] ?? `未知(${quality})`
}

function hasBiliDrm(data: BiliPlayData): boolean {
  return Boolean(JSON.stringify(data).includes('bilidrm_uri'))
}

async function fetchJson<T>(url: string, ctx: ResolveContext): Promise<T> {
  const response = await fetchWithTimeout(url, { headers: await biliApiHeaders(ctx, url) })
  if (!response.ok) throw new Error(`bilibili API HTTP ${response.status}`)
  return (await response.json()) as T
}

async function biliApiHeaders(ctx: ResolveContext, url: string): Promise<Record<string, string>> {
  const headers = biliPageHeaders()
  const auth = await loadAuth(ctx.configDir).catch(() => undefined)
  const cookies = [auth?.bilibili?.sessData ? `SESSDATA=${auth.bilibili.sessData}` : '', await loadCookieHeader(ctx.cookiesPath, url).catch(() => undefined)]
    .filter(Boolean)
    .join('; ')
  if (cookies) headers.Cookie = cookies
  return headers
}

function biliPageHeaders(): Record<string, string> {
  return { 'User-Agent': defaultUserAgent, Referer: defaultReferer, Accept: 'application/json, text/plain, */*' }
}

function biliDownloadHeaders(): Record<string, string> {
  return { 'User-Agent': defaultUserAgent, Referer: defaultReferer }
}

async function fetchWithTimeout(url: string, init: RequestInit): Promise<Response> {
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(), requestTimeoutMs)
  try {
    return await fetch(url, { ...init, signal: controller.signal })
  } catch (err) {
    if (err instanceof Error && err.name === 'AbortError') throw new Error(`bilibili request timed out after ${requestTimeoutMs}ms`)
    throw err
  } finally {
    clearTimeout(timer)
  }
}

function pickStream(streams: MediaStream[], quality = 'best'): BiliStream {
  const biliStreams = streams.filter((stream): stream is BiliStream => Boolean(stream.url && stream.audioUrl))
  if (biliStreams.length === 0) throw new Error('bilibili has no downloadable DASH stream')
  const wanted = quality.trim().toLowerCase()
  if (wanted && wanted !== 'best') {
    const match = biliStreams.find((stream) => [stream.id, stream.quality, stream.qualityName]
      .filter((value): value is string => Boolean(value))
      .some((value) => value.toLowerCase() === wanted))
    if (match) return match
    throw new Error(`bilibili quality not found: ${quality}; available: ${biliStreams.map((stream) => `${stream.id}(${stream.qualityName})`).join(', ')}`)
  }
  return biliStreams[0]
}

function selectParts(parts: BiliPart[], options: DownloadOptions, currentIndex: number): BiliPart[] {
  if (options.allParts) return parts
  if (options.part !== undefined) {
    const selected = parts[options.part - 1]
    if (!selected) throw new Error(`bilibili part out of range: ${options.part} (total ${parts.length})`)
    return [selected]
  }
  return [parts[Math.max(0, Math.min(parts.length - 1, currentIndex))] ?? parts[0]]
}

function selectCurrentEpisode(episodes: BiliEpisode[], parsed: Extract<ParsedBiliInput, { kind: 'bangumi' }>): number {
  if (parsed.idKind === 'ep') {
    const index = episodes.findIndex((ep) => String(ep.ep_id || ep.id) === parsed.id)
    if (index >= 0) return index
  }
  const formal = episodes.findIndex((ep) => (ep.section_type ?? 0) === 0)
  return formal >= 0 ? formal : 0
}

function toMediaPart(part: BiliPart): MediaPart {
  return { index: part.index, title: part.title, duration: part.duration, streams: undefined }
}

function extractShareUrl(input: string): string | undefined {
  if (/^https?:\/\//i.test(input)) return trimUrl(input)
  return input.match(urlPattern)?.[0] ? trimUrl(input.match(urlPattern)![0]) : undefined
}

function dashUrl(item: BiliDashItem): string | undefined {
  const backups = dashBackups(item)
  return item.baseUrl || item.base_url || backups[0]
}

function dashBackups(item: BiliDashItem): string[] {
  return uniqueStrings([...(item.backupUrl ?? []), ...(item.backup_url ?? [])])
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

function getRaw(info: MediaInfo): BiliRaw {
  return info.raw && typeof info.raw === 'object' ? (info.raw as BiliRaw) : { kind: 'video', ctx: { configDir: '' }, parts: [], currentIndex: 0 }
}

function biliFileStem(info: MediaInfo, part: BiliPart | undefined): string {
  const title = part ? `${info.title}_P${String(part.page).padStart(2, '0')}_${part.title}` : [info.author, info.title, info.id].filter(Boolean).join('_')
  return safeStem(title, info.id)
}

function safeStem(value: string, fallback: string): string {
  return value.replace(/[<>:"/\\|?*\x00-\x1f]+/g, '_').replace(/\s+/g, ' ').trim().slice(0, 120) || fallback
}

function trimUrl(value: string): string {
  return value.trim().replace(/^["'<>\[\]{}()]+|["'<>\[\]{}(),.!?;:，。！？；：）】]+$/g, '')
}

type BiliRaw = {
  kind: 'video' | 'bangumi'
  ctx: ResolveContext
  parts: BiliPart[]
  currentIndex: number
  streamsByPart?: Record<number, BiliStream[]>
}

type BiliPart = {
  index: number
  page: number
  title: string
  cid: number
  bvid?: string
  aid?: number
  epId?: number
  duration?: number
  badge?: string
  cover?: string
}

type BiliStream = MediaStream & {
  url: string
  audioUrl: string
  videoBackupUrls?: string[]
  audioBackupUrls?: string[]
  codecId?: number
  bandwidth?: number
}

type BiliAPIResponse<T> = { code: number; message?: string; data: T }
type BiliPGCResponse<T> = { code: number; message?: string; result: T }
type BiliPlayResponse = { code: number; message?: string; data?: BiliPlayData; result?: BiliPlayData }

type BiliViewData = {
  bvid: string
  aid: number
  title: string
  pic?: string
  duration?: number
  owner?: { name?: string }
  cid: number
  pages: Array<{ cid: number; page: number; part: string; duration?: number }>
}

type BiliSeasonData = {
  season_id?: number
  media_id?: number
  season_title?: string
  title: string
  cover?: string
  episodes: BiliEpisode[]
}

type BiliEpisode = {
  aid?: number
  bvid?: string
  cid: number
  ep_id?: number
  id?: number
  title?: string
  long_title?: string
  badge?: string
  duration?: number
  cover?: string
  section_type?: number
}

type BiliPlayData = {
  accept_quality?: number[]
  accept_description?: string[]
  support_formats?: Array<{ quality: number; new_description?: string; display_desc?: string; format?: string }>
  dash?: { video?: BiliDashVideo[]; audio?: BiliDashItem[] }
}

type BiliDashItem = {
  id?: number
  baseUrl?: string
  base_url?: string
  backupUrl?: string[]
  backup_url?: string[]
  bandwidth?: number
  width?: number
  height?: number
  codecid?: number
}

type BiliDashVideo = BiliDashItem & {
  id: number
  width?: number
  height?: number
  codecid?: number
}
