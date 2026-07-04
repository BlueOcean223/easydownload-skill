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

const apiURL = 'https://m.gifshow.com/rest/wd/ugH5App/photo/simple/info'

const mobileUserAgent =
  'Mozilla/5.0 (iPhone; CPU iPhone OS 16_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.0 Mobile/15E148 Safari/604.1'

const apiHeaders: Record<string, string> = {
  'User-Agent': mobileUserAgent,
  Referer: 'https://v.chenzhongtech.com/',
  'Content-Type': 'application/json',
}

const downloadHeaders: Record<string, string> = {
  'User-Agent': mobileUserAgent,
  Referer: 'https://v.chenzhongtech.com/',
}

const kuaishouPattern = /(?:^|\/\/)(?:[^/]+\.)?(?:kuaishou\.com|kwai\.com|chenzhongtech\.com|gifshow\.com)(?:[/:?#]|$)/i
const directPhotoIdPattern = /(?:\/fw\/photo\/|\/short-video\/)([a-zA-Z0-9_]+)/
const barePhotoIdPattern = /^[a-zA-Z0-9_]{8,32}$/

export const kuaishouPlatform: Platform = {
  id: 'kuaishou',
  displayName: '快手',
  match(input: string): boolean {
    return kuaishouPattern.test(input)
  },
  async resolve(input: string, _ctx: ResolveContext): Promise<MediaInfo> {
    const photoId = await resolvePhotoId(input)
    const data = await fetchPhotoInfo(photoId)
    return buildMediaInfo(data, photoId)
  },
  async createDownloadPlan(info: MediaInfo, options: DownloadOptions): Promise<DownloadPlan> {
    if (info.platform !== 'kuaishou') throw new Error(`expected kuaishou MediaInfo, got ${info.platform}`)

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

    if (info.type === 'video') {
      const stream = pickVideoStream(info, options.quality)
      return {
        ...basePlan,
        items: [
          {
            id: info.id,
            kind: 'file',
            filename: `${safeStem(info.title, info.id)}.${extensionFromUrl(stream.url, 'mp4')}`,
            url: stream.url,
            backupUrls: stream.backupUrls ?? [],
            headers: stream.headers ?? downloadHeaders,
          },
        ],
      }
    }

    if (info.type === 'album') {
      const raw = getKuaishouRaw(info)
      const selectedImages = selectImages(info.images ?? [], options.images)

      if (raw.photoType === 'SINGLE_PICTURE') {
        const image = selectedImages[0]
        if (!image?.url) throw new Error('kuaishou single picture has no downloadable URL')
        return {
          ...basePlan,
          items: [
            {
              id: info.id,
              kind: 'file',
              filename: `${safeStem(info.title, info.id)}.${extensionFromUrl(image.url, 'jpg')}`,
              url: image.url,
              backupUrls: [],
              headers: image.headers ?? downloadHeaders,
            },
          ],
        }
      }

      const entries: DownloadEntry[] = selectedImages.map((image) => {
        if (!image.url) throw new Error(`kuaishou image ${image.index + 1} has no URL`)
        return {
          name: `${String(image.index + 1).padStart(2, '0')}.${extensionFromUrl(image.url, 'jpg')}`,
          url: image.url,
          backupUrls: [],
          headers: image.headers ?? downloadHeaders,
        }
      })

      if (raw.musicUrl) {
        entries.push({
          name: `music.${extensionFromUrl(raw.musicUrl, 'm4a')}`,
          url: raw.musicUrl,
          backupUrls: [],
          headers: downloadHeaders,
        })
      }

      if (entries.length === 0) throw new Error('kuaishou album has no downloadable entries')
      return {
        ...basePlan,
        items: [
          {
            id: info.id,
            kind: 'zip',
            filename: `${safeStem(info.title, info.id)}.zip`,
            headers: downloadHeaders,
            entries,
          },
        ],
      }
    }

    throw new Error(`unsupported kuaishou media type: ${info.type}`)
  },
}

async function resolvePhotoId(input: string): Promise<string> {
  const direct = extractPhotoId(input)
  if (direct) return direct

  const trimmed = input.trim()
  if (barePhotoIdPattern.test(trimmed) && !trimmed.includes('.')) return trimmed

  const url = extractFirstUrl(input)
  if (!url) throw new Error('no kuaishou url found')

  let current = url
  for (let i = 0; i < 6; i++) {
    const response = await fetchWithTimeout(current, { headers: downloadHeaders, redirect: 'manual' })
    await response.body?.cancel().catch(() => undefined)

    const fromResponseUrl = extractPhotoId(response.url)
    if (fromResponseUrl) return fromResponseUrl

    const location = response.headers.get('location')
    if (location && response.status >= 300 && response.status < 400) {
      current = new URL(location, current).href
      const fromLocation = extractPhotoId(current)
      if (fromLocation) return fromLocation
      continue
    }

    if (!response.ok && response.status !== 0) throw new Error(`kuaishou redirect HTTP ${response.status}`)
    break
  }

  const fromFinalUrl = extractPhotoId(current)
  if (fromFinalUrl) return fromFinalUrl
  throw new Error('kuaishou photoId not found')
}

function extractPhotoId(input: string): string | undefined {
  const direct = input.match(directPhotoIdPattern)?.[1]
  if (direct) return direct

  const query = input.match(/[?&]photoId=([a-zA-Z0-9_]+)/)?.[1]
  if (query) return query

  try {
    const url = new URL(input)
    const photoId = url.searchParams.get('photoId')
    return photoId && barePhotoIdPattern.test(photoId) ? photoId : undefined
  } catch {
    return undefined
  }
}

function extractFirstUrl(input: string): string | undefined {
  const match = input.match(/https?:\/\/[^\s<>'"`，。；、）)】]+/i)
  return match?.[0]?.replace(/[.,!?;:，。！？；：]+$/, '')
}

async function fetchPhotoInfo(photoId: string): Promise<KuaishouAPIResponse> {
  const response = await fetchWithTimeout(apiURL, {
    method: 'POST',
    headers: apiHeaders,
    body: JSON.stringify({
      photoId,
      subBiz: 'BROWSE_SLIDE_PHOTO',
      kpn: 'KUAISHOU',
    }),
  })
  if (!response.ok) throw new Error(`kuaishou API HTTP ${response.status}`)

  const data = (await response.json()) as KuaishouAPIResponse
  if (data.result !== undefined && data.result !== 1) throw new Error(`kuaishou API result ${data.result}${data.message ? `: ${data.message}` : ''}`)
  if (!data.photo) throw new Error('kuaishou API response missing photo')
  return data
}

async function fetchWithTimeout(url: string, init: RequestInit, timeoutMs = 30_000): Promise<Response> {
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(), timeoutMs)
  try {
    return await fetch(url, { ...init, signal: controller.signal })
  } catch (err) {
    if (err instanceof Error && err.name === 'AbortError') throw new Error(`kuaishou request timed out after ${timeoutMs}ms`)
    throw err
  } finally {
    clearTimeout(timer)
  }
}

function buildMediaInfo(data: KuaishouAPIResponse, fallbackPhotoId: string): MediaInfo {
  const photo = data.photo
  const id = photo.photoId || fallbackPhotoId
  const title = photo.caption || id
  const cover = firstUrl(photo.coverUrls)

  switch (photo.photoType) {
    case 'VIDEO':
    case 'PAY_COURSE_VIDEO':
    case 'SPHERICAL_VIDEO':
      return {
        platform: 'kuaishou',
        id,
        type: 'video',
        title,
        author: photo.userName,
        cover,
        streams: buildVideoStreams(photo),
        raw: { api: data, photoType: photo.photoType } satisfies KuaishouRaw,
      }
    case 'SINGLE_PICTURE': {
      const imageUrl = cover
      if (!imageUrl) throw new Error('kuaishou single picture has no cover URL')
      return {
        platform: 'kuaishou',
        id,
        type: 'album',
        title,
        author: photo.userName,
        cover,
        images: [{ index: 0, url: imageUrl, headers: downloadHeaders }],
        raw: { api: data, photoType: photo.photoType } satisfies KuaishouRaw,
      }
    }
    case 'HORIZONTAL_ATLAS':
    case 'VERTICAL_ATLAS':
      return {
        platform: 'kuaishou',
        id,
        type: 'album',
        title,
        author: photo.userName,
        cover,
        images: buildAtlasImages(data.atlas),
        raw: { api: data, photoType: photo.photoType, musicUrl: buildMusicUrl(data.atlas) } satisfies KuaishouRaw,
      }
    default:
      throw new Error(`unsupported kuaishou photoType: ${photo.photoType || 'unknown'}`)
  }
}

function buildVideoStreams(photo: KuaishouPhoto): MediaStream[] {
  const streams: MediaStream[] = []
  for (const set of photo.manifest?.adaptationSet ?? []) {
    for (const rep of set.representation ?? []) {
      if (!rep.url) continue
      const id = [rep.qualityType || `${rep.height ?? 0}p`, rep.videoCodec].filter(Boolean).join('_') || `stream_${streams.length + 1}`
      streams.push({
        id,
        quality: rep.qualityType || `${rep.height ?? 0}p`,
        qualityName: [rep.qualityType, rep.videoCodec].filter(Boolean).join(' ') || id,
        size: rep.fileSize,
        width: rep.width,
        height: rep.height,
        url: rep.url,
        backupUrls: rep.backupUrl ?? [],
        headers: downloadHeaders,
      })
    }
  }

  if (streams.length === 0) {
    for (const [index, mv] of (photo.mainMvUrls ?? []).entries()) {
      if (!mv.url) continue
      streams.push({
        id: index === 0 ? 'main' : `main_${index + 1}`,
        quality: 'main',
        qualityName: mv.cdn ? `mainMvUrls ${mv.cdn}` : 'mainMvUrls',
        url: mv.url,
        backupUrls: [],
        headers: downloadHeaders,
      })
    }
  }

  if (streams.length === 0) throw new Error('kuaishou video has no downloadable stream')
  return streams
}

function buildAtlasImages(atlas: KuaishouAtlas | undefined): MediaImage[] {
  if (!atlas?.list?.length) throw new Error('kuaishou atlas has no images')
  const cdn = atlas.cdnList?.find((item) => item.cdn)?.cdn
  if (!cdn) throw new Error('kuaishou atlas has no image CDN')

  return atlas.list.map((path, index) => ({
    index,
    url: absoluteCdnUrl(cdn, path),
    width: atlas.size?.[index]?.w,
    height: atlas.size?.[index]?.h,
    headers: downloadHeaders,
  }))
}

function buildMusicUrl(atlas: KuaishouAtlas | undefined): string | undefined {
  if (!atlas?.music) return undefined
  const cdn = atlas.musicCdnList?.find((item) => item.cdn)?.cdn
  return cdn ? absoluteCdnUrl(cdn, atlas.music) : undefined
}

function pickVideoStream(info: MediaInfo, quality = 'best'): Required<Pick<MediaStream, 'url'>> & MediaStream {
  const streams = (info.streams ?? []).filter((stream): stream is Required<Pick<MediaStream, 'url'>> & MediaStream => Boolean(stream.url))
  if (streams.length === 0) throw new Error('kuaishou video has no downloadable stream')

  const wanted = quality.trim().toLowerCase()
  if (wanted && wanted !== 'best') {
    const exact = streams.find((stream) => stream.id.toLowerCase() === wanted)
    if (exact) return exact
    const qualityMatch = streams.find((stream) => stream.quality.toLowerCase() === wanted)
    if (qualityMatch) return qualityMatch
    const nameMatch = streams.find((stream) => stream.qualityName?.toLowerCase().includes(wanted))
    if (nameMatch) return nameMatch
    throw new Error(`kuaishou quality not found: ${quality}; available: ${streams.map((stream) => stream.id).join(', ')}`)
  }

  return [...streams].sort(compareStreamsBestFirst)[0]
}

function compareStreamsBestFirst(a: MediaStream, b: MediaStream): number {
  return (b.height ?? 0) - (a.height ?? 0) || (b.width ?? 0) - (a.width ?? 0) || (b.size ?? 0) - (a.size ?? 0)
}

function selectImages(images: MediaImage[], selection: number[] | undefined): MediaImage[] {
  if (!selection?.length) return images
  return selection.map((index) => {
    const image = images[index]
    if (!image) throw new Error(`kuaishou image index out of range: ${index + 1} (total ${images.length})`)
    return image
  })
}

function getKuaishouRaw(info: MediaInfo): KuaishouRaw {
  const raw = info.raw as Partial<KuaishouRaw> | undefined
  return { api: raw?.api, photoType: raw?.photoType, musicUrl: raw?.musicUrl }
}

function firstUrl(values: KuaishouUrl[] | undefined): string | undefined {
  return values?.find((item) => item.url)?.url
}

function absoluteCdnUrl(cdn: string, path: string): string {
  if (/^https?:\/\//i.test(path)) return path
  return `https://${cdn}${path.startsWith('/') ? path : `/${path}`}`
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

function safeStem(title: string, fallback: string): string {
  return (title || fallback).replace(/[<>:"/\\|?*\x00-\x1f]+/g, '_').replace(/\s+/g, ' ').trim().slice(0, 80) || fallback
}

type KuaishouPhotoType = 'VIDEO' | 'PAY_COURSE_VIDEO' | 'SPHERICAL_VIDEO' | 'SINGLE_PICTURE' | 'HORIZONTAL_ATLAS' | 'VERTICAL_ATLAS'

type KuaishouAPIResponse = {
  result?: number
  message?: string
  photo: KuaishouPhoto
  atlas?: KuaishouAtlas
}

type KuaishouPhoto = {
  photoId?: string
  photoType?: KuaishouPhotoType | string
  caption?: string
  userName?: string
  coverUrls?: KuaishouUrl[]
  mainMvUrls?: KuaishouUrl[]
  manifest?: {
    adaptationSet?: Array<{
      representation?: KuaishouRepresentation[]
    }>
  }
}

type KuaishouUrl = {
  cdn?: string
  url?: string
}

type KuaishouRepresentation = {
  qualityType?: string
  videoCodec?: string
  width?: number
  height?: number
  avgBitrate?: number
  fileSize?: number
  url?: string
  backupUrl?: string[]
}

type KuaishouAtlas = {
  cdnList?: Array<{ cdn?: string }>
  list?: string[]
  size?: Array<{ w?: number; h?: number }>
  music?: string
  musicCdnList?: Array<{ cdn?: string }>
}

type KuaishouRaw = {
  api?: KuaishouAPIResponse
  photoType?: string
  musicUrl?: string
}
