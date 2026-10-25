/**
 * DSH Desktop preload bridge.
 *
 * Tauri port of the official Electron preload
 * (`apps/desktop/src/preload-app.ts` + `preload-platform.ts` + `preload-theme.ts`).
 * It is installed with `WebviewBuilder::initialization_script` on the content
 * webview, so it runs before any page script, and the page only ever sees plain
 * frozen objects and promises: no Tauri internals leak through these globals.
 *
 * Two deliberate differences from the Electron preload (see
 * docs/bridge-contract.md for the full table):
 *
 *  1. No `location.protocol === 'dsh-app:'` assumption. The document may be
 *     served from `dsh-app://app` (origin A) or from a loopback http origin
 *     (origin B); both are accepted, exactly like the Rust command guard.
 *  2. Everything that crosses the shell boundary is asynchronous (`invoke`),
 *     so no member is a synchronous getter except `__DSH_HOST_PATHS__.pathFor`
 *     (always `''`) and the `data-platform` fallback, which is written
 *     synchronously as soon as the document root exists.
 *
 * Dynamic traffic never stays on the `dsh-app:` scheme (its responses are
 * buffered by the WebView), so `fetch` and `XMLHttpRequest` are rewritten onto
 * the loopback app server when the document is an app-origin document.
 */
;(function () {
  'use strict'

  /** Scheme and host of Desktop-owned application documents. */
  var APP_SCHEME = 'dsh-app:'
  var APP_HOST = 'app'
  /** Absolute URL detector: any RFC 3986 scheme prefix. */
  var SCHEME_PREFIX = /^[a-zA-Z][a-zA-Z0-9+.\-]*:/
  /** Attribute names shared with the Web UI (`ui-theme` / `ui-layout`). */
  var THEME_SOURCE_ATTRIBUTE = 'data-ds-theme-source'

  /** Event names; the Rust side emits through `Emitter::emit`. */
  var EVENT_SHORTCUTS_INPUT = 'dsh-desktop://shortcuts-input'
  var EVENT_SHORTCUTS_CHANGED = 'dsh-desktop://shortcuts-changed'
  var EVENT_UPDATES_PRESENTATION = 'dsh-desktop://updates-presentation'
  var EVENT_WINDOW_FULLSCREEN = 'dsh-desktop://window-fullscreen'
  var EVENT_ENTER_WORKSPACE = 'dsh-desktop://enter-workspace'
  var EVENT_BROWSER_OPEN_REQUESTED = 'dsh-desktop://browser-open-requested'

  /** Modifier order used for binding keys, matching the Web UI's protocol. */
  var MODIFIER_ORDER = ['control', 'alt', 'shift', 'meta']

  /**
   * Report a bridge problem without breaking the page.
   * @param message - short description.
   * @param error - optional cause.
   */
  function warn(message, error) {
    try {
      if (error === undefined) console.warn('[dsh desktop] ' + message)
      else console.warn('[dsh desktop] ' + message, error)
    } catch (ignored) {
      // A page that replaced `console` must not take the bridge down.
    }
  }

  /**
   * Parse a loopback app-server base (`http://127.0.0.1:<port>`).
   * @param value - candidate base.
   * @returns the parsed base, or null when it is not an http(s) origin.
   */
  function parseBase(value) {
    if (typeof value !== 'string') return null
    var trimmed = value.trim()
    if (trimmed === '') return null
    try {
      var url = new URL(trimmed)
      if (url.protocol !== 'http:' && url.protocol !== 'https:') return null
      return url
    } catch (error) {
      return null
    }
  }

  /**
   * Decide whether a URL belongs to the application document.
   * @param raw - request URL, absolute or relative.
   * @returns the resolved app-origin URL, or null when the URL is not ours.
   */
  function rewriteTarget(raw) {
    if (typeof raw !== 'string' || raw === '') return null
    var resolved
    if (SCHEME_PREFIX.test(raw) || raw.slice(0, 2) === '//') {
      try {
        resolved = new URL(raw)
      } catch (error) {
        return null
      }
    } else {
      try {
        resolved = new URL(raw, APP_SCHEME + '//' + APP_HOST + '/')
      } catch (error) {
        return null
      }
    }
    if (resolved.protocol !== APP_SCHEME || resolved.hostname !== APP_HOST) return null
    return resolved
  }

  /**
   * Whether `raw` has to be rewritten before it can leave the page.
   * @param raw - request URL.
   * @returns true for relative URLs and for `dsh-app://app` URLs.
   */
  function needsRewrite(raw) {
    return rewriteTarget(raw) !== null
  }

  /**
   * Rewrite page-origin traffic onto the loopback app server.
   *
   * Absolute `http(s)://`, `blob:` and `data:` URLs stay untouched so direct
   * Host calls and object URLs keep working; relative paths and app-origin
   * URLs become `base + pathname + search + hash`.
   * @param input - request URL (string; callers stringify other forms first).
   * @param base - app-server base such as `http://127.0.0.1:54321`.
   * @returns the rewritten URL, or the original string when no rewrite applies.
   */
  function rewriteUrl(input, base) {
    var raw = typeof input === 'string' ? input : String(input)
    if (raw === '') return raw
    var baseUrl = parseBase(base)
    if (baseUrl === null) return raw
    var target = rewriteTarget(raw)
    if (target === null) return raw
    // A non-special scheme can resolve to an empty path (`dsh-app://app`).
    var path = target.pathname === '' ? '/' : target.pathname
    return baseUrl.origin + path + target.search + target.hash
  }

  /**
   * Best-effort platform when no Rust answer is available yet.
   * @param userAgent - `navigator.userAgent`.
   * @param platformHint - `navigator.platform`, when the WebView exposes it.
   * @returns `darwin`, `win32` or `linux`, matching `process.platform`.
   */
  function platformFromUserAgent(userAgent, platformHint) {
    var agent = typeof userAgent === 'string' ? userAgent : ''
    var hint = typeof platformHint === 'string' ? platformHint : ''
    if (/Win/i.test(hint) || /Windows/i.test(agent)) return 'win32'
    if (/Mac/i.test(hint) || /Mac OS X|Macintosh/i.test(agent)) return 'darwin'
    return 'linux'
  }

  // The pure helpers above are also exercised by bridge/rewrite.test.cjs.
  if (typeof module === 'object' && module !== null && typeof module.exports === 'object') {
    module.exports = {
      rewriteUrl: rewriteUrl,
      needsRewrite: needsRewrite,
      parseBase: parseBase,
      platformFromUserAgent: platformFromUserAgent,
    }
  }

  var shellWindow = typeof window === 'object' && window !== null ? window : null
  if (shellWindow === null) return
  var internals = shellWindow.__TAURI_INTERNALS__
  if (internals === undefined || internals === null || typeof internals.invoke !== 'function') {
    warn('the shell IPC is unavailable; the bridge is inert')
    return
  }

  var mainFrame = true
  try {
    mainFrame = shellWindow.top === shellWindow
  } catch (error) {
    mainFrame = false
  }

  /**
   * Call one Tauri command through the shell's own IPC.
   * @param command - command name.
   * @param args - camelCase arguments.
   * @returns the command's promise.
   */
  function invoke(command, args) {
    var api = shellWindow.__TAURI_INTERNALS__
    if (api === undefined || api === null || typeof api.invoke !== 'function') {
      return Promise.reject(new Error('dsh desktop: the shell IPC is unavailable'))
    }
    return api.invoke(command, args === undefined ? {} : args)
  }

  /**
   * Subscribe to one shell event.
   * @param event - event name.
   * @param listener - receives the event payload.
   * @returns disposer; safe to call more than once.
   */
  function listen(event, listener) {
    var api = shellWindow.__TAURI_INTERNALS__
    if (api === undefined || api === null || typeof api.transformCallback !== 'function') {
      return function () {}
    }
    var target = { kind: 'Any' }
    var handler = api.transformCallback(function (message) {
      var payload = message !== null && typeof message === 'object' && 'payload' in message ? message.payload : message
      try {
        listener(payload)
      } catch (error) {
        console.error('[dsh desktop] ' + event + ' listener failed', error)
      }
    })
    var registration = invoke('plugin:event|listen', { event: event, target: target, handler: handler })
    var active = true
    return function () {
      if (!active) return
      active = false
      registration.then(
        function (eventId) {
          invoke('plugin:event|unlisten', { event: event, eventId: eventId, target: target })
            .catch(function (error) { warn('could not stop listening to ' + event, error) })
        },
        function () {},
      )
    }
  }

  /**
   * Define one non-writable bridge global, like `contextBridge.exposeInMainWorld`.
   * @param name - global name.
   * @param value - frozen value.
   */
  function expose(name, value) {
    try {
      Object.defineProperty(shellWindow, name, {
        value: value,
        configurable: false,
        writable: false,
        enumerable: false,
      })
    } catch (error) {
      warn('could not expose ' + name, error)
    }
  }

  // ---------------------------------------------------------------------------
  // Origin gate: only a Desktop-owned document gets the full bridge. Both the
  // `dsh-app://app` and the loopback http origin are accepted, so the shell can
  // switch schemes without touching this file. Rust enforces the same rule on
  // every command.
  // ---------------------------------------------------------------------------
  var protocolName = String(shellWindow.location.protocol || '')
  var hostName = String(shellWindow.location.hostname || '')
  var bridgedOrigin = ''
  try {
    if (typeof shellWindow.__DSH_BRIDGE_ORIGIN__ === 'string') bridgedOrigin = shellWindow.__DSH_BRIDGE_ORIGIN__
  } catch (error) {
    bridgedOrigin = ''
  }

  /** @returns whether this document belongs to the shell's application. */
  function documentIsBridged() {
    if (!mainFrame) return false
    if (bridgedOrigin !== '' && protocolName + '//' + hostName === parseOriginName(bridgedOrigin)) return true
    if (protocolName === APP_SCHEME && hostName === APP_HOST) return true
    if (protocolName === 'http:' || protocolName === 'https:') {
      return hostName === '127.0.0.1' || hostName === 'localhost' || hostName === '::1' || hostName === '[::1]'
    }
    return false
  }

  /**
   * Scheme and host of an origin string, without a port.
   * @param value - origin such as `dsh-app://app`.
   * @returns `scheme//host`, or '' when it cannot be parsed.
   */
  function parseOriginName(value) {
    try {
      var url = new URL(value)
      return url.protocol + '//' + url.hostname
    } catch (error) {
      return ''
    }
  }

  if (!documentIsBridged()) {
    // Sub frames and foreign documents only see the protocol marker, exactly
    // like the official preload (`{ protocolVersion: 1 }`).
    expose('dshDesktop', Object.freeze({ protocolVersion: 1 }))
    return
  }

  // ---------------------------------------------------------------------------
  // Document markers (`preload-platform.ts`): data-platform and fullscreen.
  // ---------------------------------------------------------------------------

  /**
   * Run `callback` as soon as the document root exists, synchronously when it
   * already does.
   * @param callback - receives the root element.
   */
  function whenDocumentRoot(callback) {
    var document = shellWindow.document
    if (document === undefined || document === null) return
    var run = function () {
      var root = document.documentElement
      if (root === undefined || root === null) return false
      callback(root)
      return true
    }
    if (run()) return
    if (typeof shellWindow.MutationObserver === 'function') {
      try {
        var observer = new shellWindow.MutationObserver(function () {
          if (run()) observer.disconnect()
        })
        observer.observe(document, { childList: true })
      } catch (error) {
        warn('could not watch for the document root', error)
      }
    }
    document.addEventListener('readystatechange', run)
    document.addEventListener('DOMContentLoaded', run)
  }

  /** @returns the platform guess used before Rust confirms it. */
  function guessPlatform() {
    var navigator = shellWindow.navigator
    return platformFromUserAgent(
      navigator === undefined || navigator === null ? '' : navigator.userAgent,
      navigator === undefined || navigator === null ? '' : navigator.platform,
    )
  }

  whenDocumentRoot(function (root) {
    root.dataset.platform = guessPlatform()
  })
  invoke('desktop_platform').then(
    function (value) {
      if (typeof value !== 'string' || value === '') return
      whenDocumentRoot(function (root) {
        if (root.dataset.platform !== value) root.dataset.platform = value
      })
    },
    function (error) { warn('could not read the shell platform', error) },
  )

  listen(EVENT_WINDOW_FULLSCREEN, function (fullscreen) {
    whenDocumentRoot(function (root) {
      if (fullscreen === true) root.dataset.fullscreen = 'true'
      else delete root.dataset.fullscreen
    })
  })

  // ---------------------------------------------------------------------------
  // Native theme mirror (`preload-theme.ts`): the Web UI owns the theme source,
  // the shell applies it to native chrome.
  // ---------------------------------------------------------------------------
  whenDocumentRoot(function (root) {
    var sent = null
    var send = function () {
      var value = root.getAttribute(THEME_SOURCE_ATTRIBUTE)
      if (value === null || value === sent) return
      sent = value
      invoke('desktop_native_theme_set', { source: value }).catch(function (error) {
        warn('could not mirror the theme source', error)
      })
    }
    if (typeof shellWindow.MutationObserver === 'function') {
      try {
        new shellWindow.MutationObserver(send).observe(root, { attributeFilter: [THEME_SOURCE_ATTRIBUTE] })
      } catch (error) {
        warn('could not watch the theme source', error)
      }
    }
    send()
  })

  // ---------------------------------------------------------------------------
  // Workspace handoff: focus the document once the workspace takes over.
  // ---------------------------------------------------------------------------
  listen(EVENT_ENTER_WORKSPACE, function () {
    try {
      var body = shellWindow.document.body
      if (body === null || body === undefined) return
      var previous = body.getAttribute('tabindex')
      body.tabIndex = -1
      body.focus({ preventScroll: true })
      if (previous === null) body.removeAttribute('tabindex')
      else body.setAttribute('tabindex', previous)
    } catch (error) {
      warn('could not focus the workspace document', error)
    }
  })

  // ---------------------------------------------------------------------------
  // Boot contract: `dshDesktopBoot.ready()` returns the Host payload and
  // `failed()` reports a startup failure to the shell.
  // ---------------------------------------------------------------------------
  expose('dshDesktopBoot', Object.freeze({
    ready: function () { return invoke('desktop_boot') },
    failed: function (message) { return invoke('desktop_boot_failed', { message: String(message) }) },
  }))

  // ---------------------------------------------------------------------------
  // Shortcut preferences (`@deepseek-ai/dsh-client-shortcuts/protocol`).
  //
  // The bridge also mirrors the last accepted configuration so it can recognise
  // bound combinations and forward physical key presses as native input: in the
  // desktop runtime the Web UI stops dispatching DOM key events itself, so
  // without this every application shortcut would be inert.
  // ---------------------------------------------------------------------------
  var shortcutRevision = ''
  var shortcutRecording = false
  var shortcutDefinitions = []
  var boundKeys = {}

  /** @returns the protocol platform of this device. */
  function shortcutPlatform() {
    var document = shellWindow.document
    var platform = document === undefined || document === null || document.documentElement === null
      ? ''
      : document.documentElement.dataset.platform
    if (platform === 'darwin') return 'macos'
    if (platform === 'win32') return 'windows'
    if (platform === 'linux') return 'linux'
    var guess = guessPlatform()
    return guess === 'darwin' ? 'macos' : guess === 'win32' ? 'windows' : 'linux'
  }

  /**
   * Stable key for one binding, using the protocol's modifier order.
   * @param binding - declared or normalized binding, or null.
   * @param platform - protocol platform.
   * @returns `modifiers|code(,code)` or null when the binding is unusable.
   */
  function bindingKey(binding, platform) {
    if (binding === null || typeof binding !== 'object') return null
    var code = typeof binding.code === 'string' ? binding.code : ''
    if (code === '') return null
    var declared = Array.isArray(binding.modifiers) ? binding.modifiers : []
    var expanded = {}
    for (var index = 0; index < declared.length; index++) {
      var modifier = declared[index]
      if (modifier === 'primary') expanded[platform === 'macos' ? 'meta' : 'control'] = true
      else if (modifier === 'control' || modifier === 'alt' || modifier === 'shift' || modifier === 'meta') expanded[modifier] = true
    }
    var ordered = []
    for (var position = 0; position < MODIFIER_ORDER.length; position++) {
      if (expanded[MODIFIER_ORDER[position]] === true) ordered.push(MODIFIER_ORDER[position])
    }
    var codes = typeof binding.secondCode === 'string' && binding.secondCode !== ''
      ? [code, binding.secondCode].sort()
      : [code]
    return ordered.join('+') + '|' + codes.join('+')
  }

  /**
   * Rebuild the set of combinations this profile currently binds.
   * @param document_ - accepted `ShortcutDocument`.
   */
  function recomputeBoundKeys(document_) {
    var platform = shortcutPlatform()
    var profile = 'desktop:' + platform
    var overrides = null
    if (document_ !== null && typeof document_ === 'object'
      && document_.profiles !== null && typeof document_.profiles === 'object'
      && Object.prototype.hasOwnProperty.call(document_.profiles, profile)) {
      overrides = document_.profiles[profile]
    }
    var keys = {}
    var definitions = Array.isArray(shortcutDefinitions) ? shortcutDefinitions : []
    for (var index = 0; index < definitions.length; index++) {
      var definition = definitions[index]
      if (definition === null || typeof definition !== 'object' || typeof definition.id !== 'string') continue
      var binding
      if (overrides !== null && overrides !== undefined && Object.prototype.hasOwnProperty.call(overrides, definition.id)) {
        binding = overrides[definition.id]
      } else if (definition.defaults !== null && typeof definition.defaults === 'object') {
        binding = definition.defaults[profile]
      }
      var key = bindingKey(binding === undefined ? null : binding, platform)
      if (key !== null) keys[key] = true
    }
    boundKeys = keys
  }

  /**
   * Remember one accepted snapshot (revision plus bound combinations).
   * @param snapshot - `ShortcutConfigSnapshot`.
   */
  function acceptShortcutSnapshot(snapshot) {
    if (snapshot === null || typeof snapshot !== 'object') return
    if (typeof snapshot.revision === 'string') shortcutRevision = snapshot.revision
    if (snapshot.document !== undefined) recomputeBoundKeys(snapshot.document)
  }

  var shortcutsApi = Object.freeze({
    get: function (definitions) {
      shortcutDefinitions = Array.isArray(definitions) ? definitions : []
      return invoke('desktop_shortcuts_get', { definitions: shortcutDefinitions }).then(function (snapshot) {
        acceptShortcutSnapshot(snapshot)
        return snapshot
      })
    },
    edit: function (edit, revision) {
      return invoke('desktop_shortcuts_edit', { edit: edit, revision: revision }).then(function (result) {
        if (result !== null && typeof result === 'object') acceptShortcutSnapshot(result.snapshot)
        return result
      })
    },
    recording: function (active) {
      shortcutRecording = active === true
      return invoke('desktop_shortcuts_recording', { active: shortcutRecording })
    },
    subscribe: function (listener) {
      return listen(EVENT_SHORTCUTS_CHANGED, function (snapshot) {
        acceptShortcutSnapshot(snapshot)
        if (typeof listener === 'function') listener(snapshot)
      })
    },
  })

  // ---------------------------------------------------------------------------
  // Keyboard input. Official input arrives from Electron's
  // `before-input-event`; the Tauri shell forwards physical presses instead:
  // both the synthesised DOM listener and a shell event feed the same
  // subscribers, and the embedded-frame checks mirror the official preload.
  // ---------------------------------------------------------------------------
  var keyboardListeners = []
  var keyboardDispose = null

  /**
   * Deliver one `DesktopShortcutInput` to every current subscriber.
   * @param input - candidate input from the DOM or from the shell.
   */
  function deliverShortcutInput(input) {
    if (input === null || typeof input !== 'object') return
    var document = shellWindow.document
    if (input.kind === 'iframe') {
      var element = document === undefined || document === null ? null : document.activeElement
      if (!(element instanceof shellWindow.HTMLIFrameElement) || !element.isConnected
        || !element.matches('iframe[data-sidebar-browser-frame], iframe[data-html-preview]')) return
      if (input.frameName === '' || element.name !== input.frameName) return
    }
    if (input.kind === 'webview') {
      var guest = document === undefined || document === null ? null : document.activeElement
      if (guest === null || guest === undefined || guest.matches === undefined
        || guest.matches('webview[data-sidebar-browser-frame]') !== true || !guest.isConnected
        || input.frameName === '' || guest.getAttribute('name') !== input.frameName) return
    }
    var recipients = keyboardListeners.slice()
    for (var index = 0; index < recipients.length; index++) {
      try {
        recipients[index](input)
      } catch (error) {
        console.error('[dsh desktop] keyboard listener failed', error)
      }
    }
  }

  /** @returns whether the listener state must be installed. */
  function installKeyboardSources() {
    if (keyboardDispose !== null) return
    var document = shellWindow.document
    var offShell = listen(EVENT_SHORTCUTS_INPUT, deliverShortcutInput)
    var onKeydown = function (event) {
      try {
        if (shortcutRecording) return
        if (event.defaultPrevented === true) return
        if (event.isComposing === true || event.key === 'Dead') return
        if (typeof event.getModifierState === 'function' && event.getModifierState('AltGraph')) return
        var code = typeof event.code === 'string' ? event.code : ''
        if (code === '') return
        var modified = event.ctrlKey === true || event.altKey === true || event.metaKey === true
        if (!modified && !/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) return
        var ordered = []
        if (event.ctrlKey === true) ordered.push('control')
        if (event.altKey === true) ordered.push('alt')
        if (event.shiftKey === true) ordered.push('shift')
        if (event.metaKey === true) ordered.push('meta')
        var key = ordered.join('+') + '|' + code
        if (boundKeys[key] === true) {
          try {
            event.preventDefault()
          } catch (ignored) {
            // A passive listener cannot cancel; the command still runs.
          }
        }
        deliverShortcutInput({
          kind: 'keyboard',
          frameName: '',
          code: code,
          control: event.ctrlKey === true,
          alt: event.altKey === true,
          shift: event.shiftKey === true,
          meta: event.metaKey === true,
          repeat: event.repeat === true,
          revision: shortcutRevision,
        })
      } catch (error) {
        warn('could not forward a key press', error)
      }
    }
    if (document !== undefined && document !== null && typeof document.addEventListener === 'function') {
      document.addEventListener('keydown', onKeydown, true)
    }
    keyboardDispose = function () {
      offShell()
      if (document !== undefined && document !== null && typeof document.removeEventListener === 'function') {
        document.removeEventListener('keydown', onKeydown, true)
      }
    }
  }

  var keyboardApi = Object.freeze({
    closeWindow: function (revision) {
      return invoke('desktop_shortcuts_close_window', { revision: String(revision) })
    },
    subscribe: function (listener) {
      if (typeof listener !== 'function') return function () {}
      keyboardListeners.push(listener)
      try {
        installKeyboardSources()
      } catch (error) {
        warn('could not install the keyboard source', error)
      }
      var active = true
      return function () {
        if (!active) return
        active = false
        var index = keyboardListeners.indexOf(listener)
        if (index >= 0) keyboardListeners.splice(index, 1)
        if (keyboardListeners.length === 0 && keyboardDispose !== null) {
          var dispose = keyboardDispose
          keyboardDispose = null
          try {
            dispose()
          } catch (error) {
            warn('could not remove the keyboard source', error)
          }
        }
      }
    },
  })

  // ---------------------------------------------------------------------------
  // Release updates: the shell owns checking, the Web UI only presents it.
  // ---------------------------------------------------------------------------
  var updatesApi = Object.freeze({
    status: function () { return invoke('desktop_updates_status') },
    open: function () { return invoke('desktop_updates_open') },
    subscribe: function (listener) {
      return listen(EVENT_UPDATES_PRESENTATION, function (state) {
        if (typeof listener === 'function') listener(state)
      })
    },
  })

  // ---------------------------------------------------------------------------
  // File drag: P3. Tauri exposes no drag-out controller yet, so the members
  // answer with the values the Web UI sees while a preparation is unavailable.
  // They never throw, and a missing answer simply means "no drag handle".
  // ---------------------------------------------------------------------------
  var fileDragApi = Object.freeze({
    /** @returns a promise for no handle (P3: no native drag session). */
    prepare: function () { return Promise.resolve(undefined) },
    /** No-op: nothing was prepared (P3). */
    cancel: function () {},
    /** @returns a promise for "not started" (P3). */
    start: function () { return Promise.resolve(false) },
    /** No-op: nothing to release (P3). */
    release: function () {},
  })

  // ---------------------------------------------------------------------------
  // Embedded browser guests: P3. The shape matches
  // `@deepseek-ai/dsh-client-ui-sidebar-browser`, but it is only attached when
  // the shell sets `__DSH_BRIDGE_CAPS__.browserGuests`, because merely
  // publishing `dshDesktop.browser` makes the Web UI drop its working
  // iframe-based browser pane in favour of native guests.
  // ---------------------------------------------------------------------------
  function createBrowserBridge() {
    var leases = {}
    listen(EVENT_BROWSER_OPEN_REQUESTED, function (request) {
      if (request === null || typeof request !== 'object') return
      if (typeof request.lease !== 'string' || typeof request.url !== 'string') return
      var callbacks = leases[request.lease]
      if (callbacks === undefined) return
      var recipients = callbacks.slice()
      for (var index = 0; index < recipients.length; index++) {
        try {
          recipients[index](request.url)
        } catch (error) {
          console.error('[dsh desktop] browser link handler failed', error)
        }
      }
    })
    return Object.freeze({
      acquire: function (workspace) { return invoke('desktop_browser_acquire', { workspace: workspace }) },
      release: function (lease) { return invoke('desktop_browser_release', { lease: String(lease) }) },
      onOpenRequested: function (lease, listener) {
        var callbacks = leases[lease]
        if (callbacks === undefined) {
          callbacks = []
          leases[lease] = callbacks
        }
        callbacks.push(listener)
        var active = true
        return function () {
          if (!active) return
          active = false
          var index = callbacks.indexOf(listener)
          if (index >= 0) callbacks.splice(index, 1)
          if (callbacks.length === 0 && leases[lease] === callbacks) delete leases[lease]
        }
      },
    })
  }

  /** @returns the shell-provided capability flags. */
  function capabilities() {
    var flags = shellWindow.__DSH_BRIDGE_CAPS__
    return flags === null || typeof flags !== 'object' ? {} : flags
  }

  // ---------------------------------------------------------------------------
  // The product facing API (`DshDesktopProductApi`).
  // ---------------------------------------------------------------------------
  var productApi = {}
  productApi.protocolVersion = 1
  productApi.fileDrag = fileDragApi
  productApi.keyboard = keyboardApi
  productApi.shortcuts = shortcutsApi
  productApi.updates = updatesApi
  /** @returns a `name=value; …` machine description without identifiers. */
  productApi.deviceInfo = function () { return invoke('desktop_device_info') }
  if (capabilities().browserGuests === true) productApi.browser = createBrowserBridge()
  expose('dshDesktop', Object.freeze(productApi))

  // ---------------------------------------------------------------------------
  // Desktop login (onboarding) and native directory selection.
  // ---------------------------------------------------------------------------
  expose('dshOnboarding', Object.freeze({
    hasApiKey: function () { return invoke('desktop_onboarding_api_key') },
    setActive: function (active) {
      invoke('desktop_onboarding_active', { active: active === true }).catch(function (error) {
        warn('could not mirror the onboarding state', error)
      })
    },
  }))

  expose('__DSH_DIRECTORY_PICKER__', Object.freeze({
    /** @returns the picked absolute path, or null when the user cancelled. */
    pick: function () { return invoke('desktop_directory_pick') },
  }))

  expose('__DSH_HOST_PATHS__', Object.freeze({
    /**
     * Tauri has no `webUtils.getPathForFile`: a dropped or pasted `File` has no
     * host path inside the page, so the composer uploads it instead of citing
     * it as an `@path` reference (documented difference from Electron).
     * @returns always the empty string.
     */
    pathFor: function () { return '' },
  }))

  // ---------------------------------------------------------------------------
  // Platform login pages: P3 stubs. They answer with promises so the overlay
  // that owns them can settle instead of hanging.
  // ---------------------------------------------------------------------------
  expose('dshPlatform', Object.freeze({
    open: function (page, bounds) { return invoke('desktop_platform_open', { page: page, bounds: bounds }) },
    setBounds: function (bounds) { return invoke('desktop_platform_bounds', { bounds: bounds }) },
    close: function () { return invoke('desktop_platform_close') },
  }))

  // ---------------------------------------------------------------------------
  // Language bootstrap: the shell reports the OS languages and the stored
  // preference, and remembers a later choice.
  // ---------------------------------------------------------------------------
  expose('__DSH_LOCALE__', Object.freeze({
    read: function () { return invoke('desktop_locale_bootstrap') },
    onChange: function (locale) {
      invoke('desktop_locale_changed', { locale: String(locale) }).catch(function (error) {
        warn('could not store the locale preference', error)
      })
    },
  }))

  // ---------------------------------------------------------------------------
  // Dynamic traffic rewrite.
  //
  // The page origin is `dsh-app://app` (static dist) or a loopback origin, and
  // WebView scheme-handler responses are buffered, so relative and app-origin
  // requests are sent to the loopback app server instead. WebSocket traffic is
  // not rewritten: the Web UI connects to `streamBaseUrl`, which the shell
  // already reports from `dshDesktopBoot.ready()`.
  // ---------------------------------------------------------------------------
  var injectedBase = (function () {
    try {
      var value = shellWindow.__DSH_APP_BASE__
      return typeof value === 'string' && parseBase(value) !== null ? value.trim() : null
    } catch (error) {
      return null
    }
  })()
  var cachedBase = null
  var basePromise = null

  /** @returns the app-server base known without awaiting. */
  function currentBase() {
    if (injectedBase !== null) return injectedBase
    return cachedBase
  }

  /**
   * Resolve the loopback app-server base.
   * @returns a promise for `http://127.0.0.1:<port>`, rejecting when unknown.
   */
  function resolveBase() {
    var immediate = currentBase()
    if (immediate !== null) return Promise.resolve(immediate)
    if (basePromise === null) {
      basePromise = invoke('desktop_app_base').then(function (value) {
        if (parseBase(typeof value === 'string' ? value : '') === null) {
          throw new Error('dsh desktop: the app server is unavailable')
        }
        cachedBase = String(value).trim()
        return cachedBase
      })
      basePromise.catch(function () {
        basePromise = null
        cachedBase = null
      })
    }
    return basePromise
  }

  /** @returns whether `input` is a `Request`. */
  function isRequest(input) {
    return typeof shellWindow.Request === 'function' && input instanceof shellWindow.Request
  }

  /**
   * The original `fetch`, captured before the rewrite is installed so nothing
   * the bridge (or a diagnostic hook) does later can be rewritten by itself.
   */
  var nativeFetch = null
  /** The original `XMLHttpRequest` constructor, captured the same way. */
  var nativeXhrConstructor = null

  /**
   * Request options for the rewritten request.
   *
   * A rewritten URL is cross-origin (`dsh-app://app` document, loopback app
   * server), so `mode` becomes `cors`: `same-origin`/`navigate` are rejected for
   * a cross-origin URL, and `no-cors` would hand the page an opaque, unreadable
   * response. `only-if-cached` only exists with `same-origin`, so it is dropped.
   * @param request - original request.
   * @param init - overrides supplied to `fetch`.
   * @returns the merged init dictionary.
   */
  function requestOptions(request, init) {
    var source = init !== null && typeof init === 'object' ? init : {}
    var options = {}
    var keys = ['method', 'headers', 'redirect', 'referrerPolicy', 'integrity', 'keepalive', 'signal']
    for (var index = 0; index < keys.length; index++) {
      var key = keys[index]
      if (source[key] !== undefined) options[key] = source[key]
      else if (request[key] !== undefined) options[key] = request[key]
    }
    options.mode = 'cors'
    options.credentials = source.credentials !== undefined ? source.credentials : request.credentials
    var cache = source.cache !== undefined ? source.cache : request.cache
    if (cache !== undefined && cache !== 'only-if-cached') options.cache = cache
    return options
  }

  /**
   * Rebuild a `Request` against the rewritten URL.
   *
   * A `ReadableStream` body cannot be handed to a new `Request` in this WebView
   * (`NotSupportedError: ReadableStream uploading is not supported`), and that
   * rejected attempt consumes the caller's one-shot body, so there is no
   * "try streaming, then retry" path. The body is materialised from a clone
   * instead: the original request stays usable for the fallback below, at the
   * cost of holding the body in memory. Large uploads belong on
   * `XMLHttpRequest`, which streams natively.
   * @param request - original request.
   * @param url - rewritten URL.
   * @param init - overrides supplied to `fetch`.
   * @returns a promise for the rewritten request.
   */
  function rebuildRequest(request, url, init) {
    var source = init !== null && typeof init === 'object' ? init : {}
    var options = requestOptions(request, init)
    if (source.body !== undefined && source.body !== null) {
      options.body = source.body
      return Promise.resolve(new shellWindow.Request(url, options))
    }
    var method = String(options.method === undefined ? 'GET' : options.method).toUpperCase()
    if (request.body === null || request.bodyUsed || method === 'GET' || method === 'HEAD') {
      return Promise.resolve(new shellWindow.Request(url, options))
    }
    var clone
    try {
      clone = request.clone()
    } catch (error) {
      return Promise.reject(error)
    }
    return clone.arrayBuffer().then(function (buffer) {
      if (buffer.byteLength > 0) options.body = buffer
      return new shellWindow.Request(url, options)
    })
  }

  /** Installs the `fetch` rewrite; failures leave the page untouched. */
  function installFetchShim() {
    var originalFetch = shellWindow.fetch
    if (typeof originalFetch !== 'function') return
    nativeFetch = function (input, init) {
      return init === undefined
        ? originalFetch.call(shellWindow, input)
        : originalFetch.call(shellWindow, input, init)
    }
    var replacement = function (input, init) {
      var passThrough = function () { return nativeFetch(input, init) }
      try {
        if (isRequest(input)) {
          // `fetch(new Request('dsh-app://app/…'))` must be rewritten as well.
          if (!needsRewrite(input.url)) return passThrough()
          return resolveBase().then(
            function (base) {
              return rebuildRequest(input, rewriteUrl(input.url, base), init).then(
                function (next) { return nativeFetch(next) },
                function (error) {
                  warn('could not rebuild a Request; sending it unchanged', error)
                  return passThrough()
                },
              )
            },
            function (error) {
              warn('the app server base is unavailable; sending the page-origin URL', error)
              return passThrough()
            },
          )
        }
        var raw = typeof shellWindow.URL === 'function' && input instanceof shellWindow.URL
          ? input.href
          : String(input)
        if (!needsRewrite(raw)) return passThrough()
        var base = currentBase()
        if (base !== null) return nativeFetch(rewriteUrl(raw, base), init)
        return resolveBase().then(
          function (value) { return nativeFetch(rewriteUrl(raw, value), init) },
          function (error) {
            warn('the app server base is unavailable; sending the page-origin URL', error)
            return passThrough()
          },
        )
      } catch (error) {
        warn('could not rewrite a fetch request', error)
        return passThrough()
      }
    }
    try {
      shellWindow.fetch = replacement
    } catch (error) {
      try {
        Object.defineProperty(shellWindow, 'fetch', { value: replacement, configurable: true, writable: true })
      } catch (denied) {
        warn('could not install the fetch rewrite', denied)
      }
    }
  }

  /** Installs the `XMLHttpRequest` rewrite; failures leave the page untouched. */
  function installXhrShim() {
    var Constructor = shellWindow.XMLHttpRequest
    if (typeof Constructor !== 'function' || Constructor.prototype === undefined) return
    nativeXhrConstructor = Constructor
    var proto = Constructor.prototype
    var originalOpen = proto.open
    var originalSend = proto.send
    var originalSetRequestHeader = proto.setRequestHeader
    if (typeof originalOpen !== 'function' || typeof originalSend !== 'function') return
    if (typeof shellWindow.WeakMap !== 'function') return
    var pending = new shellWindow.WeakMap()

    /**
     * Run the real `open` and replay buffered request headers.
     * @param xhr - the instance.
     * @param record - buffered call state.
     * @param url - URL to open.
     */
    function applyOpen(xhr, record, url) {
      var args = [record.method, url]
      if (record.arity > 2) args.push(record.async)
      if (record.arity > 3) args.push(record.user)
      if (record.arity > 4) args.push(record.password)
      originalOpen.apply(xhr, args)
      record.opened = true
      for (var index = 0; index < record.headers.length; index++) {
        try {
          originalSetRequestHeader.apply(xhr, record.headers[index])
        } catch (error) {
          warn('could not replay a request header', error)
        }
      }
      record.headers = []
    }

    try {
      proto.open = function (method, url, async, user, password) {
        var record = {
          method: method,
          url: String(url),
          arity: arguments.length,
          async: async,
          user: user,
          password: password,
          headers: [],
          opened: false,
        }
        pending.set(this, record)
        // A synchronous request cannot wait for an asynchronous base.
        if (arguments.length > 2 && async === false) {
          var fallback = currentBase()
          applyOpen(this, record, fallback === null ? record.url : rewriteUrl(record.url, fallback))
          return undefined
        }
        var base = currentBase()
        if (base !== null) applyOpen(this, record, rewriteUrl(record.url, base))
        return undefined
      }
    } catch (error) {
      warn('could not install the XHR open rewrite', error)
      return
    }

    if (typeof originalSetRequestHeader === 'function') {
      try {
        proto.setRequestHeader = function (name, value) {
          var record = pending.get(this)
          if (record !== undefined && record.opened === false) {
            record.headers.push([name, value])
            return undefined
          }
          return originalSetRequestHeader.apply(this, arguments)
        }
      } catch (error) {
        warn('could not install the XHR header buffer', error)
      }
    }

    try {
      proto.send = function (body) {
        var record = pending.get(this)
        if (record === undefined || record.opened === true) return originalSend.call(this, body)
        var xhr = this
        var finish = function (base) {
          if (record.opened) return
          try {
            applyOpen(xhr, record, base === null ? record.url : rewriteUrl(record.url, base))
            originalSend.call(xhr, body)
          } catch (error) {
            warn('could not send a rewritten XHR', error)
          }
        }
        resolveBase().then(finish, function (error) {
          warn('the app server base is unavailable, sending the page-origin URL', error)
          finish(null)
        })
        return undefined
      }
    } catch (error) {
      warn('could not install the XHR send rewrite', error)
    }
  }

  try {
    installFetchShim()
  } catch (error) {
    warn('the fetch rewrite is unavailable', error)
  }
  try {
    installXhrShim()
  } catch (error) {
    warn('the XHR rewrite is unavailable', error)
  }

  // Escape hatch: the shell's own diagnostics (and the Web UI, while debugging)
  // can still reach the native APIs, e.g. to call the scheme handler directly.
  expose('__DSH_BRIDGE_ESCAPE__', Object.freeze({
    /** @returns the unrewritten `fetch`, bound to this window. */
    nativeFetch: function (input, init) {
      if (nativeFetch === null) return Promise.reject(new Error('dsh desktop: fetch is unavailable'))
      return nativeFetch(input, init)
    },
    /** @returns the unrewritten `XMLHttpRequest` constructor, or null. */
    nativeXhr: function () { return nativeXhrConstructor },
    /** @returns the app-server base this document is rewritten onto, or null. */
    appBase: function () { return currentBase() },
  }))

  // Page diagnostics: forward console warnings/errors and uncaught failures to
  // the shell log. A release build has no inspector, so without this a failing
  // plugin (for example the shortcuts service) would only fail silently.
  try {
    var nativeConsoleWarn = console.warn
    var nativeConsoleError = console.error
    var forwardConsole = function (level, args) {
      try {
        var parts = []
        for (var index = 0; index < args.length; index += 1) {
          var value = args[index]
          if (typeof value === 'string') {
            parts.push(value)
          } else {
            try {
              parts.push(JSON.stringify(value))
            } catch (ignored) {
              parts.push(String(value))
            }
          }
        }
        void invoke('desktop_console', { level: level, message: parts.join(' ') })
          .catch(function () {})
      } catch (ignored) {
        // Logging must never break the page.
      }
    }
    console.warn = function () {
      forwardConsole('warn', arguments)
      return nativeConsoleWarn.apply(console, arguments)
    }
    console.error = function () {
      forwardConsole('error', arguments)
      return nativeConsoleError.apply(console, arguments)
    }
    window.addEventListener('error', function (event) {
      void invoke('desktop_console', {
        level: 'error',
        message: 'uncaught: ' + String(event.message || '') + ' @ ' + String(event.filename || ''),
      }).catch(function () {})
    })
    window.addEventListener('unhandledrejection', function (event) {
      var reason = event.reason
      var text = reason && reason.message ? reason.message : reason
      void invoke('desktop_console', {
        level: 'error',
        message: 'unhandled rejection: ' + String(text),
      }).catch(function () {})
    })
  } catch (error) {
    // A page that replaced `console` must not take the bridge down.
  }
})()
