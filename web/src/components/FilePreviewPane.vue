<script setup lang="ts">
/**
 * 文件预览面板（设计文档 3.2「文件预览矩阵」/ 8.4.8）。
 *
 * - Markdown：`import()` 按需拉 `marked` + `DOMPurify`，渲染后**必须**过 XSS 过滤；
 *   顶部「编辑源码」交给外层切到 CodeMirror（不在这里重复造编辑器）；
 * - CSV/TSV：体积 >5MB 只取前 512KB，行数 >1000 行截断，并提示下载；
 * - PDF：浏览器原生 `<iframe src="raw">`，零体积开销；
 * - Office / CAD / 压缩包等重型格式：**不引入任何解析库**，只弹提示 + 下载。
 */
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import { filesApi, type EntryInfo } from '../api/files'
import { CSV_MAX_ROWS, parseCsv, type CsvTable } from '../lib/csv'
import { formatSize } from '../lib/format'

const props = defineProps<{ entry: EntryInfo }>()
const emit = defineEmits<{ (e: 'close'): void; (e: 'edit'): void }>()

const { t } = useI18n()

/** 超过这个体积的 CSV 只取前缀解析（设计文档 8.4.8） */
const CSV_SIZE_LIMIT = 5 * 1024 * 1024
const CSV_PREFIX_BYTES = 512 * 1024

const OFFICE_EXTS = [
  'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'odt', 'ods', 'odp', 'rtf', 'pages', 'numbers', 'key',
]
const CAD_EXTS = ['dwg', 'dxf', 'step', 'stp', 'iges', 'igs', 'skp', 'blend']
const ARCHIVE_EXTS = ['zip', 'rar', '7z', 'tar', 'gz', 'bz2', 'xz', 'zst', 'iso']

type PreviewKind = 'markdown' | 'csv' | 'pdf' | 'unsupported'

const kind = computed<PreviewKind>(() => {
  const ext = (props.entry.ext || '').toLowerCase()
  if (ext === 'md' || ext === 'markdown') return 'markdown'
  if (ext === 'csv' || ext === 'tsv') return 'csv'
  if (ext === 'pdf') return 'pdf'
  return 'unsupported'
})

const loading = ref(false)
const error = ref('')
const markdownHtml = ref('')
const table = ref<CsvTable | null>(null)
const oversized = ref(false)

const unsupportedReason = computed(() => {
  const ext = (props.entry.ext || '').toLowerCase()
  if (OFFICE_EXTS.includes(ext)) return t('files.preview.office')
  if (CAD_EXTS.includes(ext)) return t('files.preview.cad')
  if (ARCHIVE_EXTS.includes(ext)) return t('files.preview.archive')
  return t('files.preview.binary')
})

/** TSV 用制表符分隔，其余按逗号 */
const delimiter = computed(() => (props.entry.ext.toLowerCase() === 'tsv' ? '\t' : ','))

/** 渲染 Markdown：marked 与 DOMPurify 都走动态 import（设计文档 2.3 代码分割） */
async function renderMarkdown(text: string): Promise<string> {
  const [{ marked }, dompurify] = await Promise.all([import('marked'), import('dompurify')])
  const purify = dompurify.default
  // 相对路径的图片（src=x 之类）渲染出来只会是裂图：只放行 http(s) / data 图片
  purify.addHook('afterSanitizeAttributes', (node) => {
    if (node.tagName === 'IMG') {
      const src = node.getAttribute('src') ?? ''
      if (!/^(https?:|data:image\/)/i.test(src)) node.remove()
    }
  })
  const html = await marked.parse(text, { gfm: true, breaks: false })
  // 渲染结果一律当不可信 HTML 处理（设计文档 3.1 / 8.4.8）
  const clean = purify.sanitize(html, { USE_PROFILES: { html: true } })
  purify.removeHook('afterSanitizeAttributes')
  return clean
}

async function load() {
  error.value = ''
  markdownHtml.value = ''
  table.value = null
  oversized.value = false

  if (kind.value === 'unsupported' || kind.value === 'pdf') return

  loading.value = true
  try {
    if (kind.value === 'markdown') {
      markdownHtml.value = await renderMarkdown(await filesApi.readText(props.entry.path))
    } else {
      oversized.value = props.entry.size > CSV_SIZE_LIMIT
      const text = oversized.value
        ? await filesApi.readTextPrefix(props.entry.path, CSV_PREFIX_BYTES)
        : await filesApi.readText(props.entry.path)
      table.value = parseCsv(text, CSV_MAX_ROWS, delimiter.value)
    }
  } catch (err) {
    error.value = err instanceof Error ? err.message : t('files.preview.loadFailed')
  } finally {
    loading.value = false
  }
}

function download() {
  window.open(filesApi.rawUrl(props.entry.path), '_blank', 'noopener')
}

onMounted(load)
watch(() => props.entry.path, load)

defineExpose({ load })
</script>

<template>
  <div class="flex min-w-0 flex-1 flex-col">
    <header class="flex h-11 shrink-0 items-center gap-2 border-b border-line bg-panel px-3">
      <button
        class="flex items-center gap-1.5 rounded-md px-2 py-1 text-xs text-muted transition-colors hover:bg-elevated hover:text-ink"
        @click="emit('close')"
      >
        <AppIcon name="arrowLeft" :size="14" />
        {{ t('files.preview.back') }}
      </button>
      <p class="min-w-0 flex-1 truncate text-[13px] text-ink">
        {{ entry.name }}
        <span class="ml-2 text-[11px] text-muted">{{ formatSize(entry.size) }}</span>
      </p>

      <button
        v-if="kind === 'markdown'"
        class="flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-xs text-muted transition-colors hover:text-ink"
        @click="emit('edit')"
      >
        <AppIcon name="edit" :size="13" />
        {{ t('files.preview.editSource') }}
      </button>
      <button
        class="flex items-center gap-1.5 rounded-md px-2 py-1 text-xs text-muted transition-colors hover:bg-elevated hover:text-ink"
        @click="download"
      >
        <AppIcon name="download" :size="13" />
        {{ t('files.preview.download') }}
      </button>
    </header>

    <!-- Markdown 渲染预览 -->
    <div v-if="kind === 'markdown'" class="min-h-0 flex-1 overflow-y-auto px-5 py-4">
      <div v-if="loading" class="space-y-2">
        <div v-for="i in 8" :key="i" class="skeleton h-3 rounded" :style="{ width: `${95 - i * 7}%` }" />
      </div>
      <p v-else-if="error" class="text-xs text-red-400">{{ error }}</p>
      <!-- eslint-disable-next-line vue/no-v-html -- 已过 DOMPurify，设计文档 3.1 允许 -->
      <article v-else class="md-body" v-html="markdownHtml" />
    </div>

    <!-- CSV 只读表格 -->
    <div v-else-if="kind === 'csv'" class="flex min-h-0 flex-1 flex-col">
      <p
        v-if="oversized || table?.truncated"
        class="shrink-0 border-b border-amber-500/30 bg-amber-500/10 px-3 py-1.5 text-[11px] text-amber-500"
      >
        {{
          oversized
            ? t('files.preview.csvOversized', { size: formatSize(CSV_SIZE_LIMIT) })
            : t('files.preview.csvTruncated', { rows: CSV_MAX_ROWS })
        }}
        {{ t('files.preview.truncatedHint') }}
      </p>

      <div v-if="loading" class="space-y-2 p-4">
        <div v-for="i in 10" :key="i" class="skeleton h-4 rounded" />
      </div>
      <p v-else-if="error" class="p-4 text-xs text-red-400">{{ error }}</p>
      <div v-else-if="table" class="min-h-0 flex-1 overflow-auto">
        <table class="w-full border-collapse text-[12px]">
          <thead class="sticky top-0 bg-elevated">
            <tr>
              <th class="w-10 border-b border-line px-2 py-1.5 text-right text-[10px] font-normal text-muted">
                #
              </th>
              <th
                v-for="(cell, index) in table.rows[0] ?? []"
                :key="index"
                class="max-w-[240px] truncate border-b border-line px-2 py-1.5 text-left font-medium text-ink"
                :title="cell"
              >
                {{ cell }}
              </th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="(row, rowIndex) in table.rows.slice(1)"
              :key="rowIndex"
              class="odd:bg-panel/40 hover:bg-elevated/60"
            >
              <td class="border-b border-line/60 px-2 py-1 text-right text-[10px] text-muted">
                {{ rowIndex + 2 }}
              </td>
              <td
                v-for="(cell, cellIndex) in row"
                :key="cellIndex"
                class="max-w-[240px] truncate border-b border-line/60 px-2 py-1 text-ink"
                :title="cell"
              >
                {{ cell }}
              </td>
            </tr>
          </tbody>
        </table>
        <p v-if="table.rows.length <= 1" class="p-4 text-xs text-muted">
          {{ t('files.preview.csvEmpty') }}
        </p>
      </div>
    </div>

    <!-- PDF：浏览器原生渲染 -->
    <div v-else-if="kind === 'pdf'" class="min-h-0 flex-1 bg-elevated/40">
      <iframe
        :src="filesApi.rawUrl(entry.path)"
        class="size-full border-0"
        :title="entry.name"
      />
    </div>

    <!-- 不支持在线预览：只给提示 + 下载 -->
    <div v-else class="grid min-h-0 flex-1 place-items-center px-6 py-10 text-center">
      <div class="max-w-sm">
        <span class="mb-4 inline-grid size-14 place-items-center rounded-2xl bg-elevated text-muted">
          <AppIcon name="archive" :size="24" />
        </span>
        <p class="text-sm font-medium text-ink">{{ t('files.preview.unsupportedTitle') }}</p>
        <p class="mt-1 text-xs leading-relaxed text-muted">{{ unsupportedReason }}</p>
        <p class="mt-3 truncate text-[11px] text-muted/70">
          {{ entry.name }} · {{ formatSize(entry.size) }}
        </p>
        <button
          class="mt-5 inline-flex items-center gap-2 rounded-lg bg-accent px-4 py-2 text-xs font-medium text-white transition-opacity hover:opacity-90"
          @click="download"
        >
          <AppIcon name="download" :size="14" />
          {{ t('files.preview.download') }}
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
/* Markdown 正文：只做基础排版，视觉跟随主题变量（不引入 typography 插件） */
.md-body {
  max-width: 78ch;
  color: var(--text-main);
  font-size: 14px;
  line-height: 1.75;
  word-break: break-word;
}
.md-body :deep(h1),
.md-body :deep(h2),
.md-body :deep(h3),
.md-body :deep(h4) {
  margin: 1.4em 0 0.6em;
  font-weight: 600;
  line-height: 1.3;
}
.md-body :deep(h1) {
  font-size: 1.6em;
  border-bottom: 1px solid var(--border-color);
  padding-bottom: 0.3em;
}
.md-body :deep(h2) {
  font-size: 1.35em;
}
.md-body :deep(h3) {
  font-size: 1.15em;
}
.md-body :deep(p),
.md-body :deep(ul),
.md-body :deep(ol),
.md-body :deep(blockquote),
.md-body :deep(table),
.md-body :deep(pre) {
  margin: 0.8em 0;
}
.md-body :deep(ul),
.md-body :deep(ol) {
  padding-left: 1.4em;
}
.md-body :deep(ul) {
  list-style: disc;
}
.md-body :deep(ol) {
  list-style: decimal;
}
.md-body :deep(a) {
  color: var(--accent);
  text-decoration: underline;
}
.md-body :deep(code) {
  font-family: var(--font-app-mono);
  font-size: 0.9em;
  background: var(--bg-elevated);
  border-radius: 4px;
  padding: 0.1em 0.35em;
}
.md-body :deep(pre) {
  background: var(--bg-elevated);
  border: 1px solid var(--border-color);
  border-radius: 8px;
  padding: 0.9em 1em;
  overflow-x: auto;
}
.md-body :deep(pre code) {
  background: none;
  padding: 0;
  font-size: 0.85em;
}
.md-body :deep(blockquote) {
  border-left: 3px solid var(--accent);
  padding-left: 0.9em;
  color: var(--text-muted);
}
.md-body :deep(table) {
  border-collapse: collapse;
  width: 100%;
  font-size: 0.92em;
}
.md-body :deep(th),
.md-body :deep(td) {
  border: 1px solid var(--border-color);
  padding: 0.4em 0.6em;
  text-align: left;
}
.md-body :deep(img) {
  max-width: 100%;
  border-radius: 8px;
}
.md-body :deep(hr) {
  border: 0;
  border-top: 1px solid var(--border-color);
  margin: 1.4em 0;
}
</style>
