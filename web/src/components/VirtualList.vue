<script setup lang="ts" generic="T">
/**
 * 定高虚拟列表：文件管理「列表视图」用（设计文档 3.2 大目录防卡死）。
 * 只渲染视口内的行，上万条目也不撑爆 DOM。
 */
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'

const props = withDefaults(
  defineProps<{
    items: readonly T[]
    /** 行高（px），行内容必须等高 */
    rowHeight: number
    overscan?: number
  }>(),
  { overscan: 6 },
)

/**
 * 滚到距底 `THRESHOLD` 以内就通知父组件续页。
 *
 * 为什么不是外面再放一个哨兵 + IntersectionObserver：这个组件自己就是滚动容器
 * （`h-full overflow-y-auto`），哨兵放在它后面永远可见，只会触发一次 ——
 * 目录/搜索结果的第一页之后就再也不续了。判断必须发生在真正的滚动源上。
 * 同一个「底部窗口」只触发一次，离开后重置；内容变化（追加）后再检查一遍，
 * 保证「一页填不满视口」时能自动补到填满。
 */
const emit = defineEmits<{ bottom: [] }>()
const THRESHOLD = 240
let bottomFired = false

const viewport = ref<HTMLElement | null>(null)
const height = ref(600)
const scrollTop = ref(0)

let observer: ResizeObserver | null = null

const totalHeight = computed(() => Math.max(0, props.items.length * props.rowHeight))
const startIndex = computed(() =>
  Math.max(0, Math.floor(scrollTop.value / props.rowHeight) - props.overscan),
)
const endIndex = computed(() =>
  Math.min(
    props.items.length,
    Math.ceil((scrollTop.value + height.value) / props.rowHeight) + props.overscan,
  ),
)

const visible = computed(() => {
  const out: { index: number; item: T; offset: number }[] = []
  for (let index = startIndex.value; index < endIndex.value; index += 1) {
    out.push({ index, item: props.items[index], offset: index * props.rowHeight })
  }
  return out
})

function measure() {
  const element = viewport.value
  if (!element) return
  height.value = element.clientHeight
  scrollTop.value = element.scrollTop
}

function onScroll() {
  if (viewport.value) scrollTop.value = viewport.value.scrollTop
  maybeEmitBottom()
}

/** 距离底部不足一屏预算就通知一次（去重，直到用户滚离底部窗口再重置） */
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

// 仅当首项变化才回到顶部；loadMore 追加不重置滚动
watch(
  () => props.items[0],
  () => {
    if (viewport.value) viewport.value.scrollTop = 0
    scrollTop.value = 0
    bottomFired = false
  },
)

// 追加（或换一批）之后重新打量一次：一页填不满视口时继续补下一页
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
        class="absolute inset-x-0"
        :style="{ top: `${entry.offset}px`, height: `${rowHeight}px`, contain: 'layout paint' }"
      >
        <slot :item="entry.item" :index="entry.index" />
      </div>
    </div>
  </div>
</template>
