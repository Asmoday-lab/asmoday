<script setup lang="ts">
/**
 * 全局媒体灯箱（设计文档 3.2 / 8.4.9 / v4.4.6，v4.9.67 统一导航层）。
 *
 * 交互骨架对齐 Telegram，工程细节参考 FileBrowser `Preview.vue`：
 * - **媒体是主角**：舞台铺满视口，顶栏与缩略图条是浮在画面上的临时层，
 *   闲置 2.6s 自动隐去（FileBrowser 是 1.5s 隐藏导航），鼠标一动、按任意键就回来；
 * - **顶栏**：文件名 → 计数 chip → 下载 / 新窗 / 分享 / 信息；左上角 ✕ **常驻**，
 *   不随 chrome 隐去（v4.9.67：以前视频只有这个入口，顶栏一隐就无处可退）；
 * - **导航层与媒体类型解耦**：左右箭头、缩略图条、计数、键盘 ←/→ 对图片与视频
 *   一视同仁；视频时缩略图条上移到播放器控制条之上，用布局避让而不是整条砍掉；
 * - **图片专属**：滚轮以光标为锚点缩放、放大后拖动平移、双击 1↔2、双指捏合、
 *   未放大时左右滑切图 / 下滑关闭（`useImageZoom`）；
 * - **视频专属**：铺满（`fill`）、播放器自带 ✕ 关掉；快进退改由 `J` / `L` 或
 *   `Shift+←/→` 承担，`←/→` 统一翻页；触屏不接管播放器手势，翻页靠箭头与缩略图条；
 * - **关闭**：Esc / 左上 ✕ / 点画面黑边（图片与视频同一套几何判定）/ 图片下滑。
 */
import { useEventListener } from '@vueuse/core'
import { computed, defineAsyncComponent, nextTick, onBeforeUnmount, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import ContextMenu, { type ContextMenuItem } from './ContextMenu.vue'
import { coverUrl, rawUrl, THUMB_WIDTH, thumbUrl } from '../api/media'
import { sharesApi } from '../api/shares'
import { formatSize, formatTime } from '../lib/format'
import { triggerDownload } from '../lib/download'
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
// 缩放、拖动、左右滑、下滑关闭只属于图片；视频不把 pointer 交给 useImageZoom，
// 否则拖播放器进度条会被判成「左右滑」而误翻页。视频的轻点走下面同一套 handleTap。
const zoom = useImageZoom({
  container: () => stageEl.value,
  media: () => (imgEl.value?.offsetWidth ? imgEl.value : layerEl.value),
  canSwipe: () => (item.value?.kind === 'image' ? lightboxTotal.value > 1 : false),
  canDismiss: () => item.value?.kind === 'image',
  onSwipe: (delta) => stepLightbox(delta),
  onDismiss: () => closeLightbox(),
  onTap: () => handleTap(downTarget),
})
const { style: zoomStyle } = zoom

/** 记录按下时的目标：区分「点图片」与「点黑边」 */
let downTarget: EventTarget | null = null
/** 按下时的指针位置（黑边判定用，pointerup 才结算） */
let downPoint: { x: number; y: number } | null = null
/** 按下那一刻工具栏是否可见：轻点在「已隐」状态下只负责把它叫回来 */
let downChromeVisible = true

/**
 * 播放器兼容模式（v4.9.68）：开关搬进灯箱信息层，跟着查看器走。
 * 以前只有影视库顶栏「更多」里能改，且要下次打开灯箱才生效；从文件管理或照片库
 * 打开的视频根本找不到入口。localStorage 沿用旧 key，老用户不用重新选。
 */
const NATIVE_KEY = 'asmoday.cinema.native'
const forceNative = ref(localStorage.getItem(NATIVE_KEY) === '1')
function setForceNative(value: boolean) {
  forceNative.value = value
  localStorage.setItem(NATIVE_KEY, value ? '1' : '0')
}

/**
 * 按下点是否落在媒体的**实际渲染矩形**内（object-contain letterbox 几何）。
 * 图片用解码后的 natural，视频用播放器上报的 videoMeta；元数据没就绪时按
 * 「在画面内」处理，宁可切工具栏也不误关。v4.9.67 起两种媒体走同一套判定。
 */
function hitsMedia(): boolean {
  const stage = stageEl.value
  const down = downPoint
  if (!stage || !down) return true
  const dims = item.value?.kind === 'video' ? videoMeta.value : natural.value
  if (!dims) return true
  const rect = stage.getBoundingClientRect()
  const scale = Math.min(rect.width / dims.width, rect.height / dims.height)
  const width = dims.width * scale
  const height = dims.height * scale
  const offsetX = rect.left + (rect.width - width) / 2
  const offsetY = rect.top + (rect.height - height) / 2
  return (
    down.x >= offsetX && down.x <= offsetX + width && down.y >= offsetY && down.y <= offsetY + height
  )
}

/** 轻点画面：图片与视频共用，保证「点黑边关闭 / 点画面切工具栏」口径一致 */
function handleTap(target: EventTarget | null) {
  // 按下点落在自己的控件（左右箭头、缩放角标）上时什么都不做：
  // 这些按钮在 stage 里，pointerup 会先走到这里——早先把「点黑边关闭」
  // 误判进来，灯箱当场关掉，按钮的 click 永远等不到（上下页失效的真凶）
  const down = target as HTMLElement | null
  if (down?.closest('button, a, [data-lightbox-control]')) return
  // 信息层也算一层：点画面先把它收掉，别让面板和工具栏抢同一次点击
  if (infoOpen.value) {
    infoOpen.value = false
    return
  }
  if (zoom.zoomed.value) {
    toggleChrome()
    return
  }
  const kind = item.value?.kind
  if ((kind === 'image' || kind === 'video') && downPoint && !hitsMedia()) {
    // 点在 object-contain 留下的黑边 / 舞台空白上 → 关闭。
    // <img> / <video> 的盒子都是 size-full 铺满舞台的，黑边也在元素里，
    // 所以必须按内容矩形算，不能靠 `target !== imgEl`（v4.6.16）。
    closeLightbox()
    return
  }
  // 工具栏已经自动隐去时，轻点只把它叫回来；否则才是「显示 ↔ 隐藏」的切换。
  // 没有这个分支，pointerdown 的 noteActivity 会先亮起、onTap 又立刻按掉，
  // 视频播放中点一下画面等于什么都没发生（v4.9.67）。
  if (!downChromeVisible) {
    chrome.value = true
    hideAfterIdle()
    return
  }
  toggleChrome()
}

function onStagePointerDown(event: PointerEvent) {
  downChromeVisible = chrome.value
  noteActivity()
  downTarget = event.target
  downPoint = { x: event.clientX, y: event.clientY }
  if (item.value?.kind !== 'image') return
  zoom.onPointerDown(event)
}

function onStagePointerMove(event: PointerEvent) {
  if (item.value?.kind !== 'image') return
  zoom.onPointerMove(event)
}

function onStagePointerUp(event: PointerEvent) {
  if (item.value?.kind === 'image') {
    zoom.onPointerUp(event)
    return
  }
  // 视频 / 音频：不接管拖动，只把「轻点」按同一套规则结算
  if (!downPoint) return
  const travel = Math.hypot(event.clientX - downPoint.x, event.clientY - downPoint.y)
  if (travel > 8) {
    downPoint = null
    return
  }
  handleTap(downTarget)
  downPoint = null
}

function onStageWheel(event: WheelEvent) {
  noteActivity()
  if (item.value?.kind !== 'image') return
  event.preventDefault()
  zoom.onWheel(event)
}

function onStageDblClick(event: MouseEvent) {
  if (item.value?.kind !== 'image') return
  // 落在自己的控件上不算「双击画面」：连点两次「下一张」会在按钮上凑出一次
  // dblclick，冒泡到这里就把刚翻过去的那张放大到 200%（用户报的「偶尔变 200%」）
  if ((event.target as HTMLElement | null)?.closest('button, a')) return
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
    // 打底图与画面用的是同一档（tile），翻页瞬间就能铺上，不用等新尺寸的缩略图
    const thumb = new Image()
    thumb.decoding = 'async'
    thumb.src = thumbUrl(entry.path, THUMB_WIDTH.tile, entry.modified)
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
    // 换条目瞬间复位缩放，但跳过回中动画：旧画面不该当着新图的面动一下
    zoom.reset(true)
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

/** 长按翻页的最小间隔：约 9 张/秒，够快又不会让解码排队到卡顿 */
const PAGE_REPEAT_MS = 110
let lastPageAt = 0

function onKey(event: KeyboardEvent) {
  if (!isOpen.value) return
  noteActivity()

  // 长按（键盘 auto-repeat）分两类处理：
  // - 离散动作（Esc / 空格 / k / 0 / ± ）只认首次按下：按住空格会在播放与暂停
  //   之间狂闪，按住 + 会一路顶到上限，放开时停在哪一档完全随机；
  // - 翻页允许连发（长按连续翻是合理诉求），但要节流：原速约 30ms 一次，
  //   一次长按能翻过几十张，解码根本跟不上，观感就是一串跳帧。
  const paging = event.key === 'ArrowLeft' || event.key === 'ArrowRight'
  if (event.repeat) {
    if (!paging) return
    const now = Date.now()
    if (now - lastPageAt < PAGE_REPEAT_MS) return
    lastPageAt = now
  } else if (paging) {
    lastPageAt = Date.now()
  }

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
  if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') {
    const back = event.key === 'ArrowLeft'
    // v4.9.67：←/→ 对图片与视频都是「上/下一项」，屏幕上的左右箭头也是这个语义。
    // 播放器的快进退让给 Shift+←/→ 与 J / L（播放器惯例），不再和导航抢键。
    if (isVideo && event.shiftKey) playerRef.value?.seekBy(back ? -5 : 5)
    else stepLightbox(back ? -1 : 1)
    return
  }
  if (isVideo && (event.key === 'j' || event.key === 'J')) {
    playerRef.value?.seekBy(-10)
    return
  }
  if (isVideo && (event.key === 'l' || event.key === 'L')) {
    playerRef.value?.seekBy(10)
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
  // 快速翻页时旧图的 load 会晚到：target 已不是当前那张（:key 换了元素），
  // 绝不能把 ready/natural 写成新 path——否则还没解码完的新原图被提前淡入，画面闪一帧空白
  if (img !== imgEl.value) return
  ready.value = item.value?.path ?? null
  natural.value = { width: img.naturalWidth, height: img.naturalHeight }
}

/** 缩略图统一用后端首帧（视频也有），失败时退回类型图标 */
function thumbOf(entry: { path: string; modified?: number | null }, width: number) {
  return thumbUrl(entry.path, width, entry.modified)
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
  // 走 `?download=1`：后端补 Content-Disposition + octet-stream，浏览器直接存盘。
  // 以前是 `window.open(raw)`，TOML / 日志这类文件会被当成 text/plain 就地渲染，
  // 而且不带 charset，中文全是乱码。
  triggerDownload(rawUrl(current.path, { download: true }))
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
    { id: 'download', label: t('files.preview.download'), icon: 'download' },
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
    <!-- Telegram 式观感：背景半透明，隐约透出下面的页面。
         刻意**不用 backdrop-blur**：那会让浏览器每帧重算整个视口背后的内容，
         1080p 下翻页明显掉帧，观感上就是「糊一下／闪一下」——比不糊更糟。
         进场用一次性动画淡入；不能包 <Transition>——关闭时 items 已被清空，
         离场帧里 item 是 null，模板里满屏的 item.name 会炸。 -->
    <div
      v-if="isOpen && item"
      class="fixed inset-0 z-[80] overflow-hidden bg-black/80 select-none lightbox-in"
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
        @pointerdown.capture="onStagePointerDown"
        @pointermove.capture="onStagePointerMove"
        @pointerup.capture="onStagePointerUp"
        @pointercancel.capture="onStagePointerUp"
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
            <!-- 打底层与原图**共用同一个盒子、同一条 object-contain 规则**。
                 以前打底用 `object-scale-down`（按缩略图自身像素渲染），原图用
                 `max-w/h-full + contain`（按可用框渲染）——两套布局模型，像素尺寸
                 一旦不等就必然跳尺寸，用户看到的就是「先大后小」。
                 现在两层都是 `size-full + object-contain`：几何只由宽高比决定，
                 缩略图和原图宽高比相同 ⇒ 渲染矩形完全重合，交叉淡入只变清晰度，
                 不变尺寸（320 缩略图被拉伸到最终框，正是照片应用该有的渐进观感）。 -->
            <img
              v-if="!failed.has(item.path)"
              :src="thumbUrl(item.path, THUMB_WIDTH.tile, item.modified)"
              class="pointer-events-none absolute inset-0 z-0 size-full object-contain transition-opacity duration-300"
              :class="ready === item.path ? 'opacity-0' : 'opacity-100'"
              aria-hidden="true"
              alt=""
            />
            <!-- 原图：加载完成后淡入，层级永远高于打底层 -->
            <img
              v-if="!failed.has(item.path)"
              ref="imgEl"
              :key="item.path"
              :src="rawUrl(item.path)"
              class="absolute inset-0 z-10 size-full object-contain transition-opacity duration-300"
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

        <!-- 视频：铺满视口，控制条与字幕交给 Video.js，顶栏只留我们自己的工具。
             不再挂 @click="noteActivity"：轻点由 stage 的 handleTap 统一结算，
             否则 click 会把刚被 toggle 掉的工具栏又亮回来。 -->
        <div v-else-if="item.kind === 'video'" :key="item.path" class="size-full">
          <MediaPlayer
            ref="playerRef"
            :src="rawUrl(item.path)"
            :poster="thumbUrl(item.path, THUMB_WIDTH.tile, item.modified)"
            :subtitle="item.subtitle ?? null"
            :start-at="item.startAt ?? 0"
            :autoplay="true"
            :force-native="forceNative"
            fill
            :show-close="false"
            @meta="videoMeta = $event"
            @progress="reportLightboxProgress(item.path, $event)"
            @ended="finishLightboxPlayback(item.path)"
          />
        </div>

        <!-- 音频 -->
        <div v-else :key="item.path" class="w-full max-w-xl px-4" data-lightbox-control>
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

        <!-- 翻页：贴视口左右边缘，与媒体实际宽度无关。
             v4.9.67 起对图片与视频都常驻（隐藏时降到 45% 而不是 pointer-events-none），
             小屏也显示——触屏上视频不接管播放器手势，这两个按钮就是它的翻页入口。 -->
        <button
          v-if="total > 1"
          class="absolute left-2 top-1/2 z-40 grid size-9 -translate-y-1/2 place-items-center rounded-full bg-black/40 text-white backdrop-blur transition-opacity hover:bg-black/70 sm:size-10"
          :class="chrome ? 'opacity-100' : 'opacity-45'"
          :title="t('gallery.viewer.prev')"
          data-testid="lightbox-prev"
          @click.stop="stepLightbox(-1)"
        >
          <AppIcon name="chevronLeft" :size="18" />
        </button>
        <button
          v-if="total > 1"
          class="absolute right-2 top-1/2 z-40 grid size-9 -translate-y-1/2 place-items-center rounded-full bg-black/40 text-white backdrop-blur transition-opacity hover:bg-black/70 sm:size-10"
          :class="chrome ? 'opacity-100' : 'opacity-45'"
          :title="t('gallery.viewer.next')"
          data-testid="lightbox-next"
          @click.stop="stepLightbox(1)"
        >
          <AppIcon name="chevronRight" :size="18" />
        </button>

        <!-- 缩放指示：Telegram 式底部胶囊，图片模式常驻——未放大时 100% 淡显，
             放大后变亮，点一下复位；不随 chrome 隐藏（缩放状态不该没有去向） -->
        <button
          v-if="item.kind === 'image'"
          class="absolute left-1/2 z-40 -translate-x-1/2 rounded-full border border-white/10 bg-black/55 px-3 py-1 text-[11px] tabular-nums backdrop-blur transition-colors hover:bg-black/75"
          :class="[
            total > 1 ? 'bottom-[84px]' : 'bottom-4',
            zoom.zoomed.value ? 'text-white/90' : 'text-white/40',
          ]"
          :title="t('gallery.viewer.zoomReset')"
          data-testid="lightbox-zoom"
          @click.stop="zoom.reset()"
        >
          {{ zoom.percent.value }}%
        </button>
      </div>

      <!-- 常驻退出：顶栏会随 chrome 隐去，但「怎么退出」不能跟着消失（v4.9.67）。
           独立放在 header 之外：父级一旦 opacity-0 + pointer-events-none，
           子元素再怎么设都点不动；隐藏时只降透明度，仍可点。 -->
      <button
        type="button"
        class="absolute left-1 top-[10px] z-40 grid size-9 place-items-center rounded-full bg-black/35 text-white/85 backdrop-blur transition-opacity hover:bg-black/70 hover:text-white sm:left-2"
        :class="chrome ? 'opacity-100' : 'opacity-45'"
        style="margin-top: env(safe-area-inset-top, 0px)"
        :title="t('gallery.viewer.close')"
        data-testid="lightbox-close"
        @click="closeLightbox()"
      >
        <AppIcon name="close" :size="18" />
      </button>

      <!-- 顶栏：文件名 → 计数 → 下载 / 新窗 / 分享 / 信息；左侧给常驻 × 留位 -->
      <header
        class="absolute inset-x-0 top-0 z-30 flex h-14 items-center gap-1.5 bg-gradient-to-b from-black/80 via-black/45 to-transparent pr-2 pl-11 transition-opacity duration-300 sm:gap-2 sm:pl-12"
        :class="chrome ? 'opacity-100' : 'pointer-events-none opacity-0'"
        style="padding-top: env(safe-area-inset-top, 0px)"
        @pointerenter="holding = true"
        @pointerleave="releaseHold"
      >
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
            :title="t('files.preview.download')"
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
          <!-- 键位提示：←/→ 改回翻页后，快进退得有可发现的去处 -->
          <p
            v-if="item.kind === 'video'"
            class="mt-2 border-t border-white/10 pt-2 text-white/40"
            data-testid="lightbox-video-keys"
          >
            {{ t('gallery.viewer.videoKeys') }}
          </p>
          <!-- 播放器兼容模式属于播放器自己的设置：放在查看器里，任何入口都能改 -->
          <label
            v-if="item.kind === 'video'"
            class="mt-2 flex cursor-pointer items-start gap-2 border-t border-white/10 pt-2"
            data-testid="lightbox-compat-row"
          >
            <input
              type="checkbox"
              class="mt-0.5 size-3.5 shrink-0 accent-sky-400"
              :checked="forceNative"
              :aria-label="t('gallery.viewer.compatMode')"
              data-testid="lightbox-compat-mode"
              @change="setForceNative(!forceNative)"
            />
            <span class="min-w-0">
              <span class="block text-white/80">{{ t('gallery.viewer.compatMode') }}</span>
              <span class="mt-0.5 block text-white/40">{{ t('gallery.viewer.compatModeHint') }}</span>
            </span>
          </label>
        </div>
      </Transition>

      <!-- 缩略图条：跟随顶栏显隐；多条目就出现，图片与视频一视同仁（v4.9.67）。
           视频的播放控制条在视口底部，所以整条上移到它上方——用布局避让，
           而不是把导航整条砍掉让用户以为「视频没有上下页」。 -->
      <div
        v-if="total > 1"
        ref="stripEl"
        class="lightbox-strip absolute inset-x-0 z-30 flex items-center gap-1.5 overflow-x-auto overflow-y-hidden bg-gradient-to-t from-black/85 via-black/50 to-transparent px-3 pt-3 pb-3 transition-all duration-300"
        :class="[
          chrome ? 'translate-y-0 opacity-100' : 'pointer-events-none translate-y-full opacity-0',
          item.kind === 'video' ? 'bottom-[76px] h-[64px]' : 'bottom-0 h-[70px]',
        ]"
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
            class="relative shrink-0 overflow-hidden rounded-md transition-all duration-200"
            :class="[
              index === lightboxState.index
                ? 'scale-105 ring-2 ring-white/85'
                : 'opacity-55 ring-1 ring-white/10 hover:opacity-90',
              item.kind === 'video' ? 'size-9' : 'size-11',
            ]"
            :title="entry.name"
            @click="jumpLightbox(index)"
          >
            <img
              :src="thumbOf(entry, THUMB_WIDTH.strip)"
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
/* 进场淡入：一次性动画，不参与 Vue Transition（见模板注释） */
.lightbox-in {
  animation: lightbox-in 180ms ease-out;
}
@keyframes lightbox-in {
  from {
    opacity: 0;
  }
  to {
    opacity: 1;
  }
}
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
