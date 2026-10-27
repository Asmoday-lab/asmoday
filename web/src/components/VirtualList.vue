<script setup lang="ts" generic="T">
/**
 * 定高虚拟列表：文件管理「列表视图」用（设计文档 3.2 大目录防卡死）。
 * 只渲染视口内的行，上万条目也不撑爆 DOM。
 */
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'

const props = withDefaults(
  defineProps<{
    items: readonly T[]
    /** 行高（px），行内容必须等高 */
    rowHeight: number
    overscan?: number
  }>(),
  { overscan: 6 },
)

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
}

// 仅当首项变化才回到顶部；loadMore 追加不重置滚动
watch(
  () => props.items[0],
  () => {
    if (viewport.value) viewport.value.scrollTop = 0
    scrollTop.value = 0
  },
)

onMounted(() => {
  measure()
  observer = new ResizeObserver(() => measure())
  if (viewport.value) observer.observe(viewport.value)
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
