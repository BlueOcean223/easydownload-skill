import { afterEach, expect, test } from 'bun:test'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createBilibiliQRSession, pollBilibiliQRSession } from '../src/core/bilibili-auth.js'
import { loadAuth } from '../src/core/auth-store.js'
import { bilibiliPlatform } from '../src/platforms/bilibili/index.js'

const originalFetch = globalThis.fetch
const bvid = 'BV1xx411c7mD'

afterEach(() => {
  globalThis.fetch = originalFetch
})

test('resolves b23 ordinary video and builds DASH plans for all parts', async () => {
  mockFetch(async (url) => {
    if (url === 'https://b23.tv/abc') {
      return new Response(null, { status: 302, headers: { location: `https://www.bilibili.com/video/${bvid}?p=2` } })
    }
    if (url.includes('/x/web-interface/view')) return jsonResponse({ code: 0, data: viewData() })
    if (url.includes('/x/player/playurl')) {
      const parsed = new URL(url)
      return jsonResponse({ code: 0, data: playData(Number(parsed.searchParams.get('cid'))) })
    }
    throw new Error(`unexpected fetch ${url}`)
  })

  const info = await bilibiliPlatform.resolve('https://b23.tv/abc', { configDir: '/tmp/easydl-test' })
  expect(info.platform).toBe('bilibili')
  expect(info.type).toBe('playlist')
  expect(info.parts?.length).toBe(2)
  expect(info.streams?.[0]).toMatchObject({ id: '80', qualityName: '1080P' })

  const defaultPlan = await bilibiliPlatform.createDownloadPlan(info, { outputDir: '/tmp/out', quality: '80' })
  expect(defaultPlan.items[0]).toMatchObject({ kind: 'dash', videoUrl: 'https://video.example.com/222-80.m4s', audioUrl: 'https://audio.example.com/222.m4s' })

  const plan = await bilibiliPlatform.createDownloadPlan(info, { outputDir: '/tmp/out', allParts: true, quality: '80' })
  expect(plan.items).toHaveLength(2)
  expect(plan.items[0]).toMatchObject({ kind: 'dash', videoUrl: 'https://video.example.com/111-80.m4s', audioUrl: 'https://audio.example.com/111.m4s' })
  expect(plan.items[1]).toMatchObject({ kind: 'dash', videoUrl: 'https://video.example.com/222-80.m4s', audioUrl: 'https://audio.example.com/222.m4s' })
})

test('resolves bangumi episode and creates DASH plan', async () => {
  mockFetch(async (url) => {
    if (url.includes('/pgc/view/web/season')) return jsonResponse({ code: 0, result: seasonData() })
    if (url.includes('/pgc/player/web/playurl')) return jsonResponse({ code: 0, result: playData(333) })
    throw new Error(`unexpected fetch ${url}`)
  })

  const info = await bilibiliPlatform.resolve('https://www.bilibili.com/bangumi/play/ep123', { configDir: '/tmp/easydl-test' })
  expect(info.type).toBe('bangumi')
  expect(info.parts?.map((part) => part.title)).toEqual(['1 第一集', '2 第二集'])

  const plan = await bilibiliPlatform.createDownloadPlan(info, { outputDir: '/tmp/out' })
  expect(plan.items[0]).toMatchObject({ kind: 'dash', videoUrl: 'https://video.example.com/333-80.m4s', audioUrl: 'https://audio.example.com/333.m4s' })
})

test('creates and polls bilibili QR login sessions', async () => {
  const configDir = await tempDir()
  mockFetch(async (url) => {
    if (url.includes('/qrcode/generate')) {
      return jsonResponse({ code: 0, data: { url: 'https://passport.bilibili.com/qrcode/test', qrcode_key: 'key123' } })
    }
    if (url.includes('/qrcode/poll')) {
      return new Response(JSON.stringify({ code: 0, data: { code: 0, message: 'ok' } }), {
        status: 200,
        headers: { 'content-type': 'application/json', 'set-cookie': 'SESSDATA=sess%2Fdata; Path=/; Domain=.bilibili.com' },
      })
    }
    throw new Error(`unexpected fetch ${url}`)
  })

  const session = await createBilibiliQRSession({ configDir })
  expect(session.sessionId).toBe('key123')
  expect(session.qrPath.endsWith('key123.png')).toBe(true)

  const result = await pollBilibiliQRSession(session.sessionId, { configDir })
  expect(result.loggedIn).toBe(true)
  expect(result.sessData).toBe('sess/data')
  expect((await loadAuth(configDir)).bilibili?.sessData).toBe('sess/data')
})

function viewData() {
  return {
    bvid,
    aid: 170001,
    title: '普通视频',
    pic: 'https://i.example.com/cover.jpg',
    duration: 120,
    owner: { name: 'UP主' },
    cid: 111,
    pages: [
      { cid: 111, page: 1, part: '第一P', duration: 60 },
      { cid: 222, page: 2, part: '第二P', duration: 60 },
    ],
  }
}

function seasonData() {
  return {
    season_id: 1,
    media_id: 2,
    season_title: '番剧标题',
    title: '番剧',
    cover: 'https://i.example.com/season.jpg',
    episodes: [
      { aid: 1, bvid: 'BVep1', cid: 333, ep_id: 123, id: 123, title: '1', long_title: '第一集', duration: 120, section_type: 0 },
      { aid: 2, bvid: 'BVep2', cid: 444, ep_id: 124, id: 124, title: '2', long_title: '第二集', duration: 120, section_type: 0 },
    ],
  }
}

function playData(cid: number) {
  return {
    accept_quality: [80, 64],
    accept_description: ['1080P', '720P'],
    support_formats: [
      { quality: 80, new_description: '1080P' },
      { quality: 64, new_description: '720P' },
    ],
    dash: {
      video: [
        { id: 80, baseUrl: `https://video.example.com/${cid}-80-h264.m4s`, bandwidth: 1000, width: 1920, height: 1080, codecid: 7 },
        { id: 80, baseUrl: `https://video.example.com/${cid}-80.m4s`, bandwidth: 900, width: 1920, height: 1080, codecid: 12 },
        { id: 64, baseUrl: `https://video.example.com/${cid}-64.m4s`, bandwidth: 500, width: 1280, height: 720, codecid: 7 },
      ],
      audio: [{ id: 30280, baseUrl: `https://audio.example.com/${cid}.m4s`, bandwidth: 128000 }],
    },
  }
}

async function tempDir(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), 'easydl-bili-test-'))
  return dir
}

function mockFetch(fn: (url: string, init?: RequestInit) => Promise<Response>): void {
  globalThis.fetch = ((input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
    return fn(url, init)
  }) as typeof fetch
}

function jsonResponse(value: unknown): Response {
  return new Response(JSON.stringify(value), { status: 200, headers: { 'content-type': 'application/json' } })
}
