import type { DownloadOptions, DownloadPlan, MediaInfo, Platform, ResolveContext } from '../../core/types.js'

const kuaishouPattern = /(?:^|\/\/)(?:[^/]+\.)?(?:kuaishou\.com|kwai\.com|chenzhongtech\.com|gifshow\.com)(?:[/:?#]|$)/i

export const kuaishouPlatform: Platform = {
  id: 'kuaishou',
  displayName: '快手',
  match(input: string): boolean {
    return kuaishouPattern.test(input)
  },
  async resolve(_input: string, _ctx: ResolveContext): Promise<MediaInfo> {
    throw new Error('kuaishou resolver is not implemented yet (planned M2)')
  },
  async createDownloadPlan(_info: MediaInfo, _options: DownloadOptions): Promise<DownloadPlan> {
    throw new Error('kuaishou download plan is not implemented yet (planned M2)')
  },
}
