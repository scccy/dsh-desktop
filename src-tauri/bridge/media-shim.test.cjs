/**
 * `media-shim.js` 的 Node 侧检查。
 *
 * 分两部分：
 *  1. **纯函数**：直接 `require` 该文件取值（它作为 CommonJS 载入时会导出内部纯函数，
 *     随后因为没有 `window` 提前返回），覆盖 EventSource 的 URL 重写规则、通道消息
 *     解码、约束归一化、失败原因映射、设备清单归一化、PCM 环形缓冲。
 *  2. **运行期行为**：把源码丢进 `node:vm` 的假环境里跑，用桩件断言真实安装路径
 *     （EventSource 改写是否生效、基址未知时是否排队并在就绪后回放监听器、
 *     `getUserMedia` 是否合成 MediaStream、通道帧是否被推进 ScriptProcessor、
 *     `track.stop()` 是否停掉原生采集）。
 *
 * 运行：`node src-tauri/bridge/media-shim.test.cjs`
 */
'use strict'

const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const vm = require('node:vm')

const shim = require('./media-shim.js')
const preload = require('./preload.js')

const BASE = 'http://127.0.0.1:54321'

// ---------------------------------------------------------------------------
// 1. EventSource URL 重写：语义必须与 preload.js 的 fetch/XHR 重写**完全一致**
//    （同一个页面里两条通道打同一个 app server，规则分叉就会 404）。
// ---------------------------------------------------------------------------
const parityCases = [
  '/api/stream',
  'api/stream',
  '/api/stream?since=1#frag',
  '?since=1',
  '#frag',
  'dsh-app://app/api/stream',
  'dsh-app://app/',
  'dsh-app://app',
  'http://127.0.0.1:9000/api',
  'https://example.com/api',
  'blob:http://127.0.0.1:54321/abc',
  'data:text/plain,hi',
  '//cdn.example.com/x.js',
  'ws://127.0.0.1:54321/socket',
  '',
]
for (const raw of parityCases) {
  assert.equal(
    shim.rewriteEventSourceUrl(raw, BASE),
    preload.rewriteUrl(raw, BASE),
    `rewrite parity for ${raw}`,
  )
}

assert.equal(shim.rewriteEventSourceUrl('/api/stream', BASE), BASE + '/api/stream')
assert.equal(shim.rewriteEventSourceUrl('dsh-app://app/sse', BASE), BASE + '/sse')
assert.equal(shim.rewriteEventSourceUrl('https://example.com/sse', BASE), 'https://example.com/sse')
// 基址缺失时保持原样（让原生实现按原样失败，而不是被改到别处）。
assert.equal(shim.rewriteEventSourceUrl('/api/stream', null), '/api/stream')
assert.equal(shim.rewriteEventSourceUrl('/api/stream', 'dsh-app://app'), '/api/stream')

assert.equal(shim.needsRewrite('/api'), true)
assert.equal(shim.needsRewrite('dsh-app://app/api'), true)
assert.equal(shim.needsRewrite('https://example.com/api'), false)
assert.equal(shim.parseBase('http://127.0.0.1:1') === null, false)
assert.equal(shim.parseBase('dsh-app://app'), null)

// ---------------------------------------------------------------------------
// 2. 通道消息解码：Tauri 的 Channel 会按负载大小给出三种形状，必须都能吃。
// ---------------------------------------------------------------------------
const header = { type: 'open', sampleRate: 48000, channelCount: 1, framesPerFrame: 960 }

// 小负载走 eval：`{ message, index }` 包装。
assert.deepEqual(shim.decodeChannelMessage({ message: header, index: 0 }), {
  kind: 'json',
  value: header,
})
// 大负载走内部 fetch：直接是 ArrayBuffer。
const pcm = new Float32Array([0.1, -0.2, 0.3])
assert.deepEqual(shim.decodeChannelMessage(pcm.buffer), { kind: 'bytes', value: pcm })
// 有的版本直接给视图。
assert.deepEqual(shim.decodeChannelMessage(pcm), { kind: 'bytes', value: pcm })
// JSON 字符串、裸对象。
assert.deepEqual(shim.decodeChannelMessage(JSON.stringify(header)), { kind: 'json', value: header })
assert.deepEqual(shim.decodeChannelMessage(header), { kind: 'json', value: header })
// 不认识的形状不抛。
assert.equal(shim.decodeChannelMessage(undefined).kind, 'unknown')
assert.equal(shim.decodeChannelMessage(42).kind, 'unknown')
// f32 对齐：偏移不是 4 的倍数时原样返回（上层会丢弃，不会读出乱值）。
const odd = new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]).subarray(1, 5)
assert.equal(shim.decodeChannelMessage(odd).value, odd)

// ---------------------------------------------------------------------------
// 3. getUserMedia 约束归一化：语音输入插件只传 audio；请求视频必须明确拒绝。
// ---------------------------------------------------------------------------
const voiceInput = {
  audio: { echoCancellation: true, noiseSuppression: true, deviceId: { exact: 'mic-1' } },
  video: false,
}
assert.deepEqual(shim.normalizeAudioConstraints(voiceInput), {
  ok: true,
  options: { deviceId: 'mic-1', echoCancellation: true, noiseSuppression: true },
})
assert.deepEqual(shim.normalizeAudioConstraints({ audio: true }), { ok: true, options: {} })
assert.deepEqual(shim.normalizeAudioConstraints({ audio: { deviceId: 'mic-2' } }), {
  ok: true,
  options: { deviceId: 'mic-2' },
})
assert.deepEqual(shim.normalizeAudioConstraints({ audio: { deviceId: { ideal: 'mic-3' } } }), {
  ok: true,
  options: { deviceId: 'mic-3' },
})
// 空 deviceId（系统默认）不是非法值。
assert.deepEqual(shim.normalizeAudioConstraints({ audio: { deviceId: '' } }), {
  ok: true,
  options: {},
})
// 视频一律拒绝，绝不假装给一条轨。
assert.equal(shim.normalizeAudioConstraints({ audio: true, video: true }).ok, false)
assert.equal(shim.normalizeAudioConstraints({ audio: true, video: true }).reason, 'unsupported-video')
assert.equal(shim.normalizeAudioConstraints({ audio: false }).ok, false)
assert.equal(shim.normalizeAudioConstraints({}).ok, false)
assert.equal(shim.normalizeAudioConstraints(undefined).ok, false)

// ---------------------------------------------------------------------------
// 4. 失败原因 → DOMException 名字：与 Rust `media::permission::FailureReason` 逐字对齐，
//    并落在插件分支会识别的名字上。
// ---------------------------------------------------------------------------
assert.equal(shim.failureFor('denied').name, 'NotAllowedError')
assert.equal(shim.failureFor('permission').name, 'NotAllowedError')
assert.equal(shim.failureFor('restricted').name, 'NotAllowedError')
assert.equal(shim.failureFor('device-missing').name, 'NotFoundError')
assert.equal(shim.failureFor('bad-device').name, 'OverconstrainedError')
assert.equal(shim.failureFor('busy').name, 'NotReadableError')
assert.equal(shim.failureFor('unsupported').name, 'NotSupportedError')
assert.equal(shim.failureFor('cancelled').name, 'AbortError')
assert.equal(shim.failureFor('').name, 'NotReadableError')
assert.match(shim.failureFor('').message, /microphone capture failed/)

// ---------------------------------------------------------------------------
// 5. 设备清单归一化：插件靠 `deviceId === 'default'` 找系统默认项，再按 groupId 反查名字。
// ---------------------------------------------------------------------------
const devices = shim.deviceListFromStatus([
  { id: 'mic-1', label: 'MacBook Pro 麦克风', groupId: 'g1', isDefault: true },
  { id: 'mic-2', label: 'USB 麦克风', groupId: 'g2', isDefault: false },
])
assert.deepEqual(
  devices.map((device) => device.deviceId),
  ['default', 'mic-1', 'mic-2'],
)
assert.equal(devices[0].label, 'MacBook Pro 麦克风')
assert.equal(devices[0].groupId, 'default')
assert.equal(devices[1].groupId, 'g1')
assert.ok(devices.every((device) => device.kind === 'audioinput'))
// 没有默认标记时用第一个设备的 label 当默认项名字。
assert.equal(shim.deviceListFromStatus([{ id: 'a', label: 'A' }])[0].label, 'A')
// 没设备时只给出一个空的默认项（插件据此显示「不可用」，而不是崩）。
assert.deepEqual(shim.deviceListFromStatus([]), [
  { deviceId: 'default', groupId: 'default', kind: 'audioinput', label: '' },
])
assert.deepEqual(shim.deviceListFromStatus(null), [
  { deviceId: 'default', groupId: 'default', kind: 'audioinput', label: '' },
])
// 保留名不重复出现。
const reserved = shim.deviceListFromStatus([
  { id: 'default' },
  { id: 'communications' },
  { id: 'x' },
])
assert.deepEqual(
  reserved.map((device) => device.deviceId),
  ['default', 'x'],
)

// ---------------------------------------------------------------------------
// 6. PCM 环形缓冲：欠载补零、积压丢最旧、单帧超容量、回绕。
// ---------------------------------------------------------------------------
const ring = new shim.PcmRing(8)
assert.equal(ring.available(), 0)
assert.equal(ring.push(new Float32Array([1, 2, 3])), 3)
const out = new Float32Array(5)
assert.equal(ring.pull(out), 3)
assert.deepEqual(Array.from(out), [1, 2, 3, 0, 0])
assert.equal(ring.starved, 2)

ring.reset()
ring.push(new Float32Array([1, 2, 3]))
ring.push(new Float32Array([4, 5, 6, 7, 8, 9, 10, 11]))
assert.equal(ring.dropped, 3)
assert.equal(ring.available(), 8)
const drained = new Float32Array(8)
assert.equal(ring.pull(drained), 8)
assert.deepEqual(Array.from(drained), [4, 5, 6, 7, 8, 9, 10, 11])

ring.reset()
ring.push(new Float32Array([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]))
assert.equal(ring.available(), 8)
const tail = new Float32Array(8)
ring.pull(tail)
assert.deepEqual(Array.from(tail), [3, 4, 5, 6, 7, 8, 9, 10])
assert.equal(ring.dropped, 2)

// 环形回绕（写指针跨过缓冲区末尾）不能错位。
const wrap = new shim.PcmRing(4)
wrap.push(new Float32Array([1, 2, 3]))
const first = new Float32Array(2)
wrap.pull(first)
assert.deepEqual(Array.from(first), [1, 2])
wrap.push(new Float32Array([4, 5, 6]))
const second = new Float32Array(4)
assert.equal(wrap.pull(second), 4)
assert.deepEqual(Array.from(second), [3, 4, 5, 6])

// 边界：空输入/空目标不改变状态。
const inert = new shim.PcmRing(4)
assert.equal(inert.push(new Float32Array(0)), 0)
assert.equal(inert.pull(new Float32Array(0)), 0)
assert.equal(new shim.PcmRing(0).available(), 0)

console.log('media-shim helpers: ok')

// ---------------------------------------------------------------------------
// 7. 运行期行为（vm 假环境）。
// ---------------------------------------------------------------------------
const source = fs.readFileSync(path.join(__dirname, 'media-shim.js'), 'utf8')

/** 原生 `EventSource` 的桩：记录构造参数与实例，允许手动派发事件。 */
class FakeEventSource {
  constructor(url, init) {
    this.url = url
    this.init = init
    this.readyState = 0
    this.listeners = []
    FakeEventSource.instances.push(this)
    FakeEventSource.constructed.push(url)
  }

  addEventListener(type, listener) {
    this.listeners.push({ type, listener })
  }

  removeEventListener(type, listener) {
    this.listeners = this.listeners.filter(
      (entry) => entry.type !== type || entry.listener !== listener,
    )
  }

  dispatchEvent() {
    return true
  }

  close() {
    this.readyState = 2
    this.closed = true
  }

  /** 测试用：派发一条事件。 */
  emit(type, payload) {
    if (type === 'open') this.readyState = 1
    for (const entry of this.listeners.slice()) {
      if (entry.type === type) entry.listener(payload)
    }
  }
}
FakeEventSource.instances = []
FakeEventSource.constructed = []

/**
 * 一条假的合成音频轨。
 *
 * 刻意写成「原型上有只读访问器 + 原型方法」的形状，复刻本机 WKWebView 实测到的
 * `MediaStreamTrack.prototype`（`label`/`kind`/`readyState` 是 getter，`getSettings`/
 * `stop` 是原型方法）：这样一旦 shim 用普通赋值去覆盖它们，测试就会像真机一样抛
 * `Attempted to assign to readonly property` —— 这正是 EventSource 那条真机缺陷的
 * 同类风险（见 `deferred event source survives readonly accessors`）。
 */
class FakeAudioTrack {
  constructor(observer) {
    this._observer = observer
    this._listeners = []
    this._stopped = 0
    this.enabled = true
    this.contentHint = ''
    this.id = 'track-1'
  }

  get kind() {
    return 'audio'
  }

  get label() {
    return 'MediaStreamAudioDestinationNode'
  }

  get readyState() {
    return this._stopped > 0 ? 'ended' : 'live'
  }

  getSettings() {
    return {}
  }

  getCapabilities() {
    return {}
  }

  clone() {
    return new FakeAudioTrack(this._observer)
  }

  addEventListener(type, listener) {
    this._listeners.push({ type, listener })
  }

  removeEventListener(type, listener) {
    const index = this._listeners.findIndex(
      (entry) => entry.type === type && entry.listener === listener,
    )
    if (index >= 0) this._listeners.splice(index, 1)
  }

  dispatchEvent(event) {
    for (const entry of this._listeners.slice()) {
      if (entry.type === event.type) entry.listener(event)
    }
    return true
  }

  stop() {
    this._stopped += 1
    this._observer.stoppedTracks += 1
  }
}

/**
 * 在假环境里执行 media-shim.js。
 * @param options - `{ appBase, invoke }`。
 * @returns 观察面。
 */
function loadShim(options = {}) {
  const calls = []
  const channel = { callback: null, id: 7 }
  const observer = { stoppedTracks: 0 }
  const audio = {
    context: null,
    destination: null,
    processor: null,
    /** 触发一次 ScriptProcessor 回调并返回输出缓冲。 */
    process() {
      const output = new Float32Array(1024)
      if (this.processor === null || this.processor.onaudioprocess === null) return output
      this.processor.onaudioprocess({ outputBuffer: { getChannelData: () => output } })
      return output
    },
  }

  class FakeAudioContext {
    constructor(config) {
      this.sampleRate = config && config.sampleRate ? config.sampleRate : 48000
      this.state = 'suspended'
      this.baseLatency = 0.0026
      audio.context = this
    }

    resume() {
      this.state = 'running'
      return Promise.resolve()
    }

    close() {
      this.state = 'closed'
      return Promise.resolve()
    }

    createMediaStreamDestination() {
      const track = new FakeAudioTrack(observer)
      audio.track = track
      const destination = {
        stream: {
          getAudioTracks: () => [track],
          getTracks: () => [track],
        },
        disconnect() {
          audio.destinationDisconnected = true
        },
      }
      audio.destination = destination
      return destination
    }

    createScriptProcessor(size) {
      const processor = {
        size,
        onaudioprocess: null,
        connected: false,
        connect(target) {
          this.connected = true
          audio.connectedTo = target
        },
        disconnect() {
          this.connected = false
          audio.processorDisconnected = true
        },
      }
      audio.processor = processor
      return processor
    }
  }

  class FakeMediaStream {
    constructor(tracks) {
      this.tracks = Array.isArray(tracks) ? tracks.slice() : []
    }

    getAudioTracks() {
      return this.tracks.filter((track) => track.kind === 'audio')
    }

    getTracks() {
      return this.tracks.slice()
    }
  }

  const window = {
    location: { protocol: 'dsh-app:', hostname: 'app', href: 'dsh-app://app/' },
    navigator: { userAgent: 'Mozilla/5.0 (Macintosh)', platform: 'MacIntel' },
    EventSource: options.EventSourceClass || FakeEventSource,
    AudioContext: FakeAudioContext,
    MediaStream: FakeMediaStream,
    __TAURI_INTERNALS__: {
      invoke(command, args) {
        calls.push({ command, args })
        if (options.invoke !== undefined) return options.invoke(command, args)
        if (command === 'desktop_app_base') return Promise.resolve(BASE)
        if (command === 'desktop_microphone_start') return Promise.resolve(null)
        if (command === 'desktop_microphone_stop') return Promise.resolve(null)
        if (command === 'desktop_microphone_status') {
          return Promise.resolve({
            authorization: 'granted',
            available: true,
            devices: [{ id: 'mic-1', label: 'MacBook Pro 麦克风', groupId: 'g1', isDefault: true }],
          })
        }
        return Promise.resolve(null)
      },
      transformCallback(callback) {
        channel.callback = callback
        return channel.id
      },
    },
    __DSH_APP_BASE__: options.appBase,
    Error,
    Event: class FakeEvent {
      constructor(type) {
        this.type = type
      }
    },
    DOMException: class FakeDomException extends Error {
      constructor(message, name) {
        super(message)
        this.name = name
      }
    },
    console: { warn() {}, error() {}, log() {} },
  }
  window.top = window

  const context = {
    window,
    console: window.console,
    URL,
    ArrayBuffer,
    Float32Array,
    Uint8Array,
    JSON,
    Promise,
    Math,
    Object,
    String,
    Number,
    isFinite,
    Error,
    setTimeout,
  }
  vm.runInNewContext(source, context, { filename: 'media-shim.js' })
  return { window, calls, channel, audio, observer }
}

/** 让排队的微任务/定时器跑完。 */
const flush = () => new Promise((resolve) => setTimeout(resolve, 0))

async function testEventSourceRewrite() {
  FakeEventSource.constructed.length = 0
  const env = loadShim({ appBase: BASE })
  const stream = new env.window.EventSource('/api/sse')
  assert.ok(stream instanceof FakeEventSource, '改写后仍是原生 EventSource 实例')
  assert.deepEqual(FakeEventSource.constructed, [BASE + '/api/sse'])
  assert.equal(stream.url, BASE + '/api/sse')

  // 绝对外链不改写。
  new env.window.EventSource('https://example.com/sse')
  assert.equal(FakeEventSource.constructed[1], 'https://example.com/sse')

  // 静态常量仍可用（前端可能读 EventSource.OPEN）。
  assert.equal(env.window.EventSource.CONNECTING, 0)
  assert.equal(env.window.EventSource.OPEN, 1)
  assert.equal(env.window.EventSource.CLOSED, 2)
  console.log('event source rewrite: ok')
}

async function testDeferredEventSource() {
  FakeEventSource.constructed.length = 0
  FakeEventSource.instances.length = 0
  // 基址还没就绪（boot 之前）：命令异步回答。
  const env = loadShim({ appBase: undefined })
  const deferred = new env.window.EventSource('/api/sse')
  const received = []
  deferred.addEventListener('message', (event) => received.push(event))
  let opened = 0
  deferred.onopen = () => {
    opened += 1
  }
  assert.equal(deferred.readyState, 0, '基址未知时应处于 CONNECTING')
  assert.equal(FakeEventSource.constructed.length, 0, '基址未知前不得发起连接')

  await flush()
  assert.deepEqual(FakeEventSource.constructed, [BASE + '/api/sse'])
  assert.equal(FakeEventSource.instances.length, 1)
  const native = FakeEventSource.instances[0]
  // 建连前注册的监听器与 on* 处理器都必须被转发。
  native.emit('open', { type: 'open' })
  assert.equal(opened, 1, 'onopen 必须被转发')
  native.emit('message', { data: 'hi' })
  assert.deepEqual(received, [{ data: 'hi' }])

  // 关闭之后不再建连。
  FakeEventSource.constructed.length = 0
  const closed = new env.window.EventSource('/api/other')
  closed.close()
  await flush()
  assert.deepEqual(FakeEventSource.constructed, [], 'close() 之后不得再建连')
  console.log('deferred event source: ok')
}

/**
 * WebKit 形状的 EventSource：`url` / `readyState` 只在原型上是**只读 getter**。
 *
 * 真机回归（macOS WKWebView）：延迟对象若用普通赋值写这两个属性，WebKit 抛
 * `Attempted to assign to readonly property`，页面每一次 `new EventSource(...)`
 * 都失败 —— 结果是 Web UI 的 `@deepseek-ai/dsh-client-hmr` 条目无法激活、整个
 * web boot 失败。这个桩让那条路径在 Node 上也能被测到。
 */
class ReadonlyEventSource {
  constructor(url) {
    this._url = url
    this._readyState = 0
    ReadonlyEventSource.instances.push(this)
  }

  get url() {
    return this._url
  }

  get readyState() {
    return this._readyState
  }

  addEventListener() {}

  removeEventListener() {}

  close() {}
}
ReadonlyEventSource.instances = []

function testDeferredSurvivesReadonlyAccessors() {
  ReadonlyEventSource.instances.length = 0
  const env = loadShim({ appBase: undefined, EventSourceClass: ReadonlyEventSource })
  let deferred
  assert.doesNotThrow(() => {
    deferred = new env.window.EventSource('/api/sse')
  }, '延迟构造不得向只读访问器赋值')
  assert.equal(deferred.url, '/api/sse', 'url 必须可读回')
  assert.equal(deferred.readyState, 0, '未知基址时应报告 CONNECTING')
  assert.ok(deferred instanceof ReadonlyEventSource, 'instanceof 仍须成立')
  console.log('deferred event source survives readonly accessors: ok')
}

async function testMicrophone() {
  FakeEventSource.constructed.length = 0
  const env = loadShim({ appBase: BASE })
  const navigator = env.window.navigator
  assert.equal(typeof navigator.mediaDevices, 'object', 'navigator.mediaDevices 必须被装上')
  assert.equal(typeof navigator.mediaDevices.getUserMedia, 'function')

  const stream = await navigator.mediaDevices.getUserMedia({
    audio: { echoCancellation: true, noiseSuppression: true },
    video: false,
  })
  assert.equal(stream.getAudioTracks().length, 1, '合成一条音频轨')

  const start = env.calls.find((call) => call.command === 'desktop_microphone_start')
  assert.ok(start !== undefined, '必须请求 shell 开始采集')
  assert.equal(start.args.channel, '__CHANNEL__:7')
  assert.equal(start.args.options.protocol, 1)
  assert.equal(start.args.options.sampleRate, 48000)
  assert.equal(start.args.options.channelCount, 1)
  assert.equal(start.args.options.frameMillis, 20)
  assert.equal(start.args.options.echoCancellation, true)
  assert.equal(start.args.options.deviceId, '')
  assert.ok(env.audio.processor.connected, 'ScriptProcessor 必须接进 destination')

  // 首帧头把真实设备身份写到合成轨上（设备选择器要用）。
  env.channel.callback({
    message: {
      type: 'open',
      sampleRate: 48000,
      channelCount: 1,
      deviceId: 'mic-1',
      groupId: 'g1',
      label: 'MacBook Pro 麦克风',
    },
    index: 0,
  })
  const track = stream.getAudioTracks()[0]
  assert.equal(track.getSettings().deviceId, 'mic-1')
  assert.equal(track.getSettings().groupId, 'g1')
  assert.equal(track.label, 'MacBook Pro 麦克风')

  // 推一帧 PCM：ScriptProcessor 回调必须把它读进输出缓冲。
  env.channel.callback(new Float32Array([0.25, -0.25]).buffer)
  const output = env.audio.process()
  assert.equal(output[0], 0.25)
  assert.equal(output[1], -0.25)
  // 缓冲被抽干后补静音，不抛错，并计入 starved。
  const silent = env.audio.process()
  assert.equal(silent[0], 0)
  assert.ok(env.window.__DSH_MEDIA__.diagnostics().starved > 0)
  assert.equal(env.window.__DSH_MEDIA__.diagnostics().frames, 1)

  // `track.stop()`（插件停止录制的唯一方式）必须停掉原生采集并补发 ended。
  let ended = 0
  track.addEventListener('ended', () => {
    ended += 1
  })
  track.stop()
  await flush()
  assert.ok(
    env.calls.some((call) => call.command === 'desktop_microphone_stop'),
    'track.stop() 必须停掉原生采集',
  )
  assert.equal(ended, 1, 'ended 必须由我们补发')
  assert.equal(env.observer.stoppedTracks, 1, '原生轨也要被停掉')
  assert.equal(env.window.__DSH_MEDIA__.diagnostics().live, false)

  // 视频请求明确失败。
  await assert.rejects(
    () => navigator.mediaDevices.getUserMedia({ audio: true, video: true }),
    (error) => error.name === 'NotSupportedError',
  )

  // shell 拒绝时映射成插件认识的 DOMException 名字。
  const denying = loadShim({
    appBase: BASE,
    invoke(command) {
      if (command === 'desktop_microphone_start') return Promise.reject(new Error('denied'))
      return Promise.resolve(null)
    },
  })
  await assert.rejects(
    () => denying.window.navigator.mediaDevices.getUserMedia({ audio: true }),
    (error) => error.name === 'NotAllowedError',
  )

  // enumerateDevices 走 shell 状态命令，并补出 `default` 项。
  const listed = await navigator.mediaDevices.enumerateDevices()
  assert.equal(listed[0].deviceId, 'default')
  assert.equal(listed[1].deviceId, 'mic-1')
  assert.equal(listed[1].label, 'MacBook Pro 麦克风')
  console.log('microphone shim: ok')
}

// ---------------------------------------------------------------------------
// 8. 源码不变量：这些是「WKWebView 实测 + 与 preload.js 分工」定下来的硬约束，
//    写死在源码里，谁改动都会在这里炸。
// ---------------------------------------------------------------------------
/**
 * 把注释抹成空格（保留换行与字符串字面量），让「源码不变量」只看代码。
 *
 * 为什么需要它：不变量断言的是「代码里不出现 `self.readyState = `」，而注释里
 * **解释这条规则**时天然会写出同样的字面量。若直接在原始源码上匹配，任何人补一句
 * 注释就会让断言假红 —— 这类「守门测试自己变成噪音」的问题必须从机制上排除，
 * 而不是靠约定。
 * @param {string} text - JS 源码。
 * @returns {string} 注释被抹成空格（换行保留）、其余原样的源码。
 */
function stripComments(text) {
  let out = ''
  let state = 'code'
  for (let index = 0; index < text.length; index += 1) {
    const ch = text[index]
    const next = text[index + 1]
    if (state === 'code') {
      if (ch === '/' && next === '/') {
        state = 'line'
        out += '  '
        index += 1
        continue
      }
      if (ch === '/' && next === '*') {
        state = 'block'
        out += '  '
        index += 1
        continue
      }
      if (ch === "'" || ch === '"' || ch === '`') state = ch
      out += ch
      continue
    }
    if (state === 'line') {
      if (ch === '\n') {
        state = 'code'
        out += ch
      } else {
        out += ' '
      }
      continue
    }
    if (state === 'block') {
      if (ch === '*' && next === '/') {
        state = 'code'
        out += '  '
        index += 1
        continue
      }
      out += ch === '\n' ? '\n' : ' '
      continue
    }
    // 字符串内部：原样输出，处理转义与闭合引号。
    out += ch
    if (ch === '\\') {
      out += next === undefined ? '' : next
      index += 1
      continue
    }
    if (ch === state) state = 'code'
  }
  return out
}

/** 只含代码（注释已抹掉）的源码视图：断言「不该出现某写法」时用它。 */
const code = stripComments(source)

function expectMatch(name, pattern) {
  assert.match(source, pattern, name)
}

// 动态流量改写归 preload.js；这里再包一层会导致双重改写（URL 被改写两次）。
assert.equal(/XMLHttpRequest\s*=(?!=)/.test(code), false, "media-shim 不得接管 XMLHttpRequest")
assert.equal(/WebSocket\s*=(?!=)/.test(code), false, "media-shim 不得接管 WebSocket")
assert.equal(/window\.fetch\s*=(?!=)/.test(code), false, "media-shim 不得接管 fetch")
// WKWebView 上不存在这两个 API（spike 实测），依赖它们就永远拿不到音频。
assert.equal(/new AudioWorkletNode/.test(code), false, "不得使用 AudioWorkletNode")
assert.equal(/MediaStreamTrackGenerator\s*\(/.test(code), false, "不得使用 MediaStreamTrackGenerator")
// 合成 MediaStream 的三件套与顺序：ScriptProcessor 必须是唯一喂料通道。
expectMatch("建立在 MediaStreamAudioDestinationNode 上", /createMediaStreamDestination\(\)/)
expectMatch("用 ScriptProcessor 喂 PCM", /createScriptProcessor\(1024, 0, 1\)/)
expectMatch("AudioContext 固定 48k 且在 await 之前同步创建", /new AudioContextCtor\(\{ sampleRate: 48000/)
// 通道 token 必须用 Tauri 的 "__CHANNEL__:<id>" 前缀（Rust 侧按此前缀反解）。
assert.ok(source.includes("\x27__CHANNEL__:\x27 + channelId"), "通道 token 前缀必须正确")
// 原生 track.stop 必须在改写之前取到，否则改写后的 stop 会调用自己（无限递归）。
expectMatch("先取原生 stop 再改写", /var nativeStop = typeof track\.stop === \x27function\x27[\s\S]{0,400}?Object\.defineProperty\(track, \x27stop\x27/)
// getUserMedia 只替换 navigator.mediaDevices，不碰其它 navigator 成员。
expectMatch("装到 navigator.mediaDevices 上", /Object\.defineProperty\(navigator, \x27mediaDevices\x27/)
// 诊断通道必须是只读的冻结对象。
expectMatch("__DSH_MEDIA__ 为冻结对象", /__DSH_MEDIA__[\s\S]{0,80}?Object\.freeze\(/)

// ---- 真机缺陷回归（WKWebView 实机抓到，详见 docs/spikes/media-input.md）----
// `EventSource.prototype` 上的 `url` / `readyState` / `withCredentials` 是**只读访问器**，
// 普通赋值在严格模式里直接抛 "Attempted to assign to readonly property"，而延迟包装的
// 原型正是原生原型，所以只能用 `Object.defineProperty` 装自有数据属性去遮蔽。
assert.equal(
  /(?:^|[^.\w])this\.(?:url|readyState|withCredentials)\s*=(?!=)/m.test(code),
  false,
  '延迟 EventSource 不得向只读访问器普通赋值',
)
assert.equal(
  /self\.(?:url|readyState|withCredentials)\s*=(?!=)/m.test(code),
  false,
  '延迟 EventSource 不得向只读访问器普通赋值（self 形式）',
)
expectMatch('延迟 EventSource 用 defineProperty 装自有属性', /Object\.defineProperty\(self, key, \{/)
expectMatch('延迟对象仍继承原生原型（instanceof 成立）', /DeferredEventSource\.prototype = Native\.prototype/)
// 合成音频轨同理：label/kind/readyState 是原型 getter，getSettings/stop 是原型方法，
// 覆盖它们也必须走 defineProperty（否则真机同样抛只读错误）。
assert.equal(
  /track\.(?:label|kind|readyState|getSettings|stop)\s*=(?!=)/m.test(code),
  false,
  '不得对合成轨的原生属性普通赋值',
)


// ---- 不变量自身的活性与「注释免疫力」自证（防止守门测试变成噪音）----
assert.equal(
  /self\.readyState\s*=(?!=)/.test(stripComments("// self.readyState = 1\n")),
  false,
  '注释里的字面量不得触发只读赋值断言',
)
assert.equal(
  /track\.label\s*=(?!=)/.test(stripComments("// c\ntrack.label = 'x' // 真代码必须被检出\n")),
  true,
  '真实代码里的赋值仍然必须被检出（断言不能永远为真）',
)
assert.ok(
  stripComments("const origin = 'http://127.0.0.1:1/x' // 尾注释").includes("'http://127.0.0.1:1/x'"),
  '字符串字面量必须原样保留（不能把 // 当注释吃掉）',
)
assert.ok(
  !stripComments("const a = 1 /* track.label = */ + 2").includes('track.label'),
  '块注释必须被抹掉',
)
assert.ok(
  stripComments("let re = /ab/ // c").includes('/ab/'),
  '正则字面量不得被当成注释起点',
)

console.log("media-shim invariants: ok")

async function main() {
  await testEventSourceRewrite()
  await testDeferredEventSource()
  testDeferredSurvivesReadonlyAccessors()
  await testMicrophone()
  console.log('media-shim bridge: ok')
}

main().catch((error) => {
  console.error(error)
  process.exitCode = 1
})
