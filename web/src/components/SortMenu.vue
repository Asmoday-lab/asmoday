<script setup lang="ts">
/**
 * 排序菜单（v4.9.0）。
 *
 * 原生 `<select>` 只有两个选项、没有方向、也没法做展开动画，这里换成自绘菜单：
 * 一行一个**维度**，只显示当前选中的那一行带翻转按钮 —— 选项不随「维度 × 方向」
 * 膨胀成八行，用户也不用先选维度再选方向。
 *
 * 父组件只传「哪些维度有意义」（照片网格没有「条目数」、文件夹墙没有意义差别），
 * 组件本身不认识业务，只负责把 `SortValue` 拼出来。
 */
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import type { SortValue } from '../api/media'

export type SortKey = 'mtime' | 'name' | 'size' | 'count'
type SortDir = 'asc' | 'desc'

const props = withDefaults(
  defineProps<{
    modelValue: SortValue
    /** 这个视图支持哪些维度，顺序即菜单顺序 */
    keys: SortKey[]
    /** 首次切到某个维度时用哪个方向 */
    defaults?: Partial<Record<SortKey, SortDir>>
  }>(),
  { defaults: () => ({}) },
)
const emit = defineEmits<{ (e: 'update:modelValue', value: SortValue): void }>()

const { t } = useI18n()

const FALLBACK_DIR: Record<SortKey, SortDir> = {
  mtime: 'desc',
  name: 'asc',
  size: 'desc',
  count: 'desc',
}

const current = computed<{ key: SortKey; dir: SortDir }>(() => {
  const [key, dir] = props.modelValue.split('_') as [SortKey, SortDir]
  return { key, dir }
})

/** 记住每个维度上次用的方向：来回切维度时不会被重置 */
const memory = ref<Partial<Record<SortKey, SortDir>>>({})

function isActive(key: SortKey): boolean {
  return current.value.key === key
}

function dirOf(key: SortKey): SortDir {
  if (isActive(key)) return current.value.dir
  return memory.value[key] ?? props.defaults[key] ?? FALLBACK_DIR[key]
}

function keyLabelOf(key: SortKey): string {
  return t(`gallery.sort.${key}`)
}

function dirLabelOf(key: SortKey): string {
  return t(`gallery.sort.${key}${dirOf(key) === 'desc' ? 'Desc' : 'Asc'}`)
}

function apply(key: SortKey, dir: SortDir) {
  memory.value = { ...memory.value, [key]: dir }
  emit('update:modelValue', `${key}_${dir}` as SortValue)
}

/** 点行：换维度（方向沿用上次/默认） */
function pick(key: SortKey) {
  apply(key, dirOf(key))
  open.value = false
}

/** 点当前行的翻转按钮：只换方向，菜单不关，方便连续对比 */
function flip() {
  apply(current.value.key, current.value.dir === 'desc' ? 'asc' : 'desc')
}

const open = ref(false)
const wrap = ref<HTMLElement | null>(null)
const triggerEl = ref<HTMLElement | null>(null)
const menuEl = ref<HTMLElement | null>(null)
/**
 * 菜单用 `fixed` + 运行时算位置，而不是 `absolute right-0`。
 *
 * 窄屏时触发按钮贴着屏幕左边，`right-0` 会把 192px 的菜单推到视口外（实测 x=-129）。
 * 这里按「优先右对齐、贴边就夹回来」算一次，再用 Teleport 挂到 body 上 ——
 * 免得被工具栏的 overflow 裁掉。
 */
const MENU_WIDTH = 192
const MENU_GAP = 4
const menuStyle = ref<Record<string, string>>({})

function placeMenu() {
  const trigger = triggerEl.value?.getBoundingClientRect()
  if (!trigger) return
  const maxLeft = window.innerWidth - MENU_WIDTH - 8
  const left = Math.min(Math.max(8, trigger.right - MENU_WIDTH), Math.max(8, maxLeft))
  const below = trigger.bottom + MENU_GAP
  // 下方空间不够就翻到按钮上方（窄屏 + 键盘弹起时很常见）
  const estimated = 40 + props.keys.length * 34
  const top = below + estimated > window.innerHeight ? Math.max(8, trigger.top - MENU_GAP - estimated) : below
  menuStyle.value = {
    left: `${Math.round(left)}px`,
    top: `${Math.round(top)}px`,
    width: `${MENU_WIDTH}px`,
  }
}

async function toggle() {
  if (open.value) {
    open.value = false
    return
  }
  open.value = true
  await nextTick()
  placeMenu()
}

function onPointerDown(event: PointerEvent) {
  if (!open.value) return
  const target = event.target
  if (!(target instanceof Node)) return
  if (wrap.value?.contains(target) || menuEl.value?.contains(target)) return
  open.value = false
}

function onKeydown(event: KeyboardEvent) {
  if (event.key === 'Escape' && open.value) {
    open.value = false
    event.stopPropagation()
  }
}

/** 滚动 / 缩放之后 fixed 菜单就和按钮对不上了，直接收起最省事 */
function onViewportChange() {
  if (open.value) open.value = false
}

onMounted(() => {
  document.addEventListener('pointerdown', onPointerDown, true)
  document.addEventListener('keydown', onKeydown)
  window.addEventListener('scroll', onViewportChange, true)
  window.addEventListener('resize', onViewportChange)
})

onBeforeUnmount(() => {
  document.removeEventListener('pointerdown', onPointerDown, true)
  document.removeEventListener('keydown', onKeydown)
  window.removeEventListener('scroll', onViewportChange, true)
  window.removeEventListener('resize', onViewportChange)
})
</script>

<template>
  <div ref="wrap" class="relative">
    <button
      ref="triggerEl"
      type="button"
      class="flex items-center gap-1.5 rounded-lg border border-line px-2 py-1 text-xs transition-colors hover:border-line hover:bg-elevated hover:text-ink"
      :class="open ? 'border-accent/40 text-ink' : 'text-muted'"
      :aria-expanded="open"
      :title="t('gallery.sort.button', { key: keyLabelOf(current.key), dir: dirLabelOf(current.key) })"
      data-testid="sort-trigger"
      @click="toggle"
    >
      <AppIcon name="sort" :size="13" />
      <span class="hidden sm:inline">{{ keyLabelOf(current.key) }}</span>
      <!-- 方向用一个小三角表达：旋转 180° 就是升序，不必再占两个字的位置 -->
      <svg
        class="size-2.5 transition-transform duration-200"
        :class="current.dir === 'asc' ? 'rotate-180' : ''"
        viewBox="0 0 8 8"
        aria-hidden="true"
      >
        <path d="M4 7 1 3h6z" fill="currentColor" />
      </svg>
    </button>

    <Teleport to="body">
      <Transition name="sort-pop">
        <div
          v-if="open"
          ref="menuEl"
          class="fixed z-50 rounded-lg border border-line bg-panel p-1 text-xs shadow-lg"
          :style="menuStyle"
          data-testid="sort-menu"
        >
          <p class="px-2 pt-1 pb-1.5 text-[11px] text-muted">{{ t('gallery.sort.title') }}</p>
          <div
            v-for="key in keys"
            :key="key"
            class="flex items-center rounded transition-colors hover:bg-elevated"
            :class="isActive(key) ? 'text-ink' : 'text-muted'"
          >
            <button
              type="button"
              class="flex min-w-0 flex-1 items-center gap-2 rounded px-2 py-1.5 text-left"
              :data-sort-key="key"
              @click="pick(key)"
            >
              <AppIcon v-if="isActive(key)" name="check" :size="12" class="text-accent" />
              <span v-else class="size-3" aria-hidden="true" />
              <span class="flex-1 truncate">{{ keyLabelOf(key) }}</span>
              <span class="shrink-0 text-[11px] text-muted">{{ dirLabelOf(key) }}</span>
            </button>
            <button
              v-if="isActive(key)"
              type="button"
              class="mr-1 grid size-6 shrink-0 place-items-center rounded text-muted transition-colors hover:bg-panel hover:text-ink"
              :title="t('gallery.sort.flip')"
              data-testid="sort-flip"
              @click.stop="flip"
            >
              <svg
                class="size-2.5 transition-transform duration-200"
                :class="current.dir === 'asc' ? 'rotate-180' : ''"
                viewBox="0 0 8 8"
                aria-hidden="true"
              >
                <path d="M4 7 1 3h6z" fill="currentColor" />
              </svg>
            </button>
          </div>
        </div>
      </Transition>
    </Teleport>
  </div>
</template>

<style scoped>
.sort-pop-enter-active,
.sort-pop-leave-active {
  transition:
    opacity 0.14s ease,
    transform 0.14s cubic-bezier(0.2, 0, 0, 1);
}

.sort-pop-enter-from,
.sort-pop-leave-to {
  opacity: 0;
  transform: translateY(-4px) scale(0.97);
}

@media (prefers-reduced-motion: reduce) {
  .sort-pop-enter-active,
  .sort-pop-leave-active {
    transition: none;
  }
}
</style>
