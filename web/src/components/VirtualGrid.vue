<script setup lang="ts" generic="T">
/**
 * 定高虚拟网格：只渲染视口内的单元格。
 * 上万个媒体条目也不会把 DOM 撑爆（设计文档 3.2 / 3.4 的虚拟列表要求）。
 */
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'

const props = withDefaults(
  defineProps<{
    items: readonly T[]
    minWidth?: number
    gap?: number
    /** 单元格高宽比：1 = 正方形，0.75 = 4:3，0.5625 = 16:9 */
    ratio?: number
    overscan?: number
  }>(),
  { minWidth: 160, gap: 8, ratio: 1, overscan: 2 },
)

/**
 * 滚到距底 `THRESHOLD` 以内就通知父组件续页。
 *
 * 组件自己就是滚动容器（`h-full overflow-y-auto`），外面的哨兵永远可见、
 * 只会触发一次；判断必须落在真正的滚动源上。同一个底部窗口只触发一次，
 * 离开后重置，追加内容后重新检查（一页填不满视口时自动补页）。
 */
const emit = defineEmits<{ bottom: [] }>()
const THRESHOLD = 240
let bottomFired = false

const viewport = ref<HTMLElement | null>(null)
const width = ref(0)
const height = ref(600)
const scrollTop = ref(0)

let observer: ResizeObserver | null = null

const columns = computed(() =>
  Math.max(1, Math.floor((width.value + props.gap) / (props.minWidth + props.gap))),
)
const cellWidth = computed(() =>
  Math.max(0, (width.value - props.gap * (columns.value - 1)) / columns.value),
)
const rowHeight = computed(() => cellWidth.value * props.ratio + props.gap)
const rows = computed(() => Math.ceil(props.items.length / columns.value))
const totalHeight = computed(() => Math.max(0, rows.value * rowHeight.value - props.gap))

const visible = computed(() => {
  const rowH = rowHeight.value || 1
  const startRow = Math.max(0, Math.floor(scrollTop.value / rowH) - props.overscan)
  const endRow = Math.min(
    rows.value,
    Math.ceil((scrollTop.value + height.value) / rowH) + props.overscan,
  )

  const out: { index: number; item: T; style: Record<string, string> }[] = []
  for (let row = startRow; row < endRow; row += 1) {
    for (let col = 0; col < columns.value; col += 1) {
      const index = row * columns.value + col
      if (index >= props.items.length) break
      out.push({
        index,
        item: props.items[index],
        style: {
          position: 'absolute',
          left: `${col * (cellWidth.value + props.gap)}px`,
          top: `${row * rowH}px`,
          width: `${cellWidth.value}px`,
          height: `${cellWidth.value * props.ratio}px`,
          contain: 'layout paint',
        },
      })
    }
  }
  return out
})

function measure() {
  const element = viewport.value
  if (!element) return
  width.value = element.clientWidth
  height.value = element.clientHeight
  scrollTop.value = element.scrollTop
}

function onScroll() {
  if (viewport.value) scrollTop.value = viewport.value.scrollTop
  maybeEmitBottom()
}

/** 距离底部不足一屏预算就通知一次（去重，直到滚离底部窗口再重置） */
function maybeEmitBottom() {
  const element = viewport.value
  if (!element) return
  const nearBottom =
    element.scrollTop + element.clientHeight >= element.scrollHeight - THRESHOLD
  if (nearBottom) {
    if (!bottomFired) {
      bottomFired = true
      emit('bottom')
    }
  } else {
    bottomFired = false
  }
}

// 仅当首项变化（换目录/换搜索词）才回到顶部；loadMore 追加不重置滚动
watch(
  () => props.items[0],
  () => {
    if (viewport.value) viewport.value.scrollTop = 0
    scrollTop.value = 0
    bottomFired = false
  },
)

// 追加之后重新打量：一页填不满视口时继续补下一页
watch(
  () => props.items.length,
  async () => {
    await nextTick()
    measure()
    bottomFired = false
    maybeEmitBottom()
  },
)

onMounted(() => {
  measure()
  observer = new ResizeObserver(() => measure())
  if (viewport.value) observer.observe(viewport.value)
  void nextTick().then(maybeEmitBottom)
})

onBeforeUnmount(() => {
  observer?.disconnect()
  observer = null
})
</script>

<template>
  <div ref="viewport" class="h-full overflow-y-auto" @scroll.passive="onScroll">
    <div class="relative w-full" :style="{ height: `${totalHeight}px` }">
      <div
        v-for="entry in visible"
        :key="entry.index"
        :style="entry.style"
        class="overflow-hidden"
      >
        <slot :item="entry.item" :index="entry.index" />
      </div>
    </div>
  </div>
</template>
