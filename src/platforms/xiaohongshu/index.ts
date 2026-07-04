import type { DownloadOptions, DownloadPlan, MediaInfo, Platform, ResolveContext } from '../../core/types.js'

const xhsHostPattern = /(?:^|\/\/)(?:[^/]+\.)?(?:xiaohongshu\.com|xhslink\.com)(?:[/:?#]|$)/i
const noteIdPattern = /\b[0-9a-f]{24}\b/i

export const xiaohongshuPlatform: Platform = {
  id: 'xhs',
  displayName: '小红书',
  match(input: string): boolean {
    return xhsHostPattern.test(input) || noteIdPattern.test(input.trim())
  },
  async resolve(_input: string, _ctx: ResolveContext): Promise<MediaInfo> {
    throw new Error('xiaohongshu resolver is not implemented yet (planned M2)')
  },
  async createDownloadPlan(_info: MediaInfo, _options: DownloadOptions): Promise<DownloadPlan> {
    throw new Error('xiaohongshu download plan is not implemented yet (planned M2)')
  },
}
