<script setup lang="ts">
/**
 * 内联 SVG 图标。零外部请求。
 */
import { computed } from 'vue'
import { ICONS, HEAD_SOLID, type IconName } from './icons'

const props = withDefaults(
  defineProps<{
    name: IconName
    size?: number | string
    strokeWidth?: number
    /** 实心图标（播放/暂停/切歌这类，参考稿就是实心图形） */
    filled?: boolean
  }>(),
  { size: 18, strokeWidth: 1.6, filled: false },
)

const paths = computed(() => ICONS[props.name] ?? [])
/** 音量类图标：第一段（喇叭体）实心，其余段（声波/叉）描边 */
const headSolid = computed(() => HEAD_SOLID.has(props.name))
/** 某一段是否走实心：整体 filled，或该图标的首段实心 */
const isFilled = (index: number) => props.filled || (headSolid.value && index === 0)
</script>

<template>
  <svg
    :width="props.size"
    :height="props.size"
    viewBox="0 0 24 24"
    :fill="props.filled ? 'currentColor' : 'none'"
    :stroke="props.filled ? 'none' : 'currentColor'"
    :stroke-width="props.filled ? 0 : props.strokeWidth"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
    class="shrink-0"
  >
    <path
      v-for="(d, i) in paths"
      :key="i"
      :d="d"
      :fill="isFilled(i) ? 'currentColor' : 'none'"
      :stroke="isFilled(i) ? 'none' : 'currentColor'"
    />
  </svg>
</template>
