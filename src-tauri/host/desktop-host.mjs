#!/usr/bin/env node
/**
 * Desktop Host launcher — the Rust port of the official `apps/desktop-host`.
 *
 * Boots the dsh `desktop` profile through the installed dsh runtime and reports
 * the authenticated web URL plus the index injections back to the shell over
 * stdout, so the Rust side never has to link against Node.
 *
 * Why this exists (and why it is not just `dsh --profile desktop`):
 * the dsh CLI hard-rejects the desktop profile (`managed exclusively by the
 * Electron application`); the official shell bypasses that by calling the
 * internal `runProfile` from `@deepseek-ai/dsh/profile-boot`. We do the same.
 *
 * Runtime model: this launcher deliberately uses the *system* dsh and node
 * (`--dsh` points at the installed `@deepseek-ai/dsh` package); no bundled
 * Python/Node/dsh/pnpm is shipped. See docs/desktop-parity.md (G2).
 *
 * Protocol (one JSON object per line, stdout):
 *   DSH_HOST_READY <json>   {"url": "...", "injections": <count>}
 *   DSH_HOST_FATAL <json>   {"message": "...", "diagnostic": "..."}
 * stdout is reserved for the protocol; all diagnostics go to stderr.
 *
 * stdin: the line `shutdown` asks for a graceful stop; closing stdin does the
 * same. SIGTERM/SIGINT are treated as shutdown too.
 *
 * Usage:
 *   node desktop-host.mjs --dsh <path/to/@deepseek-ai/dsh> --project <profileDir> [--version <v>]
 *   (or env DSH_DESKTOP_DSH_PACKAGE / DSH_DESKTOP_PROJECT_DIR / DSH_CLIENT_VERSION)
 */

import { dirname, join } from 'node:path'
import { pathToFileURL } from 'node:url'
import { inspect } from 'node:util'

const MAX_DIAGNOSTIC = 64 * 1024

function parseArgs(argv) {
  const out = { dsh: undefined, project: undefined, version: undefined }
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i]
    const next = () => {
      i += 1
      if (i >= argv.length) throw new Error(`missing value for ${arg}`)
      return argv[i]
    }
    if (arg === '--dsh') out.dsh = next()
    else if (arg === '--project') out.project = next()
    else if (arg === '--version') out.version = next()
  }
  return out
}

function send(line) {
  process.stdout.write(line + '\n')
}

async function main() {
  const args = parseArgs(process.argv.slice(2))
  const dshPackage = args.dsh ?? process.env.DSH_DESKTOP_DSH_PACKAGE
  const projectDir = args.project ?? process.env.DSH_DESKTOP_PROJECT_DIR
  if (!dshPackage) throw new Error('desktop host: --dsh (or DSH_DESKTOP_DSH_PACKAGE) is required')
  if (!projectDir) throw new Error('desktop host: --project (or DSH_DESKTOP_PROJECT_DIR) is required')

  // The desktop profile's telemetry plugin validates `serviceVersion` from this
  // variable (`packages/bundle/web-app/cordis.patch.yml`); without it the boot
  // aborts before the web server starts.
  process.env.DSH_CLIENT_VERSION ??= args.version ?? '0.0.0'

  // `runtimeDir` is the directory that contains `node_modules/@deepseek-ai/dsh`,
  // i.e. the install anchor the official shell computes from its bundled runtime.
  const installAnchor = join(dshPackage, 'package.json')
  const runtimeDir = dirname(dirname(dirname(dshPackage)))

  const { runProfile } = await import(pathToFileURL(join(dshPackage, 'lib', 'profile-boot.js')).href)
  const { loadLayeredEnv, loadProfileDirectory, reportSkippedBundles } = await import(
    pathToFileURL(join(dshPackage, 'node_modules', '@deepseek-ai', 'dsh-app-boot', 'lib', 'index.js')).href
  )

  const profile = loadProfileDirectory('dsh', projectDir, installAnchor)
  reportSkippedBundles('dsh', profile)

  const application = runProfile({
    environment: loadLayeredEnv('dsh'),
    profile: 'desktop',
    resolvedProfile: { profile, installAnchor },
    patchFiles: [],
    args: ['--no-open', '--host', '127.0.0.1', '--port', '0'],
  })

  let stopping
  const stop = () => stopping ??= (async () => {
    const running = await application.catch(() => undefined)
    await running?.shutdown?.shutdown(0)
    process.exit(0)
  })()

  process.on('SIGTERM', () => { void stop() })
  process.on('SIGINT', () => { void stop() })

  let buffer = ''
  process.stdin.setEncoding('utf8')
  process.stdin.on('data', (chunk) => {
    buffer += chunk
    let index
    while ((index = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, index).trim()
      buffer = buffer.slice(index + 1)
      if (line === 'shutdown') void stop()
    }
  })
  process.stdin.on('end', () => { void stop() })

  const { ctx } = await application
  // Newer dsh releases expose `webServer.protocol` (with the colon); the
  // 0.2.0-rc line does not, and the server is plain http there.
  const protocol = ctx.webServer.protocol ?? 'http:'
  const port = ctx.webServer.port
  const url = ctx.connection.authenticatedUrl(`${protocol}//127.0.0.1:${String(port)}`)
  const injections = ctx.webServer.collectIndexInjections?.(ctx.connection.browserPeer) ?? []

  send('DSH_HOST_READY ' + JSON.stringify({ url, injections: injections.length }))
}

main().catch((error) => {
  const message = error instanceof Error ? error.message : String(error)
  const diagnostic = inspect(error, { depth: 4, maxArrayLength: 50 }).slice(0, MAX_DIAGNOSTIC)
  send('DSH_HOST_FATAL ' + JSON.stringify({ message, diagnostic }))
  console.error(error)
  process.exitCode = 1
})
