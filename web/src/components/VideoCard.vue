<script setup lang="ts">
/**
 * 影视库的视频卡片（v4.4.8）。
 *
 * 抽成独立组件的原因：影视库现在与照片库同构——先看文件夹卡片、点进去看影片，
 * 两个层级都要渲染同一张卡片；写两遍模板迟早会漂移。
 *
 * 封面一律走后端 ffmpeg 首帧，拿不到再退回 ThumbHash 模糊底 + 图标；
 * 卡片右上角收藏、左下角续播进度、右下角体积，点卡片播放、右键出媒体菜单。
 */
import { computed, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import type { MediaItem } from '../api/media'
import { placeholderFor, thumbUrl } from '../api/media'
import { formatSize } from '../lib/format'
import { thumbHashToDataUrl } from '../lib/thumbhash'

const props = defineProps<{
  video: MediaItem
  favorite: boolean
  /** 续播秒数：>10 才显示角标（与画廊原有口径一致） */
  resumeAt: number
  /** 卡片右上角是否给收藏按钮（播放列表等只读场景可关掉） */
  showFavorite?: boolean
}>()

const emit = defineEmits<{
  (event: 'play'): void
  (event: 'menu', payload: MouseEvent): void
  (event: 'favorite'): void
}>()

const { t } = useI18n()

/** 封面加载失败一次就够：之后再换也会失败，别每次都撞一次 404 */
const coverFailed = ref(false)
const hash = computed(() => thumbHashToDataUrl(props.video.hash))
const resumeLabel = computed(() => {
  const total = Math.floor(props.resumeAt)
  const minutes = Math.floor(total / 60)
  const seconds = String(total % 60).padStart(2, '0')
  return `${minutes}:${seconds}`
})
</script>

<template>
  <button
    type="button"
    class="group text-left"
    :data-video="video.path"
    @click="emit('play')"
    @contextmenu.prevent="emit('menu', $event)"
  >
    <div
      class="relative aspect-video overflow-hidden rounded-xl border border-line bg-black"
      :style="{ backgroundColor: placeholderFor(video) }"
    >
      <img
        v-if="hash"
        :src="hash"
        class="absolute inset-0 size-full scale-110 object-cover blur-md"
        aria-hidden="true"
        alt=""
      />
      <img
        v-if="!coverFailed"
        :src="thumbUrl(video.path, 480)"
        class="absolute inset-0 size-full object-cover transition-transform duration-300 group-hover:scale-[1.03]"
        loading="lazy"
        alt=""
        @error="coverFailed = true"
      />
      <div v-else class="absolute inset-0 grid place-items-center text-white/40">
        <AppIcon name="video" :size="30" />
      </div>
      <div class="absolute inset-0 grid place-items-center">
        <span
          class="grid size-11 place-items-center rounded-full bg-black/50 text-white backdrop-blur transition-transform group-hover:scale-110"
        >
          <AppIcon name="video" :size="18" />
        </span>
      </div>
      <span
        v-if="resumeAt > 10"
        class="absolute bottom-1.5 left-1.5 rounded bg-black/70 px-1.5 py-0.5 text-[11px] text-white backdrop-blur"
      >
        {{ t('gallery.cinema.resume', { time: resumeLabel }) }}
      </span>
      <span class="absolute right-1.5 bottom-1.5 rounded bg-black/70 px-1.5 py-0.5 text-[11px] text-white">
        {{ formatSize(video.size) }}
      </span>
      <button
        v-if="showFavorite !== false"
        class="absolute right-1.5 top-1.5 grid size-7 place-items-center rounded-full bg-black/45 text-white/80 backdrop-blur transition-colors hover:text-white"
        :class="favorite ? 'fg-danger' : ''"
        :title="favorite ? t('gallery.favorite.remove') : t('gallery.favorite.add')"
        @click.stop="emit('favorite')"
      >
        <AppIcon name="heart" :size="13" :class="favorite ? 'fill-current' : ''" />
      </button>
    </div>
    <p class="mt-2 truncate text-sm font-medium text-ink">{{ video.name }}</p>
  </button>
</template>
