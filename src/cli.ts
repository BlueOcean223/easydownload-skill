#!/usr/bin/env bun
import { readFile } from 'node:fs/promises'
import { Command } from 'commander'
import { clearBilibiliAuth, loadAuth } from './core/auth-store.js'
import { createBilibiliQRSession, pollBilibiliQRSession, waitForBilibiliQRLogin } from './core/bilibili-auth.js'
import { getConfigDir, loadConfig, setConfigValue } from './core/config.js'
import { resolvePlatform } from './core/registry.js'
import { runDownloadPlan, summarizeDownloadEvents } from './core/downloader.js'
import { DownloadPlanSchema, type DownloadOptions, type ResolveContext } from './core/types.js'

const program = new Command()

program
  .name('easydl')
  .description('Agent-friendly downloader CLI for Xiaohongshu, Douyin, Kuaishou, and Bilibili')
  .version('0.1.0')
  .showHelpAfterError()
  .addHelpText('after', `
Examples:
  easydl download '<url or share text>'
  easydl download --json-progress '<url or share text>'
  easydl formats --json '<url>'
  easydl auth bilibili login
  easydl auth bilibili status --json
  easydl auth bilibili logout
  easydl config set ffmpeg /opt/homebrew/bin/ffmpeg
`)

function commonOptions(cmd: Command): Command {
  return cmd
    .option('--cookies <path>', 'Netscape cookies.txt path')
    .option('--proxy <url>', 'HTTP/SOCKS proxy URL')
    .option('--json', 'print final JSON to stdout')
}

function downloadOptions(cmd: Command): Command {
  return commonOptions(cmd)
    .option('--plan <path>', 'run a DownloadPlan JSON file; use - to read JSON from stdin')
    .option('--quality <quality>', 'quality selector', 'best')
    .option('--part <n>', '1-based part index')
    .option('--all-parts', 'download all parts')
    .option('--images <selection>', 'image selection, e.g. 1,3,5 or 1-5')
    .option('--concurrency <n>', 'item concurrency', parsePositiveInt)
    .option('--chunk-concurrency <n>', 'multipart chunk concurrency', parsePositiveInt)
    .option('--output-dir <dir>', 'output directory')
    .option('--ffmpeg <path>', 'ffmpeg executable path')
    .option('--overwrite', 'overwrite existing files')
    .option('--no-overwrite', 'skip existing files')
    .option('--json-progress', 'print NDJSON progress to stdout')
}

function parsePositiveInt(value: string): number {
  const n = Number.parseInt(value, 10)
  if (!Number.isFinite(n) || n <= 0) throw new Error(`expected positive integer, got ${value}`)
  return n
}

function printJson(value: unknown): void {
  process.stdout.write(`${JSON.stringify(value, null, 2)}\n`)
}

async function readPlanText(path: string): Promise<string> {
  if (path !== '-') return readFile(path, 'utf8')
  const chunks: Buffer[] = []
  for await (const chunk of process.stdin) chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk))
  return Buffer.concat(chunks).toString('utf8')
}

function optionValue<T = unknown>(opts: unknown, key: string): T | undefined {
  if (!opts || typeof opts !== 'object') return undefined
  const record = opts as Record<string, unknown> & { opts?: () => Record<string, unknown>; getOptionValue?: (name: string) => unknown }
  if (key in record) return record[key] as T
  if (typeof record.getOptionValue === 'function') return record.getOptionValue(key) as T | undefined
  if (typeof record.opts === 'function') return record.opts()[key] as T | undefined
  return undefined
}

function optionEnabled(opts: unknown, key: string): boolean {
  return Boolean(optionValue(opts, key))
}

function printError(err: unknown): void {
  const msg = err instanceof Error ? err.message : String(err)
  process.stderr.write(`Error: ${msg}\n`)
}

function printDownloadSummary(summary: ReturnType<typeof summarizeDownloadEvents>): void {
  process.stdout.write(summary.success ? 'Download completed\n' : 'Download completed with failures\n')
  for (const artifact of summary.artifacts) process.stdout.write(`artifact: ${artifact.path}${artifact.partial ? ' (partial)' : ''}\n`)
  for (const skipped of summary.skipped) process.stdout.write(`skipped: ${skipped.path} (${skipped.reason})\n`)
  for (const failure of summary.failures) process.stdout.write(`failure: ${failure.taskId}: ${failure.error}\n`)
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

async function handleInfo(input: string, opts: { cookies?: string; proxy?: string; json?: boolean }) {
  const platform = resolvePlatform(input)
  const info = await platform.resolve(input, await buildResolveContext(opts))
  if (opts.json) return printJson(info)
  process.stdout.write(`${platform.displayName}: ${info.title}\n`)
}

program.addCommand(
  commonOptions(new Command('info'))
    .description('resolve metadata for a link/share text')
    .argument('<input>', 'link or share text')
    .addHelpText('after', `
Examples:
  easydl info '<url or share text>'
  easydl info --json '<url>'
`)
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
    .addHelpText('after', `
Examples:
  easydl formats '<url>'
  easydl formats --json '<url>'
`)
    .action(async (input: string, opts) => {
      try {
        const platform = resolvePlatform(input)
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

async function handleBilibiliLogout(json: boolean): Promise<void> {
  const before = await loadAuth()
  await clearBilibiliAuth()
  const result = { platform: 'bilibili', loggedIn: false, cleared: Boolean(before.bilibili?.sessData || before.bilibili?.cookieHeader) }
  if (json) return printJson(result)
  process.stdout.write(result.cleared ? 'Bilibili auth cleared\n' : 'Bilibili was already logged out\n')
}

program.addCommand(
  downloadOptions(new Command('download'))
    .description('download a link/share text')
    .argument('[input]', 'link or share text')
    .addHelpText('after', `
Examples:
  easydl download '<url or share text>'
  easydl download --json '<url>'
  easydl download --json-progress '<url>'
  easydl download --plan plan.json --json-progress
  cat plan.json | easydl download --plan - --json-progress
  easydl download --quality 1080p --part 2 '<url>'
  easydl download --all-parts '<multi-part url>'
  easydl download --images 1,3-5 '<album url>'
`)
    .action(async (input: string | undefined, opts) => {
      try {
        if (opts.json && opts.jsonProgress) throw new Error('--json and --json-progress cannot be used together')
        if (opts.plan) {
          if (input) throw new Error('--plan cannot be combined with input URL/share text')
          const planJson = JSON.parse(await readPlanText(opts.plan))
          const parsedPlan = DownloadPlanSchema.parse(planJson)
          const plan = {
            ...parsedPlan,
            ...(opts.outputDir ? { outputDir: opts.outputDir } : {}),
            ...(opts.concurrency ? { concurrency: opts.concurrency } : {}),
            ...(opts.chunkConcurrency ? { chunkConcurrency: opts.chunkConcurrency } : {}),
            ...(opts.ffmpeg ? { ffmpeg: opts.ffmpeg } : {}),
            ...(opts.proxy ? { proxy: opts.proxy } : {}),
            ...(opts.overwrite ? { overwrite: true, noOverwrite: false } : {}),
            ...((Boolean(opts.noOverwrite) || opts.overwrite === false) ? { noOverwrite: true, overwrite: false } : {}),
          }
          const events = await runDownloadPlan(plan, { jsonProgress: Boolean(opts.jsonProgress) })
          const summary = summarizeDownloadEvents(events)
          if (opts.json) printJson(summary)
          else if (!opts.jsonProgress) printDownloadSummary(summary)
          if (!summary.success) process.exitCode = 1
          return
        }

        if (!input) throw new Error('missing input (or use --plan plan.json)')
        const config = await loadConfig()
        const platform = resolvePlatform(input)
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
          noOverwrite: Boolean(opts.noOverwrite) || opts.overwrite === false,
          cookiesPath: opts.cookies,
          proxy: opts.proxy,
        }
        const ffmpeg = opts.ffmpeg ?? config.ffmpeg
        const createdPlan = await platform.createDownloadPlan(info, downloadOptions)
        const plan = ffmpeg ? { ...createdPlan, ffmpeg } : createdPlan
        const events = await runDownloadPlan(plan, { jsonProgress: Boolean(opts.jsonProgress) })
        const summary = summarizeDownloadEvents(events)
        if (opts.json) printJson(summary)
        else if (!opts.jsonProgress) printDownloadSummary(summary)
        if (!summary.success) process.exitCode = 1
      } catch (err) {
        printError(err)
        process.exitCode = 1
      }
    }),
)

const auth = new Command('auth')
  .description('manage platform authentication')
  .addHelpText('after', `
Examples:
  easydl auth bilibili login
  easydl auth bilibili login create --json
  easydl auth bilibili login poll <sessionId> --json
  easydl auth bilibili status --json
  easydl auth bilibili logout
  easydl auth logout bilibili --json
`)
const bilibili = new Command('bilibili')
  .description('Bilibili authentication')
  .addHelpText('after', `
Examples:
  easydl auth bilibili login
  easydl auth bilibili status --json
  easydl auth bilibili logout --json
`)
const login = new Command('login')
  .description('Bilibili QR login')
  .addHelpText('after', `
Examples:
  easydl auth bilibili login
  easydl auth bilibili login --json
  easydl auth bilibili login create --json
  easydl auth bilibili login poll <sessionId> --json
`)
const loginCreate = login.command('create').option('--json', 'print JSON').description('create a QR login session')
loginCreate.action(async () => {
  try {
    const json = optionEnabled(loginCreate, 'json') || optionEnabled(login, 'json')
    const session = await createBilibiliQRSession({ terminal: !json })
    if (json) return printJson(session)
    if (session.terminal) process.stdout.write(`${session.terminal}\n`)
    process.stdout.write(`URL: ${session.url}\nQR image: ${session.qrPath}\nSession: ${session.sessionId}\n`)
  } catch (err) {
    printError(err)
    process.exitCode = 1
  }
})
const loginPoll = login.command('poll').argument('<sessionId>').option('--json', 'print JSON').description('poll a QR login session')
loginPoll.action(async (sessionId: string) => {
  try {
    const json = optionEnabled(loginPoll, 'json') || optionEnabled(login, 'json')
    const result = await pollBilibiliQRSession(sessionId)
    if (json) return printJson(result)
    process.stdout.write(`${result.message}${result.loggedIn ? ' (logged in)' : ''}\n`)
  } catch (err) {
    printError(err)
    process.exitCode = 1
  }
})
login.option('--json', 'print JSON').action(async () => {
  try {
    const json = optionEnabled(login, 'json')
    const session = await createBilibiliQRSession({ terminal: !json })
    if (json) printJson(session)
    else {
      if (session.terminal) process.stdout.write(`${session.terminal}\n`)
      process.stdout.write(`URL: ${session.url}\nQR image: ${session.qrPath}\nWaiting for confirmation...\n`)
    }
    const result = await waitForBilibiliQRLogin(session.sessionId)
    if (json) printJson(result)
    else process.stdout.write('Bilibili login successful\n')
  } catch (err) {
    printError(err)
    process.exitCode = 1
  }
})
bilibili.addCommand(login)
const bilibiliStatus = bilibili.command('status').description('show stored Bilibili login state').option('--json', 'print JSON')
bilibiliStatus.action(async () => {
  try {
    const auth = await loadAuth()
    const result = { platform: 'bilibili', loggedIn: Boolean(auth.bilibili?.sessData), updatedAt: auth.bilibili?.updatedAt }
    if (optionEnabled(bilibiliStatus, 'json')) return printJson(result)
    process.stdout.write(result.loggedIn ? `Bilibili logged in (${result.updatedAt})\n` : 'Bilibili not logged in\n')
  } catch (err) {
    printError(err)
    process.exitCode = 1
  }
})
const bilibiliLogout = bilibili.command('logout').description('clear stored Bilibili login state').option('--json', 'print JSON')
bilibiliLogout.action(async () => {
  try {
    await handleBilibiliLogout(optionEnabled(bilibiliLogout, 'json'))
  } catch (err) {
    printError(err)
    process.exitCode = 1
  }
})
auth.addCommand(bilibili)
const authLogout = auth.command('logout').description('clear stored platform login state').argument('[platform]', 'platform to logout from (currently bilibili)', 'bilibili').option('--json', 'print JSON')
authLogout.action(async (platform: string) => {
  try {
    if (platform !== 'bilibili') throw new Error(`unsupported auth platform: ${platform}`)
    await handleBilibiliLogout(optionEnabled(authLogout, 'json'))
  } catch (err) {
    printError(err)
    process.exitCode = 1
  }
})
program.addCommand(auth)

const configCmd = new Command('config').description('manage easydl config')
configCmd.command('get').option('--json', 'print JSON').action(async (opts) => {
  try {
    const config = await loadConfig()
    if (optionEnabled(opts, 'json')) return printJson(config)
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
