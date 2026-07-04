import { afterEach, expect, test } from 'bun:test'
import { xiaohongshuPlatform } from '../src/platforms/xiaohongshu/index.js'

const originalFetch = globalThis.fetch
const noteId = '64f123456789abcdef123456'

afterEach(() => {
  globalThis.fetch = originalFetch
})

test('resolves a redirected short-link video and builds a file plan', async () => {
  const requested: string[] = []
  mockFetch(async (url) => {
    requested.push(url)
    if (url === 'https://xhslink.com/video') {
      return new Response(null, {
        status: 302,
        headers: { location: `https://www.xiaohongshu.com/explore/${noteId}?xsec_token=tok123` },
      })
    }
    if (url === `https://www.xiaohongshu.com/explore/${noteId}?xsec_token=tok123`) {
      return htmlResponse(initialStateHtml(noteId, videoNote()))
    }
    throw new Error(`unexpected fetch ${url}`)
  })

  const info = await xiaohongshuPlatform.resolve('看这个 https://xhslink.com/video', { configDir: '/tmp/easydl-test' })
  expect(requested).toContain(`https://www.xiaohongshu.com/explore/${noteId}?xsec_token=tok123`)
  expect(info.platform).toBe('xhs')
  expect(info.type).toBe('video')
  expect(info.streams?.map((stream) => stream.id)).toEqual(['hd_115', 'hd_259'])

  const bestPlan = await xiaohongshuPlatform.createDownloadPlan(info, { outputDir: '/tmp/out' })
  expect(bestPlan.items[0]).toMatchObject({ kind: 'file', url: 'https://sns-video.example.com/hd-h265.mp4' })

  const h264Plan = await xiaohongshuPlatform.createDownloadPlan(info, { outputDir: '/tmp/out', quality: 'hd_259' })
  expect(h264Plan.items[0]).toMatchObject({
    kind: 'file',
    url: 'https://sns-video.example.com/hd-h264.mp4',
    backupUrls: ['https://sns-video.example.com/hd-h264-backup.mp4'],
  })
})

test('resolves an HTML short-link fallback and builds selected image zip entries', async () => {
  mockFetch(async (url) => {
    if (url === 'https://xhslink.com/album') {
      return new Response(`<a href="https://www.xiaohongshu.com/explore/${noteId}?xsec_token=tok_album">Found</a>`, {
        status: 200,
        headers: { 'content-type': 'text/html' },
      })
    }
    if (url === `https://www.xiaohongshu.com/explore/${noteId}?xsec_token=tok_album`) {
      return htmlResponse(initialStateHtml(noteId, imageNote()))
    }
    throw new Error(`unexpected fetch ${url}`)
  })

  const info = await xiaohongshuPlatform.resolve('https://xhslink.com/album', { configDir: '/tmp/easydl-test' })
  expect(info.type).toBe('album')
  expect(info.images?.length).toBe(2)

  const plan = await xiaohongshuPlatform.createDownloadPlan(info, { outputDir: '/tmp/out', images: [1] })
  expect(plan.items[0]?.kind).toBe('zip')
  if (plan.items[0]?.kind !== 'zip') throw new Error('expected zip item')
  expect(plan.items[0].entries).toEqual([
    expect.objectContaining({
      name: '02.jpg',
      url: 'https://sns-img.example.com/second.jpg',
      backupUrls: ['https://sns-img.example.com/second-pre.jpg'],
    }),
  ])
})

test('includes live photo sidecar video in image zip entries', async () => {
  mockFetch(async (url) => {
    if (url === `https://www.xiaohongshu.com/explore/${noteId}`) return htmlResponse(initialStateHtml(noteId, imageNote()))
    throw new Error(`unexpected fetch ${url}`)
  })

  const info = await xiaohongshuPlatform.resolve(noteId, { configDir: '/tmp/easydl-test' })
  const plan = await xiaohongshuPlatform.createDownloadPlan(info, { outputDir: '/tmp/out', images: [0] })

  expect(plan.items[0]?.kind).toBe('zip')
  if (plan.items[0]?.kind !== 'zip') throw new Error('expected zip item')
  expect(plan.items[0].entries).toEqual([
    expect.objectContaining({ name: '01.jpg', url: 'https://sns-img.example.com/first.jpg' }),
    expect.objectContaining({ name: '01_live.mp4', url: 'https://sns-video.example.com/live.mp4' }),
  ])
})

function videoNote() {
  return {
    noteId,
    type: 'video',
    title: '视频标题',
    desc: '视频描述',
    user: { nickname: '作者' },
    imageList: [{ urlDefault: 'https://sns-img.example.com/cover.jpg' }],
    video: {
      media: {
        stream: {
          h264: [
            {
              qualityType: 'HD',
              streamType: 259,
              width: 1280,
              height: 720,
              size: 300,
              masterUrl: 'https://sns-video.example.com/hd-h264.mp4',
              backupUrls: ['https://sns-video.example.com/hd-h264-backup.mp4'],
            },
          ],
          h265: [
            {
              qualityType: 'HD',
              streamType: 115,
              width: 1280,
              height: 720,
              size: 200,
              masterUrl: 'https://sns-video.example.com/hd-h265.mp4',
            },
          ],
        },
      },
    },
  }
}

function imageNote() {
  return {
    noteId,
    type: 'normal',
    title: '图文标题',
    user: { nickname: '作者' },
    imageList: [
      {
        urlDefault: 'https://sns-img.example.com/first.jpg',
        width: 1080,
        height: 1440,
        livePhoto: true,
        stream: { h264: [{ masterUrl: 'https://sns-video.example.com/live.mp4' }] },
      },
      {
        urlDefault: 'https://sns-img.example.com/second.jpg',
        urlPre: 'https://sns-img.example.com/second-pre.jpg',
        width: 1080,
        height: 1440,
      },
    ],
  }
}

function initialStateHtml(id: string, note: unknown): string {
  return `<html><script>window.__INITIAL_STATE__=${JSON.stringify({ note: { noteDetailMap: { [id]: { note } } } })};</script></html>`
}

function mockFetch(fn: (url: string, init?: RequestInit) => Promise<Response>): void {
  globalThis.fetch = ((input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
    return fn(url, init)
  }) as typeof fetch
}

function htmlResponse(value: string): Response {
  return new Response(value, { status: 200, headers: { 'content-type': 'text/html; charset=utf-8' } })
}
