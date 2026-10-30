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
import { useEventListener } from '@vueuse/core'
import { computed, defineAsyncComponent, nextTick, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import ContextMenu, { type ContextMenuItem } from './ContextMenu.vue'
import { coverUrl, rawUrl, thumbUrl } from '../api/media'
import { sharesApi } from '../api/shares'
import { openShare } from '../stores/share'
import { useScrollLock } from '../composables/useScrollLock'
import { useBackClose } from '../composables/useBackClose'
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
/** 原图已解码完成的路径：在此之前用缩略图打底（设计文档 3.4.4） */
const ready = ref<string | null>(null)
/** 已经静默预加载过的路径，避免来回翻页重复请求 */
const preloaded = new Set<string>()
const audioEl = ref<HTMLAudioElement | null>(null)
const stripEl = ref<HTMLElement | null>(null)
const thumbRefs = ref<Record<number, HTMLElement | null>>({})

useScrollLock(isOpen)

// 灯箱是一层历史：手机上按返回键应该关掉大图，而不是退出整个页面
useBackClose(isOpen, closeLightbox)

/** 静默预加载前后各一张：停留当前图时后台把邻居下载到缓存（设计文档 3.4.4） */
function preloadNeighbors() {
  if (!isOpen.value) return
  const list = lightboxState.items
  for (const offset of [-1, 1]) {
    const entry = list[lightboxState.index + offset]
    if (!entry || entry.kind !== 'image' || preloaded.has(entry.path)) continue
    preloaded.add(entry.path)
    const image = new Image()
    image.decoding = 'async'
    image.src = rawUrl(entry.path)
  }
}

// 换图就重置「已加载」状态，让打底层重新顶上来；顺便预热左右邻居
watch(
  () => [item.value?.path, isOpen.value] as const,
  () => {
    ready.value = null
    preloadNeighbors()
  },
  { immediate: true },
)

function onKey(event: KeyboardEvent) {
  if (!isOpen.value) return
  if (event.key === 'Escape') {
    // 标记「这一击已被灯箱消费」：页面级监听（如相册详情的 Esc 返回）据此跳过，
    // 否则一次 Esc 会同时关掉灯箱和相册，连退两层
    event.preventDefault()
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

// ---------------- 右键菜单（设计文档 3.2.5：灯箱复用全局 ContextMenu） ----------------
const menu = ref<{ x: number; y: number } | null>(null)

function openMenu(event: MouseEvent) {
  menu.value = { x: event.clientX, y: event.clientY }
}

const menuItems = computed<ContextMenuItem[]>(() => {
  const current = item.value
  if (!current) return []
  const items: ContextMenuItem[] = [
    { id: 'open', label: t('gallery.viewer.openOriginal'), icon: 'globe' },
    { id: 'download', label: t('gallery.viewer.original'), icon: 'download' },
    { id: 'share', label: t('files.mediaMenu.share'), icon: 'share' },
  ]
  if (current.kind === 'image') {
    items.push({ id: 'directLink', label: t('files.mediaMenu.directLink'), icon: 'link' })
  }
  return items
})

async function onMenuSelect(id: string) {
  const current = item.value
  menu.value = null
  if (!current) return
  if (id === 'open') {
    window.open(rawUrl(current.path), '_blank', 'noopener')
  } else if (id === 'download') {
    const link = document.createElement('a')
    link.href = rawUrl(current.path)
    link.download = current.name
    link.rel = 'noopener'
    document.body.appendChild(link)
    link.click()
    link.remove()
  } else if (id === 'share') {
    openShare(current.path, current.name)
  } else if (id === 'directLink') {
    try {
      const created = await sharesApi.create(current.path, { days: 0, allow_download: true })
      await navigator.clipboard.writeText(`${location.origin}${sharesApi.rawUrl(created.token)}`)
      copiedPath.value = current.path
      window.setTimeout(() => (copiedPath.value = null), 2200)
    } catch {
      /* 剪贴板被拒就算了，不打断浏览 */
    }
  }
}

const copiedPath = ref<string | null>(null)

// 键盘导航：useEventListener 负责挂载/卸载成对处理（离开灯箱不会留下悬挂监听）
useEventListener(window, 'keydown', onKey)
</script>

<template>
  <Teleport to="body">
    <div
      v-if="isOpen && item"
      class="fixed inset-0 z-[80] flex flex-col bg-[#08080a]"
      data-testid="media-lightbox"
      @click.self="closeLightbox()"
      @contextmenu.prevent="openMenu"
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

        <!-- 图片：缩略图打底 → 原图淡入（设计文档 3.4.4 渐进式加载，严禁黑屏） -->
        <template v-if="item.kind === 'image'">
          <!-- 打底层：用缓存过的缩略图占位，原图就绪后**完全淡出**。
               `object-scale-down` 而非 `contain`：小图不会被放大成比原图大一圈的
               模糊块（contain 会把盒子撑满并放大内容），两者严格重合，撤下时无残影。 -->
          <img
            v-if="!failed.has(item.path)"
            :src="thumbUrl(item.path, 960)"
            class="pointer-events-none absolute inset-0 z-0 m-auto max-h-full max-w-full rounded-lg object-scale-down transition-opacity duration-300"
            :class="ready === item.path ? 'opacity-0' : 'opacity-100'"
            aria-hidden="true"
            alt=""
          />
          <!-- 原图：加载完成后淡入，层级永远高于遮罩与打底层 -->
          <img
            v-if="!failed.has(item.path)"
            :key="item.path"
            :src="rawUrl(item.path)"
            class="relative z-10 max-h-full max-w-full rounded-lg object-contain transition-opacity duration-300"
            :class="ready === item.path ? 'opacity-100' : 'opacity-0'"
            data-testid="lightbox-image"
            alt=""
            @load="ready = item.path"
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

      <!-- 全局右键菜单（下载 / 分享 / 复制直链 / 新窗口打开） -->
      <ContextMenu
        :open="!!menu"
        :x="menu?.x ?? 0"
        :y="menu?.y ?? 0"
        :items="menuItems"
        @select="onMenuSelect"
        @close="menu = null"
      />

      <!-- 直链复制成功的一次性提示 -->
      <Transition name="fade">
        <p
          v-if="copiedPath"
          class="pointer-events-none absolute bottom-24 left-1/2 -translate-x-1/2 rounded-full bg-white/90 px-4 py-1.5 text-xs text-black shadow-lg"
        >
          {{ t('common.clipboard.linkCopied') }}
        </p>
      </Transition>
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
