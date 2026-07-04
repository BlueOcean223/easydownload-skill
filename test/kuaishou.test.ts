import { afterEach, expect, test } from 'bun:test'
import { kuaishouPlatform } from '../src/platforms/kuaishou/index.js'

const originalFetch = globalThis.fetch

afterEach(() => {
  globalThis.fetch = originalFetch
})

test('resolves a short-link video and builds a file plan', async () => {
  mockFetch(async (url, init) => {
    if (url.startsWith('https://v.kuaishou.com/')) {
      return new Response(null, {
        status: 302,
        headers: { location: 'https://v.m.chenzhongtech.com/fw/photo/3xvideo?photoId=3xvideo' },
      })
    }
    if (url.includes('/photo/simple/info')) {
      expect(init?.method).toBe('POST')
      return jsonResponse({
        result: 1,
        photo: {
          photoId: '3xvideo',
          photoType: 'VIDEO',
          caption: 'Best Goal',
          userName: '红星新闻',
          coverUrls: [{ url: 'https://p23.a.yximgs.com/cover.jpg' }],
          manifest: {
            adaptationSet: [
              {
                representation: [
                  {
                    qualityType: '720p',
                    videoCodec: 'avc',
                    width: 720,
                    height: 1280,
                    fileSize: 100,
                    url: 'https://cdn.example.com/720.mp4',
                    backupUrl: ['https://backup.example.com/720.mp4'],
                  },
                  {
                    qualityType: '1080p',
                    videoCodec: 'hevc',
                    width: 1080,
                    height: 1920,
                    fileSize: 200,
                    url: 'https://cdn.example.com/1080.mp4',
                  },
                ],
              },
            ],
          },
        },
      })
    }
    throw new Error(`unexpected fetch ${url}`)
  })

  const info = await kuaishouPlatform.resolve('分享 https://v.kuaishou.com/J0J8SFML', { configDir: '/tmp/easydl-test' })
  expect(info.id).toBe('3xvideo')
  expect(info.type).toBe('video')
  expect(info.streams?.map((stream) => stream.id)).toEqual(['720p_avc', '1080p_hevc'])

  const bestPlan = await kuaishouPlatform.createDownloadPlan(info, { outputDir: '/tmp/out' })
  expect(bestPlan.items[0]).toMatchObject({ kind: 'file', url: 'https://cdn.example.com/1080.mp4' })

  const avcPlan = await kuaishouPlatform.createDownloadPlan(info, { outputDir: '/tmp/out', quality: '720p_avc' })
  expect(avcPlan.items[0]).toMatchObject({ kind: 'file', url: 'https://cdn.example.com/720.mp4' })
})

test('builds a selected atlas zip plan with background music', async () => {
  mockFetch(async (url) => {
    if (url.includes('/photo/simple/info')) {
      return jsonResponse({
        result: 1,
        photo: {
          photoId: '3xatlas',
          photoType: 'HORIZONTAL_ATLAS',
          caption: 'Album Title',
          userName: '作者',
          coverUrls: [{ url: 'https://p23.a.yximgs.com/cover.jpg' }],
        },
        atlas: {
          cdnList: [{ cdn: 'p23.a.yximgs.com' }],
          list: ['/ufile/atlas/a0.jpg', '/ufile/atlas/a1.jpg'],
          size: [
            { w: 1080, h: 1184 },
            { w: 1080, h: 1164 },
          ],
          music: '/ufile/atlas/music.m4a',
          musicCdnList: [{ cdn: 'hwmov.a.yximgs.com' }],
        },
      })
    }
    throw new Error(`unexpected fetch ${url}`)
  })

  const info = await kuaishouPlatform.resolve('https://www.kuaishou.com/short-video/3xatlas', { configDir: '/tmp/easydl-test' })
  expect(info.images?.length).toBe(2)

  const plan = await kuaishouPlatform.createDownloadPlan(info, { outputDir: '/tmp/out', images: [1] })
  expect(plan.items[0]?.kind).toBe('zip')
  if (plan.items[0]?.kind !== 'zip') throw new Error('expected zip item')
  expect(plan.items[0].entries).toEqual([
    expect.objectContaining({ name: '02.jpg', url: 'https://p23.a.yximgs.com/ufile/atlas/a1.jpg' }),
    expect.objectContaining({ name: 'music.m4a', url: 'https://hwmov.a.yximgs.com/ufile/atlas/music.m4a' }),
  ])
})

test('builds a single-picture file plan', async () => {
  mockFetch(async (url) => {
    if (url.includes('/photo/simple/info')) {
      return jsonResponse({
        result: 1,
        photo: {
          photoId: '3xpic123',
          photoType: 'SINGLE_PICTURE',
          caption: 'Only One',
          coverUrls: [{ url: 'https://p23.a.yximgs.com/pic.jpg' }],
        },
      })
    }
    throw new Error(`unexpected fetch ${url}`)
  })

  const info = await kuaishouPlatform.resolve('3xpic123', { configDir: '/tmp/easydl-test' })
  const plan = await kuaishouPlatform.createDownloadPlan(info, { outputDir: '/tmp/out' })

  expect(plan.items[0]).toMatchObject({ kind: 'file', url: 'https://p23.a.yximgs.com/pic.jpg' })
})

function mockFetch(fn: (url: string, init?: RequestInit) => Promise<Response>): void {
  globalThis.fetch = ((input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
    return fn(url, init)
  }) as typeof fetch
}

function jsonResponse(value: unknown): Response {
  return new Response(JSON.stringify(value), { status: 200, headers: { 'content-type': 'application/json' } })
}
