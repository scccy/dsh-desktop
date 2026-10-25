/**
 * DSH Desktop 的 `<webview>` 兼容层（Sidebar Browser）。
 *
 * 官方桌面端用的是 Electron 的 `<webview>` 标签：标签本身是 Chromium 的 guest view，
 * 位置/层级由浏览器排版决定，导航 API（`loadURL`/`goBack`/`getURL`…）与 `did-*` 事件
 * 都由标签提供。Tauri 没有这个标签，等价物是窗口的**子 webview**（原生视图，浮在页面之上）。
 *
 * 这一层负责把「页面里的元素」接到「原生子 webview」：
 *
 *  1. 定义自定义元素 `webview`，提供官方 `<webview>` 里前端真正用到的那部分 API
 *     （`loadURL` / `getURL` / `getTitle` / `canGoBack` / `canGoForward` / `isLoading` /
 *     `goBack` / `goForward` / `reload` / `clearHistory`）与事件（`dom-ready` /
 *     `did-start-navigation` / `did-start-loading` / `did-stop-loading` / `did-navigate` /
 *     `did-navigate-in-page` / `page-title-updated` / `did-fail-load` / `destroyed`）；
 *  2. 观测元素的位置与可见性（ResizeObserver / IntersectionObserver / scroll / resize /
 *     visibilitychange + 500ms 兜底），把 CSS px 矩形报给 Rust，由 Rust 换算成物理像素、
 *     裁剪到 content webview 的可见区，再调用子 webview 的 set_bounds / show / hide；
 *  3. 把 Rust 推来的状态快照（`dsh-desktop://browser-guest-state`）差分成上面那些 DOM 事件。
 *
 * 与 Electron 的已知差异（见 docs/desktop-parity.md §9）：
 *  - 没有 `did-fail-load` 的等价回调：只有「建 guest 失败」会派发它，页面内导航失败表现为
 *    loading 结束但 URL 不变；
 *  - `canGoBack` / `canGoForward` 来自 Rust 侧线性历史，`goBack` 是重新导航而不是历史恢复；
 *  - 元素本身只是占位（原生视图浮在上面），所以 `element.style` 上的滤镜/透明度/overflow
 *    裁剪不会作用到页面内容；透明度与 display/visibility 只用于判定「该不该显示」。
 *
 * 注入位置：content webview 的 initialization_script，**必须与** preload.js 里
 * `__DSH_BRIDGE_CAPS__.browserGuests` 同一个开关一起开（只开一边会让 Web UI 走进没实现的分支）。
 */
;(function () {
  'use strict'

  var shellWindow = typeof window === 'object' && window !== null ? window : null
  if (shellWindow === null) return
  if (typeof document !== 'object' || document === null) return

  var mainFrame = true
  try {
    mainFrame = shellWindow.top === shellWindow
  } catch (error) {
    mainFrame = false
  }
  // 兼容层只在主 frame 装：guest 由主 frame 里的那个 <webview> 唯一承载。
  if (!mainFrame) return
  if (typeof customElements !== 'object' || customElements === null) return
  if (typeof customElements.get === 'function' && customElements.get('webview') !== undefined) return

  /** 事件名：Rust 侧 `guest::EVENT_GUEST_STATE`。 */
  var EVENT_GUEST_STATE = 'dsh-desktop://browser-guest-state'
  /** 事件名：Rust 侧 `guest::EVENT_GUEST_DOWNLOAD`（下载被拦下的上报）。 */
  var EVENT_GUEST_DOWNLOAD = 'dsh-desktop://browser-download-blocked'
  /** 元素位置上报的兜底节拍（毫秒）：CSS 过渡、侧栏动画不一定触发观察器。 */
  var BOUNDS_INTERVAL = 500
  /** 位置上报的抖动阈值（CSS px）。 */
  var BOUNDS_EPSILON = 0.5

  /**
   * 报一条壳日志（`desktop_console` 命令，release 无 inspector 时的排障通道）。
   * @param {string} message - 短描述。
   * @param {unknown} error - 可选原因。
   */
  function warn(message, error) {
    var text = '[dsh desktop] ' + message
    try {
      console.warn(text, error)
    } catch (ignored) {
      // 页面换掉了 console 也不能让兼容层挂掉。
    }
    try {
      var api = shellWindow.__TAURI_INTERNALS__
      if (api === undefined || api === null || typeof api.invoke !== 'function') return
      var detail = error === undefined ? '' : ': ' + (error && error.message ? error.message : String(error))
      void api.invoke('desktop_console', { level: 'warn', message: text + detail })
    } catch (ignored) {
      // 诊断通道本身不可用时静默。
    }
  }

  /**
   * 调一个 Tauri 命令。
   * @param {string} command - 命令名。
   * @param {Record<string, unknown>} args - camelCase 参数。
   * @returns {Promise<any>}
   */
  function invoke(command, args) {
    var api = shellWindow.__TAURI_INTERNALS__
    if (api === undefined || api === null || typeof api.invoke !== 'function') {
      return Promise.reject(new Error('dsh desktop: the shell IPC is unavailable'))
    }
    return api.invoke(command, args === undefined ? {} : args)
  }

  /**
   * 订阅一个壳事件。
   * @param {string} event - 事件名。
   * @param {(payload: any) => void} listener - 接收 payload。
   * @returns {() => void} 取消订阅。
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
        warn(event + ' listener failed', error)
      }
    })
    var registration = invoke('plugin:event|listen', { event: event, target: target, handler: handler })
    var active = true
    return function () {
      if (!active) return
      active = false
      registration.then(
        function (eventId) {
          void invoke('plugin:event|unlisten', { event: event, target: target, eventId: eventId })
        },
        function () {},
      )
    }
  }

  /**
   * 两个 URL 是否指向同一个文档（只差 fragment）。
   * @param {string} left - 旧地址。
   * @param {string} right - 新地址。
   * @returns {boolean}
   */
  function sameDocument(left, right) {
    if (left === '' || right === '') return false
    try {
      var a = new URL(left)
      var b = new URL(right)
      return a.protocol === b.protocol && a.host === b.host && a.pathname === b.pathname && a.search === b.search
    } catch (error) {
      return false
    }
  }

  /**
   * 造一个带 `isMainFrame: true` 的事件（前端按 Electron 的形状读这个字段）。
   * @param {string} name - 事件名。
   * @returns {Event}
   */
  function mainFrameEvent(name) {
    var event = new Event(name)
    try {
      Object.defineProperty(event, 'isMainFrame', { value: true, enumerable: true })
    } catch (ignored) {
      // 某些引擎不允许给 Event 加属性；前端只在主 frame 里读它，缺了也不致命。
    }
    return event
  }

  /**
   * 元素此刻是否真的显示（display / visibility / opacity / content-visibility）。
   * @param {Element} element - 待判定元素。
   * @returns {boolean}
   */
  function isRendered(element) {
    if (typeof element.checkVisibility === 'function') {
      try {
        return element.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true, contentVisibilityAuto: true })
      } catch (error) {
        // 老引擎不认识参数：回落到样式判定。
      }
    }
    var style = shellWindow.getComputedStyle(element)
    return style.display !== 'none' && style.visibility !== 'hidden' && style.opacity !== '0'
  }

  /**
   * 量出元素在视口里的矩形（CSS px，已裁剪）。
   * @param {Element} element - `<webview>` 元素。
   * @returns {{x: number, y: number, width: number, height: number} | null} null 表示不该显示。
   */
  function measure(element) {
    if (element.isConnected !== true) return null
    if (document.visibilityState !== 'visible') return null
    if (!isRendered(element)) return null
    var rect = element.getBoundingClientRect()
    if (!(rect.width > BOUNDS_EPSILON) || !(rect.height > BOUNDS_EPSILON)) return null
    var left = Math.max(rect.left, 0)
    var top = Math.max(rect.top, 0)
    var right = Math.min(rect.right, shellWindow.innerWidth)
    var bottom = Math.min(rect.bottom, shellWindow.innerHeight)
    if (right - left <= BOUNDS_EPSILON || bottom - top <= BOUNDS_EPSILON) return null
    return { x: left, y: top, width: right - left, height: bottom - top }
  }

  /**
   * 空快照。
   * @param {string} lease - lease 标识。
   * @returns {object}
   */
  function emptySnapshot(lease) {
    return {
      lease: lease,
      url: '',
      title: '',
      loading: false,
      canGoBack: false,
      canGoForward: false,
      visible: false,
    }
  }

  /** lease → 元素：状态事件靠它找到该派发给谁。 */
  var registry = Object.create(null)

  /**
   * 页面里的 `<webview>`：占位元素 + 原生子 webview 的遥控器。
   */
  class WebviewElement extends HTMLElement {
    constructor() {
      super()
      this.__lease = ''
      this.__partition = ''
      this.__snapshot = emptySnapshot('')
      this.__attachPromise = null
      this.__teardown = 0
      this.__frame = 0
      this.__timer = 0
      this.__lastBounds = null
      this.__observers = []
      this.__scheduleListener = null
    }

    // -- 官方 `<webview>` 的导航 API（形状与 Electron 一致）-------------------------

    /**
     * 导航到绝对 URL。
     * @param {string} url - 目标地址。
     * @returns {Promise<void>} 导航**开始**后即 resolve（Electron 在加载开始后 resolve）。
     */
    loadURL(url) {
      var self = this
      var target = String(url)
      return self.__attach().then(function () {
        return invoke('desktop_browser_guest_navigate', { lease: self.__lease, url: target }).then(function (snapshot) {
          self.__apply(snapshot)
        })
      })
    }

    /** @returns {string} 当前已提交的地址。 */
    getURL() {
      return this.__snapshot.url
    }

    /** @returns {string} 当前文档标题。 */
    getTitle() {
      return this.__snapshot.title
    }

    /** @returns {boolean} 线性历史里是否还能后退（近似 Electron 语义）。 */
    canGoBack() {
      return this.__snapshot.canGoBack
    }

    /** @returns {boolean} 线性历史里是否还能前进。 */
    canGoForward() {
      return this.__snapshot.canGoForward
    }

    /** @returns {boolean} 主 frame 是否在加载。 */
    isLoading() {
      return this.__snapshot.loading
    }

    /** 后退一步（重新导航到上一条记录）。 */
    goBack() {
      void this.__command('back')
    }

    /** 前进一步。 */
    goForward() {
      void this.__command('forward')
    }

    /** 重新加载当前文档。 */
    reload() {
      void this.__command('reload')
    }

    /** 清空历史，只保留当前文档。 */
    clearHistory() {
      void this.__command('clear-history')
    }

    // -- 生命周期 ----------------------------------------------------------------

    connectedCallback() {
      var self = this
      if (self.__teardown !== 0) {
        clearTimeout(self.__teardown)
        self.__teardown = 0
      }
      self.__lease = self.getAttribute('name') || ''
      self.__partition = self.getAttribute('partition') || ''
      self.__startObservers()
      self.__attach().catch(function () {
        // 失败已经派发过 did-fail-load，这里只需要不让 promise 变成未处理拒绝。
      })
    }

    disconnectedCallback() {
      var self = this
      self.__stopObservers()
      if (self.__teardown !== 0) return
      // 延迟一个宏任务再销毁：React 在同一个任务里挪动节点时不会白白丢掉 guest。
      self.__teardown = setTimeout(function () {
        self.__teardown = 0
        if (self.isConnected) return
        self.__detach()
      }, 0)
    }

    // -- 与 Rust 的往返 -----------------------------------------------------------

    /** 建子 webview（幂等）。@returns {Promise<object>} 初始快照。 */
    __attach() {
      var self = this
      if (self.__attachPromise !== null) return self.__attachPromise
      if (self.__lease === '') {
        var missing = new Error('dsh desktop: browser guest lease is missing')
        self.__fail('browser guest lease is missing')
        return Promise.reject(missing)
      }
      self.__attachPromise = invoke('desktop_browser_guest_attach', {
        lease: self.__lease,
        partition: self.__partition,
      }).then(
        function (snapshot) {
          // 初始文档是 about:blank，不该被当成一次用户导航。
          self.__apply(snapshot, true)
          self.__schedule()
          self.dispatchEvent(new Event('dom-ready'))
          return snapshot
        },
        function (error) {
          self.__attachPromise = null
          self.__fail(error && error.message ? error.message : String(error))
          throw error
        },
      )
      return self.__attachPromise
    }

    /** 销毁子 webview 并派发 destroyed。 */
    __detach() {
      var self = this
      var lease = self.__lease
      self.__attachPromise = null
      self.__lastBounds = null
      if (lease === '') return
      if (registry[lease] === self) delete registry[lease]
      self.__snapshot = emptySnapshot(lease)
      invoke('desktop_browser_guest_close', { lease: lease }).catch(function (error) {
        warn('could not close the browser guest', error)
      })
      self.dispatchEvent(new Event('destroyed'))
    }

    /**
     * 执行一条导航命令。
     * @param {string} command - back / forward / reload / clear-history。
     * @returns {Promise<void>}
     */
    __command(command) {
      var self = this
      return self.__attach().then(function () {
        return invoke('desktop_browser_guest_command', { lease: self.__lease, command: command }).then(function (snapshot) {
          self.__apply(snapshot)
        })
      })
    }

    /**
     * 应用一份状态快照，并按差异派发 DOM 事件。
     * @param {any} snapshot - Rust 推来的 `GuestSnapshot`。
     * @param {boolean} silent - 只更新状态，不派发事件（初次挂载用）。
     */
    __apply(snapshot, silent) {
      if (snapshot === null || typeof snapshot !== 'object') return
      var self = this
      var previous = self.__snapshot
      var next = {
        lease: typeof snapshot.lease === 'string' && snapshot.lease !== '' ? snapshot.lease : self.__lease,
        url: typeof snapshot.url === 'string' ? snapshot.url : '',
        title: typeof snapshot.title === 'string' ? snapshot.title : '',
        loading: snapshot.loading === true,
        canGoBack: snapshot.canGoBack === true,
        canGoForward: snapshot.canGoForward === true,
        visible: snapshot.visible === true,
      }
      self.__snapshot = next
      if (self.__lease === '' && next.lease !== '') self.__lease = next.lease
      if (next.lease !== '') registry[next.lease] = self
      if (silent === true) return

      if (next.title !== previous.title) {
        // 对齐官方 presentation.show(title)：标题同时作为无障碍标签。
        try {
          self.setAttribute('aria-label', next.title)
        } catch (ignored) {
          // 元素已脱离文档时忽略。
        }
        self.dispatchEvent(new Event('page-title-updated'))
      }
      if (next.url !== previous.url && next.url !== '') {
        var inPage = sameDocument(previous.url, next.url)
        self.dispatchEvent(mainFrameEvent(inPage ? 'did-navigate-in-page' : 'did-navigate'))
      }
      if (next.loading !== previous.loading) {
        if (next.loading) {
          self.dispatchEvent(mainFrameEvent('did-start-navigation'))
          self.dispatchEvent(new Event('did-start-loading'))
        } else {
          self.dispatchEvent(new Event('did-stop-loading'))
        }
      }
    }

    /** 派发加载失败（只有「建 guest 失败」会走到这里，见文件头）。 */
    __fail(description) {
      var event = mainFrameEvent('did-fail-load')
      try {
        Object.defineProperty(event, 'errorCode', { value: -2, enumerable: true })
        Object.defineProperty(event, 'errorDescription', { value: description, enumerable: true })
      } catch (ignored) {
        // 同上：拿不到扩展属性时前端会当作「无 code 的失败」处理。
      }
      this.dispatchEvent(event)
    }

    // -- 几何 / 可见性同步 ---------------------------------------------------------

    /** 订一次下一帧的同步。 */
    __schedule() {
      var self = this
      if (self.__frame !== 0) return
      self.__frame = shellWindow.requestAnimationFrame(function () {
        self.__frame = 0
        self.__flush()
      })
    }

    /** 量一次并上报（有变化才上报，抖动阈值 0.5 CSS px）。 */
    __flush() {
      var self = this
      if (self.__attachPromise === null || self.__lease === '') return
      if (self.isConnected !== true) return
      var measured = measure(self)
      var previous = self.__lastBounds
      var same = false
      if (previous !== null) {
        if (measured === null) same = previous.visible === false
        else {
          same = previous.visible === true
            && Math.abs(previous.x - measured.x) < BOUNDS_EPSILON
            && Math.abs(previous.y - measured.y) < BOUNDS_EPSILON
            && Math.abs(previous.width - measured.width) < BOUNDS_EPSILON
            && Math.abs(previous.height - measured.height) < BOUNDS_EPSILON
        }
      }
      if (same) return
      self.__lastBounds = measured === null
        ? { visible: false, x: 0, y: 0, width: 0, height: 0 }
        : { visible: true, x: measured.x, y: measured.y, width: measured.width, height: measured.height }
      invoke('desktop_browser_guest_bounds', { lease: self.__lease, bounds: self.__lastBounds }).then(
        function (snapshot) {
          self.__apply(snapshot)
        },
        function (error) {
          warn('could not place the browser guest', error)
        },
      )
    }

    /** 装几何观察器。 */
    __startObservers() {
      var self = this
      self.__stopObservers()
      var schedule = function () {
        self.__schedule()
      }
      if (typeof ResizeObserver === 'function') {
        var resize = new ResizeObserver(schedule)
        resize.observe(self)
        if (document.documentElement !== null) resize.observe(document.documentElement)
        self.__observers.push(resize)
      }
      if (typeof IntersectionObserver === 'function') {
        var intersection = new IntersectionObserver(schedule, { threshold: [0, 0.01, 1] })
        intersection.observe(self)
        self.__observers.push(intersection)
      }
      shellWindow.addEventListener('resize', schedule)
      // scroll 不冒泡，只能用捕获阶段听。
      shellWindow.addEventListener('scroll', schedule, true)
      document.addEventListener('scroll', schedule, true)
      document.addEventListener('visibilitychange', schedule)
      self.__scheduleListener = schedule
      // 兜底节拍：CSS 过渡与宿主侧布局变化不一定触发上面的观察器。
      self.__timer = setInterval(function () {
        if (self.isConnected) self.__flush()
      }, BOUNDS_INTERVAL)
    }

    /** 卸几何观察器。 */
    __stopObservers() {
      var self = this
      for (var index = 0; index < self.__observers.length; index++) {
        try {
          self.__observers[index].disconnect()
        } catch (ignored) {
          // 观察器已失效。
        }
      }
      self.__observers = []
      var schedule = self.__scheduleListener
      if (schedule !== null) {
        shellWindow.removeEventListener('resize', schedule)
        shellWindow.removeEventListener('scroll', schedule, true)
        document.removeEventListener('scroll', schedule, true)
        document.removeEventListener('visibilitychange', schedule)
        self.__scheduleListener = null
      }
      if (self.__frame !== 0) {
        shellWindow.cancelAnimationFrame(self.__frame)
        self.__frame = 0
      }
      if (self.__timer !== 0) {
        clearInterval(self.__timer)
        self.__timer = 0
      }
    }
  }

  customElements.define('webview', WebviewElement)

  // 状态事件：Rust 推快照 → 找到元素 → 差分派发 DOM 事件。
  listen(EVENT_GUEST_STATE, function (snapshot) {
    if (snapshot === null || typeof snapshot !== 'object') return
    var element = registry[snapshot.lease]
    if (element === undefined) return
    element.__apply(snapshot)
  })

  // 下载被拦下：官方是静默 preventDefault，这里额外给页面一个可观测事件。
  listen(EVENT_GUEST_DOWNLOAD, function (payload) {
    if (payload === null || typeof payload !== 'object') return
    var element = registry[payload.lease]
    if (element === undefined) return
    var event = new Event('download-blocked')
    try {
      Object.defineProperty(event, 'url', { value: String(payload.url), enumerable: true })
    } catch (ignored) {
      // 同上。
    }
    element.dispatchEvent(event)
  })
})()
