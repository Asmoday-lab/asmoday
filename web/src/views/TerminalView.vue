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
import { listTerminalSessions, killTerminalSession, type TerminalSession } from '../api/terminal'
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
    // 光提示没用：直接把「活动会话」面板顶上来，让用户挑一个踢掉（设计文档 3.3.6）
    void openSessions()
    return
  }
  openTab(cwd)
}

// ---------------- 活动会话（设计文档 3.3.6） ----------------
//
// 并发上限 10 个，断线后 PTY 还会保活 3 分钟。没有这个面板，用户遇到
// 「已达上限」只能干等或重启进程——这是文档明确要求补的逃生口。

const showSessions = ref(false)
const sessions = ref<TerminalSession[]>([])
const sessionLimit = ref(MAX_TABS)
const sessionsBusy = ref(false)
const sessionsError = ref('')

async function refreshSessions() {
  sessionsBusy.value = true
  sessionsError.value = ''
  try {
    const data = await listTerminalSessions()
    sessions.value = data.sessions
    if (data.limit > 0) sessionLimit.value = data.limit
  } catch (err) {
    sessionsError.value = err instanceof Error ? err.message : String(err)
  } finally {
    sessionsBusy.value = false
  }
}

function openSessions() {
  showSessions.value = true
  return refreshSessions()
}

/** 强制关闭一条会话：若它正被某个标签复用，顺手清掉 sessionId 让该标签按需重开 */
async function closeSession(session: TerminalSession) {
  try {
    await killTerminalSession(session.id)
  } catch (err) {
    showToast(err instanceof Error ? err.message : String(err))
    return
  }
  const owner = tabs.value.find((tab) => tab.sessionId === session.id)
  if (owner) updateTab(owner.id, { sessionId: null, status: 'closed' })
  showToast(t('terminal.sessions.closed'))
  await refreshSessions()
}

function isTabSession(session: TerminalSession) {
  return active.value?.sessionId === session.id
}

/** 存活时长：列表里只给「多久了」，精确到秒没有意义 */
function ageText(secs: number) {
  if (secs < 60) return t('terminal.sessions.ageSeconds', { n: secs })
  const mins = Math.floor(secs / 60)
  if (mins < 60) return t('terminal.sessions.ageMinutes', { n: mins })
  return t('terminal.sessions.ageHours', { n: Math.floor(mins / 60) })
}

/** 目录只留最后两段，避免长路径把列表撑破 */
function shortPath(path: string) {
  const parts = path.split(/[\\/]/).filter(Boolean)
  if (parts.length <= 2) return path
  return `…/${parts.slice(-2).join('/')}`
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

      <!-- 活动会话：并发打满 / 有断线残留时的逃生口（设计文档 3.3.6） -->
      <button
        type="button"
        class="flex h-7 shrink-0 items-center gap-1 rounded px-1.5 text-[11px] text-muted transition-colors hover:bg-elevated hover:text-ink"
        :title="t('terminal.sessions.title')"
        @click="openSessions"
      >
        <AppIcon name="list" :size="13" />
        <span class="hidden sm:inline">{{ t('terminal.sessions.label') }}</span>
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
            @limit="openSessions"
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

    <!-- 活动会话面板（设计文档 3.3.6）：并发打满或断线残留时踢掉占用者 -->
    <div
      v-if="showSessions"
      class="fixed inset-0 z-50 grid place-items-center bg-black/50 p-4"
      @click.self="showSessions = false"
    >
      <div
        class="flex max-h-[70vh] w-full max-w-md flex-col rounded-2xl border border-line bg-panel p-5 shadow-xl"
      >
        <div class="flex items-start justify-between gap-3">
          <div class="min-w-0">
            <p class="text-sm font-medium text-ink">{{ t('terminal.sessions.title') }}</p>
            <p class="mt-1 text-[11px] text-muted">
              {{
                t('terminal.sessions.summary', {
                  count: sessions.length,
                  limit: sessionLimit,
                })
              }}
            </p>
          </div>
          <button
            type="button"
            class="grid size-7 shrink-0 place-items-center rounded text-muted transition-colors hover:bg-elevated hover:text-ink"
            :title="t('terminal.sessions.refresh')"
            @click="refreshSessions"
          >
            <AppIcon name="refresh" :size="14" />
          </button>
        </div>

        <p
          v-if="sessionsError"
          class="mt-3 rounded-lg border border-line bg-elevated px-2.5 py-2 text-[11px] fg-danger"
        >
          {{ sessionsError }}
        </p>
        <p v-else-if="sessionsBusy && sessions.length === 0" class="mt-3 text-[11px] text-muted">
          {{ t('common.action.loading') }}
        </p>
        <p v-else-if="sessions.length === 0" class="mt-3 text-[11px] text-muted">
          {{ t('terminal.sessions.empty') }}
        </p>

        <ul v-else class="mt-3 min-h-0 flex-1 space-y-1.5 overflow-y-auto">
          <li
            v-for="session in sessions"
            :key="session.id"
            class="rounded-lg border border-line bg-elevated px-2.5 py-2"
          >
            <div class="flex items-center gap-2">
              <span class="min-w-0 flex-1 truncate text-[12px] text-ink" :title="session.cwd">
                {{ shortPath(session.cwd) }}
              </span>
              <span
                v-if="isTabSession(session)"
                class="shrink-0 rounded bg-accent-soft px-1.5 py-0.5 text-[10px] text-accent"
              >
                {{ t('terminal.sessions.current') }}
              </span>
              <button
                type="button"
                class="shrink-0 rounded px-1.5 py-0.5 text-[11px] text-muted transition-colors hover:bg-panel hover:text-ink"
                :title="t('terminal.sessions.closeTitle')"
                @click="closeSession(session)"
              >
                {{ t('terminal.sessions.close') }}
              </button>
            </div>
            <p class="mt-1 truncate text-[10px] text-muted">
              <template v-if="session.no_profile">{{ t('terminal.safeMode.label') }} · </template>
              {{ session.cols }}×{{ session.rows }} ·
              {{ t('terminal.sessions.clients', { n: session.clients }) }} ·
              {{ ageText(session.age_secs) }}
            </p>
          </li>
        </ul>

        <div class="mt-4 flex justify-end">
          <button
            type="button"
            class="rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90"
            @click="showSessions = false"
          >
            {{ t('common.action.close') }}
          </button>
        </div>
      </div>
    </div>

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
