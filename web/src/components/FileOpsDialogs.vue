<script setup lang="ts">
/**
 * 文件操作弹窗：重命名 / 移动·复制 / 删除 / 属性（设计文档 3.2.5 / 3.2.6 / 6.2）。
 *
 * 为什么抽成独立组件：文件管理、照片库、影视库、音乐库都要能对**文件与文件夹**
 * 做同一套变更操作。以前这四套弹窗只活在 `FilesView` 里，媒体库于是变成
 * 「只能看不能改」的死水；各写一遍又会冒出四套冲突策略与确认口径。
 *
 * 调用方式：宿主持有 `<FileOpsDialogs ref="ops" />`，按需调用
 * `ops.openRename(target)` / `openTransfer(mode, targets, dest?)` /
 * `openDelete(targets)` / `openStat(target)`；操作成功后组件发 `done`，
 * 宿主据此刷新自己的列表（组件不碰宿主的列表状态）。
 *
 * 红线：
 * - 冲突必须由用户明确选择（`overwrite` / `rename` / `skip`），不预选、不替用户决定；
 * - 删目录要输入目录名（防误触，不是安全边界）；
 * - 批量的失败逐项展示后端 message，阻塞类错误只给指引不给「重试」。
 */
import { useEventListener } from '@vueuse/core'
import { computed, nextTick, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import { filesApi, summarizeResults, type FileStat, type MountPoint, type OnConflict } from '../api/files'
import { localizedCode } from '../lib/errors'
import { formatSize, formatTime } from '../lib/format'

/** 弹窗作用的对象：`EntryInfo`、媒体库条目、相册目录都结构兼容 */
export interface OpsTarget {
  name: string
  path: string
  is_dir: boolean
}

export type FileOpKind = 'rename' | 'move' | 'copy' | 'delete'

/** 操作完成：宿主据此刷新列表（组件不持有列表） */
export interface FileOpDone {
  kind: FileOpKind
  /** 操作前的源路径 */
  paths: string[]
  /** 重命名后的新路径 */
  newPath?: string
}

export interface FailureItem {
  path: string
  source?: string
  error_code?: string
  message?: string
}

export interface TransferSummary {
  ok: number
  failed: number
  skipped: number
  failures: FailureItem[]
}

const props = withDefaults(
  defineProps<{
    /** 移动/复制目标浏览器的起点（通常是当前所在目录） */
    startDir?: string
    /** 挂载点清单：展示成「挂载点名/子路径」，也是只读提示的口径来源 */
    roots?: MountPoint[]
  }>(),
  { startDir: '', roots: () => [] },
)

const emit = defineEmits<{
  (e: 'done', payload: FileOpDone): void
  (e: 'toast', message: string): void
}>()

const { t } = useI18n()

function failText(err: unknown): string {
  return err instanceof Error ? err.message : t('common.state.unknownError')
}

/** 点完按钮把焦点送回输入框并全选（键盘用户可以直接改） */
async function focusFieldLater(selector: string) {
  await nextTick()
  const field = document.querySelector<HTMLInputElement>(selector)
  field?.focus()
  field?.select()
}

// ---------------------------------------------------------------------------
// 路径展示：挂载点内只给「挂载点名/子路径」，不外泄服务端真实盘符
// ---------------------------------------------------------------------------

/** 路径 → 挂载点：最长前缀命中，避免 `/mnt/a` 与 `/mnt/ab` 互相误判 */
function mountForPath(path: string): MountPoint | null {
  const normalized = path.replace(/\\/g, '/')
  let matched: MountPoint | null = null
  let matchedLength = -1
  for (const root of props.roots) {
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

function displayPath(path: string): string {
  const normalized = path.replace(/\\/g, '/')
  const mount = mountForPath(path)
  if (mount) {
    const prefix = mount.path.replace(/\\/g, '/').replace(/\/+$/, '')
    const rest = normalized.slice(prefix.length).replace(/^\/+/, '')
    return rest ? `${mount.name}/${rest}` : mount.name
  }
  const parts = normalized
    .split('/')
    .filter(Boolean)
    .filter((part) => !/^[a-zA-Z]:$/.test(part))
  return parts.join('/') || path
}

// ---------------------------------------------------------------------------
// 重命名
// ---------------------------------------------------------------------------
const renameDialog = ref<{
  target: OpsTarget
  name: string
  busy: boolean
  error: string
} | null>(null)

function openRename(target: OpsTarget) {
  renameDialog.value = { target, name: target.name, busy: false, error: '' }
  void focusFieldLater('[data-testid="rename-name"]')
}

async function submitRename() {
  const dialog = renameDialog.value
  if (!dialog || dialog.busy) return
  const name = dialog.name.trim()
  if (!name || name === dialog.target.name) {
    renameDialog.value = null
    return
  }
  dialog.busy = true
  dialog.error = ''
  try {
    const result = await filesApi.rename(dialog.target.path, name)
    const oldPath = dialog.target.path
    renameDialog.value = null
    emit('toast', t('files.rename.done'))
    emit('done', { kind: 'rename', paths: [oldPath], newPath: result.new_path })
  } catch (err) {
    dialog.error = failText(err)
  } finally {
    dialog.busy = false
  }
}

// ---------------------------------------------------------------------------
// 移动 / 复制（冲突必须由用户解析成具体策略）
// ---------------------------------------------------------------------------
type TransferMode = 'move' | 'copy'

/** 冲突策略按固定顺序展示：overwrite 风险最高放最上面但不预选 */
const CONFLICT_OPTIONS: OnConflict[] = ['overwrite', 'rename', 'skip']
const conflictChoice = ref<OnConflict | 'ask'>('ask')

const transferDialog = ref<{
  mode: TransferMode
  targets: OpsTarget[]
  dest: string
  busy: boolean
  error: string
} | null>(null)

/**
 * 目标目录浏览器（v3.5.5：点选代替手填绝对路径）。
 * 起点 = 宿主当前目录（媒体库传相册/库根），`dest` 显式传入时以它为准。
 */
const transferPicker = ref<{
  path: string
  dirs: { name: string; path: string }[]
  loading: boolean
  error: string
  manual: boolean
} | null>(null)

function openTransfer(mode: TransferMode, targets: OpsTarget[], dest?: string) {
  if (!targets.length) return
  const start = (dest ?? props.startDir ?? '').trim()
  conflictChoice.value = 'ask'
  transferDialog.value = {
    mode,
    targets,
    // 默认目标 = 起始目录（用户改路径比从零输入快）
    dest: start,
    busy: false,
    error: '',
  }
  if (!start) {
    // 没有可浏览的起点（比如刚进媒体库根还没定位）：直接手填绝对路径
    transferPicker.value = null
    return
  }
  transferPicker.value = { path: start, dirs: [], loading: true, error: '', manual: false }
  void browsePicker(start)
}

/** 浏览器面包屑：挂载点名 + 相对段，点击逐级跳转 */
const pickerCrumbs = computed(() => {
  const picker = transferPicker.value
  if (!picker) return [] as { label: string; path: string }[]
  const normalized = picker.path.replace(/\\/g, '/')
  const mount = mountForPath(picker.path)
  const result: { label: string; path: string }[] = []
  if (mount) {
    const prefix = mount.path.replace(/\\/g, '/').replace(/\/+$/, '')
    result.push({ label: mount.name, path: mount.path })
    let acc = prefix
    for (const part of normalized.slice(prefix.length).split('/').filter(Boolean)) {
      acc = `${acc}/${part}`
      result.push({ label: part, path: acc })
    }
    return result
  }
  const parts = normalized.split('/').filter(Boolean)
  let acc = ''
  for (const part of parts) {
    acc = acc ? `${acc}/${part}` : part.endsWith(':') ? `${part}/` : part
    result.push({ label: part, path: acc })
  }
  return result
})

/** 浏览目标目录：只列子目录，选中即「当前所在」 */
async function browsePicker(path: string) {
  const picker = transferPicker.value
  if (!picker) return
  picker.loading = true
  picker.error = ''
  try {
    const data = await filesApi.list(path, 1, false, 'name', 'asc')
    picker.path = data.path
    picker.dirs = data.entries
      .filter((entry) => entry.is_dir)
      .map((entry) => ({ name: entry.name, path: entry.path }))
    if (transferDialog.value) transferDialog.value.dest = data.path
  } catch (err) {
    picker.error = failText(err)
  } finally {
    picker.loading = false
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
    transferPicker.value = null
    if (summary.failed === 0) emit('toast', t('files.transfer.done'))
    else showResult(dialog.mode, summary, dest, conflictChoice.value)
    emit('done', { kind: dialog.mode, paths })
  } catch (err) {
    dialog.error = failText(err)
  } finally {
    dialog.busy = false
  }
}

// ---------------------------------------------------------------------------
// 批量结果汇总（逐项展示后端 message，缺 message 才退回 error_code 翻译）
// ---------------------------------------------------------------------------
const resultDialog = ref<
  ({ mode: TransferMode; dest?: string; destAbs?: string; conflict: OnConflict } & TransferSummary) | null
>(null)

/** 结果弹窗的唯一入口：`destAbs` 是绝对路径，`des` 展示用（挂载点内相对） */
function showResult(
  mode: TransferMode,
  summary: TransferSummary,
  destAbs: string,
  conflict: OnConflict,
) {
  resultDialog.value = {
    mode,
    dest: destAbs ? displayPath(destAbs) : undefined,
    destAbs: destAbs || undefined,
    conflict,
    ...summary,
  }
}

/**
 * 这些错误码来自**外部状态**，立刻重试必然再次失败（设计文档 8.5 第 7 条）。
 * `E_SOURCE_DELETE_FAILED` 更特殊：数据其实已经写到目标了，重试等于复制第二份。
 */
const RETRY_BLOCKED_CODES = new Set([
  'E_BUSY',
  'E_CONFLICT',
  'E_DIR_CONFLICT',
  'E_SOURCE_DELETE_FAILED',
  'E_READONLY',
  'E_FORBIDDEN',
])

function canRetry(item: FailureItem): boolean {
  return !RETRY_BLOCKED_CODES.has(item.error_code ?? '')
}

/** 阻塞项不是给个「重试」就完事，必须告诉用户到底要动哪里 */
function blockedHint(item: FailureItem): string {
  switch (item.error_code) {
    case 'E_BUSY':
      return t('files.result.hintBusy')
    case 'E_CONFLICT':
    case 'E_DIR_CONFLICT':
      return t('files.result.hintConflict')
    case 'E_SOURCE_DELETE_FAILED':
      return t('files.result.hintSourceLeft')
    case 'E_READONLY':
    case 'E_FORBIDDEN':
      return t('files.result.hintReadonly')
    default:
      return ''
  }
}

const retrying = ref(false)

/** 只重试「可能自己好起来」的失败项（网络抖动、单点权限等） */
async function retryFailures() {
  const dialog = resultDialog.value
  if (!dialog || retrying.value) return
  const targets = dialog.failures.filter(canRetry)
  if (!targets.length || !dialog.destAbs) return
  retrying.value = true
  try {
    const call = dialog.mode === 'copy' ? filesApi.copy : filesApi.move
    const response = await call(
      targets.map((item) => item.path),
      dialog.destAbs,
      dialog.conflict,
    )
    const summary = summarizeResults(response)
    // 阻塞项没参与重试，必须原样留在列表里，否则用户会以为它们成功了
    const blocked = dialog.failures.filter((item) => !canRetry(item))
    const merged: TransferSummary = {
      ok: summary.ok,
      failed: summary.failed + blocked.length,
      skipped: summary.skipped,
      failures: [...summary.failures, ...blocked],
    }
    resultDialog.value = merged.failed
      ? {
          mode: dialog.mode,
          dest: dialog.dest,
          destAbs: dialog.destAbs,
          conflict: dialog.conflict,
          ...merged,
        }
      : null
    if (!merged.failed) emit('toast', t('files.transfer.done'))
    emit('done', {
      kind: dialog.mode,
      paths: targets.map((item) => item.path),
    })
  } catch (err) {
    emit('toast', failText(err))
  } finally {
    retrying.value = false
  }
}

function failureText(item: FailureItem) {
  // 后端 message 指向具体原因（如「文件正被其他程序占用」），
  // 比通用错误码文案更有用；只有 message 缺失时才退回错误码翻译。
  return item.message?.trim() || localizedCode(item.error_code) || t('common.state.unknownError')
}

// ---------------------------------------------------------------------------
// 删除（设计文档 3.2.6.5）
// ---------------------------------------------------------------------------
const deleteTargets = ref<OpsTarget[]>([])
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

/** 删除统计请求序号：ref 是深响应 proxy，与原始对象恒不相等，只能用序号判定 */
let deleteRequestSeq = 0

function openDelete(targets: OpsTarget[]) {
  if (!targets.length) return
  const seq = ++deleteRequestSeq
  deleteTargets.value = targets
  deleteConfirmName.value = ''
  deleteError.value = ''
  deleteSurvey.value = null
  void (async () => {
    try {
      const survey = await filesApi.measure(targets.map((entry) => entry.path))
      if (seq === deleteRequestSeq && deleteTargets.value.length) deleteSurvey.value = survey
    } catch {
      if (seq === deleteRequestSeq && deleteTargets.value.length) {
        deleteSurvey.value = { items: targets.length, bytes: 0, truncated: false }
      }
    }
  })()
}

function closeDelete() {
  deleteRequestSeq += 1
  deleteTargets.value = []
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
    const many = targets.length > 1
    const dir = Boolean(targets[0]?.is_dir)
    closeDelete()
    emit(
      'toast',
      t(many ? 'files.delete.doneBatch' : dir ? 'files.delete.doneDir' : 'files.delete.doneFile'),
    )
    emit('done', { kind: 'delete', paths })
  } catch (err) {
    deleteError.value = failText(err)
  } finally {
    deleteBusy.value = false
  }
}

// ---------------------------------------------------------------------------
// 属性（只读 stat）
// ---------------------------------------------------------------------------
const statDialog = ref<{
  target: OpsTarget
  data: FileStat | null
  busy: boolean
  error: string
} | null>(null)

let statRequestSeq = 0

function openStat(target: OpsTarget) {
  const seq = ++statRequestSeq
  statDialog.value = { target, data: null, busy: true, error: '' }
  void (async () => {
    try {
      const data = await filesApi.stat(target.path)
      if (seq === statRequestSeq && statDialog.value) {
        statDialog.value = { ...statDialog.value, data, busy: false }
      }
    } catch (err) {
      if (seq === statRequestSeq && statDialog.value) {
        statDialog.value = { ...statDialog.value, busy: false, error: failText(err) }
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
 * 属性页的「所在目录」：只显示挂载点内的相对路径，
 * 不展示真实路径 —— 既和顶部的完整路径重复，又暴露服务端盘符。
 */
const statRelativeDir = computed(() => {
  const raw = statDialog.value?.data?.path ?? statDialog.value?.target.path ?? ''
  if (!raw) return '/'
  const dir = raw.replace(/[\\/][^\\/]*$/, '') || '/'
  const normalized = dir.replace(/\\/g, '/')
  const mount = mountForPath(normalized)
  if (mount) {
    const prefix = mount.path.replace(/\\/g, '/').replace(/\/+$/, '')
    const rest = normalized.slice(prefix.length).replace(/^\/+/, '')
    return rest ? `/${rest}` : '/'
  }
  const parts = normalized.split('/').filter(Boolean).filter((part) => !/^[a-zA-Z]:$/.test(part))
  return parts.length ? `/${parts.join('/')}` : '/'
})

// ---------------------------------------------------------------------------
// 宿主接口：任一层开着 / 统一关闭（Esc 与系统返回键要用）
// ---------------------------------------------------------------------------
const isOpen = computed(() =>
  Boolean(
    renameDialog.value ||
      transferDialog.value ||
      deleteTargets.value.length ||
      resultDialog.value ||
      statDialog.value,
  ),
)

/** 关掉最上面那层，返回是否真的有关掉（宿主据此决定要不要 preventDefault） */
function closeAll(): boolean {
  if (resultDialog.value) {
    resultDialog.value = null
    return true
  }
  if (deleteTargets.value.length) {
    closeDelete()
    return true
  }
  if (renameDialog.value) {
    renameDialog.value = null
    return true
  }
  if (transferDialog.value) {
    transferDialog.value = null
    transferPicker.value = null
    return true
  }
  if (statDialog.value) {
    statDialog.value = null
    return true
  }
  return false
}

/**
 * Esc 关掉最上面那层。
 *
 * 媒体库没有文件管理那套全局键盘层，弹窗必须自己收键；文件管理那边也会先处理一次，
 * `closeAll()` 是幂等的，两边都调不会关两层。
 */
/**
 * Esc 关掉最上面那层。
 *
 * 媒体库没有文件管理那套全局键盘层，弹窗必须自己收键；文件管理那边也会先处理一次，
 * `closeAll()` 是幂等的，两边都调不会关两层。
 */
function onKey(event: KeyboardEvent) {
  if (event.key !== 'Escape' || !isOpen.value) return
  if (closeAll()) event.preventDefault()
}

useEventListener(window, 'keydown', onKey)

defineExpose({ openRename, openTransfer, openDelete, openStat, showResult, isOpen, closeAll })
</script>

<template>
  <!-- 重命名（设计文档 3.2.6.1） -->
  <div
    v-if="renameDialog"
    class="fixed inset-0 z-50 grid place-items-center modal-overlay bg-black/50 p-4"
    @click.self="renameDialog = null"
  >
    <div class="w-full max-w-sm modal-card rounded-2xl border border-line bg-panel p-5 shadow-xl">
      <p class="text-sm font-medium text-ink">{{ t('files.rename.title') }}</p>
      <p class="mt-0.5 break-all text-[12px] text-muted">{{ renameDialog.target.name }}</p>
      <label class="mt-4 block text-[12px] text-muted">
        {{ t('files.rename.label') }}
        <input
          v-model="renameDialog.name"
          type="text"
          class="mt-1 w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
          data-testid="rename-name"
          autofocus
          @keydown.enter="submitRename"
          @keydown.esc="renameDialog = null"
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

  <!-- 移动 / 复制（设计文档 3.2.6.2） -->
  <div
    v-if="transferDialog"
    class="fixed inset-0 z-50 grid place-items-center modal-overlay bg-black/50 p-4"
    @click.self="transferDialog = null"
  >
    <div class="w-full max-w-sm modal-card rounded-2xl border border-line bg-panel p-5 shadow-xl">
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
            ? displayPath(transferDialog.targets[0].path)
            : t('files.select.count', { count: transferDialog.targets.length })
        }}
      </p>

      <label class="mt-4 block text-[12px] text-muted">
        {{ t('files.transfer.destination') }}
        <template v-if="transferPicker && !transferPicker.manual">
          <nav
            v-if="pickerCrumbs.length"
            class="mt-1 flex flex-wrap items-center gap-x-0.5 gap-y-1 rounded-lg border border-line bg-elevated px-2 py-1.5 text-[12px]"
            data-testid="transfer-picker-crumb"
          >
            <template v-for="(crumb, index) in pickerCrumbs" :key="crumb.path">
              <span v-if="index > 0" class="text-muted opacity-50">/</span>
              <button
                class="max-w-[160px] truncate rounded px-1 transition-colors hover:text-ink"
                :class="index === pickerCrumbs.length - 1 ? 'font-medium text-accent' : 'text-muted'"
                :title="crumb.label"
                @click="browsePicker(crumb.path)"
              >
                {{ crumb.label }}
              </button>
            </template>
          </nav>
          <div class="mt-1 max-h-40 overflow-y-auto rounded-lg border border-line bg-elevated">
            <button
              v-for="dir in transferPicker.dirs"
              :key="dir.path"
              type="button"
              class="flex w-full items-center gap-2 px-3 py-1.5 text-[12px] text-ink transition-colors hover:bg-panel"
              @click="browsePicker(dir.path)"
            >
              <AppIcon name="folder" :size="14" class="shrink-0 text-muted" />
              <span class="truncate">{{ dir.name }}</span>
              <AppIcon name="chevronRight" :size="11" class="ml-auto shrink-0 text-muted opacity-50" />
            </button>
            <p
              v-if="!transferPicker.loading && !transferPicker.dirs.length"
              class="px-3 py-2 text-[11px] text-muted"
            >
              {{ t('files.transfer.emptyDir') }}
            </p>
            <p v-if="transferPicker.loading" class="px-3 py-2 text-[11px] text-muted">
              {{ t('common.action.loading') }}
            </p>
            <p v-if="transferPicker.error" class="px-3 py-2 text-[11px] fg-danger">
              {{ transferPicker.error }}
            </p>
          </div>
          <button
            type="button"
            class="mt-1 text-[11px] text-muted underline-offset-2 transition-colors hover:text-ink hover:underline"
            @click="transferPicker.manual = true"
          >
            {{ t('files.transfer.manualInput') }}
          </button>
        </template>
        <template v-else>
          <input
            v-model="transferDialog.dest"
            type="text"
            class="mt-1 w-full rounded-lg border border-line bg-elevated px-3 py-2 text-[12px] text-ink outline-none focus:border-accent"
            data-testid="transfer-dest"
            @keydown.enter="submitTransfer"
            @keydown.esc="transferDialog = null"
          />
          <button
            v-if="transferPicker?.manual"
            type="button"
            class="mt-1 block text-[11px] text-muted underline-offset-2 transition-colors hover:text-ink hover:underline"
            @click="transferPicker.manual = false; browsePicker(transferPicker.path)"
          >
            {{ t('files.transfer.backToBrowse') }}
          </button>
        </template>
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
  <div v-if="transferDialog?.busy" class="fixed inset-0 z-[60] grid place-items-center bg-black/60 p-4">
    <div class="rounded-xl border border-line bg-panel px-6 py-5 text-center shadow-xl">
      <div class="mx-auto size-6 animate-spin rounded-full border-2 border-line border-t-accent" />
      <p class="mt-3 text-xs text-ink">
        {{ t('files.transfer.busy', { count: transferDialog.targets.length }) }}
      </p>
    </div>
  </div>

  <!-- 批量结果汇总（设计文档 6.2） -->
  <div
    v-if="resultDialog"
    class="fixed inset-0 z-50 grid place-items-center modal-overlay bg-black/50 p-4"
    @click.self="resultDialog = null"
  >
    <div class="w-full max-w-sm modal-card rounded-2xl border border-line bg-panel p-5 shadow-xl">
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
      <p v-if="resultDialog.dest" class="mt-1 break-all text-[11px] text-muted">
        {{ t('files.result.targetDir') }}：{{ resultDialog.dest }}
      </p>
      <template v-if="resultDialog.failures.length">
        <p class="mt-4 text-[12px] font-medium text-muted">{{ t('files.result.failures') }}</p>
        <ul class="mt-1.5 max-h-48 space-y-1 overflow-y-auto">
          <li
            v-for="item in resultDialog.failures"
            :key="item.path"
            class="rounded-lg border border-line bg-elevated px-2.5 py-1.5"
          >
            <p class="break-all text-[12px] text-ink">{{ displayPath(item.source ?? item.path) }}</p>
            <p class="mt-0.5 text-[11px] fg-danger">{{ failureText(item) }}</p>
            <p v-if="blockedHint(item)" class="mt-0.5 text-[11px] text-muted">
              {{ blockedHint(item) }}
            </p>
          </li>
        </ul>
      </template>
      <div class="mt-5 flex justify-end gap-2">
        <!-- 只有全部是阻塞项时才给「忽略并继续」，否则用户会连能重试的一起丢掉 -->
        <button
          v-if="!resultDialog.failures.some(canRetry)"
          class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:bg-elevated hover:text-ink"
          @click="resultDialog = null"
        >
          {{ t('files.result.ignore') }}
        </button>
        <button
          v-else
          class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:bg-elevated hover:text-ink"
          :disabled="retrying"
          @click="retryFailures"
        >
          {{ retrying ? t('common.action.loading') : t('files.result.retry') }}
        </button>
        <button
          class="rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90"
          @click="resultDialog = null"
        >
          {{ t('common.action.close') }}
        </button>
      </div>
    </div>
  </div>

  <!-- 删除确认 -->
  <div
    v-if="deleteTargets.length"
    class="fixed inset-0 z-50 grid place-items-center modal-overlay bg-black/50 p-4"
    @click.self="closeDelete"
  >
    <div class="w-full max-w-sm modal-card rounded-2xl border border-line bg-panel p-5 shadow-xl">
      <p class="text-sm font-medium text-ink">{{ deleteTitle }}</p>
      <p v-if="deleteTargets.length === 1" class="mt-2 break-all text-[12px] text-muted">
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
      <p v-if="deleteSurvey?.truncated || (deleteSurvey?.items ?? 0) > 1000" class="mt-1 text-[12px] fg-warn">
        {{ t('files.delete.large') }}
      </p>

      <p class="mt-3 text-xs leading-relaxed fg-danger">
        {{
          t(
            deleteNeedsName || deleteTargets.length > 1
              ? 'files.delete.warningDir'
              : 'files.delete.warningFile',
          )
        }}
      </p>
      <label v-if="deleteNeedsName" class="mt-3 block text-[12px] text-muted">
        {{ t('files.delete.confirmNameLabel') }}
        <input
          v-model="deleteConfirmName"
          type="text"
          :placeholder="deleteTargets[0].name"
          class="mt-1 w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
          data-testid="delete-confirm-name"
          @keydown.enter="confirmDelete"
          @keydown.esc="closeDelete"
        />
        <span class="mt-1 block text-[11px] text-muted">{{ t('files.delete.confirmNameHint') }}</span>
      </label>
      <p v-if="deleteError" class="mt-3 text-xs fg-danger">{{ deleteError }}</p>
      <div class="mt-5 flex justify-end gap-2">
        <button
          class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
          @click="closeDelete"
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

  <!-- 属性 -->
  <div
    v-if="statDialog"
    class="fixed inset-0 z-50 grid place-items-center modal-overlay bg-black/50 p-4"
    @click.self="statDialog = null"
  >
    <div class="w-full max-w-md modal-card rounded-2xl border border-line bg-panel p-5 shadow-xl">
      <p class="text-sm font-medium text-ink">{{ t('files.stat.title') }}</p>
      <p class="mt-0.5 truncate text-[12px] text-muted">{{ statDialog.target.name }}</p>

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
        <div v-if="statDialog.data.width && statDialog.data.height" class="flex gap-3">
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
</template>
