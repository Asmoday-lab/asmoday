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
  const current = new WebSocket(wsUrl())
  current.binaryType = 'arraybuffer'
  socket = current

  current.onopen = () => {
    if (socket !== current) return
    sendResize()
    term?.focus()
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
      term?.focus()
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
      term?.focus()
    })
  },
)

defineExpose({ sendRaw, reconnect, focus: () => term?.focus() })
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
</style>
