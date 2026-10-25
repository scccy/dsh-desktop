/**
 * DSH Desktop 兼容 shim：EventSource 重写 + 原生麦克风桥。
 *
 * 由来（两件 WKWebView 能力缺口，见 docs/spikes/wkwebview-scheme.md 与
 * docs/spikes/media-input.md）：
 *
 *  1. `dsh-app:` scheme 的响应会被 WebView 整体缓冲（spike Q3），所以相对路径与
 *     `dsh-app://app/...` 的 `EventSource` 必须改写到回环 app server。`preload.js`
 *     只重写了 `fetch` / `XMLHttpRequest`（spike §5-F 当时的结论是「EventSource 只要
 *     URL 是绝对回环地址就不用包装」），但前端开发期确实用 `new EventSource('/api/…')`，
 *     相对路径会打到 scheme handler 上被缓冲 —— 这一节补的就是这个重写。
 *  2. `navigator.mediaDevices` 在 macOS WKWebView 上**不存在**（Media Capture 未实现），
 *     而语音输入插件（`@deepseek-ai/dsh-experimental-client-ui-voice-input`）只认
 *     `getUserMedia` + `MediaRecorder`。所以这里用「原生采集（Rust AVCaptureSession）→
 *     PCM 帧 → ScriptProcessorNode → MediaStreamAudioDestinationNode.stream」合成一个
 *     MediaStream，冒充 `getUserMedia()` 的返回值，前端零改动。
 *
 * 注入方式：与 `preload.js` 同一个 `initialization_script`，**排在 preload.js 之后**
 * （要复用 `__DSH_BRIDGE_ESCAPE__.appBase()` 与 `__TAURI_INTERNALS__`），页面前执行。
 *
 * 两节都做了「失败即静默降级」：任何一步异常都只 `console.warn`，绝不打断页面启动。
 */
;(function () {
  'use strict'

  /** Desktop 文档的 scheme 与 host。 */
  var APP_SCHEME = 'dsh-app:'
  var APP_HOST = 'app'
  /** 任意 RFC 3986 scheme 前缀。 */
  var SCHEME_PREFIX = /^[a-zA-Z][a-zA-Z0-9+.\-]*:/
  /** 麦克风通道首帧的协议版本。 */
  var MIC_PROTOCOL = 1

  // ---------------------------------------------------------------------------
  // 纯函数区（可被 bridge/media-shim.test.cjs 直接 require 测试）。
  //
  // `parseBase` / `rewriteTarget` / `rewriteUrl` 与 `preload.js` 里同名函数必须
  // **完全一致**：两处各有一份是刻意的（preload.js 不导出自己的内部实现，而
  // initialization_script 无法 import），测试里用同一组用例交叉校验。
  // ---------------------------------------------------------------------------

  /**
   * 解析回环 app server 基址（`http://127.0.0.1:<port>`）。
   * @param value - 候选基址。
   * @returns 解析后的 URL；不是 http(s) origin 时返回 null。
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
   * 判断一个 URL 是否属于应用文档（相对路径按 `dsh-app://app/` 解析）。
   * @param raw - 请求 URL，绝对或相对。
   * @returns 解析后的应用 origin URL；不属于我们时返回 null。
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
   * 是否需要改写：相对路径与 `dsh-app://app` URL。
   * @param raw - 请求 URL。
   * @returns 布尔值。
   */
  function needsRewrite(raw) {
    return rewriteTarget(raw) !== null
  }

  /**
   * 把页面 origin 的流量改写到回环 app server。
   *
   * 绝对 `http(s)://` / `blob:` / `data:` 原样放行（直连 Host 或对象 URL 仍然要能走）；
   * 相对路径与 `dsh-app://app/...` 变成 `base + pathname + search + hash`。
   * @param input - 请求 URL（其它形态由调用方先 String 化）。
   * @param base - 形如 `http://127.0.0.1:54321` 的基址。
   * @returns 改写后的 URL；不适用时返回原字符串。
   */
  function rewriteUrl(input, base) {
    var raw = typeof input === 'string' ? input : String(input)
    if (raw === '') return raw
    var baseUrl = parseBase(base)
    if (baseUrl === null) return raw
    var target = rewriteTarget(raw)
    if (target === null) return raw
    // 非特殊 scheme 可能解析出空 path（`dsh-app://app`）。
    var path = target.pathname === '' ? '/' : target.pathname
    return baseUrl.origin + path + target.search + target.hash
  }

  /**
   * EventSource 专用的 URL 改写。
   *
   * 语义与 `rewriteUrl` 相同，单独留一个名字是为了让测试与调用点能表达
   * 「这里只做 URL 改写，不做流代理」：EventSource 的流式语义完全交给原生实现。
   * @param input - EventSource 的 URL。
   * @param base - 回环基址。
   * @returns 改写后的 URL；不适用时返回原字符串。
   */
  function rewriteEventSourceUrl(input, base) {
    return rewriteUrl(input, base)
  }

  /**
   * 归一化通道消息。
   *
   * Tauri 的 `Channel` 在不同负载大小/来源下会把消息包成不同形状：
   *  - 小的 JSON/二进制走 `webview.eval`，回调收到 `{ message, index }`；
   *  - 大的负载走内部 fetch 通道，回调直接收到 ArrayBuffer（或已解析的 JSON）；
   *  - 旧实现还会把 `Uint8Array` 直接递过来。
   * 这里统一成 `{ kind:'json'|'bytes', value }`，让上层只关心两种形态。
   * @param payload - 回调收到的原始值。
   * @returns `{ kind, value }`；无法识别时 `{ kind:'unknown', value }`。
   */
  function decodeChannelMessage(payload) {
    var value = payload
    // `{ message, index }` 包装：只解一层，避免把业务对象里的 `message` 字段当包装。
    if (value !== null && typeof value === 'object' && !isBinary(value) && 'message' in value && 'index' in value) {
      value = value.message
    }
    if (isBinary(value)) return { kind: 'bytes', value: toFloatBytes(value) }
    if (typeof value === 'string') {
      try {
        return { kind: 'json', value: JSON.parse(value) }
      } catch (error) {
        return { kind: 'unknown', value: value }
      }
    }
    if (value !== null && typeof value === 'object') return { kind: 'json', value: value }
    return { kind: 'unknown', value: value }
  }

  /**
   * 是否是二进制负载（ArrayBuffer 或其视图）。
   * @param value - 待判断值。
   * @returns 布尔值。
   */
  function isBinary(value) {
    if (value === null || typeof value !== 'object') return false
    if (typeof ArrayBuffer === 'function' && value instanceof ArrayBuffer) return true
    return typeof ArrayBuffer === 'function' && typeof ArrayBuffer.isView === 'function' && ArrayBuffer.isView(value)
  }

  /**
   * 把二进制负载统一成 `Float32Array`（Rust 侧发的是小端 f32 单声道）。
   * @param value - ArrayBuffer 或视图。
   * @returns Float32Array（可能共享原缓冲区，只读使用）。
   */
  function toFloatBytes(value) {
    if (typeof Float32Array !== 'function') return value
    if (value instanceof Float32Array) return value
    var buffer = value
    var byteOffset = 0
    var byteLength = 0
    if (typeof ArrayBuffer.isView === 'function' && ArrayBuffer.isView(value)) {
      buffer = value.buffer
      byteOffset = value.byteOffset
      byteLength = value.byteLength
    } else if (value instanceof ArrayBuffer) {
      byteLength = value.byteLength
    } else {
      return value
    }
    // 声道/对齐问题：f32 需要 4 字节对齐，否则退回按字节视图处理（上层会丢弃）。
    if (byteOffset % 4 !== 0 || byteLength % 4 !== 0) return value
    return new Float32Array(buffer, byteOffset, byteLength / 4)
  }

  /**
   * 把 `getUserMedia({ audio })` 的约束归一化成原生选项。
   *
   * 插件只传 `{ echoCancellation, noiseSuppression, deviceId:{exact} }` 和 `video:false`；
   * 其它浏览器字段（`sampleRate` / `channelCount` / `autoGainControl`）按「能传就传」处理，
   * 但**请求视频一律拒绝**：WKWebView 没有摄像头采集，静默返回一个假视频轨只会让前端更晚出错。
   * @param constraints - `getUserMedia` 的约束对象。
   * @returns `{ ok, reason?, options }`。
   */
  function normalizeAudioConstraints(constraints) {
    var source = constraints !== null && typeof constraints === 'object' ? constraints : {}
    if (source.video !== undefined && source.video !== null && source.video !== false) {
      return { ok: false, reason: 'unsupported-video', options: {} }
    }
    if (source.audio === false || source.audio === undefined || source.audio === null) {
      return { ok: false, reason: 'unsupported-audio-absent', options: {} }
    }
    var audio = typeof source.audio === 'object' ? source.audio : {}
    var options = {}
    var deviceId = audio.deviceId
    if (typeof deviceId === 'string') {
      // 空串就是「系统默认」，不进 options（Rust 侧同样把空串当默认）。
      if (deviceId !== '') options.deviceId = deviceId
    } else if (deviceId !== null && typeof deviceId === 'object') {
      // `{ exact }` 优先，其次 `{ ideal }`；两者都没有就是「系统默认」。
      if (typeof deviceId.exact === 'string' && deviceId.exact !== '') options.deviceId = deviceId.exact
      else if (typeof deviceId.ideal === 'string' && deviceId.ideal !== '') options.deviceId = deviceId.ideal
    }
    if (audio.echoCancellation === true) options.echoCancellation = true
    if (audio.noiseSuppression === true) options.noiseSuppression = true
    if (audio.autoGainControl === true) options.autoGainControl = true
    if (typeof audio.sampleRate === 'number' && isFinite(audio.sampleRate) && audio.sampleRate > 0) {
      options.sampleRate = Math.round(audio.sampleRate)
    }
    if (typeof audio.channelCount === 'number' && isFinite(audio.channelCount) && audio.channelCount > 0) {
      options.channelCount = Math.round(audio.channelCount)
    }
    return { ok: true, options: options }
  }

  /**
   * 把 Rust 侧失败原因映射成 `DOMException` 名字。
   *
   * 插件按名字分流：`NotAllowedError` → 权限被拒，`NotFoundError`/`OverconstrainedError`
   * → 设备缺失，其余原样抛出（会被当成未知错误）。所以这儿必须与插件预期一致。
   * @param reason - Rust 侧 `{state:'denied'|…}` 或错误字符串。
   * @returns `{ name, message }`。
   */
  function failureFor(reason) {
    var text = typeof reason === 'string' ? reason : ''
    switch (text) {
      case 'denied':
      case 'permission':
      case 'restricted':
        return { name: 'NotAllowedError', message: 'dsh desktop: microphone access was denied' }
      case 'device-missing':
      case 'not-found':
        return { name: 'NotFoundError', message: 'dsh desktop: no microphone is available' }
      case 'bad-device':
      case 'overconstrained':
        return { name: 'OverconstrainedError', message: 'dsh desktop: the selected microphone is unavailable' }
      case 'busy':
      case 'not-readable':
        return { name: 'NotReadableError', message: 'dsh desktop: the microphone is in use' }
      case 'unsupported':
      case 'unavailable':
        return { name: 'NotSupportedError', message: 'dsh desktop: native microphone capture is unavailable' }
      case 'cancelled':
      case 'aborted':
        return { name: 'AbortError', message: 'dsh desktop: microphone capture was cancelled' }
      default:
        return { name: 'NotReadableError', message: text === '' ? 'dsh desktop: microphone capture failed' : text }
    }
  }

  /**
   * 把 Rust 侧设备清单归一化成 `MediaDeviceInfo` 形状。
   *
   * 插件的行为：`enumerateDevices()` 里找 `deviceId === 'default'` 当系统默认项，
   * 再按 `groupId` 反查真实设备名。所以默认项必须存在，且真实设备要与默认项同 groupId。
   * @param devices - Rust 侧 `[{ id, label, groupId, isDefault }]`。
   * @returns `[{ deviceId, groupId, kind, label }]`。
   */
  function deviceListFromStatus(devices) {
    var list = []
    var items = Array.isArray(devices) ? devices : []
    var defaultLabel = ''
    for (var index = 0; index < items.length; index++) {
      var item = items[index]
      if (item === null || typeof item !== 'object') continue
      if (item.isDefault === true && defaultLabel === '') defaultLabel = String(item.label || '')
    }
    if (defaultLabel === '' && items.length > 0 && items[0] !== null && typeof items[0] === 'object') {
      defaultLabel = String(items[0].label || '')
    }
    list.push({ deviceId: 'default', groupId: 'default', kind: 'audioinput', label: defaultLabel })
    for (var position = 0; position < items.length; position++) {
      var device = items[position]
      if (device === null || typeof device !== 'object') continue
      var id = typeof device.id === 'string' ? device.id : ''
      if (id === '' || id === 'default' || id === 'communications') continue
      list.push({
        deviceId: id,
        groupId: typeof device.groupId === 'string' && device.groupId !== '' ? device.groupId : 'default',
        kind: 'audioinput',
        label: String(device.label || ''),
      })
    }
    return list
  }

  /**
   * PCM 环形缓冲。
   *
   * 采集端（Rust，20 ms 一帧）与消费端（ScriptProcessorNode，21.3 ms 一块）时钟不同源，
   * 必须有个小缓冲吸收抖动：欠载补静音、积压过头丢最旧的数据（宁可丢一小段，也不要让
   * 延迟一路涨到几百毫秒）。所有计数器都暴露给诊断接口。
   */
  function PcmRing(capacityFrames) {
    var capacity = typeof capacityFrames === 'number' && capacityFrames > 0 ? Math.floor(capacityFrames) : 0
    this.capacity = capacity
    this.buffer = capacity > 0 && typeof Float32Array === 'function' ? new Float32Array(capacity) : null
    this.start = 0
    this.count = 0
    /** push 时因溢出被丢弃的帧数。 */
    this.dropped = 0
    /** pull 时缺口长度累计（补静音的帧数）。 */
    this.starved = 0
  }

  /**
   * 写入一帧。
   * @param frame - Float32Array（单声道）。
   * @returns 实际写入的帧数。
   */
  PcmRing.prototype.push = function (frame) {
    if (this.buffer === null || frame === null || frame === undefined || frame.length === 0) return 0
    var incoming = frame.length
    if (incoming >= this.capacity) {
      // 单帧就超过整个缓冲：只保留最后 capacity 帧。
      this.dropped += this.count + (incoming - this.capacity)
      this.buffer.set(frame.subarray(incoming - this.capacity), 0)
      this.start = 0
      this.count = this.capacity
      return this.capacity
    }
    var overflow = this.count + incoming - this.capacity
    if (overflow > 0) {
      this.start = (this.start + overflow) % this.capacity
      this.count -= overflow
      this.dropped += overflow
    }
    var write = (this.start + this.count) % this.capacity
    var tail = this.capacity - write
    if (incoming <= tail) {
      this.buffer.set(frame, write)
    } else {
      this.buffer.set(frame.subarray(0, tail), write)
      this.buffer.set(frame.subarray(tail), 0)
    }
    this.count += incoming
    return incoming
  }

  /**
   * 取一帧到目标数组，不足部分补静音。
   * @param target - 目标 Float32Array。
   * @returns 实际取自缓冲的帧数（0 表示整块欠载）。
   */
  PcmRing.prototype.pull = function (target) {
    if (target === null || target === undefined || target.length === 0) return 0
    var wanted = target.length
    var available = Math.min(this.count, wanted)
    if (available > 0 && this.buffer !== null) {
      var first = Math.min(available, this.capacity - this.start)
      target.set(this.buffer.subarray(this.start, this.start + first), 0)
      if (available > first) target.set(this.buffer.subarray(0, available - first), first)
      this.start = (this.start + available) % this.capacity
      this.count -= available
    }
    if (available < wanted) {
      target.fill(0, available)
      this.starved += wanted - available
    }
    return available
  }

  /** @returns 当前积压帧数。 */
  PcmRing.prototype.available = function () {
    return this.count
  }

  /** 清空缓冲并复位计数器。 */
  PcmRing.prototype.reset = function () {
    this.start = 0
    this.count = 0
    this.dropped = 0
    this.starved = 0
  }

  // 纯函数导出：作为 CommonJS 载入（测试）时导出，作为 initialization_script
  // 注入时 `module` 不存在，直接跳过。
  if (typeof module === 'object' && module !== null && typeof module.exports === 'object') {
    module.exports = {
      APP_SCHEME: APP_SCHEME,
      APP_HOST: APP_HOST,
      MIC_PROTOCOL: MIC_PROTOCOL,
      parseBase: parseBase,
      needsRewrite: needsRewrite,
      rewriteUrl: rewriteUrl,
      rewriteEventSourceUrl: rewriteEventSourceUrl,
      decodeChannelMessage: decodeChannelMessage,
      normalizeAudioConstraints: normalizeAudioConstraints,
      failureFor: failureFor,
      deviceListFromStatus: deviceListFromStatus,
      PcmRing: PcmRing,
    }
  }

  // ---------------------------------------------------------------------------
  // 运行期：只有 Desktop 自己的文档才装 shim（子 frame / 外部页面什么都不做）。
  // ---------------------------------------------------------------------------
  var shellWindow = typeof window === 'object' && window !== null ? window : null
  if (shellWindow === null) return
  var internals = shellWindow.__TAURI_INTERNALS__
  if (internals === undefined || internals === null || typeof internals.invoke !== 'function') return

  var mainFrame = true
  try {
    mainFrame = shellWindow.top === shellWindow
  } catch (error) {
    mainFrame = false
  }
  if (!mainFrame) return

  /** 诊断/降级用日志；绝不抛。 */
  function warn(message, error) {
    try {
      if (error === undefined) console.warn('[dsh desktop media] ' + message)
      else console.warn('[dsh desktop media] ' + message, error)
    } catch (ignored) {
      // 页面替换过 console 也不能让 shim 挂掉。
    }
  }

  /**
   * 调用 shell 命令。
   * @param command - 命令名。
   * @param args - camelCase 参数。
   * @returns 命令的 promise。
   */
  function invoke(command, args) {
    var api = shellWindow.__TAURI_INTERNALS__
    if (api === undefined || api === null || typeof api.invoke !== 'function') {
      return Promise.reject(new Error('dsh desktop: the shell IPC is unavailable'))
    }
    return api.invoke(command, args === undefined ? {} : args)
  }

  /** @returns preload 暴露的能力开关。 */
  function capabilities() {
    var flags = shellWindow.__DSH_BRIDGE_CAPS__
    return flags === null || typeof flags !== 'object' ? {} : flags
  }

  /**
   * 同步读取回环 app server 基址。
   *
   * 顺序与 preload.js 一致：先看 shell 注入的全局，再问 `__DSH_BRIDGE_ESCAPE__`
   * （它内部缓存了 boot 之后拿到的 base）。两者都没有就返回 null。
   * @returns 基址字符串或 null。
   */
  function currentAppBase() {
    try {
      var injected = shellWindow.__DSH_APP_BASE__
      if (typeof injected === 'string' && parseBase(injected) !== null) return injected.trim()
    } catch (error) {
      // 忽略：注入值不可读时走逃生通道。
    }
    try {
      var escape = shellWindow.__DSH_BRIDGE_ESCAPE__
      if (escape !== null && typeof escape === 'object' && typeof escape.appBase === 'function') {
        var cached = escape.appBase()
        if (typeof cached === 'string' && parseBase(cached) !== null) return cached.trim()
      }
    } catch (error) {
      // 同上。
    }
    return null
  }

  // ---------------------------------------------------------------------------
  // 第 1 节：EventSource URL 重写。
  //
  // 只改 URL，不碰流：原生 EventSource 的解析、重连、事件顺序全部保留。
  // 基址在构造时可能还没拿到（boot 之前），此时用一个「排队 + 就绪后建连」的
  // 轻量包装顶上，避免把请求打到会被缓冲的 scheme handler 上。
  // ---------------------------------------------------------------------------
  ;(function installEventSourceShim() {
    var Native = shellWindow.EventSource
    if (typeof Native !== 'function') return

    /**
     * 基址未知时的延迟 EventSource。
     *
     * 只代理前端真正用到的表面：`addEventListener` / `removeEventListener` /
     * `close` / `readyState` / `url` / `onopen` / `onmessage` / `onerror`。
     * @param url - 原始（未改写）URL。
     * @param init - 原生构造的第二个参数。
     * @param resolveBase - 返回 promise 的基址解析函数。
     */
    function DeferredEventSource(url, init, resolveBase) {
      var self = this
      // This object inherits `EventSource.prototype`, where `url` and
      // `readyState` are getter-only accessors. A plain assignment therefore
      // throws in WebKit ("Attempted to assign to readonly property"), which
      // would break the page that merely constructed an EventSource — so the
      // deferred surface is installed as own data properties instead, shadowing
      // those accessors.
      var define = function (key, value) {
        Object.defineProperty(self, key, {
          value: value,
          writable: true,
          enumerable: true,
          configurable: true,
        })
      }
      define('__dshDeferred', true)
      define('url', url)
      define('withCredentials', init !== null && typeof init === 'object' && init.withCredentials === true)
      define('readyState', 0) // CONNECTING
      define('onopen', null)
      define('onmessage', null)
      define('onerror', null)
      define('onmessageerror', null)
      var listeners = []
      var native = null
      var closed = false

      var forward = function (type, target) {
        if (type === 'open' && typeof self.onopen === 'function') self.onopen(target)
        if (type === 'message' && typeof self.onmessage === 'function') self.onmessage(target)
        if (type === 'error' && typeof self.onerror === 'function') self.onerror(target)
        if (type === 'messageerror' && typeof self.onmessageerror === 'function') self.onmessageerror(target)
      }

      var connect = function (base) {
        if (closed) return
        var target = rewriteEventSourceUrl(url, base)
        try {
          native = new Native(target, init)
        } catch (error) {
          warn('could not open the event stream', error)
          define('readyState', 2)
          return
        }
        define('url', target)
        define('readyState', native.readyState)
        // 订阅者可能在前置阶段就注册了：先整体回放，再恢复直通。
        for (var index = 0; index < listeners.length; index++) {
          var entry = listeners[index]
          native.addEventListener(entry.type, entry.listener, entry.options)
        }
        listeners = []
        // 注意：用函数包一层固定 `type`，否则 `var` 会让所有闭包看到最后一个事件名。
        var bridge = ['open', 'message', 'error', 'messageerror']
        var bridgeFrom = function (type) {
          native.addEventListener(type, function (event) {
            // `native.readyState` is live: mirror it without touching the
            // inherited accessor.
            if (self.readyState !== native.readyState) define('readyState', native.readyState)
            forward(type, event)
          })
        }
        for (var position = 0; position < bridge.length; position++) bridgeFrom(bridge[position])
      }

      resolveBase().then(connect, function (error) {
        // 拿不到基址也不能让页面挂：退回原始 URL，让它按原样失败。
        warn('the app server base is unavailable; keeping the page-origin stream URL', error)
        connect(null)
      })

      // 这四个也在遮蔽 `EventTarget.prototype` / `EventSource.prototype` 上的原生方法。
      // 方法本身是数据属性、普通赋值本该可行，但**同一个对象上已经栽过一次**
      // （`url`/`readyState` 是只读访问器），所以统一走 `define`：将来 WebKit 把某个方法
      // 改成访问器时，这里不会第二次把页面打挂。
      define('addEventListener', function (type, listener, options) {
        if (native !== null) return native.addEventListener(type, listener, options)
        if (closed) return undefined
        listeners.push({ type: type, listener: listener, options: options })
        return undefined
      })
      define('removeEventListener', function (type, listener, options) {
        if (native !== null) return native.removeEventListener(type, listener, options)
        for (var index = listeners.length - 1; index >= 0; index--) {
          if (listeners[index].type === type && listeners[index].listener === listener) listeners.splice(index, 1)
        }
        return undefined
      })
      define('dispatchEvent', function (event) {
        if (native !== null) return native.dispatchEvent(event)
        return false
      })
      define('close', function () {
        closed = true
        if (native !== null) native.close()
        define('readyState', 2)
      })
    }
    // `instanceof EventSource` 必须成立：延迟对象继承原生原型。
    DeferredEventSource.prototype = Native.prototype

    var replacement = function EventSource(url, init) {
      var raw = String(url)
      if (!needsRewrite(raw)) return new Native(raw, init)
      var base = currentAppBase()
      if (base !== null) return new Native(rewriteEventSourceUrl(raw, base), init)
      return new DeferredEventSource(raw, init, function () {
        return invoke('desktop_app_base').then(function (value) {
          if (parseBase(typeof value === 'string' ? value : '') === null) {
            throw new Error('dsh desktop: the app server base is unavailable')
          }
          return String(value).trim()
        })
      })
    }
    replacement.prototype = Native.prototype
    replacement.CONNECTING = 0
    replacement.OPEN = 1
    replacement.CLOSED = 2
    try {
      shellWindow.EventSource = replacement
    } catch (error) {
      try {
        Object.defineProperty(shellWindow, 'EventSource', { value: replacement, configurable: true, writable: true })
      } catch (denied) {
        warn('could not install the EventSource rewrite', denied)
      }
    }
  })()

  // ---------------------------------------------------------------------------
  // 第 2 节：麦克风（原生采集 → 合成 MediaStream）。
  //
  // 链路：desktop_microphone_start(Channel) → Rust AVCaptureSession → 20 ms f32 单声道
  // → 页面 PcmRing → ScriptProcessorNode → MediaStreamAudioDestinationNode.stream
  // → 交给 get/MediaRecorder/analyser（前端原样使用）。
  //
  // 为什么是 ScriptProcessorNode：这台机器的 WKWebView 里 `AudioWorkletNode` 与
  // `MediaStreamTrackGenerator` 都不存在（见 docs/spikes/media-input.md 的实测），
  // ScriptProcessor 是唯一能在无 Media Capture 的 WebView 里把外部 PCM 喂进
  // MediaStream 的通道。
  // ---------------------------------------------------------------------------
  ;(function installMicrophoneShim() {
    var navigator = shellWindow.navigator
    if (navigator === undefined || navigator === null) return
    var existing = navigator.mediaDevices
    if (existing !== null && typeof existing === 'object'
      && typeof existing.getUserMedia === 'function'
      && capabilities().syntheticMicrophone !== true) {
      // 该 WebView 自带 Media Capture（将来的 WebKit）：原生优先，不动它。
      return
    }
    var AudioContextCtor = shellWindow.AudioContext || shellWindow.webkitAudioContext
    var MediaStreamCtor = shellWindow.MediaStream
    if (typeof AudioContextCtor !== 'function' || typeof MediaStreamCtor !== 'function') {
      warn('the WebView cannot synthesize a media stream; the microphone stays unavailable')
      return
    }
    if (typeof internals.transformCallback !== 'function') {
      warn('the shell IPC cannot carry an audio channel; the microphone stays unavailable')
      return
    }

    /** 采集会话：一次 getUserMedia 一个。 */
    var session = null
    /** 最近一次 open 头（含设备信息），用于 enumerateDevices/诊断。 */
    var lastOpen = null

    /**
     * 把 promise 拒绝包装成 DOMException 形状的错误。
     * @param reason - 失败原因。
     * @returns DOMException（无法构造时返回 Error）。
     */
    function failure(reason) {
      var mapped = failureFor(reason)
      try {
        return new shellWindow.DOMException(mapped.message, mapped.name)
      } catch (error) {
        var fallback = new Error(mapped.message)
        fallback.name = mapped.name
        return fallback
      }
    }

    /**
     * 结束当前会话：停采集、断节点、关上下文、补发 ended。
     * @param notify - 是否派发 track 的 `ended` 事件（插件用它做中断检测）。
     */
    function teardown(notify) {
      var current = session
      session = null
      if (current === null) return
      current.alive = false
      try {
        if (current.processor !== null && typeof current.processor.disconnect === 'function') {
          current.processor.onaudioprocess = null
          current.processor.disconnect()
        }
      } catch (error) {
        warn('could not detach the audio processor', error)
      }
      try {
        if (current.destination !== null && typeof current.destination.disconnect === 'function') {
          current.destination.disconnect()
        }
      } catch (error) {
        warn('could not detach the audio destination', error)
      }
      try {
        if (current.context !== null && typeof current.context.close === 'function') {
          var closing = current.context.close()
          if (closing !== null && typeof closing.catch === 'function') closing.catch(function () {})
        }
      } catch (error) {
        warn('could not close the audio context', error)
      }
      try {
        if (current.stopNative) {
          current.stopNative().catch(function (error) { warn('could not stop native capture', error) })
        }
      } catch (error) {
        warn('could not stop native capture', error)
      }
      if (notify === true && current.track !== null && typeof current.track.dispatchEvent === 'function') {
        try {
          current.track.dispatchEvent(new shellWindow.Event('ended'))
        } catch (error) {
          warn('could not report the end of the audio track', error)
        }
      }
    }

    /**
     * 建立一次采集会话。
     * @param options - 归一化后的约束。
     * @returns promise，成功时给 MediaStream。
     */
    function openSession(options) {
      // AudioContext 必须在用户手势的同一任务里创建/恢复，否则 WebKit 会把它挂在
      // suspended 上，脚本处理节点永远不被调度。所以这里同步建，再异步等 Rust。
      var context
      try {
        context = new AudioContextCtor({ sampleRate: 48000, latencyHint: 'interactive' })
      } catch (error) {
        try {
          context = new AudioContextCtor()
        } catch (denied) {
          return Promise.reject(failure('unavailable'))
        }
      }
      try {
        var resuming = context.resume()
        if (resuming !== null && typeof resuming.catch === 'function') resuming.catch(function () {})
      } catch (error) {
        // resume 失败也要继续：真机上通常仍会 running。
      }

      var destination
      var processor
      var track
      try {
        destination = context.createMediaStreamDestination()
        processor = context.createScriptProcessor(1024, 0, 1)
        track = destination.stream.getAudioTracks()[0]
      } catch (error) {
        try {
          context.close()
        } catch (ignored) {}
        return Promise.reject(failure('unavailable'))
      }

      var ring = new PcmRing(Math.max(1, Math.floor((context.sampleRate || 48000) / 2)))
      var current = {
        context: context,
        destination: destination,
        processor: processor,
        track: track,
        ring: ring,
        alive: true,
        sampleRate: context.sampleRate || 48000,
        frames: 0,
        overruns: 0,
        stopNative: null,
      }
      session = current

      // 插件停止录制的方式是 `stream.getTracks().forEach((track) => track.stop())`。
      // 合成轨的原生 `stop()` 只把这条 WebAudio 轨置成 ended，**不会**停掉 Rust 侧的
      // AVCaptureSession，所以必须在本实例上劫持 `stop`：先停原生轨，再拆会话并补发
      // `ended`。实测原生 `stop()` 不会自己派发 `ended`（见 spike），因此这里补发是
      // 唯一的一次；万一将来的 WebKit 也开始派发，插件的 `lifetime.abort` 会先挡住
      // 第二次中断处理。
      try {
        var nativeStop = typeof track.stop === 'function' ? track.stop : null
        Object.defineProperty(track, 'stop', {
          value: function () {
            if (nativeStop !== null) {
              try {
                nativeStop.call(track)
              } catch (error) {
                warn('could not stop the synthetic track', error)
              }
            }
            if (session === current) teardown(true)
          },
          configurable: true,
          writable: true,
        })
      } catch (error) {
        warn('could not intercept the synthetic track stop', error)
      }

      processor.onaudioprocess = function (event) {
        try {
          var output = event.outputBuffer.getChannelData(0)
          var taken = ring.pull(output)
          if (taken === 0) current.overruns += 1
        } catch (error) {
          warn('the audio processor failed', error)
        }
      }
      try {
        processor.connect(destination)
      } catch (error) {
        teardown(false)
        return Promise.reject(failure('unavailable'))
      }

      // 通道：把 transformCallback 的 id 交给 Rust，Rust 用
      // `Channel<InvokeResponseBody>` 反向推帧（二进制 PCM / JSON 头）。
      var channelId = internals.transformCallback(function (payload) {
        if (!current.alive) return
        var decoded = decodeChannelMessage(payload)
        if (decoded.kind === 'json' && decoded.value !== null && typeof decoded.value === 'object') {
          if (decoded.value.type === 'open') {
            current.sampleRate = typeof decoded.value.sampleRate === 'number' ? decoded.value.sampleRate : current.sampleRate
            lastOpen = decoded.value
            applyDeviceIdentity(track, decoded.value)
          }
          return
        }
        if (decoded.kind === 'bytes' && typeof decoded.value.length === 'number') {
          current.frames += 1
          ring.push(decoded.value)
        }
      })

      current.stopNative = function () {
        return invoke('desktop_microphone_stop')
      }
      return invoke('desktop_microphone_start', {
        channel: '__CHANNEL__:' + channelId,
        options: {
          protocol: MIC_PROTOCOL,
          sampleRate: current.sampleRate,
          channelCount: 1,
          frameMillis: 20,
          deviceId: typeof options.deviceId === 'string' ? options.deviceId : '',
          echoCancellation: options.echoCancellation === true,
          noiseSuppression: options.noiseSuppression === true,
          autoGainControl: options.autoGainControl === true,
        },
      }).then(
        function () {
          if (!current.alive) throw failure('cancelled')
          return new MediaStreamCtor([track])
        },
        function (error) {
          var reason = error !== null && typeof error === 'object' && typeof error.message === 'string'
            ? error.message
            : String(error)
          teardown(false)
          throw failure(reason)
        },
      )
    }

    /**
     * 把真实设备身份写到合成 track 上。
     *
     * 合成轨的 `label` 是 `MediaStreamAudioDestinationNode`、`getSettings()` 返回 `{}`，
     * 而设备选择器要拿 `label` + `getSettings().groupId` 回显「当前用的是哪只麦克风」。
     * @param track - 合成音频轨。
     * @param header - Rust 侧 open 头。
     */
    function applyDeviceIdentity(track, header) {
      var settings = {
        deviceId: typeof header.deviceId === 'string' ? header.deviceId : '',
        groupId: typeof header.groupId === 'string' ? header.groupId : '',
        sampleRate: typeof header.sampleRate === 'number' ? header.sampleRate : 0,
        channelCount: typeof header.channelCount === 'number' ? header.channelCount : 1,
        echoCancellation: header.echoCancellation === true,
        noiseSuppression: header.noiseSuppression === true,
        autoGainControl: header.autoGainControl === true,
      }
      try {
        Object.defineProperty(track, 'getSettings', {
          value: function () { return settings },
          configurable: true,
          writable: true,
        })
      } catch (error) {
        warn('could not describe the synthetic track', error)
      }
      if (typeof header.label === 'string' && header.label !== '') {
        try {
          Object.defineProperty(track, 'label', { value: header.label, configurable: true })
        } catch (error) {
          warn('could not label the synthetic track', error)
        }
      }
    }

    var mediaDevices = {
      /** @returns promise，成功时给一个由原生采集喂料的 MediaStream。 */
      getUserMedia: function (constraints) {
        var normalized = normalizeAudioConstraints(constraints)
        if (!normalized.ok) {
          var reason = normalized.reason === 'unsupported-video' ? 'unsupported' : 'unavailable'
          return Promise.reject(failure(reason))
        }
        if (session !== null) teardown(true)
        return openSession(normalized.options)
      },
      /** @returns promise，给 `MediaDeviceInfo` 形状的输入设备清单。 */
      enumerateDevices: function () {
        return invoke('desktop_microphone_status').then(
          function (status) {
            var devices = status !== null && typeof status === 'object' && Array.isArray(status.devices)
              ? status.devices
              : Array.isArray(status) ? status : []
            return deviceListFromStatus(devices)
          },
          function (error) {
            warn('could not list the microphones', error)
            return deviceListFromStatus([])
          },
        )
      },
      /** @returns 与 `getSupportedConstraints()` 同名的能力表。 */
      getSupportedConstraints: function () {
        return {
          deviceId: true,
          echoCancellation: true,
          noiseSuppression: true,
          autoGainControl: true,
          sampleRate: true,
          channelCount: true,
        }
      },
      addEventListener: function () {},
      removeEventListener: function () {},
      dispatchEvent: function () { return false },
    }

    try {
      Object.defineProperty(navigator, 'mediaDevices', {
        value: mediaDevices,
        configurable: true,
        writable: true,
      })
    } catch (error) {
      try {
        Object.defineProperty(shellWindow.Navigator.prototype, 'mediaDevices', {
          get: function () { return mediaDevices },
          configurable: true,
        })
      } catch (denied) {
        warn('could not install the microphone bridge', denied)
        return
      }
    }

    // 诊断 / 自救通道：与 `__DSH_BRIDGE_ESCAPE__` 同一条思路，只读。
    try {
      Object.defineProperty(shellWindow, '__DSH_MEDIA__', {
        value: Object.freeze({
          protocol: MIC_PROTOCOL,
          /** @returns 采集状态与缓冲水位（排查卡顿/延迟用）。 */
          diagnostics: function () {
            var current = session
            return {
              live: current !== null,
              sampleRate: current === null ? 0 : current.sampleRate,
              frames: current === null ? 0 : current.frames,
              buffered: current === null ? 0 : current.ring.available(),
              capacity: current === null ? 0 : current.ring.capacity,
              dropped: current === null ? 0 : current.ring.dropped,
              starved: current === null ? 0 : current.ring.starved,
              overruns: current === null ? 0 : current.overruns,
              device: lastOpen === null ? null : { deviceId: lastOpen.deviceId, groupId: lastOpen.groupId, label: lastOpen.label },
            }
          },
          /** @returns promise，给 Rust 侧权限/设备状态。 */
          status: function () { return invoke('desktop_microphone_status') },
          /** 立即停止采集（等同插件里的 track.stop()）。 */
          stop: function () { teardown(true) },
        }),
        configurable: true,
        writable: false,
        enumerable: false,
      })
    } catch (error) {
      warn('could not publish the media diagnostics', error)
    }
  })()
})()
