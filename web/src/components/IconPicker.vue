<script setup lang="ts">
/**
 * 图标选择器（v4.9.70）：按钮 + 浮层图标宫格。
 *
 * 原来是原生 `<select>` 列 icon 名字 —— 用户面对 `folderOpen`、`chevronLeft` 这类
 * 英文标识符根本不知道选的是什么。这里按 SortMenu 的同一套轻量浮层范式
 * （Teleport + fixed 定位 + 点外/Esc/滚动收起）自绘宫格，每个格子就是图标本身。
 *
 * 值域是任意字符串：配置源码模式可能写入图标集之外的值（或空），组件不丢值 ——
 * 触发按钮照常回显（未知值显示原文），宫格里只是没有高亮格子，选了才覆盖。
 *
 * 不压 useBackClose 哨兵：和 SortMenu 一样属于「一闪即收」的轻量菜单，
 * 物理返回键离开页面是预期行为，压哨兵反而多一步。
 */
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from 'vue'
import AppIcon from './AppIcon.vue'
import { ICONS, isIconName, type IconName } from './icons'

const props = withDefaults(
  defineProps<{
    modelValue: string
    /** 空值（= 默认图标）在触发按钮上显示的文字，由调用方传入 i18n 文案 */
    defaultLabel: string
    /** 触发按钮的可访问名 */
    ariaLabel?: string
  }>(),
  { ariaLabel: '' },
)
const emit = defineEmits<{ (e: 'update:modelValue', value: string): void }>()

/**
 * 宫格只放「当一张服务卡片的图标说得通」的图标（v4.9.70 从全集里筛过）：
 * 纯界面控件（箭头、对勾、汉堡、音量三档、实心播放族…）出现在卡片上没有语义，
 * 全集 70+ 枚摊开反而难找。图标集之外的存量值不受影响（见文件头注释）。
 */
const PICKABLE: IconName[] = [
  'home',
  'folder',
  'folderOpen',
  'gallery',
  'grid',
  'rows',
  'terminal',
  'gear',
  'users',
  'chip',
  'file',
  'fileText',
  'archive',
  'image',
  'video',
  'film',
  'audio',
  'disc',
  'play',
  'list',
  'search',
  'globe',
  'link',
  'share',
  'download',
  'upload',
  'shield',
  'lock',
  'eye',
  'info',
  'sparkle',
  'heart',
  'edit',
  'save',
  'refresh',
  'trash',
  'sun',
  'moon',
].filter((name): name is IconName => name in ICONS)

/** 触发按钮上的图标：合法名字直接渲染，未知值退回 grid（值本身不动） */
const triggerIcon = computed<IconName>(() =>
  isIconName(props.modelValue) ? props.modelValue : 'grid',
)
/** 空值显示「默认」；合法名字直接显示（帮助区分形近图标）；未知值显示原文，让用户知道现在是什么 */
const triggerLabel = computed(() =>
  props.modelValue ? props.modelValue : props.defaultLabel,
)

const open = ref(false)
const wrap = ref<HTMLElement | null>(null)
const triggerEl = ref<HTMLElement | null>(null)
const popEl = ref<HTMLElement | null>(null)

/**
 * 浮层 `fixed` + 运行时算位置（同 SortMenu）：触发按钮可能在窄屏贴边，
 * `absolute` 会被父容器 overflow 裁掉，`left-0` 会推出视口外。
 * 优先右对齐，贴边夹回来；下方放不下翻到上方。
 */
const POP_WIDTH = 300
const POP_GAP = 4
const popStyle = ref<Record<string, string>>({})

function placePop() {
  const trigger = triggerEl.value?.getBoundingClientRect()
  if (!trigger) return
  const maxLeft = window.innerWidth - POP_WIDTH - 8
  const left = Math.min(Math.max(8, trigger.right - POP_WIDTH), Math.max(8, maxLeft))
  const below = trigger.bottom + POP_GAP
  const estimated = 292
  const top = below + estimated > window.innerHeight ? Math.max(8, trigger.top - POP_GAP - estimated) : below
  popStyle.value = {
    left: `${Math.round(left)}px`,
    top: `${Math.round(top)}px`,
    width: `${POP_WIDTH}px`,
  }
}

async function toggle() {
  if (open.value) {
    open.value = false
    return
  }
  open.value = true
  await nextTick()
  placePop()
}

function pick(value: string) {
  emit('update:modelValue', value)
  open.value = false
}

function onPointerDown(event: PointerEvent) {
  if (!open.value) return
  const target = event.target
  if (!(target instanceof Node)) return
  if (wrap.value?.contains(target) || popEl.value?.contains(target)) return
  open.value = false
}

function onKeydown(event: KeyboardEvent) {
  if (event.key === 'Escape' && open.value) {
    open.value = false
    event.stopPropagation()
  }
}

/** 滚动 / 缩放后 fixed 浮层与按钮错位，直接收起 */
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
  <div ref="wrap" class="relative inline-block">
    <button
      ref="triggerEl"
      type="button"
      class="flex items-center gap-1.5 rounded-lg border border-line bg-elevated px-2 py-1.5 text-xs transition-colors hover:border-accent/40 hover:text-ink"
      :class="open ? 'border-accent/40 text-ink' : 'text-muted'"
      :aria-expanded="open"
      :aria-label="props.ariaLabel || undefined"
      :data-value="props.modelValue"
      data-testid="service-icon"
      @click="toggle"
    >
      <AppIcon :name="triggerIcon" :size="14" />
      <span class="max-w-24 truncate font-mono text-[11px]">{{ triggerLabel }}</span>
      <svg
        class="size-2.5 text-muted transition-transform duration-200"
        :class="open ? 'rotate-180' : ''"
        viewBox="0 0 8 8"
        aria-hidden="true"
      >
        <path d="M4 7 1 3h6z" fill="currentColor" />
      </svg>
    </button>

    <Teleport to="body">
      <Transition name="icon-pop">
        <div
          v-if="open"
          ref="popEl"
          class="fixed z-50 rounded-lg border border-line bg-panel p-2 shadow-lg"
          :style="popStyle"
          data-testid="service-icon-pop"
        >
          <div class="grid grid-cols-8 gap-1">
            <!-- 默认（清空自定义图标） -->
            <button
              type="button"
              class="grid h-8 place-items-center rounded text-muted transition-colors hover:bg-elevated hover:text-ink"
              :class="!props.modelValue ? 'bg-elevated text-ink ring-1 ring-accent/50' : ''"
              :title="props.defaultLabel"
              data-testid="service-icon-default"
              @click="pick('')"
            >
              <span class="font-mono text-[10px] leading-none">--</span>
            </button>
            <button
              v-for="name in PICKABLE"
              :key="name"
              type="button"
              class="grid h-8 place-items-center rounded text-muted transition-colors hover:bg-elevated hover:text-ink"
              :class="props.modelValue === name ? 'bg-elevated text-ink ring-1 ring-accent/50' : ''"
              :title="name"
              :data-icon="name"
              :data-testid="`service-icon-tile-${name}`"
              @click="pick(name)"
            >
              <AppIcon :name="name" :size="16" />
            </button>
          </div>
        </div>
      </Transition>
    </Teleport>
  </div>
</template>

<style scoped>
.icon-pop-enter-active,
.icon-pop-leave-active {
  transition:
    opacity 0.14s ease,
    transform 0.14s cubic-bezier(0.2, 0, 0, 1);
}

.icon-pop-enter-from,
.icon-pop-leave-to {
  opacity: 0;
  transform: translateY(-4px) scale(0.97);
}

@media (prefers-reduced-motion: reduce) {
  .icon-pop-enter-active,
  .icon-pop-leave-active {
    transition: none;
  }
}
</style>
