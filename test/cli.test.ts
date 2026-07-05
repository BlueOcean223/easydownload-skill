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

    const conflict = await runCli(['download', '--json', '--json-progress', '--plan', planPath])
    expect(conflict.code).toBe(1)
    expect(conflict.stdout).toBe('')
    expect(conflict.stderr).toContain('--json and --json-progress cannot be used together')
  } finally {
    await rm(tmp, { recursive: true, force: true })
  }
})

function runCli(args: string[]): Promise<{ code: number | null; stdout: string; stderr: string }> {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ['run', 'src/cli.ts', ...args], { cwd: process.cwd(), stdio: ['ignore', 'pipe', 'pipe'] })
    let stdout = ''
    let stderr = ''
    child.stdout.on('data', (chunk) => { stdout += chunk.toString() })
    child.stderr.on('data', (chunk) => { stderr += chunk.toString() })
    child.once('error', reject)
    child.once('exit', (code) => resolve({ code, stdout, stderr }))
  })
}
