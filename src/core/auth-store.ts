import { join } from 'node:path'
import { getConfigDir, readJsonFile, writeJsonFile } from './config.js'

export type BilibiliAuthState = {
  sessData: string
  cookieHeader?: string
  updatedAt: string
}

export type AuthState = {
  bilibili?: BilibiliAuthState
}

export function getAuthPath(configDir = getConfigDir()): string {
  return join(configDir, 'auth.json')
}

export async function loadAuth(configDir = getConfigDir()): Promise<AuthState> {
  return readJsonFile<AuthState>(getAuthPath(configDir), {})
}

export async function saveAuth(auth: AuthState, configDir = getConfigDir()): Promise<void> {
  await writeJsonFile(getAuthPath(configDir), auth)
}

export async function setBilibiliAuth(state: { sessData: string; cookieHeader?: string }, configDir = getConfigDir()): Promise<void> {
  const auth = await loadAuth(configDir)
  auth.bilibili = { sessData: state.sessData, cookieHeader: state.cookieHeader, updatedAt: new Date().toISOString() }
  await saveAuth(auth, configDir)
}

export async function setBilibiliSessData(sessData: string, configDir = getConfigDir()): Promise<void> {
  await setBilibiliAuth({ sessData }, configDir)
}

export async function clearBilibiliAuth(configDir = getConfigDir()): Promise<void> {
  const auth = await loadAuth(configDir)
  delete auth.bilibili
  await saveAuth(auth, configDir)
}
