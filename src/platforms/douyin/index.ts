import type { DownloadOptions, DownloadPlan, MediaInfo, Platform, ResolveContext } from '../../core/types.js'

const douyinHostPattern = /(?:^|\/\/)(?:[^/]+\.)?(?:douyin\.com|iesdouyin\.com)(?:[/:?#]|$)|(?:^|\/\/)v\.douyin\.com(?:[/:?#]|$)/i

export const douyinPlatform: Platform = {
  id: 'douyin',
  displayName: '抖音',
  match(input: string): boolean {
    return douyinHostPattern.test(input)
  },
  async resolve(_input: string, _ctx: ResolveContext): Promise<MediaInfo> {
    throw new Error('douyin resolver is not implemented yet (planned M3)')
  },
  async createDownloadPlan(_info: MediaInfo, _options: DownloadOptions): Promise<DownloadPlan> {
    throw new Error('douyin download plan is not implemented yet (planned M3)')
  },
}
