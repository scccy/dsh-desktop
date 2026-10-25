/**
 * Behavioural checks for the request shim inside `preload.js`.
 *
 * The WebView constraints from docs/spikes/wkwebview-scheme.md §5 are exercised
 * against a minimal DOM stub instead of a real window:
 *
 *   node src-tauri/bridge/shim.test.cjs
 *
 * The stub reproduces the two properties that made the shim necessary:
 *  - `XMLHttpRequest#setRequestHeader` throws unless the object is OPENED, so a
 *    deferred `open` really has to buffer headers;
 *  - the native `open` records the URL it was given, so the rewrite is visible.
 */
'use strict'

const assert = require('node:assert/strict')
const path = require('node:path')

const PRELOAD = path.join(__dirname, 'preload.js')
const BASE = 'http://127.0.0.1:54321'
const APP_ORIGIN = 'dsh-app://app'

class FakeXhr {
  constructor() {
    this.log = []
    this.readyState = 0
  }

  open(method, url) {
    this.readyState = 1
    this.log.push(['open', method, url])
  }

  setRequestHeader(name, value) {
    if (this.readyState !== 1) throw new Error("InvalidStateError: The object's state must be OPENED")
    this.log.push(['header', name, value])
  }

  send(body) {
    this.log.push(['send', body === undefined ? undefined : String(body)])
  }
}

/**
 * Load `preload.js` against a fresh stub window.
 * @param {{ base?: string, deferredBase?: string }} options - how the base is discovered.
 * @returns the stub window plus the recorded IPC and fetch traffic.
 */
function loadBridge(options) {
  const calls = []
  const fetchCalls = []
  const stub = {
    location: { protocol: 'dsh-app:', hostname: 'app', href: APP_ORIGIN + '/' },
    navigator: { userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)', platform: 'MacIntel' },
    document: {
      documentElement: { dataset: {}, getAttribute: () => null },
      readyState: 'complete',
      addEventListener() {},
      removeEventListener() {},
    },
    Request,
    Headers,
    URL,
    WeakMap,
    XMLHttpRequest: FakeXhr,
    fetch(input, init) {
      fetchCalls.push({ input, init })
      return Promise.resolve({ ok: true })
    },
  }
  stub.top = stub
  stub.__TAURI_INTERNALS__ = {
    invoke(command, args) {
      calls.push({ command, args })
      if (command === 'desktop_platform') return Promise.resolve('darwin')
      if (command === 'desktop_app_base') {
        return options.deferredBase === undefined
          ? Promise.resolve('')
          : Promise.resolve(options.deferredBase)
      }
      return Promise.resolve(null)
    },
    transformCallback(callback) {
      return callback
    },
  }
  if (options.base !== undefined) stub.__DSH_APP_BASE__ = options.base

  const previous = globalThis.window
  globalThis.window = stub
  try {
    delete require.cache[require.resolve(PRELOAD)]
    require(PRELOAD)
  } finally {
    globalThis.window = previous
  }
  return { window: stub, calls, fetchCalls }
}

async function fetchScenarios() {
  const { window, fetchCalls } = loadBridge({ base: BASE })
  const escape = window.__DSH_BRIDGE_ESCAPE__
  assert.equal(typeof window.fetch, 'function')
  assert.equal(typeof escape.nativeFetch, 'function', 'the native fetch stays reachable')
  assert.equal(escape.appBase(), BASE)

  await window.fetch('/api/session?q=1')
  await window.fetch(new URL(APP_ORIGIN + '/plugins/x.json'))
  await window.fetch('https://api.deepseek.com/v1/models')
  assert.equal(fetchCalls.length, 3)
  assert.equal(fetchCalls[0].input, BASE + '/api/session?q=1')
  assert.equal(fetchCalls[1].input, BASE + '/plugins/x.json')
  assert.equal(fetchCalls[2].input, 'https://api.deepseek.com/v1/models', 'foreign URLs pass through')

  // Constraint A + C: body buffered from a clone, mode forced to cors, and the
  // caller's request left unconsumed so the fallback stays usable.
  const original = new Request(APP_ORIGIN + '/api/upload', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ hello: 'world' }),
  })
  await window.fetch(original)
  const forwarded = fetchCalls[fetchCalls.length - 1].input
  assert.ok(forwarded instanceof Request, 'a rebuilt Request is forwarded')
  assert.equal(forwarded.url, BASE + '/api/upload')
  assert.equal(forwarded.method, 'POST')
  assert.equal(forwarded.mode, 'cors', 'a cross-origin rewrite asks for cors')
  assert.equal(forwarded.headers.get('content-type'), 'application/json')
  assert.equal(original.bodyUsed, false, 'the caller request stays usable')
  assert.equal(await forwarded.text(), JSON.stringify({ hello: 'world' }))
}

async function xhrScenarios() {
  const { window } = loadBridge({ base: BASE })
  const xhr = new window.XMLHttpRequest()
  xhr.open('POST', '/api/x')
  assert.deepEqual(xhr.log, [['open', 'POST', BASE + '/api/x']], 'the known base is used immediately')
  xhr.setRequestHeader('x-test', '1')
  xhr.send('body')
  assert.deepEqual(xhr.log, [
    ['open', 'POST', BASE + '/api/x'],
    ['header', 'x-test', '1'],
    ['send', 'body'],
  ])

  // A foreign absolute URL is not rewritten but still opens synchronously.
  const foreign = new window.XMLHttpRequest()
  foreign.open('GET', 'https://api.deepseek.com/v1/models')
  assert.deepEqual(foreign.log, [['open', 'GET', 'https://api.deepseek.com/v1/models']])
}

async function deferredBaseScenarios() {
  const { window, fetchCalls } = loadBridge({ deferredBase: BASE })

  // Constraint B: open is deferred, so headers must be buffered instead of
  // reaching the native (still UNSENT) object.
  const xhr = new window.XMLHttpRequest()
  xhr.open('PUT', APP_ORIGIN + '/api/y')
  assert.deepEqual(xhr.log, [], 'nothing is opened before the base is known')
  xhr.setRequestHeader('x-test', '2')
  xhr.send('payload')
  await new Promise((resolve) => setTimeout(resolve, 0))
  assert.deepEqual(
    xhr.log,
    [
      ['open', 'PUT', BASE + '/api/y'],
      ['header', 'x-test', '2'],
      ['send', 'payload'],
    ],
    'headers are buffered then replayed in order after the real open',
  )

  // fetch waits for the same base.
  await window.fetch('/api/late')
  assert.equal(fetchCalls[fetchCalls.length - 1].input, BASE + '/api/late')
}

async function main() {
  await fetchScenarios()
  await xhrScenarios()
  await deferredBaseScenarios()
  console.log('preload shim: ok')
}

main().catch((error) => {
  console.error(error)
  process.exit(1)
})
