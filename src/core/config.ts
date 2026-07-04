import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'

export function getConfigDir(env: NodeJS.ProcessEnv = process.env): string {
  if (env.EASYDL_CONFIG_DIR?.trim()) return env.EASYDL_CONFIG_DIR.trim()
  if (env.XDG_CONFIG_HOME?.trim()) return join(env.XDG_CONFIG_HOME.trim(), 'easydl')
  return join(homedir(), '.config', 'easydl')
}

export function getConfigPath(configDir = getConfigDir()): string {
  return join(configDir, 'config.json')
}

export async function readJsonFile<T>(path: string, fallback: T): Promise<T> {
  try {
    const data = await readFile(path, 'utf8')
    return JSON.parse(data) as T
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === 'ENOENT') return fallback
    throw err
  }
}

export async function writeJsonFile(path: string, value: unknown): Promise<void> {
  await mkdir(dirname(path), { recursive: true, mode: 0o700 })
  await writeFile(path, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 })
}

export type EasydlConfig = {
  outputDir?: string
  ffmpeg?: string
  concurrency?: number
  chunkConcurrency?: number
}

export async function loadConfig(configDir = getConfigDir()): Promise<EasydlConfig> {
  return readJsonFile<EasydlConfig>(getConfigPath(configDir), {})
}

export async function saveConfig(config: EasydlConfig, configDir = getConfigDir()): Promise<void> {
  await writeJsonFile(getConfigPath(configDir), config)
}

export async function setConfigValue(key: keyof EasydlConfig, value: string, configDir = getConfigDir()): Promise<EasydlConfig> {
  const config = await loadConfig(configDir)
  if (key === 'concurrency' || key === 'chunkConcurrency') {
    const parsed = Number.parseInt(value, 10)
    if (!Number.isFinite(parsed) || parsed <= 0) throw new Error(`${key} must be a positive integer`)
    config[key] = parsed
  } else {
    config[key] = value
  }
  await saveConfig(config, configDir)
  return config
}
