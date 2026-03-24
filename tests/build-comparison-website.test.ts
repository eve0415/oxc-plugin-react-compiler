import { access, rm } from 'node:fs/promises'
import { constants } from 'node:fs'
import { join } from 'node:path'

import { build } from 'vite-plus'
import { describe, expect, it } from 'vite-plus/test'

import { collectJsFiles, compareExactJsOutputs, logExactMismatchSummary } from './utils/build-compare.js'

const websiteDir = '/tmp/website'

const exists = async (path: string): Promise<boolean> => {
  try {
    await access(path, constants.F_OK)
    return true
  } catch {
    return false
  }
}

const buildWebsite = async (configFile: string): Promise<number> => {
  const start = performance.now()
  await build({ configFile: join(websiteDir, configFile) })
  return performance.now() - start
}

describe('build comparison: eve0415/website', { timeout: 180_000 }, () => {
  it('Babel and OXC test configs produce identical JS chunks when the fixture is present', async () => {
    if (!(await exists(websiteDir)) || !(await exists(join(websiteDir, 'node_modules')))) {
      console.log('\n  /tmp/website not present; skipping real-world build parity check')
      return
    }

    await rm(join(websiteDir, 'dist-oxc'), { recursive: true, force: true })
    await rm(join(websiteDir, 'dist-babel'), { recursive: true, force: true })

    const [oxcMs, babelMs] = await Promise.all([
      buildWebsite('vite.config.oxc-test.ts'),
      buildWebsite('vite.config.babel-test.ts'),
    ])

    console.log(
      `\n  website build timings:\n` +
      `    OXC:   ${oxcMs.toFixed(0)}ms\n` +
      `    Babel: ${babelMs.toFixed(0)}ms\n` +
      `    Speedup: ${(babelMs / oxcMs).toFixed(2)}x\n`,
    )

    const oxcDir = join(websiteDir, 'dist-oxc')
    const babelDir = join(websiteDir, 'dist-babel')

    const [oxcFiles, babelFiles] = await Promise.all([
      collectJsFiles(oxcDir),
      collectJsFiles(babelDir),
    ])

    expect(oxcFiles.length).toBeGreaterThan(0)
    expect(babelFiles.length).toBeGreaterThan(0)

    const mismatches = await compareExactJsOutputs(babelDir, oxcDir)
    logExactMismatchSummary('website', mismatches)
    expect(mismatches).toHaveLength(0)
  })
})
