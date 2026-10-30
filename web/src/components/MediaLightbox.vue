<script setup lang="ts">
/**
 * 全局媒体灯箱（设计文档 3.2 / 8.4.9 / v4.4.6）。
 *
 * 交互骨架对齐 Telegram，工程细节参考 FileBrowser `Preview.vue`：
 * - **媒体是主角**：舞台铺满视口，顶栏与缩略图条是浮在画面上的临时层，
 *   闲置 2.6s 自动隐去（FileBrowser 是 1.5s 隐藏导航），鼠标一动、按任意键就回来；
 * - **顶栏**：左 × → 文件名 → 计数 chip → 下载 / 新窗 / 分享 / 信息（右键菜单保留，触屏也有入口了）；
 * - **底栏**：只在图片且多于一张时出现，跟随顶栏显隐；当前项加白环并轻微放大；
 *   视频不给底栏——Video.js 的控制条就在视口底部，两条会叠在一起；
 * - **图片**：滚轮以光标为锚点缩放、放大后拖动平移、双击 1↔2、双指捏合、
 *   未放大时左右滑切图 / 下滑关闭（`useImageZoom`）；
 * - **视频**：铺满（`fill`），不再套 `max-w-4xl` 的盒子；播放器自带的 ✕ 关掉，
 *   避免和顶栏重复；`←/→` 在视频上做 ±5s 快进，不切条目（FileBrowser 特意做了这个放行）。
 */
import { useEventListener } from '@vueuse/core'
import { computed, defineAsyncComponent, nextTick, onBeforeUnmount, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import ContextMenu, { type ContextMenuItem } from './ContextMenu.vue'
import { coverUrl, rawUrl, thumbUrl } from '../api/media'
import { sharesApi } from '../api/shares'
import { formatSize, formatTime } from '../lib/format'
import { openShare } from '../stores/share'
import { useScrollLock } from '../composables/useScrollLock'
import { useBackClose } from '../composables/useBackClose'
import { useImageZoom } from '../composables/useImageZoom'
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

const stageEl = ref<HTMLElement | null>(null)
const layerEl = ref<HTMLElement | null>(null)
const imgEl = ref<HTMLImageElement | null>(null)
const playerRef = ref<{ seekBy: (seconds: number) => void; togglePlay: () => void } | null>(null)

/** 原图固有尺寸（信息层用，缩略图打底阶段先空着） */
const natural = ref<{ width: number; height: number } | null>(null)
const videoMeta = ref<{ width: number; height: number; duration: number } | null>(null)
const infoOpen = ref(false)

// ---------------- chrome 显隐（Telegram：媒体是主角，UI 是临时层） ----------------
const chrome = ref(true)
/** 指针停在工具栏 / 缩略图条上时不自动隐藏，否则点的瞬间它会消失 */
const holding = ref(false)
let idleTimer: number | null = null
let lastNote = 0

const IDLE_MS = 2600

function hideAfterIdle() {
  if (idleTimer !== null) window.clearTimeout(idleTimer)
  idleTimer = window.setTimeout(() => {
    idleTimer = null
    if (holding.value || menu.value || infoOpen.value) return
    chrome.value = false
  }, IDLE_MS)
}

/** 任何主动作（移动 / 滚轮 / 按键 / 触摸）都先把 chrome 叫回来，再重新计时 */
function noteActivity() {
  if (!isOpen.value) return
  const now = Date.now()
  if (now - lastNote < 120) return
  lastNote = now
  chrome.value = true
  hideAfterIdle()
}

/** 指针离开工具栏 / 底条：解除保持并重新开始计时，否则它会一直挂着不隐 */
function releaseHold() {
  holding.value = false
  if (chrome.value) hideAfterIdle()
}

function toggleChrome() {
  chrome.value = !chrome.value
  if (chrome.value) hideAfterIdle()
  else if (idleTimer !== null) {
    window.clearTimeout(idleTimer)
    idleTimer = null
  }
}

// ---------------- 图片缩放 / 滑动 ----------------
const zoom = useImageZoom({
  container: () => stageEl.value,
  media: () => (imgEl.value?.offsetWidth ? imgEl.value : layerEl.value),
  canSwipe: () => (item.value?.kind === 'image' ? lightboxTotal.value > 1 : false),
  canDismiss: () => item.value?.kind === 'image',
  onSwipe: (delta) => stepLightbox(delta),
  onDismiss: () => closeLightbox(),
  onTap: () => {
    // 信息层也算一层：点画面先把它收掉，别让面板和工具栏抢同一次点击
    if (infoOpen.value) {
      infoOpen.value = false
      return
    }
    // 放大状态下点哪儿都只是切工具栏；没放大时点图切工具栏、点黑边直接关
    if (zoom.zoomed.value) {
      toggleChrome()
      return
    }
    if (downTarget && imgEl.value && downTarget !== imgEl.value) {
      closeLightbox()
      return
    }
    toggleChrome()
  },
})
const { style: zoomStyle } = zoom

/** 记录按下时的目标：区分「点图片」与「点黑边」 */
let downTarget: EventTarget | null = null

function onStagePointerDown(event: PointerEvent) {
  if (item.value?.kind !== 'image') return
  downTarget = event.target
  noteActivity()
  zoom.onPointerDown(event)
}

function onStagePointerMove(event: PointerEvent) {
  if (item.value?.kind !== 'image') return
  zoom.onPointerMove(event)
}

function onStagePointerUp(event: PointerEvent) {
  if (item.value?.kind !== 'image') return
  zoom.onPointerUp(event)
}

function onStageWheel(event: WheelEvent) {
  noteActivity()
  if (item.value?.kind !== 'image') return
  event.preventDefault()
  zoom.onWheel(event)
}

function onStageDblClick(event: MouseEvent) {
  if (item.value?.kind !== 'image') return
  if (zoom.zoomed.value) zoom.reset()
  else zoom.zoomAt(event.clientX, event.clientY, 2)
}

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

// 换条目：清掉与上一张有关的全部瞬时状态，chrome 重新亮起
watch(
  () => [item.value?.path, isOpen.value] as const,
  () => {
    ready.value = null
    natural.value = null
    videoMeta.value = null
    infoOpen.value = false
    zoom.reset()
    if (isOpen.value) {
      chrome.value = true
      hideAfterIdle()
      preloadNeighbors()
    } else if (idleTimer !== null) {
      // 关闭时把计时器收掉，免得下次打开前它先把 chrome 关掉
      window.clearTimeout(idleTimer)
      idleTimer = null
    }
  },
  { immediate: true },
)

const infoRows = computed(() => {
  const current = item.value
  if (!current) return [] as Array<{ label: string; value: string }>
  const rows: Array<{ label: string; value: string }> = []
  if (current.kind === 'image' && natural.value) {
    rows.push({
      label: t('gallery.viewer.dimensions'),
      value: `${natural.value.width} × ${natural.value.height}`,
    })
  }
  if (current.kind === 'video' && videoMeta.value) {
    rows.push({
      label: t('gallery.viewer.dimensions'),
      value: `${videoMeta.value.width} × ${videoMeta.value.height}`,
    })
    if (Number.isFinite(videoMeta.value.duration)) {
      const totalSeconds = Math.round(videoMeta.value.duration)
      const minutes = Math.floor(totalSeconds / 60)
      const seconds = String(totalSeconds % 60).padStart(2, '0')
      rows.push({
        label: t('gallery.viewer.duration'),
        value: `${minutes}:${seconds}`,
      })
    }
  }
  if (current.size) rows.push({ label: t('gallery.viewer.size'), value: formatSize(current.size) })
  if (current.modified) {
    rows.push({ label: t('gallery.viewer.modified'), value: formatTime(current.modified) })
  }
  return rows
})

function onKey(event: KeyboardEvent) {
  if (!isOpen.value) return
  noteActivity()
  if (event.key === 'Escape') {
    // 信息层是浮层里的一层，先收它
    if (infoOpen.value) {
      infoOpen.value = false
      return
    }
    // 标记「这一击已被灯箱消费」：页面级监听（如相册详情的 Esc 返回）据此跳过，
    // 否则一次 Esc 会同时关掉灯箱和相册，连退两层
    event.preventDefault()
    closeLightbox()
    return
  }
  const isVideo = item.value?.kind === 'video'
  if (event.key === 'ArrowLeft') {
    if (isVideo) playerRef.value?.seekBy(-5)
    else stepLightbox(-1)
    return
  }
  if (event.key === 'ArrowRight') {
    if (isVideo) playerRef.value?.seekBy(5)
    else stepLightbox(1)
    return
  }
  if (isVideo && (event.key === ' ' || event.key === 'k')) {
    event.preventDefault()
    playerRef.value?.togglePlay()
    return
  }
  if (item.value?.kind !== 'image') return
  if (event.key === '+' || event.key === '=') {
    event.preventDefault()
    zoom.zoomBy(1.25)
  } else if (event.key === '-' || event.key === '_') {
    event.preventDefault()
    zoom.zoomBy(0.8)
  } else if (event.key === '0') {
    zoom.reset()
  }
}

function markFailed(path: string) {
  failed.value = new Set(failed.value).add(path)
}

function onImageLoad(event: Event) {
  const img = event.target as HTMLImageElement
  ready.value = item.value?.path ?? null
  natural.value = { width: img.naturalWidth, height: img.naturalHeight }
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

function download() {
  const current = item.value
  if (!current) return
  const link = document.createElement('a')
  link.href = rawUrl(current.path)
  link.download = current.name
  link.rel = 'noopener'
  document.body.appendChild(link)
  link.click()
  link.remove()
}

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
    download()
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

onBeforeUnmount(() => {
  if (idleTimer !== null) window.clearTimeout(idleTimer)
  idleTimer = null
})
</script>

<template>
  <Teleport to="body">
    <div
      v-if="isOpen && item"
      class="fixed inset-0 z-[80] overflow-hidden bg-[#08080a] select-none"
      :class="chrome ? '' : 'cursor-none'"
      data-testid="media-lightbox"
      @pointermove.passive="noteActivity"
      @wheel.passive="noteActivity"
      @contextmenu.prevent="openMenu"
    >
      <!-- 舞台：铺满视口，顶栏与底栏都浮在它上面，不再挤压媒体 -->
      <div
        ref="stageEl"
        class="absolute inset-0 flex items-center justify-center overflow-hidden"
        :style="item.kind === 'image' ? { touchAction: 'none' } : undefined"
        @wheel="onStageWheel"
        @pointerdown="onStagePointerDown"
        @pointermove="onStagePointerMove"
        @pointerup="onStagePointerUp"
        @pointercancel="onStagePointerUp"
        @dblclick="onStageDblClick"
      >
        <!-- 图片：缩略图打底 → 原图淡入，整层跟着缩放/平移/滑动 -->
        <template v-if="item.kind === 'image'">
          <div
            ref="layerEl"
            class="flex size-full items-center justify-center"
            :style="zoomStyle"
            data-testid="lightbox-zoom-layer"
          >
            <!-- 打底层：用缓存过的缩略图占位，原图就绪后**完全淡出**。
                 `object-scale-down` 而非 `contain`：小图不会被放大成比原图大一圈的
                 模糊块（contain 会把盒子撑满并放大内容），两者严格重合，撤下时无残影。 -->
            <img
              v-if="!failed.has(item.path)"
              :src="thumbUrl(item.path, 960)"
              class="pointer-events-none absolute inset-0 z-0 m-auto max-h-full max-w-full object-scale-down transition-opacity duration-300"
              :class="ready === item.path ? 'opacity-0' : 'opacity-100'"
              aria-hidden="true"
              alt=""
            />
            <!-- 原图：加载完成后淡入，层级永远高于遮罩与打底层 -->
            <img
              v-if="!failed.has(item.path)"
              ref="imgEl"
              :key="item.path"
              :src="rawUrl(item.path)"
              class="relative z-10 max-h-full max-w-full object-contain transition-opacity duration-300"
              :class="ready === item.path ? 'opacity-100' : 'opacity-0'"
              data-testid="lightbox-image"
              alt=""
              draggable="false"
              @load="onImageLoad"
              @error="markFailed(item.path)"
            />
            <div v-else class="grid place-items-center gap-2 text-white/50">
              <AppIcon name="gallery" :size="40" />
              <p class="text-xs">{{ t('gallery.viewer.broken') }}</p>
            </div>
          </div>
        </template>

        <!-- 视频：铺满视口，控制条与字幕交给 Video.js，顶栏只留我们自己的工具 -->
        <div v-else-if="item.kind === 'video'" :key="item.path" class="size-full" @click="noteActivity">
          <MediaPlayer
            ref="playerRef"
            :src="rawUrl(item.path)"
            :poster="thumbUrl(item.path, 960)"
            :subtitle="item.subtitle ?? null"
            :autoplay="true"
            fill
            :show-close="false"
            @meta="videoMeta = $event"
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

        <!-- 翻页：贴视口左右边缘，与媒体实际宽度无关；触屏靠左右滑，不占地方 -->
        <button
          v-if="total > 1"
          class="absolute left-2 top-1/2 z-20 hidden size-10 -translate-y-1/2 place-items-center rounded-full bg-black/40 text-white backdrop-blur transition-all hover:bg-black/70 sm:grid"
          :class="chrome ? 'opacity-100' : 'pointer-events-none opacity-0'"
          :title="t('gallery.viewer.prev')"
          data-testid="lightbox-prev"
          @click.stop="stepLightbox(-1)"
        >
          <AppIcon name="chevronLeft" :size="18" />
        </button>
        <button
          v-if="total > 1"
          class="absolute right-2 top-1/2 z-20 hidden size-10 -translate-y-1/2 place-items-center rounded-full bg-black/40 text-white backdrop-blur transition-all hover:bg-black/70 sm:grid"
          :class="chrome ? 'opacity-100' : 'pointer-events-none opacity-0'"
          :title="t('gallery.viewer.next')"
          data-testid="lightbox-next"
          @click.stop="stepLightbox(1)"
        >
          <AppIcon name="chevronRight" :size="18" />
        </button>

        <!-- 缩放角标：只在放大后出现，点一下复位 -->
        <button
          v-if="item.kind === 'image' && zoom.zoomed.value"
          class="absolute left-1/2 z-20 -translate-x-1/2 rounded-full bg-black/60 px-3 py-1 text-[11px] tabular-nums text-white/80 backdrop-blur transition-opacity hover:bg-black/80"
          :class="total > 1 ? 'bottom-[84px]' : 'bottom-4'"
          :title="t('gallery.viewer.zoomReset')"
          data-testid="lightbox-zoom"
          @click.stop="zoom.reset()"
        >
          {{ zoom.percent.value }}%
        </button>
      </div>

      <!-- 顶栏：Telegram 顺序（× 在最左）与我们全屏编辑器保持一致 -->
      <header
        class="absolute inset-x-0 top-0 z-30 flex h-14 items-center gap-1.5 bg-gradient-to-b from-black/80 via-black/45 to-transparent pr-2 pl-1 transition-opacity duration-300 sm:gap-2 sm:pl-2"
        :class="chrome ? 'opacity-100' : 'pointer-events-none opacity-0'"
        style="padding-top: env(safe-area-inset-top, 0px)"
        @pointerenter="holding = true"
        @pointerleave="releaseHold"
      >
        <button
          type="button"
          class="grid size-9 shrink-0 place-items-center rounded-full text-white/85 transition-colors hover:bg-white/15 hover:text-white"
          :title="t('gallery.viewer.close')"
          data-testid="lightbox-close"
          @click="closeLightbox()"
        >
          <AppIcon name="close" :size="18" />
        </button>

        <p class="min-w-0 flex-1 truncate text-[13px] text-white/85" :title="item.name">
          {{ item.name }}
        </p>

        <span
          v-if="total > 1"
          class="shrink-0 rounded-full bg-white/10 px-1.5 py-0.5 text-[10px] tabular-nums text-white/60 sm:px-2 sm:text-[11px]"
          data-testid="lightbox-counter"
        >
          {{ lightboxState.index + 1 }} / {{ total }}
        </span>

        <div class="flex shrink-0 items-center gap-0.5">
          <button
            type="button"
            class="grid size-9 place-items-center rounded-full text-white/75 transition-colors hover:bg-white/15 hover:text-white"
            :title="t('gallery.viewer.original')"
            data-testid="lightbox-download"
            @click="download()"
          >
            <AppIcon name="download" :size="17" />
          </button>
          <a
            :href="rawUrl(item.path)"
            target="_blank"
            rel="noopener noreferrer"
            class="grid size-9 place-items-center rounded-full text-white/75 transition-colors hover:bg-white/15 hover:text-white"
            :title="t('gallery.viewer.openOriginal')"
          >
            <AppIcon name="globe" :size="17" />
          </a>
          <button
            type="button"
            class="grid size-9 place-items-center rounded-full text-white/75 transition-colors hover:bg-white/15 hover:text-white"
            :title="t('gallery.viewer.share')"
            data-testid="lightbox-share"
            @click="openShare(item.path, item.name)"
          >
            <AppIcon name="share" :size="17" />
          </button>
          <button
            type="button"
            class="grid size-9 place-items-center rounded-full transition-colors hover:bg-white/15"
            :class="infoOpen ? 'bg-white/15 text-white' : 'text-white/75 hover:text-white'"
            :title="t('gallery.viewer.info')"
            data-testid="lightbox-info"
            @click="infoOpen = !infoOpen"
          >
            <AppIcon name="info" :size="17" />
          </button>
        </div>
      </header>

      <!-- 信息层：尺寸 / 时长 / 体积 / 修改时间，就地展开，不另开弹窗 -->
      <Transition name="fade">
        <div
          v-if="infoOpen"
          class="absolute top-16 right-2 z-40 w-72 max-w-[calc(100vw-1rem)] rounded-xl border border-white/10 bg-black/85 p-3 text-[11px] text-white/75 shadow-2xl backdrop-blur"
          data-testid="lightbox-info-panel"
        >
          <p class="mb-2 truncate text-[12px] text-white/90">{{ item.name }}</p>
          <dl v-if="infoRows.length" class="space-y-1.5">
            <div v-for="row in infoRows" :key="row.label" class="flex items-baseline justify-between gap-3">
              <dt class="shrink-0 text-white/45">{{ row.label }}</dt>
              <dd class="min-w-0 truncate text-right tabular-nums">{{ row.value }}</dd>
            </div>
          </dl>
          <p v-else class="text-white/40">{{ t('gallery.viewer.infoPending') }}</p>
        </div>
      </Transition>

      <!-- 缩略图条：跟随顶栏显隐；只有图片且多于一张才出现 -->
      <div
        v-if="total > 1 && item.kind === 'image'"
        ref="stripEl"
        class="lightbox-strip absolute inset-x-0 bottom-0 z-30 flex h-[70px] items-center gap-1.5 overflow-x-auto overflow-y-hidden bg-gradient-to-t from-black/85 via-black/50 to-transparent px-3 pt-3 pb-3 transition-all duration-300"
        :class="chrome ? 'translate-y-0 opacity-100' : 'pointer-events-none translate-y-full opacity-0'"
        style="padding-bottom: calc(0.75rem + env(safe-area-inset-bottom, 0px))"
        data-testid="lightbox-strip"
        @pointerenter="holding = true"
        @pointerleave="releaseHold"
        @wheel="onStripWheel"
      >
        <div class="mx-auto flex min-w-max items-center gap-1.5 px-1">
          <button
            v-for="(entry, index) in lightboxState.items"
            :key="entry.path"
            :ref="(el) => setThumbRef(index, el as Element | null)"
            type="button"
            class="relative size-11 shrink-0 overflow-hidden rounded-md transition-all duration-200"
            :class="
              index === lightboxState.index
                ? 'scale-105 ring-2 ring-white/85'
                : 'opacity-55 ring-1 ring-white/10 hover:opacity-90'
            "
            :title="entry.name"
            @click="jumpLightbox(index)"
          >
            <img
              :src="thumbOf(entry.path, 160)"
              alt=""
              class="size-full object-cover"
              loading="lazy"
              @error="($event.target as HTMLImageElement).classList.add('hidden')"
            />
            <span class="pointer-events-none absolute inset-0 grid place-items-center text-white/70">
              <AppIcon v-if="entry.kind !== 'image'" :name="entry.kind === 'video' ? 'video' : 'audio'" :size="14" />
            </span>
          </button>
        </div>
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
  height: 5px;
}
.lightbox-strip::-webkit-scrollbar-thumb {
  background: rgba(255, 255, 255, 0.35);
  border-radius: 3px;
}
</style>
