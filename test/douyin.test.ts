import { afterEach, expect, test } from 'bun:test'
import { douyinPlatform } from '../src/platforms/douyin/index.js'

const originalFetch = globalThis.fetch
const awemeId = '7312345678901234567'

afterEach(() => {
  globalThis.fetch = originalFetch
})

test('resolves a short-link video and builds a file plan', async () => {
  mockFetch(async (url) => {
    if (url === 'https://v.douyin.com/abc/') {
      return new Response(null, {
        status: 302,
        headers: { location: `https://www.douyin.com/video/${awemeId}` },
      })
    }
    if (url === `https://www.iesdouyin.com/share/video/${awemeId}/?app=aweme`) {
      return htmlResponse(routerDataHtml(videoItem()))
    }
    throw new Error(`unexpected fetch ${url}`)
  })

  const info = await douyinPlatform.resolve('复制口令 https://v.douyin.com/abc/', { configDir: '/tmp/easydl-test' })
  expect(info.platform).toBe('douyin')
  expect(info.type).toBe('video')
  expect(info.streams?.map((stream) => stream.id)).toEqual(['1080p', '720p'])

  const bestPlan = await douyinPlatform.createDownloadPlan(info, { outputDir: '/tmp/out' })
  expect(bestPlan.items[0]).toMatchObject({ kind: 'file', url: 'https://video.example.com/1080-play.mp4' })

  const plan720 = await douyinPlatform.createDownloadPlan(info, { outputDir: '/tmp/out', quality: '720p' })
  expect(plan720.items[0]).toMatchObject({ kind: 'file', url: 'https://video.example.com/720-play.mp4' })
})

test('builds selected album zip entries from share note data', async () => {
  mockFetch(async (url) => {
    if (url === `https://www.iesdouyin.com/share/video/${awemeId}/?app=aweme`) {
      return htmlResponse('<html><script>window._ROUTER_DATA={"loaderData":{}};</script></html>')
    }
    if (url === `https://www.iesdouyin.com/share/note/${awemeId}/?app=aweme`) {
      return htmlResponse(routerDataHtml(albumItem()))
    }
    throw new Error(`unexpected fetch ${url}`)
  })

  const info = await douyinPlatform.resolve(`https://www.douyin.com/note/${awemeId}`, { configDir: '/tmp/easydl-test' })
  expect(info.type).toBe('album')
  expect(info.images?.length).toBe(2)

  const plan = await douyinPlatform.createDownloadPlan(info, { outputDir: '/tmp/out', images: [1] })
  expect(plan.items[0]?.kind).toBe('zip')
  if (plan.items[0]?.kind !== 'zip') throw new Error('expected zip item')
  expect(plan.items[0].entries).toEqual([
    expect.objectContaining({ name: '02.jpg', url: 'https://image.example.com/2.jpg' }),
    expect.objectContaining({ name: '02_video.mp4', url: 'https://video.example.com/clip-play.mp4' }),
  ])
})

test('uses ratio probing when share video lacks bit_rate URLs', async () => {
  mockFetch(async (url, init) => {
    if (url === `https://www.iesdouyin.com/share/video/${awemeId}/?app=aweme`) {
      return htmlResponse(routerDataHtml(videoItemWithoutBitrate()))
    }
    if (url.startsWith('https://aweme.snssdk.com/aweme/v1/play/')) {
      const parsed = new URL(url)
      const ratio = parsed.searchParams.get('ratio')
      if (ratio === '1080p') throw new Error('should not probe above source resolution')
      if (ratio === '720p') {
        return new Response('xx', {
          status: 206,
          headers: { 'content-range': 'bytes 0-1/12345', 'content-type': 'video/mp4' },
        })
      }
      return new Response('', { status: 404 })
    }
    throw new Error(`unexpected fetch ${url} ${JSON.stringify(init?.headers)}`)
  })

  const info = await douyinPlatform.resolve(`https://www.douyin.com/video/${awemeId}`, { configDir: '/tmp/easydl-test' })
  expect(info.streams?.[0]).toMatchObject({ id: '720p', size: 12345 })
})

test('retries share page with ttwid when router data is hidden', async () => {
  mockFetch(async (url, init) => {
    const headers = init?.headers as Record<string, string> | undefined
    if (url === `https://www.iesdouyin.com/share/video/${awemeId}/?app=aweme`) {
      if (headers?.Cookie === 'ttwid=test-token') return htmlResponse(routerDataHtml(videoItemWithoutBitrate()))
      return htmlResponse('<html>open app</html>')
    }
    if (url === 'https://ttwid.bytedance.com/ttwid/union/register/') {
      return new Response('{}', { status: 200, headers: { 'set-cookie': 'ttwid=test-token; Path=/; Domain=bytedance.com' } })
    }
    if (url.startsWith('https://aweme.snssdk.com/aweme/v1/play/')) {
      return new Response('xx', { status: 206, headers: { 'content-range': 'bytes 0-1/222', 'content-type': 'video/mp4' } })
    }
    throw new Error(`unexpected fetch ${url} ${JSON.stringify(init?.headers)}`)
  })

  const info = await douyinPlatform.resolve(`https://www.douyin.com/video/${awemeId}`, { configDir: '/tmp/easydl-test' })
  expect(info.streams?.[0]).toMatchObject({ id: '720p', size: 222 })
})

test('does not expose unplayable play_addr URLs when ratio probing fails', async () => {
  mockFetch(async (url) => {
    if (url === `https://www.iesdouyin.com/share/video/${awemeId}/?app=aweme`) return htmlResponse(routerDataHtml(videoItemWithoutBitrate()))
    if (url === `https://www.iesdouyin.com/share/note/${awemeId}/?app=aweme`) return new Response('not found', { status: 404 })
    if (url.startsWith('https://aweme.snssdk.com/aweme/v1/play/')) return new Response('', { status: 404 })
    throw new Error(`unexpected fetch ${url}`)
  })

  await expect(douyinPlatform.resolve(`https://www.douyin.com/video/${awemeId}`, { configDir: '/tmp/easydl-test' })).rejects.toThrow('no playable video URL')
})

function videoItem() {
  return {
    aweme_id: awemeId,
    desc: '视频标题',
    aweme_type: 0,
    author: { nickname: '作者' },
    video: {
      cover: { url_list: ['https://image.example.com/cover.jpg'] },
      bit_rate: [
        {
          gear_name: 'normal_720p',
          bit_rate: 800,
          play_addr: { width: 720, height: 1280, url_list: ['https://video.example.com/720-playwm.mp4'] },
        },
        {
          gear_name: 'normal_1080p',
          bit_rate: 1200,
          play_addr: { width: 1080, height: 1920, url_list: ['https://video.example.com/1080-playwm.mp4'] },
        },
      ],
    },
  }
}

function videoItemWithoutBitrate() {
  return {
    aweme_id: awemeId,
    desc: '待探测视频',
    aweme_type: 0,
    author: { nickname: '作者' },
    video: {
      play_addr: { uri: 'video-uri', width: 720, height: 1280, url_list: [] },
      bit_rate: [],
    },
  }
}

function albumItem() {
  return {
    aweme_id: awemeId,
    desc: '图集标题',
    aweme_type: 68,
    author: { nickname: '作者' },
    images: [
      { url_list: ['https://image.example.com/1.jpg'], width: 1080, height: 1440 },
      {
        url_list: ['https://image.example.com/2.jpg'],
        width: 1080,
        height: 1440,
        video: { play_addr: { url_list: ['https://video.example.com/clip-playwm.mp4'] } },
      },
    ],
  }
}

function routerDataHtml(item: unknown): string {
  const payload = { loaderData: { [`video_${awemeId}`]: { videoInfoRes: { item_list: [item] } } } }
  return `<html><script>window._ROUTER_DATA=${JSON.stringify(payload)};</script></html>`
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
