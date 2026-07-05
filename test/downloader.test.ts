import { afterEach, expect, test } from 'bun:test'
import { createServer, type Server } from 'node:http'
import { chmod, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { runDownloadPlan, summarizeDownloadEvents } from '../src/core/downloader.js'

const originalFetch = globalThis.fetch
const cleanups: Array<() => Promise<void> | void> = []

afterEach(async () => {
  globalThis.fetch = originalFetch
  while (cleanups.length > 0) await cleanups.pop()?.()
})

test('downloads a file plan from a range-capable server', async () => {
  const tmp = await tempDir()
  const server = await fixtureServer({ '/file.txt': 'hello downloader\n' })

  const events = await runDownloadPlan({
    outputDir: tmp,
    items: [{ id: 'file', kind: 'file', url: server.url('/file.txt'), filename: 'file.txt' }],
  })
  const summary = summarizeDownloadEvents(events)

  expect(summary.success).toBe(true)
  expect(await readFile(join(tmp, 'file.txt'), 'utf8')).toBe('hello downloader\n')
})

test('resumes an existing .part file instead of auto-renaming', async () => {
  const tmp = await tempDir()
  const body = Buffer.from('0123456789abcdef')
  const server = await fixtureServer({ '/resume.bin': body })
  await writeFile(join(tmp, 'resume.bin.part'), body.subarray(0, 4))

  const events = await runDownloadPlan({
    outputDir: tmp,
    items: [{ id: 'resume', kind: 'file', url: server.url('/resume.bin'), filename: 'resume.bin' }],
  })
  const summary = summarizeDownloadEvents(events)

  expect(summary.success).toBe(true)
  expect(await readFile(join(tmp, 'resume.bin'))).toEqual(body)
})

test('uses multipart chunks above the threshold', async () => {
  const tmp = await tempDir()
  const body = Buffer.alloc(64 * 1024)
  for (let i = 0; i < body.length; i++) body[i] = i % 251
  const server = await fixtureServer({ '/big.bin': body })

  const events = await runDownloadPlan({
    outputDir: tmp,
    multipartThresholdBytes: 1024,
    chunkConcurrency: 3,
    items: [{ id: 'big', kind: 'file', url: server.url('/big.bin'), filename: 'big.bin' }],
  })
  const summary = summarizeDownloadEvents(events)

  expect(summary.success).toBe(true)
  expect(await readFile(join(tmp, 'big.bin'))).toEqual(body)
  expect(server.rangeRequests()).toBeGreaterThan(1)
})

test('passes proxy option to fetch requests', async () => {
  const tmp = await tempDir()
  const seenProxy: Array<string | undefined> = []
  globalThis.fetch = ((input: RequestInfo | URL, init?: RequestInit) => {
    seenProxy.push((init as RequestInit & { proxy?: string } | undefined)?.proxy)
    const headers = new Headers({ 'content-length': '2' })
    if ((init?.headers as Record<string, string> | undefined)?.Range) headers.set('content-range', 'bytes 0-1/2')
    return Promise.resolve(new Response('ok', { status: 200, headers }))
  }) as typeof fetch

  const events = await runDownloadPlan({
    outputDir: tmp,
    proxy: 'http://127.0.0.1:8888',
    items: [{ id: 'proxied', kind: 'file', url: 'https://example.com/file.txt', filename: 'file.txt' }],
  })
  const summary = summarizeDownloadEvents(events)

  expect(summary.success).toBe(true)
  expect(seenProxy.length).toBeGreaterThanOrEqual(2)
  expect(seenProxy.every((value) => value === 'http://127.0.0.1:8888')).toBe(true)
})

test('respects item concurrency limit', async () => {
  const tmp = await tempDir()
  const body = Buffer.from('concurrency')
  let inFlight = 0
  let maxInFlight = 0
  const server: Server = createServer(async (req, res) => {
    inFlight++
    maxInFlight = Math.max(maxInFlight, inFlight)
    await delay(20)
    const range = req.headers.range
    const match = typeof range === 'string' ? range.match(/^bytes=(\d+)-(\d*)$/) : null
    if (match) {
      const start = Number.parseInt(match[1], 10)
      const end = Math.min(match[2] ? Number.parseInt(match[2], 10) : body.length - 1, body.length - 1)
      const chunk = body.subarray(start, end + 1)
      res.writeHead(206, { 'content-length': String(chunk.byteLength), 'content-range': `bytes ${start}-${end}/${body.length}` })
      res.end(chunk, () => inFlight--)
      return
    }
    res.writeHead(200, { 'content-length': String(body.byteLength) })
    res.end(body, () => inFlight--)
  })
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve))
  cleanups.push(() => new Promise<void>((resolve, reject) => server.close((err) => (err ? reject(err) : resolve()))))
  const address = server.address()
  if (!address || typeof address === 'string') throw new Error('server did not listen on tcp')

  const events = await runDownloadPlan({
    outputDir: tmp,
    concurrency: 1,
    items: [
      { id: 'one', kind: 'file', url: `http://127.0.0.1:${address.port}/one.bin`, filename: 'one.bin' },
      { id: 'two', kind: 'file', url: `http://127.0.0.1:${address.port}/two.bin`, filename: 'two.bin' },
    ],
  })
  const summary = summarizeDownloadEvents(events)

  expect(summary.success).toBe(true)
  expect(maxInFlight).toBe(1)
})

test('creates a partial zip when some entries fail', async () => {
  const tmp = await tempDir()
  const server = await fixtureServer({ '/a.txt': 'aaa', '/b.txt': 'bbb' })

  const events = await runDownloadPlan({
    outputDir: tmp,
    items: [
      {
        id: 'zip',
        kind: 'zip',
        filename: 'archive.zip',
        entries: [
          { name: 'a.txt', url: server.url('/a.txt') },
          { name: 'folder/b.txt', url: server.url('/b.txt') },
          { name: 'missing.txt', url: server.url('/missing.txt') },
        ],
      },
    ],
  })
  const summary = summarizeDownloadEvents(events)
  const zip = await readFile(join(tmp, 'archive.zip'))

  expect(summary.success).toBe(false)
  expect(summary.artifacts[0]?.partial).toBe(true)
  expect(zip.readUInt32LE(0)).toBe(0x04034b50)
  expect(zip.includes(Buffer.from('a.txt'))).toBe(true)
  expect(zip.includes(Buffer.from('folder/b.txt'))).toBe(true)
  expect(zip.includes(Buffer.from('aaa'))).toBe(true)
  expect(zip.includes(Buffer.from('bbb'))).toBe(true)
})

test('downloads dash tracks and invokes configured ffmpeg', async () => {
  const tmp = await tempDir()
  const server = await fixtureServer({ '/video.mp4': 'video-bytes', '/audio.m4a': 'audio-bytes' })
  const fakeFfmpeg = join(tmp, 'fake-ffmpeg.sh')
  await writeFile(fakeFfmpeg, '#!/bin/sh\nfor last do :; done\nprintf muxed > "$last"\n')
  await chmod(fakeFfmpeg, 0o700)

  const events = await runDownloadPlan({
    outputDir: tmp,
    ffmpeg: fakeFfmpeg,
    items: [
      {
        id: 'dash',
        kind: 'dash',
        filename: 'merged.mp4',
        videoUrl: server.url('/video.mp4'),
        audioUrl: server.url('/audio.m4a'),
      },
    ],
  })
  const summary = summarizeDownloadEvents(events)

  expect(summary.success).toBe(true)
  expect(await readFile(join(tmp, 'merged.mp4'), 'utf8')).toBe('muxed')
})

async function tempDir(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), 'easydl-test-'))
  cleanups.push(() => rm(dir, { recursive: true, force: true }))
  return dir
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms))
}

async function fixtureServer(routes: Record<string, string | Buffer>) {
  let rangeCount = 0
  const server: Server = createServer((req, res) => {
    const url = new URL(req.url ?? '/', `http://${req.headers.host ?? '127.0.0.1'}`)
    const value = routes[url.pathname]
    if (value === undefined) {
      res.writeHead(404, { 'content-length': '0' })
      res.end()
      return
    }

    const body = Buffer.isBuffer(value) ? value : Buffer.from(value)
    const range = req.headers.range
    const match = typeof range === 'string' ? range.match(/^bytes=(\d+)-(\d*)$/) : null
    if (match) {
      rangeCount++
      const start = Number.parseInt(match[1], 10)
      const end = Math.min(match[2] ? Number.parseInt(match[2], 10) : body.length - 1, body.length - 1)
      if (start >= body.length || end < start) {
        res.writeHead(416, { 'content-length': '0' })
        res.end()
        return
      }
      const chunk = body.subarray(start, end + 1)
      res.writeHead(206, {
        'accept-ranges': 'bytes',
        'content-length': String(chunk.byteLength),
        'content-range': `bytes ${start}-${end}/${body.length}`,
      })
      res.end(chunk)
      return
    }

    res.writeHead(200, { 'accept-ranges': 'bytes', 'content-length': String(body.byteLength) })
    res.end(body)
  })

  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve))
  cleanups.push(() => new Promise<void>((resolve, reject) => server.close((err) => (err ? reject(err) : resolve()))))
  const address = server.address()
  if (!address || typeof address === 'string') throw new Error('server did not listen on tcp')
  return {
    url: (path: string) => `http://127.0.0.1:${address.port}${path}`,
    rangeRequests: () => rangeCount,
  }
}
