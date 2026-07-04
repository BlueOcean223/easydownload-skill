import { createWriteStream } from 'node:fs'
import { mkdir, open, readFile, rename, rm, stat, writeFile } from 'node:fs/promises'
import { dirname, extname, join, basename } from 'node:path'
import { once } from 'node:events'
import {
  DownloadPlanSchema,
  type DownloadPlan,
  type DownloadItem,
  type FileItem,
  type DownloadEvent,
} from './types.js'

const DEFAULT_RETRIES = 3
const DEFAULT_CHUNK_SIZE = 8 * 1024 * 1024
const PROGRESS_EMIT_INTERVAL = 512 * 1024

export type RunDownloadOptions = {
  jsonProgress?: boolean
  onEvent?: (event: DownloadEvent) => void
}

export async function runDownloadPlan(planInput: unknown, options: RunDownloadOptions = {}): Promise<DownloadEvent[]> {
  const plan = DownloadPlanSchema.parse(planInput)
  const executor = new DownloadExecutor(plan, options)
  return executor.run()
}

export function summarizeDownloadEvents(events: DownloadEvent[]) {
  const done = [...events].reverse().find((event) => event.type === 'done')
  if (done?.type === 'done') return done
  return {
    type: 'done' as const,
    success: false,
    artifacts: [],
    failures: [{ taskId: 'downloader', error: 'downloader did not emit done event' }],
    skipped: [],
  }
}

type Artifact = { taskId: string; path: string; partial?: boolean }
type Failure = { taskId: string; error: string }
type Skipped = { taskId: string; path: string; reason: string }

type TargetResult =
  | { kind: 'target'; path: string }
  | { kind: 'skipped'; skipped: Skipped }

type RangeProbe = { supportsRange: boolean; total: number }

type Chunk = { index: number; start: number; end: number }

class DownloadExecutor {
  private readonly events: DownloadEvent[] = []
  private readonly artifacts: Artifact[] = []
  private readonly failures: Failure[] = []
  private readonly skipped: Skipped[] = []

  constructor(
    private readonly plan: DownloadPlan,
    private readonly options: RunDownloadOptions,
  ) {}

  async run(): Promise<DownloadEvent[]> {
    await mkdir(this.plan.outputDir, { recursive: true })
    await mapLimit(this.plan.items, this.plan.concurrency, async (item) => this.processItem(item))
    this.emit({
      type: 'done',
      success: this.failures.length === 0,
      artifacts: this.artifacts,
      failures: this.failures,
      skipped: this.skipped,
    })
    return this.events
  }

  private async processItem(item: DownloadItem): Promise<void> {
    this.emit({ type: 'start', taskId: item.id, filename: item.filename })
    try {
      switch (item.kind) {
        case 'file':
          await this.processFile(item)
          return
        case 'zip':
          throw new Error('zip item is not implemented yet')
        case 'dash':
          throw new Error('dash item is not implemented yet')
      }
    } catch (err) {
      this.addFailure(item.id, err)
    }
  }

  private async processFile(item: FileItem): Promise<void> {
    const target = await this.prepareTarget(item)
    if (target.kind === 'skipped') {
      this.addSkipped(target.skipped)
      return
    }
    await this.downloadWithFallback(item.id, item.url, item.backupUrls, item.headers, target.path)
    this.addArtifact({ taskId: item.id, path: target.path })
  }

  private emit(event: DownloadEvent): void {
    this.events.push(event)
    this.options.onEvent?.(event)
    if (this.options.jsonProgress) process.stdout.write(`${JSON.stringify(event)}\n`)
  }

  private progress(taskId: string, downloaded: number, total = 0): void {
    this.emit({ type: 'progress', taskId, downloaded, total })
  }

  private addArtifact(artifact: Artifact): void {
    this.artifacts.push(artifact)
    this.emit({ type: 'artifact', ...artifact })
  }

  private addFailure(taskId: string, err: unknown): void {
    const failure = { taskId: taskId || 'unknown', error: err instanceof Error ? err.message : String(err) }
    this.failures.push(failure)
    this.emit({ type: 'error', ...failure })
  }

  private addSkipped(skipped: Skipped): void {
    this.skipped.push(skipped)
    this.emit({ type: 'skipped', ...skipped })
  }

  private async prepareTarget(item: DownloadItem): Promise<TargetResult> {
    const filename = sanitizeFilename(item.filename) || sanitizeFilename(item.id) || 'download.bin'
    const basePath = join(this.plan.outputDir, filename)

    if (this.plan.overwrite) {
      await rm(basePath, { force: true })
      await rm(`${basePath}.part`, { force: true })
      await rm(`${basePath}.parts`, { force: true, recursive: true })
      return { kind: 'target', path: basePath }
    }

    if (this.plan.noOverwrite) {
      if (await fileExists(basePath)) return { kind: 'skipped', skipped: { taskId: item.id, path: basePath, reason: 'file exists' } }
      return { kind: 'target', path: basePath }
    }

    if (!(await pathCollision(basePath))) return { kind: 'target', path: basePath }

    const ext = extname(filename)
    const stem = filename.slice(0, filename.length - ext.length)
    for (let i = 1; i < 10000; i++) {
      const candidate = join(this.plan.outputDir, `${stem} (${i})${ext}`)
      if (!(await pathCollision(candidate))) return { kind: 'target', path: candidate }
    }
    throw new Error(`could not allocate unique filename for ${filename}`)
  }

  private async downloadWithFallback(taskId: string, primary: string, backups: string[], headers: Record<string, string>, dest: string): Promise<void> {
    const urls = uniqueUrls([primary, ...backups])
    if (urls.length === 0) throw new Error('no download URL available')
    let lastErr: unknown
    for (let i = 0; i < urls.length; i++) {
      if (i > 0) {
        await rm(`${dest}.part`, { force: true })
        await rm(`${dest}.parts`, { force: true, recursive: true })
      }
      for (let attempt = 1; attempt <= DEFAULT_RETRIES; attempt++) {
        try {
          await this.downloadOne(taskId, urls[i], headers, dest)
          return
        } catch (err) {
          lastErr = err
          if (attempt < DEFAULT_RETRIES) await sleep(attempt * 1000)
        }
      }
    }
    throw new Error(`all download URLs failed: ${lastErr instanceof Error ? lastErr.message : String(lastErr)}`)
  }

  private async downloadOne(taskId: string, url: string, headers: Record<string, string>, dest: string): Promise<void> {
    const probe = await this.probeRange(url, headers).catch((err) => {
      process.stderr.write(`range probe failed for ${shortUrl(url)}: ${err instanceof Error ? err.message : String(err)}; using sequential download\n`)
      return { supportsRange: false, total: 0 }
    })

    if (probe.supportsRange && probe.total > 0 && (await fileSize(`${dest}.part`)) === probe.total) {
      await finalizePart(dest)
      return
    }

    if (probe.supportsRange && probe.total >= this.plan.multipartThresholdBytes && this.plan.chunkConcurrency > 1 && !(await fileExists(`${dest}.part`))) {
      await this.downloadMultipart(taskId, url, headers, dest, probe.total)
      return
    }

    await this.downloadSequential(taskId, url, headers, dest, probe.total)
  }

  private async probeRange(url: string, headers: Record<string, string>): Promise<RangeProbe> {
    const response = await fetch(url, { headers: { ...headers, Range: 'bytes=0-1' } })
    await response.arrayBuffer().catch(() => undefined)
    if (response.status === 206) {
      const parsed = parseContentRange(response.headers.get('content-range'))
      if (parsed && parsed.start === 0 && parsed.total > 0) return { supportsRange: true, total: parsed.total }
    }
    const contentLength = Number.parseInt(response.headers.get('content-length') ?? '', 10)
    if (response.status === 200 && Number.isFinite(contentLength) && contentLength > 0) return { supportsRange: false, total: contentLength }
    if (response.status >= 400) throw new Error(`probe HTTP ${response.status}`)
    return { supportsRange: false, total: 0 }
  }

  private async downloadSequential(taskId: string, url: string, headers: Record<string, string>, dest: string, totalHint: number): Promise<void> {
    await mkdir(dirname(dest), { recursive: true })
    const partPath = `${dest}.part`
    let resume = await fileSize(partPath)

    for (let restart = 0; restart < 2; restart++) {
      const requestHeaders: Record<string, string> = { ...headers }
      if (resume > 0) requestHeaders.Range = `bytes=${resume}-`
      const response = await fetch(url, { headers: requestHeaders })
      if (resume > 0 && response.status === 200) {
        await response.body?.cancel().catch(() => undefined)
        await rm(partPath, { force: true })
        resume = 0
        continue
      }
      if (response.status !== 200 && response.status !== 206) throw new Error(`HTTP ${response.status}`)
      if (!response.body) throw new Error('response body is empty')

      const total = totalFromResponse(response, resume, totalHint)
      const downloaded = await writeStreamToFile(response.body, partPath, resume > 0, resume, total, (n) => this.progress(taskId, n, total))
      if (total > 0 && downloaded !== total) throw new Error(`short download: got ${downloaded} of ${total} bytes`)
      this.progress(taskId, downloaded, total || downloaded)
      await finalizePart(dest)
      return
    }

    throw new Error('server ignored range after restart')
  }

  private async downloadMultipart(taskId: string, url: string, headers: Record<string, string>, dest: string, total: number): Promise<void> {
    const partsDir = `${dest}.parts`
    await mkdir(partsDir, { recursive: true })
    await ensureMultipartMeta(partsDir, url, total)

    const chunks = buildChunks(total, DEFAULT_CHUNK_SIZE)
    let completed = 0
    for (const chunk of chunks) {
      if ((await fileSize(chunkPath(partsDir, chunk))) === chunk.end - chunk.start + 1) completed += chunk.end - chunk.start + 1
    }
    this.progress(taskId, completed, total)

    await mapLimit(chunks, this.plan.chunkConcurrency, async (chunk) => {
      const path = chunkPath(partsDir, chunk)
      if ((await fileSize(path)) === chunk.end - chunk.start + 1) return
      await this.downloadChunk(url, headers, partsDir, chunk, total)
      completed += chunk.end - chunk.start + 1
      this.progress(taskId, completed, total)
    })

    const partPath = `${dest}.part`
    await concatChunks(partPath, partsDir, chunks)
    if ((await fileSize(partPath)) !== total) throw new Error(`multipart concat size mismatch`)
    await finalizePart(dest)
    await rm(partsDir, { force: true, recursive: true })
    this.progress(taskId, total, total)
  }

  private async downloadChunk(url: string, headers: Record<string, string>, partsDir: string, chunk: Chunk, total: number): Promise<void> {
    const response = await fetch(url, { headers: { ...headers, Range: `bytes=${chunk.start}-${chunk.end}` } })
    if (response.status !== 206) throw new Error(`chunk ${chunk.index} expected 206, got HTTP ${response.status}`)
    const range = parseContentRange(response.headers.get('content-range'))
    if (!range || range.start !== chunk.start || range.end !== chunk.end || range.total !== total) {
      throw new Error(`chunk ${chunk.index} invalid Content-Range ${response.headers.get('content-range')}`)
    }
    if (!response.body) throw new Error(`chunk ${chunk.index} response body is empty`)
    const path = chunkPath(partsDir, chunk)
    const tmp = `${path}.tmp`
    const written = await writeStreamToFile(response.body, tmp, false, 0, chunk.end - chunk.start + 1)
    const expected = chunk.end - chunk.start + 1
    if (written !== expected) {
      await rm(tmp, { force: true })
      throw new Error(`chunk ${chunk.index} short read: got ${written} want ${expected}`)
    }
    await rename(tmp, path)
  }
}

async function mapLimit<T>(items: T[], limit: number, fn: (item: T) => Promise<void>): Promise<void> {
  const queue = [...items]
  const workers = Array.from({ length: Math.min(Math.max(limit, 1), items.length) }, async () => {
    while (queue.length > 0) {
      const item = queue.shift()
      if (item !== undefined) await fn(item)
    }
  })
  await Promise.all(workers)
}

async function writeStreamToFile(
  body: ReadableStream<Uint8Array>,
  path: string,
  append: boolean,
  initial: number,
  total: number,
  progress?: (downloaded: number) => void,
): Promise<number> {
  await mkdir(dirname(path), { recursive: true })
  const stream = createWriteStream(path, { flags: append ? 'a' : 'w' })
  let downloaded = initial
  let nextEmit = initial + PROGRESS_EMIT_INTERVAL
  try {
    for await (const chunk of body as AsyncIterable<Uint8Array>) {
      if (!stream.write(chunk)) await once(stream, 'drain')
      downloaded += chunk.byteLength
      if (progress && (downloaded >= nextEmit || (total > 0 && downloaded === total))) {
        progress(downloaded)
        nextEmit = downloaded + PROGRESS_EMIT_INTERVAL
      }
    }
  } finally {
    stream.end()
    await once(stream, 'close')
  }
  return downloaded
}

function totalFromResponse(response: Response, resume: number, totalHint: number): number {
  const range = parseContentRange(response.headers.get('content-range'))
  if (range) return range.total
  const length = Number.parseInt(response.headers.get('content-length') ?? '', 10)
  if (Number.isFinite(length) && length > 0) return response.status === 206 ? resume + length : length
  return totalHint
}

function parseContentRange(value: string | null): { start: number; end: number; total: number } | null {
  const match = value?.trim().match(/^bytes\s+(\d+)-(\d+)\/(\d+|\*)$/i)
  if (!match || match[3] === '*') return null
  const start = Number.parseInt(match[1], 10)
  const end = Number.parseInt(match[2], 10)
  const total = Number.parseInt(match[3], 10)
  if (!Number.isFinite(start) || !Number.isFinite(end) || !Number.isFinite(total) || start < 0 || end < start || total <= end) return null
  return { start, end, total }
}

function buildChunks(total: number, chunkSize: number): Chunk[] {
  const chunks: Chunk[] = []
  for (let index = 0, start = 0; start < total; index++, start += chunkSize) {
    chunks.push({ index, start, end: Math.min(start + chunkSize - 1, total - 1) })
  }
  return chunks
}

function chunkPath(partsDir: string, chunk: Chunk): string {
  return join(partsDir, `chunk_${String(chunk.index).padStart(5, '0')}_${chunk.start}_${chunk.end}.part`)
}

async function ensureMultipartMeta(partsDir: string, url: string, total: number): Promise<void> {
  const metaPath = join(partsDir, 'meta.json')
  try {
    const meta = JSON.parse(await readFile(metaPath, 'utf8')) as { url?: string; total?: number }
    if (meta.url === url && meta.total === total) return
    await rm(partsDir, { force: true, recursive: true })
    await mkdir(partsDir, { recursive: true })
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code !== 'ENOENT') throw err
  }
  await writeFile(metaPath, `${JSON.stringify({ url, total }, null, 2)}\n`)
}

async function concatChunks(partPath: string, partsDir: string, chunks: Chunk[]): Promise<void> {
  const tmp = `${partPath}.concat`
  const out = await open(tmp, 'w')
  try {
    for (const chunk of chunks) {
      const data = await readFile(chunkPath(partsDir, chunk))
      await out.write(data)
    }
  } finally {
    await out.close()
  }
  await rename(tmp, partPath)
}

async function finalizePart(dest: string): Promise<void> {
  const partPath = `${dest}.part`
  if (!(await fileExists(partPath))) throw new Error(`missing part file: ${partPath}`)
  await rename(partPath, dest)
}

async function pathCollision(path: string): Promise<boolean> {
  return (await fileExists(path)) || (await fileExists(`${path}.part`)) || (await dirExists(`${path}.parts`))
}

async function fileExists(path: string): Promise<boolean> {
  try {
    const st = await stat(path)
    return st.isFile()
  } catch {
    return false
  }
}

async function dirExists(path: string): Promise<boolean> {
  try {
    const st = await stat(path)
    return st.isDirectory()
  } catch {
    return false
  }
}

async function fileSize(path: string): Promise<number> {
  try {
    const st = await stat(path)
    return st.isFile() ? st.size : 0
  } catch {
    return 0
  }
}

function sanitizeFilename(name: string): string {
  return basename(name.trim().replaceAll('\\', '/')).replace(/[<>:"/\\|?*\x00-\x1f]/g, '_').replace(/^\.+$/, '').trim().slice(0, 180)
}

function uniqueUrls(urls: string[]): string[] {
  const seen = new Set<string>()
  const out: string[] = []
  for (const url of urls) {
    const trimmed = url.trim()
    if (!trimmed || seen.has(trimmed)) continue
    seen.add(trimmed)
    out.push(trimmed)
  }
  return out
}

function shortUrl(url: string): string {
  return url.length <= 80 ? url : `${url.slice(0, 77)}...`
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms))
}
