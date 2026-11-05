<script setup lang="ts">
/**
 * 下载页（v4.9.31，设计文档 3.10）。
 *
 * 内置引擎是**一级功能**（与文件管理、媒体库同级）：任务列表、新建下载、引擎参数
 * 与 RPC 接入信息都在这里，不用去 config.toml 手写 TOML。
 *
 * 两个通道共用同一个 DownloadManager：本页走 Cookie 会话的 `/api/downloads*`，
 * AriaNg 等外部客户端走 `/jsonrpc` + `token:<secret>`。密钥只在「引擎设置」里
 * 给出供复制，不落 localStorage。
 */
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from '../components/AppIcon.vue'
import {
  downloadsApi,
  type DownloadTask,
  type EngineInfo,
  type GlobalStat,
  type Snapshot,
} from '../api/downloads'
import { useScrollLock } from '../composables/useScrollLock'
import { APP_BASE } from '../lib/base'
import { readApiError } from '../lib/errors'

/** `[download]` 段（与后端 `DownloadConfig` 对齐；只在本页编辑） */
interface DownloadConfig {
  enable: boolean
  rpc_secret: string
  segments: number
  max_concurrent: number
  roots: string[]
  enable_bt: boolean
  bt_listen_port_min: number
  bt_listen_port_max: number
  bt_disable_dht: boolean
}

type Tab = 'active' | 'queue' | 'done'
const TABS: Tab[] = ['active', 'queue', 'done']

const { t } = useI18n()

const snapshot = ref<Snapshot | null>(null)
const engine = ref<EngineInfo | null>(null)
const loading = ref(true)
const error = ref('')
const toast = ref('')
const tab = ref<Tab>('active')
const busyGid = ref('')

const addOpen = ref(false)
const addMode = ref<'uri' | 'torrent'>('uri')
const addUri = ref('')
const addDir = ref('')
const addFile = ref<File | null>(null)
const addBusy = ref(false)
const addError = ref('')

const settingsOpen = ref(false)
const settings = ref<DownloadConfig | null>(null)
const settingsBusy = ref(false)
const settingsError = ref('')
const secretShown = ref(false)
/** 保存时的整份配置：`POST /api/config` 收完整结构，不能只发 `[download]` */
const fullConfig = ref<Record<string, unknown> | null>(null)
const savedNeedsRestart = ref(false)

useScrollLock(computed(() => addOpen.value || settingsOpen.value))

const groups = computed(() => ({
  active: snapshot.value?.tasks?.active ?? [],
  queue: snapshot.value?.tasks?.waiting ?? [],
  done: snapshot.value?.tasks?.stopped ?? [],
}))

const rows = computed(() => groups.value[tab.value])

const stat = computed<GlobalStat>(
  () =>
    snapshot.value?.global ?? {
      downloadSpeed: 0,
      uploadSpeed: 0,
      numActive: 0,
      numWaiting: 0,
      numStopped: 0,
      numStoppedTotal: 0,
    },
)

const enabled = computed(() => snapshot.value?.enabled !== false)
const btEnabled = computed(() => !!snapshot.value?.bt)

const rpcUrl = computed(
  () => `${window.location.origin}${APP_BASE}${engine.value?.rpc_path ?? '/jsonrpc'}`,
)

function showToast(message: string) {
  toast.value = message
  window.setTimeout(() => {
    if (toast.value === message) toast.value = ''
  }, 2400)
}

function formatBytes(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let size = value
  let unit = 0
  while (size >= 1024 && unit < units.length - 1) {
    size /= 1024
    unit += 1
  }
  return `${size >= 100 || unit === 0 ? Math.round(size) : size.toFixed(1)} ${units[unit]}`
}

const formatSpeed = (value: number) => `${formatBytes(value)}/s`

function taskName(task: DownloadTask): string {
  if (task.name) return task.name
  const fromBt = task.bittorrent?.info?.name
  if (fromBt) return fromBt
  // magnet 还没拿到元信息：用 GID 前 8 位，别显示「未知任务」这种没法区分的东西
  return task.gid.slice(0, 8)
}

function percent(task: DownloadTask): number {
  if (task.status === 'complete') return 100
  const total = task.totalLength
  if (!total) return 0
  return Math.min(100, Math.floor((task.completedLength / total) * 100))
}

function taskPath(task: DownloadTask): string {
  return task.files?.[0]?.path || task.dir
}

async function refresh() {
  try {
    snapshot.value = await downloadsApi.snapshot()
    error.value = ''
  } catch (err) {
    // 轮询失败不要把已有列表清空：网络抖一下不该让页面变白
    if (!snapshot.value) error.value = err instanceof Error ? err.message : t('downloads.loadFailed')
  } finally {
    loading.value = false
  }
}

async function loadEngine() {
  try {
    engine.value = await downloadsApi.engine()
  } catch (err) {
    error.value = err instanceof Error ? err.message : t('downloads.loadFailed')
  }
}

async function conflict(errorMessage: string) {
  error.value = errorMessage
  window.setTimeout(() => {
    if (error.value === errorMessage) error.value = ''
  }, 4000)
}

async function togglePause(task: DownloadTask) {
  busyGid.value = task.gid
  try {
    if (task.status === 'paused') {
      await downloadsApi.unpause(task.gid)
      showToast(t('downloads.toast.resumed'))
    } else {
      await downloadsApi.pause(task.gid)
      showToast(t('downloads.toast.paused'))
    }
    await refresh()
  } catch (err) {
    await conflict(err instanceof Error ? err.message : t('downloads.actionFailed'))
  } finally {
    busyGid.value = ''
  }
}

async function removeTask(task: DownloadTask) {
  busyGid.value = task.gid
  try {
    await downloadsApi.remove(task.gid)
    await refresh()
    showToast(t('downloads.toast.removed'))
  } catch (err) {
    await conflict(err instanceof Error ? err.message : t('downloads.actionFailed'))
  } finally {
    busyGid.value = ''
  }
}

async function purgeStopped() {
  try {
    const result = await downloadsApi.purge()
    await refresh()
    showToast(t('downloads.toast.purged', { count: result.removed }))
  } catch (err) {
    await conflict(err instanceof Error ? err.message : t('downloads.actionFailed'))
  }
}

async function copyPath(task: DownloadTask) {
  try {
    await navigator.clipboard.writeText(taskPath(task))
    showToast(t('downloads.toast.pathCopied'))
  } catch {
    showToast(t('common.clipboard.denied'))
  }
}

function openAdd() {
  addMode.value = 'uri'
  addUri.value = ''
  addFile.value = null
  addError.value = ''
  // 默认落到引擎的第一个白名单目录，用户不用每次重选
  addDir.value = engine.value?.roots[0] ?? ''
  addOpen.value = true
}

async function submitAdd() {
  if (addBusy.value) return
  addBusy.value = true
  addError.value = ''
  try {
    if (addMode.value === 'torrent') {
      if (!addFile.value) throw new Error(t('downloads.dialog.pickFile'))
      await downloadsApi.addTorrent(addFile.value, addDir.value || undefined)
    } else {
      // 一行一个链接：批量粘贴是常态，逐条建任务比逼用户分开提交合理
      const uris = addUri.value
        .split('\n')
        .map((line) => line.trim())
        .filter(Boolean)
      if (!uris.length) throw new Error(t('downloads.dialog.uriRequired'))
      for (const uri of uris) {
        await downloadsApi.addUri({ uri, dir: addDir.value || undefined })
      }
    }
    addOpen.value = false
    tab.value = 'active'
    await refresh()
    showToast(t('downloads.toast.added'))
  } catch (err) {
    addError.value = err instanceof Error ? err.message : t('downloads.actionFailed')
  } finally {
    addBusy.value = false
  }
}

async function openSettings() {
  settingsOpen.value = true
  settingsError.value = ''
  savedNeedsRestart.value = false
  try {
    // 与设置页同一份配置：只改 `[download]`，其余原样回传
    const res = await fetch('/api/config')
    if (!res.ok) throw new Error(await readApiError(res))
    const config = (await res.json()) as Record<string, unknown>
    fullConfig.value = config
    settings.value = structuredClone(config.download as DownloadConfig)
    if (!engine.value) await loadEngine()
  } catch (err) {
    settingsError.value = err instanceof Error ? err.message : t('downloads.loadFailed')
  }
}

async function saveSettings() {
  if (settingsBusy.value || !settings.value || !fullConfig.value) return
  settingsBusy.value = true
  settingsError.value = ''
  try {
    const next = structuredClone(fullConfig.value)
    next.download = { ...(next.download as DownloadConfig), ...settings.value }
    const res = await fetch('/api/config', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(next),
    })
    if (!res.ok) throw new Error(await readApiError(res))
    fullConfig.value = next
    savedNeedsRestart.value = true
    showToast(t('downloads.settings.saved'))
    await loadEngine()
  } catch (err) {
    settingsError.value = err instanceof Error ? err.message : t('downloads.actionFailed')
  } finally {
    settingsBusy.value = false
  }
}

async function copyText(value: string, message: string) {
  try {
    await navigator.clipboard.writeText(value)
    showToast(message)
  } catch {
    showToast(t('common.clipboard.denied'))
  }
}

let timer: number | undefined

onMounted(async () => {
  await Promise.all([refresh(), loadEngine()])
  // 1s 轮询：与 aria2 客户端一致；页面不可见时停掉，后台标签页不白烧 CPU
  timer = window.setInterval(() => {
    if (document.visibilityState === 'visible') void refresh()
  }, 1000)
})

onUnmounted(() => {
  if (timer) window.clearInterval(timer)
})
</script>

<template>
  <section class="mx-auto w-full max-w-4xl px-4 py-6 md:px-8 md:py-10">
    <div class="flex flex-wrap items-center gap-3">
      <div class="min-w-0 flex-1">
        <p class="text-sm font-semibold text-ink">{{ t('downloads.heading') }}</p>
        <p class="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-[12px] text-muted">
          <span class="inline-flex items-center gap-1">
            <AppIcon name="download" :size="12" />
            {{ formatSpeed(stat.downloadSpeed) }}
          </span>
          <span class="inline-flex items-center gap-1">
            <AppIcon name="upload" :size="12" />
            {{ formatSpeed(stat.uploadSpeed) }}
          </span>
          <span>{{ t('downloads.summary.counts', { active: stat.numActive, queue: stat.numWaiting }) }}</span>
          <span v-if="engine?.bt_port" class="rounded bg-elevated px-1.5 py-0.5 font-mono text-[11px]">
            BT {{ engine.bt_port }}
          </span>
        </p>
      </div>
      <div class="flex items-center gap-2">
        <button
          class="flex h-9 items-center gap-1.5 rounded-lg bg-accent px-3 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50"
          :disabled="!enabled"
          data-testid="download-add"
          @click="openAdd"
        >
          <AppIcon name="plus" :size="13" />
          {{ t('downloads.add') }}
        </button>
        <button
          class="flex h-9 items-center gap-1.5 rounded-lg border border-line px-3 text-xs text-muted transition-colors hover:border-accent/50 hover:text-ink"
          data-testid="download-settings"
          @click="openSettings"
        >
          <AppIcon name="gear" :size="13" />
          {{ t('downloads.settingsButton') }}
        </button>
      </div>
    </div>

    <!-- 引擎关闭：整页没有可用操作，直接说清楚去哪开 -->
    <div
      v-if="!enabled && !loading"
      class="mt-6 rounded-xl border border-amber-500/30 bg-amber-500/10 px-4 py-3 text-[12px] leading-relaxed fg-warn"
    >
      <p class="font-medium">{{ t('downloads.disabled.title') }}</p>
      <p class="mt-1">{{ t('downloads.disabled.hint') }}</p>
    </div>

    <template v-else>
      <div class="mt-5 flex items-center gap-1 border-b border-line">
        <button
          v-for="item in TABS"
          :key="item"
          class="relative -mb-px border-b-2 px-3 py-2 text-xs transition-colors"
          :class="
            tab === item
              ? 'border-accent font-medium text-ink'
              : 'border-transparent text-muted hover:text-ink'
          "
          @click="tab = item"
        >
          {{ t(`downloads.tabs.${item}`) }}
          <span class="ml-1 text-[11px] text-muted">{{ groups[item].length }}</span>
        </button>
        <button
          v-if="tab === 'done' && groups.done.length"
          class="ml-auto mb-1 rounded-md px-2 py-1 text-[11px] text-muted transition-colors hover:text-ink"
          @click="purgeStopped"
        >
          {{ t('downloads.clearDone') }}
        </button>
      </div>

      <div v-if="loading" class="mt-4 space-y-2">
        <div v-for="i in 3" :key="i" class="skeleton h-16 rounded-xl" />
      </div>

      <div v-else class="mt-3 overflow-hidden rounded-xl border border-line">
        <div
          v-for="(task, index) in rows"
          :key="task.gid"
          class="flex items-start gap-3 border-b border-line/60 px-4 py-3 last:border-b-0"
          :class="index % 2 === 1 ? 'bg-elevated/40' : ''"
          :data-gid="task.gid"
        >
          <span class="mt-0.5 shrink-0 text-muted">
            <AppIcon :name="task.kind === 'bt' ? 'link' : 'download'" :size="15" />
          </span>

          <div class="min-w-0 flex-1">
            <div class="flex items-baseline gap-2">
              <span class="min-w-0 flex-1 truncate text-sm text-ink" :title="taskPath(task)">
                {{ taskName(task) }}
              </span>
              <span
                v-if="task.seeder && task.status === 'complete'"
                class="shrink-0 rounded bg-elevated px-1.5 py-0.5 text-[10px] text-muted"
              >
                {{ t('downloads.seeding') }}
              </span>
              <span v-if="task.kind === 'bt'" class="shrink-0 text-[10px] font-mono text-muted">BT</span>
            </div>

            <div class="mt-2 h-1 overflow-hidden rounded-full bg-elevated">
              <div
                class="h-full rounded-full transition-[width] duration-300"
                :class="task.status === 'error' ? 'bg-red-500/70' : 'bg-accent'"
                :style="{ width: `${percent(task)}%` }"
              />
            </div>

            <div class="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-muted">
              <span class="font-mono">
                {{ formatBytes(task.completedLength) }} / {{ formatBytes(task.totalLength) }}
                · {{ percent(task) }}%
              </span>
              <span v-if="task.status === 'active'" class="font-mono">
                ↓ {{ formatSpeed(task.downloadSpeed) }}
              </span>
              <span v-if="task.uploadSpeed > 0" class="font-mono">
                ↑ {{ formatSpeed(task.uploadSpeed) }}
              </span>
              <span v-if="task.connections">
                {{ t('downloads.connections', { count: task.connections }) }}
              </span>
              <span>{{ t('downloads.status.' + task.status) }}</span>
            </div>

            <p v-if="task.errorMessage" class="mt-1 text-[11px] fg-danger">
              {{ task.errorMessage }}
            </p>
          </div>

          <div class="flex shrink-0 items-center gap-0.5">
            <button
              v-if="task.status === 'active' || task.status === 'waiting' || task.status === 'paused'"
              class="rounded-md p-1.5 text-muted transition-colors hover:bg-elevated hover:text-ink disabled:opacity-40"
              :disabled="busyGid === task.gid"
              :title="t(task.status === 'paused' ? 'downloads.actions.resume' : 'downloads.actions.pause')"
              @click="togglePause(task)"
            >
              <AppIcon :name="task.status === 'paused' ? 'play' : 'pause'" :size="14" />
            </button>
            <button
              v-else
              class="rounded-md p-1.5 text-muted transition-colors hover:bg-elevated hover:text-ink"
              :title="t('downloads.actions.copyPath')"
              @click="copyPath(task)"
            >
              <AppIcon name="copy" :size="13" />
            </button>
            <button
              class="rounded-md p-1.5 text-muted transition-colors hover:bg-elevated hover-danger disabled:opacity-40"
              :disabled="busyGid === task.gid"
              :title="t('downloads.actions.remove')"
              @click="removeTask(task)"
            >
              <AppIcon name="trash" :size="14" />
            </button>
          </div>
        </div>

        <p v-if="!rows.length" class="px-4 py-6 text-center text-[12px] text-muted">
          {{ t(`downloads.empty.${tab}`) }}
        </p>
      </div>
    </template>

    <p
      v-if="error"
      class="mt-4 rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs fg-danger"
    >
      {{ error }}
    </p>

    <!-- ================= 新建下载 ================= -->
    <Teleport to="body">
      <div
        v-if="addOpen"
        class="fixed inset-0 z-[85] grid place-items-center modal-overlay bg-black/50 p-4"
        data-testid="download-dialog"
        @click.self="addOpen = false"
      >
        <div class="w-full max-w-md modal-card rounded-2xl border border-line bg-panel p-5 shadow-xl">
          <p class="text-sm font-medium text-ink">{{ t('downloads.dialog.title') }}</p>

          <div class="mt-3 flex items-center rounded-lg border border-line p-0.5 text-xs">
            <button
              class="flex-1 rounded px-2.5 py-1 transition-colors"
              :class="addMode === 'uri' ? 'bg-accent-soft text-accent' : 'text-muted hover:text-ink'"
              @click="addMode = 'uri'"
            >
              {{ t('downloads.dialog.modeUri') }}
            </button>
            <button
              class="flex-1 rounded px-2.5 py-1 transition-colors disabled:opacity-40"
              :class="addMode === 'torrent' ? 'bg-accent-soft text-accent' : 'text-muted hover:text-ink'"
              :disabled="!btEnabled"
              :title="btEnabled ? undefined : t('downloads.dialog.btDisabled')"
              @click="addMode = 'torrent'"
            >
              {{ t('downloads.dialog.modeTorrent') }}
            </button>
          </div>

          <template v-if="addMode === 'uri'">
            <textarea
              v-model="addUri"
              rows="4"
              spellcheck="false"
              :placeholder="t('downloads.dialog.uriPlaceholder')"
              class="mt-3 w-full resize-y rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
            />
            <p class="mt-1.5 text-[11px] leading-relaxed text-muted">
              {{ t('downloads.dialog.uriHint') }}
            </p>
          </template>

          <template v-else>
            <label
              class="mt-3 flex cursor-pointer items-center justify-center rounded-lg border border-dashed border-line px-3 py-6 text-xs text-muted transition-colors hover:border-accent/50 hover:text-ink"
            >
              <input
                type="file"
                accept=".torrent,application/x-bittorrent"
                class="hidden"
                @change="addFile = ($event.target as HTMLInputElement).files?.[0] ?? null"
              />
              <span class="truncate">
                {{ addFile ? addFile.name : t('downloads.dialog.pickFile') }}
              </span>
            </label>
          </template>

          <label class="mt-3 block text-[12px] text-muted">
            {{ t('downloads.dialog.dir') }}
            <select
              v-if="engine?.roots.length"
              v-model="addDir"
              class="mt-1 w-full rounded-lg border border-line bg-elevated px-2 py-2 text-xs text-ink outline-none focus:border-accent"
            >
              <option v-for="root in engine.roots" :key="root" :value="root">{{ root }}</option>
            </select>
            <p v-else class="mt-1 rounded-lg border border-line bg-elevated px-2 py-2 text-xs text-muted">
              {{ t('downloads.dialog.defaultDir') }}
            </p>
          </label>

          <p v-if="addError" class="mt-3 text-xs fg-danger">{{ addError }}</p>

          <div class="mt-5 flex justify-end gap-2">
            <button
              class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
              @click="addOpen = false"
            >
              {{ t('common.action.cancel') }}
            </button>
            <button
              class="rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50"
              :disabled="addBusy"
              data-testid="download-submit"
              @click="submitAdd"
            >
              {{ addBusy ? t('downloads.adding') : t('downloads.dialog.submit') }}
            </button>
          </div>
        </div>
      </div>
    </Teleport>

    <!-- ================= 引擎设置 ================= -->
    <Teleport to="body">
      <div
        v-if="settingsOpen"
        class="fixed inset-0 z-[85] grid place-items-center modal-overlay bg-black/50 p-4"
        data-testid="download-settings-dialog"
        @click.self="settingsOpen = false"
      >
        <div
          class="modal-card flex max-h-[88vh] w-full max-w-lg flex-col rounded-2xl border border-line bg-panel shadow-xl"
        >
          <div class="flex items-center justify-between border-b border-line px-5 py-3">
            <p class="text-sm font-medium text-ink">{{ t('downloads.settings.title') }}</p>
            <button class="rounded-md p-1 text-muted hover:text-ink" @click="settingsOpen = false">
              <AppIcon name="close" :size="15" />
            </button>
          </div>

          <div class="min-h-0 flex-1 overflow-y-auto px-5 py-4">
            <template v-if="settings">
              <div class="space-y-2.5">
                <label class="flex items-center justify-between gap-3 text-xs text-ink">
                  <span>{{ t('downloads.settings.enable') }}</span>
                  <input v-model="settings.enable" type="checkbox" class="accent-[var(--accent)]" />
                </label>
                <label class="flex items-center justify-between gap-3 text-xs text-ink">
                  <span>{{ t('downloads.settings.enableBt') }}</span>
                  <input v-model="settings.enable_bt" type="checkbox" class="accent-[var(--accent)]" />
                </label>
                <label class="flex items-center justify-between gap-3 text-xs text-ink">
                  <span>{{ t('downloads.settings.disableDht') }}</span>
                  <input
                    v-model="settings.bt_disable_dht"
                    type="checkbox"
                    class="accent-[var(--accent)]"
                  />
                </label>
              </div>

              <div class="mt-4 grid gap-3 sm:grid-cols-2">
                <label class="block text-[12px] text-muted">
                  {{ t('downloads.settings.segments') }}
                  <input
                    v-model.number="settings.segments"
                    type="number"
                    min="1"
                    max="32"
                    class="mt-1 w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
                  />
                </label>
                <label class="block text-[12px] text-muted">
                  {{ t('downloads.settings.maxConcurrent') }}
                  <input
                    v-model.number="settings.max_concurrent"
                    type="number"
                    min="1"
                    max="32"
                    class="mt-1 w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
                  />
                </label>
                <label class="block text-[12px] text-muted">
                  {{ t('downloads.settings.portMin') }}
                  <input
                    v-model.number="settings.bt_listen_port_min"
                    type="number"
                    min="0"
                    max="65535"
                    class="mt-1 w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
                  />
                </label>
                <label class="block text-[12px] text-muted">
                  {{ t('downloads.settings.portMax') }}
                  <input
                    v-model.number="settings.bt_listen_port_max"
                    type="number"
                    min="0"
                    max="65535"
                    class="mt-1 w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
                  />
                </label>
              </div>

              <div class="mt-4">
                <p class="text-[12px] text-muted">{{ t('downloads.settings.roots') }}</p>
                <div class="mt-1 space-y-1">
                  <p
                    v-for="root in engine?.roots ?? []"
                    :key="root"
                    class="truncate rounded-lg border border-line bg-elevated px-2 py-1.5 font-mono text-[11px] text-ink"
                    :title="root"
                  >
                    {{ root }}
                  </p>
                  <p
                    v-if="!(engine?.roots ?? []).length"
                    class="rounded-lg border border-line bg-elevated px-2 py-1.5 text-[11px] text-muted"
                  >
                    {{ t('downloads.dialog.defaultDir') }}
                  </p>
                </div>
                <p class="mt-1 text-[11px] leading-relaxed text-muted">
                  {{ t('downloads.settings.rootsHint') }}
                </p>
              </div>

              <p
                class="mt-4 rounded-lg border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-[11px] leading-relaxed fg-warn"
              >
                {{ t('downloads.settings.restartHint') }}
              </p>
              <p v-if="savedNeedsRestart" class="mt-2 text-[11px] text-accent">
                {{ t('downloads.settings.restartPending') }}
              </p>
            </template>
            <p v-else class="text-xs text-muted">{{ t('common.action.loading') }}</p>

            <div class="mt-5 rounded-xl border border-line p-3">
              <p class="text-[12px] font-medium text-ink">{{ t('downloads.rpc.title') }}</p>
              <p class="mt-1 text-[11px] leading-relaxed text-muted">{{ t('downloads.rpc.hint') }}</p>

              <div class="mt-2 flex items-center gap-2">
                <span class="w-12 shrink-0 text-[11px] text-muted">{{ t('downloads.rpc.address') }}</span>
                <code class="min-w-0 flex-1 truncate rounded bg-elevated px-2 py-1 text-[11px] text-ink">{{
                  rpcUrl
                }}</code>
                <button
                  class="shrink-0 rounded-md p-1 text-muted hover:text-ink"
                  :title="t('downloads.rpc.copy')"
                  @click="copyText(rpcUrl, t('downloads.rpc.copied'))"
                >
                  <AppIcon name="copy" :size="13" />
                </button>
              </div>

              <div class="mt-1.5 flex items-center gap-2">
                <span class="w-12 shrink-0 text-[11px] text-muted">{{ t('downloads.rpc.secret') }}</span>
                <code class="min-w-0 flex-1 truncate rounded bg-elevated px-2 py-1 font-mono text-[11px] text-ink">{{
                  secretShown ? engine?.rpc_secret : '••••••••••••'
                }}</code>
                <button
                  class="shrink-0 rounded-md p-1 text-muted hover:text-ink"
                  :title="t(secretShown ? 'downloads.rpc.hide' : 'downloads.rpc.show')"
                  @click="secretShown = !secretShown"
                >
                  <AppIcon name="eye" :size="13" />
                </button>
                <button
                  class="shrink-0 rounded-md p-1 text-muted hover:text-ink"
                  :title="t('downloads.rpc.copy')"
                  @click="copyText(engine?.rpc_secret ?? '', t('downloads.rpc.copied'))"
                >
                  <AppIcon name="copy" :size="13" />
                </button>
              </div>
            </div>

            <p v-if="settingsError" class="mt-3 text-xs fg-danger">{{ settingsError }}</p>
          </div>

          <div class="flex justify-end gap-2 border-t border-line px-5 py-3">
            <button
              class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
              @click="settingsOpen = false"
            >
              {{ t('common.action.close') }}
            </button>
            <button
              class="rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50"
              :disabled="settingsBusy || !settings"
              data-testid="download-settings-save"
              @click="saveSettings"
            >
              {{ settingsBusy ? t('downloads.settings.saving') : t('common.action.save') }}
            </button>
          </div>
        </div>
      </div>
    </Teleport>

    <Transition name="fade">
      <div
        v-if="toast"
        class="pointer-events-none fixed bottom-6 left-1/2 z-[90] -translate-x-1/2 rounded-full border border-line bg-panel px-4 py-2 text-xs text-ink shadow-lg"
      >
        {{ toast }}
      </div>
    </Transition>
  </section>
</template>
