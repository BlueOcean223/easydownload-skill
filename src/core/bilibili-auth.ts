import { mkdir } from 'node:fs/promises'
import { join } from 'node:path'
import QRCode from 'qrcode'
import { getConfigDir } from './config.js'
import { setBilibiliSessData } from './auth-store.js'

const requestTimeoutMs = 30_000
const defaultUserAgent = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36'

type QRGenerateResponse = {
  code: number
  message?: string
  data?: {
    url?: string
    qrcode_key?: string
  }
}

type QRPollResponse = {
  code: number
  message?: string
  data?: {
    code?: number
    message?: string
    url?: string
    refresh_token?: string
    timestamp?: number
  }
}

export type BilibiliQRSession = {
  sessionId: string
  qrcodeKey: string
  url: string
  qrPath: string
  terminal?: string
}

export type BilibiliQRPollResult = {
  sessionId: string
  code: number
  message: string
  loggedIn: boolean
  sessData?: string
}

export async function createBilibiliQRSession(options: { configDir?: string; terminal?: boolean } = {}): Promise<BilibiliQRSession> {
  const response = await fetchJson<QRGenerateResponse>('https://passport.bilibili.com/x/passport-login/web/qrcode/generate')
  if (response.code !== 0 || !response.data?.url || !response.data.qrcode_key) {
    throw new Error(`bilibili QR create failed: ${response.message ?? response.code}`)
  }

  const configDir = options.configDir ?? getConfigDir()
  const qrDir = join(configDir, 'qrcode')
  await mkdir(qrDir, { recursive: true, mode: 0o700 })
  const qrPath = join(qrDir, `${response.data.qrcode_key}.png`)
  await QRCode.toFile(qrPath, response.data.url, { width: 320, margin: 2 })

  return {
    sessionId: response.data.qrcode_key,
    qrcodeKey: response.data.qrcode_key,
    url: response.data.url,
    qrPath,
    terminal: options.terminal ? await QRCode.toString(response.data.url, { type: 'terminal' }) : undefined,
  }
}

export async function pollBilibiliQRSession(sessionId: string, options: { configDir?: string } = {}): Promise<BilibiliQRPollResult> {
  const qrcodeKey = sessionId.trim()
  if (!qrcodeKey) throw new Error('empty bilibili QR sessionId')

  const response = await fetchWithTimeout(`https://passport.bilibili.com/x/passport-login/web/qrcode/poll?qrcode_key=${encodeURIComponent(qrcodeKey)}`)
  const setCookie = response.headers.get('set-cookie') ?? ''
  const payload = (await response.json()) as QRPollResponse
  if (payload.code !== 0) throw new Error(`bilibili QR poll failed: ${payload.message ?? payload.code}`)

  const code = payload.data?.code ?? -1
  const message = payload.data?.message || messageForQRStatus(code)
  const sessData = extractSessData(setCookie) ?? extractSessData(payload.data?.url ?? '')
  if (code === 0) {
    if (!sessData) throw new Error('bilibili QR login succeeded but SESSDATA was not found')
    await setBilibiliSessData(sessData, options.configDir ?? getConfigDir())
  }

  return {
    sessionId: qrcodeKey,
    code,
    message,
    loggedIn: code === 0,
    sessData: code === 0 ? sessData : undefined,
  }
}

export async function waitForBilibiliQRLogin(sessionId: string, options: { configDir?: string; intervalMs?: number; timeoutMs?: number } = {}): Promise<BilibiliQRPollResult> {
  const started = Date.now()
  const intervalMs = options.intervalMs ?? 2_000
  const timeoutMs = options.timeoutMs ?? 180_000

  while (true) {
    const result = await pollBilibiliQRSession(sessionId, { configDir: options.configDir })
    if (result.loggedIn) return result
    if (result.code === 86038) throw new Error('bilibili QR code expired')
    if (Date.now() - started > timeoutMs) throw new Error('bilibili QR login timed out')
    await sleep(intervalMs)
  }
}

async function fetchJson<T>(url: string): Promise<T> {
  const response = await fetchWithTimeout(url)
  if (!response.ok) throw new Error(`bilibili auth HTTP ${response.status}`)
  return (await response.json()) as T
}

async function fetchWithTimeout(url: string): Promise<Response> {
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(), requestTimeoutMs)
  try {
    return await fetch(url, { signal: controller.signal, headers: { 'User-Agent': defaultUserAgent, Referer: 'https://passport.bilibili.com/' } })
  } catch (err) {
    if (err instanceof Error && err.name === 'AbortError') throw new Error(`bilibili auth request timed out after ${requestTimeoutMs}ms`)
    throw err
  } finally {
    clearTimeout(timer)
  }
}

function extractSessData(value: string): string | undefined {
  const match = value.match(/(?:^|[?&;\s])SESSDATA=([^;&\s]+)/)
  return match?.[1] ? decodeURIComponent(match[1]) : undefined
}

function messageForQRStatus(code: number): string {
  switch (code) {
    case 0:
      return 'login successful'
    case 86038:
      return 'QR code expired'
    case 86090:
      return 'QR code scanned; waiting for confirmation'
    case 86101:
      return 'QR code not scanned'
    default:
      return `QR status ${code}`
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms))
}
