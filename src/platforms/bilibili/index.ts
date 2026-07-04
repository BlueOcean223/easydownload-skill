import type { DownloadOptions, DownloadPlan, MediaInfo, Platform, ResolveContext } from '../../core/types.js'

const bilibiliPattern = /(?:^|\/\/)(?:[^/]+\.)?(?:bilibili\.com|b23\.tv)(?:[/:?#]|$)|\b(?:BV[0-9A-Za-z]+|av\d+|ep\d+|ss\d+|md\d+)\b/i

export const bilibiliPlatform: Platform = {
  id: 'bilibili',
  displayName: 'B站',
  match(input: string): boolean {
    return bilibiliPattern.test(input)
  },
  async resolve(_input: string, _ctx: ResolveContext): Promise<MediaInfo> {
    throw new Error('bilibili resolver is not implemented yet (planned M4a/M4b)')
  },
  async createDownloadPlan(_info: MediaInfo, _options: DownloadOptions): Promise<DownloadPlan> {
    throw new Error('bilibili download plan is not implemented yet (planned M4a/M4b)')
  },
}
