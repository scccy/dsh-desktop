/**
 * Node-level checks for the pure helpers inside `preload.js`.
 *
 * `preload.js` exports its URL-rewrite and platform helpers when it is loaded
 * as a CommonJS module and returns early because no Tauri IPC exists, so this
 * test needs no DOM stub:
 *
 *   node src-tauri/bridge/rewrite.test.cjs
 */
'use strict'

const assert = require('node:assert/strict')
const { rewriteUrl, needsRewrite, parseBase, platformFromUserAgent } = require('./preload.js')

const BASE = 'http://127.0.0.1:54321'

// Relative URLs move onto the loopback app server.
assert.equal(rewriteUrl('/api/session', BASE), BASE + '/api/session')
assert.equal(rewriteUrl('api/session', BASE), BASE + '/api/session')
assert.equal(rewriteUrl('/api/session?q=1&r=2#top', BASE), BASE + '/api/session?q=1&r=2#top')
assert.equal(rewriteUrl('?q=1', BASE), BASE + '/?q=1')
assert.equal(rewriteUrl('#frag', BASE), BASE + '/#frag')

// App-origin URLs lose the scheme and keep their path.
assert.equal(rewriteUrl('dsh-app://app/assets/index.js', BASE), BASE + '/assets/index.js')
assert.equal(rewriteUrl('dsh-app://app/', BASE), BASE + '/')
assert.equal(rewriteUrl('dsh-app://app', BASE), BASE + '/')

// Absolute host URLs, object URLs and foreign schemes are left alone.
assert.equal(rewriteUrl('http://127.0.0.1:9000/api', BASE), 'http://127.0.0.1:9000/api')
assert.equal(rewriteUrl('https://api.deepseek.com/v1/models', BASE), 'https://api.deepseek.com/v1/models')
assert.equal(rewriteUrl('blob:http://127.0.0.1:54321/abc', BASE), 'blob:http://127.0.0.1:54321/abc')
assert.equal(rewriteUrl('data:text/plain,hi', BASE), 'data:text/plain,hi')
assert.equal(rewriteUrl('//cdn.example.com/x.js', BASE), '//cdn.example.com/x.js')
assert.equal(rewriteUrl('ws://127.0.0.1:54321/socket', BASE), 'ws://127.0.0.1:54321/socket')

// A missing or unusable base never changes the request.
assert.equal(rewriteUrl('/api/session', ''), '/api/session')
assert.equal(rewriteUrl('/api/session', undefined), '/api/session')
assert.equal(rewriteUrl('/api/session', 'dsh-app://app'), '/api/session')
assert.equal(rewriteUrl('', BASE), '')

// A base carrying a path still contributes only its origin.
assert.equal(rewriteUrl('/api', 'http://127.0.0.1:1234/ignored'), 'http://127.0.0.1:1234/api')

assert.equal(needsRewrite('/api'), true)
assert.equal(needsRewrite('dsh-app://app/api'), true)
assert.equal(needsRewrite('https://example.com/api'), false)
assert.equal(needsRewrite('//example.com/api'), false)

assert.equal(parseBase('http://127.0.0.1:1') === null, false)
assert.equal(parseBase('dsh-app://app'), null)
assert.equal(parseBase('  '), null)

// Platform fallback used before the shell answers `desktop_platform`.
assert.equal(platformFromUserAgent('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)', 'MacIntel'), 'darwin')
assert.equal(platformFromUserAgent('Mozilla/5.0 (Windows NT 10.0; Win64; x64)', 'Win32'), 'win32')
assert.equal(platformFromUserAgent('Mozilla/5.0 (X11; Linux x86_64)', 'Linux x86_64'), 'linux')
assert.equal(platformFromUserAgent(undefined, undefined), 'linux')

// ---------------------------------------------------------------------------
// Invariants of the request shim that the real WebView forced on us
// (docs/spikes/wkwebview-scheme.md §5). They are checked against the source
// because they describe how the code calls the platform, not a pure function.
// ---------------------------------------------------------------------------
const fs = require('node:fs')
const path = require('node:path')
const source = fs.readFileSync(path.join(__dirname, 'preload.js'), 'utf8')

function expectMatch(name, pattern) {
  assert.match(source, pattern, name)
}

// A: a `Request` body is rebuilt from a clone, never from the live stream.
expectMatch('Request bodies are buffered from a clone', /clone = request\.clone\(\)[\s\S]{0,300}?clone\.arrayBuffer\(\)/)
for (const consumer of ['arrayBuffer', 'text', 'json', 'blob', 'formData']) {
  assert.ok(
    !source.includes(`request.${consumer}(`),
    `the original Request must not be consumed (found request.${consumer}())`,
  )
}
// B: request headers are buffered until the native open ran.
expectMatch('headers are buffered before the native open', /record\.headers\.push\(\[name, value\]\)/)
expectMatch('headers are replayed after the native open', /originalOpen\.apply\(xhr, args\)[\s\S]{0,400}originalSetRequestHeader\.apply/)
// C: a rewritten request is cross-origin, so it must ask for CORS.
expectMatch('mode is forced to cors', /options\.mode = 'cors'/)
expectMatch('only-if-cached is dropped', /cache !== 'only-if-cached'/)
// E: the native primitives are captured before the rewrite is installed.
expectMatch('native fetch is captured first', /var originalFetch = shellWindow\.fetch[\s\S]{0,220}nativeFetch = function/)
expectMatch('escape hatch is published', /__DSH_BRIDGE_ESCAPE__/)
// EventSource stays native: it only needs an absolute loopback URL.
assert.equal(/EventSource\s*=/.test(source), false, 'EventSource must not be patched')
// WebSocket stays native too: the Web UI uses `streamBaseUrl`.
assert.equal(/WebSocket\s*=/.test(source), false, 'WebSocket must not be patched')

console.log('preload bridge helpers: ok')
