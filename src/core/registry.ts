import type { Platform } from './types.js'
import { bilibiliPlatform } from '../platforms/bilibili/index.js'
import { douyinPlatform } from '../platforms/douyin/index.js'
import { kuaishouPlatform } from '../platforms/kuaishou/index.js'
import { xiaohongshuPlatform } from '../platforms/xiaohongshu/index.js'

export const platforms: Platform[] = [douyinPlatform, xiaohongshuPlatform, bilibiliPlatform, kuaishouPlatform]

export function resolvePlatform(input: string): Platform {
  const matched = platforms.filter((p) => p.match(input))
  if (matched.length === 1) return matched[0]
  if (matched.length === 0) throw new Error('could not detect platform from input URL/share text')
  throw new Error(`input matches multiple platforms (${matched.map((p) => p.id).join(', ')})`)
}
