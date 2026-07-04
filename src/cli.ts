#!/usr/bin/env bun
import { readFile } from 'node:fs/promises'
import { Command } from 'commander'
import { z } from 'zod'
import { clearBilibiliAuth, loadAuth } from './core/auth-store.js'
import { getConfigDir, loadConfig, saveConfig, setConfigValue } from './core/config.js'
import { resolvePlatform, type PlatformId } from './core/registry.js'
import { runDownloadPlan, summarizeDownloadEvents } from './core/downloader.js'
import { DownloadPlanSchema, type DownloadOptions, type ResolveContext } from './core/types.js'

const program = new Command()

program
  .name('easydl')
  .description('Agent-friendly downloader CLI for Xiaohongshu, Douyin, Kuaishou, and Bilibili')
  .version('0.1.0')

function commonOptions(cmd: Command): Command {
  return cmd
    .option('--platform <platform>', 'auto|douyin|xhs|bilibili|kuaishou', 'auto')
    .option('--cookies <path>', 'Netscape cookies.txt path')
    .option('--proxy <url>', 'HTTP/SOCKS proxy URL')
    .option('--json', 'print final JSON to stdout')
}

function downloadOptions(cmd: Command): Command {
  return commonOptions(cmd)
    .option('--plan <path>', 'hidden/debug: run an existing DownloadPlan JSON')
    .option('--quality <quality>', 'quality selector', 'best')
    .option('--part <n>', '1-based part index')
    .option('--all-parts', 'download all parts')
    .option('--images <selection>', 'image selection, e.g. 1,3,5 or 1-5')
    .option('--concurrency <n>', 'item concurrency', parsePositiveInt)
    .option('--chunk-concurrency <n>', 'multipart chunk concurrency', parsePositiveInt)
    .option('--output-dir <dir>', 'output directory')
    .option('--overwrite', 'overwrite existing files')
    .option('--no-overwrite', 'skip existing files')
    .option('--json-progress', 'print NDJSON progress to stdout')
}

function parsePositiveInt(value: string): number {
  const n = Number.parseInt(value, 10)
  if (!Number.isFinite(n) || n <= 0) throw new Error(`expected positive integer, got ${value}`)
  return n
}

function parsePlatform(value: string): PlatformId {
  const parsed = z.enum(['auto', 'douyin', 'xhs', 'bilibili', 'kuaishou']).safeParse(value)
  if (!parsed.success) throw new Error(`unsupported platform: ${value}`)
  return parsed.data
}

function printJson(value: unknown): void {
  process.stdout.write(`${JSON.stringify(value, null, 2)}\n`)
}

function printError(err: unknown): void {
  const msg = err instanceof Error ? err.message : String(err)
  process.stderr.write(`Error: ${msg}\n`)
}

function parseImageSelection(value?: string): number[] | undefined {
  if (!value?.trim()) return undefined
  const out = new Set<number>()
  for (const part of value.split(',')) {
    const trimmed = part.trim()
    if (!trimmed) continue
    const range = trimmed.match(/^(\d+)\s*-\s*(\d+)$/)
    if (range) {
      const start = Number.parseInt(range[1], 10)
      const end = Number.parseInt(range[2], 10)
      if (start <= 0 || end <= 0 || end < start) throw new Error(`invalid image range: ${trimmed}`)
      for (let i = start; i <= end; i++) out.add(i - 1)
      continue
    }
    const n = Number.parseInt(trimmed, 10)
    if (!Number.isFinite(n) || n <= 0) throw new Error(`invalid image index: ${trimmed}`)
    out.add(n - 1)
  }
  return [...out].sort((a, b) => a - b)
}

async function buildResolveContext(opts: { cookies?: string; proxy?: string }): Promise<ResolveContext> {
  return {
    cookiesPath: opts.cookies,
    proxy: opts.proxy,
    configDir: getConfigDir(),
  }
}

async function handleInfo(input: string, opts: { platform: string; cookies?: string; proxy?: string; json?: boolean }) {
  const platform = resolvePlatform(input, parsePlatform(opts.platform))
  const info = await platform.resolve(input, await buildResolveContext(opts))
  if (opts.json) return printJson(info)
  process.stdout.write(`${platform.displayName}: ${info.title}\n`)
}

program.addCommand(
  commonOptions(new Command('info'))
    .description('resolve metadata for a link/share text')
    .argument('<input>', 'link or share text')
    .action(async (input: string, opts) => {
      try {
        await handleInfo(input, opts)
      } catch (err) {
        printError(err)
        process.exitCode = 1
      }
    }),
)

program.addCommand(
  commonOptions(new Command('formats'))
    .description('list available formats/parts/images for a link/share text')
    .argument('<input>', 'link or share text')
    .action(async (input: string, opts) => {
      try {
        const platform = resolvePlatform(input, parsePlatform(opts.platform))
        const info = await platform.resolve(input, await buildResolveContext(opts))
        const result = {
          platform: info.platform,
          id: info.id,
          title: info.title,
          streams: info.streams ?? [],
          parts: info.parts ?? [],
          images: info.images ?? [],
        }
        if (opts.json) return printJson(result)
        process.stdout.write(`${info.title}\n`)
        for (const stream of result.streams) process.stdout.write(`- ${stream.quality}${stream.qualityName ? ` (${stream.qualityName})` : ''}\n`)
        if (result.parts.length) process.stdout.write(`parts: ${result.parts.length}\n`)
        if (result.images.length) process.stdout.write(`images: ${result.images.length}\n`)
      } catch (err) {
        printError(err)
        process.exitCode = 1
      }
    }),
)

program.addCommand(
  downloadOptions(new Command('download'))
    .description('download a link/share text')
    .argument('[input]', 'link or share text')
    .action(async (input: string | undefined, opts) => {
      try {
        if (opts.plan) {
          const planJson = JSON.parse(await readFile(opts.plan, 'utf8'))
          const plan = DownloadPlanSchema.parse(planJson)
          const events = await runDownloadPlan(plan, { jsonProgress: Boolean(opts.jsonProgress) })
          const summary = summarizeDownloadEvents(events)
          if (opts.json || !opts.jsonProgress) printJson(summary)
          if (!summary.success) process.exitCode = 1
          return
        }

        if (!input) throw new Error('missing input (or use --plan plan.json)')
        const config = await loadConfig()
        const platform = resolvePlatform(input, parsePlatform(opts.platform))
        const info = await platform.resolve(input, await buildResolveContext(opts))
        const downloadOptions: DownloadOptions = {
          quality: opts.quality,
          part: opts.part ? parsePositiveInt(opts.part) : undefined,
          allParts: Boolean(opts.allParts),
          images: parseImageSelection(opts.images),
          outputDir: opts.outputDir ?? config.outputDir,
          concurrency: opts.concurrency ?? config.concurrency,
          chunkConcurrency: opts.chunkConcurrency ?? config.chunkConcurrency,
          overwrite: Boolean(opts.overwrite),
          noOverwrite: Boolean(opts.noOverwrite),
          cookiesPath: opts.cookies,
          proxy: opts.proxy,
        }
        const plan = await platform.createDownloadPlan(info, downloadOptions)
        const events = await runDownloadPlan(plan, { jsonProgress: Boolean(opts.jsonProgress) })
        const summary = summarizeDownloadEvents(events)
        if (opts.json || !opts.jsonProgress) printJson(summary)
        if (!summary.success) process.exitCode = 1
      } catch (err) {
        printError(err)
        process.exitCode = 1
      }
    }),
)

const auth = new Command('auth').description('manage platform authentication')
const bilibili = new Command('bilibili').description('Bilibili authentication')
const login = new Command('login').description('Bilibili QR login')
login
  .command('create')
  .option('--json', 'print JSON')
  .description('create a QR login session')
  .action(() => {
    printError('bilibili QR login create is not implemented yet (planned M4a)')
    process.exitCode = 1
  })
login
  .command('poll')
  .argument('<sessionId>')
  .option('--json', 'print JSON')
  .description('poll a QR login session')
  .action(() => {
    printError('bilibili QR login poll is not implemented yet (planned M4a)')
    process.exitCode = 1
  })
login.option('--json', 'print JSON').action(() => {
  printError('bilibili blocking QR login is not implemented yet (planned M4a)')
  process.exitCode = 1
})
bilibili.addCommand(login)
bilibili.command('status').option('--json', 'print JSON').action(async (opts) => {
  try {
    const auth = await loadAuth()
    const result = { platform: 'bilibili', loggedIn: Boolean(auth.bilibili?.sessData), updatedAt: auth.bilibili?.updatedAt }
    if (opts.json) return printJson(result)
    process.stdout.write(result.loggedIn ? `Bilibili logged in (${result.updatedAt})\n` : 'Bilibili not logged in\n')
  } catch (err) {
    printError(err)
    process.exitCode = 1
  }
})
bilibili.command('logout').action(async () => {
  try {
    await clearBilibiliAuth()
    process.stdout.write('Bilibili auth cleared\n')
  } catch (err) {
    printError(err)
    process.exitCode = 1
  }
})
auth.addCommand(bilibili)
program.addCommand(auth)

const configCmd = new Command('config').description('manage easydl config')
configCmd.command('get').option('--json', 'print JSON').action(async (opts) => {
  try {
    const config = await loadConfig()
    if (opts.json) return printJson(config)
    for (const [key, value] of Object.entries(config)) process.stdout.write(`${key}=${value}\n`)
  } catch (err) {
    printError(err)
    process.exitCode = 1
  }
})
configCmd.command('set').argument('<key>').argument('<value>').action(async (key: string, value: string) => {
  try {
    if (!['outputDir', 'ffmpeg', 'concurrency', 'chunkConcurrency'].includes(key)) throw new Error(`unsupported config key: ${key}`)
    const config = await setConfigValue(key as 'outputDir' | 'ffmpeg' | 'concurrency' | 'chunkConcurrency', value)
    printJson(config)
  } catch (err) {
    printError(err)
    process.exitCode = 1
  }
})
program.addCommand(configCmd)

await program.parseAsync(process.argv)
