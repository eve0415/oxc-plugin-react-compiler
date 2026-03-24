import { describe, expect, it } from 'vite-plus/test'
import { build } from 'vite-plus'
import { rm } from 'node:fs/promises'
import { join } from 'node:path'
import { collectJsFiles, compareExactJsOutputs, logExactMismatchSummary } from './utils/build-compare.js'

const fixtureDir = join(import.meta.dirname, 'fixtures/shadcn-app')

const buildOnce = async (configFile: string): Promise<number> => {
  const start = performance.now()
  await build({ configFile: join(fixtureDir, configFile) })
  return performance.now() - start
}

describe('build comparison: shadcn-style app (~25 components)', { timeout: 120_000 }, () => {
  it('both OXC and Babel produce successful builds', async () => {
    await rm(join(fixtureDir, 'dist-oxc'), { recursive: true, force: true })
    await rm(join(fixtureDir, 'dist-babel'), { recursive: true, force: true })

    const [oxcMs, babelMs] = await Promise.all([
      buildOnce('vite.config.oxc.ts'),
      buildOnce('vite.config.babel.ts'),
    ])

    console.log(
      `\n  shadcn-app build timings (cold):\n` +
      `    OXC:     ${oxcMs.toFixed(0)}ms\n` +
      `    Babel:   ${babelMs.toFixed(0)}ms\n` +
      `    Speedup: ${(babelMs / oxcMs).toFixed(2)}x\n`,
    )

    const oxcFiles = await collectJsFiles(join(fixtureDir, 'dist-oxc'))
    const babelFiles = await collectJsFiles(join(fixtureDir, 'dist-babel'))
    expect(oxcFiles.length).toBeGreaterThan(0)
    expect(babelFiles.length).toBeGreaterThan(0)
  })

  it('timing across 5 warm runs', async () => {
    const oxcTimes: number[] = []
    const babelTimes: number[] = []
    const runs = 5

    for (let i = 0; i < runs; i++) {
      await rm(join(fixtureDir, 'dist-oxc'), { recursive: true, force: true })
      const t = await buildOnce('vite.config.oxc.ts')
      oxcTimes.push(t)
    }

    for (let i = 0; i < runs; i++) {
      await rm(join(fixtureDir, 'dist-babel'), { recursive: true, force: true })
      const t = await buildOnce('vite.config.babel.ts')
      babelTimes.push(t)
    }

    const median = (arr: number[]) => {
      const s = [...arr].sort((a, b) => a - b)
      return s[Math.floor(s.length / 2)]!
    }
    const avg = (arr: number[]) => arr.reduce((a, b) => a + b, 0) / arr.length

    const oxcMedian = median(oxcTimes)
    const babelMedian = median(babelTimes)
    const oxcAvg = avg(oxcTimes)
    const babelAvg = avg(babelTimes)

    console.log(
      `\n  shadcn-app build timings (${String(runs)} warm runs):\n` +
      `    OXC   — median: ${oxcMedian.toFixed(0)}ms, avg: ${oxcAvg.toFixed(0)}ms, all: [${oxcTimes.map((t) => t.toFixed(0)).join(', ')}]\n` +
      `    Babel — median: ${babelMedian.toFixed(0)}ms, avg: ${babelAvg.toFixed(0)}ms, all: [${babelTimes.map((t) => t.toFixed(0)).join(', ')}]\n` +
      `    Speedup (median): ${(babelMedian / oxcMedian).toFixed(2)}x\n` +
      `    Speedup (avg):    ${(babelAvg / oxcAvg).toFixed(2)}x\n`,
    )

    // Just assert builds completed
    expect(oxcTimes.length).toBe(runs)
    expect(babelTimes.length).toBe(runs)
  })

  it('exact JS output comparison', async () => {
    const oxcDir = join(fixtureDir, 'dist-oxc')
    const babelDir = join(fixtureDir, 'dist-babel')

    const oxcFiles = await collectJsFiles(oxcDir)
    const babelFiles = await collectJsFiles(babelDir)

    expect(oxcFiles.length).toBeGreaterThan(0)
    expect(babelFiles.length).toBe(oxcFiles.length)
    const mismatches = await compareExactJsOutputs(babelDir, oxcDir)
    logExactMismatchSummary('shadcn-app', mismatches)
    expect(mismatches).toHaveLength(0)
  })
})
