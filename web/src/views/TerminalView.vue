<script setup lang="ts">
/**
 * 系统终端：多标签 + 真实 PTY + 移动端 Termux 两行键盘。
 *
 * 标签状态放在模块级 store 里，离开本页再回来时会带上 session_id 重连，
 * 后端在 3 分钟内保留 PTY，因此现场不会丢。
 */
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { useRoute } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import TerminalPane from '../components/TerminalPane.vue'
import { useTerminalStore, saveSafeModeDefault, type TerminalStatus } from '../stores/terminal'

const { tabs, activeId, active, openTab, closeTab, setActive, updateTab } = useTerminalStore()
const { t } = useI18n()
const route = useRoute()

const showKeys = ref(true)
const ctrl = ref(false)
const alt = ref(false)
const toast = ref('')

const panes = new Map<string, InstanceType<typeof TerminalPane>>()

function setPane(id: string, instance: unknown) {
  if (instance) panes.set(id, instance as InstanceType<typeof TerminalPane>)
  else panes.delete(id)
}

const activePane = computed(() => (activeId.value ? panes.get(activeId.value) : undefined))

/** 后端 MAX_SESSIONS 是 10，前端不无限堆标签（超限后新会话会被服务端直接拒绝） */
const MAX_TABS = 10

let toastTimer: number | null = null
function showToast(message: string) {
  toast.value = message
  if (toastTimer !== null) window.clearTimeout(toastTimer)
  toastTimer = window.setTimeout(() => {
    if (toast.value === message) toast.value = ''
  }, 2600)
}

function newTab(cwd = '') {
  if (tabs.value.length >= MAX_TABS) {
    showToast(t('terminal.reconnect.limit', { max: MAX_TABS }))
    return
  }
  openTab(cwd)
}

function openFromRoute() {
  const path = typeof route.query.path === 'string' ? route.query.path.trim() : ''
  if (!path) return
  // 同一目录反复从文件管理进入时复用已有标签，不再无限新建
  const existing = tabs.value.find((tab) => tab.cwd === path)
  if (existing) {
    setActive(existing.id)
    return
  }
  newTab(path)
}

onMounted(() => {
  if (route.query.path) openFromRoute()
  else if (tabs.value.length === 0) openTab('')
})

watch(
  () => route.query.path,
  (next) => {
    if (typeof next === 'string' && next.trim()) openFromRoute()
  },
)

function onStatus(payload: { id: string; status: TerminalStatus }) {
  updateTab(payload.id, { status: payload.status })
}

function onSession(payload: {
  id: string
  sessionId: string
  cwd: string
  noProfile: boolean
  platform: string
}) {
  // 后端返回的是平台原生路径（Windows `C:\Users\x`、Linux `/home/x`），分隔符两种都要认
  const name = payload.cwd.split(/[\\/]/).filter(Boolean).pop() ?? 'shell'
  updateTab(payload.id, {
    sessionId: payload.sessionId,
    cwd: payload.cwd,
    title: name,
    noProfile: payload.noProfile,
    platform: payload.platform,
  })
}

function onError(message: string) {
  showToast(message)
}

/**
 * 切换纯净模式（设计文档 3.3.7）。
 *
 * profile 是 shell 启动时加载的，跑起来就改不回去 —— 只能丢掉当前 PTY 重开一个，
 * 所以这里同时清掉 sessionId 并主动重连。滚动缓冲会丢，这正是「救急」的代价。
 */
function toggleSafeMode() {
  const tab = active.value
  if (!tab) return
  const next = !tab.noProfile
  saveSafeModeDefault(next)
  updateTab(tab.id, { noProfile: next, sessionId: null, status: 'connecting' })
  showToast(next ? t('terminal.safeMode.on') : t('terminal.safeMode.off'))
  // tab 是响应式对象，pane 读到的已是最新 noProfile，重连即按新模式拉起
  activePane.value?.reconnect()
}

/** Windows 乱码一键修：把 chcp 65001 直接打进当前 shell */
function fixCharset() {
  const pane = activePane.value
  if (!pane) return
  pane.sendRaw('chcp 65001\r')
  showToast(t('terminal.charset.sent'))
}

/** 状态栏「重新连接」：自动重试已用尽或服务端拒绝后，给一次手动机会 */
function reconnectActive() {
  const pane = activePane.value
  if (!pane) return
  updateTab(activeId.value, { sessionId: null })
  pane.reconnect()
}

function toggleSticky(key: 'ctrl' | 'alt') {
  if (key === 'ctrl') {
    ctrl.value = !ctrl.value
    if (ctrl.value) alt.value = false
  } else {
    alt.value = !alt.value
    if (alt.value) ctrl.value = false
  }
}

function sendKey(sequence: string) {
  activePane.value?.sendRaw(sequence)
  ctrl.value = false
  alt.value = false
}

const statusLabel = computed<Record<TerminalStatus, string>>(() => ({
  connecting: t('terminal.status.connecting'),
  ready: t('terminal.status.ready'),
  closed: t('terminal.status.closed'),
  error: t('terminal.status.error'),
}))

const statusTone: Record<TerminalStatus, string> = {
  connecting: 'bg-amber-400',
  ready: 'bg-emerald-400',
  closed: 'bg-slate-500',
  error: 'bg-red-400',
}
</script>

<template>
  <!-- 外壳（标签栏/键盘/状态栏）跟随全局主题；只有 xterm 渲染区恒为深色（v2.7.0 3.3.5 / 8.4.4） -->
  <section class="flex h-full min-h-0 flex-col bg-panel text-ink">
    <!-- 标签栏 -->
    <header
      class="flex h-10 shrink-0 items-center gap-1 border-b border-line bg-panel px-2"
    >
      <div class="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto">
        <button
          v-for="tab in tabs"
          :key="tab.id"
          type="button"
          class="group flex shrink-0 items-center gap-2 rounded-t-md border-t-2 px-3 py-1.5 text-xs transition-colors"
          :class="
            tab.id === activeId
              ? 'border-accent bg-elevated text-ink'
              : 'border-transparent text-muted hover:text-ink'
          "
          :title="tab.cwd || t('terminal.tabs.defaultDir')"
          @click="setActive(tab.id)"
        >
          <span class="size-1.5 rounded-full" :class="statusTone[tab.status]" />
          <AppIcon name="terminal" :size="13" />
          <span class="max-w-[140px] truncate">{{ tab.title }}</span>
          <span
            class="ml-1 rounded px-1 text-[11px] text-muted opacity-0 transition-opacity group-hover:opacity-100"
            role="button"
            :title="t('terminal.tabs.close')"
            @click.stop="closeTab(tab.id)"
          >
            ✕
          </span>
        </button>
        <!-- 新建按钮紧跟最后一个标签页（v2.9），不再孤零零挂在工具栏最右端 -->
        <button
          type="button"
          class="grid size-6 shrink-0 place-items-center rounded text-muted transition-colors hover:bg-elevated hover:text-ink"
          :title="t('terminal.tabs.new')"
          @click="newTab()"
        >
          <AppIcon name="plus" :size="14" />
        </button>
      </div>

      <!-- 纯净模式（设计文档 3.3.7）：profile 死循环/严重报错时的救急开关 -->
      <button
        type="button"
        class="flex h-7 shrink-0 items-center gap-1 rounded px-1.5 text-[11px] transition-colors"
        :class="
          active?.noProfile
            ? 'bg-accent-soft text-accent'
            : 'text-muted hover:bg-elevated hover:text-ink'
        "
        :title="t('terminal.safeMode.title')"
        :disabled="!active"
        @click="toggleSafeMode"
      >
        <AppIcon name="shield" :size="13" />
        <span class="hidden sm:inline">{{ t('terminal.safeMode.label') }}</span>
      </button>

      <!-- Windows 字符集一键修复：后端已在启动时 chcp 65001，这里兜底被 profile 改回去的情况 -->
      <button
        v-if="active?.platform === 'windows'"
        type="button"
        class="flex h-7 shrink-0 items-center gap-1 rounded px-1.5 text-[11px] text-muted transition-colors hover:bg-elevated hover:text-ink"
        :title="t('terminal.charset.title')"
        @click="fixCharset"
      >
        <AppIcon name="refresh" :size="12" />
        <span class="hidden sm:inline">UTF-8</span>
      </button>

      <button
        type="button"
        class="grid size-7 shrink-0 place-items-center rounded text-muted transition-colors hover:bg-elevated hover:text-ink md:hidden"
        :title="t('terminal.tabs.toggleKeys')"
        @click="showKeys = !showKeys"
      >
        <AppIcon name="menu" :size="15" />
      </button>
    </header>

    <!-- 终端区 -->
    <div class="relative min-h-0 flex-1">
      <div v-if="tabs.length === 0" class="grid h-full place-items-center">
        <div class="text-center">
          <p class="text-sm text-muted">{{ t('terminal.empty.title') }}</p>
          <p class="mt-1 text-xs text-muted opacity-80">{{ t('terminal.empty.hint') }}</p>
          <button
            class="mt-4 rounded-md bg-accent px-4 py-2 text-xs text-white hover:opacity-90"
            @click="newTab()"
          >
            {{ t('terminal.empty.action') }}
          </button>
        </div>
      </div>

      <div
        v-for="tab in tabs"
        v-show="tab.id === activeId"
        :key="tab.id"
        class="absolute inset-0 p-2 md:p-3"
        :class="{ 'pb-[80px] md:pb-3': showKeys }"
      >
        <!-- 浅色主题下的「画中画」缓冲：主区域浅色，终端壳圆角 + 阴影（8.4.4 第 2 条） -->
        <div
          class="h-full w-full overflow-hidden rounded-xl border border-line bg-[#101216] shadow-sm"
        >
          <TerminalPane
            :ref="(el) => setPane(tab.id, el)"
            :tab="tab"
            :active="tab.id === activeId"
            :ctrl="ctrl"
            :alt="alt"
            @status="onStatus"
            @session="onSession"
            @consumed="ctrl = false; alt = false"
            @error="onError"
          />
        </div>
      </div>
    </div>

    <!-- 移动端 Termux 两行键盘 -->
    <div
      v-if="showKeys && tabs.length > 0"
      class="absolute bottom-0 left-0 right-0 z-30 h-[72px] border-t border-line bg-panel md:hidden"
      style="padding-bottom: env(safe-area-inset-bottom, 0px); height: calc(72px + env(safe-area-inset-bottom, 0px))"
    >
      <div class="grid h-1/2 grid-cols-7 border-b border-line bg-elevated">
        <button class="term-key" @click="sendKey('\x1b')">ESC</button>
        <button class="term-key" @click="sendKey('/')">/</button>
        <button class="term-key" @click="sendKey('-')">-</button>
        <button class="term-key" @click="sendKey('\x1b[H')">HOME</button>
        <button class="term-key" @click="sendKey('\x1b[A')">↑</button>
        <button class="term-key" @click="sendKey('\x1b[F')">END</button>
        <button class="term-key" @click="sendKey('\x1b[5~')">PGUP</button>
      </div>
      <div class="grid h-1/2 grid-cols-7">
        <button class="term-key" @click="sendKey('\t')">TAB</button>
        <button class="term-key" :class="{ 'term-key-active': ctrl }" @click="toggleSticky('ctrl')">
          CTRL
        </button>
        <button class="term-key" :class="{ 'term-key-active': alt }" @click="toggleSticky('alt')">
          ALT
        </button>
        <button class="term-key" @click="sendKey('\x1b[D')">←</button>
        <button class="term-key" @click="sendKey('\x1b[B')">↓</button>
        <button class="term-key" @click="sendKey('\x1b[C')">→</button>
        <button class="term-key" @click="sendKey('\x1b[6~')">PGDN</button>
      </div>
    </div>

    <!-- 底部状态提示 -->
    <footer
      v-if="tabs.length > 0"
      class="flex h-6 shrink-0 items-center justify-between border-t border-line bg-panel px-3 text-[11px] text-muted"
    >
      <span class="truncate">{{ active?.cwd || t('terminal.tabs.defaultDir') }}</span>
      <span class="ml-3 flex shrink-0 items-center gap-2">
        <span>
          {{
            active?.sessionId
              ? t('terminal.status.session', { id: active.sessionId })
              : t('terminal.status.establishing')
          }}
          ·
          {{ statusLabel[active?.status ?? 'connecting'] }}
        </span>
        <button
          v-if="active && (active.status === 'closed' || active.status === 'error')"
          type="button"
          class="flex items-center gap-1 rounded bg-elevated px-1.5 py-0.5 text-[11px] text-ink transition-colors hover:bg-elevated/80"
          :title="t('terminal.reconnect.title')"
          @click="reconnectActive"
        >
          <AppIcon name="refresh" :size="11" />
          {{ t('terminal.reconnect.button') }}
        </button>
      </span>
    </footer>

    <Transition name="fade">
      <div
        v-if="toast"
        class="pointer-events-none absolute bottom-20 left-1/2 z-40 -translate-x-1/2 rounded-full border border-line bg-elevated px-4 py-2 text-xs text-ink shadow-lg"
      >
        {{ toast }}
      </div>
    </Transition>
  </section>
</template>

<style scoped>
.term-key {
  display: flex;
  align-items: center;
  justify-content: center;
  border-right: 1px solid var(--border-color);
  color: var(--text-main);
  font-family: var(--font-app-mono);
  font-size: 12px;
  user-select: none;
}
.term-key:last-child {
  border-right: none;
}
.term-key:active {
  background: var(--bg-elevated);
}
.term-key-active {
  background: var(--accent-soft);
  color: var(--accent);
}
</style>
