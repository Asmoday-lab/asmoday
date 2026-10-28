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
import { placeholderFor, thumbUrl, type MediaItem } from '../api/media'
import { thumbHashToDataUrl } from '../lib/thumbhash'

const props = withDefaults(
  defineProps<{
    item: MediaItem
    /** 请求的缩略图宽度 */
    width?: number
  }>(),
  { width: 400 },
)

const { t } = useI18n()

/** 缩略图加载失败（坏文件 / 无权限）时回落图标，别留一块空色块 */
const failed = ref(false)
const placeholder = computed(() => thumbHashToDataUrl(props.item.hash))
</script>

<template>
  <button
    type="button"
    class="group relative aspect-square w-full overflow-hidden rounded-lg border border-transparent transition-colors hover:border-line"
    style="content-visibility: auto; contain-intrinsic-size: 150px"
    :style="{ backgroundColor: placeholderFor(item) }"
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

    <!-- 视频同样取后端首帧当封面（v2.8），失败才回落图标 -->
    <img
      v-if="!failed"
      :src="thumbUrl(item.path, width)"
      class="relative size-full object-cover transition-transform duration-300 group-hover:scale-[1.03]"
      loading="lazy"
      alt=""
      @error="failed = true"
    />
    <span v-else class="grid size-full place-items-center text-white/70">
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
