# Spike：WKWebView 的媒体输入能力（P3 麦克风）

> **状态（2026-10-25）**：结论已采纳并落地为
> `src-tauri/src/media/`（原生采集）+ `src-tauri/bridge/media-shim.js`（页面侧合成）。
> 本文保留原始实测证据与「被否掉的路线」，供后续换 WebView / 换平台时复用。

> 目的：`docs/desktop-parity.md` §9 记着一条「麦克风需原生采集」。这条决定了 P3 的
> 语音输入能不能做、要多久：
> WKWebView（macOS）**不实现 Media Capture**，而 dsh 前端的语音输入插件只认
> `navigator.mediaDevices.getUserMedia` + `MediaRecorder`。
> 本文回答三件事：① 页面侧能不能凭空合成一个`MediaStream`喂给现有前端；
> ② 合成用什么通道（AudioWorklet / WebCodecs / ScriptProcessor）；
> ③ 原生采集怎么做、有哪些系统门槛。

## 1. 环境与复现

| 项 | 值 |
| --- | --- |
| 机器 | macOS 27.0.1 (26A434)，arm64 |
| WebView | 系统 WebKit（WKWebView），与 Tauri/wry 0.57 用的是同一个引擎 |
| 前端 | 全局 `dsh` 0.2.1-rc.2 的 `dsh-web-frontend/dist` |
| 麦克风消费者 | `@deepseek-ai/dsh-experimental-client-ui-voice-input/lib/client.js`（唯一一处） |
| 探针 | 独立 `swiftc` 程序：`NSApplication` + 离屏 `WKWebView.loadHTMLString` + `evaluateJavaScript`，结果经 `WKScriptMessageHandler` 回传 |

复现方式（不依赖本仓库、不碰 `target/`）：

```bash
# 1) 起一个离屏 WKWebView 跑一段 JS（探针骨架）
swiftc -O probe.swift -o probe -framework WebKit -framework AppKit && ./probe
# 2) JS 里要测的断言见 §3；关键几条也可以直接在 DevTools（dev 构建）里重跑
```

探针骨架（两轮实验都用它）：

```swift
let web = WKWebView(frame: .zero, configuration: config)   // config 里挂 WKScriptMessageHandler(name: "report")
let window = NSWindow(contentRect: .init(x: 40, y: 40, width: 400, height: 300),
                      styleMask: [.titled], backing: .buffered, defer: false)
window.contentView = web; window.orderFrontRegardless()
web.loadHTMLString("<!doctype html><body>spike</body>", baseURL: nil)
web.evaluateJavaScript(js) { _, error in if let error { print(error) } }
app.run()   // 结果由 JS 里的 webkit.messageHandlers.report.postMessage(JSON.stringify(out)) 回传
```

> 关于 origin：探针跑在 `about:blank`（`isSecureContext === false`）。真实页面是
> `dsh-app://app`，spike [wkwebview-scheme.md](wkwebview-scheme.md) §3.1 已实测它是
> secure context。下面所有能力**在两种 origin 上都存在**（能力缺失不是 secure context
> 或权限问题 —— 这一点是本 spike 最重要的判断）。

## 2. 结论表（原始实测值）

| # | 问题 | 结论 | 关键证据 |
| --- | --- | --- | --- |
| 1 | `navigator.mediaDevices` 存在吗？ | **不存在**（复核 P1 时的结论仍成立） | `typeof navigator.mediaDevices === "undefined"`；`typeof navigator.getUserMedia === "undefined"`；`navigator.permissions.query({name:'microphone'})` → `state=prompt`（权限"看起来"可用，但 API 根本没有） |
| 2 | 能覆盖 `navigator.mediaDevices` 吗？ | **能，两条路都成功** | `Navigator.prototype` 上**没有**该属性（`Object.getOwnPropertyDescriptor` → absent）；`Object.defineProperty(navigator, 'mediaDevices', {value})` → ok；在 `Navigator.prototype` 装 getter → ok |
| 3 | `MediaRecorder` 存在吗？ | **存在**，默认 `audio/mp4; codecs=mp4a.40.2` | `isTypeSupported`：`audio/webm` true、`audio/webm;codecs=opus` true、`audio/mp4` true、`audio/mp4;codecs=mp4a.40.2` true、`audio/aac` false、`audio/wav` false |
| 4 | 能不能合成"真" MediaStream？ | **能**（`AudioContext.createMediaStreamDestination()`） | track：`kind=audio`、`label=MediaStreamAudioDestinationNode`、`readyState=live`；`new MediaStream([track])` ok |
| 5 | `AudioWorkletNode` 可用吗？ | **不可用** | `typeof AudioWorkletNode === "undefined"`；`ctx.audioWorklet` 是 undefined（`addModule` 抛 `TypeError: undefined is not an object`） |
| 6 | WebCodecs 的 `MediaStreamTrackGenerator` 可用吗？ | **不可用** | `typeof MediaStreamTrackGenerator === "undefined"`、`typeof MediaStreamTrackProcessor === "undefined"`（`AudioData` / `AudioEncoder` 反而存在，但没法变成 track） |
| 7 | `ScriptProcessorNode` 可用吗？ | **可用**（deprecated 但活着） | `typeof ctx.createScriptProcessor === "function"`；1024 帧块在 48 kHz 下 900 ms 内回调 42 次（理论 42.2） |
| 8 | **把外部 PCM 灌进 MediaStream，MediaRecorder 能录到吗？** | **能，且电平保真** | `createScriptProcessor(1024,0,1)` 写入幅度 0.5 的 440 Hz 正弦 → `MediaStreamAudioDestinationNode` → `new MediaRecorder(dest.stream)` 录 0.9 s → blob 16,883 B、`audio/mp4; codecs=mp4a.40.2` → `decodeAudioData` 解出 48 kHz / 0.872 s → **rms 0.351、peak 0.505**（写入幅度 0.5） |
| 9 | `decodeAudioData` 能吃 MediaRecorder 的 mp4 吗？ | **能**（这条是整条链的关键，插件就靠它把录音转成 WAV） | 同上：`decodedSampleRate=48000`、`decodedDuration=0.872`；随后 `OfflineAudioContext(1, …, 16000)` 重采样得到 12,245 帧、峰值 0.505 |
| 10 | 插件要的电平表（`createMediaStreamSource` + AnalyserNode）能工作吗？ | **能** | 对合成 stream 建 `MediaStreamAudioSourceNode` + `Analyser`，300 ms 后 `getFloatTimeDomainData` 的 RMS = **0.344** |
| 11 | 合成轨的 `getSettings()` 有设备信息吗？ | **是空对象**（但可覆盖） | `track.getSettings()` → `{}`；`Object.defineProperty(track,'getSettings',{value:…})` → ok，能返回 `{groupId:'g1'}` |
| 12 | `track.stop()` 会派发 `ended` 吗？ | **不派发** | 调用后 `readyState` 变 `ended`，但 `addEventListener('ended')` 的回调**一次都没触发** —— 页面侧必须自己补发（插件用 `ended` 做中断检测） |
| 13 | `new AudioContext({sampleRate: 48000})` 生效吗？ | **生效** | `sampleRate=48000`（不传时也是 48000）、`baseLatency≈0.0027` |
| 14 | AudioContext 初始状态 | `suspended`，`resume()` 可成功 | `initialState=suspended` → `afterResume=running`（本探针没有用户手势也能恢复，但产品路径本来就是点击触发，按"必须在手势任务里创建"实现更稳） |

### 前端实际链路（决定了上面必须全绿）

`dsh-experimental-client-ui-voice-input/lib/client.js`：

```text
navigator.mediaDevices.getUserMedia({audio:{echoCancellation,noiseSuppression,deviceId:{exact}}})
  → new AudioContext() + createAnalyser + createMediaStreamSource(stream)   // 画电平
  → new MediaRecorder(stream)                                              // 不传 mimeType
  → Blob(chunks, recorder.mimeType) → context.decodeAudioData(arrayBuffer)
  → OfflineAudioContext(1, …, 16000) 重采样 → encodeWave() → base64 上传
```

另有 `media.enumerateDevices()`（过滤 `audioinput`，按 `deviceId==='default'` 找系统默认项、
按 `groupId` 反查名字）与 `track.getSettings().groupId`、`track.addEventListener('ended')`。
**结论：只要 `getUserMedia` 能返回一个"活的" MediaStream，前端一行都不用改。**

## 3. 选定路线

```text
AVCaptureSession + AVCaptureAudioDataOutput（src-tauri/src/media/macos.rs）
  → 20 ms 单声道 f32 PCM（pcm.rs：下混 / 重采样 / 定长分帧）
  → Tauri Channel<InvokeResponseBody> 二进制帧（≥1024 B 走内部 fetch 通道）
  → media-shim.js：PcmRing（吸收 20 ms 帧 vs 21.3 ms 块的时钟差）
  → ScriptProcessorNode → MediaStreamAudioDestinationNode.stream
  → 作为 getUserMedia() 的返回值交给前端
```

关键选择与理由：

- **传输用 Tauri `Channel` 而不是 event**：`Channel<InvokeResponseBody>::send(Raw(Vec<u8>))` 直推
  二进制；48 kHz f32 单声道 20 ms = 3840 B/帧 = 192 KB/s。若走 JSON event，同一份数据要变成
  约 500 KB/s 的文本并让页面每 20 ms `JSON.parse` 一次。JS 侧只需
  `transformCallback(fn)` + 传字符串 `"__CHANNEL__:<id>"`（`IPC_PAYLOAD_PREFIX` 在
  tauri 2.12.4 里就是 `__CHANNEL__:`），无需 `@tauri-apps/api`。
- **AudioContext 必须在用户手势的同一任务里同步创建**：`getUserMedia()` 里先建
  `AudioContext`（`sampleRate: 48000`）+ `resume()`，再去 `await` 原生启动，否则 WebKit 可能把
  上下文挂在 `suspended`，`ScriptProcessor` 永远不回调。
- **`track.stop()` 必须被劫持**：插件停止录制的方式就是 `stream.getTracks().forEach(t => t.stop())`，
  而合成轨的原生 `stop()` 只把自己置成 ended（且不派发事件），不会停 Rust 侧的采集。所以
  shim 在实例上重定义 `stop`：先调原生 `stop`（先取到原生引用，避免自递归），再拆会话并
  手动 `dispatchEvent(new Event('ended'))`。

## 4. 被否掉的路线

| 路线 | 为什么否掉 |
| --- | --- |
| `MediaStreamTrackGenerator` + `AudioData` 写帧（WebCodecs 正路） | 这台机器的 WKWebView 里该构造器不存在（§2 #6） |
| `AudioWorklet` 生成采样 | `ctx.audioWorklet` 是 undefined，`addModule` 直接抛（§2 #5）；`ScriptProcessorNode` 是唯一可用通道 |
| 只做 Rust 侧采集 + 转写，页面不拿 MediaStream | 要改前端（语音输入插件的电平、录制、WAV 重编码全在页面侧），且与官方语义偏离；工作量更大 |
| 让 WKWebView 走 WebRTC / `getUserMedia` 的私有开关 | WKWebView 无公开 API；`WKUIDelegate` 的 `requestMediaCapturePermissionFor` 只在 iOS 生效，且底层仍要求 Media Capture 实现 |
| 用 `AVAudioEngine.inputNode` tap 代替 AVCaptureSession | 可行但引动输出设备（`mainMixerNode`），macOS 上更容易与正在播放的音频打架；`AVCaptureSession` 与 TCC/设备枚举是同一套模型，选它是为了与 Electron 官方的权限模型对齐（`AVCaptureDevice.authorizationStatus` / `requestAccess`） |
| 页面侧 `MediaRecorder` 换成 Rust 编码（AVAssetWriter） | 插件用 `MediaRecorder` 产出的 blob 再 `decodeAudioData` 重采样成 16 kHz WAV；Rust 编码要复刻 `encodeWave` 的上传约定，纯属绕远 |

## 4.1 绑定层实测（编译期证据，来自把 `media/macos.rs` 编进 crate 的那一轮）

| 发现 | 证据 | 处理 |
| --- | --- | --- |
| `AVCaptureDevice.groupID` 在 **objc2-av-foundation 0.3.2 里没有导出** | `error[E0599]: no method named `groupID` found for struct `Retained<AVCaptureDevice>``（该 crate 的 `AVCaptureDevice.rs` 里搜不到 `groupID`） | 用 `uniqueID` 充当 group id。页面只用 group 把「当前设备」与 `enumerateDevices()` 的条目对齐，两侧同源（`status()` 与通道首帧都从同一个 `DeviceInfo` 出），所以设备选择行为不变；但**这个值不是真正的 hardware group id** —— 若将来要区分「同一只设备的输入/输出」，需要自己走 `AudioObjectGetPropertyData(kAudioDevicePropertyDeviceUID)` 或等 objc2 补绑定 |
| `AVCaptureDevice.devicesWithMediaType:` 被 objc2 标为 **deprecated**（"Use AVCaptureDeviceDiscoverySession instead"） | `cargo clippy -D warnings` 报 `use of deprecated associated function`；而 `AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes:mediaType:position:` 要求调用方自建 `NSArray<AVCaptureDeviceType>`，`&'static` 常量无法直接进数组（需要 `Retained::retain` 手工组数） | 唯一一处枚举点加 `#[allow(deprecated)]` 并写明理由；行为与发现会话一致（音频输入设备全集），等 objc2 提供便利构造再换 |
| `CMSampleBuffer::audio_buffer_list_with_retained_block_buffer` 与 `CMAudioFormatDescriptionGetStreamBasicDescription` 被 **`objc2-core-media` 的 `objc2-core-audio-types` feature 门控** | 未开该 feature 时：`error[E0599]: no method named ...` / `error[E0432]: unresolved import` | 在 `objc2-core-media` 的 features 里补 `"objc2-core-audio-types"`（不新增 crate，lock 里本来就有） |

## 4.2 真机缺陷：延迟 `EventSource` 撞上原生只读访问器（release 构建，已修）

> 这是本 spike 里**最有价值的一条证据**：它证明「Node 假环境测原生对象表面」不够。

**现象**：release 构建真机启动时，页面里每个 `new EventSource(...)` 都抛
`Attempted to assign to readonly property.`。触发点是 Web UI 的
`@deepseek-ai/dsh-client-hmr` 条目（它用 `new EventSource(EVENTS_ROUTE)`，路由是**相对路径**，
因此必然走我们的重写/延迟路径）。一个条目没激活 → `web boot: 1 entry did not activate`
→ 整个 web boot 失败，壳跳错误页。**症状是"壳坏了"，根因在一条 dev 期才常用的通道上。**

**根因**：延迟包装为了让 `instanceof EventSource` 成立，把
`DeferredEventSource.prototype = Native.prototype`，于是它继承的是**原生原型**；而
`EventSource.prototype` 上的 `url` / `readyState` / `withCredentials` 是**只读访问器**
（WebIDL 的 `readonly attribute`）。构造函数里 `this.url = url`、`this.readyState = 0`
这类普通赋值在严格模式下直接抛错 —— 页面只是**构造**一个 EventSource 就崩了。

**为什么单测没抓到**：测试桩 `FakeEventSource` 用的是普通数据属性（`this.url = url` 在桩上完全合法）。
桩把"原生对象的表面"简化掉了，等于把最危险的差异测没了。

**修法**（现在 `media-shim.js` 里的形态）：

- 延迟对象的对外表面一律用 `Object.defineProperty` 装**自有数据属性**去遮蔽继承来的访问器，
  绝不用普通赋值；`connect()` / `close()` 里更新 `url` / `readyState` 同样走 `define`；
- `addEventListener` / `removeEventListener` / `dispatchEvent` / `close` 这四个"遮蔽原生方法"的
  位置也统一走 `define`：方法本身是数据属性、普通赋值本该可行，但同一个对象上已经栽过一次，
  不值得赌第二次（将来 WebKit 把某个方法改成访问器就会重现同类故障）；
- 代码里留了注释写明"为什么不能普通赋值"，避免后人改回去。

**回归测试**（`bridge/media-shim.test.cjs`）：

- `ReadonlyEventSource`：原型上 `url` / `readyState` 只有 getter，**复刻 WKWebView**；
  用例 `deferred event source survives readonly accessors` 断言构造不抛错、`url` 可读回、
  `readyState === 0`、`instanceof` 仍成立；
- 合成音频轨的测试桩同时升级为「原型上有只读访问器（`label`/`kind`/`readyState`）
  + 原型方法（`getSettings`/`stop`）」的形状，覆盖麦克风侧同类风险；
- 源码不变量：`this.url = ...` / `self.readyState = ...` / `track.label = ...` 这类普通赋值
  一律断言**不存在**。断言前会先用 `stripComments()` 把注释抹成空格（保留字符串字面量与
  换行），因此**在注释里复述规则不会假红**；同时自带三条自证：注释不触发 / 真实代码仍被
  检出 / `'http://…'` 这类字面量不被当成注释吃掉。

**同类风险审计结论**（`media-shim.js` 全量）：

| 位置 | 处理 | 是否安全 |
| --- | --- | --- |
| 延迟 EventSource 的 `url`/`readyState`/`withCredentials` | `define` 遮蔽只读访问器 | ✅（真机缺陷修复点） |
| 延迟 EventSource 的四个原型方法 | `define` 遮蔽 | ✅（预防性统一） |
| 延迟 EventSource 的 `onopen`/`onmessage`/… | `define` 自有数据属性 | ✅ |
| 合成轨的 `stop` / `getSettings` / `label` | `defineProperty` 覆盖实例属性（`label`/`getSettings` 的真机可覆盖性已在 §2 #11 实测） | ✅ |
| `navigator.mediaDevices` | `defineProperty`（两条路都实测过，见 §2 #2） | ✅ |
| `window.EventSource = replacement` | 先普通赋值 + `try/catch` 回退到 `defineProperty` | ✅（全局接口对象可写；回退兜底） |
| `PcmRing` 的 `this.capacity/...` | 自有类，非原生对象 | ✅ 无风险 |

**遗留（同一类，但需要新依赖）**：`readyState` 在建连后是"事件驱动地跟随 native"
（`open`/`message`/`error` 回调里同步一次），不是实时值。现有消费者（`dsh-client-hmr`、
`documentpreview`）都只用 `addEventListener('message')`、**不读 `readyState`**，所以当前无害；
若将来有消费者轮询它，正确修法是把它改成**自有 getter**：`get() { return native === null ? (closed ? 2 : 0) : native.readyState }`。

**教训（已并入验收流程）**：凡是「在原生对象/原生原型派生对象上做手脚」的代码，
① 桩必须复刻**属性形态**（getter-only / 方法 / 数据属性），而不只是属性值；
② 能在真机跑就别只跑单测；release 构建 + 截图必须进验收。

## 5. 系统门槛（真机验收前必须知道）

1. **`Info.plist` 必须有 `NSMicrophoneUsageDescription`**（本仓库已加在 `src-tauri/Info.plist`）。
   缺它时系统**不会**弹授权窗，`AVCaptureDevice` 一直返回静音数据 —— 表现为"能录音但全是静音"，
   非常难查。
2. TCC 授权按 **bundle** 记一次：本地签名（ad-hoc）每次重建签名可能让系统重新问一遍。
3. 授权**之前** `AVCaptureDevice.devicesWithMediaType:` 返回空数组（macOS 行为），所以
   "设备清单为空"不能当成"机器没有麦克风"；本实现先走 `requestAccess` 再重新枚举
   （`media::permission::start_decision` 里有对应单测）。
4. 公证/上架时还需要 `com.apple.security.device.audio-input` entitlement；本地 ad-hoc 签名
   刻意**不加**（加了反而可能让本地签名构建失败），等做公证时随 hardened runtime 一起加。
5. 委托回调在**非主线程**：回调里只做取字节 + 推帧，任何 UI/窗口操作都不能放进去。

## 6. 未验证项（诚实记录）

- **CPU 占用**：未测。预期可忽略（48 kHz 单声道 f32 的下混/重采样/编码是几 MB/s 级的内存搬运），
  但 `ScriptProcessorNode` 在主线程上跑，极端卡顿时会丢块 —— 页面侧 `PcmRing` 的
  `dropped/starved/overruns` 计数就是留给这种情况的（`__DSH_MEDIA__.diagnostics()`）。
- **端到端延迟**：20 ms 帧 + 21.3 ms 块 + MediaRecorder 缓冲，预期 60–120 ms。语音输入
  （非通话）完全够用；若将来要做实时字幕再看。
- **长时间漂移**：设备时钟与 `AudioContext` 时钟不同源，`PcmRing` 用"欠载补静音 / 积压丢最旧"
  硬扛，未做基于时间戳的相位追踪。10 分钟以上的录音若出现周期性丢/补，再上正式的重采样同步。
- **MediaRecorder 的容器**：默认给 `audio/mp4`（AAC）。插件不关心容器（它 `decodeAudioData`
  后重编码成 WAV），所以对我们无害；但若将来有别的消费方直接把 blob 上传给只认 webm/opus 的
  服务，会踩到这条差异。
- **多声道**：只输出单声道（页面也只用一条 ScriptProcessor 输出通道），设备是立体声阵列时会先下混。

- **设备中断/拔出未实现**（已知缺口，本轮不做）：只观察了「页面主动 stop」，没有观察
  `AVCaptureSessionRuntimeErrorNotification` / `AVCaptureSessionWasInterruptedNotification`。
  当前行为：拔掉 USB 麦克风或被其它进程独占后，**电平归零、继续录静音**，页面的「录音中断」
  提示**不会出现**（`ended` 事件只有主动 stop 时才由壳补发）；用户路径是重新点一次语音输入
  （会先 `stop()` 再 `start()`，状态可自愈）。
  将来要补的话：加 `objc2-foundation`（lock 里已有 0.3.2），用
  `NSNotificationCenter.addObserverForName:object:queue:usingBlock:` 观察上面两个通知 → 置
  "通道已断"标记 → 复用现有看门狗路径 `stop()` 并补发 `ended`，约 40 行 FFI；**必须配一次
  真机插拔验收**才算完成。

## 7. 与官方（Electron）的差异

| 项 | 官方 | 本项目 |
| --- | --- | --- |
| 权限 | `session.setPermissionRequestHandler` + `systemPreferences.askForMediaAccess('microphone')` | `AVCaptureDevice.authorizationStatus/requestAccess`（同一套 TCC） |
| 采集 | Chromium 内置媒体栈（`getUserMedia` 直接可用） | `AVCaptureSession` 原生采集 + 页面合成 `MediaStream` |
| 设备列表 | Chromium 枚举 + `deviceId` 语义 | 自建：`enumerateDevices()` 返回 `default` 项 + 真实设备；`groupId` 用 `uniqueID` 顶替（objc2 绑定未导出 `groupID`，见 §4.1） |
| 采样率 | Chromium 自动重采样 | 壳按页面 `AudioContext.sampleRate`（48 kHz）出帧；源率不是 48 kHz 时壳侧重采样 |
| 电平/录制 | 原生 MediaStream | 页面 `ScriptProcessorNode` 喂 `MediaStreamAudioDestinationNode`；语义等价、多一层缓冲 |

### 7.1 已知差异（桥级细节，不进 parity §9）

| 项 | 官方（Electron） | 本项目 | 影响 |
| --- | --- | --- | --- |
| 延迟 `EventSource.url` | 始终是解析后的**绝对** URL | 基址就绪前存构造时的原始字符串（可能是相对路径），`open` 事件到达后才换成改写后的 native URL | 现有消费者（`dsh-client-hmr`、`documentpreview`）都不读 `.url`，无实际影响 |
| 延迟 `EventSource.readyState` | 实时反映连接状态 | 事件驱动跟随（`open`/`message`/`error` 回调里各同步一次），事件之间可能滞后 | 无消费者读它；若将来有人轮询，改成自有 getter 委托 `native.readyState` 即可（`media-shim.js` 注释里写了做法） |

## 8. 真机验收步骤（Lead 执行）

1. `./scripts/build-local-macos.sh --install`（签名 dmg → `/Applications`），首次启动。
2. 打开侧栏/输入框的**语音输入**（麦克风图标）→ **首次应弹系统授权窗**
   （文案来自 `NSMicrophoneUsageDescription`：「dsh desktop 需要麦克风…」）→ 点"允许"。
3. 允许后：**电平条随说话起伏**（这是 §2 #10 的 analyser 在跑）；不弹"麦克风不可用"错误。
4. 说一句话后停止：应生成一段录音并上传转写（前端会把 `MediaRecorder` 的 mp4 解码、
   重采样成 16 kHz WAV 再上传；上传请求在日志里是 `/api/…` 的 POST）。
5. 反向用例：系统设置 → 隐私与安全性 → 麦克风里关掉 dsh desktop，再点语音输入 →
   应显示**权限被拒**（页面拿到 `NotAllowedError`），而不是卡住或静默。
6. 中断用例（**已知缺口，验收时按"记录现象"而不是"要求通过"**）：录音中拔掉 USB 麦克风 /
   切到别的输入设备 → 当前行为是**电平归零、继续录静音**，页面的「录音中断」提示**不出现**
   （只有主动 stop 才补发 `ended`）。把这一现象记进验收记录即可；修复方案与依赖见 §6 未验证项。
   随后重新点一次语音输入应能正常恢复（内部会先 `stop()` 再 `start()`）。
