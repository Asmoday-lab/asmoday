<script setup lang="ts">
/**
 * 下载引擎页（v4.9.31 起，v4.9.38 把设置从弹窗搬进页内）。
 *
 * 内置引擎是**一级功能**（与文件管理、媒体库同级）：任务列表、新建下载、引擎参数
 * 与 RPC 接入信息都在这里，不用去 config.toml 手写 TOML。
 *
 * 页面只有两个视图：`tasks`（任务）与 `settings`（引擎设置）。设置曾经是弹窗，
 * 字段一多就要在固定高度里滚，且没法一边看列表一边改；现在整页切换，宽屏下
 * 参数卡与目录卡并排，底部是 RPC 接入卡。
 *
 * 两个通道共用同一个 DownloadManager：本页走 Cookie 会话的 `/api/downloads*`，
 * AriaNg 等外部客户端走 `/jsonrpc` + `token:<secret>`。密钥只在设置视图里
 * 给出供复制，不落 localStorage。
 */
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { onBeforeRouteLeave } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import {
  downloadsApi,
  type DownloadTask,
  type EngineInfo,
  type GlobalStat,
  type Snapshot,
} from '../api/downloads'
import { filesApi, type MountPoint } from '../api/files'
import { useBackClose } from '../composables/useBackClose'
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

/** 页内视图：任务 / 引擎设置（v4.9.38；设置不再走弹窗） */
type View = 'tasks' | 'settings'
const view = ref<View>('tasks')

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

/** 保存目录的编辑态：只存「挂载点 id + 子路径」，绝对路径由后端解析（v4.9.37） */
interface RootDraft {
  mount_id: string
  rel: string
}

/**
 * 草稿基线（v4.9.55）：进设置视图时和草稿**同时**快照，保存成功后各自推进。
 *
 * 之前两块草稿是直接跟 `engine.value` 的 live 状态比：草稿只有第一次进设置视图
 * 才填，而页面一挂载 `loadEngine()` 就把保存目录拉回来了 —— 用户压根没进过设置页，
 * `rootDrafts` 还是空数组，于是「允许的保存目录」凭空变脏，点侧边栏离开就弹
 * 「离开引擎设置？」。同理，保存参数后重新拉 engine 也可能让 live 值与草稿漂移。
 * `draftsReady` 同时挡住首次加载那段空窗（加载失败时也不许弹）。
 */
const rootsBaseline = ref<RootDraft[]>([])
const trackersBaseline = ref<string[]>([])
const draftsReady = ref(false)

const settings = ref<DownloadConfig | null>(null)
const settingsBusy = ref(false)
const settingsError = ref('')
const secretShown = ref(false)
/** 刷新密钥的两段式「上膛」：第一次点击只确认，3 秒内再点才真的换 */
const rotateArmed = ref(false)
const secretRotating = ref(false)
let armTimer: number | undefined
/** 保存时的整份配置：`POST /api/config` 收完整结构，不能只发 `[download]` */
const fullConfig = ref<Record<string, unknown> | null>(null)
const savedNeedsRestart = ref(false)
/**
 * 参数快照（v4.9.40）：进设置视图与保存成功后各记一次 JSON 串，比对即知有没有未保存改动。
 * 比逐字段 watch 省事，也不会漏掉新加的字段。
 */
const paramsBaseline = ref('')

/** 三个开关的元数据：模板里 v-for 一次，省得写三段几乎一样的行 */
const toggleFields: {
  key: 'enable' | 'enable_bt' | 'bt_disable_dht'
  label: string
  hint: string
}[] = [
  { key: 'enable', label: 'downloads.settings.enable', hint: 'downloads.settings.enableHint' },
  { key: 'enable_bt', label: 'downloads.settings.enableBt', hint: 'downloads.settings.enableBtHint' },
  {
    key: 'bt_disable_dht',
    label: 'downloads.settings.disableDht',
    hint: 'downloads.settings.disableDhtHint',
  },
]

/** 挂载点清单（保存目录只能从这里选，v4.9.37） */
const mounts = ref<MountPoint[]>([])
const rootDrafts = ref<RootDraft[]>([])
const rootWarnings = ref<string[]>([])
const rootsBusy = ref(false)
const newRootMount = ref('')
const newRootRel = ref('downloads')

/** Tracker 编辑态：一行一个，保存时交给后端归一化（v4.9.39） */
const trackerText = ref('')
const trackersBusy = ref(false)
const trackerWarnings = ref<string[]>([])

/**
 * 「填入常用列表」的底稿：公开 tracker（ngosang/trackerslist 常青项）。
 * 只是起点，不是默认值——不替用户静默 announce 到第三方。
 */
const COMMON_TRACKERS = [
  'udp://tracker.opentrackr.org:1337/announce',
  'udp://open.tracker.cl:1337/announce',
  'udp://open.demonii.com:1337/announce',
  'udp://tracker.torrent.eu.org:451/announce',
  'udp://exodus.desync.com:6969/announce',
  'udp://tracker.openbittorrent.com:6969/announce',
  'udp://explodie.org:6969/announce',
  'http://tracker.openbittorrent.com:80/announce',
  'https://tracker.tamersunion.org:443/announce',
  'udp://tracker.dler.org:6969/announce',
]

/** 悬挂中的「要不要放弃改动」询问：一次只开一个（先声明，下面的滚动锁要用） */
const leaveOpen = ref(false)
let leaveResolve: ((ok: boolean) => void) | null = null

useScrollLock(computed(() => addOpen.value || leaveOpen.value))

/**
 * 未保存改动（v4.9.40）：三块草稿各自与「已保存态」比对。
 *
 * 设置视图里参数、保存目录、Tracker 是三个独立接口，任何一块改了都算未保存；
 * 返回按钮与路由离开都靠这个判定，避免「点返回，填的东西悄悄没了」。
 */
const rootsDirty = computed(
  () => draftsReady.value && rootPairs(rootDrafts.value) !== rootPairs(rootsBaseline.value),
)

const trackersDirty = computed(
  () =>
    draftsReady.value &&
    trackerLines(trackerText.value) !== trackerLines(trackersBaseline.value),
)

const paramsDirty = computed(
  () =>
    draftsReady.value &&
    !!settings.value &&
    paramsBaseline.value !== '' &&
    JSON.stringify(settings.value) !== paramsBaseline.value,
)

/** 弹窗里逐条列出来的「丢了什么」 */
const unsavedParts = computed(() => {
  const parts: string[] = []
  if (paramsDirty.value) parts.push('downloads.settings.unsavedParams')
  if (rootsDirty.value) parts.push('downloads.settings.unsavedRoots')
  if (trackersDirty.value) parts.push('downloads.settings.unsavedTrackers')
  return parts
})

/** 只有设置视图里的草稿才需要保护：任务列表视图不该被「离开引擎设置？」拦住 */
const unsaved = computed(() => view.value === 'settings' && unsavedParts.value.length > 0)

function askLeave(): Promise<boolean> {
  if (leaveOpen.value) return Promise.resolve(false)
  leaveOpen.value = true
  return new Promise<boolean>((resolve) => {
    leaveResolve = resolve
  })
}

/** `ok = true` 表示放弃草稿放行，`false` 表示留在设置视图继续编辑 */
function settleLeave(ok: boolean) {
  leaveOpen.value = false
  const resolve = leaveResolve
  leaveResolve = null
  resolve?.(ok)
}

/** 目录草稿的比对键：挂载点 id + 子路径（顺序也算改动，用户可能是故意排的） */
function rootPairs(list: { mount_id: string; rel: string }[]): string {
  return list.map((root) => `${root.mount_id}\u0000${root.rel}`).join('\u0001')
}

/** Tracker 比对键：换行 / 逗号混写先归一，再比对（粘贴格式不同不算改动） */
function trackerLines(value: string | string[]): string {
  const lines = Array.isArray(value) ? value : value.split(/[\r\n,]+/)
  return lines
    .map((line) => line.trim())
    .filter(Boolean)
    .join('\n')
}

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

/** 可写入的挂载点（只读的不给选；掉线的显示但禁用） */
const writableMounts = computed(() => mounts.value.filter((mount) => !mount.readonly))

/** 编辑态标签：`挂载点名 / 子路径`，根目录只显示名字 */
function rootLabel(draft: RootDraft): string {
  const mount = mounts.value.find((item) => item.id === draft.mount_id)
  const name = mount?.name ?? draft.mount_id
  return draft.rel ? `${name} / ${draft.rel}` : name
}

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
  // 默认落到第一个在线的保存目录，用户不用每次重选
  addDir.value = engine.value?.roots.find((root) => root.online)?.path ?? ''
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

/**
 * 进设置视图（v4.9.38：页内第二视图，不再是弹窗）。
 *
 * 只有第一次进入才拉配置与挂载点：来回切换时保留用户正在编辑的草稿，
 * 不会把填了一半的参数重置掉（要丢掉草稿，走返回时的确认弹窗）。
 */
async function openSettings() {
  view.value = 'settings'
  scrollToTop()
  // 只有「三块草稿都已就位」才算来过：上次加载到一半失败，下次进来要重试
  if (draftsReady.value) return
  settingsError.value = ''
  savedNeedsRestart.value = false
  rootWarnings.value = []
  try {
    // 与设置页同一份配置：只改 `[download]`，其余原样回传
    const res = await fetch('/api/config')
    if (!res.ok) throw new Error(await readApiError(res))
    const config = (await res.json()) as Record<string, unknown>
    fullConfig.value = config
    settings.value = structuredClone(config.download as DownloadConfig)
    paramsBaseline.value = JSON.stringify(settings.value)
    if (!engine.value) await loadEngine()
    // 保存目录只能选挂载点：拉实时清单（文件页每次进入都重拉，这里同理）
    mounts.value = (await filesApi.mounts()).mounts
    const savedRoots: RootDraft[] = (engine.value?.roots ?? []).map((root) => ({
      mount_id: root.mount_id,
      rel: root.rel,
    }))
    const savedTrackers = [...(engine.value?.trackers ?? [])]
    rootDrafts.value = savedRoots.map((root) => ({ ...root }))
    rootsBaseline.value = savedRoots
    trackerText.value = savedTrackers.join('\n')
    trackersBaseline.value = savedTrackers
    newRootMount.value = writableMounts.value.find((mount) => mount.online)?.id ?? ''
    // 最后才放行脏检测：上面的窗口里草稿还是空的，不能让离开确认先弹出来
    draftsReady.value = true
  } catch (err) {
    settingsError.value = err instanceof Error ? err.message : t('downloads.loadFailed')
  }
}

/**
 * 回下载列表（v4.9.40）：有未保存改动先确认，确认放弃才回滚草稿并切视图。
 *
 * 「回滚」不只是切视图——留着脏草稿，下次进来还是旧输入，容易误以为已保存。
 * 切完顺手滚到顶部，否则用户停在设置页滚动位置，看到的还是列表中段。
 */
async function showTasks() {
  if (unsaved.value) {
    const ok = await askLeave()
    if (!ok) return
    discardDrafts()
  }
  view.value = 'tasks'
  scrollToTop()
}

/** 丢掉三块草稿，回到「已保存态」 */
function discardDrafts() {
  // 基线是纯 JSON 串：不能对 `fullConfig.download` 做 structuredClone——那份是响应式代理，
  // 浏览器会抛 DataCloneError（v4.9.40 实测踩到）
  if (paramsBaseline.value) {
    settings.value = JSON.parse(paramsBaseline.value) as DownloadConfig
  }
  // 回滚到基线快照，不碰 live engine —— 它可能在草稿编辑期间被别的操作刷新过
  rootDrafts.value = rootsBaseline.value.map((root) => ({ ...root }))
  trackerText.value = trackersBaseline.value.join('\n')
  rootWarnings.value = []
  trackerWarnings.value = []
  settingsError.value = ''
}

/** 本页的滚动容器是布局里的 `#app-scroll`，不是 body */
function scrollToTop() {
  document.getElementById('app-scroll')?.scrollTo({ top: 0 })
}

/**
 * 系统返回键先回任务列表，而不是直接离开本页（v4.9.41）。
 *
 * 设置是**页内视图**、不在路由里：不接管的话 Android 物理返回 / 浏览器后退 /
 * 边缘滑动手势会一步跳过任务列表，直接退回进入下载页之前那页（通常是首页），
 * 用户看到的就是「点返回掉到聚合主页」。压一条同 URL 哨兵后，返回键等价于点
 * 「返回下载列表」——有未保存改动同样先确认，取消则视图不变、哨兵由
 * useBackClose 自动重挂。
 */
useBackClose(
  computed(() => view.value === 'settings'),
  () => void showTasks(),
)

/** 侧边栏跳走同样要拦：草稿只在内存里，离开就没了 */
onBeforeRouteLeave(async () => {
  if (!unsaved.value) return true
  const ok = await askLeave()
  if (ok) discardDrafts()
  return ok
})

function addRoot() {
  if (!newRootMount.value) return
  const rel = newRootRel.value.trim().replace(/^\/+/, '')
  if (rootDrafts.value.some((root) => root.mount_id === newRootMount.value && root.rel === rel)) return
  rootDrafts.value.push({ mount_id: newRootMount.value, rel })
  newRootRel.value = 'downloads'
}

function removeRoot(index: number) {
  rootDrafts.value.splice(index, 1)
}

/** 保存目录单独保存：写库 + 热替换，立即生效（v4.9.37） */
async function saveRoots() {
  if (rootsBusy.value) return
  if (!rootDrafts.value.length) {
    settingsError.value = t('downloads.settings.rootsEmpty')
    return
  }
  rootsBusy.value = true
  settingsError.value = ''
  try {
    const result = await downloadsApi.setRoots(rootDrafts.value.map((root) => ({ ...root })))
    const saved = result.roots.map((root) => ({ mount_id: root.mount_id, rel: root.rel }))
    rootDrafts.value = saved.map((root) => ({ ...root }))
    rootsBaseline.value = saved
    rootWarnings.value = result.warnings
    await loadEngine()
    showToast(t('downloads.settings.rootsSaved'))
  } catch (err) {
    settingsError.value = err instanceof Error ? err.message : t('downloads.actionFailed')
  } finally {
    rootsBusy.value = false
  }
}

/**
 * 保存 Tracker 列表（v4.9.39）。
 *
 * 语义同 aria2 的 `bt-tracker`：追加到**新**任务的 announce 列表，运行中的
 * 种子不追溯（librqbit 没有运行时增删 tracker 的接口）。写库 + 换内存，
 * 不需要重启。
 */
async function saveTrackers() {
  if (trackersBusy.value) return
  trackersBusy.value = true
  trackerWarnings.value = []
  settingsError.value = ''
  try {
    const result = await downloadsApi.setTrackers(
      trackerText.value.split('\n').map((line) => line.trim()).filter(Boolean),
    )
    trackerText.value = result.trackers.join('\n')
    trackersBaseline.value = [...result.trackers]
    trackerWarnings.value = result.rejected
    if (engine.value) engine.value.trackers = result.trackers
    showToast(t('downloads.trackers.saved'))
  } catch (err) {
    settingsError.value = err instanceof Error ? err.message : t('downloads.actionFailed')
  } finally {
    trackersBusy.value = false
  }
}

function fillCommonTrackers() {
  trackerText.value = COMMON_TRACKERS.join('\n')
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
    // 保存成功即新的基线：否则「刚存的改动」还会被当成未保存
    paramsBaseline.value = JSON.stringify(settings.value)
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

/**
 * 刷新 RPC 密钥（v4.9.36）。
 *
 * 两段式：第一次点击只是「上膛」（3 秒后自动复位），再点一次才换——密钥一换，
 * 已配置的 AriaNg / Aria2 Explorer 立刻 401，误触的代价是逐个重填，值得一次确认。
 * 新密钥直接就地写回 engine，不用重新拉接口；顺手展开明文，让用户复制。
 */
async function rotateSecret() {
  if (secretRotating.value) return
  if (!rotateArmed.value) {
    rotateArmed.value = true
    if (armTimer) window.clearTimeout(armTimer)
    armTimer = window.setTimeout(() => {
      rotateArmed.value = false
    }, 3000)
    return
  }
  if (armTimer) window.clearTimeout(armTimer)
  rotateArmed.value = false
  secretRotating.value = true
  settingsError.value = ''
  try {
    const result = await downloadsApi.rotateSecret()
    if (engine.value) engine.value.rpc_secret = result.rpc_secret
    secretShown.value = true
    showToast(t('downloads.rpc.rotated'))
  } catch (err) {
    settingsError.value = err instanceof Error ? err.message : t('downloads.actionFailed')
  } finally {
    secretRotating.value = false
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
  if (armTimer) window.clearTimeout(armTimer)
})
</script>

<template>
  <section class="mx-auto w-full max-w-5xl px-4 py-6 md:px-8 md:py-10">
    <!-- 页头：任务 / 引擎设置共用一行，右侧按钮随视图换 -->
    <div class="flex flex-wrap items-start gap-3">
      <div class="min-w-0 flex-1">
        <div class="flex flex-wrap items-center gap-2">
          <p class="text-sm font-semibold text-ink">
            {{ t(view === 'tasks' ? 'downloads.heading' : 'downloads.settings.title') }}
          </p>
          <span
            v-if="view === 'settings' && unsaved"
            class="rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] fg-warn"
            data-testid="download-unsaved-badge"
          >
            {{ t('downloads.settings.unsaved') }}
          </span>
        </div>
        <p
          v-if="view === 'tasks'"
          class="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-[12px] text-muted"
        >
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
        <p v-else class="mt-1 text-[12px] text-muted">{{ t('downloads.settings.subtitle') }}</p>
      </div>
      <div class="flex shrink-0 items-center gap-2">
        <template v-if="view === 'tasks'">
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
        </template>
        <button
          v-else
          class="flex h-9 items-center gap-1.5 rounded-lg border border-line px-3 text-xs text-muted transition-colors hover:border-accent/50 hover:text-ink"
          data-testid="download-back"
          @click="showTasks"
        >
          <AppIcon name="arrowLeft" :size="13" />
          {{ t('downloads.settings.back') }}
        </button>
      </div>
    </div>

    <template v-if="view === 'tasks'">
      <!-- 引擎关闭：任务视图下没有可用操作，直接说清楚去哪开 -->
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
    </template>

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
              <option
                v-for="root in engine.roots"
                :key="root.mount_id + root.rel"
                :value="root.path"
                :disabled="!root.online"
              >
                {{ root.online ? root.label : `${root.label}（${t('downloads.settings.offline')}）` }}
              </option>
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

    <!-- ================= 未保存改动确认（v4.9.40） ================= -->
    <Teleport to="body">
      <Transition name="fade">
        <div
          v-if="leaveOpen"
          class="fixed inset-0 z-[86] grid place-items-center modal-overlay bg-black/50 p-4"
          data-testid="download-unsaved-dialog"
          @click.self="settleLeave(false)"
        >
          <div class="w-full max-w-sm modal-card rounded-2xl border border-line bg-panel p-5 shadow-xl">
            <p class="text-sm font-medium text-ink">{{ t('downloads.settings.unsavedTitle') }}</p>
            <p class="mt-1.5 text-[12px] leading-relaxed text-muted">
              {{ t('downloads.settings.unsavedBody') }}
            </p>
            <ul class="mt-2.5 space-y-1">
              <li
                v-for="part in unsavedParts"
                :key="part"
                class="flex items-center gap-1.5 text-[12px] text-ink"
              >
                <span class="h-1 w-1 shrink-0 rounded-full bg-accent" />
                {{ t(part) }}
              </li>
            </ul>
            <div class="mt-4 flex items-center justify-end gap-2">
              <button
                class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
                data-testid="download-unsaved-stay"
                @click="settleLeave(false)"
              >
                {{ t('downloads.settings.leaveStay') }}
              </button>
              <button
                class="rounded-lg bg-red-500/90 px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90"
                data-testid="download-unsaved-discard"
                @click="settleLeave(true)"
              >
                {{ t('downloads.settings.leaveDiscard') }}
              </button>
            </div>
          </div>
        </div>
      </Transition>
    </Teleport>

    <!-- ================= 引擎设置（页内视图，v4.9.38） ================= -->
    <template v-if="view === 'settings'">
      <p v-if="settingsError && !settings" class="mt-5 rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs fg-danger">
        {{ settingsError }}
      </p>
      <div v-else-if="!settingsError && !settings" class="mt-5 space-y-3">
        <div class="skeleton h-32 rounded-xl" />
        <div class="skeleton h-32 rounded-xl" />
      </div>

      <div v-if="settings" class="mt-5 grid items-start gap-4 lg:grid-cols-2">
        <!-- 引擎参数：进程启动时读取，改动需重启 -->
        <section class="flex flex-col rounded-xl border border-line bg-panel p-4">
          <div class="flex items-center justify-between gap-2">
            <p class="text-[13px] font-medium text-ink">{{ t('downloads.settings.paramsTitle') }}</p>
            <span class="shrink-0 rounded bg-elevated px-1.5 py-0.5 text-[10px] text-muted">
              {{ t('downloads.settings.restartBadge') }}
            </span>
          </div>

          <div class="mt-3 divide-y divide-line/60 rounded-lg border border-line">
            <label
              v-for="item in toggleFields"
              :key="item.key"
              class="flex cursor-pointer items-center justify-between gap-3 px-3 py-2.5 text-xs text-ink transition-colors first:rounded-t-lg last:rounded-b-lg hover:bg-elevated/60"
            >
              <span class="min-w-0">
                <span class="block">{{ t(item.label) }}</span>
                <span class="mt-0.5 block text-[11px] leading-relaxed text-muted">{{ t(item.hint) }}</span>
              </span>
              <span class="relative inline-flex h-5 w-9 shrink-0 items-center">
                <input
                  v-model="settings[item.key]"
                  type="checkbox"
                  class="peer absolute inset-0 z-10 h-full w-full cursor-pointer opacity-0"
                />
                <span
                  class="h-5 w-9 rounded-full bg-elevated ring-1 ring-inset ring-line transition-colors peer-checked:bg-accent peer-checked:ring-accent peer-focus-visible:ring-2 peer-focus-visible:ring-accent/40"
                />
                <span
                  class="pointer-events-none absolute left-0.5 h-4 w-4 rounded-full bg-white shadow-sm transition-transform peer-checked:translate-x-4"
                />
              </span>
            </label>
          </div>

          <div class="mt-3 grid gap-3 sm:grid-cols-2">
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

          <p class="mt-3 rounded-lg bg-elevated/60 px-3 py-2 text-[11px] leading-relaxed text-muted">
            {{ t('downloads.settings.restartHint') }}
          </p>

          <div class="mt-auto flex items-center justify-end gap-2 pt-3">
            <span v-if="savedNeedsRestart" class="mr-auto text-[11px] text-accent">
              {{ t('downloads.settings.restartPending') }}
            </span>
            <button
              class="rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50"
              :disabled="settingsBusy"
              data-testid="download-settings-save"
              @click="saveSettings"
            >
              {{ settingsBusy ? t('downloads.settings.saving') : t('downloads.settings.saveParams') }}
            </button>
          </div>
        </section>

        <!-- Tracker：追加到新任务的 announce 列表（aria2 bt-tracker 语义） -->
        <section class="rounded-xl border border-line bg-panel p-4 lg:col-span-2">
          <div class="flex items-center justify-between gap-2">
            <p class="text-[13px] font-medium text-ink">{{ t('downloads.trackers.title') }}</p>
            <span class="shrink-0 rounded bg-accent-soft px-1.5 py-0.5 text-[10px] text-accent">
              {{ t('downloads.trackers.live') }}
            </span>
          </div>
          <p class="mt-1 text-[11px] leading-relaxed text-muted">
            {{ t('downloads.trackers.hint') }}
          </p>

          <textarea
            v-model="trackerText"
            rows="6"
            spellcheck="false"
            :disabled="!engine?.bt"
            :placeholder="t('downloads.trackers.placeholder')"
            class="mt-2 w-full resize-y rounded-lg border border-line bg-elevated px-3 py-2 font-mono text-[11px] leading-relaxed text-ink outline-none focus:border-accent disabled:opacity-60"
          />

          <div class="mt-2 flex flex-wrap items-center gap-1.5">
            <button
              class="rounded-md border border-line px-2 py-1 text-[11px] text-muted transition-colors hover:text-ink disabled:opacity-50"
              :disabled="!engine?.bt"
              @click="fillCommonTrackers"
            >
              {{ t('downloads.trackers.fillCommon') }}
            </button>
            <button
              class="rounded-md border border-line px-2 py-1 text-[11px] text-muted transition-colors hover:text-ink disabled:opacity-50"
              :disabled="!engine?.bt || !trackerText"
              @click="trackerText = ''"
            >
              {{ t('downloads.trackers.clear') }}
            </button>
            <button
              class="ml-auto rounded-lg border border-line px-2.5 py-1 text-[11px] text-ink transition-colors hover:border-accent/50 disabled:opacity-50"
              :disabled="trackersBusy || !engine?.bt"
              data-testid="download-save-trackers"
              @click="saveTrackers"
            >
              {{ trackersBusy ? t('downloads.settings.saving') : t('downloads.trackers.save') }}
            </button>
          </div>

          <p v-if="!engine?.bt" class="mt-1.5 text-[11px] fg-warn">
            {{ t('downloads.trackers.btOff') }}
          </p>
          <p v-for="warning in trackerWarnings" :key="warning" class="mt-1 text-[11px] fg-warn">
            {{ t('downloads.trackers.rejected', { entry: warning }) }}
          </p>
        </section>

        <!-- 右列：保存目录在前（常用），RPC 接入在后 -->
        <div class="space-y-4 lg:col-start-2 lg:row-start-1">
          <!-- 保存目录：挂载点子路径，写库 + 热替换，立即生效 -->
          <section class="rounded-xl border border-line bg-panel p-4">
            <div class="flex items-center justify-between gap-2">
              <p class="text-[13px] font-medium text-ink">{{ t('downloads.settings.roots') }}</p>
              <span class="shrink-0 rounded bg-accent-soft px-1.5 py-0.5 text-[10px] text-accent">
                {{ t('downloads.settings.rootsLive') }}
              </span>
            </div>
            <p class="mt-1 text-[11px] leading-relaxed text-muted">
              {{ t('downloads.settings.rootsHint') }}
            </p>

            <div class="mt-2 space-y-1">
              <div
                v-for="(root, index) in rootDrafts"
                :key="root.mount_id + root.rel"
                class="flex items-center gap-2 rounded-lg border border-line bg-elevated px-2.5 py-2"
              >
                <span class="min-w-0 flex-1 truncate text-[11px] text-ink">{{ rootLabel(root) }}</span>
                <button
                  class="shrink-0 rounded p-0.5 text-muted transition-colors hover:text-ink"
                  :title="t('downloads.settings.removeRoot')"
                  @click="removeRoot(index)"
                >
                  <AppIcon name="close" :size="12" />
                </button>
              </div>
              <p
                v-if="!rootDrafts.length"
                class="rounded-lg border border-dashed border-line px-2.5 py-2 text-[11px] text-muted"
              >
                {{ t('downloads.settings.rootsEmpty') }}
              </p>
            </div>

            <div class="mt-2 flex flex-wrap items-center gap-1.5">
              <select
                v-model="newRootMount"
                class="min-w-0 flex-1 rounded-lg border border-line bg-elevated px-2 py-1.5 text-[11px] text-ink outline-none focus:border-accent"
                data-testid="download-new-root-mount"
              >
                <option value="" disabled>{{ t('downloads.settings.pickMount') }}</option>
                <option
                  v-for="mount in writableMounts"
                  :key="mount.id"
                  :value="mount.id"
                  :disabled="!mount.online"
                >
                  {{ mount.online ? mount.name : `${mount.name}（${t('downloads.settings.offline')}）` }}
                </option>
              </select>
              <input
                v-model="newRootRel"
                :placeholder="t('downloads.settings.subPath')"
                class="w-28 rounded-lg border border-line bg-elevated px-2 py-1.5 text-[11px] text-ink outline-none focus:border-accent"
                data-testid="download-new-root-rel"
                @keyup.enter="addRoot"
              />
              <button
                class="rounded-lg border border-line px-2 py-1.5 text-[11px] text-muted transition-colors hover:text-ink disabled:opacity-50"
                :disabled="!newRootMount"
                data-testid="download-add-root"
                @click="addRoot"
              >
                {{ t('downloads.settings.addRoot') }}
              </button>
            </div>

            <p v-for="warning in rootWarnings" :key="warning" class="mt-1 text-[11px] fg-warn">
              {{ warning }}
            </p>

            <div class="mt-3 flex items-center justify-end gap-2">
              <span v-if="rootsDirty" class="mr-auto text-[11px] text-muted">
                {{ t('downloads.settings.rootsDirty') }}
              </span>
              <button
                class="rounded-lg border border-line px-3 py-1.5 text-xs transition-colors hover:text-ink disabled:opacity-50"
                :class="rootsDirty ? 'text-ink' : 'text-muted'"
                :disabled="rootsBusy || !rootsDirty"
                data-testid="download-save-roots"
                @click="saveRoots"
              >
                {{ rootsBusy ? t('downloads.settings.saving') : t('downloads.settings.saveRoots') }}
              </button>
            </div>
          </section>

          <!-- RPC 接入：外部客户端用同一份队列 -->
          <section class="rounded-xl border border-line bg-panel p-4">
            <p class="text-[13px] font-medium text-ink">{{ t('downloads.rpc.title') }}</p>
            <p class="mt-1 text-[11px] leading-relaxed text-muted">{{ t('downloads.rpc.hint') }}</p>

            <div class="mt-3 space-y-2">
              <div class="flex items-center gap-2 rounded-lg border border-line bg-elevated px-2.5 py-2">
                <span class="shrink-0 text-[11px] text-muted">{{ t('downloads.rpc.address') }}</span>
                <code class="min-w-0 flex-1 truncate font-mono text-[11px] text-ink">{{ rpcUrl }}</code>
                <button
                  class="shrink-0 rounded-md p-1 text-muted hover:text-ink"
                  :title="t('downloads.rpc.copy')"
                  @click="copyText(rpcUrl, t('downloads.rpc.copied'))"
                >
                  <AppIcon name="copy" :size="13" />
                </button>
              </div>

              <div class="flex items-center gap-2 rounded-lg border border-line bg-elevated px-2.5 py-2">
                <span class="shrink-0 text-[11px] text-muted">{{ t('downloads.rpc.secret') }}</span>
                <code class="min-w-0 flex-1 truncate font-mono text-[11px] text-ink">{{
                  secretShown ? engine?.rpc_secret : '••••••••••••'
                }}</code>
                <button
                  class="shrink-0 rounded-md p-1 text-muted hover:text-ink"
                  :title="t(secretShown ? 'downloads.rpc.hide' : 'downloads.rpc.show')"
                  @click="secretShown = !secretShown"
                >
                  <AppIcon :name="secretShown ? 'eyeOff' : 'eye'" :size="13" />
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

            <div class="mt-3 flex flex-wrap items-center gap-x-3 gap-y-1.5">
              <button
                class="rounded-md border border-line px-2 py-1 text-[11px] transition-colors hover:text-ink disabled:opacity-50"
                :class="rotateArmed ? 'fg-danger' : 'text-muted'"
                :disabled="secretRotating"
                data-testid="download-rotate-secret"
                @click="rotateSecret"
              >
                {{
                  secretRotating
                    ? t('downloads.rpc.rotating')
                    : rotateArmed
                      ? t('downloads.rpc.rotateConfirm')
                      : t('downloads.rpc.rotate')
                }}
              </button>
              <p class="min-w-0 flex-1 text-[11px] leading-relaxed text-muted">
                {{ t('downloads.rpc.rotateHint') }}
              </p>
            </div>
          </section>
        </div>

        <p v-if="settingsError" class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs fg-danger lg:col-span-2">
          {{ settingsError }}
        </p>
      </div>

      <!-- 设置页比一屏长：滚到底也要有出口，不用再滚回去找页头（v4.9.40） -->
      <div
        v-if="settings"
        class="mt-5 flex flex-wrap items-center justify-between gap-2 border-t border-line pt-4"
      >
        <span v-if="unsaved" class="text-[11px] leading-relaxed fg-warn">
          {{ t('downloads.settings.unsavedHint') }}
        </span>
        <span v-else class="text-[11px] leading-relaxed text-muted">
          {{ t('downloads.settings.backHint') }}
        </span>
        <button
          class="flex h-9 shrink-0 items-center gap-1.5 rounded-lg border border-line px-3 text-xs text-muted transition-colors hover:border-accent/50 hover:text-ink"
          data-testid="download-back-bottom"
          @click="showTasks"
        >
          <AppIcon name="arrowLeft" :size="13" />
          {{ t('downloads.settings.back') }}
        </button>
      </div>
    </template>

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
