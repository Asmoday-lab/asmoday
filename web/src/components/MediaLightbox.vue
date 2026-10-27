<script setup lang="ts">
/**
 * 全局媒体灯箱（设计文档 3.2 / 7 / 8.4.9）。
 *
 * - **单例**：只在 App.vue 挂在一次，Teleport 到 body；文件管理、相册、影视库都复用它，
 *   严禁各自再手写 `<img>` / `<video>` 弹窗；
 * - 图片支持左右切换（← →），视频走全局 MediaPlayer，音频用原生 `<audio>` + 全局音量；
 * - 关闭时整块 `v-if` 卸载，MediaPlayer 内部会 `pause(); src=''; load()` 彻底销毁，
 *   不会在后台继续缓冲（8.4.9）；
 * - 打开时锁底层滚动，按 Esc 关闭。
 */
import { computed, defineAsyncComponent, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import { coverUrl, rawUrl } from '../api/media'
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
      class="fixed inset-0 z-[80] flex flex-col bg-black/95 backdrop-blur-sm"
      data-testid="media-lightbox"
      @click.self="closeLightbox()"
    >
      <header class="flex h-12 shrink-0 items-center gap-3 px-3 md:px-4">
        <p class="min-w-0 flex-1 truncate text-xs text-white/70">
          {{ item.name }}
          <span v-if="total > 1" class="ml-2 shrink-0 text-white/45">{{ lightboxState.index + 1 }} / {{ total }}</span>
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

      <div class="flex min-h-0 flex-1 items-center justify-center px-2 pb-4">
        <button
          v-if="total > 1"
          class="mr-2 grid size-10 shrink-0 place-items-center rounded-full bg-white/10 text-white transition-colors hover:bg-white/20"
          :title="t('gallery.viewer.prev')"
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
          class="ml-2 grid size-10 shrink-0 place-items-center rounded-full bg-white/10 text-white transition-colors hover:bg-white/20"
          :title="t('gallery.viewer.next')"
          @click.stop="stepLightbox(1)"
        >
          <AppIcon name="chevronRight" :size="18" />
        </button>
      </div>

      <!-- 缩略图条：多于 1 项时提供快速跳转 -->
      <div
        v-if="total > 1"
        class="flex h-16 shrink-0 items-center gap-1.5 overflow-x-auto px-3 pb-2"
      >
        <button
          v-for="(entry, index) in lightboxState.items"
          :key="entry.path"
          class="relative size-12 shrink-0 overflow-hidden rounded-md border transition-opacity"
          :class="index === lightboxState.index ? 'border-white/80' : 'border-transparent opacity-55 hover:opacity-90'"
          :title="entry.name"
          @click="jumpLightbox(index)"
        >
          <img
            v-if="entry.kind === 'image'"
            :src="rawUrl(entry.path)"
            alt=""
            class="size-full object-cover"
            loading="lazy"
          />
          <span v-else class="grid size-full place-items-center bg-white/10 text-white/70">
            <AppIcon :name="entry.kind === 'video' ? 'video' : 'audio'" :size="16" />
          </span>
        </button>
      </div>
    </div>
  </Teleport>
</template>
