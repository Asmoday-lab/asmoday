<script setup lang="ts">
/**
 * 内联 SVG 图标。零外部请求。
 */
import { computed } from 'vue'
import { ICONS, type IconName } from './icons'

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
    <path v-for="(d, i) in paths" :key="i" :d="d" />
  </svg>
</template>
