import { expect, test } from 'bun:test'
import { DownloadPlanSchema } from '../src/core/types.js'

test('DownloadPlan schema accepts a basic file plan', () => {
  const plan = DownloadPlanSchema.parse({
    outputDir: './downloads',
    items: [{ id: 'one', kind: 'file', url: 'https://example.com/a.mp4', filename: 'a.mp4' }],
  })
  expect(plan.concurrency).toBe(4)
  expect(plan.chunkConcurrency).toBe(4)
})
