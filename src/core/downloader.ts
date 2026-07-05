import { createWriteStream } from 'node:fs'
import { mkdir, open, readFile, rename, rm, stat, writeFile } from 'node:fs/promises'
import { spawn } from 'node:child_process'
import { basename, dirname, extname, join } from 'node:path'
import { once } from 'node:events'
import {
  DownloadPlanSchema,
  type DownloadPlan,
  type DownloadItem,
  type DashItem,
  type FileItem,
  type ZipItem,
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
          await this.processZip(item)
          return
        case 'dash':
          await this.processDash(item)
          return
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

  private async processZip(item: ZipItem): Promise<void> {
    const target = await this.prepareTarget(item)
    if (target.kind === 'skipped') {
      this.addSkipped(target.skipped)
      return
    }

    const tempDir = `${target.path}.parts`
    await mkdir(tempDir, { recursive: true })
    const successes: Array<{ name: string; path: string }> = []
    const usedNames = new Set<string>()

    await mapLimit(item.entries, Math.min(this.plan.concurrency, item.entries.length), async (entry, index) => {
      const name = uniqueZipEntryName(sanitizeZipEntryName(entry.name, index), usedNames)
      const entryPath = join(tempDir, `entry_${String(index).padStart(5, '0')}_${sanitizeFilename(basename(name)) || 'file'}`)
      try {
        await this.downloadWithFallback(`${item.id}:${name}`, entry.url, entry.backupUrls, { ...item.headers, ...entry.headers }, entryPath)
        successes.push({ name, path: entryPath })
      } catch (err) {
        this.addFailure(`${item.id}:${name}`, err)
      }
    })

    if (successes.length === 0) {
      await rm(tempDir, { force: true, recursive: true })
      this.addFailure(item.id, new Error('all zip entries failed'))
      return
    }

    successes.sort((a, b) => a.name.localeCompare(b.name))
    await writeZipArchive(target.path, successes)
    await rm(tempDir, { force: true, recursive: true })
    this.addArtifact({ taskId: item.id, path: target.path, ...(successes.length < item.entries.length ? { partial: true } : {}) })
  }

  private async processDash(item: DashItem): Promise<void> {
    const target = await this.prepareTarget(item)
    if (target.kind === 'skipped') {
      this.addSkipped(target.skipped)
      return
    }

    const tempDir = `${target.path}.parts`
    await mkdir(tempDir, { recursive: true })
    const videoPath = join(tempDir, `video${extensionFromUrl(item.videoUrl, '.video')}`)
    const audioPath = join(tempDir, `audio${extensionFromUrl(item.audioUrl, '.audio')}`)

    await this.downloadWithFallback(`${item.id}:video`, item.videoUrl, item.videoBackupUrls, item.headers, videoPath)
    await this.downloadWithFallback(`${item.id}:audio`, item.audioUrl, item.audioBackupUrls, item.headers, audioPath)
    await runFfmpegMux(this.plan.ffmpeg, videoPath, audioPath, `${target.path}.part`)
    await finalizePart(target.path)
    await rm(tempDir, { force: true, recursive: true })
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
      await removeDownloadState(basePath)
      return { kind: 'target', path: basePath }
    }

    if (this.plan.noOverwrite) {
      if (await fileExists(basePath)) return { kind: 'skipped', skipped: { taskId: item.id, path: basePath, reason: 'file exists' } }
      return { kind: 'target', path: basePath }
    }

    // Default mode auto-renames completed files, but keeps .part/.parts state so reruns resume.
    if (!(await fileExists(basePath))) return { kind: 'target', path: basePath }

    const ext = extname(filename)
    const stem = filename.slice(0, filename.length - ext.length)
    for (let i = 1; i < 10000; i++) {
      const candidate = join(this.plan.outputDir, `${stem} (${i})${ext}`)
      if (!(await fileExists(candidate))) return { kind: 'target', path: candidate }
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
          if (!isRetryableError(err)) break
          if (attempt < DEFAULT_RETRIES) await sleep(attempt * 1000)
        }
      }
    }
    throw new Error(`all download URLs failed: ${lastErr instanceof Error ? lastErr.message : String(lastErr)}`)
  }

  private async downloadOne(taskId: string, url: string, headers: Record<string, string>, dest: string): Promise<void> {
    const probe = await this.probeRange(url, headers).catch((err) => {
      if (!isHttpStatusError(err, 404)) {
        process.stderr.write(`range probe failed for ${shortUrl(url)}: ${err instanceof Error ? err.message : String(err)}; using sequential download\n`)
      }
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
    const response = await this.fetch(url, { headers: { ...headers, Range: 'bytes=0-1' } })
    await response.body?.cancel().catch(() => undefined)
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
      const response = await this.fetch(url, { headers: requestHeaders })
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
    const response = await this.fetch(url, { headers: { ...headers, Range: `bytes=${chunk.start}-${chunk.end}` } })
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

  private fetch(url: string, init: RequestInit): Promise<Response> {
    return fetchWithTimeout(url, init, this.plan.timeoutMs, this.plan.proxy)
  }
}

async function mapLimit<T>(items: T[], limit: number, fn: (item: T, index: number) => Promise<void>): Promise<void> {
  const queue = items.map((item, index) => ({ item, index }))
  const workers = Array.from({ length: Math.min(Math.max(limit, 1), items.length) }, async () => {
    while (queue.length > 0) {
      const next = queue.shift()
      if (next) await fn(next.item, next.index)
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
  const done = new Promise<void>((resolve, reject) => {
    stream.once('finish', resolve)
    stream.once('error', reject)
  })
  let downloaded = initial
  let nextEmit = initial + PROGRESS_EMIT_INTERVAL
  const reader = body.getReader()
  try {
    while (true) {
      const { done: streamDone, value } = await reader.read()
      if (streamDone) break
      if (!value) continue
      if (!stream.write(value)) await once(stream, 'drain')
      downloaded += value.byteLength
      if (progress && (downloaded >= nextEmit || (total > 0 && downloaded === total))) {
        progress(downloaded)
        nextEmit = downloaded + PROGRESS_EMIT_INTERVAL
      }
    }
  } finally {
    stream.end()
    await done
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

async function fetchWithTimeout(url: string, init: RequestInit, timeoutMs: number, proxy?: string): Promise<Response> {
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(), timeoutMs)
  const requestInit = withProxy({ ...init, signal: controller.signal }, proxy)
  try {
    return await fetch(url, requestInit)
  } catch (err) {
    if (err instanceof Error && err.name === 'AbortError') throw new Error(`request timed out after ${timeoutMs}ms`)
    throw err
  } finally {
    clearTimeout(timer)
  }
}

function withProxy(init: RequestInit, proxy: string | undefined): RequestInit {
  const trimmed = proxy?.trim()
  return trimmed ? ({ ...init, proxy: trimmed } as RequestInit) : init
}

async function runFfmpegMux(ffmpeg: string, videoPath: string, audioPath: string, outputPath: string): Promise<void> {
  await rm(outputPath, { force: true })
  await mkdir(dirname(outputPath), { recursive: true })
  const args = ['-hide_banner', '-y', '-i', videoPath, '-i', audioPath, '-c', 'copy', '-f', 'mp4', outputPath]
  await new Promise<void>((resolve, reject) => {
    const child = spawn(ffmpeg, args, { stdio: ['ignore', 'ignore', 'pipe'] })
    let stderr = ''
    child.stderr?.on('data', (chunk) => {
      stderr += chunk.toString()
      if (stderr.length > 8000) stderr = stderr.slice(-8000)
    })
    child.once('error', (err) => reject(new Error(`failed to start ffmpeg (${ffmpeg}): ${err.message}`)))
    child.once('exit', (code, signal) => {
      if (code === 0) resolve()
      else reject(new Error(`ffmpeg failed${signal ? ` with signal ${signal}` : ` with exit code ${code}`}${stderr.trim() ? `: ${stderr.trim()}` : ''}`))
    })
  })
  if (!(await fileExists(outputPath))) throw new Error('ffmpeg did not produce output file')
}

async function writeZipArchive(dest: string, entries: Array<{ name: string; path: string }>): Promise<void> {
  await mkdir(dirname(dest), { recursive: true })
  const partPath = `${dest}.part`
  const tmpPath = `${partPath}.ziptmp`
  await rm(tmpPath, { force: true })

  const central: Buffer[] = []
  let offset = 0
  const out = await open(tmpPath, 'w')
  try {
    for (const entry of entries) {
      const data = await readFile(entry.path)
      const name = Buffer.from(entry.name, 'utf8')
      const crc = crc32(data)
      const sizes = checkedZipSize(data.byteLength, entry.name)
      const localOffset = offset
      const local = zipLocalHeader(name, crc, sizes)
      await writeBuffer(out, local)
      await writeBuffer(out, data)
      offset += local.byteLength + data.byteLength
      central.push(zipCentralHeader(name, crc, sizes, localOffset))
    }

    const centralOffset = offset
    for (const header of central) {
      await writeBuffer(out, header)
      offset += header.byteLength
    }
    const centralSize = offset - centralOffset
    await writeBuffer(out, zipEndOfCentralDirectory(central.length, centralSize, centralOffset))
  } finally {
    await out.close()
  }

  await rename(tmpPath, partPath)
  await finalizePart(dest)
}

function zipLocalHeader(name: Buffer, crc: number, size: number): Buffer {
  const header = Buffer.alloc(30)
  const { time, date } = zipDosDateTime(new Date())
  header.writeUInt32LE(0x04034b50, 0)
  header.writeUInt16LE(20, 4)
  header.writeUInt16LE(0x0800, 6)
  header.writeUInt16LE(0, 8)
  header.writeUInt16LE(time, 10)
  header.writeUInt16LE(date, 12)
  header.writeUInt32LE(crc, 14)
  header.writeUInt32LE(size, 18)
  header.writeUInt32LE(size, 22)
  header.writeUInt16LE(name.byteLength, 26)
  header.writeUInt16LE(0, 28)
  return Buffer.concat([header, name])
}

function zipCentralHeader(name: Buffer, crc: number, size: number, localOffset: number): Buffer {
  if (localOffset > 0xffffffff) throw new Error('zip64 is not supported yet')
  const header = Buffer.alloc(46)
  const { time, date } = zipDosDateTime(new Date())
  header.writeUInt32LE(0x02014b50, 0)
  header.writeUInt16LE(20, 4)
  header.writeUInt16LE(20, 6)
  header.writeUInt16LE(0x0800, 8)
  header.writeUInt16LE(0, 10)
  header.writeUInt16LE(time, 12)
  header.writeUInt16LE(date, 14)
  header.writeUInt32LE(crc, 16)
  header.writeUInt32LE(size, 20)
  header.writeUInt32LE(size, 24)
  header.writeUInt16LE(name.byteLength, 28)
  header.writeUInt16LE(0, 30)
  header.writeUInt16LE(0, 32)
  header.writeUInt16LE(0, 34)
  header.writeUInt16LE(0, 36)
  header.writeUInt32LE(0, 38)
  header.writeUInt32LE(localOffset, 42)
  return Buffer.concat([header, name])
}

function zipEndOfCentralDirectory(count: number, centralSize: number, centralOffset: number): Buffer {
  if (count > 0xffff || centralSize > 0xffffffff || centralOffset > 0xffffffff) throw new Error('zip64 is not supported yet')
  const header = Buffer.alloc(22)
  header.writeUInt32LE(0x06054b50, 0)
  header.writeUInt16LE(0, 4)
  header.writeUInt16LE(0, 6)
  header.writeUInt16LE(count, 8)
  header.writeUInt16LE(count, 10)
  header.writeUInt32LE(centralSize, 12)
  header.writeUInt32LE(centralOffset, 16)
  header.writeUInt16LE(0, 20)
  return header
}

function checkedZipSize(size: number, name: string): number {
  if (size > 0xffffffff) throw new Error(`zip entry is too large for zip32: ${name}`)
  return size
}

async function writeBuffer(out: Awaited<ReturnType<typeof open>>, buffer: Buffer | Uint8Array): Promise<void> {
  await out.write(buffer, 0, buffer.byteLength)
}

function zipDosDateTime(date: Date): { time: number; date: number } {
  const year = Math.max(1980, date.getFullYear())
  return {
    time: (date.getHours() << 11) | (date.getMinutes() << 5) | Math.floor(date.getSeconds() / 2),
    date: ((year - 1980) << 9) | ((date.getMonth() + 1) << 5) | date.getDate(),
  }
}

const CRC32_TABLE = (() => {
  const table = new Uint32Array(256)
  for (let i = 0; i < table.length; i++) {
    let c = i
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1
    table[i] = c >>> 0
  }
  return table
})()

function crc32(data: Uint8Array): number {
  let c = 0xffffffff
  for (const byte of data) c = CRC32_TABLE[(c ^ byte) & 0xff] ^ (c >>> 8)
  return (c ^ 0xffffffff) >>> 0
}

async function removeDownloadState(path: string): Promise<void> {
  await rm(path, { force: true })
  await rm(`${path}.part`, { force: true })
  await rm(`${path}.parts`, { force: true, recursive: true })
}

async function fileExists(path: string): Promise<boolean> {
  try {
    const st = await stat(path)
    return st.isFile()
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

function sanitizeZipEntryName(name: string, index: number): string {
  const parts = name
    .trim()
    .replaceAll('\\', '/')
    .split('/')
    .map((part) => sanitizeFilename(part))
    .filter(Boolean)
  return parts.join('/') || `entry-${index + 1}`
}

function uniqueZipEntryName(name: string, used: Set<string>): string {
  if (!used.has(name)) {
    used.add(name)
    return name
  }

  const parts = name.split('/')
  const last = parts.pop() || 'entry'
  const ext = extname(last)
  const stem = last.slice(0, last.length - ext.length) || 'entry'
  for (let i = 1; i < 10000; i++) {
    const candidate = [...parts, `${stem} (${i})${ext}`].join('/')
    if (!used.has(candidate)) {
      used.add(candidate)
      return candidate
    }
  }
  throw new Error(`could not allocate unique zip entry name for ${name}`)
}

function extensionFromUrl(url: string, fallback: string): string {
  try {
    const ext = extname(new URL(url).pathname)
    return ext && ext.length <= 12 ? ext : fallback
  } catch {
    return fallback
  }
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

function isRetryableError(err: unknown): boolean {
  const status = httpStatusFromError(err)
  if (status !== undefined) return status === 408 || status === 429 || status >= 500
  return true
}

function isHttpStatusError(err: unknown, status: number): boolean {
  return httpStatusFromError(err) === status
}

function httpStatusFromError(err: unknown): number | undefined {
  const message = err instanceof Error ? err.message : String(err)
  const match = message.match(/\bHTTP\s+(\d{3})\b/)
  return match ? Number.parseInt(match[1], 10) : undefined
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms))
}
