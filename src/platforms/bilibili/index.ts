import { loadAuth } from '../../core/auth-store.js'
import { loadCookieHeader } from '../../core/cookies.js'
import type { DownloadEntry, DownloadOptions, DownloadPlan, MediaInfo, MediaPart, MediaStream, Platform, ResolveContext } from '../../core/types.js'

const defaultUserAgent = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36'
const defaultReferer = 'https://www.bilibili.com/'
const requestTimeoutMs = 30_000

const bilibiliPattern = /(?:^|\/\/)(?:[^/]+\.)?(?:bilibili\.com|b23\.tv)(?:[/:?#]|$)|(?:^|\/\/)(?:[^/]+\.)?gf\.bilibili\.com(?:[/:?#]|$)|\b(?:BV[0-9A-Za-z]+|av\d+|ep\d+|ss\d+|md\d+)\b/i
const urlPattern = /https?:\/\/[^\s<>'"`，。；、）)】]+/gi
const bvPattern = /\bBV[0-9A-Za-z]+\b/i
const avPattern = /\bav(\d+)\b/i
const epPattern = /(?:^|\/bangumi\/play\/|\b)ep(\d+)\b/i
const ssPattern = /(?:^|\/bangumi\/play\/|\b)ss(\d+)\b/i
const mdPattern = /(?:^|\/bangumi\/media\/|\b)md(\d+)\b/i

type ParsedBiliInput =
  | { kind: 'video'; id: string; page?: number }
  | { kind: 'bangumi'; idKind: 'ep' | 'season' | 'media'; id: string }
  | { kind: 'collection'; mid: string; seasonId: string }
  | { kind: 'series'; mid: string; seriesId: string }
  | { kind: 'mediaList'; rawUrl: string; upId: string }
  | { kind: 'cheese'; idKind: 'ep' | 'season' | 'paid'; id?: string }
  | { kind: 'gongfang'; orderId: string }

export const bilibiliPlatform: Platform = {
  id: 'bilibili',
  displayName: 'B站',
  match(input: string): boolean {
    return bilibiliPattern.test(input)
  },
  async resolve(input: string, ctx: ResolveContext): Promise<MediaInfo> {
    const parsed = await parseBilibiliInput(input, ctx)
    if (parsed.kind === 'bangumi') return resolveBangumi(parsed, ctx)
    if (parsed.kind === 'collection' || parsed.kind === 'series' || parsed.kind === 'mediaList') return resolveBiliPlaylist(parsed, ctx)
    if (parsed.kind === 'cheese') return resolveCheese(parsed, ctx)
    if (parsed.kind === 'gongfang') return resolveGongfang(parsed, ctx)
    return resolveOrdinaryVideo(parsed, ctx)
  },
  async createDownloadPlan(info: MediaInfo, options: DownloadOptions): Promise<DownloadPlan> {
    if (info.platform !== 'bilibili') throw new Error(`expected bilibili MediaInfo, got ${info.platform}`)
    const raw = getRaw(info)
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

    if (raw.kind === 'gongfang') {
      if (!raw.entries?.length) throw new Error('bilibili gongfang has no downloadable files')
      return {
        ...basePlan,
        items: [{ id: info.id, kind: 'zip', filename: `${safeStem(info.title, info.id)}.zip`, headers: biliDownloadHeaders(), entries: raw.entries }],
      }
    }

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
      for (const subtitle of part.subtitles ?? []) {
        items.push({
          id: `${info.id}-p${part.page}-sub-${subtitle.language}`,
          kind: 'file' as const,
          filename: `${biliFileStem(info, selected.length > 1 ? part : undefined)}.${safeStem(subtitle.language, 'subtitle')}.json`,
          url: subtitle.url,
          backupUrls: [],
          headers: biliDownloadHeaders(),
        })
      }
    }

    return { ...basePlan, items }
  },
}

async function parseBilibiliInput(input: string, ctx: ResolveContext): Promise<ParsedBiliInput> {
  const trimmed = input.trim()
  if (!trimmed) throw new Error('empty bilibili input')

  const bare = parseBiliIdentifier(trimmed)
  if (bare) return bare

  const rawUrl = extractShareUrl(trimmed)
  if (!rawUrl) throw new Error('could not extract bilibili URL or identifier')
  const resolvedUrl = await resolveB23IfNeeded(rawUrl, ctx)
  const normalized = normalizeBiliUrl(resolvedUrl)
  const parsed = parseBiliIdentifier(normalized)
  if (parsed) return parsed
  throw new Error('could not extract bilibili video/bangumi identifier')
}

function parseBiliIdentifier(input: string): ParsedBiliInput | undefined {
  try {
    const url = new URL(input)
    const page = Number.parseInt(url.searchParams.get('p') ?? '', 10)
    const host = url.hostname.toLowerCase()
    const withSearch = `${url.pathname}${url.search}`
    const cheeseEp = withSearch.match(/\/cheese\/play\/ep(\d+)/i)?.[1]
    if (cheeseEp) return { kind: 'cheese', idKind: 'ep', id: cheeseEp }
    const cheeseSs = withSearch.match(/\/cheese\/play\/ss(\d+)/i)?.[1]
    if (cheeseSs) return { kind: 'cheese', idKind: 'season', id: cheeseSs }
    if (host.endsWith('bilibili.com') && url.pathname.replace(/\/+$/, '') === '/cheese') return { kind: 'cheese', idKind: 'paid' }
    const gongfang = `${host}${url.pathname}`.match(/gf\.bilibili\.com\/order\/hyg-download\/(\d+)/i)?.[1]
    if (gongfang) return { kind: 'gongfang', orderId: gongfang }
    const collection = parseSpaceChannelUrl(host, url.pathname, url.searchParams, 'collectiondetail')
    if (collection) return { kind: 'collection', mid: collection.mid, seasonId: collection.sid }
    const series = parseSpaceChannelUrl(host, url.pathname, url.searchParams, 'seriesdetail')
    if (series) return { kind: 'series', mid: series.mid, seriesId: series.sid }
    const spaceList = parseSpaceListsUrl(host, url.pathname, url.searchParams)
    if (spaceList) return { kind: 'collection', mid: spaceList.mid, seasonId: spaceList.sid }
    const webList = parseWebListUrl(host, url.pathname, url.searchParams)
    if (webList?.sid) return { kind: 'collection', mid: webList.upId, seasonId: webList.sid }
    if (webList) return { kind: 'mediaList', rawUrl: url.href, upId: webList.upId }
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

  const cheeseEp = input.match(/(?:^|\/cheese\/play\/)ep(\d+)\b/i)?.[1]
  if (cheeseEp) return { kind: 'cheese', idKind: 'ep', id: cheeseEp }
  const cheeseSs = input.match(/(?:^|\/cheese\/play\/)ss(\d+)\b/i)?.[1]
  if (cheeseSs) return { kind: 'cheese', idKind: 'season', id: cheeseSs }
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

async function resolveB23IfNeeded(rawUrl: string, ctx: ResolveContext): Promise<string> {
  const url = new URL(rawUrl)
  if (url.hostname.toLowerCase() !== 'b23.tv') return rawUrl
  let current = rawUrl
  for (let i = 0; i < 5; i++) {
    const response = await fetchWithTimeout(current, { redirect: 'manual', headers: biliPageHeaders() }, ctx.proxy)
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
  try {
    const url = new URL(rawUrl)
    if (url.hostname.toLowerCase() === 'm.bilibili.com') {
      const space = url.pathname.match(/^\/space\/(\d+)(\/.*)?$/i)
      if (space) {
        const normalized = new URL(`https://space.bilibili.com/${space[1]}${space[2] ?? ''}`)
        normalized.search = url.search
        return normalized.href
      }
    }
  } catch {
    // Keep the original string if it is not a URL.
  }
  return rawUrl
}

function parseSpaceChannelUrl(host: string, pathname: string, searchParams: URLSearchParams, detailKind: 'collectiondetail' | 'seriesdetail'): { mid: string; sid: string } | undefined {
  if (host !== 'space.bilibili.com') return undefined
  const match = pathname.match(new RegExp(`^/(\\d+)/channel/${detailKind}/?$`, 'i'))
  const sid = searchParams.get('sid') ?? undefined
  return match?.[1] && sid ? { mid: match[1], sid } : undefined
}

function parseSpaceListsUrl(host: string, pathname: string, searchParams: URLSearchParams): { mid: string; sid: string } | undefined {
  if (host !== 'space.bilibili.com') return undefined
  const match = pathname.match(/^\/(\d+)\/lists(?:\/(\d+))?\/?$/i)
  if (!match) return undefined
  const sid = match[2] ?? searchParams.get('sid') ?? undefined
  return sid ? { mid: match[1], sid } : undefined
}

function parseWebListUrl(host: string, pathname: string, searchParams: URLSearchParams): { upId: string; sid?: string } | undefined {
  if (host !== 'www.bilibili.com' && host !== 'bilibili.com') return undefined
  const match = pathname.match(/^\/list\/([A-Za-z0-9_]+)\/?$/i)
  if (!match) return undefined
  return { upId: match[1], sid: searchParams.get('sid') ?? undefined }
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
  await attachSubtitles(parts, ctx)

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
  await attachSubtitles(parts, ctx)
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

async function resolveBiliPlaylist(parsed: Extract<ParsedBiliInput, { kind: 'collection' | 'series' | 'mediaList' }>, ctx: ResolveContext): Promise<MediaInfo> {
  const { title, bvids } = await fetchPlaylistBvids(parsed, ctx)
  const parts: BiliPart[] = []
  for (const [videoIndex, bvid] of bvids.entries()) {
    const view = await fetchVideoView(bvid, ctx)
    const pages = view.pages.length ? view.pages : [{ cid: view.cid, page: 1, part: view.title, duration: view.duration }]
    for (const [pageIndex, page] of pages.entries()) {
      parts.push({
        index: parts.length,
        page: parts.length + 1,
        title: pages.length > 1 ? `[${String(videoIndex + 1).padStart(2, '0')}] ${view.title} P${pageIndex + 1} ${page.part || ''}`.trim() : `[${String(videoIndex + 1).padStart(2, '0')}] ${view.title}`,
        cid: page.cid,
        bvid: view.bvid,
        aid: view.aid,
        duration: page.duration,
        subtitles: await fetchSubtitles(view.bvid, page.cid, ctx),
      })
    }
  }
  if (parts.length === 0) throw new Error('bilibili playlist has no videos')
  const streams = await fetchPartStreams('video', parts[0], ctx)
  return {
    platform: 'bilibili',
    id: `bili-${parsed.kind}`,
    type: 'playlist',
    title,
    streams,
    parts: parts.map(toMediaPart),
    raw: { kind: 'video', ctx, parts, currentIndex: 0, streamsByPart: { [parts[0].index]: streams } } satisfies BiliRaw,
  }
}

async function fetchPlaylistBvids(parsed: Extract<ParsedBiliInput, { kind: 'collection' | 'series' | 'mediaList' }>, ctx: ResolveContext): Promise<{ title: string; bvids: string[] }> {
  if (parsed.kind === 'collection') return fetchCollectionBvids(parsed.mid, parsed.seasonId, ctx)
  if (parsed.kind === 'series') return fetchSeriesBvids(parsed.mid, parsed.seriesId, ctx)
  return fetchMediaListBvids(parsed.rawUrl, parsed.upId, ctx)
}

async function fetchCollectionBvids(mid: string, seasonId: string, ctx: ResolveContext): Promise<{ title: string; bvids: string[] }> {
  let title = `bilibili_collection_${seasonId}`
  const bvids: string[] = []
  for (let page = 1; page <= 99; page++) {
    const api = `https://api.bilibili.com/x/polymer/web-space/seasons_archives_list?mid=${encodeURIComponent(mid)}&season_id=${encodeURIComponent(seasonId)}&sort_reverse=false&page_num=${page}&page_size=99`
    const resp = await fetchJson<BiliAPIResponse<{ meta?: { name?: string }; archives?: Array<{ bvid?: string }> }>>(api, ctx)
    if (resp.code !== 0) {
      if (page === 1) throw new Error(`bilibili collection API error ${resp.code}: ${resp.message ?? ''}`.trim())
      break
    }
    if (resp.data.meta?.name) title = resp.data.meta.name
    const pageBvids = (resp.data.archives ?? []).map((item) => item.bvid).filter((value): value is string => Boolean(value))
    if (pageBvids.length === 0) break
    bvids.push(...pageBvids)
  }
  if (bvids.length === 0) throw new Error('bilibili collection has no videos')
  return { title, bvids: uniqueStrings(bvids) }
}

async function fetchSeriesBvids(mid: string, seriesId: string, ctx: ResolveContext): Promise<{ title: string; bvids: string[] }> {
  let title = `bilibili_series_${seriesId}`
  const meta = await fetchJson<BiliAPIResponse<{ meta?: { name?: string }; name?: string }>>(`https://api.bilibili.com/x/series/series?series_id=${encodeURIComponent(seriesId)}`, ctx).catch(() => undefined)
  if (meta?.data?.meta?.name || meta?.data?.name) title = meta.data.meta?.name || meta.data.name || title

  const bvids: string[] = []
  for (let page = 1; page <= 99; page++) {
    const api = `https://api.bilibili.com/x/series/archives?mid=${encodeURIComponent(mid)}&series_id=${encodeURIComponent(seriesId)}&only_normal=true&sort=desc&pn=${page}&ps=99`
    const resp = await fetchJson<BiliAPIResponse<{ archives?: Array<{ bvid?: string }> }>>(api, ctx)
    if (resp.code !== 0) {
      if (page === 1) throw new Error(`bilibili series API error ${resp.code}: ${resp.message ?? ''}`.trim())
      break
    }
    const pageBvids = (resp.data.archives ?? []).map((item) => item.bvid).filter((value): value is string => Boolean(value))
    if (pageBvids.length === 0) break
    bvids.push(...pageBvids)
  }
  if (bvids.length === 0) throw new Error('bilibili series has no videos')
  return { title, bvids: uniqueStrings(bvids) }
}

async function fetchMediaListBvids(rawUrl: string, upId: string, ctx: ResolveContext): Promise<{ title: string; bvids: string[] }> {
  const html = await fetchWithTimeout(rawUrl, { headers: await biliApiHeaders(ctx, rawUrl) }, ctx.proxy).then((resp) => resp.text()).catch(() => '')
  const redirected = html.match(/space\.bilibili\.com\/(\d+)\/channel\/(collection|series)detail\?sid=(\d+)/i)
  if (redirected?.[2] === 'collection') return fetchCollectionBvids(redirected[1], redirected[3], ctx)
  if (redirected?.[2] === 'series') return fetchSeriesBvids(redirected[1], redirected[3], ctx)

  const api = `https://api.bilibili.com/x/v2/medialist/resource/list?mobi_app=web&type=1&biz_id=${encodeURIComponent(upId)}&oid=0&otype=2&ps=99&direction=false&desc=false&sort_field=1&tid=0&with_current=false`
  const resp = await fetchJson<BiliAPIResponse<{ media_list?: Array<{ bv_id?: string }>; upper?: { name?: string } }>>(api, ctx)
  if (resp.code !== 0) throw new Error(`bilibili media list API error ${resp.code}: ${resp.message ?? ''}`.trim())
  const bvids = (resp.data.media_list ?? []).map((item) => item.bv_id).filter((value): value is string => Boolean(value))
  if (bvids.length === 0) throw new Error('bilibili media list has no videos')
  return { title: resp.data.upper?.name || `bilibili_list_${upId}`, bvids: uniqueStrings(bvids) }
}

async function resolveCheese(parsed: Extract<ParsedBiliInput, { kind: 'cheese' }>, ctx: ResolveContext): Promise<MediaInfo> {
  if (parsed.idKind === 'paid') {
    const courses = await fetchCheesePaidCourses(ctx)
    const parts: BiliPart[] = []
    for (const course of courses) {
      const season = await fetchCheeseSeason({ kind: 'cheese', idKind: 'season', id: course.id }, ctx).catch(() => undefined)
      if (!season) continue
      for (const ep of cheeseEpisodes(season)) {
        parts.push({ index: parts.length, page: parts.length + 1, title: `${course.title || season.title || 'course'} -- ${ep.title}`, cid: 0, epId: Number(ep.id), duration: ep.duration })
      }
    }
    return buildCheeseMediaInfo('Bilibili Cheese Paid Courses', parts, ctx)
  }

  const season = await fetchCheeseSeason(parsed, ctx)
  const parts = cheeseEpisodes(season).map((ep, index) => ({ index, page: index + 1, title: ep.sectionTitle ? `${ep.sectionTitle} -- ${ep.title}` : ep.title, cid: 0, epId: Number(ep.id), duration: ep.duration }))
  return buildCheeseMediaInfo(season.title || `cheese_${parsed.id ?? ''}`, parts, ctx)
}

async function buildCheeseMediaInfo(title: string, parts: BiliPart[], ctx: ResolveContext): Promise<MediaInfo> {
  if (parts.length === 0) throw new Error('bilibili cheese has no episodes')
  const streams = await fetchPartStreams('cheese', parts[0], ctx)
  return {
    platform: 'bilibili',
    id: `cheese-${parts[0].epId}`,
    type: 'playlist',
    title,
    streams,
    parts: parts.map(toMediaPart),
    raw: { kind: 'cheese', ctx, parts, currentIndex: 0, streamsByPart: { [parts[0].index]: streams } } satisfies BiliRaw,
  }
}

async function fetchCheeseSeason(parsed: Extract<ParsedBiliInput, { kind: 'cheese' }>, ctx: ResolveContext): Promise<BiliCheeseSeason> {
  if (parsed.idKind === 'paid') throw new Error('paid cheese list does not map to one season')
  const url = parsed.idKind === 'ep'
    ? `https://api.bilibili.com/pugv/view/web/season?ep_id=${encodeURIComponent(parsed.id ?? '')}`
    : `https://api.bilibili.com/pugv/view/web/season/v2?season_id=${encodeURIComponent(parsed.id ?? '')}`
  const resp = await fetchJson<BiliAPIResponse<BiliCheeseSeason>>(url, ctx)
  if (resp.code !== 0) throw new Error(`bilibili cheese season API error ${resp.code}: ${resp.message ?? ''}`.trim())
  if (cheeseEpisodes(resp.data).length === 0) throw new Error('bilibili cheese season has no episodes')
  return resp.data
}

async function fetchCheesePaidCourses(ctx: ResolveContext): Promise<Array<{ id: string; title: string }>> {
  const courses: Array<{ id: string; title: string }> = []
  const seen = new Set<string>()
  for (let page = 1; page <= 500; page++) {
    const resp = await fetchJson<BiliAPIResponse<{ data?: BiliCheesePaidCourse[]; list?: BiliCheesePaidCourse[]; total?: number; page?: { total?: number } }>>(`https://api.bilibili.com/pugv/pay/web/my/paid?ps=10&pn=${page}`, ctx)
    if (resp.code !== 0) throw new Error(`bilibili cheese paid API error ${resp.code}: ${resp.message ?? ''}`.trim())
    const pageCourses = [...(resp.data.data ?? []), ...(resp.data.list ?? [])]
    if (pageCourses.length === 0) break
    for (const item of pageCourses) {
      const id = stringId(item.id) || stringId(item.season_id)
      if (!id || seen.has(id)) continue
      seen.add(id)
      courses.push({ id, title: item.title ?? `course_${id}` })
    }
    const total = resp.data.page?.total || resp.data.total || 0
    if (total > 0 && courses.length >= total) break
  }
  if (courses.length === 0) throw new Error('bilibili cheese paid list is empty')
  return courses
}

function cheeseEpisodes(season: BiliCheeseSeason): Array<{ id: string; title: string; sectionTitle?: string; duration?: number }> {
  const out: Array<{ id: string; title: string; sectionTitle?: string; duration?: number }> = []
  const seen = new Set<string>()
  const add = (raw: BiliCheeseEpisode, sectionTitle?: string) => {
    const id = stringId(raw.id) || stringId(raw.ep_id) || stringId(raw.episode_id)
    if (!id || seen.has(id)) return
    seen.add(id)
    out.push({ id, title: raw.title || raw.long_title || `episode_${id}`, sectionTitle, duration: raw.duration })
  }
  for (const ep of season.episodes ?? []) add(ep)
  for (const section of season.sections ?? []) for (const ep of section.episodes ?? []) add(ep, section.title)
  return out
}

async function resolveGongfang(parsed: Extract<ParsedBiliInput, { kind: 'gongfang' }>, ctx: ResolveContext): Promise<MediaInfo> {
  const title = await fetchGongfangTitle(parsed.orderId, ctx).catch(() => `gongfang_${parsed.orderId}`)
  const items = await fetchGongfangItems(parsed.orderId, ctx)
  const entries: DownloadEntry[] = []
  for (const [index, item] of items.entries()) {
    const sourceId = stringId(item.shipOrderDetailsId)
    if (!sourceId || !item.fileContentType) continue
    const url = await fetchGongfangDownloadUrl(parsed.orderId, sourceId, ctx).catch(() => undefined)
    if (!url) continue
    entries.push({ name: `${String(index + 1).padStart(2, '0')}_${safeStem(stripExt(item.fileName || sourceId, item.fileContentType), sourceId)}.${extensionFromContentType(item.fileContentType, url)}`, url, backupUrls: [], headers: biliDownloadHeaders() })
  }
  if (entries.length === 0) throw new Error('bilibili gongfang has no downloadable files')
  return { platform: 'bilibili', id: `gongfang-${parsed.orderId}`, type: 'playlist', title, raw: { kind: 'gongfang', ctx, parts: [], currentIndex: 0, entries } satisfies BiliRaw }
}

async function fetchGongfangTitle(orderId: string, ctx: ResolveContext): Promise<string> {
  const url = `https://mall.bilibili.com/mall-c/order/detail?orderId=${encodeURIComponent(orderId)}`
  const html = await fetchWithTimeout(url, { headers: await biliApiHeaders(ctx, url) }, ctx.proxy).then((resp) => resp.text())
  const raw = html.match(/"itemsName"\s*:\s*"((?:\\.|[^"\\])*)"/)?.[1]
  return raw ? JSON.parse(`"${raw}"`) : `gongfang_${orderId}`
}

async function fetchGongfangItems(orderId: string, ctx: ResolveContext): Promise<BiliGongfangItem[]> {
  const resp = await postJson<{ code: number; message?: string; data?: { shipOrderDetails?: BiliGongfangItem[] } }>('https://mall.bilibili.com/mall-c/ship/orderdetails/query', { orderId }, ctx)
  if (resp.code !== 0) throw new Error(`bilibili gongfang detail API error ${resp.code}: ${resp.message ?? ''}`.trim())
  return resp.data?.shipOrderDetails ?? []
}

async function fetchGongfangDownloadUrl(orderId: string, shipOrderDetailsId: string, ctx: ResolveContext): Promise<string | undefined> {
  const resp = await postJson<{ code: number; message?: string; data?: string | { url?: string } }>('https://mall.bilibili.com/mall-c/ship/orderdetails/querydownloadurl', { orderId, shipOrderDetailsId }, ctx)
  if (resp.code !== 0) throw new Error(`bilibili gongfang download API error ${resp.code}: ${resp.message ?? ''}`.trim())
  return typeof resp.data === 'string' ? resp.data : resp.data?.url
}

async function postJson<T>(url: string, body: unknown, ctx: ResolveContext): Promise<T> {
  const response = await fetchWithTimeout(url, { method: 'POST', headers: { ...(await biliApiHeaders(ctx, url)), 'Content-Type': 'application/json;charset=utf-8' }, body: JSON.stringify(body) }, ctx.proxy)
  if (!response.ok) throw new Error(`bilibili API HTTP ${response.status}`)
  return (await response.json()) as T
}

async function attachSubtitles(parts: BiliPart[], ctx: ResolveContext): Promise<void> {
  await Promise.all(parts.map(async (part) => {
    if (!part.bvid || !part.cid) return
    part.subtitles = await fetchSubtitles(part.bvid, part.cid, ctx).catch(() => [])
  }))
}

async function fetchSubtitles(bvid: string, cid: number, ctx: ResolveContext): Promise<BiliSubtitle[]> {
  const url = `https://api.bilibili.com/x/player/v2?bvid=${encodeURIComponent(bvid)}&cid=${encodeURIComponent(String(cid))}`
  const resp = await fetchJson<BiliAPIResponse<{ subtitle?: { subtitles?: Array<{ lan?: string; lan_doc?: string; subtitle_url?: string }> } }>>(url, ctx)
  if (resp.code !== 0) return []
  return (resp.data.subtitle?.subtitles ?? [])
    .map((item) => {
      const rawUrl = item.subtitle_url?.trim()
      if (!rawUrl) return undefined
      return { language: item.lan_doc || item.lan || 'unknown', url: rawUrl.startsWith('//') ? `https:${rawUrl}` : rawUrl }
    })
    .filter((item): item is BiliSubtitle => Boolean(item))
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

async function fetchPartStreams(kind: 'video' | 'bangumi' | 'cheese', part: BiliPart, ctx: ResolveContext): Promise<BiliStream[]> {
  const url = kind === 'cheese' ? buildCheesePlayUrl(part) : kind === 'bangumi' ? buildBangumiPlayUrl(part) : buildVideoPlayUrl(part)
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

function buildCheesePlayUrl(part: BiliPart): string {
  if (!part.epId) throw new Error('bilibili cheese part missing ep_id')
  const url = new URL('https://api.bilibili.com/pugv/player/web/playurl')
  url.searchParams.set('fnval', '16')
  url.searchParams.set('fourk', '1')
  url.searchParams.set('ep_id', String(part.epId))
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
  const response = await fetchWithTimeout(url, { headers: await biliApiHeaders(ctx, url) }, ctx.proxy)
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

async function fetchWithTimeout(url: string, init: RequestInit, proxy?: string): Promise<Response> {
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(), requestTimeoutMs)
  const requestInit = withProxy({ ...init, signal: controller.signal }, proxy)
  try {
    return await fetch(url, requestInit)
  } catch (err) {
    if (err instanceof Error && err.name === 'AbortError') throw new Error(`bilibili request timed out after ${requestTimeoutMs}ms`)
    throw err
  } finally {
    clearTimeout(timer)
  }
}

function withProxy(init: RequestInit, proxy: string | undefined): RequestInit {
  const trimmed = proxy?.trim()
  return trimmed ? ({ ...init, proxy: trimmed } as RequestInit) : init
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

function stringId(value: unknown): string {
  if (typeof value === 'string') return value.trim()
  if (typeof value === 'number' && Number.isFinite(value)) return String(Math.trunc(value))
  return ''
}

function stripExt(name: string, ext: string): string {
  let out = name.trim()
  const normalizedExt = ext.startsWith('.') ? ext : `.${ext}`
  if (out.toLowerCase().endsWith(normalizedExt.toLowerCase())) out = out.slice(0, -normalizedExt.length)
  return out.trim()
}

function extensionFromContentType(contentType: string, url: string): string {
  const normalized = contentType.trim().replace(/^\./, '').toLowerCase()
  if (normalized && /^[a-z0-9]{1,8}$/.test(normalized)) return normalized
  try {
    const ext = new URL(url).pathname.match(/\.([a-zA-Z0-9]{1,8})$/)?.[1]
    if (ext) return ext.toLowerCase()
  } catch {
    // Ignore malformed URL; use fallback below.
  }
  return 'bin'
}

type BiliRaw = {
  kind: 'video' | 'bangumi' | 'cheese' | 'gongfang'
  ctx: ResolveContext
  parts: BiliPart[]
  currentIndex: number
  streamsByPart?: Record<number, BiliStream[]>
  entries?: DownloadEntry[]
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
  subtitles?: BiliSubtitle[]
}

type BiliStream = MediaStream & {
  url: string
  audioUrl: string
  videoBackupUrls?: string[]
  audioBackupUrls?: string[]
  codecId?: number
  bandwidth?: number
}

type BiliSubtitle = {
  language: string
  url: string
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

type BiliCheesePaidCourse = {
  id?: string | number
  season_id?: string | number
  title?: string
}

type BiliCheeseSeason = {
  title?: string
  episodes?: BiliCheeseEpisode[]
  sections?: Array<{ title?: string; episodes?: BiliCheeseEpisode[] }>
}

type BiliCheeseEpisode = {
  id?: string | number
  ep_id?: string | number
  episode_id?: string | number
  title?: string
  long_title?: string
  duration?: number
}

type BiliGongfangItem = {
  fileContentType?: string
  fileName?: string
  shipOrderDetailsId?: string | number
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
