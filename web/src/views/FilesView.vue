<script setup lang="ts">
/**
 * 文件管理（设计文档 3.2「极简无树」）：
 * - 入口页平铺展示所有挂载点卡片，不带常驻目录树，移动端也能最大化利用宽度；
 * - 进入挂载点后用面包屑（Home / 根名 / 子目录）回退与跳转；
 * - 网格/列表均虚拟化，超 1000 条按 has_more 分页续接；
 * - 双击按类型分流到**全局**预览：图片/音视频→MediaLightbox，Markdown/CSV/PDF→只读预览面板，
 *   纯文本/代码→CodeMirror，Office/压缩包→提示下载（3.2 文件预览矩阵）。
 */
import {
  computed,
  defineAsyncComponent,
  nextTick,
  onBeforeUnmount,
  onMounted,
  reactive,
  ref,
  watch,
} from 'vue'
import { useI18n } from 'vue-i18n'
import { useRouter } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import FilePreviewPane from '../components/FilePreviewPane.vue'
import VirtualGrid from '../components/VirtualGrid.vue'
import VirtualList from '../components/VirtualList.vue'
import { LANGUAGES, detectLanguage, type LanguageKey } from '../api/languages'
import {
  ApiError,
  filesApi,
  summarizeResults,
  type EntryInfo,
  type FileStat,
  type Listing,
  type MountPoint,
  type OnConflict,
} from '../api/files'
import { localizedCode, messageFromBody } from '../lib/errors'
import { sharesApi } from '../api/shares'
import { thumbUrl } from '../api/media'
import { formatSize, formatTime } from '../lib/format'
import { useTheme } from '../composables/useTheme'
import { useScrollLock } from '../composables/useScrollLock'
import { openLightbox, type LightboxItem } from '../stores/lightbox'

// CodeMirror 内核与语言包都在这个异步 chunk 里，首次打开编辑器才下载
const CodeEditor = defineAsyncComponent(() => import('../components/CodeEditor.vue'))

const { isDark } = useTheme()
const { t } = useI18n()
const router = useRouter()

/** 右键菜单 */
const menu = ref<{ x: number; y: number; entry: EntryInfo } | null>(null)
const menuEl = ref<HTMLElement | null>(null)

/**
 * 右键菜单定位（v2.8 修）：原来用固定 316px 估算高度，菜单实际有 11 项约 380px，
 * 矮窗口下「属性 / 删除」被挤出视口，点了没反应。改成先按光标位置渲染，
 * 下一帧按真实尺寸夹到视口内（放不下就向上翻转）。
 */
async function openMenu(event: MouseEvent, entry: EntryInfo) {
  const margin = 8
  menu.value = { x: event.clientX, y: event.clientY, entry }
  selected.value = entry.path
  await nextTick()
  const el = menuEl.value
  if (!el || !menu.value) return
  const rect = el.getBoundingClientRect()
  const maxX = Math.max(margin, window.innerWidth - rect.width - margin)
  const maxY = Math.max(margin, window.innerHeight - rect.height - margin)
  menu.value = {
    ...menu.value,
    x: Math.min(Math.max(margin, event.clientX), maxX),
    y: Math.min(Math.max(margin, event.clientY), maxY),
  }
}

function openTerminal(entry: EntryInfo) {
  const target = entry.is_dir ? entry.path : entry.path.replace(/\/[^/]*$/, '')
  menu.value = null
  router.push({ path: '/terminal', query: { path: target } })
}

async function copyPath(entry: EntryInfo) {
  try {
    await navigator.clipboard.writeText(entry.path)
    showToast(t('files.menu.pathCopied'))
  } catch {
    showToast(t('common.clipboard.denied'))
  }
  menu.value = null
}

function downloadEntry(entry: EntryInfo) {
  menu.value = null
  window.open(filesApi.rawUrl(entry.path), '_blank', 'noopener')
}

function menuOpenEntry() {
  const entry = menu.value?.entry
  menu.value = null
  if (entry) void activate(entry)
}

function menuOpenTerminal() {
  const entry = menu.value?.entry
  menu.value = null
  if (entry) openTerminal(entry)
}

function menuCopyPath() {
  const entry = menu.value?.entry
  if (entry) void copyPath(entry)
}

function menuDownload() {
  const entry = menu.value?.entry
  if (entry) downloadEntry(entry)
}

// ---------------- 分享 / 图床直链（设计文档 3.1 / 3.2） ----------------
const shareDialog = ref<{ entry: EntryInfo } | null>(null)
const shareForm = reactive({ password: '', days: 0, allowDownload: true })
const shareBusy = ref(false)
const shareResult = ref<{ url: string; token: string } | null>(null)
const shareError = ref('')

function openShareDialog() {
  const entry = menu.value?.entry
  menu.value = null
  if (!entry) return
  shareForm.password = ''
  shareForm.days = 7
  shareForm.allowDownload = true
  shareResult.value = null
  shareError.value = ''
  shareDialog.value = { entry }
}

async function createShare() {
  const dialog = shareDialog.value
  if (!dialog || shareBusy.value) return
  shareBusy.value = true
  shareError.value = ''
  try {
    const result = await sharesApi.create(dialog.entry.path, {
      password: shareForm.password || undefined,
      days: shareForm.days,
      allow_download: shareForm.allowDownload,
    })
    shareResult.value = { url: `${location.origin}${result.url}`, token: result.token }
  } catch (err) {
    shareError.value = err instanceof Error ? err.message : t('files.share.failed')
  } finally {
    shareBusy.value = false
  }
}

/** 图片一键复制图床直链：永久、无口令的 raw 地址 */
async function copyDirectLink(entry: EntryInfo) {
  menu.value = null
  try {
    const result = await sharesApi.create(entry.path, { days: 0, allow_download: true })
    await navigator.clipboard.writeText(`${location.origin}${sharesApi.rawUrl(result.token)}`)
    showToast(t('files.menu.directLinkCopied'))
  } catch (err) {
    showToast(err instanceof Error ? err.message : t('files.menu.directLinkFailed'))
  }
}

function menuShare() {
  const entry = menu.value?.entry
  if (entry) openShareDialog()
}

function menuDirectLink() {
  const entry = menu.value?.entry
  if (entry) void copyDirectLink(entry)
}

// ---------------- 新建 / 重命名 / 移动复制（设计文档 3.2.6 / 6.2 / 8.5） ----------------
//
// 全部走封装好的 Modal，**禁止** confirm()/alert()/prompt()（设计文档 8.4 第 10 条）。
const createDialog = ref<{
  kind: 'dir' | 'file'
  name: string
  busy: boolean
  error: string
} | null>(null)

function openCreateDialog(kind: 'dir' | 'file') {
  if (readonly.value) {
    showToast(t('files.readonly.tooltip'))
    return
  }
  createDialog.value = { kind, name: '', busy: false, error: '' }
}

async function submitCreate() {
  const dialog = createDialog.value
  if (!dialog || dialog.busy) return
  const name = dialog.name.trim()
  if (!name) {
    dialog.error = t('files.create.placeholder')
    return
  }
  dialog.busy = true
  dialog.error = ''
  try {
    if (dialog.kind === 'dir') await filesApi.mkdir(currentPath.value, name)
    else await filesApi.touch(currentPath.value, name)
    createDialog.value = null
    showToast(t('files.create.done'))
    await open(currentPath.value)
  } catch (err) {
    dialog.error = describe(err).message
  } finally {
    dialog.busy = false
  }
}

const renameDialog = ref<{
  entry: EntryInfo
  name: string
  busy: boolean
  error: string
} | null>(null)

function openRenameDialog() {
  const entry = menu.value?.entry
  menu.value = null
  if (!entry) return
  renameDialog.value = { entry, name: entry.name, busy: false, error: '' }
}

async function submitRename() {
  const dialog = renameDialog.value
  if (!dialog || dialog.busy) return
  const name = dialog.name.trim()
  if (!name || name === dialog.entry.name) {
    renameDialog.value = null
    return
  }
  dialog.busy = true
  dialog.error = ''
  try {
    const result = await filesApi.rename(dialog.entry.path, name)
    if (editor.value?.path === dialog.entry.path) {
      editor.value.path = result.new_path
      editor.value.name = name
    }
    renameDialog.value = null
    showToast(t('files.rename.done'))
    await open(currentPath.value)
  } catch (err) {
    dialog.error = describe(err).message
  } finally {
    dialog.busy = false
  }
}

type TransferMode = 'copy' | 'move'

/** 冲突策略按固定顺序展示：overwrite 风险最高放最上面但不预选（设计文档 3.2.6.2） */
const CONFLICT_OPTIONS: OnConflict[] = ['overwrite', 'rename', 'skip']

const transferDialog = ref<{
  mode: TransferMode
  targets: EntryInfo[]
  dest: string
  busy: boolean
  error: string
} | null>(null)

/** 冲突策略：默认「未选择」，强制用户明确表态（设计文档 3.2.6.2） */
const conflictChoice = ref<OnConflict | 'ask'>('ask')

function openTransferDialog(mode: TransferMode, targets?: EntryInfo[]) {
  const list = targets ?? (menu.value ? [menu.value.entry] : [])
  menu.value = null
  if (!list.length) return
  conflictChoice.value = 'ask'
  transferDialog.value = {
    mode,
    targets: list,
    // 默认目标 = 当前目录（用户改路径比从零输入快）
    dest: currentPath.value,
    busy: false,
    error: '',
  }
}

async function submitTransfer() {
  const dialog = transferDialog.value
  if (!dialog || dialog.busy) return
  const dest = dialog.dest.trim()
  if (!dest) {
    dialog.error = t('files.transfer.noTarget')
    return
  }
  if (conflictChoice.value === 'ask') {
    dialog.error = t('files.transfer.conflictRequired')
    return
  }
  dialog.busy = true
  dialog.error = ''
  try {
    const call = dialog.mode === 'copy' ? filesApi.copy : filesApi.move
    const paths = dialog.targets.map((entry) => entry.path)
    const response = await call(paths, dest, conflictChoice.value)
    const summary = summarizeResults(response)
    transferDialog.value = null
    exitSelect()
    showTransferResult(dialog.mode, summary)
    await open(currentPath.value)
  } catch (err) {
    dialog.error = describe(err).message
  } finally {
    dialog.busy = false
  }
}

interface TransferSummary {
  ok: number
  failed: number
  skipped: number
  failures: { path: string; source?: string; error_code?: string; message?: string }[]
}

const resultDialog = ref<({ mode: TransferMode } & TransferSummary) | null>(null)

function showTransferResult(mode: TransferMode, summary: TransferSummary) {
  if (summary.failed === 0) {
    showToast(t('files.transfer.done'))
    return
  }
  resultDialog.value = { mode, ...summary }
}

function failureText(item: { error_code?: string; message?: string }) {
  return localizedCode(item.error_code) ?? item.message ?? t('common.state.unknownError')
}

// ---------------- 多选（设计文档 3.2.6.6 / 8.6） ----------------
//
// 进入方式：桌面端 Ctrl/⌘ + 点击，移动端长按 500ms（位移 > 10px 判为滚动并取消）。
// 选中后底部滑出操作栏（安全区已加），列表容器底部同时补等量 padding，避免最后一行被挡。
const selectMode = ref(false)
const selection = ref<string[]>([])

const selectedEntries = computed(() => {
  const picked = new Set(selection.value)
  return displayEntries.value.filter((entry) => picked.has(entry.path))
})

function enterSelect(entry: EntryInfo) {
  selectMode.value = true
  if (!selection.value.includes(entry.path)) selection.value.push(entry.path)
}

function toggleSelect(entry: EntryInfo) {
  const index = selection.value.indexOf(entry.path)
  if (index >= 0) selection.value.splice(index, 1)
  else selection.value.push(entry.path)
  // 全部取消选中时自动退出多选，省掉一次「取消」点击
  if (!selection.value.length) exitSelect()
}

function exitSelect() {
  selectMode.value = false
  selection.value = []
}

function isSelected(path: string) {
  return selection.value.includes(path)
}

// ---- 长按判定 ----
const LONG_PRESS_MS = 500
const LONG_PRESS_SLOP = 10
let pressTimer: number | null = null
let pressOrigin = { x: 0, y: 0 }
/** 长按已触发：紧随其后的 click/touchend 必须被吞掉，否则会误打开文件 */
let pressConsumed = false

function cancelPress() {
  if (pressTimer !== null) {
    clearTimeout(pressTimer)
    pressTimer = null
  }
}

function onPressStart(event: TouchEvent, entry: EntryInfo) {
  if (selectMode.value) return
  const touch = event.touches[0]
  if (!touch) return
  pressOrigin = { x: touch.clientX, y: touch.clientY }
  pressConsumed = false
  cancelPress()
  pressTimer = window.setTimeout(() => {
    pressTimer = null
    pressConsumed = true
    navigator.vibrate?.(50)
    enterSelect(entry)
  }, LONG_PRESS_MS)
}

function onPressMove(event: TouchEvent) {
  if (pressTimer === null) return
  const touch = event.touches[0]
  if (!touch) return
  const dx = Math.abs(touch.clientX - pressOrigin.x)
  const dy = Math.abs(touch.clientY - pressOrigin.y)
  if (dx > LONG_PRESS_SLOP || dy > LONG_PRESS_SLOP) cancelPress()
}

// ---- 批量下载（设计文档 3.2.6.6：不做服务端 ZIP，逐个触发浏览器下载） ----
const bulkDownloadConfirm = ref<EntryInfo[] | null>(null)

function triggerDownload(entry: EntryInfo) {
  const link = document.createElement('a')
  link.href = filesApi.rawUrl(entry.path)
  link.rel = 'noopener'
  document.body.appendChild(link)
  link.click()
  link.remove()
}

function downloadSelected() {
  const targets = selectedEntries.value.filter((entry) => !entry.is_dir)
  if (!targets.length) {
    showToast(t('files.select.noDownloadable'))
    return
  }
  // > 10 个时浏览器多半会拦，先让用户确认
  if (targets.length > 10) {
    bulkDownloadConfirm.value = targets
    return
  }
  for (const entry of targets) triggerDownload(entry)
  exitSelect()
}

function confirmBulkDownload() {
  const targets = bulkDownloadConfirm.value ?? []
  bulkDownloadConfirm.value = null
  for (const entry of targets) triggerDownload(entry)
  exitSelect()
}

// ---------------- 删除（设计文档 3.2.6.5） ----------------
const deleteTargets = ref<EntryInfo[]>([])
const deleteBusy = ref(false)
const deleteError = ref('')
/** 删除目录时必须输入目录名：防误触，不是安全边界 */
const deleteConfirmName = ref('')
/** 删除前统计（项数 / 总大小），null = 统计中 */
const deleteSurvey = ref<{ items: number; bytes: number; truncated: boolean } | null>(null)

/** 只有「单选一个目录」才要求输入名字；批量走 60 秒一次性令牌 */
const deleteNeedsName = computed(
  () => deleteTargets.value.length === 1 && deleteTargets.value[0].is_dir,
)
const deleteTitle = computed(() => {
  if (deleteTargets.value.length > 1) return t('files.delete.titleBatch')
  return t(deleteTargets.value[0]?.is_dir ? 'files.delete.titleDir' : 'files.delete.titleFile')
})

const deleteNameMatches = computed(() => {
  if (!deleteNeedsName.value) return true
  const target = deleteTargets.value[0]
  if (!target) return false
  return deleteConfirmName.value.trim() === target.name.trim()
})

/** 删除统计请求序号（同 statDialog：不能用数组身份比较） */
let deleteRequestSeq = 0

function openDeleteDialog(targets?: EntryInfo[]) {
  const list = targets ?? (menu.value ? [menu.value.entry] : [])
  menu.value = null
  if (!list.length) return
  const seq = ++deleteRequestSeq
  deleteTargets.value = list
  deleteConfirmName.value = ''
  deleteError.value = ''
  deleteSurvey.value = null
  void (async () => {
    try {
      const survey = await filesApi.measure(list.map((entry) => entry.path))
      if (seq === deleteRequestSeq && deleteTargets.value.length) deleteSurvey.value = survey
    } catch {
      if (seq === deleteRequestSeq && deleteTargets.value.length) {
        deleteSurvey.value = { items: list.length, bytes: 0, truncated: false }
      }
    }
  })()
}

async function confirmDelete() {
  const targets = deleteTargets.value
  if (!targets.length || deleteBusy.value) return
  if (!deleteNameMatches.value) {
    deleteError.value = t('errors.E_NAME_MISMATCH')
    return
  }
  deleteBusy.value = true
  deleteError.value = ''
  try {
    const paths = targets.map((entry) => entry.path)
    const confirmName = deleteNeedsName.value ? targets[0].name : undefined
    await filesApi.remove(paths, confirmName)
    deleteTargets.value = []
    for (const entry of targets) {
      if (editor.value?.path === entry.path) closeEditor()
      if (selected.value === entry.path) selected.value = null
    }
    exitSelect()
    showToast(t(targets.length > 1 ? 'files.delete.doneBatch' : targets[0].is_dir ? 'files.delete.doneDir' : 'files.delete.doneFile'))
    await open(currentPath.value)
  } catch (err) {
    deleteError.value = describe(err).message
  } finally {
    deleteBusy.value = false
  }
}

// ---------------- 属性（设计文档 3.2 右键菜单 / 3.2.6.8 只读 stat） ----------------
const statDialog = ref<{
  entry: EntryInfo
  data: FileStat | null
  busy: boolean
  error: string
} | null>(null)

/** 属性请求序号：ref.value 是深响应 proxy，与原始对象恒不相等，只能用序号判定「仍是这次打开」 */
let statRequestSeq = 0

function openStatDialog() {
  const entry = menu.value?.entry
  menu.value = null
  if (!entry) return
  const seq = ++statRequestSeq
  statDialog.value = { entry, data: null, busy: true, error: '' }
  void (async () => {
    try {
      const data = await filesApi.stat(entry.path)
      if (seq === statRequestSeq && statDialog.value) {
        statDialog.value = { ...statDialog.value, data, busy: false }
      }
    } catch (err) {
      if (seq === statRequestSeq && statDialog.value) {
        statDialog.value = { ...statDialog.value, busy: false, error: describe(err).message }
      }
    }
  })()
}

/** 权限位显示成 `0644`：只用来给用户看，不参与任何判定 */
function formatMode(mode: number | null | undefined) {
  if (mode === null || mode === undefined) return '—'
  return `0${(mode & 0o7777).toString(8).padStart(3, '0')}`
}

/**
 * 属性页的「所在目录」（v2.9）：只显示挂载点内的相对路径，
 * 不再展示真实路径 —— 既和顶部的完整路径重复，又暴露服务端盘符。
 */
const statRelativeDir = computed(() => {
  const raw = statDialog.value?.data?.path ?? statDialog.value?.entry.path ?? ''
  if (!raw) return '/'
  const dir = raw.replace(/[\\/][^\\/]*$/, '') || '/'
  const normalized = dir.replace(/\\/g, '/')
  let matchedPrefix = ''
  for (const root of roots.value) {
    const prefix = root.path.replace(/\\/g, '/').replace(/\/+$/, '')
    if (
      prefix &&
      normalized.toLowerCase().startsWith(prefix.toLowerCase()) &&
      prefix.length > matchedPrefix.length
    ) {
      matchedPrefix = prefix
    }
  }
  if (matchedPrefix) {
    const rest = normalized.slice(matchedPrefix.length).replace(/^\/+/, '')
    return rest ? `/${rest}` : '/'
  }
  // 兜底：不在任何挂载点下（理论上不会发生），按段展示但隐藏盘符
  const parts = normalized.split('/').filter(Boolean).filter((part) => !/^[a-zA-Z]:$/.test(part))
  return parts.length ? `/${parts.join('/')}` : '/'
})

async function copyShareResult() {
  if (!shareResult.value) return
  try {
    await navigator.clipboard.writeText(shareResult.value.url)
    showToast(t('common.clipboard.linkCopied'))
  } catch {
    showToast(t('common.clipboard.denied'))
  }
}

const roots = ref<MountPoint[]>([])
const rootsImplicit = ref(false)
/** 当前目录所在挂载点是否只读：只读时全部变更入口置灰（设计文档 3.2.6） */
const readonly = computed(() => listing.value?.readonly ?? false)
/** 在线挂载点数量：1 个时跳过卡片页直达根目录（设计文档 3.2.1） */
const onlineMounts = computed(() => roots.value.filter((root) => root.online))
const singleMount = computed(() => onlineMounts.value.length === 1)
/** 工具栏「新建」下拉 */
const newMenuOpen = ref(false)
const currentPath = ref('')
const listing = ref<Listing | null>(null)
const loading = ref(true)
const loadingMore = ref(false)
const error = ref<{ status: number; message: string } | null>(null)
const selected = ref<string | null>(null)
const page = ref(1)

/** 隐藏文件开关（设计文档 3.2）：默认关闭，选择记在本地 */
const showHidden = ref(localStorage.getItem('asmoday.files.hidden') === '1')
/** 缩略图加载失败的路径：坏图/无权限时回退成类型图标（8.4.8） */
const thumbFailed = ref(new Set<string>())

type ViewMode = 'grid' | 'list'
const viewMode = ref<ViewMode>(
  (localStorage.getItem('asmoday.files.view') as ViewMode | null) === 'list' ? 'list' : 'grid',
)

// ---------------- 排序（设计文档 v2.7.0 3.2.1 / 8.4.5） ----------------
type SortKey = 'name' | 'mtime' | 'size'
type SortDir = 'asc' | 'desc'
const SORT_OPTIONS: { key: SortKey }[] = [{ key: 'name' }, { key: 'mtime' }, { key: 'size' }]

/** 排序偏好持久化到 localStorage，刷新后保持 */
function loadSortPref(): { key: SortKey; dir: SortDir } {
  try {
    const raw = JSON.parse(localStorage.getItem('asmoday.files.sort') ?? '{}') as {
      key?: string
      dir?: string
    }
    if (
      SORT_OPTIONS.some((option) => option.key === raw.key) &&
      (raw.dir === 'asc' || raw.dir === 'desc')
    ) {
      return { key: raw.key as SortKey, dir: raw.dir }
    }
  } catch {
    // 损坏的本地值直接回落默认
  }
  return { key: 'name', dir: 'asc' }
}

const sortKey = ref<SortKey>(loadSortPref().key)
const sortDir = ref<SortDir>(loadSortPref().dir)
const sortMenuOpen = ref(false)
/** 工具栏上的当前排序摘要（如「名称 ↑」） */
const sortLabel = computed(
  () => `${t(`files.sort.${sortKey.value}`)} ${sortDir.value === 'asc' ? '↑' : '↓'}`,
)

/** 同键点击切换升降序；变更后必须清空列表重新拉取（8.4.5 第 4 条） */
async function applySort(key: SortKey) {
  if (key === sortKey.value) {
    sortDir.value = sortDir.value === 'asc' ? 'desc' : 'asc'
  } else {
    sortKey.value = key
    sortDir.value = 'asc'
  }
  localStorage.setItem(
    'asmoday.files.sort',
    JSON.stringify({ key: sortKey.value, dir: sortDir.value }),
  )
  sortMenuOpen.value = false
  thumbFailed.value = new Set()
  if (currentPath.value) await open(currentPath.value)
  if (searchTerm.value) void runSearch()
}

// ---------------- 无限滚动（8.4.5 第 4 条：到底自动加载 + 骨架屏） ----------------
const sentinelEl = ref<HTMLElement | null>(null)
let scrollObserver: IntersectionObserver | null = null
watch(sentinelEl, (el) => {
  scrollObserver?.disconnect()
  if (!el) return
  scrollObserver = new IntersectionObserver(
    (entries) => {
      if (entries.some((entry) => entry.isIntersecting)) void loadMore()
    },
    { rootMargin: '160px' },
  )
  scrollObserver.observe(el)
})
onBeforeUnmount(() => scrollObserver?.disconnect())

/** 触屏设备没有双击语义，单击即打开 */
const coarsePointer = window.matchMedia?.('(hover: none)').matches ?? false

// ---------------- 搜索 ----------------
const searchTerm = ref('')
const searchResults = ref<EntryInfo[] | null>(null)
const searching = ref(false)
let searchTimer: number | undefined

function scheduleSearch() {
  window.clearTimeout(searchTimer)
  searchTimer = window.setTimeout(runSearch, 260)
}

async function runSearch() {
  const keyword = searchTerm.value.trim()
  if (!keyword) {
    searchResults.value = null
    return
  }
  searching.value = true
  try {
    const params = new URLSearchParams({ q: keyword })
    if (currentPath.value) params.set('path', currentPath.value)
    const res = await fetch(`/api/files/search?${params.toString()}`)
    if (!res.ok) throw new Error(`HTTP ${res.status}`)
    const data = (await res.json()) as {
      items: { name: string; path: string; is_dir: boolean; size: number; kind: string; ext: string; editable: boolean }[]
    }
    searchResults.value = data.items.map((item) => ({
      name: item.name,
      path: item.path,
      is_dir: item.is_dir,
      size: item.size,
      modified: null,
      kind: item.kind as EntryInfo['kind'],
      ext: item.ext,
      editable: item.editable,
    }))
  } catch (err) {
    showToast(err instanceof Error ? err.message : t('files.list.searchFailed'))
  } finally {
    searching.value = false
  }
}

function clearSearch() {
  searchTerm.value = ''
  searchResults.value = null
}

// ---------------- 上传 ----------------
interface UploadTask {
  id: number
  name: string
  progress: number
  status: 'uploading' | 'done' | 'error'
  error?: string
}

const uploads = ref<UploadTask[]>([])
const dragging = ref(false)
const fileInput = ref<HTMLInputElement | null>(null)
let uploadSeq = 0

function pickFiles() {
  // 只读挂载点直接拦在入口，不依赖后端 403（设计文档 8.4 第 7 条）
  if (readonly.value) {
    showToast(t('files.readonly.tooltip'))
    return
  }
  fileInput.value?.click()
}

async function onFilesPicked(event: Event) {
  const input = event.target as HTMLInputElement
  const files = input.files ? [...input.files] : []
  input.value = ''
  if (files.length) await uploadFiles(files)
}

async function onDrop(event: DragEvent) {
  dragging.value = false
  const files = event.dataTransfer?.files ? [...event.dataTransfer.files] : []
  if (files.length) await uploadFiles(files)
}

async function uploadFiles(files: File[]) {
  const dir = currentPath.value
  if (!dir) {
    showToast(t('files.list.uploadFirst'))
    return
  }
  for (const file of files) {
    const task: UploadTask = {
      id: ++uploadSeq,
      name: file.name,
      progress: 0,
      status: 'uploading',
    }
    uploads.value.push(task)
    try {
      await uploadOne(file, dir, task)
      task.status = 'done'
      task.progress = 100
    } catch (err) {
      task.status = 'error'
      task.error = err instanceof Error ? err.message : t('files.upload.error')
    }
  }
  await open(dir)
  window.setTimeout(() => {
    uploads.value = uploads.value.filter((task) => task.status !== 'done')
  }, 4000)
}

function uploadOne(file: File, dir: string, task: UploadTask): Promise<void> {
  return new Promise((resolve, reject) => {
    const url = `/api/files/upload?dir=${encodeURIComponent(dir)}&name=${encodeURIComponent(file.name)}`
    const xhr = new XMLHttpRequest()
    xhr.open('PUT', url)
    xhr.setRequestHeader('Content-Type', 'application/octet-stream')
    xhr.upload.onprogress = (event) => {
      if (event.lengthComputable) {
        task.progress = Math.round((event.loaded / event.total) * 100)
      }
    }
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        resolve()
        return
      }
      let message = `HTTP ${xhr.status}`
      try {
        message = messageFromBody(
          JSON.parse(xhr.responseText) as { code?: string; message?: string; error?: string },
          xhr.status,
        )
      } catch {
        /* 非 JSON */
      }
      reject(new Error(message))
    }
    xhr.onerror = () => reject(new Error(t('files.upload.network')))
    xhr.send(file)
  })
}

const displayEntries = computed(() => searchResults.value ?? entries.value)
const isSearching = computed(() => searchResults.value !== null)

interface EditorState {
  path: string
  name: string
  language: LanguageKey
  content: string
  original: string
  saving: boolean
  error: string | null
}

const editor = ref<EditorState | null>(null)
/** 只读预览面板（Markdown / CSV / PDF / 不支持格式） */
const preview = ref<EntryInfo | null>(null)
const editorLoading = ref(false)
const toast = ref('')

const entries = computed(() => listing.value?.entries ?? [])
const total = computed(() => listing.value?.total ?? 0)
const hasMore = computed(() => listing.value?.has_more ?? false)
const dirty = computed(() => !!editor.value && editor.value.content !== editor.value.original)

const crumbs = computed(() => {
  const path = currentPath.value
  if (!path) return [] as { label: string; path: string }[]
  const normalized = path.replace(/\\/g, '/')

  // 面包屑不暴露真实文件系统路径：优先把路径映射成「根名 + 相对段」的虚拟显示
  let matched: MountPoint | null = null
  let matchedPrefix = ''
  for (const root of roots.value) {
    const prefix = root.path.replace(/\\/g, '/').replace(/\/+$/, '')
    if (
      prefix &&
      normalized.toLowerCase().startsWith(prefix.toLowerCase()) &&
      prefix.length > matchedPrefix.length
    ) {
      matched = root
      matchedPrefix = prefix
    }
  }

  const result: { label: string; path: string }[] = []
  if (matched) {
    // 单挂载点模式下首节点就是挂载点本身，点击回根目录；多挂载点模式多点一次回卡片页
    result.push({ label: matched.name, path: matched.path })
    let acc = matchedPrefix
    const rest = normalized.slice(matchedPrefix.length).split('/').filter(Boolean)
    for (const part of rest) {
      acc = `${acc}/${part}`
      result.push({ label: part, path: acc })
    }
    return result
  }

  // 兜底：不在任何根下（理论上不会发生），按段展示但隐藏盘符前缀
  const parts = normalized.split('/').filter(Boolean)
  let acc = ''
  for (const part of parts) {
    // Windows 盘符（C:）后面必须紧跟斜杠，否则会被解析成「当前盘符相对路径」
    acc = acc ? `${acc}/${part}` : part.endsWith(':') ? `${part}/` : part
    result.push({ label: part, path: acc })
  }
  return result
})

function showToast(message: string) {
  toast.value = message
  window.setTimeout(() => {
    if (toast.value === message) toast.value = ''
  }, 2200)
}

function describe(err: unknown): { status: number; message: string } {
  if (err instanceof ApiError) return { status: err.status, message: err.message }
  return { status: 0, message: err instanceof Error ? err.message : t('common.state.unknownError') }
}

/** 重新加载挂载点，并回到挂载点入口页 */
async function boot() {
  loading.value = true
  error.value = null
  currentPath.value = ''
  listing.value = null
  selected.value = null
  searchTerm.value = ''
  preview.value = null
  try {
    // 每次进入都必须重新拉取，禁止本地缓存（设计文档 3.2.1）
    const data = await filesApi.mounts()
    roots.value = data.mounts
    rootsImplicit.value = data.implicit
    const online = data.mounts.filter((mount) => mount.online)
    if (!data.mounts.length) {
      error.value = null
    } else if (online.length === 1) {
      // 单挂载点：跳过卡片页直达根目录。这里用「直接打开」而不是再压一层路由，
      // 返回卡片页由面包屑的 home 按钮处理，逻辑等价且不产生历史栈膨胀。
      await openRoot(online[0])
      return
    }
  } catch (err) {
    error.value = describe(err)
  } finally {
    loading.value = false
  }
}

/** 打开挂载点根（内部走 open，但保留 loading 态避免白屏闪烁） */
async function openRoot(root: MountPoint) {
  if (!root.online) {
    showToast(t('files.roots.missingToast'))
    return
  }
  if (editor.value) closeEditor()
  await open(root.path)
}

/** 进入某个挂载点 */
function enterRoot(root: MountPoint) {
  void openRoot(root)
}

/** 面包屑首段：多挂载点回卡片页，单挂载点回该挂载点根目录 */
function goHome() {
  if (singleMount.value) {
    const root = onlineMounts.value[0]
    if (root && currentPath.value !== root.path) {
      if (editor.value) closeEditor()
      preview.value = null
      void open(root.path)
      return
    }
  }
  if (editor.value) closeEditor()
  preview.value = null
  currentPath.value = ''
  listing.value = null
  selected.value = null
  searchTerm.value = ''
  error.value = null
}

async function open(path: string, pageNo = 1) {
  loading.value = true
  error.value = null
  selected.value = null
  preview.value = null
  try {
    const data = await filesApi.list(path, pageNo, showHidden.value, sortKey.value, sortDir.value)
    listing.value = data
    currentPath.value = data.path
    page.value = data.page
  } catch (err) {
    error.value = describe(err)
  } finally {
    loading.value = false
  }
}

async function loadMore() {
  if (!hasMore.value || loadingMore.value) return
  loadingMore.value = true
  try {
    const next = await filesApi.list(
      currentPath.value,
      page.value + 1,
      showHidden.value,
      sortKey.value,
      sortDir.value,
    )
    if (listing.value) {
      listing.value = { ...next, entries: [...listing.value.entries, ...next.entries] }
    }
    page.value = next.page
  } catch (err) {
    error.value = describe(err)
  } finally {
    loadingMore.value = false
  }
}

/** 隐藏文件开关（设计文档 3.2）：切换后重新拉当前目录/搜索结果 */
async function toggleHidden() {
  showHidden.value = !showHidden.value
  localStorage.setItem('asmoday.files.hidden', showHidden.value ? '1' : '0')
  thumbFailed.value = new Set()
  if (currentPath.value) await open(currentPath.value)
  if (searchTerm.value) void runSearch()
}

function markThumbFailed(path: string) {
  thumbFailed.value = new Set(thumbFailed.value).add(path)
}

/** 图片与视频都能出真缩略图（视频由后端 ffmpeg 抽首帧，v2.8） */
function hasThumb(entry: EntryInfo) {
  return !entry.is_dir && (entry.kind === 'image' || entry.kind === 'video')
}

function closePreview() {
  preview.value = null
}

/** 预览里点「编辑源码」：交给 CodeMirror（不重复实现编辑器） */
function editFromPreview() {
  const entry = preview.value
  preview.value = null
  if (entry) void openEditor(entry)
}

function setView(mode: ViewMode) {
  viewMode.value = mode
  localStorage.setItem('asmoday.files.view', mode)
}

/** 可以进灯箱的类型（设计文档 3.2：图片必须能左右切换，音视频拉起全局播放器） */
const VIEWABLE_KINDS = new Set(['image', 'video', 'audio'])
/** 走只读预览面板的扩展名（Markdown / CSV / PDF） */
const PREVIEW_EXTS = new Set(['md', 'markdown', 'csv', 'tsv', 'pdf'])

function toLightboxItem(entry: EntryInfo): LightboxItem {
  return {
    path: entry.path,
    name: entry.name,
    kind: entry.kind as LightboxItem['kind'],
    subtitle: null,
  }
}

/** 灯箱的兄弟列表：当前目录（或搜索结果）里同类型的媒体，双击即可左右切换 */
function mediaSiblings(): EntryInfo[] {
  return displayEntries.value.filter((item) => !item.is_dir && VIEWABLE_KINDS.has(item.kind))
}

function openMediaViewer(entry: EntryInfo) {
  const list = mediaSiblings()
  const index = list.findIndex((item) => item.path === entry.path)
  openLightbox(list.map(toLightboxItem), Math.max(0, index))
}

async function activate(entry: EntryInfo) {
  // 多选态下双击不打开：否则一次误触就把整个多选打断
  if (selectMode.value) {
    toggleSelect(entry)
    return
  }
  if (entry.is_dir) {
    await open(entry.path)
    return
  }

  const ext = (entry.ext || '').toLowerCase()

  // 图片 / 视频 / 音频：交给全局灯箱，绝不自建 <img>/<video> 弹窗
  if (VIEWABLE_KINDS.has(entry.kind)) {
    openMediaViewer(entry)
    return
  }

  // Markdown / CSV / PDF：只读预览面板
  if (PREVIEW_EXTS.has(ext)) {
    if (editor.value) closeEditor()
    preview.value = entry
    return
  }

  // 纯文本 / 代码：内置 CodeMirror
  if (entry.editable) {
    await openEditor(entry)
    return
  }

  // Office / CAD / 压缩包 / 未知二进制：不引入解析库，只提示下载
  if (editor.value) closeEditor()
  preview.value = entry
}

function onEntryClick(entry: EntryInfo, event?: MouseEvent) {
  // 长按触发多选后，这一下 click 必须被吞掉（设计文档 8.6 第 1 条）
  if (pressConsumed) {
    pressConsumed = false
    return
  }
  // Ctrl / ⌘ + 点击 = 进入或追加多选
  if (event && (event.ctrlKey || event.metaKey)) {
    if (!selectMode.value) selectMode.value = true
    toggleSelect(entry)
    return
  }
  if (selectMode.value) {
    toggleSelect(entry)
    return
  }
  if (coarsePointer) {
    void activate(entry)
  } else {
    selected.value = entry.path
  }
}

async function openEditor(entry: EntryInfo) {
  preview.value = null
  editorLoading.value = true
  editor.value = {
    path: entry.path,
    name: entry.name,
    language: detectLanguage(entry.name),
    content: '',
    original: '',
    saving: false,
    error: null,
  }
  try {
    const text = await filesApi.readText(entry.path)
    const state = editor.value
    if (!state) return
    state.content = text
    state.original = text
  } catch (err) {
    const info = describe(err)
    if (editor.value) editor.value.error = info.message
  } finally {
    editorLoading.value = false
  }
}

function onEditorChange(value: string) {
  if (editor.value) editor.value.content = value
}

async function saveEditor() {
  const state = editor.value
  if (!state || state.saving) return
  state.saving = true
  state.error = null
  try {
    await filesApi.saveText(state.path, state.content)
    state.original = state.content
    showToast(t('files.editor.saved'))
    // 目录里的体积/时间需要刷新
    if (listing.value) {
      const data = await filesApi.list(currentPath.value, 1, showHidden.value, sortKey.value, sortDir.value)
      listing.value = { ...data, entries: [...data.entries] }
    }
  } catch (err) {
    state.error = describe(err).message
  } finally {
    state.saving = false
  }
}

function closeEditor() {
  if (dirty.value && !window.confirm(t('files.editor.confirmClose'))) return
  editor.value = null
  editorLoading.value = false
}

function iconFor(entry: EntryInfo) {
  if (entry.is_dir) return 'folder'
  switch (entry.kind) {
    case 'image':
      return 'gallery'
    case 'video':
      return 'video'
    case 'audio':
      return 'audio'
    case 'archive':
      return 'archive'
    case 'text':
      return 'fileText'
    default:
      return 'file'
  }
}

function toneFor(entry: EntryInfo) {
  if (entry.is_dir) return 'fg-warn'
  switch (entry.kind) {
    case 'image':
      return 'fg-info'
    case 'video':
      return 'fg-video'
    case 'audio':
      return 'fg-ok'
    case 'archive':
      return 'fg-archive'
    case 'text':
      return 'fg-text'
    default:
      return 'text-muted'
  }
}

// 分享 / 编辑器 / 预览 / 各类变更弹窗都是模态层，打开时锁住列表滚动
useScrollLock(
  computed(() =>
    Boolean(
      shareDialog.value ||
        editor.value ||
        preview.value ||
        deleteTargets.value.length ||
        createDialog.value ||
        renameDialog.value ||
        transferDialog.value ||
        resultDialog.value ||
        statDialog.value ||
        bulkDownloadConfirm.value,
    ),
  ),
)

onMounted(boot)
</script>

<template>
  <section
    class="relative flex h-full min-h-0 overflow-hidden"
    @dragover.prevent="dragging = true"
    @dragleave.prevent="dragging = false"
    @drop.prevent="onDrop"
  >

    <!-- 右栏：编辑器 或 列表 -->
    <div v-if="editor" class="flex min-w-0 flex-1 flex-col">
      <header class="flex h-11 shrink-0 items-center gap-3 border-b border-line bg-panel px-3">
        <button
          class="flex items-center gap-1.5 rounded-md px-2 py-1 text-xs text-muted transition-colors hover:bg-elevated hover:text-ink"
          @click="closeEditor"
        >
          <AppIcon name="arrowLeft" :size="14" />
          {{ t('files.editor.back') }}
        </button>
        <p class="min-w-0 flex-1 truncate text-[14px] text-ink">
          {{ editor.name }}
          <span v-if="dirty" class="text-accent" :title="t('files.editor.unsaved')">●</span>
        </p>
        <select
          v-model="editor.language"
          class="rounded-md border border-line bg-elevated px-1.5 py-1 text-xs text-ink outline-none focus:border-accent"
          :title="t('files.editor.switchLanguage')"
        >
          <option v-for="option in LANGUAGES" :key="option.key" :value="option.key">
            {{ option.key === 'plain' ? t('files.language.plain') : option.label }}
          </option>
        </select>
        <button
          class="flex items-center gap-1.5 rounded-md bg-accent px-2.5 py-1 text-xs text-white transition-opacity hover:opacity-90 disabled:opacity-50"
          :disabled="editor.saving"
          @click="saveEditor"
        >
          <AppIcon name="save" :size="13" />
          {{ editor.saving ? t('files.editor.saving') : t('files.editor.save') }}
          <kbd class="ml-1 rounded bg-black/25 px-1 text-[11px]">Ctrl+S</kbd>
        </button>
      </header>

      <p
        v-if="editor.error"
        class="border-b border-red-500/30 bg-red-500/10 px-3 py-1.5 text-xs fg-danger"
      >
        {{ editor.error }}
      </p>

      <div class="relative min-h-0 flex-1">
        <div v-if="editorLoading" class="absolute inset-0 z-10 grid place-items-center bg-panel">
          <div class="w-2/3 space-y-2">
            <div v-for="i in 6" :key="i" class="skeleton h-3 rounded" :style="{ width: `${95 - i * 9}%` }" />
          </div>
        </div>
        <CodeEditor
          :content="editor.content"
          :language="editor.language"
          :dark="isDark"
          @change="onEditorChange"
          @save="saveEditor"
        />
      </div>
    </div>

    <!-- 中栏：只读预览（Markdown / CSV / PDF / 不支持格式） -->
    <FilePreviewPane
      v-else-if="preview"
      :key="preview.path"
      :entry="preview"
      @close="closePreview"
      @edit="editFromPreview"
    />

    <div v-else class="flex min-w-0 flex-1 flex-col">
      <!-- 挂载点入口（设计文档 3.2：极简无树，主区域平铺所有挂载点） -->
      <template v-if="!currentPath">
        <header class="flex h-11 shrink-0 items-center gap-2 border-b border-line bg-panel px-3">
          <span class="text-[14px] font-medium text-ink">{{ t('files.roots.title') }}</span>
          <span
            v-if="rootsImplicit"
            class="rounded bg-elevated px-1.5 py-0.5 text-[11px] text-muted"
            :title="t('files.roots.implicitHint')"
          >
            {{ t('files.roots.implicit') }}
          </span>
          <button
            class="ml-auto grid size-7 place-items-center rounded-md text-muted transition-colors hover:bg-elevated hover:text-ink"
            :title="t('files.roots.refresh')"
            @click="boot"
          >
            <AppIcon name="refresh" :size="15" />
          </button>
        </header>

        <div class="min-h-0 flex-1 overflow-y-auto p-4 md:p-6">
          <div
            v-if="loading"
            class="grid grid-cols-[repeat(auto-fill,minmax(150px,1fr))] gap-3 md:grid-cols-[repeat(auto-fill,minmax(200px,1fr))]"
          >
            <div v-for="i in 4" :key="i" class="skeleton h-24 rounded-xl" />
          </div>

          <div v-else-if="error" class="grid h-full place-items-center py-10 text-center">
            <div>
              <span class="mb-3 inline-grid size-12 place-items-center rounded-full bg-elevated text-muted">
                <AppIcon :name="error.status === 403 ? 'lock' : 'alert'" :size="22" />
              </span>
              <p class="text-sm font-medium text-ink">
                {{
                  error.status === 403 ? t('files.roots.forbidden') : t('files.roots.failed')
                }}
              </p>
              <p class="mt-1 max-w-sm text-xs text-muted">{{ error.message }}</p>
            </div>
          </div>

          <div v-else-if="!roots.length" class="grid h-full place-items-center py-10 text-center">
            <div>
              <span class="mb-3 inline-grid size-12 place-items-center rounded-full bg-elevated text-muted">
                <AppIcon name="folder" :size="22" />
              </span>
              <p class="text-sm text-ink">{{ t('files.roots.empty') }}</p>
              <p class="mt-1 text-xs text-muted">{{ t('files.roots.emptyHint') }}</p>
            </div>
          </div>

          <div
            v-else
            class="grid grid-cols-[repeat(auto-fill,minmax(150px,1fr))] gap-3 md:grid-cols-[repeat(auto-fill,minmax(200px,1fr))]"
          >
            <button
              v-for="root in roots"
              :key="root.path"
              class="glass-card flex items-start gap-3 rounded-xl p-4 text-left"
              :class="root.online ? '' : 'opacity-60'"
              :title="root.online ? t('files.roots.enter') : t('files.roots.unavailable')"
              :data-root="root.name"
              @click="enterRoot(root)"
            >
              <span class="grid size-10 shrink-0 place-items-center rounded-lg bg-accent-soft text-accent">
                <AppIcon name="folder" :size="18" />
              </span>
              <span class="min-w-0 flex-1">
                <span class="block truncate text-sm font-medium text-ink">{{ root.name }}</span>
                <span class="mt-0.5 flex items-center gap-1.5">
                  <span
                    v-if="root.readonly"
                    class="shrink-0 rounded bg-elevated px-1 py-px text-[11px] text-muted"
                    :title="t('files.readonly.tooltip')"
                  >
                    {{ t('files.readonly.badge') }}
                  </span>
                  <span class="truncate text-[12px] text-muted">
                    {{ root.online ? t('files.roots.browse') : t('files.roots.unavailable') }}
                  </span>
                </span>
              </span>
              <AppIcon name="arrowRight" :size="14" class="mt-1 shrink-0 text-muted" />
            </button>
          </div>
        </div>
      </template>

      <template v-else>
      <!-- 工具栏 -->
            <header class="flex h-11 shrink-0 items-center gap-2 border-b border-line bg-panel px-2 md:px-3">
        <button
          class="grid size-7 shrink-0 place-items-center rounded-md text-muted transition-colors hover:bg-elevated hover:text-ink"
          :title="t('files.toolbar.refresh')"
          data-testid="toolbar-refresh"
          @click="open(currentPath)"
        >
          <AppIcon name="refresh" :size="15" />
        </button>

        <!-- 新建（设计文档 3.2 工具栏 / 3.2.6.1） -->
        <div class="relative shrink-0">
          <button
            class="flex items-center gap-1 rounded-md border border-line px-2 py-1.5 text-xs text-ink transition-colors hover:border-accent/50 disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:border-line"
            :disabled="readonly"
            :title="readonly ? t('files.readonly.tooltip') : t('files.toolbar.new')"
            data-testid="new-entry"
            :aria-expanded="newMenuOpen"
            @click="newMenuOpen = !newMenuOpen"
          >
            <AppIcon name="plus" :size="13" />
            <span class="hidden sm:inline">{{ t('files.toolbar.new') }}</span>
          </button>
          <div v-if="newMenuOpen" class="fixed inset-0 z-40" @click="newMenuOpen = false" />
          <div
            v-if="newMenuOpen"
            class="absolute right-0 top-full z-50 mt-1 w-40 overflow-hidden rounded-lg border border-line bg-panel py-1 shadow-xl"
          >
            <button class="menu-item" @click="newMenuOpen = false; openCreateDialog('dir')">
              <AppIcon name="folder" :size="14" />
              {{ t('files.toolbar.newFolder') }}
            </button>
            <button class="menu-item" @click="newMenuOpen = false; openCreateDialog('file')">
              <AppIcon name="file" :size="14" />
              {{ t('files.toolbar.newFile') }}
            </button>
          </div>
        </div>

        <button
          class="flex shrink-0 items-center gap-1.5 rounded-md bg-accent px-2.5 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-40"
          :disabled="readonly"
          :title="readonly ? t('files.readonly.tooltip') : t('files.toolbar.uploadHint')"
          @click="pickFiles"
        >
          <AppIcon name="upload" :size="13" />
          <span class="hidden sm:inline">{{ t('files.toolbar.upload') }}</span>
        </button>
        <input ref="fileInput" type="file" multiple class="hidden" @change="onFilesPicked" />

        <nav class="flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto text-[13px]">
          <button
            class="shrink-0 rounded px-1.5 py-1 text-muted transition-colors hover:bg-elevated hover:text-ink"
            :title="t('files.toolbar.home')"
            @click="goHome"
          >
            <AppIcon name="home" :size="12" />
          </button>
          <AppIcon name="chevronRight" :size="11" class="shrink-0 text-muted opacity-50" />
          <template v-for="(crumb, index) in crumbs" :key="crumb.path">
            <AppIcon
              v-if="index > 0"
              name="chevronRight"
              :size="11"
              class="shrink-0 text-muted opacity-50"
            />
            <button
              class="max-w-[160px] shrink-0 truncate rounded px-1.5 py-1 transition-colors hover:bg-elevated"
              :class="index === crumbs.length - 1 ? 'text-ink' : 'text-muted'"
              :title="crumb.label"
              @click="open(crumb.path)"
            >
              {{ crumb.label }}
            </button>
          </template>
        </nav>

        <!-- 只读挂载点：全部变更入口置灰并带 Tooltip（设计文档 8.4 第 7 条） -->
        <span
          v-if="readonly"
          class="hidden shrink-0 items-center gap-1 rounded bg-elevated px-1.5 py-0.5 text-[11px] text-muted sm:flex"
          :title="t('files.readonly.tooltip')"
          data-testid="readonly-badge"
        >
          <AppIcon name="lock" :size="10" />
          {{ t('files.readonly.badge') }}
        </span>

        <div class="relative hidden w-40 shrink-0 sm:block md:w-56">
          <span class="absolute left-2 top-1/2 -translate-y-1/2 text-muted">
            <AppIcon name="search" :size="13" />
          </span>
          <input
            v-model="searchTerm"
            :placeholder="searching ? t('files.toolbar.searching') : t('files.toolbar.searchPlaceholder')"
            class="w-full rounded-full border border-line bg-elevated py-1 pl-7 pr-7 text-[12px] text-ink outline-none focus:border-accent"
            @input="scheduleSearch"
            @keydown.esc="clearSearch"
          />
          <button
            v-if="searchTerm"
            class="absolute right-1.5 top-1/2 -translate-y-1/2 text-muted hover:text-ink"
            :title="t('files.toolbar.clearSearch')"
            @click="clearSearch"
          >
            <AppIcon name="close" :size="12" />
          </button>
        </div>

        <!-- 排序（设计文档 v2.7.0 3.2.1：名称/修改时间/大小 + 升降序，状态持久化） -->
        <div class="relative shrink-0">
          <button
            class="flex shrink-0 items-center gap-1 rounded-md border border-line px-2 py-1.5 text-xs text-ink transition-colors hover:border-accent/50"
            :title="t('files.sort.label')"
            data-testid="sort-menu"
            :aria-expanded="sortMenuOpen"
            @click="sortMenuOpen = !sortMenuOpen"
          >
            <AppIcon name="sort" :size="13" />
            <span class="hidden md:inline">{{ sortLabel }}</span>
          </button>
          <div v-if="sortMenuOpen" class="fixed inset-0 z-40" @click="sortMenuOpen = false" />
          <div
            v-if="sortMenuOpen"
            class="absolute right-0 top-full z-50 mt-1 w-40 overflow-hidden rounded-lg border border-line bg-panel py-1 shadow-xl"
          >
            <button
              v-for="option in SORT_OPTIONS"
              :key="option.key"
              class="menu-item"
              :data-sort="option.key"
              @click="applySort(option.key)"
            >
              <AppIcon
                name="check"
                :size="14"
                :class="sortKey === option.key ? 'text-accent' : 'opacity-0'"
              />
              {{ t(`files.sort.${option.key}`) }}
              <span
                v-if="sortKey === option.key"
                class="ml-auto text-[11px] text-muted"
              >{{ sortDir === 'asc' ? '↑' : '↓' }}</span>
            </button>
          </div>
        </div>

        <button
          class="grid size-7 shrink-0 place-items-center rounded-md transition-colors"
          :class="showHidden ? 'bg-accent-soft text-accent' : 'text-muted hover:bg-elevated hover:text-ink'"
          :title="showHidden ? t('files.toolbar.hideHidden') : t('files.toolbar.showHidden')"
          data-testid="toggle-hidden"
          :aria-pressed="showHidden"
          @click="toggleHidden"
        >
          <AppIcon :name="showHidden ? 'eye' : 'eyeOff'" :size="15" />
        </button>

        <div class="flex shrink-0 items-center rounded-md border border-line p-0.5">
          <button
            class="grid size-6 place-items-center rounded transition-colors"
            :class="viewMode === 'grid' ? 'bg-accent-soft text-accent' : 'text-muted hover:text-ink'"
            :title="t('files.toolbar.grid')"
            @click="setView('grid')"
          >
            <AppIcon name="grid" :size="13" />
          </button>
          <button
            class="grid size-6 place-items-center rounded transition-colors"
            :class="viewMode === 'list' ? 'bg-accent-soft text-accent' : 'text-muted hover:text-ink'"
            :title="t('files.toolbar.list')"
            @click="setView('list')"
          >
            <AppIcon name="rows" :size="13" />
          </button>
        </div>
      </header>

      <!-- 内容 -->
      <div class="relative min-h-0 flex-1 overflow-y-auto" :class="selectMode ? 'pb-20' : ''">
        <!-- 单目录扫描上限 10000 条（设计文档 3.2.4） -->
        <p
          v-if="listing?.truncated"
          class="border-b border-amber-500/30 bg-amber-500/10 px-3 py-1.5 text-[12px] fg-warn"
        >
          {{ t('files.list.truncated') }}
        </p>

        <div v-if="loading" class="p-3">
          <div class="grid grid-cols-[repeat(auto-fill,minmax(104px,1fr))] gap-2">
            <div v-for="i in 12" :key="i" class="flex flex-col items-center gap-2 p-3">
              <div class="skeleton size-12 rounded-lg" />
              <div class="skeleton h-2.5 w-3/4 rounded" />
            </div>
          </div>
        </div>

        <div v-else-if="error" class="grid h-full place-items-center px-6 py-10 text-center">
          <div>
            <span class="mb-3 inline-grid size-12 place-items-center rounded-full bg-elevated text-muted">
              <AppIcon :name="error.status === 403 ? 'lock' : 'alert'" :size="22" />
            </span>
            <p class="text-sm font-medium text-ink">
              {{ error.status === 403 ? t('files.list.forbidden') : t('files.list.failed') }}
            </p>
            <p class="mt-1 max-w-sm text-xs text-muted">{{ error.message }}</p>
            <button
              class="mt-4 rounded-md border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
              @click="boot"
            >
              {{ t('files.list.backToRoots') }}
            </button>
          </div>
        </div>

        <div
          v-else-if="displayEntries.length === 0"
          class="grid h-full place-items-center px-6 py-10 text-center"
        >
          <div>
            <span class="mb-3 inline-grid size-12 place-items-center rounded-full bg-elevated text-muted">
              <AppIcon name="folderOpen" :size="22" />
            </span>
            <p class="text-sm text-ink">
              {{ isSearching ? t('files.list.noResults') : t('files.list.empty') }}
            </p>
            <p class="mt-1 text-xs text-muted">
              {{ isSearching ? t('files.list.noResultsHint') : t('files.list.emptyHint') }}
            </p>
          </div>
        </div>

        <!-- 网格视图（虚拟化：只渲染视口内单元格） -->
        <VirtualGrid
          v-else-if="viewMode === 'grid'"
          :items="displayEntries"
          :min-width="132"
          :gap="8"
          :ratio="1.42"
        >
          <template #default="{ item: entry }">
            <button
              type="button"
              class="entry-tile group relative flex size-full flex-col items-center gap-2 rounded-xl border border-transparent p-3 transition-colors hover:border-line hover:bg-panel"
              :class="[
                selected === entry.path ? 'border-accent/50 bg-panel' : '',
                selectMode && isSelected(entry.path) ? 'border-accent bg-accent-soft' : '',
              ]"
              :title="entry.name"
              :data-entry="entry.name"
              :data-kind="entry.kind"
              :data-selected="isSelected(entry.path) ? '1' : undefined"
              @click="onEntryClick(entry, $event)"
              @dblclick="activate(entry)"
              @contextmenu.prevent="openMenu($event, entry)"
              @touchstart.passive="onPressStart($event, entry)"
              @touchmove.passive="onPressMove($event)"
              @touchend="cancelPress"
              @touchcancel="cancelPress"
            >
              <span
                v-if="selectMode"
                class="absolute right-1 top-1 grid size-5 place-items-center rounded-full border transition-colors"
                :class="
                  isSelected(entry.path)
                    ? 'border-accent bg-accent text-white'
                    : 'border-line bg-panel text-transparent'
                "
              >
                <AppIcon name="check" :size="11" />
              </span>
              <!-- 缩略图随格宽放大（≥100x100，设计文档 v2.7.0 3.2.1 / 8.4.5） -->
              <span
                class="relative grid aspect-square w-full place-items-center overflow-hidden rounded-lg bg-elevated/60 transition-transform group-hover:scale-105"
                :class="toneFor(entry)"
              >
                <img
                  v-if="hasThumb(entry) && !thumbFailed.has(entry.path)"
                  :src="thumbUrl(entry.path, 320)"
                  class="size-full object-cover"
                  loading="lazy"
                  alt=""
                  @error="markThumbFailed(entry.path)"
                />
                <AppIcon v-else :name="iconFor(entry)" :size="34" />
                <!-- 视频封面之上叠播放角标，一眼区分「能看」和「只是文件」 -->
                <span
                  v-if="entry.kind === 'video' && !thumbFailed.has(entry.path)"
                  class="pointer-events-none absolute right-1.5 bottom-1.5 grid size-6 place-items-center rounded-full bg-black/55 text-white backdrop-blur"
                >
                  <AppIcon name="play" :size="11" />
                </span>
              </span>
              <span class="w-full truncate text-center text-[12px] leading-tight text-ink">
                {{ entry.name }}
              </span>
            </button>
          </template>
        </VirtualGrid>

        <!-- 列表视图（定高虚拟列表，设计文档 v2.7.0 3.2.1：表头行 + h-12 行高） -->
        <template v-else>
          <div
            class="sticky top-0 z-10 flex h-10 shrink-0 items-center gap-2 border-b border-line bg-panel px-3 text-[12px] font-medium text-muted"
          >
            <span class="min-w-0 flex-1">{{ t('files.header.name') }}</span>
            <span class="w-24 shrink-0 text-right">{{ t('files.header.size') }}</span>
            <span class="hidden w-36 shrink-0 text-right sm:block">{{
              t('files.header.time')
            }}</span>
          </div>
          <VirtualList :items="displayEntries" :row-height="48">
          <template #default="{ item: entry }">
            <div
              class="entry-tile flex h-full cursor-default items-center gap-2 border-t border-line/60 px-3 text-[13px] transition-colors hover:bg-elevated/60"
              :class="[
                selected === entry.path ? 'bg-accent-soft' : '',
                selectMode && isSelected(entry.path) ? 'bg-accent-soft' : '',
              ]"
              :data-entry="entry.name"
              :data-kind="entry.kind"
              :data-selected="isSelected(entry.path) ? '1' : undefined"
              @click="onEntryClick(entry, $event)"
              @dblclick="activate(entry)"
              @contextmenu.prevent="openMenu($event, entry)"
              @touchstart.passive="onPressStart($event, entry)"
              @touchmove.passive="onPressMove($event)"
              @touchend="cancelPress"
              @touchcancel="cancelPress"
            >
              <span
                v-if="selectMode"
                class="grid size-4 shrink-0 place-items-center rounded-full border transition-colors"
                :class="
                  isSelected(entry.path)
                    ? 'border-accent bg-accent text-white'
                    : 'border-line text-transparent'
                "
              >
                <AppIcon name="check" :size="10" />
              </span>
              <span :class="toneFor(entry)"><AppIcon :name="iconFor(entry)" :size="15" /></span>
              <span class="min-w-0 flex-1 truncate text-ink">{{ entry.name }}</span>
              <span class="w-24 shrink-0 text-right text-muted whitespace-nowrap">
                {{ entry.is_dir ? '—' : formatSize(entry.size) }}
              </span>
              <span class="hidden w-36 shrink-0 text-right text-muted whitespace-nowrap sm:block">
                {{ formatTime(entry.modified) }}
              </span>
            </div>
          </template>
          </VirtualList>
        </template>

        <!-- 无限滚动哨兵 + 加载骨架（8.4.5 第 4 条） -->
        <div v-if="hasMore && !loading && !error" ref="sentinelEl" class="h-px" />
        <div v-if="loadingMore" class="flex items-center justify-center gap-2 py-4">
          <span class="skeleton size-3.5 rounded-full" />
          <span class="skeleton h-2.5 w-24 rounded" />
        </div>
      </div>

      <!-- 底栏 / 分页 -->
      <footer
        v-if="!loading && !error"
        class="flex h-9 shrink-0 items-center justify-between border-t border-line bg-panel px-3 text-[12px] text-muted"
      >
        <span v-if="isSearching">
          {{ t('files.list.found', { count: displayEntries.length }) }}
          <span class="hidden sm:inline"> · {{ t('files.list.escToClear') }}</span>
        </span>
        <span v-else>
          {{ t('files.list.total', { count: total }) }}
          <span class="hidden sm:inline">
            · {{ t('files.list.shown', { shown: displayEntries.length }) }}
          </span>
        </span>
      </footer>

      <!-- 多选操作栏（设计文档 8.6 第 2 条：安全区 + 列表底部补等量 padding） -->
      <Transition name="fade">
        <div
          v-if="selectMode"
          class="absolute inset-x-0 bottom-0 z-30 border-t border-line bg-panel/95 backdrop-blur"
          style="padding-bottom: env(safe-area-inset-bottom)"
          data-testid="select-bar"
        >
          <div class="flex items-center gap-1.5 px-3 py-2">
            <span class="min-w-0 flex-1 truncate text-[12px] text-muted">
              {{ t('files.select.count', { count: selection.length }) }}
            </span>
            <button
              class="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] text-ink transition-colors hover:border-accent/50"
              @click="downloadSelected"
            >
              <AppIcon name="download" :size="12" />
              <span class="hidden sm:inline">{{ t('files.select.download') }}</span>
            </button>
            <button
              class="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] text-ink transition-colors hover:border-accent/50 disabled:cursor-not-allowed disabled:opacity-40"
              :disabled="readonly"
              :title="readonly ? t('files.readonly.tooltip') : ''"
              @click="openTransferDialog('move', selectedEntries)"
            >
              <AppIcon name="arrowRight" :size="12" />
              <span class="hidden sm:inline">{{ t('files.select.move') }}</span>
            </button>
            <button
              class="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] text-ink transition-colors hover:border-accent/50 disabled:cursor-not-allowed disabled:opacity-40"
              :disabled="readonly"
              :title="readonly ? t('files.readonly.tooltip') : ''"
              @click="openTransferDialog('copy', selectedEntries)"
            >
              <AppIcon name="gallery" :size="12" />
              <span class="hidden sm:inline">{{ t('files.select.copy') }}</span>
            </button>
            <button
              class="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] fg-danger transition-colors hover:border-red-400/50 disabled:cursor-not-allowed disabled:opacity-40"
              :disabled="readonly"
              :title="readonly ? t('files.readonly.tooltip') : ''"
              @click="openDeleteDialog(selectedEntries)"
            >
              <AppIcon name="trash" :size="12" />
              <span class="hidden sm:inline">{{ t('files.select.delete') }}</span>
            </button>
            <button
              class="rounded-md bg-elevated px-2 py-1 text-[12px] text-ink transition-colors hover:bg-line"
              @click="exitSelect"
            >
              {{ t('files.select.cancel') }}
            </button>
          </div>
        </div>
      </Transition>
      </template>
    </div>

    <!-- 右键菜单 -->
    <div v-if="menu" class="fixed inset-0 z-40" @click="menu = null" @contextmenu.prevent="menu = null" />
    <div
      v-if="menu"
      ref="menuEl"
      class="fixed z-50 max-h-[calc(100dvh-16px)] w-48 overflow-y-auto overflow-x-hidden rounded-lg border border-line bg-panel py-1 shadow-xl"
      :style="{ left: `${menu.x}px`, top: `${menu.y}px` }"
    >
      <button class="menu-item" @click="menuOpenEntry">
        <AppIcon :name="menu.entry.is_dir ? 'folderOpen' : 'file'" :size="14" />
        {{
          menu.entry.is_dir
            ? t('files.menu.openDir')
            : menu.entry.editable
              ? t('files.menu.edit')
              : t('files.menu.open')
        }}
      </button>
      <button v-if="menu.entry.is_dir" class="menu-item" @click="menuOpenTerminal">
        <AppIcon name="terminal" :size="14" />
        {{ t('files.menu.terminal') }}
      </button>
      <button class="menu-item" @click="menuCopyPath">
        <AppIcon name="fileText" :size="14" />
        {{ t('files.menu.copyPath') }}
      </button>
      <button v-if="!menu.entry.is_dir" class="menu-item" @click="menuDownload">
        <AppIcon name="archive" :size="14" />
        {{ t('files.menu.download') }}
      </button>
      <button class="menu-item" @click="menuShare">
        <AppIcon name="share" :size="14" />
        {{ t('files.menu.share') }}
      </button>
      <button
        v-if="menu.entry.kind === 'image' && !menu.entry.is_dir"
        class="menu-item"
        @click="menuDirectLink"
      >
        <AppIcon name="link" :size="14" />
        {{ t('files.menu.directLink') }}
      </button>
      <div class="my-1 border-t border-line" />
      <button
        class="menu-item"
        :class="readonly ? 'menu-item--off' : ''"
        :disabled="readonly"
        :title="readonly ? t('files.readonly.tooltip') : ''"
        @click="openRenameDialog"
      >
        <AppIcon name="edit" :size="14" />
        {{ t('files.menu.rename') }}
      </button>
      <button
        class="menu-item"
        :class="readonly ? 'menu-item--off' : ''"
        :disabled="readonly"
        :title="readonly ? t('files.readonly.tooltip') : ''"
        @click="openTransferDialog('move')"
      >
        <AppIcon name="arrowRight" :size="14" />
        {{ t('files.menu.moveTo') }}
      </button>
      <button
        class="menu-item"
        :class="readonly ? 'menu-item--off' : ''"
        :disabled="readonly"
        :title="readonly ? t('files.readonly.tooltip') : ''"
        @click="openTransferDialog('copy')"
      >
        <AppIcon name="gallery" :size="14" />
        {{ t('files.menu.copyTo') }}
      </button>
      <div class="my-1 border-t border-line" />
      <button class="menu-item" @click="openStatDialog">
        <AppIcon name="chip" :size="14" />
        {{ t('files.menu.properties') }}
      </button>
      <button
        class="menu-item menu-item--danger"
        :class="readonly ? 'menu-item--off' : ''"
        :disabled="readonly"
        :title="readonly ? t('files.readonly.tooltip') : ''"
        @click="openDeleteDialog()"
      >
        <AppIcon name="trash" :size="14" />
        {{ t('files.menu.delete') }}
      </button>
      <!-- 置灰项在部分浏览器上不弹原生 Tooltip，这里再给一条常驻说明 -->
      <p v-if="readonly" class="px-3 pt-1 pb-1.5 text-[11px] leading-snug text-muted">
        {{ t('files.readonly.tooltip') }}
      </p>
    </div>

    <!-- 删除确认 -->
    <div
      v-if="deleteTargets.length"
      class="fixed inset-0 z-50 grid place-items-center bg-black/50 p-4"
      @click.self="deleteTargets = []"
    >
      <div class="w-full max-w-sm rounded-2xl border border-line bg-panel p-5 shadow-xl">
        <p class="text-sm font-medium text-ink">{{ deleteTitle }}</p>
        <p
          v-if="deleteTargets.length === 1"
          class="mt-2 break-all text-[12px] text-muted"
        >
          {{ deleteTargets[0].name }}
        </p>
        <ul v-else class="mt-2 max-h-32 space-y-0.5 overflow-y-auto">
          <li
            v-for="entry in deleteTargets"
            :key="entry.path"
            class="truncate text-[12px] text-muted"
          >
            {{ entry.name }}
          </li>
        </ul>

        <!-- 删除前统计（设计文档 3.2.6.5） -->
        <p class="mt-2 text-[12px] text-muted">
          <template v-if="!deleteSurvey">{{ t('files.delete.measuring') }}</template>
          <template v-else-if="deleteSurvey.truncated">
            {{ t('files.delete.summaryMore', { count: deleteSurvey.items }) }}
          </template>
          <template v-else>
            {{
              t('files.delete.summary', {
                count: deleteSurvey.items,
                size: formatSize(deleteSurvey.bytes),
              })
            }}
          </template>
        </p>
        <p
          v-if="deleteSurvey?.truncated || (deleteSurvey?.items ?? 0) > 1000"
          class="mt-1 text-[12px] fg-warn"
        >
          {{ t('files.delete.large') }}
        </p>

        <p class="mt-3 text-xs leading-relaxed fg-danger">
          {{
            t(
              deleteNeedsName
                ? 'files.delete.warningDir'
                : deleteTargets.length > 1
                  ? 'files.delete.warningDir'
                  : 'files.delete.warningFile',
            )
          }}
        </p>
        <!-- 删除单个目录必须输入目录名（设计文档 3.2.6.5：防误触，不是安全边界） -->
        <label v-if="deleteNeedsName" class="mt-3 block text-[12px] text-muted">
          {{ t('files.delete.confirmNameLabel') }}
          <input
            v-model="deleteConfirmName"
            type="text"
            :placeholder="deleteTargets[0].name"
            class="mt-1 w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
            data-testid="delete-confirm-name"
            @keydown.enter="confirmDelete"
          />
          <span class="mt-1 block text-[11px] text-muted">{{ t('files.delete.confirmNameHint') }}</span>
        </label>
        <p v-if="deleteError" class="mt-3 text-xs fg-danger">{{ deleteError }}</p>
        <div class="mt-5 flex justify-end gap-2">
          <button
            class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
            @click="deleteTargets = []"
          >
            {{ t('common.action.cancel') }}
          </button>
          <button
            class="rounded-lg bg-red-500 px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50"
            :disabled="deleteBusy || !deleteNameMatches"
            @click="confirmDelete"
          >
            {{ deleteBusy ? t('common.action.deleting') : t('files.delete.confirm') }}
          </button>
        </div>
      </div>
    </div>

    <!-- 多选下载确认（设计文档 3.2.6.6：> 10 个先提示会被浏览器拦截） -->
    <div
      v-if="bulkDownloadConfirm"
      class="fixed inset-0 z-50 grid place-items-center bg-black/50 p-4"
      @click.self="bulkDownloadConfirm = null"
    >
      <div class="w-full max-w-sm rounded-2xl border border-line bg-panel p-5 shadow-xl">
        <p class="text-sm font-medium text-ink">
          {{ t('files.select.downloadConfirm', { count: bulkDownloadConfirm.length }) }}
        </p>
        <p class="mt-2 text-xs leading-relaxed text-muted">{{ t('files.select.downloadHint') }}</p>
        <div class="mt-5 flex justify-end gap-2">
          <button
            class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
            @click="bulkDownloadConfirm = null"
          >
            {{ t('common.action.cancel') }}
          </button>
          <button
            class="rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90"
            @click="confirmBulkDownload"
          >
            {{ t('files.select.download') }}
          </button>
        </div>
      </div>
    </div>

    <!-- 新建文件夹 / 空文件（设计文档 3.2.6.1） -->
    <div
      v-if="createDialog"
      class="fixed inset-0 z-50 grid place-items-center bg-black/50 p-4"
      @click.self="createDialog = null"
    >
      <div class="w-full max-w-sm rounded-2xl border border-line bg-panel p-5 shadow-xl">
        <p class="text-sm font-medium text-ink">
          {{
            t(
              createDialog.kind === 'dir'
                ? 'files.create.titleFolder'
                : 'files.create.titleFile',
            )
          }}
        </p>
        <p class="mt-0.5 truncate text-[12px] text-muted">{{ currentPath }}</p>
        <label class="mt-4 block text-[12px] text-muted">
          {{ t('files.create.placeholder') }}
          <input
            v-model="createDialog.name"
            type="text"
            :placeholder="t('files.create.placeholder')"
            class="mt-1 w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
            data-testid="create-name"
            @keydown.enter="submitCreate"
          />
        </label>
        <p v-if="createDialog.error" class="mt-3 text-xs fg-danger">{{ createDialog.error }}</p>
        <div class="mt-5 flex justify-end gap-2">
          <button
            class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
            @click="createDialog = null"
          >
            {{ t('common.action.cancel') }}
          </button>
          <button
            class="rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50"
            :disabled="createDialog.busy"
            @click="submitCreate"
          >
            {{ t('files.create.submit') }}
          </button>
        </div>
      </div>
    </div>

    <!-- 重命名（设计文档 3.2.6.1） -->
    <div
      v-if="renameDialog"
      class="fixed inset-0 z-50 grid place-items-center bg-black/50 p-4"
      @click.self="renameDialog = null"
    >
      <div class="w-full max-w-sm rounded-2xl border border-line bg-panel p-5 shadow-xl">
        <p class="text-sm font-medium text-ink">{{ t('files.rename.title') }}</p>
        <p class="mt-0.5 break-all text-[12px] text-muted">{{ renameDialog.entry.name }}</p>
        <label class="mt-4 block text-[12px] text-muted">
          {{ t('files.rename.label') }}
          <input
            v-model="renameDialog.name"
            type="text"
            class="mt-1 w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
            data-testid="rename-name"
            @keydown.enter="submitRename"
          />
        </label>
        <p v-if="renameDialog.error" class="mt-3 text-xs fg-danger">{{ renameDialog.error }}</p>
        <div class="mt-5 flex justify-end gap-2">
          <button
            class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
            @click="renameDialog = null"
          >
            {{ t('common.action.cancel') }}
          </button>
          <button
            class="rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50"
            :disabled="renameDialog.busy"
            @click="submitRename"
          >
            {{ t('files.rename.submit') }}
          </button>
        </div>
      </div>
    </div>

    <!-- 移动 / 复制（设计文档 3.2.6.2：冲突必须由用户解析成具体策略） -->
    <div
      v-if="transferDialog"
      class="fixed inset-0 z-50 grid place-items-center bg-black/50 p-4"
      @click.self="transferDialog = null"
    >
      <div class="w-full max-w-sm rounded-2xl border border-line bg-panel p-5 shadow-xl">
        <p class="text-sm font-medium text-ink">
          {{
            t(
              transferDialog.mode === 'move'
                ? 'files.transfer.titleMove'
                : 'files.transfer.titleCopy',
            )
          }}
        </p>
        <p class="mt-0.5 text-[12px] text-muted">
          {{
            transferDialog.targets.length === 1
              ? transferDialog.targets[0].path
              : t('files.select.count', { count: transferDialog.targets.length })
          }}
        </p>

        <label class="mt-4 block text-[12px] text-muted">
          {{ t('files.transfer.destination') }}
          <input
            v-model="transferDialog.dest"
            type="text"
            class="mt-1 w-full rounded-lg border border-line bg-elevated px-3 py-2 text-[12px] text-ink outline-none focus:border-accent"
            data-testid="transfer-dest"
            @keydown.enter="submitTransfer"
          />
          <span class="mt-1 block text-[11px] text-muted">
            {{ t('files.transfer.destinationHint') }}
          </span>
        </label>

        <p class="mt-4 text-[12px] text-muted">{{ t('files.transfer.conflict') }}</p>
        <div class="mt-1.5 space-y-1">
          <label
            v-for="option in CONFLICT_OPTIONS"
            :key="option"
            class="flex cursor-pointer items-center gap-2 rounded-lg border border-line px-3 py-1.5 text-xs text-ink"
            :class="conflictChoice === option ? 'border-accent/60 bg-accent-soft' : ''"
          >
            <input
              v-model="conflictChoice"
              type="radio"
              :value="option"
              class="accent-[var(--accent)]"
              :data-testid="`conflict-${option}`"
            />
            {{
              t(
                option === 'overwrite'
                  ? 'files.transfer.overwrite'
                  : option === 'rename'
                    ? 'files.transfer.renamePolicy'
                    : 'files.transfer.skip',
              )
            }}
          </label>
        </div>

        <p v-if="transferDialog.error" class="mt-3 text-xs fg-danger">
          {{ transferDialog.error }}
        </p>
        <div class="mt-5 flex justify-end gap-2">
          <button
            class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
            @click="transferDialog = null"
          >
            {{ t('common.action.cancel') }}
          </button>
          <button
            class="rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50"
            :disabled="transferDialog.busy"
            @click="submitTransfer"
          >
            {{ transferDialog.busy ? t('common.action.loading') : t('files.transfer.submit') }}
          </button>
        </div>
      </div>
    </div>

    <!-- 批量操作进行中：同步请求，只给不确定进度 + 遮罩，不伪造百分比（设计文档 8.5 第 2 条） -->
    <div
      v-if="transferDialog?.busy"
      class="fixed inset-0 z-[60] grid place-items-center bg-black/60 p-4"
    >
      <div class="rounded-xl border border-line bg-panel px-6 py-5 text-center shadow-xl">
        <div
          class="mx-auto size-6 animate-spin rounded-full border-2 border-line border-t-accent"
        />
        <p class="mt-3 text-xs text-ink">
          {{ t('files.transfer.busy', { count: transferDialog.targets.length }) }}
        </p>
      </div>
    </div>

    <!-- 批量结果汇总（设计文档 6.2：逐项 error_code 翻译，不甩英文 message） -->
    <div
      v-if="resultDialog"
      class="fixed inset-0 z-50 grid place-items-center bg-black/50 p-4"
      @click.self="resultDialog = null"
    >
      <div class="w-full max-w-sm rounded-2xl border border-line bg-panel p-5 shadow-xl">
        <p class="text-sm font-medium text-ink">{{ t('files.result.title') }}</p>
        <p class="mt-2 text-xs text-muted">
          {{
            t('files.result.summary', {
              ok: resultDialog.ok,
              failed: resultDialog.failed,
              skipped: resultDialog.skipped,
            })
          }}
        </p>
        <template v-if="resultDialog.failures.length">
          <p class="mt-4 text-[12px] font-medium text-muted">{{ t('files.result.failures') }}</p>
          <ul class="mt-1.5 max-h-48 space-y-1 overflow-y-auto">
            <li
              v-for="item in resultDialog.failures"
              :key="item.path"
              class="rounded-lg border border-line bg-elevated px-2.5 py-1.5"
            >
              <p class="break-all text-[12px] text-ink">{{ item.source ?? item.path }}</p>
              <p class="mt-0.5 text-[11px] fg-danger">{{ failureText(item) }}</p>
            </li>
          </ul>
        </template>
        <div class="mt-5 flex justify-end">
          <button
            class="rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90"
            @click="resultDialog = null"
          >
            {{ t('common.action.close') }}
          </button>
        </div>
      </div>
    </div>

    <!-- 属性（设计文档 3.2 右键菜单 / 6.2 stat） -->
    <div
      v-if="statDialog"
      class="fixed inset-0 z-50 grid place-items-center bg-black/50 p-4"
      @click.self="statDialog = null"
    >
      <div class="w-full max-w-md rounded-2xl border border-line bg-panel p-5 shadow-xl">
        <p class="text-sm font-medium text-ink">{{ t('files.stat.title') }}</p>
        <!-- 只显示文件名：完整路径（含盘符）不给属性页（v2.9） -->
        <p class="mt-0.5 truncate text-[12px] text-muted">{{ statDialog.entry.name }}</p>

        <p v-if="statDialog.busy" class="mt-4 text-xs text-muted">
          {{ t('common.action.loading') }}
        </p>
        <p v-else-if="statDialog.error" class="mt-4 text-xs fg-danger">{{ statDialog.error }}</p>
        <dl v-else-if="statDialog.data" class="mt-4 space-y-1.5 text-[12px]">
          <div class="flex gap-3">
            <dt class="w-20 shrink-0 text-muted">{{ t('files.stat.size') }}</dt>
            <dd class="min-w-0 flex-1 break-all text-ink">
              <template v-if="statDialog.data.is_dir">—</template>
              <template v-else>
                {{ formatSize(statDialog.data.size) }}
                <!-- 精确字节数用浅色小字附带，方便对账 / 校验哈希（v2.9） -->
                <span class="ml-1 text-[11px] text-muted">
                  {{ t('files.stat.bytes', { count: statDialog.data.size.toLocaleString() }) }}
                </span>
              </template>
            </dd>
          </div>
          <!-- 分辨率只有媒体才有，其余类型不占位（v2.9） -->
          <div
            v-if="statDialog.data.width && statDialog.data.height"
            class="flex gap-3"
          >
            <dt class="w-20 shrink-0 text-muted">{{ t('files.stat.resolution') }}</dt>
            <dd class="min-w-0 flex-1 text-ink">
              {{ statDialog.data.width }} × {{ statDialog.data.height }}
            </dd>
          </div>
          <div class="flex gap-3">
            <dt class="w-20 shrink-0 text-muted">{{ t('files.stat.mime') }}</dt>
            <dd class="min-w-0 flex-1 break-all text-ink">{{ statDialog.data.mime }}</dd>
          </div>
          <div class="flex gap-3">
            <dt class="w-20 shrink-0 text-muted">{{ t('files.stat.modified') }}</dt>
            <dd class="min-w-0 flex-1 text-ink">{{ formatTime(statDialog.data.mtime) }}</dd>
          </div>
          <!-- Windows 拿不到权限/所有者/inode：整行隐藏，绝不画「—」占位（v2.9） -->
          <div
            v-if="statDialog.data.mode !== null && statDialog.data.mode !== undefined"
            class="flex gap-3"
          >
            <dt class="w-20 shrink-0 text-muted">{{ t('files.stat.mode') }}</dt>
            <dd class="min-w-0 flex-1 text-ink">{{ formatMode(statDialog.data.mode) }}</dd>
          </div>
          <div v-if="statDialog.data.owner" class="flex gap-3">
            <dt class="w-20 shrink-0 text-muted">{{ t('files.stat.owner') }}</dt>
            <dd class="min-w-0 flex-1 break-all text-ink">{{ statDialog.data.owner }}</dd>
          </div>
          <div
            v-if="statDialog.data.inode !== null && statDialog.data.inode !== undefined"
            class="flex gap-3"
          >
            <dt class="w-20 shrink-0 text-muted">{{ t('files.stat.inode') }}</dt>
            <dd class="min-w-0 flex-1 text-ink">{{ statDialog.data.inode }}</dd>
          </div>
          <div class="flex gap-3">
            <dt class="w-20 shrink-0 text-muted">{{ t('files.stat.directory') }}</dt>
            <dd class="min-w-0 flex-1 break-all text-ink" :title="statRelativeDir">
              {{ statRelativeDir }}
            </dd>
          </div>
          <div v-if="statDialog.data.is_symlink" class="flex gap-3">
            <dt class="w-20 shrink-0 text-muted">{{ t('files.stat.symlink') }}</dt>
            <dd class="min-w-0 flex-1 break-all fg-warn">
              {{ statDialog.data.symlink_target ?? '—' }}
            </dd>
          </div>
          <div v-if="statDialog.data.readonly" class="flex gap-3">
            <dt class="w-20 shrink-0 text-muted">{{ t('files.stat.readonly') }}</dt>
            <dd class="min-w-0 flex-1 text-ink">{{ t('files.readonly.badge') }}</dd>
          </div>
        </dl>

        <div class="mt-5 flex justify-end">
          <button
            class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
            @click="statDialog = null"
          >
            {{ t('common.action.close') }}
          </button>
        </div>
      </div>
    </div>

    <!-- 分享弹窗 -->
    <div v-if="shareDialog" class="fixed inset-0 z-50 grid place-items-center bg-black/50 p-4" @click.self="shareDialog = null">
      <div class="w-full max-w-sm rounded-2xl border border-line bg-panel p-5 shadow-xl">
        <template v-if="!shareResult">
          <p class="text-sm font-medium text-ink">{{ t('files.share.create') }}</p>
          <p class="mt-0.5 truncate text-[12px] text-muted">
            {{ shareDialog.entry.name }}
          </p>

          <label class="mt-4 block text-[12px] text-muted">
            {{ t('files.share.password') }}
            <input
              v-model="shareForm.password"
              type="text"
              :placeholder="t('files.share.passwordPlaceholder')"
              class="mt-1 w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
            />
          </label>

          <label class="mt-3 block text-[12px] text-muted">
            {{ t('files.share.expiry') }}
            <select
              v-model.number="shareForm.days"
              class="mt-1 w-full rounded-lg border border-line bg-elevated px-2 py-2 text-xs text-ink outline-none focus:border-accent"
            >
              <option :value="1">{{ t('files.share.days1') }}</option>
              <option :value="7">{{ t('files.share.days7') }}</option>
              <option :value="30">{{ t('files.share.days30') }}</option>
              <option :value="0">{{ t('files.share.permanent') }}</option>
            </select>
          </label>

          <label class="mt-3 flex items-center gap-2 text-xs text-ink">
            <input v-model="shareForm.allowDownload" type="checkbox" class="accent-[var(--accent)]" />
            {{ t('files.share.allowDownload') }}
          </label>

          <p v-if="shareError" class="mt-3 text-xs fg-danger">{{ shareError }}</p>

          <div class="mt-5 flex justify-end gap-2">
            <button
              class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
              @click="shareDialog = null"
            >
              {{ t('common.action.cancel') }}
            </button>
            <button
              class="rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50"
              :disabled="shareBusy"
              @click="createShare"
            >
              {{ shareBusy ? t('common.action.creating') : t('files.share.submit') }}
            </button>
          </div>
        </template>

        <template v-else>
          <p class="text-sm font-medium text-ink">{{ t('files.share.created') }}</p>
          <div class="mt-3 break-all rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink">
            {{ shareResult.url }}
          </div>
          <p class="mt-2 text-[12px] text-muted">
            {{ t('files.share.hint') }}
          </p>
          <div class="mt-4 flex justify-end gap-2">
            <button
              class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
              @click="shareDialog = null"
            >
              {{ t('common.action.close') }}
            </button>
            <button
              class="rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90"
              @click="copyShareResult"
            >
              {{ t('files.share.copyLink') }}
            </button>
          </div>
        </template>
      </div>
    </div>

    <!-- 拖拽上传遮罩 -->
    <div
      v-if="dragging && currentPath"
      class="pointer-events-none absolute inset-0 z-30 grid place-items-center bg-accent/10 backdrop-blur-sm"
    >
      <div class="rounded-2xl border-2 border-dashed border-accent/60 bg-panel/80 px-10 py-8 text-center">
        <p class="text-sm font-medium text-ink">{{ t('files.upload.dropTitle') }}</p>
        <p class="mt-1 max-w-xs truncate text-[12px] text-muted">{{ currentPath }}</p>
      </div>
    </div>

    <!-- 上传进度 -->
    <div
      v-if="uploads.length"
      class="absolute right-3 bottom-12 z-40 w-64 space-y-2 rounded-xl border border-line bg-panel p-3 shadow-xl"
    >
      <p class="text-[12px] font-medium text-muted">
        {{ t('files.upload.queue', { count: uploads.length }) }}
      </p>
      <div v-for="task in uploads" :key="task.id" class="space-y-1">
        <div class="flex items-center gap-2 text-[12px]">
          <span class="min-w-0 flex-1 truncate text-ink">{{ task.name }}</span>
          <span
            :class="task.status === 'error' ? 'fg-danger' : 'text-muted'"
            :title="task.error ?? ''"
          >
            {{
              task.status === 'error'
                ? t('files.upload.failed')
                : task.status === 'done'
                  ? t('files.upload.done')
                  : `${task.progress}%`
            }}
          </span>
        </div>
        <div class="h-1 overflow-hidden rounded-full bg-elevated">
          <div
            class="h-full rounded-full transition-all"
            :class="task.status === 'error' ? 'bg-red-400' : 'bg-accent'"
            :style="{ width: `${task.status === 'error' ? 100 : task.progress}%` }"
          />
        </div>
      </div>
    </div>

    <!-- 轻提示 -->
    <Transition name="fade">
      <div
        v-if="toast"
        class="pointer-events-none fixed bottom-6 left-1/2 z-50 -translate-x-1/2 rounded-full border border-line bg-panel px-4 py-2 text-xs text-ink shadow-lg"
      >
        {{ toast }}
      </div>
    </Transition>
  </section>
</template>

<style scoped>
/* 移动端长按选中文件时，禁止系统自带的文本选择与长按菜单（设计文档 8.6 第 1 条） */
.entry-tile {
  user-select: none;
  -webkit-user-select: none;
  -webkit-touch-callout: none;
}
.menu-item {
  display: flex;
  width: 100%;
  align-items: center;
  gap: 8px;
  padding: 8px 12px;
  font-size: 13px;
  color: var(--text-main);
  transition: background-color 0.15s ease;
}
.menu-item:hover {
  background: var(--bg-elevated);
}
.menu-item--danger {
  color: var(--tone-danger);
}
.menu-item--danger:hover {
  background: rgb(248 113 113 / 0.12);
}
/* 只读挂载点：置灰但仍可 hover 出 Tooltip（设计文档 8.4 第 7 条） */
.menu-item--off,
.menu-item--off:hover {
  cursor: not-allowed;
  background: transparent;
  opacity: 0.4;
}
.menu-item--off.menu-item--danger,
.menu-item--off.menu-item--danger:hover {
  color: var(--tone-danger);
}
</style>
