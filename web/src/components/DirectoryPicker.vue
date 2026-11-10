<script setup lang="ts">
/**
 * 下载页共用的目录选择器（点选代替手填子路径）。
 *
 * 刻意做成**内联面板**而不是浮层：新建下载本身已经是 Teleport 浮层，再叠一层
 * 就要多压一条 useBackClose 哨兵，物理返回键的行为会跟着变。这里只管
 * 「浏览 / 新建文件夹 / 选定」，选中的绝对路径交给父组件决定怎么用。
 */
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import { filesApi } from '../api/files'
import type { DirChoice, DirRoot } from '../lib/dir-picker'

const props = defineProps<{
  /** 可切换的起点；只有一个时隐藏下拉 */
  roots: DirRoot[]
  initialId: string
  /** 起始目录（绝对路径），越界时回退到上界 */
  startPath?: string
  /** 浏览上界；不传 = 当前起点的 path */
  basePath?: string
}>()

const emit = defineEmits<{
  (e: 'select', choice: DirChoice): void
  (e: 'cancel'): void
}>()

const { t } = useI18n()

const activeId = ref(props.initialId || props.roots[0]?.id || '')
const activeRoot = computed(
  () => props.roots.find((item) => item.id === activeId.value) ?? props.roots[0] ?? null,
)

/** 统一成 POSIX 风格去尾斜杠：后端返回的路径在 Windows 上可能带反斜杠 */
function norm(value: string): string {
  return value.replace(/\\/g, '/').replace(/\/+$/, '')
}

const base = computed(() => norm(props.basePath || activeRoot.value?.path || ''))

/** 大小写不敏感（Windows），且必须卡在路径段边界上，防止 `C:/a` 放行 `C:/ab` */
function within(path: string, root: string = base.value): boolean {
  if (!root) return true
  const lower = norm(path).toLowerCase()
  const upper = root.toLowerCase()
  return lower === upper || lower.startsWith(upper + '/')
}

const current = ref('')
const dirs = ref<{ name: string; path: string }[]>([])
const loading = ref(false)
const error = ref('')
const creatingOpen = ref(false)
const newName = ref('')
const creating = ref(false)

/** 面包屑第一段是起点名，后面按当前目录逐段拆 */
const crumbs = computed(() => {
  const root = base.value
  const out = [{ label: activeRoot.value?.name ?? t('downloads.picker.root'), path: root }]
  const cur = norm(current.value)
  if (!root || cur.length <= root.length) return out
  let acc = root
  for (const part of cur.slice(root.length).split('/').filter(Boolean)) {
    acc = `${acc}/${part}`
    out.push({ label: part, path: acc })
  }
  return out
})

function failText(err: unknown): string {
  return err instanceof Error ? err.message : t('downloads.picker.failed')
}

async function browse(path: string) {
  const target = norm(path) || base.value
  if (!within(target)) {
    error.value = t('downloads.picker.outside')
    return
  }
  loading.value = true
  error.value = ''
  try {
    const data = await filesApi.list(target, 1, false, 'name', 'asc')
    // 后端可能回 canonical 后的写法；万一越界就保留请求路径，别把用户甩出上界
    current.value = within(data.path) ? norm(data.path) : target
    dirs.value = data.entries
      .filter((entry) => entry.is_dir && within(entry.path))
      .map((entry) => ({ name: entry.name, path: entry.path }))
  } catch (err) {
    error.value = failText(err)
    dirs.value = []
  } finally {
    loading.value = false
  }
}

function chooseMount(event: Event) {
  activeId.value = (event.target as HTMLSelectElement).value
  creatingOpen.value = false
  newName.value = ''
}

async function createFolder() {
  const name = newName.value.trim()
  if (!name || creating.value) return
  if (/[\\/]/.test(name) || name === '.' || name === '..') {
    error.value = t('downloads.picker.invalidName')
    return
  }
  creating.value = true
  error.value = ''
  try {
    const result = await filesApi.mkdir(current.value, name)
    newName.value = ''
    creatingOpen.value = false
    // 建完直接进新目录，用户点「选择此目录」即可；后端 setRoots 也会 create_dir_all
    await browse(within(result.path) ? result.path : current.value)
  } catch (err) {
    error.value = failText(err)
  } finally {
    creating.value = false
  }
}

function confirm() {
  const mountRoot = norm(activeRoot.value?.path ?? base.value)
  const cur = norm(current.value)
  const prefix = mountRoot.toLowerCase()
  const rel =
    cur.toLowerCase() === prefix
      ? ''
      : cur.toLowerCase().startsWith(prefix + '/')
        ? cur.slice(mountRoot.length).replace(/^\/+/, '')
        : ''
  emit('select', { path: current.value, mountId: activeId.value, rel })
}

onMounted(() => {
  const start = norm(props.startPath || '')
  current.value = start && within(start) ? start : base.value
  void browse(current.value)
})

// 切换起点等价于换一棵树：watch 统一负责重置 + 重新拉列表
watch(base, (next) => {
  current.value = next
  creatingOpen.value = false
  void browse(next)
})
</script>

<template>
  <div class="mt-2 rounded-lg border border-line bg-elevated p-2" data-testid="dir-picker">
    <div class="flex items-center gap-1.5">
      <select
        v-if="roots.length > 1"
        :value="activeId"
        class="min-w-0 flex-1 rounded border border-line bg-panel px-2 py-1 text-[11px] text-ink outline-none focus:border-accent"
        data-testid="dir-picker-mount"
        @change="chooseMount"
      >
        <option v-for="root in roots" :key="root.id" :value="root.id">{{ root.name }}</option>
      </select>
      <span v-else class="min-w-0 flex-1 truncate text-[11px] text-ink">
        {{ roots[0]?.name ?? t('downloads.picker.root') }}
      </span>
      <button
        type="button"
        class="shrink-0 rounded border border-line px-2 py-1 text-[11px] text-muted transition-colors hover:text-ink"
        data-testid="dir-picker-cancel"
        @click="emit('cancel')"
      >
        {{ t('downloads.picker.cancel') }}
      </button>
    </div>

    <nav
      class="mt-2 flex flex-wrap items-center gap-x-0.5 gap-y-1 rounded border border-line bg-panel px-2 py-1.5 text-[11px]"
      data-testid="dir-picker-crumbs"
    >
      <template v-for="(crumb, index) in crumbs" :key="crumb.path">
        <span v-if="index > 0" class="text-muted opacity-50">/</span>
        <button
          type="button"
          class="max-w-[140px] truncate rounded px-1 transition-colors hover:text-ink"
          :class="index === crumbs.length - 1 ? 'font-medium text-accent' : 'text-muted'"
          :title="crumb.path"
          data-testid="dir-picker-crumb"
          @click="browse(crumb.path)"
        >
          {{ crumb.label }}
        </button>
      </template>
    </nav>

    <div class="mt-1 max-h-40 overflow-y-auto rounded border border-line bg-panel">
      <button
        v-for="dir in dirs"
        :key="dir.path"
        type="button"
        class="flex w-full items-center gap-2 px-2.5 py-1.5 text-left text-[11px] text-ink transition-colors hover:bg-elevated"
        data-testid="dir-picker-dir"
        @click="browse(dir.path)"
      >
        <AppIcon name="folder" :size="13" class="shrink-0 text-muted" />
        <span class="truncate">{{ dir.name }}</span>
        <AppIcon name="chevronRight" :size="11" class="ml-auto shrink-0 text-muted opacity-50" />
      </button>
      <p v-if="loading" class="px-2.5 py-2 text-[11px] text-muted">
        {{ t('common.action.loading') }}
      </p>
      <p v-else-if="!dirs.length" class="px-2.5 py-2 text-[11px] text-muted">
        {{ t('downloads.picker.empty') }}
      </p>
    </div>

    <p v-if="error" class="mt-1 text-[11px] fg-danger">{{ error }}</p>

    <div v-if="creatingOpen" class="mt-2 flex items-center gap-1.5">
      <input
        v-model="newName"
        type="text"
        :placeholder="t('downloads.picker.folderName')"
        class="min-w-0 flex-1 rounded border border-line bg-panel px-2 py-1 text-[11px] text-ink outline-none focus:border-accent"
        data-testid="dir-picker-new-name"
        @keyup.enter="createFolder"
      />
      <button
        type="button"
        class="shrink-0 rounded border border-line px-2 py-1 text-[11px] text-muted transition-colors hover:text-ink disabled:opacity-50"
        :disabled="creating || !newName.trim()"
        data-testid="dir-picker-create"
        @click="createFolder"
      >
        {{ creating ? t('common.action.loading') : t('downloads.picker.create') }}
      </button>
    </div>

    <div class="mt-2 flex items-center justify-between gap-2">
      <button
        v-if="!creatingOpen"
        type="button"
        class="text-[11px] text-muted underline-offset-2 transition-colors hover:text-ink hover:underline"
        data-testid="dir-picker-new"
        @click="creatingOpen = true"
      >
        + {{ t('downloads.picker.newFolder') }}
      </button>
      <span v-else />
      <button
        type="button"
        class="shrink-0 rounded bg-accent px-2.5 py-1 text-[11px] font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50"
        :disabled="loading || !current"
        data-testid="dir-picker-confirm"
        @click="confirm"
      >
        {{ t('downloads.picker.confirm') }}
      </button>
    </div>
  </div>
</template>
