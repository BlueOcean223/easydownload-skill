import { readFile } from 'node:fs/promises'

export async function loadCookieHeader(cookiesPath: string | undefined, targetUrl: string): Promise<string | undefined> {
  if (!cookiesPath?.trim()) return undefined
  const text = await readFile(cookiesPath, 'utf8')
  const rawHeader = parseRawCookieHeader(text)
  if (rawHeader) return rawHeader

  const url = new URL(targetUrl)
  const pairs: string[] = []
  const nowSeconds = Math.floor(Date.now() / 1000)

  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim()
    if (!trimmed || (trimmed.startsWith('#') && !trimmed.startsWith('#HttpOnly_'))) continue

    const fields = trimmed.split('\t')
    if (fields.length < 7) continue

    const domain = fields[0].replace(/^#HttpOnly_/i, '')
    const includeSubdomains = /^true$/i.test(fields[1]) || domain.startsWith('.')
    const cookiePath = fields[2] || '/'
    const secure = /^true$/i.test(fields[3])
    const expiry = Number.parseInt(fields[4], 10)
    const name = fields[5]
    const value = fields.slice(6).join('\t')

    if (!name) continue
    if (secure && url.protocol !== 'https:') continue
    if (Number.isFinite(expiry) && expiry > 0 && expiry < nowSeconds) continue
    if (!domainMatches(url.hostname, domain, includeSubdomains)) continue
    if (!url.pathname.startsWith(cookiePath)) continue
    pairs.push(`${name}=${value}`)
  }

  return pairs.length ? pairs.join('; ') : undefined
}

function parseRawCookieHeader(text: string): string | undefined {
  const lines = text.split(/\r?\n/).map((line) => line.trim()).filter(Boolean)
  if (lines.length !== 1) return undefined
  const line = lines[0]
  if (line.startsWith('#') || line.includes('\t')) return undefined
  return /^[^=;\s]+=[^;]*(?:;\s*[^=;\s]+=[^;]*)*$/.test(line) ? line : undefined
}

function domainMatches(hostname: string, domain: string, includeSubdomains: boolean): boolean {
  const host = hostname.toLowerCase().replace(/\.$/, '')
  const normalized = domain.toLowerCase().replace(/^\./, '').replace(/\.$/, '')
  if (!normalized) return false
  if (host === normalized) return true
  return includeSubdomains && host.endsWith(`.${normalized}`)
}
