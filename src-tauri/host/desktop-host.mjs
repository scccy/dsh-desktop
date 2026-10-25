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
 *   DSH_HOST_READY <json>   {"url":"http://127.0.0.1:PORT/?token=…",
 *                            "origin":"http://127.0.0.1:PORT",
 *                            "protocol":"http:","protocolVersion":1,
 *                            "injections":[{…},…]}
 *   DSH_HOST_FATAL <json>   {"code":"version","message":"…",
 *                            "found":"0.2.0","minimum":"0.2.1-rc.2"}
 *
 * `injections` is the complete row array (the rows carry the index HTML, styles
 * and scripts), written in a single flushed write so a multi-hundred-kilobyte
 * payload is never split or truncated. stdout is reserved for the protocol; all
 * diagnostics go to stderr.
 *
 * One further protocol line answers a control request (see `stdin` below):
 *   DSH_HOST_CONTROL <json>  {"type":"quit-inspection","activeTasks":false,
 *                             "scheduledTasks":false,"error":"…"?}
 *
 * stdin: the line `shutdown` asks for a graceful stop; closing stdin does the
 * same. SIGTERM/SIGINT are treated as shutdown too.
 *
 * stdin, control requests: one JSON object per line. `{"type":"quit-inspection"}`
 * asks what quitting now would interrupt; the answer is one `DSH_HOST_CONTROL`
 * line whose `activeTasks` follows the official `dsh-desktop-host` rule
 * (a running agent, queued inbox messages, or a running/stopping job) and whose
 * `scheduledTasks` counts armed reminders in the loaded sessions. A Host that
 * cannot answer reports `activeTasks: true` with an `error`, so the shell warns
 * before quitting instead of quitting silently.
 *
 * Usage:
 *   node desktop-host.mjs --dsh <path/to/@deepseek-ai/dsh> --project <profileDir> [--version <v>]
 *   (or env DSH_DESKTOP_DSH_PACKAGE / DSH_DESKTOP_PROJECT_DIR / DSH_CLIENT_VERSION)
 */

import { execFileSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { pathToFileURL } from 'node:url'
import { inspect } from 'node:util'

const MAX_DIAGNOSTIC = 64 * 1024

// Version policy. Must match `crates/dsh-lite-core/src/version_gate.rs`.
// `0.2.0-rc.2` boots but skips the marketplace bundle and reports no
// `webServer.protocol`; `0.2.1-rc.2` is the first release that matches this
// launcher's contract, so everything older is refused with the upgrade command.
const MINIMUM_VERSION = '0.2.1-rc.2'
const TESTED_VERSION = '0.2.1'
const UPGRADE_COMMAND = 'npm i -g @deepseek-ai/dsh'
const VERSION_PROBE_TIMEOUT_MS = 20_000

/** Protocol version of this stdout contract. */
const PROTOCOL_VERSION = 1

/** `token=VALUE` fragments in redirected diagnostics. */
const TOKEN_IN_TEXT = /token=[^\s&"']*/gi

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

/**
 * Keeps stdout for this protocol alone.
 *
 * The profile boot prints its own `dsh web: <url>` readiness line to stdout, and
 * that line carries the one-shot token. Anything that is not a `DSH_HOST_*` line
 * is moved to stderr, where the shell keeps only a redacted tail.
 */
function claimStdout() {
  const write = process.stdout.write.bind(process.stdout)
  process.stdout.write = (chunk, encoding, callback) => {
    const text = typeof chunk === 'string' ? chunk : chunk.toString('utf8')
    const done = typeof encoding === 'function' ? encoding : callback
    if (
      text.startsWith('DSH_HOST_READY ') ||
      text.startsWith('DSH_HOST_FATAL ') ||
      text.startsWith('DSH_HOST_CONTROL ')
    ) {
      return write(text, done)
    }
    // The token is redacted here as well: the shell redacts its own tails, but a
    // launcher run from a terminal must not spill it either.
    return process.stderr.write(text.replace(TOKEN_IN_TEXT, 'token=<redacted>'), done)
  }
}

/** Writes one protocol line and resolves once the bytes reached the OS. */
function sendLine(line) {
  const payload = line + '\n'
  return new Promise((resolve, reject) => {
    process.stdout.write(payload, (error) => (error ? reject(error) : resolve()))
  })
}

function warn(message) {
  process.stderr.write(`desktop host: ${message}\n`)
}

/**
 * Reads the first semantic version in `text`.
 * Build metadata is captured but ignored during comparison (semver §10).
 */
function parseVersion(text) {
  const match = /(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?/.exec(String(text))
  if (!match) return undefined
  return {
    raw: match[0],
    major: Number(match[1]),
    minor: Number(match[2]),
    patch: Number(match[3]),
    prerelease: match[4] ? match[4].split('.') : [],
  }
}

function compareIdentifiers(left, right) {
  const leftNumeric = /^\d+$/.test(left)
  const rightNumeric = /^\d+$/.test(right)
  if (leftNumeric && rightNumeric) return Math.sign(Number(left) - Number(right))
  if (leftNumeric) return -1
  if (rightNumeric) return 1
  return left === right ? 0 : left < right ? -1 : 1
}

/** Semver precedence; a prerelease sorts below the release it leads to. */
function compareVersions(left, right) {
  for (const key of ['major', 'minor', 'patch']) {
    if (left[key] !== right[key]) return left[key] < right[key] ? -1 : 1
  }
  const leftPre = left.prerelease
  const rightPre = right.prerelease
  if (leftPre.length === 0 && rightPre.length === 0) return 0
  if (leftPre.length === 0) return 1
  if (rightPre.length === 0) return -1
  const shared = Math.min(leftPre.length, rightPre.length)
  for (let index = 0; index < shared; index += 1) {
    const ordering = compareIdentifiers(leftPre[index], rightPre[index])
    if (ordering !== 0) return ordering
  }
  return Math.sign(leftPre.length - rightPre.length)
}

function readManifest(dshPackage) {
  try {
    return JSON.parse(readFileSync(join(dshPackage, 'package.json'), 'utf8'))
  } catch {
    return {}
  }
}

function manifestBin(manifest) {
  if (typeof manifest.bin === 'string') return manifest.bin
  if (manifest.bin && typeof manifest.bin === 'object' && typeof manifest.bin.dsh === 'string') {
    return manifest.bin.dsh
  }
  return undefined
}

/**
 * Asks the installed dsh for its version.
 *
 * `dsh --version` is the public contract; the package manifest is only the
 * fallback when the CLI cannot be run, so a wrapper or a stripped install does
 * not turn into a false refusal.
 */
function readDshVersion(dshPackage) {
  const manifest = readManifest(dshPackage)
  const entry = manifestBin(manifest) ?? 'lib/bin.js'
  try {
    const output = execFileSync(process.execPath, [join(dshPackage, entry), '--version'], {
      encoding: 'utf8',
      timeout: VERSION_PROBE_TIMEOUT_MS,
      stdio: ['ignore', 'pipe', 'ignore'],
    })
    const version = parseVersion(output)
    if (version) return { version, source: 'dsh --version' }
  } catch {
    // Fall through to the manifest; the probe itself is not a gate.
  }
  const fallback = parseVersion(manifest.version ?? '')
  if (fallback) return { version: fallback, source: 'package.json' }
  return { version: undefined, source: 'none' }
}

/**
 * Classifies the reported version.
 *
 * - `below`      → refuse: the profile this launcher drives does not exist yet;
 * - `untested`   → accept with a warning: a newer dsh may have changed the
 *                  desktop profile, but refusing it would break on every release;
 * - `supported`  → accept;
 * - `unknown`    → accept with a warning: a version we cannot compare is not
 *                  evidence that dsh is broken.
 */
function gateVersion(reported) {
  if (!reported.version) return { state: 'unknown' }
  const minimum = parseVersion(MINIMUM_VERSION)
  const tested = parseVersion(TESTED_VERSION)
  const found = reported.version.raw
  if (compareVersions(reported.version, minimum) < 0) return { state: 'below', found }
  if (compareVersions(reported.version, tested) > 0) return { state: 'untested', found }
  return { state: 'supported', found }
}

async function refuseVersion(gate, source) {
  await sendLine(
    'DSH_HOST_FATAL ' +
      JSON.stringify({
        code: 'version',
        message: `dsh ${gate.found} is older than the required ${MINIMUM_VERSION} (read from ${source}); upgrade with \`${UPGRADE_COMMAND}\` and try again`,
        found: gate.found,
        minimum: MINIMUM_VERSION,
        upgrade: UPGRADE_COMMAND,
      }),
  )
  process.exitCode = 1
}

/**
 * Whether stopping now would interrupt work, mirroring the official
 * `dsh-desktop-host` rule: a running agent (including subagents and turns
 * waiting for approval), queued inbox messages, or a running/stopping job.
 */
function hasActiveTasks(agents, jobs) {
  const liveAgents = agents.list()
  if (
    liveAgents.some(
      (agent) =>
        agent.status === 'running' ||
        agent.inbox?.nextTurn?.length > 0 ||
        agent.inbox?.nextStep?.length > 0,
    )
  ) {
    return true
  }
  return [undefined, ...liveAgents].some((agent) =>
    jobs.list(agent?.id).some((job) => job.status === 'running' || job.status === 'stopping'),
  )
}

/**
 * Inspects what a quit would interrupt.
 *
 * `scheduledTasks` only counts reminders of sessions loaded in this process:
 * a reminder in a never-loaded session cannot fire, so it is not counted.
 */
async function inspectQuit(ctx) {
  const agents = ctx.get('agents')
  const jobs = ctx.get('jobs')
  if (agents === undefined || jobs === undefined) {
    throw new Error('desktop quit: task services are unavailable')
  }
  const liveAgents = agents.list()
  const activeTasks = hasActiveTasks(agents, jobs)
  let scheduledTasks = false
  for (const agent of liveAgents) {
    const activity = await ctx.waterfall(
      'workspace/session-activity',
      { sessionId: agent.id },
      () => Promise.resolve([]),
    )
    if (Array.isArray(activity) && activity.some((entry) => entry?.kind === 'schedule')) {
      scheduledTasks = true
      break
    }
  }
  return { activeTasks, scheduledTasks }
}

async function main() {
  claimStdout()
  const args = parseArgs(process.argv.slice(2))
  const dshPackage = args.dsh ?? process.env.DSH_DESKTOP_DSH_PACKAGE
  const projectDir = args.project ?? process.env.DSH_DESKTOP_PROJECT_DIR
  if (!dshPackage) throw new Error('desktop host: --dsh (or DSH_DESKTOP_DSH_PACKAGE) is required')
  if (!projectDir) throw new Error('desktop host: --project (or DSH_DESKTOP_PROJECT_DIR) is required')

  // Gate the installed dsh before paying for a profile boot: a stale install
  // fails deep inside the profile otherwise, which is hard to read.
  const reported = readDshVersion(dshPackage)
  const gate = gateVersion(reported)
  if (gate.state === 'below') {
    await refuseVersion(gate, reported.source)
    return
  }
  if (gate.state === 'untested') {
    warn(
      `dsh ${gate.found} is newer than the tested ${TESTED_VERSION}; continuing, but the desktop profile may have changed`,
    )
  }
  if (gate.state === 'unknown') {
    warn(`could not read the dsh version (${reported.source}); continuing without the version check`)
  }

  // The desktop profile's telemetry plugin validates `serviceVersion` from this
  // variable (`packages/bundle/web-app/cordis.patch.yml`); without it the boot
  // aborts before the web server starts.
  process.env.DSH_CLIENT_VERSION ??= args.version ?? '0.0.0'

  // The install anchor is the manifest the official shell computes from its
  // bundled runtime: profile resolution is anchored on it.
  const installAnchor = join(dshPackage, 'package.json')

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

  // The booted profile context: available once the Host finished starting, so a
  // control request that arrives earlier is answered as "cannot inspect".
  let context

  /**
   * Answers one control request with one `DSH_HOST_CONTROL` line.
   *
   * Unknown request types are ignored: a newer shell may ask something this
   * launcher does not know, and a wrong answer is worse than no answer.
   */
  const answerControl = async (request) => {
    if (request.type !== 'quit-inspection') return
    const answer = { type: 'quit-inspection' }
    try {
      if (context === undefined) throw new Error('desktop quit: Host is not ready')
      const inspection = await inspectQuit(context)
      answer.activeTasks = inspection.activeTasks
      answer.scheduledTasks = inspection.scheduledTasks
    } catch (error) {
      // An inspector that cannot answer counts as interruptible work, so the
      // shell asks before quitting instead of quitting silently.
      answer.activeTasks = true
      answer.scheduledTasks = false
      answer.error = error instanceof Error ? error.message : String(error)
    }
    await sendLine('DSH_HOST_CONTROL ' + JSON.stringify(answer))
  }

  let buffer = ''
  process.stdin.setEncoding('utf8')
  process.stdin.on('data', (chunk) => {
    buffer += chunk
    let index
    while ((index = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, index).trim()
      buffer = buffer.slice(index + 1)
      if (line === '') continue
      if (line === 'shutdown') {
        void stop()
        continue
      }
      let request
      try {
        request = JSON.parse(line)
      } catch {
        warn('ignoring a control line that is neither `shutdown` nor JSON')
        continue
      }
      if (request !== null && typeof request === 'object' && typeof request.type === 'string') {
        void answerControl(request)
      }
    }
  })
  process.stdin.on('end', () => { void stop() })

  const { ctx } = await application
  context = ctx
  // Newer dsh releases expose `webServer.protocol` (with the colon); the
  // 0.2.0-rc line does not, and the server is plain http there.
  const protocol = ctx.webServer.protocol ?? 'http:'
  const port = ctx.webServer.port
  const origin = `${protocol}//127.0.0.1:${String(port)}`
  const url = ctx.connection.authenticatedUrl(origin)
  // The rows carry the index HTML, styles and scripts: send them whole, the
  // renderer needs them to boot (`dshDesktopBoot.ready()` in the preload bridge).
  let injections = []
  if (typeof ctx.webServer.collectIndexInjections === 'function') {
    const rows = ctx.webServer.collectIndexInjections(ctx.connection.browserPeer)
    if (Array.isArray(rows)) injections = rows
  } else {
    warn('this dsh does not expose index injections; starting without them')
  }

  await sendLine(
    'DSH_HOST_READY ' +
      JSON.stringify({ url, origin, protocol, protocolVersion: PROTOCOL_VERSION, injections }),
  )
}

main().catch(async (error) => {
  const message = error instanceof Error ? error.message : String(error)
  const diagnostic = inspect(error, { depth: 4, maxArrayLength: 50 }).slice(0, MAX_DIAGNOSTIC)
  await sendLine('DSH_HOST_FATAL ' + JSON.stringify({ message, diagnostic })).catch(() => {})
  console.error(error)
  process.exitCode = 1
})
