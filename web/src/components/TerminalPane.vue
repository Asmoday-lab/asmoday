<script setup lang="ts">
/**
 * 单条终端面板：xterm.js + WebSocket。
 *
 * 客户端 → 服务端：键盘输入走 Binary，控制帧（resize）走 Text JSON；
 * 服务端 → 客户端：控制帧（ready/error）走 Text，PTY 输出走 Binary。
 */
import { nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'
import type { TerminalStatus, TerminalTab } from '../stores/terminal'
import { withBase } from '../lib/base'

const props = defineProps<{
  tab: TerminalTab
  active: boolean
  ctrl: boolean
  alt: boolean
}>()

const emit = defineEmits<{
  (
    e: 'session',
    payload: { id: string; sessionId: string; cwd: string; noProfile: boolean; platform: string },
  ): void
  (e: 'status', payload: { id: string; status: TerminalStatus }): void
  (e: 'consumed'): void
  (e: 'error', message: string): void
  /** 服务端以 E_TOO_MANY 拒绝建会话：父组件据此打开「活动会话」面板 */
  (e: 'limit'): void
}>()

const { t } = useI18n()
const host = ref<HTMLElement | null>(null)

let term: Terminal | null = null
let fit: FitAddon | null = null
let socket: WebSocket | null = null
let resizeObserver: ResizeObserver | null = null
/** visualViewport 监听（移动端软键盘重排） */
let viewportHandler: (() => void) | null = null
let disposed = false
/** 连续重连次数，握手成功后清零 */
let attempts = 0
let retryTimer: number | null = null
/** 服务端明确拒绝（如会话数超限）：重连没有意义，直接停手 */
let fatal = false
/** 是否已经收到过 ready（区分“初次连不上”和“连上后又断”） */
let everReady = false

/** 退避上限：服务重启/网络抖动时重连，不会打满后端 */
const MAX_ATTEMPTS = 8

function clearRetry() {
  if (retryTimer !== null) {
    window.clearTimeout(retryTimer)
    retryTimer = null
  }
}

const encoder = new TextEncoder()

// 终端渲染区恒为深色：ANSI 高亮（ls 目录、vim 语法色）只在深底上可读，
// 不跟随全局浅色主题（设计文档 v2.7.0 3.3.5）。
// 色板用 VS Code Dark+（标准 16 色 ANSI 映射）：对比度高、 saturated 足，
// neofetch 色块 / ls --color / vim 主题都按这套色相设计，观感与本地终端一致。
const darkTheme = {
  background: '#1e1e1e',
  foreground: '#cccccc',
  cursor: '#ffffff',
  cursorAccent: '#1e1e1e',
  selectionBackground: '#264f78',
  // 标准 ANSI（VS Code Dark+）
  black: '#000000',
  red: '#cd3131',
  green: '#0dbc79',
  yellow: '#e5e510',
  blue: '#2472c8',
  magenta: '#bc3fbc',
  cyan: '#11a8cd',
  white: '#e5e5e5',
  // bright ANSI
  brightBlack: '#666666',
  brightRed: '#f14c4c',
  brightGreen: '#23d18b',
  brightYellow: '#f5f543',
  brightBlue: '#3b8eea',
  brightMagenta: '#d670d6',
  brightCyan: '#29b8db',
  brightWhite: '#e5e5e5',
}

/** xterm 等宽字体链：Nerd Font 随应用打包（src/style.css @font-face），
 *  专治 starship / oh-my-zsh 的 powerline 图标与 Git 分支符号变豆腐块 */
const TERMINAL_FONT_FAMILY =
  '"JetBrainsMono Nerd Font", "Cascadia Code", Consolas, "Courier New", monospace'

/** 前端绝不自作聪明传 `~` / `%USERPROFILE%`：未指定目录时后端用 dirs::home_dir() 兜底 */
const PLACEHOLDER_PATH = /^(~|\$HOME|\$\{HOME\}|%USERPROFILE%)([\\/]|$)/i

function wsUrl(): string {
  const scheme = window.location.protocol === 'https:' ? 'wss' : 'ws'
  const params = new URLSearchParams()
  const cwd = props.tab.cwd?.trim() ?? ''
  // 空串/占位符一律不传，交给后端解析系统主目录（设计文档 6.3）
  if (cwd && !PLACEHOLDER_PATH.test(cwd)) params.set('path', cwd)
  if (props.tab.sessionId) params.set('session_id', props.tab.sessionId)
  // 纯净模式（设计文档 3.3.7）：后端据此加 -NoProfile / --noprofile --norc
  if (props.tab.noProfile) params.set('no_profile', '1')
  params.set('cols', String(term?.cols ?? 120))
  params.set('rows', String(term?.rows ?? 30))
  return `${scheme}://${window.location.host}${withBase('/api/terminal/ws')}?${params.toString()}`
}

function sendResize() {
  if (!socket || socket.readyState !== WebSocket.OPEN || !term) return
  socket.send(JSON.stringify({ type: 'resize', cols: term.cols, rows: term.rows }))
}

/**
 * 把焦点还给 xterm 的隐藏输入框。
 *
 * 触屏上点任何 `<button>` 都会把焦点从 textarea 抢走，输入法立刻收起 ——
 * 屏幕键盘（快捷键条）必须每次按完都把焦点要回来。`preventScroll` 是因为
 * textarea 被 xterm 放在视口外（left:-9999em），浏览器会为了「露出焦点」
 * 把整页横向滚一下，手机上表现为画面莫名偏移。
 */
function focusTerminal() {
  const textarea = term?.textarea
  if (textarea) {
    textarea.focus({ preventScroll: true })
    return
  }
  term?.focus()
}

/** 触屏设备不自动抢焦点：iOS 也不会因为脚本 focus 就弹输入法，
 *  Android 却会 —— 一进页面就弹键盘挡住半屏，行为还因平台而异，不如统一交给用户点。 */
function autoFocus() {
  const coarse = window.matchMedia?.('(pointer: coarse)').matches ?? false
  if (coarse) return
  focusTerminal()
}

/** 重新适配容器尺寸（ResizeObserver 与 visualViewport 共用） */
function refit() {
  if (!fit || !term) return
  try {
    fit.fit()
  } catch {
    /* 容器尚未布局 */
  }
  sendResize()
}

/** 重连用尽后探测一次失败原因，给出可读提示（浏览器不会告诉我们握手为什么被拒） */
async function diagnose(): Promise<string> {
  try {
    const res = await fetch(withBase('/api/auth/me'), { credentials: 'same-origin' })
    if (res.status === 401) return t('terminal.reconnect.expired')
    if (!res.ok) return t('terminal.reconnect.serviceError', { status: res.status })
    return t('terminal.reconnect.interrupted')
  } catch {
    return t('terminal.reconnect.offline')
  }
}

function scheduleReconnect() {
  if (disposed || fatal) return
  if (attempts >= MAX_ATTEMPTS) {
    emit('status', { id: props.tab.id, status: 'closed' })
    void diagnose().then((message) => emit('error', message))
    return
  }
  // 0.6s 起步，指数退避到 8s：服务重启通常几秒内就能重新连上
  const delay = Math.min(8000, 600 * 2 ** attempts)
  attempts += 1
  emit('status', { id: props.tab.id, status: 'connecting' })
  clearRetry()
  retryTimer = window.setTimeout(() => {
    retryTimer = null
    connect()
  }, delay)
}

function connect() {
  clearRetry()
  // 上一根 socket 先摘掉回调，避免它的 close 事件触发重复调度
  if (socket) {
    socket.onopen = null
    socket.onmessage = null
    socket.onclose = null
    socket.onerror = null
    try {
      socket.close()
    } catch {
      /* 已关闭 */
    }
    socket = null
  }

  emit('status', { id: props.tab.id, status: 'connecting' })
  openSocket()
}

function openSocket() {
  const current = new WebSocket(wsUrl())
  current.binaryType = 'arraybuffer'
  socket = current

  current.onopen = () => {
    if (socket !== current) return
    sendResize()
    autoFocus()
  }

  current.onmessage = (event) => {
    if (socket !== current) return
    if (typeof event.data === 'string') {
      try {
        const control = JSON.parse(event.data) as {
          type?: string
          session_id?: string
          cwd?: string
          message?: string
          warning?: string | null
          no_profile?: boolean
          platform?: string
          code?: string
        }
        if (control.type === 'ready' && control.session_id) {
          attempts = 0
          everReady = true
          if (control.warning) emit('error', control.warning)
          emit('session', {
            id: props.tab.id,
            sessionId: control.session_id,
            cwd: control.cwd ?? props.tab.cwd,
            noProfile: control.no_profile ?? props.tab.noProfile,
            platform: control.platform ?? '',
          })
          emit('status', { id: props.tab.id, status: 'ready' })
        } else if (control.type === 'error') {
          // 服务端明确拒绝（PTY 建不起来、会话超限等），重连只会空转
          fatal = true
          if (control.code === 'E_TOO_MANY') emit('limit')
          emit('error', control.message ?? t('terminal.reconnect.generic'))
          emit('status', { id: props.tab.id, status: 'error' })
        }
      } catch {
        term?.write(event.data)
      }
      return
    }
    term?.write(new Uint8Array(event.data as ArrayBuffer))
  }

  current.onclose = () => {
    if (socket !== current) return
    socket = null
    if (disposed) return
    if (fatal) {
      emit('status', { id: props.tab.id, status: 'error' })
      return
    }
    // 连上过又断（服务重启、代理超时、休眠唤醒）：静默重连，现场由后端缓冲回放
    if (everReady && attempts === 0) {
      term?.write(`\r\n\x1b[33m${t('terminal.reconnect.banner')}\x1b[0m\r\n`)
    }
    scheduleReconnect()
  }

  current.onerror = () => {
    // onerror 之后浏览器必定触发 onclose，状态与时序统一交给 onclose 处理
    if (socket !== current) return
  }
}

/** 手动重连（状态栏按钮用） */
function reconnect() {
  if (disposed) return
  fatal = false
  attempts = 0
  connect()
}

/** 粘性 Ctrl/Alt：按下修饰键后再输入单字符时转换为控制序列 */
function transformInput(data: string): string {
  if (props.ctrl && data.length === 1) {
    const code = data.toUpperCase().charCodeAt(0)
    if (code >= 64 && code <= 95) {
      emit('consumed')
      return String.fromCharCode(code - 64)
    }
  }
  if (props.alt && data.length === 1) {
    emit('consumed')
    return `\x1b${data}`
  }
  return data
}

function sendRaw(text: string) {
  if (!socket || socket.readyState !== WebSocket.OPEN) return
  socket.send(encoder.encode(text))
}

// ---------------- 触屏滚动 ----------------
//
// xterm 6 把滚动交给内部虚拟滚动条（`SmoothScrollableElement`），只有滚轮事件
// 能驱动它，触摸拖动没有任何一条路径会走到滚动 —— 手机上等于「翻不了历史输出」。
// 这里自己把纵向拖动换算成行：普通缓冲区走 `scrollLines`（公开 API，会同步
// ydisp 与滚动条），全屏应用（vim / htop）没有 scrollback，就合成滚轮事件
// 让 xterm 自己按方向键 / 鼠标上报处理。

/** 拖动多少像素才认定是「滚」而不是「点」 */
const TAP_SLOP = 8
/** 惯性摩擦（每 16ms 保留的速度比例） */
const FRICTION = 0.94

let touchActive = false
let touchScrolling = false
let touchStartX = 0
let touchStartY = 0
let touchPrevY = 0
let touchPrevT = 0
let touchVelocity = 0
/** 不足一行的高度的余量，攒够一整行才滚，避免抖动 */
let touchAcc = 0
let inertiaFrame = 0
/** 手指当前位置：合成滚轮事件时要带上，否则鼠标上报的坐标会落在左上角 */
let touchPointX = 0
let touchPointY = 0

/** 单行像素高度：拿 `.xterm-screen` 的实测高度除以行数，包含 line-height */
function cellHeight(): number {
  if (!term) return 16
  const screen = term.element?.querySelector('.xterm-screen') as HTMLElement | null
  const box = (screen ?? term.element)?.getBoundingClientRect()
  if (!box || box.height <= 0 || term.rows <= 0) return 16
  return box.height / term.rows
}

function stopInertia() {
  if (inertiaFrame) cancelAnimationFrame(inertiaFrame)
  inertiaFrame = 0
}

/** 按像素滚动：正数 = 手指下拉 = 看更早的输出 */
function scrollByPixels(dy: number) {
  if (!term || dy === 0) return
  const cell = cellHeight()
  touchAcc += dy
  const lines = Math.trunc(touchAcc / cell)
  if (!lines) return
  touchAcc -= lines * cell
  if (term.buffer.active.type === 'alternate') {
    // 全屏应用：合成的 wheel 会被 xterm 翻译成方向键，或按应用是否开启
    // 鼠标上报转成滚轮事件 —— 两种语义都由它自己判定，比我们猜更准
    term.element?.dispatchEvent(
      new WheelEvent('wheel', {
        deltaY: lines > 0 ? -120 : 120,
        clientX: touchPointX,
        clientY: touchPointY,
        bubbles: true,
        cancelable: true,
      }),
    )
    return
  }
  term.scrollLines(-lines)
}

function startInertia(velocity: number) {
  stopInertia()
  let last = performance.now()
  let speed = velocity
  const step = (now: number) => {
    // 掉帧时别一次补太多，否则甩一下直接到底
    const dt = Math.min(48, now - last)
    last = now
    speed *= FRICTION ** (dt / 16)
    if (Math.abs(speed) < 0.05) {
      inertiaFrame = 0
      return
    }
    scrollByPixels(speed * dt)
    inertiaFrame = requestAnimationFrame(step)
  }
  inertiaFrame = requestAnimationFrame(step)
}

function onTouchStart(event: TouchEvent) {
  if (event.touches.length !== 1) {
    touchActive = false
    return
  }
  stopInertia()
  const touch = event.touches[0]
  touchActive = true
  touchScrolling = false
  touchStartX = touch.clientX
  touchStartY = touch.clientY
  touchPointX = touch.clientX
  touchPointY = touch.clientY
  touchPrevY = touch.clientY
  touchPrevT = performance.now()
  touchVelocity = 0
  touchAcc = 0
}

function onTouchMove(event: TouchEvent) {
  if (!touchActive || event.touches.length !== 1) return
  const touch = event.touches[0]
  const dy = touch.clientY - touchPrevY
  const now = performance.now()
  const dt = now - touchPrevT
  if (!touchScrolling) {
    const dx = touch.clientX - touchStartX
    const travelY = touch.clientY - touchStartY
    // 横向拖动（抽屉/横向滚动）与轻点都不接管
    if (Math.abs(travelY) < TAP_SLOP || Math.abs(dx) > Math.abs(travelY)) return
    touchScrolling = true
  }
  // 接管纵向拖动：不拦默认行为的话页面会跟着橡皮筋、还可能触发下拉刷新
  if (event.cancelable) event.preventDefault()
  if (dt > 0) touchVelocity = touchVelocity * 0.7 + (dy / dt) * 0.3
  touchPointX = touch.clientX
  touchPointY = touch.clientY
  touchPrevY = touch.clientY
  touchPrevT = now
  scrollByPixels(dy)
}

function onTouchEnd() {
  if (!touchActive) return
  const flung = touchScrolling
  touchActive = false
  touchScrolling = false
  // 甩动惯性：手指离开后按最后的速度继续减速
  if (flung && Math.abs(touchVelocity) > 0.15) startInertia(touchVelocity)
}

onMounted(() => {
  if (!host.value) return

  term = new Terminal({
    cursorBlink: true,
    fontSize: 13,
    fontFamily: TERMINAL_FONT_FAMILY,
    lineHeight: 1.35,
    scrollback: 5000,
    theme: darkTheme,
    allowProposedApi: true,
  })

  fit = new FitAddon()
  term.loadAddon(fit)
  term.open(host.value)

  // 触屏滚动（见上方 scrollByPixels）：touchmove 必须非 passive 才能拦住默认滚动
  host.value.addEventListener('touchstart', onTouchStart, { passive: true })
  host.value.addEventListener('touchmove', onTouchMove, { passive: false })
  host.value.addEventListener('touchend', onTouchEnd)
  host.value.addEventListener('touchcancel', onTouchEnd)

  // Tab 键必须进 PTY：浏览器默认行为是移动焦点（自动补全直接失效）。
  // xterm 收到 return false 会走自己的键盘处理流程把 \t 发给 onData。
  term.attachCustomKeyEventHandler((event) => {
    if (event.type === 'keydown' && event.key === 'Tab') {
      event.preventDefault()
      event.stopPropagation()
      sendRaw('\t')
      return false
    }
    return true
  })

  // nextTick 确保 host 已完成布局再 fit：挂载瞬间容器尺寸可能是 0，
  // 提前 fit 会算出错误的 cols/rows，PTY 与画布列数错位（色块换行错乱）。
  void nextTick(() => {
    requestAnimationFrame(() => {
      try {
        fit?.fit()
      } catch {
        /* 容器尚未布局 */
      }
      sendResize()
      autoFocus()
    })
  })

  term.onData((data) => {
    const payload = transformInput(data)
    if (!socket || socket.readyState !== WebSocket.OPEN) return
    socket.send(encoder.encode(payload))
  })

  resizeObserver = new ResizeObserver(() => {
    refit()
  })
  resizeObserver.observe(host.value)

  // 移动端软键盘（设计文档 8.4 第 11 条）：软键盘弹出只改 `visualViewport`，
  // 布局视口与容器尺寸都不变，ResizeObserver 收不到 —— 必须单独监听，否则
  // 光标会被键盘顶到看不见的地方。
  const viewport = window.visualViewport
  if (viewport) {
    viewportHandler = () => requestAnimationFrame(refit)
    viewport.addEventListener('resize', viewportHandler)
    viewport.addEventListener('scroll', viewportHandler)
  }

  connect()
})

onBeforeUnmount(() => {
  disposed = true
  clearRetry()
  stopInertia()
  if (host.value) {
    host.value.removeEventListener('touchstart', onTouchStart)
    host.value.removeEventListener('touchmove', onTouchMove)
    host.value.removeEventListener('touchend', onTouchEnd)
    host.value.removeEventListener('touchcancel', onTouchEnd)
  }
  resizeObserver?.disconnect()
  resizeObserver = null
  if (viewportHandler && window.visualViewport) {
    window.visualViewport.removeEventListener('resize', viewportHandler)
    window.visualViewport.removeEventListener('scroll', viewportHandler)
  }
  viewportHandler = null
  // 只断开连接，PTY 由后端保留 3 分钟，重连可原地恢复
  if (socket) {
    socket.onclose = null
    socket.close()
  }
  socket = null
  term?.dispose()
  term = null
})

watch(
  () => props.active,
  (active) => {
    if (!active) return
    requestAnimationFrame(() => {
      try {
        fit?.fit()
      } catch {
        /* ignore */
      }
      sendResize()
      autoFocus()
    })
  },
)

/** 隐藏输入框当前是否持有焦点（= 系统输入法是否处于弹出状态） */
function hasFocus(): boolean {
  return !!term?.textarea && document.activeElement === term.textarea
}

defineExpose({ sendRaw, reconnect, focus: focusTerminal, hasFocus })
</script>

<template>
  <!-- 渲染区强制深色（设计文档 3.3.4 / 8.4.4）：xterm 初始化与字体加载前也不能白闪；
       外壳（标签栏/底边）由 TerminalView 跟随全局主题，这里只管画布本身 -->
  <div ref="host" class="terminal-canvas h-full w-full overflow-hidden px-3 py-2" />
</template>

<style scoped>
.terminal-canvas {
  background: #1e1e1e;
  border-radius: 6px;
}

/* 触屏上纵向拖动由 scrollByPixels 接管：不声明 touch-action 的话，
   浏览器会先自己滚一段（页面橡皮筋 / 下拉刷新），我们的 touchmove 再也拦不住 */
@media (pointer: coarse) {
  .terminal-canvas {
    touch-action: none;
  }
}
</style>
