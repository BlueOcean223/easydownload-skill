import { afterEach, expect, test } from 'bun:test'
import { mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { loadCookieHeader } from '../src/core/cookies.js'

const cleanups: Array<() => Promise<void>> = []

afterEach(async () => {
  while (cleanups.length > 0) await cleanups.pop()?.()
})

test('uses a raw Cookie header file as-is', async () => {
  const file = await tempFile('a=1; b=2')
  expect(await loadCookieHeader(file, 'https://www.example.com/')).toBe('a=1; b=2')
})

test('filters Netscape cookies by domain, path, secure, and expiry', async () => {
  const future = Math.floor(Date.now() / 1000) + 3600
  const past = Math.floor(Date.now() / 1000) - 3600
  const file = await tempFile([
    '# Netscape HTTP Cookie File',
    `.example.com\tTRUE\t/\tTRUE\t${future}\tsid\tok`,
    `.example.com\tTRUE\t/private\tTRUE\t${future}\tprivate\tno`,
    `.example.com\tTRUE\t/\tTRUE\t${past}\told\tno`,
    `.other.com\tTRUE\t/\tTRUE\t${future}\tother\tno`,
  ].join('\n'))

  expect(await loadCookieHeader(file, 'https://www.example.com/path')).toBe('sid=ok')
})

async function tempFile(content: string): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), 'easydl-cookies-test-'))
  cleanups.push(() => rm(dir, { recursive: true, force: true }))
  const file = join(dir, 'cookies.txt')
  await writeFile(file, content)
  return file
}
