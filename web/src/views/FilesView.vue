<script setup lang="ts">
/**
 * 文件管理（设计文档 3.2「极简无树」）：
 * - 入口页平铺展示所有挂载点卡片，不带常驻目录树，移动端也能最大化利用宽度；
 * - 进入挂载点后用面包屑（Home / 根名 / 子目录）回退与跳转；
 * - 网格/列表均虚拟化，超 1000 条按 has_more 分页续接；双击文本文件进内置编辑器。
 */
import { computed, defineAsyncComponent, onMounted, reactive, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useRouter } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import VirtualGrid from '../components/VirtualGrid.vue'
import VirtualList from '../components/VirtualList.vue'
import { LANGUAGES, detectLanguage, type LanguageKey } from '../api/languages'
import { ApiError, filesApi, type EntryInfo, type Listing, type RootEntry } from '../api/files'
import { messageFromBody } from '../lib/errors'
import { sharesApi } from '../api/shares'
import { thumbUrl } from '../api/media'
import { formatSize, formatTime } from '../lib/format'
import { useTheme } from '../composables/useTheme'
import { useScrollLock } from '../composables/useScrollLock'

// CodeMirror 内核与语言包都在这个异步 chunk 里，首次打开编辑器才下载
const CodeEditor = defineAsyncComponent(() => import('../components/CodeEditor.vue'))

const { isDark } = useTheme()
const { t } = useI18n()
const router = useRouter()

/** 右键菜单 */
const menu = ref<{ x: number; y: number; entry: EntryInfo } | null>(null)

function openMenu(event: MouseEvent, entry: EntryInfo) {
  menu.value = {
    x: Math.min(event.clientX, window.innerWidth - 184),
    y: Math.max(8, Math.min(event.clientY, window.innerHeight - 316)),
    entry,
  }
  selected.value = entry.path
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

// ---------------- 删除（设计文档 3.2「右键菜单」） ----------------
const deleteTarget = ref<EntryInfo | null>(null)
const deleteBusy = ref(false)
const deleteError = ref('')

function openDeleteDialog() {
  const entry = menu.value?.entry
  menu.value = null
  if (!entry) return
  deleteTarget.value = entry
  deleteError.value = ''
}

async function confirmDelete() {
  const entry = deleteTarget.value
  if (!entry || deleteBusy.value) return
  deleteBusy.value = true
  deleteError.value = ''
  try {
    await filesApi.remove(entry.path)
    deleteTarget.value = null
    if (editor.value?.path === entry.path) closeEditor()
    if (selected.value === entry.path) selected.value = null
    showToast(t(entry.is_dir ? 'files.delete.doneDir' : 'files.delete.doneFile'))
    await open(currentPath.value)
  } catch (err) {
    deleteError.value = err instanceof Error ? err.message : t('files.delete.failed')
  } finally {
    deleteBusy.value = false
  }
}

async function copyShareResult() {
  if (!shareResult.value) return
  try {
    await navigator.clipboard.writeText(shareResult.value.url)
    showToast(t('common.clipboard.linkCopied'))
  } catch {
    showToast(t('common.clipboard.denied'))
  }
}

const roots = ref<RootEntry[]>([])
const rootsImplicit = ref(false)
const currentPath = ref('')
const listing = ref<Listing | null>(null)
const loading = ref(true)
const loadingMore = ref(false)
const error = ref<{ status: number; message: string } | null>(null)
const selected = ref<string | null>(null)
const page = ref(1)

type ViewMode = 'grid' | 'list'
const viewMode = ref<ViewMode>(
  (localStorage.getItem('asmoday.files.view') as ViewMode | null) === 'list' ? 'list' : 'grid',
)

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
  let matched: RootEntry | null = null
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
  try {
    const data = await filesApi.roots()
    roots.value = data.roots
    rootsImplicit.value = data.implicit
    if (!data.roots.length) {
      error.value = null
    }
  } catch (err) {
    error.value = describe(err)
  } finally {
    loading.value = false
  }
}

/** 进入某个挂载点 */
function enterRoot(root: RootEntry) {
  if (!root.exists) {
    showToast(t('files.roots.missingToast'))
    return
  }
  if (editor.value) closeEditor()
  void open(root.path)
}

/** 面包屑首段的 Home：回到挂载点入口页 */
function goHome() {
  if (editor.value) closeEditor()
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
  try {
    const data = await filesApi.list(path, pageNo)
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
    const next = await filesApi.list(currentPath.value, page.value + 1)
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

function setView(mode: ViewMode) {
  viewMode.value = mode
  localStorage.setItem('asmoday.files.view', mode)
}

async function activate(entry: EntryInfo) {
  if (entry.is_dir) {
    await open(entry.path)
    return
  }
  if (entry.editable) {
    await openEditor(entry)
    return
  }
  window.open(filesApi.rawUrl(entry.path), '_blank', 'noopener')
}

function onEntryClick(entry: EntryInfo) {
  if (coarsePointer) {
    void activate(entry)
  } else {
    selected.value = entry.path
  }
}

async function openEditor(entry: EntryInfo) {
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
      const data = await filesApi.list(currentPath.value, 1)
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
  if (entry.is_dir) return 'text-amber-400'
  switch (entry.kind) {
    case 'image':
      return 'text-sky-400'
    case 'video':
      return 'text-violet-400'
    case 'audio':
      return 'text-emerald-400'
    case 'archive':
      return 'text-orange-400'
    case 'text':
      return 'text-indigo-300'
    default:
      return 'text-muted'
  }
}

// 分享弹窗 / 编辑器是模态层，打开时锁住列表滚动
useScrollLock(computed(() => Boolean(shareDialog.value || editor.value || deleteTarget.value)))

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
        <p class="min-w-0 flex-1 truncate text-[13px] text-ink">
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
          <kbd class="ml-1 rounded bg-black/25 px-1 text-[10px]">Ctrl+S</kbd>
        </button>
      </header>

      <p
        v-if="editor.error"
        class="border-b border-red-500/30 bg-red-500/10 px-3 py-1.5 text-xs text-red-400"
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

    <div v-else class="flex min-w-0 flex-1 flex-col">
      <!-- 挂载点入口（设计文档 3.2：极简无树，主区域平铺所有挂载点） -->
      <template v-if="!currentPath">
        <header class="flex h-11 shrink-0 items-center gap-2 border-b border-line bg-panel px-3">
          <span class="text-[13px] font-medium text-ink">{{ t('files.roots.title') }}</span>
          <span
            v-if="rootsImplicit"
            class="rounded bg-elevated px-1.5 py-0.5 text-[10px] text-muted"
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
              :class="root.exists ? '' : 'opacity-60'"
              :title="root.exists ? t('files.roots.enter') : t('files.roots.unavailable')"
              :data-root="root.name"
              @click="enterRoot(root)"
            >
              <span class="grid size-10 shrink-0 place-items-center rounded-lg bg-accent-soft text-accent">
                <AppIcon name="folder" :size="18" />
              </span>
              <span class="min-w-0 flex-1">
                <span class="block truncate text-sm font-medium text-ink">{{ root.name }}</span>
                <span class="mt-0.5 block truncate text-[11px] text-muted">
                  {{ root.exists ? t('files.roots.browse') : t('files.roots.unavailable') }}
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
          @click="open(currentPath)"
        >
          <AppIcon name="refresh" :size="15" />
        </button>

        <nav class="flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto text-[12px]">
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

        <div class="relative hidden w-40 shrink-0 sm:block md:w-56">
          <span class="absolute left-2 top-1/2 -translate-y-1/2 text-muted">
            <AppIcon name="search" :size="13" />
          </span>
          <input
            v-model="searchTerm"
            :placeholder="searching ? t('files.toolbar.searching') : t('files.toolbar.searchPlaceholder')"
            class="w-full rounded-full border border-line bg-elevated py-1 pl-7 pr-7 text-[11px] text-ink outline-none focus:border-accent"
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

        <button
          class="flex shrink-0 items-center gap-1.5 rounded-md bg-accent px-2.5 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90"
          :title="t('files.toolbar.uploadHint')"
          @click="pickFiles"
        >
          <AppIcon name="upload" :size="13" />
          <span class="hidden sm:inline">{{ t('files.toolbar.upload') }}</span>
        </button>
        <input ref="fileInput" type="file" multiple class="hidden" @change="onFilesPicked" />
      </header>

      <!-- 内容 -->
      <div class="relative min-h-0 flex-1 overflow-y-auto">
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
          :min-width="98"
          :gap="6"
          :ratio="1.42"
        >
          <template #default="{ item: entry }">
            <button
              type="button"
              class="group flex size-full flex-col items-center gap-1.5 rounded-xl border border-transparent px-1.5 py-2.5 transition-colors hover:border-line hover:bg-panel"
              :class="selected === entry.path ? 'border-accent/50 bg-panel' : ''"
              :title="entry.name"
              :data-entry="entry.name"
              :data-kind="entry.kind"
              @click="onEntryClick(entry)"
              @dblclick="activate(entry)"
              @contextmenu.prevent="openMenu($event, entry)"
            >
              <span
                class="grid size-11 place-items-center overflow-hidden rounded-lg bg-elevated/60 transition-transform group-hover:scale-105 md:size-12"
                :class="toneFor(entry)"
              >
                <img
                  v-if="entry.kind === 'image'"
                  :src="thumbUrl(entry.path, 160)"
                  class="size-full object-cover"
                  loading="lazy"
                  alt=""
                />
                <AppIcon v-else :name="iconFor(entry)" :size="22" />
              </span>
              <span class="w-full truncate text-center text-[11px] leading-tight text-ink">
                {{ entry.name }}
              </span>
            </button>
          </template>
        </VirtualGrid>

        <!-- 列表视图（定高虚拟列表） -->
        <VirtualList v-else :items="displayEntries" :row-height="40">
          <template #default="{ item: entry }">
            <div
              class="flex h-full cursor-default items-center gap-2 border-t border-line/60 px-3 text-[12px] transition-colors hover:bg-elevated/60"
              :class="selected === entry.path ? 'bg-accent-soft' : ''"
              :data-entry="entry.name"
              :data-kind="entry.kind"
              @click="onEntryClick(entry)"
              @dblclick="activate(entry)"
              @contextmenu.prevent="openMenu($event, entry)"
            >
              <span :class="toneFor(entry)"><AppIcon :name="iconFor(entry)" :size="15" /></span>
              <span class="min-w-0 flex-1 truncate text-ink">{{ entry.name }}</span>
              <span class="w-20 shrink-0 text-right text-muted whitespace-nowrap">
                {{ entry.is_dir ? '—' : formatSize(entry.size) }}
              </span>
              <span class="hidden w-36 shrink-0 text-right text-muted whitespace-nowrap sm:block">
                {{ formatTime(entry.modified) }}
              </span>
            </div>
          </template>
        </VirtualList>
      </div>

      <!-- 底栏 / 分页 -->
      <footer
        v-if="!loading && !error"
        class="flex h-9 shrink-0 items-center justify-between border-t border-line bg-panel px-3 text-[11px] text-muted"
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
        <button
          v-if="hasMore"
          class="rounded-md border border-line px-2 py-1 text-[11px] text-ink transition-colors hover:border-accent/50"
          :disabled="loadingMore"
          @click="loadMore"
        >
          {{ loadingMore ? t('common.action.loadingMore') : t('common.action.loadMore') }}
        </button>
      </footer>
      </template>
    </div>

    <!-- 右键菜单 -->
    <div v-if="menu" class="fixed inset-0 z-40" @click="menu = null" @contextmenu.prevent="menu = null" />
    <div
      v-if="menu"
      class="fixed z-50 w-44 overflow-hidden rounded-lg border border-line bg-panel py-1 shadow-xl"
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
      <button class="menu-item" @click="menuOpenTerminal">
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
      <button class="menu-item menu-item--danger" @click="openDeleteDialog">
        <AppIcon name="trash" :size="14" />
        {{ t('files.menu.delete') }}
      </button>
    </div>

    <!-- 删除确认 -->
    <div
      v-if="deleteTarget"
      class="fixed inset-0 z-50 grid place-items-center bg-black/50 p-4"
      @click.self="deleteTarget = null"
    >
      <div class="w-full max-w-sm rounded-2xl border border-line bg-panel p-5 shadow-xl">
        <p class="text-sm font-medium text-ink">
          {{ t(deleteTarget.is_dir ? 'files.delete.titleDir' : 'files.delete.titleFile') }}
        </p>
        <p class="mt-2 break-all text-[11px] text-muted">{{ deleteTarget.name }}</p>
        <p class="mt-3 text-xs leading-relaxed text-red-400">
          {{ t(deleteTarget.is_dir ? 'files.delete.warningDir' : 'files.delete.warningFile') }}
        </p>
        <p v-if="deleteError" class="mt-3 text-xs text-red-400">{{ deleteError }}</p>
        <div class="mt-5 flex justify-end gap-2">
          <button
            class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
            @click="deleteTarget = null"
          >
            {{ t('common.action.cancel') }}
          </button>
          <button
            class="rounded-lg bg-red-500 px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50"
            :disabled="deleteBusy"
            @click="confirmDelete"
          >
            {{ deleteBusy ? t('common.action.deleting') : t('files.delete.confirm') }}
          </button>
        </div>
      </div>
    </div>

    <!-- 分享弹窗 -->
    <div v-if="shareDialog" class="fixed inset-0 z-50 grid place-items-center bg-black/50 p-4" @click.self="shareDialog = null">
      <div class="w-full max-w-sm rounded-2xl border border-line bg-panel p-5 shadow-xl">
        <template v-if="!shareResult">
          <p class="text-sm font-medium text-ink">{{ t('files.share.create') }}</p>
          <p class="mt-0.5 truncate text-[11px] text-muted">
            {{ shareDialog.entry.name }}
          </p>

          <label class="mt-4 block text-[11px] text-muted">
            {{ t('files.share.password') }}
            <input
              v-model="shareForm.password"
              type="text"
              :placeholder="t('files.share.passwordPlaceholder')"
              class="mt-1 w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
            />
          </label>

          <label class="mt-3 block text-[11px] text-muted">
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

          <p v-if="shareError" class="mt-3 text-xs text-red-400">{{ shareError }}</p>

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
          <p class="mt-2 text-[11px] text-muted">
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
        <p class="mt-1 max-w-xs truncate text-[11px] text-muted">{{ currentPath }}</p>
      </div>
    </div>

    <!-- 上传进度 -->
    <div
      v-if="uploads.length"
      class="absolute right-3 bottom-12 z-40 w-64 space-y-2 rounded-xl border border-line bg-panel p-3 shadow-xl"
    >
      <p class="text-[11px] font-medium text-muted">
        {{ t('files.upload.queue', { count: uploads.length }) }}
      </p>
      <div v-for="task in uploads" :key="task.id" class="space-y-1">
        <div class="flex items-center gap-2 text-[11px]">
          <span class="min-w-0 flex-1 truncate text-ink">{{ task.name }}</span>
          <span
            :class="task.status === 'error' ? 'text-red-400' : 'text-muted'"
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
.menu-item {
  display: flex;
  width: 100%;
  align-items: center;
  gap: 8px;
  padding: 7px 12px;
  font-size: 12px;
  color: var(--text-main);
  transition: background-color 0.15s ease;
}
.menu-item:hover {
  background: var(--bg-elevated);
}
.menu-item--danger {
  color: #f87171;
}
.menu-item--danger:hover {
  background: rgb(248 113 113 / 0.12);
}
</style>
