<script setup lang="ts">
/**
 * 全局右键菜单（设计文档 3.2.5 / 8.4.13）。
 *
 * 文件管理、照片灯箱、影视库海报墙、音乐列表**必须**复用这一个组件，
 * 媒体库不允许出现「只能看不能操作」的孤立页面。
 *
 * - 定位用 viewport 坐标（clientX/clientY），Teleport 到 body，避开父级 overflow 裁剪；
 * - 首次渲染后按实际尺寸把菜单夹回视口内（8px 安全边距）；
 * - Esc / 点击遮罩 / 滚动 都关闭；置灰项带 `hint` 当 Tooltip（只读挂载点场景）。
 */
import { useEventListener, useMediaQuery } from '@vueuse/core'
import { computed, nextTick, ref, watch } from 'vue'
import AppIcon from './AppIcon.vue'
import type { IconName } from './icons'

export interface ContextMenuItem {
  id: string
  label: string
  icon?: IconName
  /** 危险动作（红色，删除类） */
  danger?: boolean
  disabled?: boolean
  /** 置灰原因，渲染成 title（浏览器对 disabled 按钮不弹 Tooltip，父级 span 承接） */
  hint?: string
  /** 在这一项之前画一条分隔线 */
  divider?: boolean
}

const props = defineProps<{
  open: boolean
  x: number
  y: number
  items: ContextMenuItem[]
  /** 移动端底部面板的标题（通常是目标名字，让用户确认操作对象） */
  title?: string
}>()

const emit = defineEmits<{
  (e: 'select', id: string): void
  (e: 'close'): void
}>()

/**
 * 窄屏（手机）改成**底部动作面板**：右键在触屏上根本不存在，靠「…」按钮唤起，
 * 面板贴底 = 拇指可及区，条目高度 48px 也才够点。宽屏保持跟随光标的浮层。
 */
const isSheet = useMediaQuery('(max-width: 639.98px)')

const root = ref<HTMLElement | null>(null)
const pos = ref({ x: props.x, y: props.y })

const margin = 8

/** 菜单实际高度在渲染前未知，先按请求坐标摆一次，再按 rect 夹回视口 */
async function clamp() {
  pos.value = { x: props.x, y: props.y }
  if (isSheet.value) return
  await nextTick()
  const el = root.value
  if (!el) return
  const rect = el.getBoundingClientRect()
  pos.value = {
    x: Math.min(Math.max(margin, props.x), Math.max(margin, window.innerWidth - rect.width - margin)),
    y: Math.min(
      Math.max(margin, props.y),
      Math.max(margin, window.innerHeight - rect.height - margin),
    ),
  }
}

watch(
  () => [props.open, props.x, props.y, props.items.length] as const,
  () => {
    if (props.open) void clamp()
  },
  { immediate: true },
)

function close() {
  emit('close')
}

function pick(item: ContextMenuItem) {
  if (item.disabled) return
  emit('select', item.id)
}

function onKey(event: KeyboardEvent) {
  // 监听常驻，只有菜单真的打开时才响应 Esc
  if (!props.open) return
  if (event.key === 'Escape') close()
}

useEventListener(window, 'keydown', onKey)

const style = computed(() => (isSheet.value ? undefined : { left: `${pos.value.x}px`, top: `${pos.value.y}px` }))
</script>

<template>
  <Teleport to="body">
    <div
      v-if="open"
      class="fixed inset-0 z-[90]"
      :class="isSheet ? 'bg-black/40 backdrop-blur-[2px]' : ''"
      data-testid="context-menu-backdrop"
      @click="close"
      @contextmenu.prevent="close"
    />
    <div
      v-if="open"
      ref="root"
      class="fixed z-[91] overflow-x-hidden overflow-y-auto border-line bg-panel shadow-xl"
      :class="
        isSheet
          ? 'inset-x-0 bottom-0 max-h-[72dvh] rounded-t-2xl border-t pt-1'
          : 'max-h-[calc(100dvh-16px)] w-48 rounded-lg border py-1'
      "
      :style="style"
      role="menu"
      data-testid="context-menu"
      :data-sheet="isSheet ? '1' : undefined"
    >
      <!-- 底部面板：先给一条抓手 + 目标名，避免「这些操作到底作用在谁身上」 -->
      <template v-if="isSheet">
        <div class="flex items-center justify-center pt-1.5 pb-1">
          <span class="h-1 w-9 rounded-full bg-line" />
        </div>
        <p
          v-if="title"
          class="truncate border-b border-line/60 px-4 pb-2.5 text-[13px] font-medium text-ink"
        >
          {{ title }}
        </p>
      </template>

      <template v-for="item in items" :key="item.id">
        <div v-if="item.divider" class="my-1 border-t border-line" />
        <span :title="item.disabled ? item.hint : undefined" class="block">
          <button
            type="button"
            role="menuitem"
            class="menu-item"
            :class="[
              item.danger ? 'menu-item--danger' : '',
              item.disabled ? 'menu-item--off' : '',
              isSheet ? 'menu-item--sheet' : '',
            ]"
            :disabled="item.disabled"
            :data-menu="item.id"
            @click="pick(item)"
          >
            <AppIcon v-if="item.icon" :name="item.icon" :size="isSheet ? 17 : 14" />
            {{ item.label }}
          </button>
        </span>
      </template>

      <!-- 置灰项在部分浏览器上不弹原生 Tooltip，这里再给一条常驻说明 -->
      <p
        v-if="items.some((item) => item.disabled && item.hint)"
        class="px-3 pt-1 pb-1.5 text-[11px] leading-snug text-muted"
      >
        {{ items.find((item) => item.disabled && item.hint)?.hint }}
      </p>

      <div v-if="isSheet" class="px-3 pt-1" style="padding-bottom: env(safe-area-inset-bottom, 0px)">
        <button
          type="button"
          class="mb-2 h-11 w-full rounded-xl bg-elevated text-[13px] text-muted"
          @click="close"
        >
          {{ $t('common.action.cancel') }}
        </button>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
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
/* 底部面板：拇指点得中的高度（48px），左对齐、图标更大 */
.menu-item--sheet {
  min-height: 48px;
  padding: 0 18px;
  font-size: 14px;
  gap: 12px;
}
.menu-item--sheet:active {
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
