<script setup lang="ts">
/**
 * 照片网格瓦片（照片时间线 / 相册详情共用）。
 *
 * 抽成组件是为了让两个视图共用同一套渲染：ThumbHash 模糊打底 → 真缩略图 → 加载失败回图标，
 * 视频额外压一个角标。避免两处各写一遍后行为跑偏（设计文档 1.4 第 9 条：不得私删规定视图）。
 */
import { computed, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import { placeholderFor, THUMB_WIDTH, thumbUrl, type MediaItem } from '../api/media'
import { thumbHashToDataUrl } from '../lib/thumbhash'
import { useThumbSrc } from '../lib/thumb-loader'

const props = withDefaults(
  defineProps<{
    item: MediaItem
    /** 请求的缩略图宽度 */
    width?: number
    /** 等高行布局：格子像素宽度（v4.6.19，时间线用；给了就不用方格比例） */
    cellWidth?: number
    /** 等高行布局：格子像素高度，与 `cellWidth` 成对出现 */
    cellHeight?: number
  }>(),
  { width: THUMB_WIDTH.tile, cellWidth: 0, cellHeight: 0 },
)

const { t } = useI18n()

/**
 * 缩略图装载（v4.6.21）：滚到附近才请求、按行序排队、服务端拥塞时重试。
 * 以前直接 `<img loading="lazy">`，几十个请求并行 + 服务端按需生成 = 谁先回来谁先亮。
 */
const rootEl = ref<HTMLElement | null>(null)
const { src: thumbSrc, failed } = useThumbSrc(rootEl, () =>
  thumbUrl(props.item.path, props.width, props.item.modified),
)
const placeholder = computed(() => thumbHashToDataUrl(props.item.hash))
/** 等高行里的格子是确定性尺寸，方格网格里则是 1:1 自适应 */
const sized = computed(() => props.cellWidth > 0 && props.cellHeight > 0)
const boxStyle = computed(() => {
  const background = { backgroundColor: placeholderFor(props.item) }
  return sized.value
    ? { ...background, width: `${props.cellWidth}px`, height: `${props.cellHeight}px` }
    : background
})
</script>

<template>
  <button
    ref="rootEl"
    type="button"
    class="group relative overflow-hidden rounded-lg border border-transparent transition-colors hover:border-line"
    :class="sized ? 'block shrink-0' : 'aspect-square w-full'"
    style="content-visibility: auto; contain-intrinsic-size: 150px"
    :style="boxStyle"
    :title="item.name"
    :data-photo="item.path"
    :data-kind="item.kind"
  >
    <!-- ThumbHash 模糊占位：真图加载完成前就有正确构图，避免整块跳色 -->
    <img
      v-if="placeholder"
      :src="placeholder"
      class="absolute inset-0 size-full scale-110 object-cover blur-md"
      aria-hidden="true"
      alt=""
    />

    <!-- 视频同样取后端首帧当封面（v2.8），失败才回落图标；src 由调度器按行序点亮 -->
    <img
      v-if="thumbSrc"
      :src="thumbSrc"
      class="relative size-full object-cover transition-transform duration-300 group-hover:scale-[1.03]"
      decoding="async"
      alt=""
    />
    <!-- 取不到（坏文件 / 重试用尽）才落图标；还在路上时留 ThumbHash 打底 -->
    <span v-else-if="failed" class="grid size-full place-items-center text-white/70">
      <AppIcon :name="item.kind === 'video' ? 'video' : 'gallery'" :size="26" />
    </span>

    <span
      v-if="item.kind === 'video'"
      class="absolute right-1 bottom-1 rounded bg-black/60 px-1.5 py-0.5 text-[11px] text-white backdrop-blur"
    >
      {{ t('gallery.photos.videoBadge') }}
    </span>
  </button>
</template>
