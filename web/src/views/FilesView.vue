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
  ref,
  watch,
} from 'vue'
import { useI18n } from 'vue-i18n'
import { useMediaQuery } from '@vueuse/core'
import { onBeforeRouteLeave, useRoute, useRouter } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import ContextMenu, { type ContextMenuItem } from '../components/ContextMenu.vue'
import FileOpsDialogs, { type FileOpDone } from '../components/FileOpsDialogs.vue'
import FilePreviewPane from '../components/FilePreviewPane.vue'
import VirtualGrid from '../components/VirtualGrid.vue'
import VirtualList from '../components/VirtualList.vue'
import { LANGUAGES, detectLanguage, type LanguageKey } from '../api/languages'
import {
  ApiError,
  filesApi,
  summarizeResults,
  tusApi,
  type EntryInfo,
  type EolType,
  type Listing,
  type MountPoint,
  type SearchHit,
  type TextEncoding,
} from '../api/files'
import { messageFromBody } from '../lib/errors'
import { triggerDownload } from '../lib/download'
import { sharesApi } from '../api/shares'
import { THUMB_WIDTH, thumbUrl } from '../api/media'
import { formatSize, formatTime } from '../lib/format'
import { playerClearance } from '../lib/player-layout'
import { currentTrack, player } from '../stores/player'
import { useTheme } from '../composables/useTheme'
import { useScrollLock } from '../composables/useScrollLock'
import { useBackClose } from '../composables/useBackClose'
import { useKeyboardInset } from '../composables/useKeyboardInset'
import { useDeferredLoading } from '../composables/useDeferredLoading'
import { readCache, writeCache } from '../lib/view-cache'
import { openLightbox, type LightboxItem } from '../stores/lightbox'
import { openShare, shareTarget } from '../stores/share'

// CodeMirror 内核与语言包都在这个异步 chunk 里，首次打开编辑器才下载
const CodeEditor = defineAsyncComponent(() => import('../components/CodeEditor.vue'))

const { isDark } = useTheme()
const { t } = useI18n()
const route = useRoute()
const router = useRouter()

/** 右键菜单目标（定位与视口夹取由全局 ContextMenu 组件负责） */
const menu = ref<{ x: number; y: number; entry: EntryInfo } | null>(null)

/**
 * 变更类弹窗（重命名 / 移动复制 / 删除 / 属性）：**文件管理与媒体库共用同一个组件**。
 * 这里只负责把「点的是谁」翻译成组件的方法调用，操作完成后由 `onOpsDone` 刷新列表。
 */
const ops = ref<InstanceType<typeof FileOpsDialogs> | null>(null)

function openMenu(event: MouseEvent, entry: EntryInfo) {
  menu.value = { x: event.clientX, y: event.clientY, entry }
  selected.value = entry.path
}

/**
 * 空白处右键菜单（设计文档 3.2.5）：作用对象是**当前目录**，不是某个条目。
 *
 * 「在哪儿点就往哪儿建」——新建/上传/粘贴/开终端放在这里比回工具栏找按钮自然得多。
 * 条目自己的右键由 `openMenu` 收，两边靠 `[data-entry]` 区分：条目的 handler 没有
 * `stopPropagation`，冒泡到容器时这一下必须认出来并放行，否则会弹出两层菜单。
 */
const blankMenu = ref<{ x: number; y: number } | null>(null)

function openBlankMenu(event: MouseEvent) {
  const el = event.target as HTMLElement | null
  if (el?.closest('[data-entry]')) return
  // 不清高亮会让人以为「接下来操作的是那一行」
  selected.value = null
  blankMenu.value = { x: event.clientX, y: event.clientY }
}

/** 手机端底部面板的标题：说清「这些新东西建在哪个目录里」 */
const blankTitle = computed(() => {
  const path = currentPath.value.replace(/\\/g, '/').replace(/\/+$/, '')
  if (!path) return undefined
  // 挂载点根用配置里的名字（用户认的是「Media」，不是路径末段的 media）
  const mount = mountForPath(path)
  if (mount && mount.path.replace(/\\/g, '/').replace(/\/+$/, '') === path) return mount.name
  return path.split('/').pop() || path
})

/**
 * 跳到系统终端并带上目录（条目右键与空白右键共用）。
 *
 * 顺序很关键：**先导航、后关菜单**。关菜单会让 useBackClose 的 `disarm()` 用 `history.back()`
 * 退掉浮层哨兵；若此时目标页还在加载（终端是懒加载 chunk），这条 back 会把 in-flight 的
 * 导航取消掉（vue-router 回 `NavigationFailure=cancelled`），表现就是「点了没反应、URL 原地不动」。
 * 等导航落地再关：那时历史里已不是哨兵记录，`disarm()` 会自己放弃退栈。
 *
 * 失败也不能静默：`void router.push()` 的 rejection 谁都不看，这里兜一条 toast 把原因说出来。
 */
function openTerminalAt(path: string) {
  void router
    .push({ path: '/terminal', query: { path } })
    .catch((err: unknown) => showToast(err instanceof Error ? err.message : String(err)))
    .finally(() => {
      menu.value = null
      blankMenu.value = null
    })
}

function openTerminal(entry: EntryInfo) {
  openTerminalAt(entry.is_dir ? entry.path : entry.path.replace(/\/[^/]*$/, ''))
}

/** 下载一个条目：目录走流式 zip，文件走 `?download=1`（服务端补 attachment） */
function downloadEntry(entry: EntryInfo) {
  menu.value = null
  const url = entry.is_dir
    ? filesApi.archiveUrl(entry.path)
    : filesApi.rawUrl(entry.path, { download: true })
  triggerDownload(url)
}

function menuOpenEntry() {
  const entry = menu.value?.entry
  menu.value = null
  if (entry) void activate(entry)
}

/** 关菜单的动作由 openTerminalAt 在导航落地后统一做（见那里的注释） */
function menuOpenTerminal() {
  const entry = menu.value?.entry
  if (entry) openTerminal(entry)
}

function menuDownload() {
  const entry = menu.value?.entry
  if (entry) downloadEntry(entry)
}

// ---------------- 分享 / 图床直链（设计文档 3.1 / 3.2） ----------------
// 分享表单已抽成全局单例（components/ShareDialog.vue），这里只负责唤起。

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
  if (entry) {
    menu.value = null
    openShare(entry.path, entry.name)
  }
}

function menuDirectLink() {
  const entry = menu.value?.entry
  if (entry) void copyDirectLink(entry)
}

/** 右键菜单条目（设计文档 3.2.5）：只读挂载点的变更项统一置灰并带原因 */
const fileMenuItems = computed<ContextMenuItem[]>(() => {
  const entry = menu.value?.entry
  if (!entry) return []
  const hint = t('files.readonly.tooltip')
  const items: ContextMenuItem[] = [
    {
      id: 'open',
      label: entry.is_dir
        ? t('files.menu.openDir')
        : entry.editable
          ? t('files.menu.edit')
          : t('files.menu.open'),
      icon: entry.is_dir ? 'folderOpen' : 'file',
    },
  ]
  // 极客联动（设计文档 3.2.5）：「在此终端中打开」紧跟「打开」——要路径时直接进终端，
  // 比复制一串文本有用（v4.4.9 起移除了「复制相对/绝对路径」，只留图床直链）。
  items.push({ id: 'terminal', label: t('files.menu.terminal'), icon: 'terminal' })
  // 目录也能下载：后端打包成流式 zip（v4.4.9）
  items.push({ id: 'download', label: t('files.menu.download'), icon: 'archive' })
  items.push({ id: 'share', label: t('files.menu.share'), icon: 'share' })
  if (entry.kind === 'image' && !entry.is_dir) {
    items.push({ id: 'directLink', label: t('files.menu.directLink'), icon: 'link' })
  }
  items.push(
    {
      id: 'rename',
      label: t('files.menu.rename'),
      icon: 'edit',
      divider: true,
      disabled: readonly.value,
      hint,
    },
    {
      id: 'move',
      label: t('files.menu.moveTo'),
      icon: 'arrowRight',
      disabled: readonly.value,
      hint,
    },
    {
      id: 'copy',
      label: t('files.menu.copyTo'),
      icon: 'gallery',
      disabled: readonly.value,
      hint,
    },
    { id: 'properties', label: t('files.menu.properties'), icon: 'chip', divider: true },
    {
      id: 'delete',
      label: t('files.menu.delete'),
      icon: 'trash',
      danger: true,
      disabled: readonly.value,
      hint,
    },
  )
  return items
})

function onMenuSelect(id: string) {
  switch (id) {
    case 'open':
      menuOpenEntry()
      break
    case 'terminal':
      menuOpenTerminal()
      break
    case 'download':
      menuDownload()
      break
    case 'share':
      menuShare()
      break
    case 'directLink':
      menuDirectLink()
      break
    case 'rename':
      openRenameDialog()
      break
    case 'move':
      openTransferDialog('move')
      break
    case 'copy':
      openTransferDialog('copy')
      break
    case 'properties':
      openStatDialog()
      break
    case 'delete':
      openDeleteDialog()
      break
    default:
      menu.value = null
  }
}

/**
 * 空白处（当前目录）的右键菜单。
 *
 * 只读挂载点：新建/上传/粘贴一律置灰并带原因——和工具栏同一条红线，不靠后端 403 兜。
 * 粘贴只在内部剪贴板非空时出现（没东西可粘就别摆一个永远点不动的项）。
 */
const blankMenuItems = computed<ContextMenuItem[]>(() => {
  if (!blankMenu.value) return []
  const hint = t('files.readonly.tooltip')
  const clip = clipboard.value
  const items: ContextMenuItem[] = [
    {
      id: 'newDir',
      label: t('files.blankMenu.newDir'),
      icon: 'folder',
      disabled: readonly.value,
      hint,
    },
    {
      id: 'newFile',
      label: t('files.blankMenu.newFile'),
      icon: 'file',
      disabled: readonly.value,
      hint,
    },
    {
      id: 'upload',
      label: t('files.blankMenu.upload'),
      icon: 'upload',
      disabled: readonly.value,
      hint,
    },
  ]
  if (clip) {
    items.push({
      id: 'paste',
      label: t('files.blankMenu.paste', { count: clip.entries.length }),
      icon: 'copy',
      divider: true,
      disabled: readonly.value,
      hint,
    })
  }
  // 终端永远可用：只读挂载点进去也只能看，但「看」本身就是需求
  items.push({
    id: 'terminal',
    label: t('files.menu.terminal'),
    icon: 'terminal',
    divider: true,
  })
  items.push({ id: 'refresh', label: t('files.toolbar.refresh'), icon: 'refresh' })
  items.push({
    id: 'hidden',
    label: showHidden.value ? t('files.toolbar.hideHidden') : t('files.toolbar.showHidden'),
    icon: showHidden.value ? 'eyeOff' : 'eye',
    divider: true,
  })
  if (displayEntries.value.length) {
    items.push({ id: 'selectAll', label: t('files.select.all'), icon: 'check' })
  }
  return items
})

function onBlankMenuSelect(id: string) {
  // 终端例外：它要等导航落地再关菜单，提前关会把这次导航自己取消掉
  if (id !== 'terminal') blankMenu.value = null
  switch (id) {
    case 'newDir':
      openCreateDialog('dir')
      break
    case 'newFile':
      openCreateDialog('file')
      break
    case 'upload':
      pickFiles()
      break
    case 'paste': {
      const clip = clipboard.value
      if (clip) void runTransfer(clip.mode, clip.entries, currentPath.value)
      break
    }
    case 'terminal':
      openTerminalAt(currentPath.value)
      break
    case 'refresh':
      void open(currentPath.value)
      break
    case 'hidden':
      void toggleHidden()
      break
    case 'selectAll':
      selectMode.value = true
      selection.value = displayEntries.value.map((entry) => entry.path)
      break
    default:
      break
  }
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
  void focusFieldLater('[data-testid="create-name"]')
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

/** 弹窗弹出后把焦点送进输入框并全选原名（新建之后可以直接敲字） */
async function focusFieldLater(selector: string) {
  await nextTick()
  const field = document.querySelector<HTMLInputElement>(selector)
  field?.focus()
  field?.select()
}

/** `entry` 省略时取右键菜单选中的那条（F2 走快捷键直传） */
function openRenameDialog(entry?: EntryInfo) {
  const target = entry ?? menu.value?.entry
  menu.value = null
  if (target) ops.value?.openRename(target)
}

/** 变更成功后的统一收尾：编辑器指向的文件被改/被删要跟着走，多选退出，列表重拉 */
async function onOpsDone(payload: FileOpDone) {
  if (payload.kind === 'delete') {
    for (const path of payload.paths) {
      if (editor.value?.path === path) forceCloseEditor()
      if (selected.value === path) selected.value = null
    }
  }
  if (payload.kind === 'rename' && payload.newPath && editor.value) {
    if (editor.value.path === payload.paths[0]) {
      editor.value.path = payload.newPath
      editor.value.name = payload.newPath.replace(/^.*\//, '')
    }
  }
  exitSelect()
  await open(currentPath.value)
}

type TransferMode = 'copy' | 'move'

/** `dest` 省略时默认当前目录（剪贴板粘贴 / 拖拽到文件夹都会显式传目标） */
function openTransferDialog(mode: TransferMode, targets?: EntryInfo[], dest?: string) {
  const list = targets ?? (menu.value ? [menu.value.entry] : [])
  menu.value = null
  if (list.length) ops.value?.openTransfer(mode, list, dest ?? currentPath.value)
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

/** 悬停复选框：没进多选模式时点一下就进（并勾上本项），已在多选里则纯切换 */
function toggleFromCheckbox(entry: EntryInfo) {
  if (selectMode.value) toggleSelect(entry)
  else enterSelect(entry)
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

// ---- 批量下载（设计文档 3.2.6.6） ----
//
// 一次手势只允许一个下载：连着开 N 个 `<a>`/iframe，浏览器会把第 2 个起当成
// 「多文件下载」拦掉（手机上连提示都没有），所以多选一律交给后端打成**一个** zip。
// 目录也一起打包 —— 以前多选只挑文件，选中文件夹等于被静默忽略。
const bulkDownloadConfirm = ref<EntryInfo[] | null>(null)

function downloadSelected() {
  const targets = selectedEntries.value
  if (!targets.length) return
  // 打包要读一遍文件、体积也不小，先让用户知道自己在拉什么
  if (targets.length > 10) {
    bulkDownloadConfirm.value = targets
    return
  }
  runBulkDownload(targets)
}

/** 多选下载的唯一出口：1 个文件直接存盘，其余（含目录）合成一个 zip */
function runBulkDownload(targets: EntryInfo[]) {
  if (targets.length === 1 && !targets[0].is_dir) {
    triggerDownload(filesApi.rawUrl(targets[0].path, { download: true }))
  } else {
    triggerDownload(filesApi.archiveUrl(targets.map((entry) => entry.path)))
  }
  exitSelect()
}

function confirmBulkDownload() {
  const targets = bulkDownloadConfirm.value ?? []
  bulkDownloadConfirm.value = null
  if (targets.length) runBulkDownload(targets)
}

// ---------------- 删除 / 属性（弹窗实现在 components/FileOpsDialogs.vue） ----------------

/** `targets` 省略时取右键菜单选中的那条（快捷键 / 多选操作栏都显式传） */
function openDeleteDialog(targets?: EntryInfo[]) {
  const list = targets ?? (menu.value ? [menu.value.entry] : [])
  menu.value = null
  ops.value?.openDelete(list)
}

function openStatDialog() {
  const entry = menu.value?.entry
  menu.value = null
  if (entry) ops.value?.openStat(entry)
}

const roots = ref<MountPoint[]>([])
const rootsImplicit = ref(false)
/** 当前目录所在挂载点是否只读：只读时全部变更入口置灰（设计文档 3.2.6） */
const readonly = computed(() => listing.value?.readonly ?? false)
/** 在线挂载点数量：1 个时跳过卡片页直达根目录（设计文档 3.2.1） */
const onlineMounts = computed(() => roots.value.filter((root) => root.online))
const singleMount = computed(() => onlineMounts.value.length === 1)
/** 当前所在挂载点（URL 里的 :mountId 与它保持同步） */
const activeMount = ref<MountPoint | null>(null)
/** 单挂载点 replace 跳转期间的全屏 Loading（设计文档 8.1：防闪烁） */
const redirecting = ref(false)

/**
 * 路径 → 挂载点：取「最长前缀命中」，避免 `/mnt/a` 与 `/mnt/ab` 互相误判。
 * 面包屑与 URL 同步共用同一套判定。
 */
function mountForPath(path: string): MountPoint | null {
  const normalized = path.replace(/\\/g, '/')
  let matched: MountPoint | null = null
  let matchedLength = -1
  for (const root of roots.value) {
    const prefix = root.path.replace(/\\/g, '/').replace(/\/+$/, '')
    if (
      prefix &&
      normalized.toLowerCase().startsWith(prefix.toLowerCase()) &&
      prefix.length > matchedLength
    ) {
      matched = root
      matchedLength = prefix.length
    }
  }
  return matched
}

/** 挂载点 + 绝对路径 → 路由目标（`/files/:mountId/<相对段>`） */
function routeFor(mount: MountPoint, path: string) {
  const base = mount.path.replace(/\\/g, '/').replace(/\/+$/, '')
  const normalized = path.replace(/\\/g, '/')
  const rest = base && normalized.toLowerCase().startsWith(base.toLowerCase())
    ? normalized.slice(base.length)
    : ''
  const segments = rest.split('/').filter(Boolean)
  return { name: 'files-mount' as const, params: { mountId: mount.id, pathMatch: segments } }
}

/** 路由参数 → 绝对路径（vue-router 已对 params 解码，这里不做二次解码） */
function pathFromRoute(mount: MountPoint, raw: unknown): string {
  const segments = Array.isArray(raw) ? raw.map(String) : raw ? [String(raw)] : []
  const base = mount.path.replace(/\\/g, '/').replace(/\/+$/, '')
  return segments.length ? `${base}/${segments.filter(Boolean).join('/')}` : mount.path
}

/**
 * 把当前目录同步进 URL（设计文档 3.2.1）。
 *
 * 文件管理不允许「原地换目录但不改 URL」——刷新、分享、浏览器前进后退都必须能回到同一目录。
 * v4.6.16 起用户导航（进目录 / 面包屑 / 返回上级）一律 push：目录层级必须留在历史栈里，
 * 浏览器返回键才能「回到上一个目录」而不是直接掉出文件管理（v4.6.15 用户反馈）。
 * replace 只保留给非导航场景：boot 落地、单挂载点跳转、操作完成后的原地刷新。
 */
function syncUrl(path: string, mode: 'push' | 'replace' = 'replace') {
  const mount = mountForPath(path)
  if (!mount) return
  activeMount.value = mount
  const target = routeFor(mount, path)
  if (router.resolve(target).fullPath === route.fullPath) return
  const navigate = mode === 'push' ? router.push : router.replace
  void navigate(target)
}
/** 工具栏「新建」下拉 */
const newMenuOpen = ref(false)
const currentPath = ref('')
const listing = ref<Listing | null>(null)
const loading = ref(true)
/**
 * 播放条让位（v4.6.9）：文件管理是满高应用面，桌面端播放条浮在内容之上，
 * 布局层不再整块预留高度（否则底栏离开窗口底边）。这里给滚动区补等同的底部空间，
 * 最后一行才能滚到播放条上方（详见 lib/player-layout.ts）。
 */
const clearance = computed(() =>
  currentTrack.value ? playerClearance(player.state.barStyle) : '',
)

/**
 * 骨架延后 180ms 再出现（v4.6.8）：目录扫描常常 100ms 内就回来，立刻画骨架只会
 * 「闪一下又换回真内容」；配上会话缓存，重新进入同一个目录时连骨架都不会出现。
 */
const loadingSkeleton = useDeferredLoading(loading)
/** 静默刷新中：列表照旧在屏幕上，只有刷新键转过来说明「正在重新读目录」 */
const refreshing = ref(false)
const loadingMore = ref(false)
const error = ref<{ status: number; message: string } | null>(null)
const selected = ref<string | null>(null)
const page = ref(1)
/** 下一页游标（设计文档 3.2.4：固定排序 + 游标分页，不再用页码偏移） */
const nextCursor = ref<string | null>(null)

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
// 续页触发点挂在 VirtualList / VirtualGrid 的 `bottom` 事件上，而不是外层塞一个
// 哨兵：这两个组件自己就是滚动容器（`h-full overflow-y-auto`），放在它们外面的
// 哨兵永远处于「可见」状态，IntersectionObserver 只会响一次 —— 第一页之后再
// 也不动。真正的滚动源在组件内部，判断就得在内部做。

/** 触屏设备没有双击语义，单击即打开 */
const coarsePointer = window.matchMedia?.('(hover: none)').matches ?? false
/** 窄屏（手机）：工具栏放不下搜索框，改成一个可展开的搜索行 + 「…」动作面板 */
const isMobile = useMediaQuery('(max-width: 639.98px)')
const mobileSearchOpen = ref(false)
const mobileSearchInput = ref<HTMLInputElement | null>(null)

function toggleMobileSearch() {
  mobileSearchOpen.value = !mobileSearchOpen.value
  if (mobileSearchOpen.value) {
    void nextTick(() => mobileSearchInput.value?.focus())
  } else if (searchTerm.value) {
    clearSearch()
  }
}

/** 手机上的「…」：没有右键，所有条目操作都从这里进 */
function openSheet(entry: EntryInfo) {
  menu.value = { x: 0, y: 0, entry }
  selected.value = entry.path
}

// ---------------- 搜索 ----------------
/**
 * 搜索是「递归 + 游标续页」的（设计文档 4.6）：
 * 默认只搜当前目录（跟以前一样），打开开关就搜整棵子树；
 * 两种模式都没有结果条数上限，翻到底才停。
 */
const SEARCH_RECURSIVE_KEY = 'asmoday.files.searchRecursive'
const searchTerm = ref('')
const searchResults = ref<EntryInfo[] | null>(null)
const searching = ref(false)
const searchingMore = ref(false)
/** 本页累计扫描过的条目数：关键词很窄时用它说明「还在往里翻」 */
const searchScanned = ref(0)
const searchHasMore = ref(false)
const recursiveSearch = ref(localStorage.getItem(SEARCH_RECURSIVE_KEY) === '1')
let searchCursor: string | null = null
/** 竞态令牌：连续敲键时只接受最后一次请求的结果 */
let searchToken = 0
let searchTimer: number | undefined

function scheduleSearch() {
  window.clearTimeout(searchTimer)
  searchTimer = window.setTimeout(runSearch, 260)
}

/** 切换「包含子目录」：持久化偏好并立刻重搜（否则开关看起来像没反应） */
function toggleRecursiveSearch() {
  recursiveSearch.value = !recursiveSearch.value
  localStorage.setItem(SEARCH_RECURSIVE_KEY, recursiveSearch.value ? '1' : '0')
  if (searchTerm.value.trim()) void runSearch()
}

function toSearchEntry(hit: SearchHit): EntryInfo {
  return {
    name: hit.name,
    path: hit.path,
    rel: hit.rel,
    is_dir: hit.is_dir,
    size: hit.size,
    modified: null,
    kind: hit.kind,
    ext: hit.ext,
    editable: hit.editable,
  }
}

async function runSearch() {
  const keyword = searchTerm.value.trim()
  if (!keyword) {
    searchResults.value = null
    searchCursor = null
    searchHasMore.value = false
    searchScanned.value = 0
    return
  }
  const token = ++searchToken
  searching.value = true
  searchCursor = null
  searchHasMore.value = false
  try {
    const page = await filesApi.search(keyword, currentPath.value, {
      recursive: recursiveSearch.value,
    })
    if (token !== searchToken) return
    searchResults.value = page.items.map(toSearchEntry)
    searchScanned.value = page.scanned
    searchCursor = page.next_cursor
    searchHasMore.value = page.has_more
  } catch (err) {
    if (token !== searchToken) return
    showToast(err instanceof Error ? err.message : t('files.list.searchFailed'))
  } finally {
    if (token === searchToken) searching.value = false
  }
}

/** 搜索结果触底续页：带上一页游标，按路径去重后再追加 */
async function loadMoreSearch() {
  const keyword = searchTerm.value.trim()
  if (!keyword || !searchHasMore.value || searchingMore.value || !searchCursor) return
  const token = searchToken
  searchingMore.value = true
  try {
    const page = await filesApi.search(keyword, currentPath.value, {
      recursive: recursiveSearch.value,
      cursor: searchCursor,
    })
    if (token !== searchToken) return
    searchScanned.value += page.scanned
    const seen = new Set((searchResults.value ?? []).map((entry) => entry.path))
    const extra = page.items.filter((hit) => !seen.has(hit.path)).map(toSearchEntry)
    searchResults.value = [...(searchResults.value ?? []), ...extra]
    searchCursor = page.next_cursor
    searchHasMore.value = page.has_more
  } catch (err) {
    showToast(err instanceof Error ? err.message : t('files.list.searchFailed'))
  } finally {
    searchingMore.value = false
  }
}

/** 搜索结果里这条命中的所在目录（相对搜索根；根下条目返回空串） */
function searchParent(entry: EntryInfo): string {
  const rel = entry.rel ?? ''
  const index = rel.lastIndexOf('/')
  return index < 0 ? '' : rel.slice(0, index)
}

function clearSearch() {
  searchToken += 1
  searchTerm.value = ''
  searchResults.value = null
  searchCursor = null
  searchHasMore.value = false
  searchScanned.value = 0
  searching.value = false
  searchingMore.value = false
}

// ---------------- 上传 ----------------
interface UploadTask {
  id: number
  name: string
  progress: number
  status: 'uploading' | 'done' | 'error'
  error?: string
  /** 原始文件与目标目录：错误后「重试」要用（用户可能已切走目录） */
  file?: File
  dir?: string
  /** 是否被本任务取消（组件卸载等），避免重试复活已取消的传输 */
  cancelled?: boolean
}

const uploads = ref<UploadTask[]>([])
/** tus 分片大小（MiB，0 = 关闭分块）。跟挂载点清单一起下发（3.2.3.1） */
const uploadChunkMb = ref(0)
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
      file,
      dir,
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

/** 错误任务的重试入口：文件对象还挂在任务上，直接重跑（分块自动续传） */
async function retryTask(task: UploadTask) {
  if (!task.file || !task.dir || task.status === 'uploading') return
  task.status = 'uploading'
  task.error = undefined
  try {
    await uploadOne(task.file, task.dir, task)
    task.status = 'done'
    task.progress = 100
    await open(task.dir)
  } catch (err) {
    task.status = 'error'
    task.error = err instanceof Error ? err.message : t('files.upload.error')
  }
}

// ---------------- tus 分块上传与断点续传（设计文档 3.2.3.1） ----------------

/** tus 续传凭证：同一文件（目录+名字+大小+mtime 全一致）断线/刷新后接着传 */
function tusResumeKey(dir: string, file: File): string {
  return `asmoday.tus.${[dir, file.name, file.size, file.lastModified]
    .map((part) => encodeURIComponent(String(part)))
    .join('|')}`
}

function readTusSession(dir: string, file: File): string | null {
  try {
    return localStorage.getItem(tusResumeKey(dir, file))
  } catch {
    return null
  }
}

function saveTusSession(dir: string, file: File, url: string) {
  try {
    localStorage.setItem(tusResumeKey(dir, file), url)
  } catch {
    /* 隐私模式等 localStorage 不可用：只损失续传能力，不影响上传 */
  }
}

function clearTusSession(dir: string, file: File) {
  try {
    localStorage.removeItem(tusResumeKey(dir, file))
  } catch {
    /* ignore */
  }
}

function uploadOne(file: File, dir: string, task: UploadTask): Promise<void> {
  const chunkMb = uploadChunkMb.value
  if (chunkMb > 0 && file.size > chunkMb * 1024 * 1024) {
    return uploadChunked(file, dir, task)
  }
  return uploadDirect(file, dir, task)
}

/** 整包 PUT：小文件走这条，一次请求完事（比建会话少两趟往返） */
function uploadDirect(file: File, dir: string, task: UploadTask): Promise<void> {
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

/**
 * tus 分块上传：逐片 PATCH，断点续传。
 *
 * 偏移的权威在服务端临时文件——每次（重）进入都先 HEAD 问真实进度，
 * 本地只保存会话地址当凭证；分片失败保留会话，重试时从服务端断点接着传。
 */
async function uploadChunked(file: File, dir: string, task: UploadTask): Promise<void> {
  const chunkSize = Math.max(1, uploadChunkMb.value) * 1024 * 1024
  let sessionUrl = readTusSession(dir, file)
  let offset = 0

  if (sessionUrl) {
    try {
      offset = await tusApi.offset(sessionUrl)
      // 上次最后一个分片已落盘但响应没回来：服务端已落位，直接当成功
      if (offset >= file.size) {
        clearTusSession(dir, file)
        return
      }
    } catch {
      // 会话失效（服务重启 / TTL 过期 / 手动清理）：从头再来
      sessionUrl = null
      offset = 0
    }
  }
  if (!sessionUrl) {
    sessionUrl = await tusApi.init(dir, file.name, file.size)
    saveTusSession(dir, file, sessionUrl)
    offset = 0
  }

  while (offset < file.size) {
    const end = Math.min(offset + chunkSize, file.size)
    try {
      await tusApi.patch(sessionUrl, file.slice(offset, end), offset, (loaded) => {
        task.progress = Math.min(99, Math.round(((offset + loaded) / file.size) * 100))
      })
    } catch (err) {
      // 会话已保留：点「重试」或重新拖入同一文件都会从断点续传
      throw new Error(
        `${err instanceof Error && err.message ? err.message : t('files.upload.network')}（${t('files.upload.resume')}）`,
      )
    }
    offset = end
  }
  clearTusSession(dir, file)
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
  // ---- 代码页元信息（设计文档 3.2.2.1）：保存时必须原样带回 ----
  encoding: TextEncoding
  /** 打开文件时的编码，用于判断「用户手动转换过」 */
  encodingOriginal: TextEncoding
  eol: EolType
  eolOriginal: EolType
  hasBom: boolean
  /** 原文混用 LF 与 CRLF（后端按多数派定风格，保存后统一） */
  mixedEol: boolean
  readonly: boolean
}

const editor = ref<EditorState | null>(null)
/** 编码 / 换行符下拉菜单 */
const metaMenu = ref(false)
/** 只读预览面板（Markdown / CSV / PDF / 不支持格式） */
const preview = ref<EntryInfo | null>(null)
const editorLoading = ref(false)
const toast = ref('')
/** 整屏编辑器根节点：手机软键盘弹出时靠它算出要让位的高度 */
const editorShell = ref<HTMLElement | null>(null)

/**
 * 正文字号（px）：编辑器是整屏的，字号是用户自己的抬头习惯，存本地。
 * 桌面 14 起步，窄屏 13（手机上 14 一行放不下多少字符）。
 */
const FONT_MIN = 11
const FONT_MAX = 20
const FONT_KEY = 'asmoday.editor.fontSize'
const fontSize = ref(readFontSize())

function readFontSize(): number {
  const fallback = typeof window !== 'undefined' && window.innerWidth < 640 ? 13 : 14
  try {
    const raw = Number(localStorage.getItem(FONT_KEY))
    if (Number.isFinite(raw) && raw >= FONT_MIN && raw <= FONT_MAX) return raw
  } catch {
    /* 隐私模式下 localStorage 可能不可用，用默认值即可 */
  }
  return fallback
}

/**
 * 软键盘遮挡高度。整屏编辑器在手机上会被键盘盖掉底栏和光标附近的行：
 * 把遮挡高度补成根容器的 padding-bottom，整块内容顶到键盘上方（收起自动归零）。
 */
const editorKbInset = useKeyboardInset(editorShell)

function stepFont(delta: number) {
  const next = Math.min(FONT_MAX, Math.max(FONT_MIN, fontSize.value + delta))
  if (next === fontSize.value) return
  fontSize.value = next
  try {
    localStorage.setItem(FONT_KEY, String(next))
  } catch {
    /* 存不下就算了，本次会话仍然生效 */
  }
}

/**
 * 未保存修改的关闭确认（取消 / 保存更改 / 放弃）。
 *
 * 不再用原生 `window.confirm`：它只有两态，用户想「存一下就关」还得先取消、
 * 再点保存、再关一次；而且原生弹窗在移动端样式完全不受控。
 * 弹层本身是 Promise，调用方 `await` 结果决定后续动作。
 */
const closePrompt = ref(false)
let closeAnswer: ((choice: 'cancel' | 'discard' | 'save') => void) | null = null

function askUnsaved(): Promise<'cancel' | 'discard' | 'save'> {
  // 已经有一层在问：当成取消，绝不叠两个弹窗
  if (closePrompt.value) return Promise.resolve('cancel')
  closePrompt.value = true
  metaMenu.value = false
  return new Promise((resolve) => {
    closeAnswer = resolve
  })
}

function answerClose(choice: 'cancel' | 'discard' | 'save') {
  closePrompt.value = false
  const resolve = closeAnswer
  closeAnswer = null
  resolve?.(choice)
}

const entries = computed(() => listing.value?.entries ?? [])
const total = computed(() => listing.value?.total ?? 0)
const hasMore = computed(() => listing.value?.has_more ?? false)
const dirty = computed(() => {
  const state = editor.value
  if (!state) return false
  // 换行符 / 编码的转换也算未保存修改：它们会改变落盘字节
  return (
    state.content !== state.original ||
    state.eol !== state.eolOriginal ||
    state.encoding !== state.encodingOriginal
  )
})

/** 状态栏展示用的编码名（与后端 `encoding_label` 同一套写法） */
const ENCODING_LABELS: Record<TextEncoding, string> = {
  'utf-8': 'UTF-8',
  gbk: 'GBK',
  'utf-16le': 'UTF-16 LE',
  'utf-16be': 'UTF-16 BE',
}
const encodingLabel = (encoding: TextEncoding) => ENCODING_LABELS[encoding] ?? encoding.toUpperCase()
const eolLabel = (eol: EolType) => (eol === 'crlf' ? 'CRLF' : 'LF')
const EOL_OPTIONS: { value: EolType; label: string; hint: string }[] = [
  { value: 'lf', label: 'LF', hint: 'Linux / macOS' },
  { value: 'crlf', label: 'CRLF', hint: 'Windows' },
]

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

/**
 * 编辑器路径行：挂载点名 + 相对目录 + 文件名。
 *
 * 与列表面包屑同源同口径——界面上绝不摊开真实盘符路径，
 * 也绝不因为路径长就把文件名挤掉（路径行本身可横向滚动）。
 */
const editorCrumbs = computed(() => {
  const state = editor.value
  if (!state) return [] as { label: string }[]
  const normalized = state.path.replace(/\\/g, '/')

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

  // 不在任何挂载点下（理论上不会发生）：只显示文件名，绝不退化成真实路径
  if (!matched) return [{ label: state.name }]

  const result: { label: string }[] = [{ label: matched.name }]
  for (const part of normalized.slice(matchedPrefix.length).split('/').filter(Boolean)) {
    result.push({ label: part })
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

// ---------------- 会话缓存：进目录先把上一帧贴出来（v4.6.8） ----------------
//
// 每次进入页面照旧全量重新拉取（设计文档 3.2.1：禁止拿旧数据当答案），缓存只负责
// 「第一帧画什么」—— 没有它，任何一次进入都要先闪一下骨架屏。键里带上排序与隐藏
// 文件开关，换了口径就当成另一个列表，绝不拿排序不符的旧内容冒充。
interface FilesSnapshot {
  listing: Listing
  page: number
  nextCursor: string | null
}

interface RootsSnapshot {
  mounts: MountPoint[]
  implicit: boolean
  /** 分片大小（MiB，0 = 全部整包）。快照里缺字段按 0 处理，等活数据到达再修正 */
  uploadChunkSizeMb?: number
}

const ROOTS_SNAPSHOT_KEY = 'files:roots'

/**
 * 目录快照键。
 *
 * v4.6.16 起键里带挂载点的真实路径：挂载点 id 是配置序号（m0/m1/…），
 * 改了挂载点指向的路径后 id 不变——只按 id 存取会把旧位置目录的上一帧
 * 当成新位置的「上一帧」贴回来（v4.6.15 用户反馈的「换挂载点后旧索引残留」）。
 */
function snapshotKey(mountId: string, segments: string[]): string {
  const mount = roots.value.find((root) => root.id === mountId)
  const base = mount ? mount.path.replace(/\\/g, '/').replace(/\/+$/, '') : ''
  const flags = `${sortKey.value}${sortDir.value}${showHidden.value ? 'h' : '-'}`
  return `files:${mountId}:${base}/${segments.join('/')}|${flags}`
}

/** 当前 URL 里的挂载点与目录段 */
function routeTarget(): { mountId: string; segments: string[] } {
  const mountId = typeof route.params.mountId === 'string' ? route.params.mountId : ''
  const raw = route.params.pathMatch
  const segments = (Array.isArray(raw) ? raw : raw ? [raw] : []).map(String)
  return { mountId, segments }
}

/** 绝对路径对应的缓存键：走「挂载点 → 路由段」，与 URL 完全同一套换算 */
function pathSnapshotKey(path: string): string {
  const mount = mountForPath(path)
  if (!mount) return ''
  const target = routeFor(mount, path)
  return snapshotKey(mount.id, (target.params.pathMatch ?? []).map(String))
}

/** 挂载点清单：面包屑要靠它把绝对路径映射成「根名 + 相对段」，必须跟列表一起贴 */
function seedRoots() {
  const snapshot = readCache<RootsSnapshot>(ROOTS_SNAPSHOT_KEY)
  if (!snapshot) return
  roots.value = snapshot.mounts
  rootsImplicit.value = snapshot.implicit
  uploadChunkMb.value = snapshot.uploadChunkSizeMb ?? 0
}

/** 把上一帧目录贴回屏幕；没有缓存就返回 false，调用方照旧进骨架态 */
function seedListing(mountId: string, segments: string[]): boolean {
  if (!mountId) return false
  const snapshot = readCache<FilesSnapshot>(snapshotKey(mountId, segments))
  if (!snapshot) return false
  listing.value = snapshot.listing
  currentPath.value = snapshot.listing.path
  page.value = snapshot.page
  nextCursor.value = snapshot.nextCursor
  loading.value = false
  return true
}

function saveListing(data: Listing) {
  const key = pathSnapshotKey(data.path)
  if (!key) return
  writeCache(key, {
    listing: data,
    page: data.page,
    nextCursor: data.next_cursor ?? null,
  } satisfies FilesSnapshot)
}

/** 重新加载挂载点，并按 URL 落到对应目录（设计文档 3.2.1 路由强制规范） */
async function boot() {
  // 上一帧先贴出来（v4.6.8）：重新进入同一个目录时不该先闪一下骨架屏，
  // 面包屑也不该「先只显示末段、再跳成完整路径」，所以挂载点清单一起贴。
  seedRoots()
  const seededRoots = roots.value.length ? roots.value : null
  const routeLoc = routeTarget()
  // 单挂载点：URL 只是 /files 时必然被 replace 到该挂载点根。与其先画卡片页、
  // 再盖一层跳转遮罩、再等目录接口，不如现在就把那个目录的上一帧贴出来。
  const cachedOnline = roots.value.filter((mount) => mount.online)
  const landing = routeLoc.mountId
    ? routeLoc
    : cachedOnline.length === 1
      ? { mountId: cachedOnline[0].id, segments: [] as string[] }
      : null
  const seeded = landing ? seedListing(landing.mountId, landing.segments) : roots.value.length > 0
  loading.value = !seeded
  refreshing.value = seeded
  error.value = null
  // 单挂载点从卡片页直达目录时，面包屑/只读判定都要有挂载点，先补上
  if (!routeLoc.mountId && cachedOnline.length === 1 && seeded) activeMount.value = cachedOnline[0]
  if (!seeded) {
    currentPath.value = ''
    listing.value = null
  }
  selected.value = null
  clearSearch()
  preview.value = null
  try {
    // 每次进入都必须重新拉取，禁止本地缓存（设计文档 3.2.1）
    const data = await filesApi.mounts()
    roots.value = data.mounts
    rootsImplicit.value = data.implicit
    uploadChunkMb.value = data.upload_chunk_size_mb ?? 0
    // 挂载点清单与上一帧不一致（改过配置 / 挂载点增删 / 掉线恢复）：
    // 贴回来的旧帧不可信，立刻清掉换骨架——别让旧目录内容顶着右上角转圈
    // 一直挂到新数据回来（v4.6.15 用户反馈的「换挂载点后旧索引残留」）
    if (
      seededRoots &&
      (seededRoots.length !== data.mounts.length ||
        seededRoots.some(
          (seed, index) =>
            seed.id !== data.mounts[index].id ||
            seed.path !== data.mounts[index].path ||
            seed.name !== data.mounts[index].name ||
            seed.online !== data.mounts[index].online,
        ))
    ) {
      listing.value = null
      currentPath.value = ''
    }
    writeCache(ROOTS_SNAPSHOT_KEY, {
      mounts: data.mounts,
      implicit: data.implicit,
      uploadChunkSizeMb: data.upload_chunk_size_mb ?? 0,
    } satisfies RootsSnapshot)

    // 一、URL 里已带 mountId（单挂载点跳转后的稳定形态 / 直接粘贴深链接）
    const requestedId = typeof route.params.mountId === 'string' ? route.params.mountId : ''
    const requested = requestedId
      ? data.mounts.find((mount) => mount.id === requestedId)
      : undefined
    if (requested && requested.online) {
      activeMount.value = requested
      const target = pathFromRoute(requested, route.params.pathMatch)
      const opened = await open(target, 1, { sync: false })
      if (!opened && target !== requested.path) {
        // 深链接指向的目录已不存在：退到挂载点根，别把用户留在报错页
        showToast(t('files.roots.missingToast'))
        await open(requested.path, 1, { sync: false })
      }
      return
    }
    // 配置被改过：URL 里的挂载点已经没了 / 掉线了 → 卡片页才是正确落点，
    // 但必须说清楚原因，静默替换会让用户以为点错了（设计文档 3.2.8）
    if (requestedId) {
      showToast(
        t(
          requested
            ? 'files.roots.offlineToast'
            : 'files.roots.removedToast',
        ),
      )
      void router.replace({ name: 'files' })
    }

    const online = data.mounts.filter((mount) => mount.online)
    if (!data.mounts.length) {
      error.value = null
    } else if (online.length === 1) {
      // 单挂载点：必须 router.replace 到 /files/:mountId/，跳过卡片页直达根目录。
      // 严禁「原地打开不改 URL」——那样刷新会回到卡片页，深链接也无从分享。
      // 已经贴好该目录的上一帧时不再盖遮罩：那层 blur 本身就是一次闪烁（v4.6.8）
      redirecting.value = !seeded
      try {
        activeMount.value = online[0]
        await router.replace(routeFor(online[0], online[0].path))
        await open(online[0].path, 1, { sync: false })
      } finally {
        redirecting.value = false
      }
      return
    }
  } catch (err) {
    error.value = describe(err)
  } finally {
    loading.value = false
    refreshing.value = false
  }
}

/** 打开挂载点根（内部走 open，但保留 loading 态避免白屏闪烁） */
async function openRoot(root: MountPoint, options: { push?: boolean } = {}) {
  if (!root.online) {
    showToast(t('files.roots.missingToast'))
    return
  }
  // 有未保存修改时用户可能选「取消」：那就别换目录了，不然还得再问一次
  if (editor.value && !(await closeEditor())) return
  activeMount.value = root
  await open(root.path, 1, { push: options.push })
}

/** 进入某个挂载点 */
function enterRoot(root: MountPoint) {
  void openRoot(root, { push: true })
}

/** 面包屑首段：多挂载点回卡片页，单挂载点回该挂载点根目录 */
async function goHome() {
  const home = activeMount.value ?? (singleMount.value ? onlineMounts.value[0] : null)
  if (singleMount.value && home) {
    if (currentPath.value !== home.path) {
      if (editor.value && !(await closeEditor())) return
      preview.value = null
      void open(home.path, 1, { push: true })
      return
    }
    // 已在挂载点根：按文档 3.2.1 单挂载点语义「点第一级回根」，原地不动
    return
  }
  if (editor.value && !(await closeEditor())) return
  preview.value = null
  currentPath.value = ''
  listing.value = null
  selected.value = null
  clearSearch()
  error.value = null
  activeMount.value = null
  // 回到卡片页时 URL 也要跟着回 /files（否则刷新又跳回目录）
  if (route.name !== 'files') void router.replace({ name: 'files' })
}

/**
 * 「返回上级」：目录内取父目录；已在挂载点根且是多挂载点模式时回卡片页。
 * 单挂载点模式下根目录没有上一级（文档 3.2.1：单挂载点跳过卡片页），按钮置灰。
 */
const canGoUp = computed(() => crumbs.value.length >= 2 || !singleMount.value)

async function goUp() {
  const list = crumbs.value
  if (list.length >= 2) {
    if (editor.value && !(await closeEditor())) return
    void open(list[list.length - 2].path, 1, { push: true })
    return
  }
  if (singleMount.value) return
  await goHome()
}

async function open(
  path: string,
  pageNo = 1,
  options: { sync?: boolean; push?: boolean } = {},
): Promise<boolean> {
  // 屏幕上已经是这个目录（会话缓存贴回来的上一帧 / 操作完成后的原地刷新）：
  // 这次只做静默刷新，绝不回到骨架态 —— 否则每次进目录、每次删完文件都要闪一下
  const silent = pageNo === 1 && listing.value?.path === path
  if (silent) refreshing.value = true
  else loading.value = true
  error.value = null
  selected.value = null
  preview.value = null
  // 中断上一次仍在飞的列表请求：挂载点被移除时，旧响应绝不能覆盖新状态
  inflight?.abort()
  const controller = new AbortController()
  inflight = controller
  try {
    const data = await filesApi.list(
      path,
      pageNo,
      showHidden.value,
      sortKey.value,
      sortDir.value,
      null,
      controller.signal,
    )
    listing.value = data
    currentPath.value = data.path
    page.value = data.page
    nextCursor.value = data.next_cursor ?? null
    saveListing(data)
    if (options.sync !== false) syncUrl(data.path, options.push ? 'push' : 'replace')
    return true
  } catch (err) {
    // 被后一次导航取消：静默退出，不要把 AbortError 当错误展示
    if (controller.signal.aborted) return false
    // 读不到目录时先确认挂载点还在（设计文档 3.2.8）：配置热重载删掉/掉线了，
    // 必须回卡片页并说明，不能把用户留在「目标不存在」的报错页
    if (!(await ensureMountAlive())) return false
    const described = describe(err)
    // 静默刷新失败：旧内容还在屏幕上，只轻提示，不要把整页换成错误页
    if (silent) {
      showToast(described.message)
      return false
    }
    error.value = described
    return false
  } finally {
    if (inflight === controller) {
      inflight = null
      loading.value = false
      refreshing.value = false
    }
  }
}

/** 当前在飞的目录请求（挂载点守卫与重新导航都要能立刻掐断它） */
let inflight: AbortController | null = null

/**
 * 挂载点存活性守卫（设计文档 3.2.8）。
 *
 * 返回 false 表示当前挂载点已经不可用、页面已被踢回卡片页。
 */
async function ensureMountAlive(): Promise<boolean> {
  const mount = activeMount.value
  if (!mount) return true
  let data: { mounts: MountPoint[]; implicit: boolean }
  try {
    data = await filesApi.mounts()
  } catch {
    // 网络抖动不当作挂载点消失，否则会把正在正常浏览的用户踢出去
    return true
  }
  roots.value = data.mounts
  rootsImplicit.value = data.implicit
  const fresh = data.mounts.find((item) => item.id === mount.id)
  if (!fresh) {
    leaveMount('removed')
    return false
  }
  if (!fresh.online) {
    leaveMount('offline')
    return false
  }
  activeMount.value = fresh
  return true
}

/** 中断在途请求、清空目录态并回卡片页，同时说明原因 */
function leaveMount(reason: 'removed' | 'offline') {
  inflight?.abort()
  inflight = null
  listing.value = null
  currentPath.value = ''
  selected.value = null
  preview.value = null
  error.value = null
  loading.value = false
  activeMount.value = null
  showToast(
    t(reason === 'removed' ? 'files.roots.removedToast' : 'files.roots.offlineToast'),
  )
  void router.replace({ name: 'files' })
}

async function loadMore() {
  // 搜索结果是另一条流：它的游标、去重和进度都跟目录列表独立
  if (isSearching.value) {
    await loadMoreSearch()
    return
  }
  if (!hasMore.value || loadingMore.value) return
  loadingMore.value = true
  try {
    const next = await filesApi.list(
      currentPath.value,
      page.value + 1,
      showHidden.value,
      sortKey.value,
      sortDir.value,
      nextCursor.value,
    )
    if (listing.value) {
      listing.value = { ...next, entries: [...listing.value.entries, ...next.entries] }
    }
    page.value = next.page
    nextCursor.value = next.next_cursor ?? null
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
    size: entry.size,
    modified: entry.modified,
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
  // 断链先拦：目标不存在或逃出挂载点，点击给明确提示而不是让后端报 4xx
  if (linkBroken(entry)) {
    showToast(t('files.symlink.brokenToast'))
    return
  }
  // 目录（含指向挂载点内目录的软链接）直接进入；
  // 后端解析 canonicalize 后归属校验照常生效，越权仍会被拒
  if (entry.is_dir || linkToDir(entry)) {
    await open(entry.path, 1, { push: true })
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
    if (editor.value && !(await closeEditor())) return
    preview.value = entry
    return
  }

  // 纯文本 / 代码：内置 CodeMirror
  if (entry.editable) {
    await openEditor(entry)
    return
  }

  // Office / CAD / 压缩包 / 未知二进制：不引入解析库，只提示下载
  if (editor.value && !(await closeEditor())) return
  preview.value = entry
}

function onEntryClick(entry: EntryInfo, event?: MouseEvent) {
  // 长按触发多选后，这一下 click 必须被吞掉（设计文档 8.6 第 1 条）
  if (pressConsumed) {
    pressConsumed = false
    return
  }
  // Shift + 点击 = 从锚点连选到这一条（桌面管理器 / FileBrowser 同款）
  if (event && event.shiftKey) {
    const list = displayEntries.value
    const anchorPath = rangeAnchor.value ?? selected.value ?? entry.path
    const from = list.findIndex((item) => item.path === anchorPath)
    const to = list.findIndex((item) => item.path === entry.path)
    if (from >= 0 && to >= 0) {
      event.preventDefault()
      selectMode.value = true
      const [start, end] = from <= to ? [from, to] : [to, from]
      const picked = new Set(selection.value)
      for (let index = start; index <= end; index += 1) picked.add(list[index].path)
      selection.value = [...picked]
      return
    }
  }
  // Ctrl / ⌘ + 点击 = 进入或追加多选
  if (event && (event.ctrlKey || event.metaKey)) {
    if (!selectMode.value) selectMode.value = true
    rangeAnchor.value = entry.path
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
    rangeAnchor.value = entry.path
  }
}

async function openEditor(entry: EntryInfo) {
  preview.value = null
  editorLoading.value = true
  metaMenu.value = false
  editor.value = {
    path: entry.path,
    name: entry.name,
    language: detectLanguage(entry.name),
    content: '',
    original: '',
    saving: false,
    error: null,
    encoding: 'utf-8',
    encodingOriginal: 'utf-8',
    eol: 'lf',
    eolOriginal: 'lf',
    hasBom: false,
    mixedEol: false,
    readonly: false,
  }
  const opened = entry.path
  try {
    // 走 /api/files/text：正文里的换行已被后端归一成 \n，元信息单独返回
    const file = await filesApi.readEditorFile(opened)
    const state = editor.value
    // 加载期间用户可能已经切到别的文件，按路径判定而不是对象身份（proxy 恒不等）
    if (!state || state.path !== opened) return
    state.content = file.content
    state.original = file.content
    state.encoding = file.encoding
    state.encodingOriginal = file.encoding
    state.eol = file.eol_type
    state.eolOriginal = file.eol_type
    state.hasBom = file.has_bom
    state.mixedEol = file.mixed_eol
    state.readonly = file.readonly
  } catch (err) {
    const info = describe(err)
    if (editor.value?.path === opened) editor.value.error = info.message
  } finally {
    if (editor.value?.path === opened) editorLoading.value = false
  }
}

function onEditorChange(value: string) {
  if (editor.value) editor.value.content = value
}

/** 状态栏下拉菜单：切换换行符（或把 GBK 转成 UTF-8） */
function pickEol(eol: EolType) {
  const state = editor.value
  metaMenu.value = false
  if (!state || state.eol === eol) return
  state.eol = eol
}

function convertToUtf8() {
  const state = editor.value
  metaMenu.value = false
  if (!state || state.encoding === 'utf-8') return
  state.encoding = 'utf-8'
}

async function saveEditor(): Promise<boolean> {
  const state = editor.value
  if (!state || state.saving) return false
  state.saving = true
  state.error = null
  try {
    const result = await filesApi.saveText(state.path, state.content, {
      encoding: state.encoding,
      eol: state.eol,
      bom: state.hasBom,
    })
    state.original = state.content
    // 以服务端实际落盘的风格为准（例如 GBK 不支持 BOM 时会被忽略）
    state.encoding = result.encoding
    state.encodingOriginal = result.encoding
    state.eol = result.eol_type
    state.eolOriginal = result.eol_type
    state.hasBom = result.has_bom
    state.mixedEol = false
    showToast(t('files.editor.saved'))
    // 目录里的体积/时间需要刷新
    if (listing.value) {
      const data = await filesApi.list(currentPath.value, 1, showHidden.value, sortKey.value, sortDir.value)
      listing.value = { ...data, entries: [...data.entries] }
    }
    return true
  } catch (err) {
    state.error = describe(err).message
    return false
  } finally {
    state.saving = false
  }
}

/** 无条件关掉编辑器（文件被删掉、被改名这类场景，问也没意义） */
function forceCloseEditor() {
  editor.value = null
  editorLoading.value = false
  metaMenu.value = false
  if (closePrompt.value) answerClose('discard')
}

/**
 * 关闭编辑器。@returns 是否真的关掉了（有未保存修改时用户可能点「取消」，
 * 也可能选「保存更改」但保存失败——那都不该继续后面的换目录/开预览）
 */
async function closeEditor(): Promise<boolean> {
  if (!editor.value) return true
  if (!dirty.value) {
    forceCloseEditor()
    return true
  }
  const choice = await askUnsaved()
  if (choice === 'cancel') return false
  if (choice === 'save' && !(await saveEditor())) return false
  forceCloseEditor()
  return true
}

/** 断链：后端无法跟随解析（目标不存在或逃出挂载点），target_is_dir 缺省/null */
function linkBroken(entry: EntryInfo): boolean {
  return Boolean(entry.is_symlink) && entry.target_is_dir == null
}

/** 软链接指向目录：按目录渲染（可进入），图标叠快捷方式箭头 */
function linkToDir(entry: EntryInfo): boolean {
  return Boolean(entry.is_symlink) && entry.target_is_dir === true
}

function iconFor(entry: EntryInfo) {
  // 软链接三态：断链 → link 图标（置灰）；指向目录 → 文件夹（叠箭头角标）；
  // 指向文件 → 按目标类型渲染，同样叠箭头角标
  if (entry.is_symlink) {
    if (entry.target_is_dir == null) return 'link'
    if (entry.target_is_dir) return 'folder'
  }
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
  if (linkBroken(entry)) return 'text-muted'
  if (entry.is_dir || linkToDir(entry)) return 'fg-warn'
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

/** 悬停标题：软链接展示目标路径与断链状态（设计文档 3.2.7） */
function entryTitle(entry: EntryInfo): string {
  if (!entry.is_symlink) return entry.name
  const target = entry.symlink_target || '?'
  if (entry.target_is_dir == null) return `${entry.name} → ${target}（${t('files.symlink.broken')}）`
  const kind = entry.target_is_dir ? t('files.symlink.dirTarget') : t('files.symlink.fileTarget')
  return `${entry.name} → ${target}（${kind}）`
}

// 分享 / 编辑器 / 预览 / 各类变更弹窗都是模态层，打开时锁住列表滚动
useScrollLock(
  computed(() =>
    Boolean(
      shareTarget.value ||
        editor.value ||
        preview.value ||
        createDialog.value ||
        bulkDownloadConfirm.value ||
        // 变更弹窗（重命名/移动复制/删除/属性）在 FileOpsDialogs 里，问它有没有开着
        ops.value?.isOpen,
    ),
  ),
)

// 浏览器前进/后退或外部改 URL：按新 URL 重新定位，但不反向 sync（否则死循环）。
// 后退到看过的目录时先贴会话快照（v4.6.16）：目录导航已改为 push，返回键
// 高频落在历史目录上，不该每次都从骨架屏开始。
watch(
  () => [route.params.mountId, route.params.pathMatch] as const,
  async () => {
    const id = typeof route.params.mountId === 'string' ? route.params.mountId : ''
    if (!id) {
      // 回到卡片页
      if (currentPath.value) {
        currentPath.value = ''
        listing.value = null
        activeMount.value = null
      }
      return
    }
    if (redirecting.value) return
    const mount = roots.value.find((root) => root.id === id)
    if (!mount || !mount.online) return
    const segments = (Array.isArray(route.params.pathMatch)
      ? route.params.pathMatch
      : route.params.pathMatch
        ? [route.params.pathMatch]
        : []
    ).map(String)
    seedListing(id, segments)
    const target = pathFromRoute(mount, route.params.pathMatch)
    activeMount.value = mount
    // 贴中快照时 open() 自行走静默刷新（listing.path === target）；没贴中就进骨架态
    await open(target, 1, { sync: false })
  },
)


// ---------------- 键盘 / 内部剪贴板 / 拖拽移动（对齐 FileBrowser 的等效交互） ----------------
//
// FileBrowser 的文件管理用 F2 重命名、Delete 删除、Ctrl+C/X/V 内部剪贴板、
// 把条目拖到文件夹即移动。这里沿用同一套键位与手势，但**不新增任何 UI**：
// 全部复用已有的重命名弹窗 / 删除确认 / 移动复制链路（含冲突必须明确表态的红线）。
// 移动端（coarsePointer）没有键盘，长按多选与底部操作栏保持原样。

/** 内部剪贴板：只存条目，落地时才换成路径（跨目录、跨挂载点都由后端判定） */
const clipboard = ref<{ mode: TransferMode; entries: EntryInfo[] } | null>(null)
/** 正在被拖拽的条目；非空表示这是一次内部拖拽，用来挡住「拖文件上传」的遮罩 */
const internalDrag = ref<EntryInfo[]>([])
/** 拖拽悬停的目录（高亮 + 松手即移动到这里） */
const dropTargetPath = ref<string | null>(null)
/** Shift 范围选择的锚点 */
const rangeAnchor = ref<string | null>(null)

/** 键盘操作的作用对象：多选态用选中的一批，否则用当前高亮的一条 */
function keyTargets(): EntryInfo[] {
  if (selectMode.value && selection.value.length) return selectedEntries.value
  if (!selected.value) return []
  const hit = displayEntries.value.find((entry) => entry.path === selected.value)
  return hit ? [hit] : []
}

/** 焦点在输入框里时绝不抢键（重命名输入框里按 Delete 是删字，不是删文件） */
function typingInField(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null
  if (!el) return false
  return (
    el.tagName === 'INPUT' ||
    el.tagName === 'TEXTAREA' ||
    el.tagName === 'SELECT' ||
    el.isContentEditable
  )
}

/** 任一弹层打开时不接管全局键（Esc 除外，留给弹层自己的 @keydown.esc） */
function anyOverlayOpen(): boolean {
  return Boolean(
    menu.value ||
      blankMenu.value ||
      createDialog.value ||
      bulkDownloadConfirm.value ||
      ops.value?.isOpen ||
      closePrompt.value ||
      preview.value ||
      editor.value,
  )
}

/** Esc 的关闭顺序：先关最上面那层，再退多选，最后取消高亮 */
function closeTopLayer(): boolean {
  if (closePrompt.value) {
    answerClose('cancel')
    return true
  }
  if (menu.value) {
    menu.value = null
    return true
  }
  if (blankMenu.value) {
    blankMenu.value = null
    return true
  }
  // 变更弹窗自己按「结果 → 删除 → 重命名 → 移动复制 → 属性」的顺序关
  if (ops.value?.closeAll()) return true
  if (createDialog.value) {
    createDialog.value = null
    return true
  }
  if (bulkDownloadConfirm.value) {
    bulkDownloadConfirm.value = null
    return true
  }
  if (searchResults.value) {
    clearSearch()
    return true
  }
  if (selectMode.value) {
    exitSelect()
    return true
  }
  if (selected.value) {
    selected.value = null
    return true
  }
  return false
}

/**
 * 系统返回键的关闭顺序。
 *
 * 与 Esc（closeTopLayer）刻意不同：编辑器排在弹窗之前——手机上用户想退出编辑
 * 一定是按返回，而不是先关掉某个弹窗；而 Esc 在 CodeMirror 里有补全等自身用途，
 * 不能顺手把编辑器关掉。预览面板同理。
 */
function closeOverlayForBack(): boolean {
  // 确认弹窗本身也是一层：返回键等于「取消」，不然会越关越乱
  if (closePrompt.value) {
    answerClose('cancel')
    return true
  }
  // closeEditor 是异步的（可能要用户选保存），这里只负责发起；
  // 浮层是否留下由 useBackClose 在下一轮 tick 重新判定并补哨兵
  if (editor.value) {
    void closeEditor()
    return true
  }
  if (preview.value) {
    preview.value = null
    return true
  }
  return closeTopLayer()
}

// 浮层在真机上是「一层历史」：返回键先退浮层，再退页面（此前按返回等于直接离开）
useBackClose(computed(anyOverlayOpen), closeOverlayForBack)

/**
 * 未保存的编辑内容绝不静默丢弃（离开页面 = 点侧边栏、点面包屑回卡片页）。
 *
 * 只问不改状态：确认后由路由自己把组件卸掉，别在这里 closeEditor()，
 * 否则组件还没卸载就先触发一次多余的响应式更新。
 */
onBeforeRouteLeave(async () => {
  if (!editor.value || !dirty.value) return true
  const choice = await askUnsaved()
  if (choice === 'cancel') return false
  if (choice === 'save' && !(await saveEditor())) return false
  return true
})

async function onKeydown(event: KeyboardEvent) {
  const key = event.key
  const meta = event.ctrlKey || event.metaKey

  // Esc 先于输入框判定：焦点在弹窗输入框里时也要能关掉弹窗
  if (key === 'Escape') {
    if (closeTopLayer()) event.preventDefault()
    return
  }

  if (typingInField(event.target)) return

  if (anyOverlayOpen() || !currentPath.value) return
  const targets = keyTargets()

  if (key === 'F2') {
    if (targets.length !== 1) return
    event.preventDefault()
    openRenameDialog(targets[0])
    return
  }

  // Backspace 只在列表上有选中时才当删除，避免用户想「返回上级」却删了东西
  if (key === 'Delete' || (key === 'Backspace' && targets.length)) {
    if (!targets.length || readonly.value) return
    event.preventDefault()
    openDeleteDialog(targets)
    return
  }

  if (!meta) return

  if (key === 'a' || key === 'A') {
    if (isSearching.value) return
    event.preventDefault()
    selectMode.value = true
    selection.value = displayEntries.value.map((entry) => entry.path)
    return
  }

  if (key === 'c' || key === 'C') {
    if (!targets.length) return
    clipboard.value = { mode: 'copy', entries: targets }
    showToast(t('files.keys.copied', { count: targets.length }))
    return
  }

  if (key === 'x' || key === 'X') {
    if (!targets.length || readonly.value) return
    clipboard.value = { mode: 'move', entries: targets }
    showToast(t('files.keys.cut', { count: targets.length }))
    return
  }

  if (key === 'v' || key === 'V') {
    const clip = clipboard.value
    if (!clip || (clip.mode === 'move' && readonly.value)) return
    event.preventDefault()
    await runTransfer(clip.mode, clip.entries, currentPath.value)
  }
}

/**
 * 直接落盘（冲突一律先按 skip 走）：没有同名项时一次点按就完成，命中冲突再回退到
 * 移动/复制对话框让用户明确选择覆盖还是改名 —— 绝不替用户决定覆盖。
 */
async function runTransfer(mode: TransferMode, targets: EntryInfo[], dest: string) {
  if (!targets.length || !dest) return
  try {
    const call = mode === 'copy' ? filesApi.copy : filesApi.move
    const response = await call(targets.map((entry) => entry.path), dest, 'skip')
    const summary = summarizeResults(response)
    // `skip` 模式命中的同名项会进 `skipped` 而不是 `failures`：那也是「需要用户决定」
    const conflict =
      summary.skipped > 0 ||
      summary.failures.some(
        (item) => item.error_code === 'E_CONFLICT' || item.error_code === 'E_DIR_CONFLICT',
      )
    if (conflict) {
      openTransferDialog(mode, targets, dest)
      return
    }
    exitSelect()
    if (summary.failed) {
      ops.value?.showResult(mode, summary, dest, 'skip')
    } else {
      showToast(t(mode === 'copy' ? 'files.keys.copiedDone' : 'files.keys.movedDone', { count: summary.ok }))
    }
    await open(currentPath.value)
  } catch (err) {
    showToast(describe(err).message)
  }
}

// ---- 条目拖到文件夹 = 移动（按住 Alt 拖 = 复制，与桌面管理器一致） ----
function movableEntries(entry: EntryInfo): EntryInfo[] {
  if (selectMode.value && isSelected(entry.path)) return selectedEntries.value
  return [entry]
}

function onEntryDragStart(event: DragEvent, entry: EntryInfo) {
  if (readonly.value) return
  const items = movableEntries(entry)
  internalDrag.value = items
  dropTargetPath.value = null
  // 拖拽期间别让容器把这次拖动当成「拖文件上传」
  dragging.value = false
  if (event.dataTransfer) {
    event.dataTransfer.effectAllowed = 'copyMove'
    // Firefox 必须带数据才会真正开始拖拽；内容不重要，内部拖拽只认 internalDrag
    event.dataTransfer.setData('text/plain', items.map((item) => item.name).join(', '))
  }
}

function onEntryDragOver(event: DragEvent, entry: EntryInfo) {
  if (!internalDrag.value.length) return
  if (!entry.is_dir && !linkToDir(entry)) return
  if (internalDrag.value.some((item) => item.path === entry.path)) return
  event.preventDefault()
  event.stopPropagation()
  if (event.dataTransfer) {
    event.dataTransfer.dropEffect = event.altKey ? 'copy' : 'move'
  }
  dropTargetPath.value = entry.path
}

function onEntryDragLeave(entry: EntryInfo) {
  if (dropTargetPath.value === entry.path) dropTargetPath.value = null
}

async function onEntryDrop(event: DragEvent, entry: EntryInfo) {
  const items = internalDrag.value
  if (!items.length) return
  if (!entry.is_dir && !linkToDir(entry)) return
  event.preventDefault()
  event.stopPropagation()
  internalDrag.value = []
  dropTargetPath.value = null
  dragging.value = false
  // 拖到自己身上 / 拖到自己所在目录：什么都不做
  if (items.some((item) => item.path === entry.path)) return
  const mode: TransferMode = event.altKey ? 'copy' : 'move'
  await runTransfer(mode, items, entry.path)
}

function onEntryDragEnd() {
  internalDrag.value = []
  dropTargetPath.value = null
  dragging.value = false
}

/** 容器级 dragover：内部拖拽不该弹出「拖到这里上传」的整屏遮罩 */
function onRootDragOver() {
  if (internalDrag.value.length) return
  dragging.value = true
}

onMounted(boot)

// ---------------- 挂载点存活性巡检（设计文档 3.2.8） ----------------
//
// 配置热重载没有推送通道，只能靠「回到页面立即查 + 每 30 秒巡检」发现当前挂载点
// 被移除或掉线；一旦发现就中断在途请求、清空目录态并回卡片页。
const MOUNT_PROBE_MS = 30_000
let probeTimer: number | null = null

function probeMounts() {
  if (document.visibilityState !== 'visible') return
  if (!activeMount.value) return
  void ensureMountAlive()
}

/** 刷新 / 关标签页 / 直接输新地址：浏览器只认「有没有未保存的编辑」，不问细节 */
function onBeforeUnload(event: BeforeUnloadEvent) {
  if (!editor.value || !dirty.value) return
  event.preventDefault()
  // 现代浏览器忽略自定义文案，但仍要求 returnValue 有值才会弹确认框
  event.returnValue = ''
}

onMounted(() => {
  probeTimer = window.setInterval(probeMounts, MOUNT_PROBE_MS)
  document.addEventListener('visibilitychange', probeMounts)
  window.addEventListener('keydown', onKeydown)
  window.addEventListener('beforeunload', onBeforeUnload)
})

onBeforeUnmount(() => {
  if (probeTimer !== null) window.clearInterval(probeTimer)
  probeTimer = null
  document.removeEventListener('visibilitychange', probeMounts)
  window.removeEventListener('keydown', onKeydown)
  window.removeEventListener('beforeunload', onBeforeUnload)
  inflight?.abort()
  inflight = null
})
</script>

<template>
  <section
    class="relative flex h-full min-h-0 overflow-hidden"
    @dragover.prevent="onRootDragOver"
    @dragleave.prevent="dragging = false"
    @drop.prevent="onDrop"
  >
    <!-- 单挂载点 replace 跳转期间的全屏遮罩：绝不闪一下卡片页（设计文档 8.1 防闪烁） -->
    <div
      v-if="redirecting"
      class="absolute inset-0 z-40 grid place-items-center bg-base/85 backdrop-blur-sm"
      data-testid="files-redirecting"
    >
      <div class="flex items-center gap-2 text-xs text-muted">
        <span class="size-4 animate-spin rounded-full border-2 border-line border-t-accent" />
        <span>
          {{ t('files.roots.opening') }}
          <template v-if="activeMount"> · {{ activeMount.name }}</template>
        </span>
      </div>
    </div>

    <!--
      编辑器：整屏接管（参考 FileBrowser 的编辑页，样式全用本站的）。
      Teleport 到 body 是刻意的——文件页在 #app-scroll 里，且路由过渡期祖先带 transform，
      fixed 会被当成相对祖先定位而错位；挂到 body 上才是真的「铺满浏览器」。
      列表不卸载：关掉编辑器立刻回到原来的目录与滚动位置，不重新拉一遍。
    -->
    <Teleport to="body">
      <div
        v-if="editor"
        ref="editorShell"
        class="fixed inset-0 z-[70] flex flex-col bg-canvas text-ink"
        :style="editorKbInset ? { paddingBottom: `${editorKbInset}px` } : undefined"
        data-testid="editor-fullscreen"
      >
        <header
          class="flex min-h-12 shrink-0 items-center gap-1.5 border-b border-line bg-panel px-2 sm:gap-3 sm:px-3"
          style="padding-top: env(safe-area-inset-top, 0px)"
        >
          <button
            type="button"
            class="grid size-8 shrink-0 place-items-center rounded-md text-muted transition-colors hover:bg-elevated hover:text-ink"
            :title="t('files.editor.close')"
            data-testid="editor-close"
            @click="closeEditor"
          >
            <AppIcon name="close" :size="16" />
          </button>

          <p class="min-w-0 flex-1 truncate text-[14px] text-ink" :title="editor.name">
            {{ editor.name }}<span
              v-if="dirty"
              class="ml-1 text-accent"
              :title="t('files.editor.unsaved')"
              data-testid="editor-dirty"
              >●</span
            >
          </p>

          <!-- 字号步进器（FileBrowser 的「+ 14px −」）：中间那格按一下回默认值 -->
          <div
            class="hidden shrink-0 items-center overflow-hidden rounded-md border border-line bg-elevated sm:flex"
            data-testid="editor-font"
          >
            <button
              type="button"
              class="grid size-7 place-items-center text-muted transition-colors hover:text-ink disabled:opacity-30 disabled:hover:text-muted"
              :disabled="fontSize <= FONT_MIN"
              :title="t('files.editor.fontSmaller')"
              @click="stepFont(-1)"
            >
              <AppIcon name="minus" :size="12" />
            </button>
            <button
              type="button"
              class="h-7 w-12 border-x border-line text-center font-mono text-[11px] text-muted transition-colors hover:bg-panel hover:text-ink"
              :title="t('files.editor.fontReset')"
              @click="stepFont(14 - fontSize)"
            >
              {{ fontSize }}px
            </button>
            <button
              type="button"
              class="grid size-7 place-items-center text-muted transition-colors hover:text-ink disabled:opacity-30 disabled:hover:text-muted"
              :disabled="fontSize >= FONT_MAX"
              :title="t('files.editor.fontLarger')"
              @click="stepFont(1)"
            >
              <AppIcon name="plus" :size="12" />
            </button>
          </div>

          <select
            v-model="editor.language"
            class="w-[86px] shrink-0 rounded-md border border-line bg-elevated px-1.5 py-1 text-xs text-ink outline-none focus:border-accent sm:w-auto sm:max-w-[130px]"
            :title="t('files.editor.switchLanguage')"
            data-testid="editor-language"
          >
            <option v-for="option in LANGUAGES" :key="option.key" :value="option.key">
              {{ option.key === 'plain' ? t('files.language.plain') : option.label }}
            </option>
          </select>

          <button
            type="button"
            class="flex shrink-0 items-center gap-1.5 rounded-md bg-accent px-2.5 py-1.5 text-xs text-white transition-opacity hover:opacity-90 disabled:opacity-50"
            :disabled="editor.saving"
            data-testid="editor-save"
            @click="saveEditor"
          >
            <AppIcon name="save" :size="13" />
            <span class="hidden sm:inline">
              {{ editor.saving ? t('files.editor.saving') : t('files.editor.save') }}
            </span>
            <kbd class="ml-1 hidden rounded bg-black/25 px-1 text-[11px] md:inline">Ctrl+S</kbd>
          </button>
        </header>

        <!-- 路径行：虚拟路径（挂载点名 › 目录 › 文件名），长路径横向滚动也不挤掉文件名 -->
        <nav
          class="flex h-7 shrink-0 items-center gap-1 overflow-x-auto border-b border-line bg-panel/60 px-2 text-[11px] text-muted sm:px-3"
          data-testid="editor-path"
        >
          <AppIcon name="folder" :size="11" class="shrink-0 opacity-60" />
          <template v-for="(crumb, index) in editorCrumbs" :key="`${index}-${crumb.label}`">
            <AppIcon
              v-if="index > 0"
              name="chevronRight"
              :size="10"
              class="shrink-0 opacity-40"
            />
            <span
              class="max-w-[220px] shrink-0 truncate"
              :class="index === editorCrumbs.length - 1 ? 'text-ink' : ''"
              :title="crumb.label"
            >
              {{ crumb.label }}
            </span>
          </template>
        </nav>

        <p
          v-if="editor.error"
          class="shrink-0 border-b border-red-500/30 bg-red-500/10 px-3 py-1.5 text-xs fg-danger"
        >
          {{ editor.error }}
        </p>

        <div class="relative min-h-0 flex-1">
          <div v-if="editorLoading" class="absolute inset-0 z-10 grid place-items-center bg-panel">
            <div class="w-2/3 space-y-2">
              <div
                v-for="i in 6"
                :key="i"
                class="skeleton h-3 rounded"
                :style="{ width: `${95 - i * 9}%` }"
              />
            </div>
          </div>
          <CodeEditor
            :content="editor.content"
            :language="editor.language"
            :dark="isDark"
            :font-size="fontSize"
            @change="onEditorChange"
            @save="saveEditor"
          />
        </div>

        <!-- 状态栏：编码 + 换行符（设计文档 3.2.2.1），点击可手动转换 -->
        <footer
          class="relative flex min-h-8 shrink-0 items-center gap-2 border-t border-line bg-panel px-2 text-[11px] text-muted"
          style="padding-bottom: env(safe-area-inset-bottom, 0px)"
        >
          <button
            type="button"
            class="flex items-center gap-1.5 rounded-md px-1.5 py-0.5 transition-colors hover:bg-elevated hover:text-ink disabled:cursor-not-allowed disabled:opacity-60"
            :title="t('files.editor.meta.toggle')"
            :disabled="editor.readonly"
            @click="metaMenu = !metaMenu"
          >
            <span class="font-mono">{{ encodingLabel(editor.encoding) }}</span>
            <span class="opacity-40">·</span>
            <span class="font-mono">{{ eolLabel(editor.eol) }}</span>
            <span
              v-if="editor.hasBom"
              class="rounded bg-elevated px-1 text-[10px] tracking-wide"
              :title="t('files.editor.meta.bomHint')"
            >
              BOM
            </span>
          </button>
          <span v-if="editor.mixedEol" class="fg-warn">
            {{ t('files.editor.meta.mixed', { eol: eolLabel(editor.eol) }) }}
          </span>
          <span v-if="editor.readonly" class="ml-auto">{{ t('files.editor.meta.readonly') }}</span>

          <div v-if="metaMenu" class="fixed inset-0 z-30" @click="metaMenu = false" />
          <div
            v-if="metaMenu"
            class="absolute bottom-full left-2 z-40 mb-1 w-56 overflow-hidden rounded-lg border border-line bg-elevated py-1 shadow-xl"
          >
            <p class="px-2.5 py-1 text-[10px] tracking-wide text-muted uppercase">
              {{ t('files.editor.meta.eol') }}
            </p>
            <button
              v-for="option in EOL_OPTIONS"
              :key="option.value"
              type="button"
              class="flex w-full items-center justify-between px-2.5 py-1.5 text-left text-xs transition-colors hover:bg-panel"
              :class="editor.eol === option.value ? 'text-accent' : 'text-ink'"
              @click="pickEol(option.value)"
            >
              <span class="font-mono">{{ option.label }}</span>
              <span class="text-[10px] text-muted">{{ option.hint }}</span>
            </button>
            <div class="my-1 border-t border-line" />
            <p class="px-2.5 py-1 text-[10px] tracking-wide text-muted uppercase">
              {{ t('files.editor.meta.encoding') }}
            </p>
            <button
              v-if="editor.encoding !== 'utf-8'"
              type="button"
              class="flex w-full items-center justify-between px-2.5 py-1.5 text-left text-xs text-ink transition-colors hover:bg-panel"
              @click="convertToUtf8"
            >
              <span>{{ t('files.editor.meta.toUtf8') }}</span>
              <span class="text-[10px] text-muted">UTF-8</span>
            </button>
            <p v-else class="flex items-center justify-between px-2.5 py-1.5 text-xs text-muted">
              <span class="font-mono">UTF-8</span>
              <span class="text-accent">✓</span>
            </p>
          </div>
        </footer>
      </div>
    </Teleport>

    <!--
      未保存修改的关闭确认：取消 / 保存更改 / 放弃修改。
      取代原来的原生 confirm —— 原生只有两态，想「存一下就关」得先取消再存再关。
    -->
    <Teleport to="body">
      <div
        v-if="closePrompt"
        class="fixed inset-0 z-[75] grid place-items-center modal-overlay bg-black/50 p-4"
        @click.self="answerClose('cancel')"
      >
        <div
          class="w-full max-w-sm modal-card rounded-2xl border border-line bg-panel p-5 shadow-xl"
          data-testid="editor-unsaved"
        >
          <p class="text-sm font-medium text-ink">{{ t('files.editor.confirmCloseTitle') }}</p>
          <p class="mt-2 truncate text-[12px] text-muted">{{ editor?.name }}</p>
          <p class="mt-1 text-[12px] text-muted">{{ t('files.editor.confirmCloseHint') }}</p>
          <div class="mt-4 flex items-center justify-end gap-2">
            <button
              type="button"
              class="rounded-md px-3 py-1.5 text-xs text-muted transition-colors hover:bg-elevated hover:text-ink"
              data-testid="editor-cancel-close"
              @click="answerClose('cancel')"
            >
              {{ t('common.action.cancel') }}
            </button>
            <button
              type="button"
              class="rounded-md bg-accent px-3 py-1.5 text-xs text-white transition-opacity hover:opacity-90 disabled:opacity-50"
              :disabled="editor?.saving"
              data-testid="editor-save-and-close"
              @click="answerClose('save')"
            >
              {{ t('files.editor.saveClose') }}
            </button>
            <button
              type="button"
              class="rounded-md px-3 py-1.5 text-xs fg-danger transition-colors hover:bg-red-500/10"
              data-testid="editor-discard"
              @click="answerClose('discard')"
            >
              {{ t('files.editor.discard') }}
            </button>
          </div>
        </div>
      </div>
    </Teleport>
    <!-- 中栏：只读预览（Markdown / CSV / PDF / 不支持格式） -->
    <FilePreviewPane
      v-if="preview"
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
            data-testid="roots-refresh"
            @click="boot"
          >
            <AppIcon name="refresh" :size="15" :class="refreshing ? 'animate-spin' : ''" />
          </button>
        </header>

        <div class="min-h-0 flex-1 overflow-y-auto p-4 md:p-6">
          <div
            v-if="loading"
            class="grid grid-cols-[repeat(auto-fill,minmax(150px,1fr))] gap-3 md:grid-cols-[repeat(auto-fill,minmax(200px,1fr))]"
          >
            <div v-if="!loadingSkeleton" class="col-span-full h-24" />
            <template v-else>
              <div v-for="i in 4" :key="i" class="skeleton h-24 rounded-xl" />
            </template>
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
            <!-- 工具栏（设计文档 8.6）
              分成互不重叠的两区：左边「导航」（返回上级 + 路径），右边「动作」（新建 / 上传 /
              刷新 + 搜索 / 排序 / 视图）。以前全部挤在一行、路径紧贴上传按钮，点路径回退时手
              一滑就点到上传；现在中间留弹性空隙 + 竖分隔线，窄屏直接拆两行（导航在上、动作
              在下），触屏也不会跨区误触。 -->
            <header
              class="flex shrink-0 flex-col border-b border-line bg-panel md:h-11 md:flex-row md:items-center md:px-3"
            >
              <!-- 导航区：只放导航，点击目标不再和动作按钮相邻 -->
              <nav
                class="order-1 flex min-w-0 items-center gap-1 px-1.5 py-1 md:flex-1 md:px-0 md:py-0 md:pr-3"
                data-testid="breadcrumb"
              >
                <button
                  class="touch-target grid size-7 shrink-0 place-items-center rounded-md text-muted transition-colors hover:bg-elevated hover:text-ink disabled:cursor-not-allowed disabled:opacity-30 disabled:hover:bg-transparent"
                  :title="t('files.toolbar.up')"
                  :disabled="!canGoUp"
                  data-testid="toolbar-up"
                  @click="goUp"
                >
                  <AppIcon name="arrowLeft" :size="15" />
                </button>
                <span class="h-4 w-px shrink-0 bg-line" aria-hidden="true" />

                <!-- 路径用「/」分段而不是 chevron：与顶栏「页面 > 子页」的层级箭头区分开，
                     一眼能看出这里是文件路径，不是又一套面包屑 -->
                <div class="flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto text-[13px]">
                  <button
                    class="grid size-6 shrink-0 place-items-center rounded text-muted transition-colors hover:bg-elevated hover:text-ink"
                    :title="t('files.toolbar.home')"
                    data-testid="breadcrumb-home"
                    @click="goHome"
                  >
                    <AppIcon name="home" :size="13" />
                  </button>
                  <span class="shrink-0 px-0.5 text-[11px] text-muted opacity-50" aria-hidden="true">/</span>
                  <template v-for="(crumb, index) in crumbs" :key="crumb.path">
                    <span
                      v-if="index > 0"
                      class="shrink-0 px-0.5 text-[11px] text-muted opacity-50"
                      aria-hidden="true"
                      >/</span
                    >
                    <!-- 当前目录不可点：以前点它等于原地刷新一次，是最常见的误触 -->
                    <span
                      v-if="index === crumbs.length - 1"
                      class="max-w-[200px] shrink-0 truncate rounded px-1.5 py-1 font-medium text-ink"
                      :title="crumb.label"
                      data-testid="breadcrumb-current"
                      >{{ crumb.label }}</span
                    >
                    <button
                      v-else
                      class="max-w-[160px] shrink-0 truncate rounded px-1.5 py-1 text-muted transition-colors hover:bg-elevated hover:text-ink"
                      :title="crumb.label"
                      @click="open(crumb.path, 1, { push: true })"
                    >
                      {{ crumb.label }}
                    </button>
                  </template>
                </div>

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
              </nav>

              <!-- 动作区：与导航之间隔着竖线与 12px 空白，手滑也点不到 -->
              <div
                class="order-2 flex w-full items-center gap-3 overflow-x-auto border-t border-line/60 px-1.5 py-1.5 md:w-auto md:shrink-0 md:gap-4 md:border-t-0 md:border-l md:border-line md:py-0 md:pl-3"
              >
                <!-- 目录动作：新建（3.2 工具栏 / 3.2.6.1）、上传、刷新 -->
                <div class="flex shrink-0 items-center gap-2">
                  <div class="relative shrink-0">
                    <button
                      class="touch-chip flex items-center gap-1 rounded-md border border-line px-2 py-1.5 text-xs text-ink transition-colors hover:border-accent/50 disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:border-line"
                      :disabled="readonly"
                      :title="readonly ? t('files.readonly.tooltip') : t('files.toolbar.new')"
                      data-testid="new-entry"
                      :aria-expanded="newMenuOpen"
                      @click="newMenuOpen = !newMenuOpen"
                    >
                      <AppIcon name="plus" :size="13" />
                      <span class="inline">{{ t('files.toolbar.new') }}</span>
                    </button>
                    <div v-if="newMenuOpen" class="fixed inset-0 z-40" @click="newMenuOpen = false" />
                    <div
                      v-if="newMenuOpen"
                      class="absolute left-0 top-full z-50 mt-1 w-40 overflow-hidden rounded-lg border border-line bg-panel py-1 shadow-xl"
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
                    class="touch-chip flex shrink-0 items-center gap-1.5 rounded-md bg-accent px-2.5 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-40"
                    :disabled="readonly"
                    :title="readonly ? t('files.readonly.tooltip') : t('files.toolbar.uploadHint')"
                    data-testid="toolbar-upload"
                    @click="pickFiles"
                  >
                    <AppIcon name="upload" :size="13" />
                    <span class="inline">{{ t('files.toolbar.upload') }}</span>
                  </button>
                  <input ref="fileInput" type="file" multiple class="hidden" @change="onFilesPicked" />
                </div>

                <!-- 视图控制：刷新 / 搜索 / 排序 / 隐藏文件 / 网格列表（都不改目录内容，与主操作分开） -->
                <div class="ml-auto flex shrink-0 items-center gap-1">
                  <button
                    class="touch-target grid size-7 shrink-0 place-items-center rounded-md text-muted transition-colors hover:bg-elevated hover:text-ink"
                    :title="t('files.toolbar.refresh')"
                    data-testid="toolbar-refresh"
                    @click="open(currentPath)"
                  >
                    <!-- 静默刷新不再有骨架屏，刷新键转起来是唯一的「正在读目录」反馈 -->
                    <AppIcon name="refresh" :size="15" :class="refreshing ? 'animate-spin' : ''" />
                  </button>

                  <div class="relative hidden w-44 shrink-0 sm:block md:w-40 xl:w-60">
                    <span class="absolute left-2 top-1/2 -translate-y-1/2 text-muted">
                      <AppIcon name="search" :size="13" />
                    </span>
                    <input
                      v-model="searchTerm"
                      :placeholder="
                        searching
                          ? t('files.toolbar.searching')
                          : recursiveSearch
                            ? t('files.toolbar.searchPlaceholderDeep')
                            : t('files.toolbar.searchPlaceholder')
                      "
                      class="w-full rounded-full border border-line bg-elevated py-1 pl-7 text-[12px] text-ink outline-none focus:border-accent"
                      :class="searchTerm ? 'pr-12' : 'pr-7'"
                      @input="scheduleSearch"
                      @keydown.esc="clearSearch"
                    />
                    <!-- 搜索范围开关：贴在输入框里，不额外占工具栏宽度 -->
                    <button
                      v-if="searchTerm"
                      class="absolute right-6 top-1/2 -translate-y-1/2 transition-colors"
                      :class="recursiveSearch ? 'text-accent' : 'text-muted hover:text-ink'"
                      :title="
                        recursiveSearch
                          ? t('files.toolbar.searchScopeAll')
                          : t('files.toolbar.searchScopeDir')
                      "
                      :aria-pressed="recursiveSearch"
                      data-testid="search-scope"
                      @click="toggleRecursiveSearch"
                    >
                      <AppIcon :name="recursiveSearch ? 'folderOpen' : 'folder'" :size="12" />
                    </button>
                    <button
                      v-if="searchTerm"
                      class="absolute right-1.5 top-1/2 -translate-y-1/2 text-muted hover:text-ink"
                      :title="t('files.toolbar.clearSearch')"
                      @click="clearSearch"
                    >
                      <AppIcon name="close" :size="12" />
                    </button>
                  </div>

                  <!-- 窄屏没有常驻搜索框：一个按钮展开第二行，手机也能搜（设计文档 8.6） -->
                  <button
                    class="touch-target grid size-7 shrink-0 place-items-center rounded-md transition-colors hover:bg-elevated sm:hidden"
                    :class="mobileSearchOpen ? 'bg-accent-soft text-accent' : 'text-muted hover:text-ink'"
                    :title="t('files.toolbar.search')"
                    data-testid="mobile-search-toggle"
                    :aria-expanded="mobileSearchOpen"
                    @click="toggleMobileSearch"
                  >
                    <AppIcon name="search" :size="15" />
                  </button>

                  <!-- 排序（设计文档 v2.7.0 3.2.1：名称/修改时间/大小 + 升降序，状态持久化） -->
                  <div class="relative shrink-0">
                    <button
                      class="touch-chip flex shrink-0 items-center gap-1 rounded-md border border-line px-2 py-1.5 text-xs text-ink transition-colors hover:border-accent/50"
                      :title="t('files.sort.label')"
                      data-testid="sort-menu"
                      :aria-expanded="sortMenuOpen"
                      @click="sortMenuOpen = !sortMenuOpen"
                    >
                      <AppIcon name="sort" :size="13" />
                      <span class="hidden xl:inline">{{ sortLabel }}</span>
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
                    class="touch-target grid size-7 shrink-0 place-items-center rounded-md transition-colors"
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
                      data-testid="view-grid"
                      @click="setView('grid')"
                    >
                      <AppIcon name="grid" :size="13" />
                    </button>
                    <button
                      class="grid size-6 place-items-center rounded transition-colors"
                      :class="viewMode === 'list' ? 'bg-accent-soft text-accent' : 'text-muted hover:text-ink'"
                      :title="t('files.toolbar.list')"
                      data-testid="view-list"
                      @click="setView('list')"
                    >
                      <AppIcon name="rows" :size="13" />
                    </button>
                  </div>
                </div>
              </div>
            </header>

      <!-- 窄屏搜索行：与工具栏分两行，输入框才够宽（不抢面包屑的位置） -->
      <div
        v-if="mobileSearchOpen"
        class="flex shrink-0 items-center gap-1.5 border-b border-line bg-panel px-2 py-1.5 sm:hidden"
        data-testid="mobile-search-row"
      >
        <div class="relative min-w-0 flex-1">
          <span class="absolute left-2 top-1/2 -translate-y-1/2 text-muted">
            <AppIcon name="search" :size="13" />
          </span>
          <input
            ref="mobileSearchInput"
            v-model="searchTerm"
            :placeholder="
              searching
                ? t('files.toolbar.searching')
                : recursiveSearch
                  ? t('files.toolbar.searchPlaceholderDeep')
                  : t('files.toolbar.searchPlaceholder')
            "
            class="w-full rounded-full border border-line bg-elevated py-1.5 pl-7 pr-7 text-[13px] text-ink outline-none focus:border-accent"
            enterkeyhint="search"
            @input="scheduleSearch"
          />
          <button
            v-if="searchTerm"
            class="absolute right-1.5 top-1/2 -translate-y-1/2 text-muted"
            :title="t('files.toolbar.clearSearch')"
            @click="clearSearch"
          >
            <AppIcon name="close" :size="12" />
          </button>
        </div>
        <button
          class="grid size-9 shrink-0 place-items-center rounded-lg transition-colors"
          :class="recursiveSearch ? 'bg-accent-soft text-accent' : 'text-muted'"
          :title="
            recursiveSearch ? t('files.toolbar.searchScopeAll') : t('files.toolbar.searchScopeDir')
          "
          :aria-pressed="recursiveSearch"
          @click="toggleRecursiveSearch"
        >
          <AppIcon :name="recursiveSearch ? 'folderOpen' : 'folder'" :size="15" />
        </button>
        <button
          class="h-9 shrink-0 rounded-lg px-2.5 text-[13px] text-muted"
          @click="toggleMobileSearch"
        >
          {{ t('common.action.cancel') }}
        </button>
      </div>

      <!-- 内容 -->
      <div
        class="relative min-h-0 flex-1 overflow-y-auto"
        :class="selectMode ? 'pb-32 sm:pb-20' : ''"
        data-testid="entry-scroll"
        @contextmenu.prevent="openBlankMenu"
      >
        <div v-if="loading" class="p-3">
          <!-- 180ms 内就回来的目录不画骨架：直接留白，避免「闪一下灰块又换成真内容」 -->
          <div v-if="!loadingSkeleton" class="h-full" />
          <div v-else class="grid grid-cols-[repeat(auto-fill,minmax(104px,1fr))] gap-2">
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
          v-else-if="displayEntries.length === 0 && !searchHasMore && !searchingMore"
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
          :class="clearance"
          @bottom="loadMore"
        >
          <template #default="{ item: entry }">
            <button
              type="button"
              class="entry-tile group relative flex size-full flex-col items-center gap-2 rounded-xl border border-transparent p-3 transition-colors hover:border-line hover:bg-panel"
              :class="[
                selected === entry.path ? 'border-accent/50 bg-panel' : '',
                selectMode && isSelected(entry.path) ? 'border-accent bg-accent-soft' : '',
                dropTargetPath === entry.path ? 'border-accent ring-2 ring-accent/40' : '',
                linkBroken(entry) ? 'opacity-50' : '',
              ]"
              :title="entryTitle(entry)"
              :data-entry="entry.name"
              :data-kind="entry.kind"
              :data-selected="isSelected(entry.path) ? '1' : undefined"
              :draggable="!readonly"
              @click="onEntryClick(entry, $event)"
              @dblclick="activate(entry)"
              @contextmenu.prevent="openMenu($event, entry)"
              @dragstart="onEntryDragStart($event, entry)"
              @dragend="onEntryDragEnd"
              @dragover="onEntryDragOver($event, entry)"
              @dragleave="onEntryDragLeave(entry)"
              @drop="onEntryDrop($event, entry)"
              @touchstart.passive="onPressStart($event, entry)"
              @touchmove.passive="onPressMove($event)"
              @touchend="cancelPress"
              @touchcancel="cancelPress"
            >
              <!-- 窄屏「…」：触屏没有右键，重命名/分享/属性只能从这里进（设计文档 8.6） -->
              <span
                v-if="isMobile"
                role="button"
                :aria-label="t('files.menu.more')"
                class="absolute right-0.5 top-0.5 z-10 grid size-8 place-items-center rounded-full bg-panel/85 text-muted shadow-sm active:bg-elevated"
                data-entry-more
                @click.stop="openSheet(entry)"
              >
                <AppIcon name="more" :size="15" />
              </span>
              <!-- 悬停浮现复选框（设计文档 3.2.1 / 8.4.12）：左上角，与缩略图角标区分 -->
              <span
                role="checkbox"
                :aria-checked="isSelected(entry.path)"
                :title="t('files.select.toggle')"
                class="absolute left-1 top-1 z-10 grid size-5 cursor-pointer place-items-center rounded-full border transition-all"
                :class="[
                  isSelected(entry.path)
                    ? 'border-accent bg-accent text-white'
                    : 'border-line bg-panel/90 text-transparent hover:border-accent/60',
                  selectMode ? 'opacity-100' : 'opacity-0 group-hover:opacity-100',
                ]"
                data-select-toggle
                @click.stop="toggleFromCheckbox(entry)"
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
                  :src="thumbUrl(entry.path, THUMB_WIDTH.tile, entry.modified)"
                  class="size-full object-cover"
                  loading="lazy"
                  alt=""
                  draggable="false"
                  @error="markThumbFailed(entry.path)"
                />
                <AppIcon v-else :name="iconFor(entry)" :size="34" />
                <!-- 软链接角标：文件夹/文件图标叠快捷方式箭头（设计文档 3.2.7） -->
                <span
                  v-if="entry.is_symlink && entry.target_is_dir != null"
                  class="pointer-events-none absolute right-1 top-1 grid size-4 place-items-center rounded-full bg-panel/90 text-ink shadow-sm"
                >
                  <AppIcon name="arrowUpRight" :size="9" />
                </span>
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
              <!-- 搜索结果可能来自任意子目录，标出它所在的位置 -->
              <span
                v-if="isSearching && searchParent(entry)"
                class="-mt-1 w-full truncate text-center text-[10px] leading-tight text-muted"
              >
                {{ searchParent(entry) }}
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
          <VirtualList
            :items="displayEntries"
            :row-height="48"
            :class="clearance"
            @bottom="loadMore"
          >
          <template #default="{ item: entry }">
            <div
              class="entry-tile flex h-full cursor-default items-center gap-2 border-t border-line/60 px-3 text-[13px] transition-colors hover:bg-elevated/60"
              :class="[
                selected === entry.path ? 'bg-accent-soft' : '',
                selectMode && isSelected(entry.path) ? 'bg-accent-soft' : '',
                dropTargetPath === entry.path ? 'bg-accent-soft ring-2 ring-inset ring-accent/40' : '',
                linkBroken(entry) ? 'opacity-50' : '',
              ]"
              :title="entryTitle(entry)"
              :data-entry="entry.name"
              :data-kind="entry.kind"
              :data-selected="isSelected(entry.path) ? '1' : undefined"
              :draggable="!readonly"
              @click="onEntryClick(entry, $event)"
              @dblclick="activate(entry)"
              @contextmenu.prevent="openMenu($event, entry)"
              @dragstart="onEntryDragStart($event, entry)"
              @dragend="onEntryDragEnd"
              @dragover="onEntryDragOver($event, entry)"
              @dragleave="onEntryDragLeave(entry)"
              @drop="onEntryDrop($event, entry)"
              @touchstart.passive="onPressStart($event, entry)"
              @touchmove.passive="onPressMove($event)"
              @touchend="cancelPress"
              @touchcancel="cancelPress"
            >
              <!-- 列表左侧常驻复选框（设计文档 3.2.1：必须有） -->
              <span
                role="checkbox"
                :aria-checked="isSelected(entry.path)"
                :title="t('files.select.toggle')"
                class="grid size-4 shrink-0 cursor-pointer place-items-center rounded-full border transition-colors"
                :class="
                  isSelected(entry.path)
                    ? 'border-accent bg-accent text-white'
                    : 'border-line text-transparent hover:border-accent/60'
                "
                data-select-toggle
                @click.stop="toggleFromCheckbox(entry)"
              >
                <AppIcon name="check" :size="10" />
              </span>
              <span class="relative" :class="toneFor(entry)">
                <AppIcon :name="iconFor(entry)" :size="15" />
                <!-- 软链接角标：列表视图同样叠快捷方式箭头 -->
                <span
                  v-if="entry.is_symlink && entry.target_is_dir != null"
                  class="absolute -right-1.5 -top-1 grid size-2.5 place-items-center rounded-full bg-panel text-ink"
                >
                  <AppIcon name="arrowUpRight" :size="7" />
                </span>
              </span>
              <span class="min-w-0 flex-1 truncate text-ink" :title="entryTitle(entry)">
                {{ entry.name }}
                <span v-if="isSearching && searchParent(entry)" class="ml-1 text-[11px] text-muted">
                  {{ searchParent(entry) }}
                </span>
              </span>
              <span class="w-24 shrink-0 text-right text-muted whitespace-nowrap">
                {{ entry.is_dir ? '—' : formatSize(entry.size) }}
              </span>
              <span class="hidden w-36 shrink-0 text-right text-muted whitespace-nowrap sm:block">
                {{ formatTime(entry.modified) }}
              </span>
              <button
                v-if="isMobile"
                type="button"
                class="grid size-9 shrink-0 place-items-center rounded-lg text-muted active:bg-elevated"
                :title="t('files.menu.more')"
                data-entry-more
                @click.stop="openSheet(entry)"
              >
                <AppIcon name="more" :size="16" />
              </button>
            </div>
          </template>
          </VirtualList>
        </template>

        <!-- 续页骨架（触底判定在虚拟列表内部，见 VirtualGrid/VirtualList 的 bottom 事件） -->
        <div
          v-if="loadingMore || searchingMore"
          class="flex items-center justify-center gap-2 py-4"
        >
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
          <span v-if="recursiveSearch" class="hidden sm:inline">
            · {{ t('files.list.scopeAll') }}
          </span>
          <span v-if="searchHasMore" class="hidden md:inline">
            · {{ t('files.list.scanned', { count: searchScanned }) }}
          </span>
          <span class="hidden sm:inline"> · {{ t('files.list.escToClear') }}</span>
        </span>
        <span v-else>
          {{ t('files.list.total', { count: total }) }}
          <span class="hidden sm:inline">
            · {{ t('files.list.shown', { shown: displayEntries.length }) }}
          </span>
        </span>
        <!-- 键位提示挂在底栏（不占新位置）：鼠标用户第一次也能发现 F2 / Delete / 拖拽 -->
        <span v-if="!coarsePointer" class="hidden truncate pl-3 text-[11px] md:inline">
          {{ t('files.keys.hint') }}
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
          <!--
            手机上按钮只留图标等于让用户猜（下载/移动/复制/删除四个箭头和方块太像），
            这里分两行：上面一行「已选 N 项 + 取消」，下面一行等宽图标+文字（设计文档 8.6）。
          -->
          <div class="px-3 py-2">
            <div class="flex items-center gap-1.5 pb-2 sm:hidden">
              <span class="min-w-0 flex-1 truncate text-[12px] text-muted">
                {{ t('files.select.count', { count: selection.length }) }}
              </span>
              <button
                class="touch-chip rounded-md bg-elevated px-3 py-1 text-[12px] text-ink"
                @click="exitSelect"
              >
                {{ t('files.select.cancel') }}
              </button>
            </div>
            <div class="flex items-center gap-1.5">
              <span class="hidden min-w-0 flex-1 truncate text-[12px] text-muted sm:block">
                {{ t('files.select.count', { count: selection.length }) }}
              </span>
              <button
                class="touch-chip flex flex-1 items-center justify-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] text-ink transition-colors hover:border-accent/50 sm:flex-none"
                @click="downloadSelected"
              >
                <AppIcon name="download" :size="12" />
                <span>{{ t('files.select.download') }}</span>
              </button>
              <button
                class="touch-chip flex flex-1 items-center justify-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] text-ink transition-colors hover:border-accent/50 disabled:cursor-not-allowed disabled:opacity-40 sm:flex-none"
                :disabled="readonly"
                :title="readonly ? t('files.readonly.tooltip') : ''"
                @click="openTransferDialog('move', selectedEntries)"
              >
                <AppIcon name="arrowRight" :size="12" />
                <span>{{ t('files.select.move') }}</span>
              </button>
              <button
                class="touch-chip flex flex-1 items-center justify-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] text-ink transition-colors hover:border-accent/50 disabled:cursor-not-allowed disabled:opacity-40 sm:flex-none"
                :disabled="readonly"
                :title="readonly ? t('files.readonly.tooltip') : ''"
                @click="openTransferDialog('copy', selectedEntries)"
              >
                <AppIcon name="gallery" :size="12" />
                <span>{{ t('files.select.copy') }}</span>
              </button>
              <button
                class="touch-chip flex flex-1 items-center justify-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] fg-danger transition-colors hover:border-red-400/50 disabled:cursor-not-allowed disabled:opacity-40 sm:flex-none"
                :disabled="readonly"
                :title="readonly ? t('files.readonly.tooltip') : ''"
                @click="openDeleteDialog(selectedEntries)"
              >
                <AppIcon name="trash" :size="12" />
                <span>{{ t('files.select.delete') }}</span>
              </button>
              <button
                class="touch-chip hidden rounded-md bg-elevated px-2 py-1 text-[12px] text-ink transition-colors hover:bg-line sm:block"
                @click="exitSelect"
              >
                {{ t('files.select.cancel') }}
              </button>
            </div>
          </div>
        </div>
      </Transition>
      </template>
    </div>

    <!-- 全局右键菜单（设计文档 3.2.5：文件管理 / 灯箱 / 影视库共用同一组件） -->
    <ContextMenu
      :open="!!menu"
      :x="menu?.x ?? 0"
      :y="menu?.y ?? 0"
      :items="fileMenuItems"
      :title="menu?.entry.name"
      @select="onMenuSelect"
      @close="menu = null"
    />

    <!-- 空白处右键：作用对象是当前目录（新建在哪儿点就在哪儿建） -->
    <ContextMenu
      :open="!!blankMenu"
      :x="blankMenu?.x ?? 0"
      :y="blankMenu?.y ?? 0"
      :items="blankMenuItems"
      :title="blankTitle"
      @select="onBlankMenuSelect"
      @close="blankMenu = null"
    />

    <!-- 变更弹窗（重命名 / 移动复制 / 删除 / 属性）：与媒体库共用同一个实现 -->
    <FileOpsDialogs
      ref="ops"
      :start-dir="currentPath"
      :roots="roots"
      @done="onOpsDone"
      @toast="showToast"
    />

    <!-- 多选下载确认（设计文档 3.2.6.6：> 10 个先提示会被浏览器拦截） -->
    <div
      v-if="bulkDownloadConfirm"
      class="fixed inset-0 z-50 grid place-items-center modal-overlay bg-black/50 p-4"
      @click.self="bulkDownloadConfirm = null"
    >
      <div class="w-full max-w-sm modal-card rounded-2xl border border-line bg-panel p-5 shadow-xl">
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
      class="fixed inset-0 z-50 grid place-items-center modal-overlay bg-black/50 p-4"
      @click.self="createDialog = null"
    >
      <div class="w-full max-w-sm modal-card rounded-2xl border border-line bg-panel p-5 shadow-xl">
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
            autofocus
            @keydown.enter="submitCreate"
            @keydown.esc="createDialog = null"
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
          <button
            v-if="task.status === 'error' && task.file"
            class="shrink-0 text-accent hover:underline"
            @click="retryTask(task)"
          >
            {{ t('files.upload.retry') }}
          </button>
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

/* 下拉菜单项（新建/排序）：.menu-item 只在 ContextMenu.vue 的 scoped 样式里，
   本组件里必须自带一份，否则选项挤成一行、箭头贴在文字后面 */
.menu-item {
  display: flex;
  width: 100%;
  align-items: center;
  gap: 8px;
  padding: 8px 12px;
  font-size: 13px;
  text-align: left;
  color: var(--text-main);
  transition: background-color 0.15s ease;
}
.menu-item:hover {
  background: var(--bg-elevated);
}
</style>
