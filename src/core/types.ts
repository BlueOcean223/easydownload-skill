import { z } from 'zod'

export const HeadersSchema = z.record(z.string(), z.string())

export const DownloadEntrySchema = z.object({
  name: z.string().min(1),
  url: z.url(),
  headers: HeadersSchema.default({}),
  backupUrls: z.array(z.url()).default([]),
})

const BaseItemSchema = z.object({
  id: z.string().min(1),
  filename: z.string().min(1),
  headers: HeadersSchema.default({}),
})

export const FileItemSchema = BaseItemSchema.extend({
  kind: z.literal('file'),
  url: z.url(),
  backupUrls: z.array(z.url()).default([]),
})

export const ZipItemSchema = BaseItemSchema.extend({
  kind: z.literal('zip'),
  entries: z.array(DownloadEntrySchema).min(1),
})

export const DashItemSchema = BaseItemSchema.extend({
  kind: z.literal('dash'),
  videoUrl: z.url(),
  audioUrl: z.url(),
  videoBackupUrls: z.array(z.url()).default([]),
  audioBackupUrls: z.array(z.url()).default([]),
})

export const DownloadItemSchema = z.discriminatedUnion('kind', [
  FileItemSchema,
  ZipItemSchema,
  DashItemSchema,
])

export const DownloadPlanSchema = z.object({
  title: z.string().default('download'),
  outputDir: z.string().default('./downloads'),
  concurrency: z.number().int().positive().default(4),
  chunkConcurrency: z.number().int().positive().default(4),
  multipartThresholdBytes: z.number().int().nonnegative().default(32 * 1024 * 1024),
  timeoutMs: z.number().int().positive().default(30_000),
  proxy: z.string().min(1).optional(),
  ffmpeg: z.string().min(1).default('ffmpeg'),
  overwrite: z.boolean().default(false),
  noOverwrite: z.boolean().default(false),
  items: z.array(DownloadItemSchema).min(1),
})

export type DownloadEntry = z.infer<typeof DownloadEntrySchema>
export type FileItem = z.infer<typeof FileItemSchema>
export type ZipItem = z.infer<typeof ZipItemSchema>
export type DashItem = z.infer<typeof DashItemSchema>
export type DownloadItem = z.infer<typeof DownloadItemSchema>
export type DownloadPlan = z.infer<typeof DownloadPlanSchema>

export type MediaType = 'video' | 'album' | 'playlist' | 'bangumi'

export interface MediaStream {
  id: string
  quality: string
  qualityName?: string
  size?: number
  width?: number
  height?: number
  url?: string
  audioUrl?: string
  backupUrls?: string[]
  videoBackupUrls?: string[]
  audioBackupUrls?: string[]
  headers?: Record<string, string>
}

export interface MediaImage {
  index: number
  url?: string
  backupUrls?: string[]
  width?: number
  height?: number
  livePhoto?: boolean
  livePhotoUrl?: string
  videoUrl?: string
  headers?: Record<string, string>
}

export interface MediaPart {
  index: number
  title: string
  duration?: number
  streams?: MediaStream[]
}

export interface MediaInfo {
  platform: string
  id: string
  type: MediaType
  title: string
  author?: string
  cover?: string
  streams?: MediaStream[]
  images?: MediaImage[]
  parts?: MediaPart[]
  raw?: unknown
}

export interface ResolveContext {
  cookiesPath?: string
  proxy?: string
  configDir: string
}

export interface DownloadOptions {
  quality?: string
  part?: number
  allParts?: boolean
  images?: number[]
  outputDir?: string
  concurrency?: number
  chunkConcurrency?: number
  overwrite?: boolean
  noOverwrite?: boolean
  cookiesPath?: string
  proxy?: string
}

export interface Platform {
  id: 'douyin' | 'xhs' | 'bilibili' | 'kuaishou'
  displayName: string
  match(input: string): boolean
  resolve(input: string, ctx: ResolveContext): Promise<MediaInfo>
  createDownloadPlan(info: MediaInfo, options: DownloadOptions): Promise<DownloadPlan>
}

export const DownloadEventSchema = z.discriminatedUnion('type', [
  z.object({ type: z.literal('start'), taskId: z.string(), filename: z.string() }),
  z.object({ type: z.literal('progress'), taskId: z.string(), downloaded: z.number(), total: z.number().default(0) }),
  z.object({ type: z.literal('artifact'), taskId: z.string(), path: z.string(), partial: z.boolean().optional() }),
  z.object({ type: z.literal('skipped'), taskId: z.string(), path: z.string(), reason: z.string() }),
  z.object({ type: z.literal('error'), taskId: z.string(), error: z.string() }),
  z.object({
    type: z.literal('done'),
    success: z.boolean(),
    artifacts: z.array(z.object({ taskId: z.string(), path: z.string(), partial: z.boolean().optional() })).default([]),
    failures: z.array(z.object({ taskId: z.string(), error: z.string() })).default([]),
    skipped: z.array(z.object({ taskId: z.string(), path: z.string(), reason: z.string() })).default([]),
  }),
])

export type DownloadEvent = z.infer<typeof DownloadEventSchema>
