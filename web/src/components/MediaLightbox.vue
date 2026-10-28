<script setup lang="ts">
/**
 * 全局媒体灯箱（设计文档 3.2 / 8.4.9 / v2.8）。
 *
 * - **单例**：只在 App.vue 挂一次，Teleport 到 body；文件管理、相册、影视库都复用它；
 * - 图片 / 视频 / 音频混排也能左右切换（← →），视频走全局 MediaPlayer；
 * - **翻页按钮贴容器左右边缘绝对定位**：按钮位置绝不能跟着图片宽度走
 *   （原来放在 flex 流里，图片一变宽按钮就往外漂，点「下一张」要重新找位置）；
 * - **底部缩略图条可滚动**：滚轮纵向增量映射成横向滚动，当前项自动滚入视野；
 * - 视频缩略图用后端抽取的首帧，播放器传 `poster`，点开即有画面而不是一片黑；
 * - 关闭时整块 `v-if` 卸载，MediaPlayer 内部会 `pause(); src=''; load()` 彻底销毁。
 */
import { computed, defineAsyncComponent, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import { coverUrl, rawUrl, thumbUrl } from '../api/media'
import { useScrollLock } from '../composables/useScrollLock'
import { bindVolume } from '../lib/volume'
import {
  closeLightbox,
  currentLightboxItem,
  finishLightboxPlayback,
  jumpLightbox,
  lightboxState,
  lightboxTotal,
  reportLightboxProgress,
  stepLightbox,
} from '../stores/lightbox'

// 播放器内核只点开视频才下载（设计文档 2.3 代码分割）
const MediaPlayer = defineAsyncComponent(() => import('./MediaPlayer.vue'))

const { t } = useI18n()

const isOpen = computed(() => lightboxState.open)
const item = currentLightboxItem
const total = lightboxTotal

/** 打不开的图片（坏文件 / 无权限）回退成图标，避免整屏空白 */
const failed = ref(new Set<string>())
const audioEl = ref<HTMLAudioElement | null>(null)
const stripEl = ref<HTMLElement | null>(null)
const thumbRefs = ref<Record<number, HTMLElement | null>>({})

useScrollLock(isOpen)

function onKey(event: KeyboardEvent) {
  if (!isOpen.value) return
  if (event.key === 'Escape') {
    closeLightbox()
    return
  }
  if (event.key === 'ArrowLeft') stepLightbox(-1)
  if (event.key === 'ArrowRight') stepLightbox(1)
}

function markFailed(path: string) {
  failed.value = new Set(failed.value).add(path)
}

/** 缩略图统一用后端首帧（视频也有），失败时退回类型图标 */
function thumbOf(path: string, width: number) {
  return thumbUrl(path, width)
}

/** 滚轮在缩略图条上是「横向滚动」，别让浏览器把它当成页面滚动（8.2 滚动体验） */
function onStripWheel(event: WheelEvent) {
  const el = stripEl.value
  if (!el) return
  const delta = Math.abs(event.deltaY) > Math.abs(event.deltaX) ? event.deltaY : event.deltaX
  if (delta === 0) return
  el.scrollLeft += delta
  event.preventDefault()
}

function setThumbRef(index: number, el: Element | null) {
  thumbRefs.value[index] = el as HTMLElement | null
}

/** 切换项后把当前缩略图滚到可见区域（不改变页面滚动） */
watch(
  () => lightboxState.index,
  async (index) => {
    await nextTick()
    const el = thumbRefs.value[index]
    el?.scrollIntoView({ block: 'nearest', inline: 'center', behavior: 'smooth' })
  },
)

function onAudioReady(event: Event) {
  bindVolume(event.target as HTMLAudioElement)
}

watch(
  () => item.value?.path,
  () => {
    // 换曲/换图时把上一首的音频彻底停掉，防止两条音轨叠在一起
    const el = audioEl.value
    if (el) {
      el.pause()
      el.removeAttribute('src')
      el.load()
    }
  },
)

onMounted(() => window.addEventListener('keydown', onKey))
onBeforeUnmount(() => window.removeEventListener('keydown', onKey))
</script>

<template>
  <Teleport to="body">
    <div
      v-if="isOpen && item"
      class="fixed inset-0 z-[80] flex flex-col bg-black/[0.97]"
      data-testid="media-lightbox"
      @click.self="closeLightbox()"
    >
      <header class="flex h-12 shrink-0 items-center gap-3 px-3 md:px-4">
        <p class="min-w-0 flex-1 truncate text-xs text-white/70">
          {{ item.name }}
          <span v-if="total > 1" class="ml-2 shrink-0 text-white/45">
            {{ lightboxState.index + 1 }} / {{ total }}
          </span>
        </p>
        <a
          :href="rawUrl(item.path)"
          target="_blank"
          rel="noopener noreferrer"
          class="rounded px-2 py-1 text-xs text-white/60 transition-colors hover:text-white"
          :title="t('gallery.viewer.openOriginal')"
        >
          {{ t('gallery.viewer.original') }}
        </a>
        <button
          class="rounded p-1 text-white/70 transition-colors hover:text-white"
          :title="t('gallery.viewer.close')"
          @click="closeLightbox()"
        >
          <AppIcon name="close" :size="18" />
        </button>
      </header>

      <!-- 舞台：翻页按钮 absolute 贴在左右边缘，与媒体实际宽度无关 -->
      <div class="relative flex min-h-0 flex-1 items-center justify-center px-3 pb-2 md:px-14">
        <button
          v-if="total > 1"
          class="absolute left-1 top-1/2 z-10 grid size-10 -translate-y-1/2 place-items-center rounded-full bg-black/45 text-white backdrop-blur transition-colors hover:bg-black/70 md:left-3"
          :title="t('gallery.viewer.prev')"
          data-testid="lightbox-prev"
          @click.stop="stepLightbox(-1)"
        >
          <AppIcon name="chevronLeft" :size="18" />
        </button>

        <!-- 图片 -->
        <template v-if="item.kind === 'image'">
          <img
            v-if="!failed.has(item.path)"
            :key="item.path"
            :src="rawUrl(item.path)"
            class="max-h-full max-w-full rounded-lg object-contain"
            alt=""
            @error="markFailed(item.path)"
          />
          <div v-else class="grid place-items-center gap-2 text-white/50">
            <AppIcon name="gallery" :size="40" />
            <p class="text-xs">{{ t('gallery.viewer.broken') }}</p>
          </div>
        </template>

        <!-- 视频 -->
        <div v-else-if="item.kind === 'video'" :key="item.path" class="w-full max-w-4xl">
          <MediaPlayer
            :src="rawUrl(item.path)"
            :poster="thumbUrl(item.path, 960)"
            :subtitle="item.subtitle ?? null"
            :autoplay="true"
            @close="closeLightbox()"
            @progress="reportLightboxProgress(item.path, $event)"
            @ended="finishLightboxPlayback(item.path)"
          />
        </div>

        <!-- 音频 -->
        <div v-else :key="item.path" class="w-full max-w-xl px-4">
          <div class="mb-4 flex flex-col items-center gap-3">
            <img
              :src="coverUrl(item.path)"
              alt=""
              class="size-32 rounded-xl object-cover shadow-lg"
              @error="($event.target as HTMLImageElement).style.display = 'none'"
            />
            <p class="max-w-full truncate text-sm text-white/80">{{ item.name }}</p>
          </div>
          <audio
            ref="audioEl"
            :src="rawUrl(item.path)"
            controls
            autoplay
            class="w-full"
            @loadedmetadata="onAudioReady"
            @play="onAudioReady"
          />
        </div>

        <button
          v-if="total > 1"
          class="absolute right-1 top-1/2 z-10 grid size-10 -translate-y-1/2 place-items-center rounded-full bg-black/45 text-white backdrop-blur transition-colors hover:bg-black/70 md:right-3"
          :title="t('gallery.viewer.next')"
          data-testid="lightbox-next"
          @click.stop="stepLightbox(1)"
        >
          <AppIcon name="chevronRight" :size="18" />
        </button>
      </div>

      <!-- 缩略图条：可横向滚动 + 滚轮映射 + 当前项自动定位（v2.8） -->
      <div
        v-if="total > 1"
        ref="stripEl"
        class="lightbox-strip flex h-[76px] shrink-0 items-center gap-1.5 overflow-x-auto overflow-y-hidden px-3 pb-2"
        data-testid="lightbox-strip"
        @wheel="onStripWheel"
      >
        <button
          v-for="(entry, index) in lightboxState.items"
          :key="entry.path"
          :ref="(el) => setThumbRef(index, el as Element | null)"
          class="relative size-12 shrink-0 overflow-hidden rounded-md border transition-opacity"
          :class="
            index === lightboxState.index
              ? 'border-white/80 opacity-100'
              : 'border-transparent opacity-55 hover:opacity-90'
          "
          :title="entry.name"
          @click="jumpLightbox(index)"
        >
          <img
            :src="thumbOf(entry.path, 160)"
            alt=""
            class="size-full object-cover"
            loading="lazy"
            @error="
              ($event.target as HTMLImageElement).classList.add('hidden')
            "
          />
          <span class="pointer-events-none absolute inset-0 grid place-items-center text-white/70">
            <AppIcon v-if="entry.kind !== 'image'" :name="entry.kind === 'video' ? 'video' : 'audio'" :size="14" />
          </span>
        </button>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
/* 滚动条在深色 backdrop 上默认几乎看不见：给缩略图条一条常驻的细滚动条 */
.lightbox-strip {
  scrollbar-width: thin;
  scrollbar-color: rgba(255, 255, 255, 0.35) transparent;
  overscroll-behavior-x: contain;
}
.lightbox-strip::-webkit-scrollbar {
  height: 6px;
}
.lightbox-strip::-webkit-scrollbar-thumb {
  background: rgba(255, 255, 255, 0.35);
  border-radius: 3px;
}
</style>
