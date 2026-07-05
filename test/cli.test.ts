import { expect, test } from 'bun:test'
import { spawn } from 'node:child_process'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

test('download defaults to human summary and rejects mixed JSON modes', async () => {
  const tmp = await mkdtemp(join(tmpdir(), 'easydl-cli-test-'))
  try {
    const planPath = join(tmp, 'plan.json')
    await writeFile(planPath, JSON.stringify({
      outputDir: tmp,
      items: [{ id: 'hello', kind: 'file', filename: 'hello.txt', url: 'data:text/plain,hello' }],
    }))

    const human = await runCli(['download', '--plan', planPath])
    expect(human.code).toBe(0)
    expect(human.stdout).toContain('Download completed')
    expect(human.stdout).toContain('artifact:')
    expect(await readFile(join(tmp, 'hello.txt'), 'utf8')).toBe('hello')

    const skipped = await runCli(['download', '--plan', planPath, '--no-overwrite', '--json'])
    expect(skipped.code).toBe(0)
    const skippedSummary = JSON.parse(skipped.stdout)
    expect(skippedSummary.artifacts).toEqual([])
    expect(skippedSummary.skipped[0]).toMatchObject({ taskId: 'hello', reason: 'file exists' })

    const conflict = await runCli(['download', '--json', '--json-progress', '--plan', planPath])
    expect(conflict.code).toBe(1)
    expect(conflict.stdout).toBe('')
    expect(conflict.stderr).toContain('--json and --json-progress cannot be used together')
  } finally {
    await rm(tmp, { recursive: true, force: true })
  }
})

test('help documents auth logout and logout clears stored bilibili auth', async () => {
  const help = await runCli(['--help'])
  expect(help.code).toBe(0)
  expect(help.stdout).toContain('Examples:')
  expect(help.stdout).toContain('easydl auth bilibili logout')

  const authHelp = await runCli(['auth', 'bilibili', '--help'])
  expect(authHelp.code).toBe(0)
  expect(authHelp.stdout).toContain('logout')
  expect(authHelp.stdout).toContain('clear stored Bilibili login state')

  const tmp = await mkdtemp(join(tmpdir(), 'easydl-auth-test-'))
  try {
    await writeFile(join(tmp, 'auth.json'), JSON.stringify({ bilibili: { sessData: 'sess', cookieHeader: 'SESSDATA=sess', updatedAt: '2026-01-01T00:00:00.000Z' } }))
    const logout = await runCli(['auth', 'bilibili', 'logout', '--json'], { EASYDL_CONFIG_DIR: tmp })
    expect(logout.code).toBe(0)
    expect(JSON.parse(logout.stdout)).toEqual({ platform: 'bilibili', loggedIn: false, cleared: true })
    expect(JSON.parse(await readFile(join(tmp, 'auth.json'), 'utf8'))).toEqual({})

    const authLogoutAlias = await runCli(['auth', 'logout', 'bilibili', '--json'], { EASYDL_CONFIG_DIR: tmp })
    expect(authLogoutAlias.code).toBe(0)
    expect(JSON.parse(authLogoutAlias.stdout)).toEqual({ platform: 'bilibili', loggedIn: false, cleared: false })
  } finally {
    await rm(tmp, { recursive: true, force: true })
  }
})

function runCli(args: string[], env?: NodeJS.ProcessEnv): Promise<{ code: number | null; stdout: string; stderr: string }> {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ['run', 'src/cli.ts', ...args], { cwd: process.cwd(), env: { ...process.env, ...env }, stdio: ['ignore', 'pipe', 'pipe'] })
    let stdout = ''
    let stderr = ''
    child.stdout.on('data', (chunk) => { stdout += chunk.toString() })
    child.stderr.on('data', (chunk) => { stderr += chunk.toString() })
    child.once('error', reject)
    child.once('exit', (code) => resolve({ code, stdout, stderr }))
  })
}
