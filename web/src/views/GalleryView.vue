<script setup lang="ts">
/**
 * 媒体画廊：照片（相册 / 时间线）+ 影视库 + 音乐库。
 *
 * 视频播放器由 defineAsyncComponent 按需加载（可在「原生播放器」降级开关切换）；
 * 照片网格用虚拟列表渲染；音乐走全局悬浮播放条（跨页面不中断）。
 */
import { computed, defineAsyncComponent, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { useRoute } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import {
  browseMedia,
  coverUrl,
  libraryApi,
  placeholderFor,
  placeholderColor,
  rawUrl,
  thumbUrl,
  type MediaItem,
} from '../api/media'
import { formatSize } from '../lib/format'
import { useScrollLock } from '../composables/useScrollLock'
import { player } from '../stores/player'
import { openLightbox } from '../stores/lightbox'

/** 缩略图加载失败（坏文件 / 无权限）时回落图标，别留一块空色块 */
const failedThumbs = ref<Set<string>>(new Set())
function markThumbFailed(path: string) {
  failedThumbs.value = new Set(failedThumbs.value).add(path)
}

const MediaPlayer = defineAsyncComponent(() => import('../components/MediaPlayer.vue'))
const { t } = useI18n()

type MediaTab = 'photos' | 'cinema' | 'music'
type PhotoView = 'timeline' | 'albums'

// 媒体分区由路由驱动（/photos /cinema /music 共用本组件，见 router 的 meta.mediaTab）
const route = useRoute()
const tab = ref<MediaTab>(route.meta.mediaTab ?? 'photos')
watch(
  () => route.meta.mediaTab,
  (value) => {
    if (value === 'photos' || value === 'cinema' || value === 'music') {
      tab.value = value
      activeAlbum.value = null
    }
  },
)
const photoView = ref<PhotoView>('albums')

const items = ref<MediaItem[]>([])
const root = ref('')
const loading = ref(true)
const error = ref('')
const truncated = ref(false)

const playing = ref<MediaItem | null>(null)

/** 轻提示：坏文件只提示一次，不弹阻断性弹窗（设计文档 3.4.7 第 1 条） */
const toast = ref('')
let toastTimer: number | null = null
function showToast(message: string) {
  toast.value = message
  if (toastTimer !== null) clearTimeout(toastTimer)
  toastTimer = window.setTimeout(() => {
    toast.value = ''
    toastTimer = null
  }, 2600)
}
onBeforeUnmount(() => {
  if (toastTimer !== null) clearTimeout(toastTimer)
})

// 影视库播放器是整屏遮罩，锁住底层滚动（设计文档 8.4）；灯箱自己管自己的锁
useScrollLock(computed(() => Boolean(playing.value)))
/** 进入某个相册后的详情列表；null = 相册列表 */
const activeAlbum = ref<{ folder: string; name: string; items: MediaItem[] } | null>(null)

// ---------------- 收藏 / 播放历史 / 播放器降级开关 ----------------
const favorites = ref(new Set<string>())
const progressMap = ref(new Map<string, number>())
const onlyFavorites = ref(false)
const nativePlayer = ref(localStorage.getItem('asmoday.cinema.native') === '1')
/** 原生播放器无法解码时的可读提示（mkv / avi 等常见容器） */
const nativeError = ref('')
const NATIVE_UNSUPPORTED = ['mkv', 'avi', 'wmv', 'flv', 'rmvb', 'rm', 'ts', 'm2ts', 'vob']

function seekNative(event: Event) {
  const element = event.target as HTMLVideoElement
  const start = resumeAt(playing.value?.path ?? '')
  if (start > 10 && Math.abs(element.currentTime - start) > 1) {
    try {
      element.currentTime = start
    } catch {
      /* 元数据未就绪，忽略 */
    }
  }
}

function onNativeError() {
  const ext = (playing.value?.ext ?? '').toLowerCase()
  nativeError.value = NATIVE_UNSUPPORTED.includes(ext)
    ? t('gallery.video.unsupportedExt', { ext })
    : t('gallery.video.loadFailed')
  // 坏文件只 Toast + 就地提示，绝不让整页崩掉（设计文档 3.4.7 第 1 条）
  showToast(nativeError.value)
}

/** 坏文件的兜底出口：浏览器解不了就交给下载（设计文档 3.4.7 第 1 条） */
function downloadPlaying() {
  const item = playing.value
  if (!item) return
  const link = document.createElement('a')
  link.href = rawUrl(item.path)
  link.rel = 'noopener'
  document.body.appendChild(link)
  link.click()
  link.remove()
}

// 换片时清掉上一条错误提示
watch(playing, () => {
  nativeError.value = ''
})

function isFavorite(path: string) {
  return favorites.value.has(path)
}

async function toggleFavorite(item: MediaItem) {
  const next = !isFavorite(item.path)
  if (next) favorites.value.add(item.path)
  else favorites.value.delete(item.path)
  favorites.value = new Set(favorites.value)
  try {
    await libraryApi.setFavorite(item.path, next)
  } catch {
    // 失败回滚
    if (next) favorites.value.delete(item.path)
    else favorites.value.add(item.path)
    favorites.value = new Set(favorites.value)
  }
}

function setNativePlayer(value: boolean) {
  nativePlayer.value = value
  localStorage.setItem('asmoday.cinema.native', value ? '1' : '0')
}

let lastReport = 0
function savePosition(path: string, seconds: number) {
  void libraryApi.saveProgress(path, seconds).catch(() => {})
  progressMap.value.set(path, seconds)
}

function reportProgress(seconds: number) {
  const video = playing.value
  if (!video) return
  // 小于 5 秒的位置没有续播价值，忽略（防止卸载/误报的 0 覆盖已有进度）
  if (seconds < 5) return
  const now = Date.now()
  if (now - lastReport < 5000) return
  lastReport = now
  savePosition(video.path, seconds)
}

/** 播放结束：进度清零，下次从头看 */
function onPlayEnded(path: string) {
  lastReport = Date.now()
  savePosition(path, 0)
}

function resumeAt(path: string): number {
  return progressMap.value.get(path) ?? 0
}

function fmtSeconds(seconds: number): string {
  const total = Math.floor(seconds)
  const minutes = Math.floor(total / 60)
  return `${minutes}:${String(total % 60).padStart(2, '0')}`
}

const images = computed(() => items.value.filter((item) => item.kind === 'image'))
/** 照片分区只含图片与视频（音频归音乐库，否则时间线里会出现点不开的黑块） */
const photoItems = computed(() =>
  items.value.filter((item) => item.kind === 'image' || item.kind === 'video'),
)
const videos = computed(() =>
  items.value.filter((item) => item.kind === 'video' && (!onlyFavorites.value || isFavorite(item.path))),
)
const audios = computed(() =>
  items.value.filter((item) => item.kind === 'audio' && (!onlyFavorites.value || isFavorite(item.path))),
)

/** 音乐展示名：优先内嵌 ID3 标题 */
function trackTitle(item: MediaItem): string {
  if (item.title) return item.title
  // 无 ID3 标签时用「去扩展名的文件名」当歌名（设计文档 3.4.7 第 2 条）
  const stripped = item.name.replace(/\.[^.]+$/, '')
  return stripped || item.name
}

/** 音乐副标题：歌手 · 专辑，都没有时退回所在目录 */
function trackSubtitle(item: MediaItem): string {
  const parts = [item.artist, item.album].filter(Boolean) as string[]
  return parts.length > 0 ? parts.join(' · ') : item.folder
}

/** 内嵌封面加载失败的曲目（坏标签很常见），记住后不再重复请求 */
const failedCovers = ref(new Set<string>())

function markCoverFailed(path: string) {
  failedCovers.value.add(path)
}

function showCover(item: MediaItem): boolean {
  return Boolean(item.cover) && !failedCovers.value.has(item.path)
}

/** 把绝对路径裁成「相对挂载根」的展示名，避免界面泄漏服务器真实路径 */
function relativeFolder(folder: string): string {
  const base = root.value.replace(/\\/g, '/').replace(/\/+$/, '')
  const current = folder.replace(/\\/g, '/')
  if (base && current.toLowerCase().startsWith(base.toLowerCase())) {
    const rest = current.slice(base.length).replace(/^\/+/, '')
    return rest || t('gallery.photos.rootFolder')
  }
  return current.split('/').filter(Boolean).pop() ?? folder
}

/** 时间线：按 年-月 分组（图片 + 视频） */
const timeline = computed(() => {
  const groups = new Map<string, MediaItem[]>()
  for (const item of photoItems.value) {
    const date = item.modified ? new Date(item.modified * 1000) : null
    const key = date
      ? t('common.time.yearMonth', { year: date.getFullYear(), month: date.getMonth() + 1 })
      : t('common.time.unknown')
    const bucket = groups.get(key)
    if (bucket) bucket.push(item)
    else groups.set(key, [item])
  }
  return [...groups.entries()].map(([label, list]) => ({ label, items: list }))
})

/** 时间线排序：新的在前（后端已按 mtime 倒序，这里再稳一次分组内顺序） */
const timelineSorted = computed(() =>
  timeline.value.map((group) => ({
    ...group,
    items: [...group.items].sort((a, b) => (b.modified ?? 0) - (a.modified ?? 0)),
  })),
)

interface Album {
  folder: string
  name: string
  cover: MediaItem
  items: MediaItem[]
  count: number
}

/** 相册：按所在文件夹聚合 */
const albums = computed<Album[]>(() => {
  const groups = new Map<string, MediaItem[]>()
  for (const item of images.value) {
    const bucket = groups.get(item.folder)
    if (bucket) bucket.push(item)
    else groups.set(item.folder, [item])
  }
  return [...groups.entries()]
    .map(([folder, list]) => ({
      folder,
      name: relativeFolder(folder),
      cover: list[0],
      items: list,
      count: list.length,
    }))
    .sort((a, b) => b.count - a.count)
})

function openAlbum(album: Album) {
  activeAlbum.value = { folder: album.folder, name: album.name, items: album.items }
}

/** 点开图片/视频：交给全局灯箱（设计文档 3.2：严禁重复造轮子） */
function openViewer(list: MediaItem[], index: number) {
  openLightbox(
    list.map((item) => ({
      path: item.path,
      name: item.name,
      kind: item.kind,
      subtitle: item.subtitle ?? null,
    })),
    index,
    { onProgress: savePosition },
  )
}

function onKey(event: KeyboardEvent) {
  const target = event.target as HTMLElement | null
  // 输入框里不抢键
  if (target && /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName)) return

  if (playing.value) {
    if (event.key === 'Escape') playing.value = null
    return
  }
  if (event.key === 'Escape') activeAlbum.value = null
}

async function load() {
  loading.value = true
  error.value = ''
  try {
    const result = await browseMedia(undefined, 'media', 2)
    items.value = result.items
    root.value = result.root
    truncated.value = result.truncated
  } catch (err) {
    error.value = err instanceof Error ? err.message : t('gallery.toolbar.loadFailed')
  } finally {
    loading.value = false
  }
}

async function loadLibraryState() {
  try {
    const [favPaths, history] = await Promise.all([libraryApi.favorites(), libraryApi.history()])
    favorites.value = new Set(favPaths)
    progressMap.value = new Map(
      history
        .filter((entry) => (entry.progress ?? 0) > 0)
        .map((entry) => [entry.path, entry.progress ?? 0]),
    )
  } catch {
    /* 收藏/历史加载失败不阻塞画廊 */
  }
}

onMounted(() => {
  void load()
  void loadLibraryState()
  window.addEventListener('keydown', onKey)
})

onBeforeUnmount(() => {
  window.removeEventListener('keydown', onKey)
})
</script>

<template>
  <section class="flex h-full min-h-0 flex-col">
    <!-- 顶部：照片子视图 + 控制区 -->
    <header class="flex shrink-0 flex-wrap items-center gap-4 border-b border-line px-4 py-2 md:px-6">
      <div v-if="tab === 'photos'" class="flex items-center gap-6 text-sm font-medium">
        <button
          class="pb-2 transition-colors"
          :class="photoView === 'albums' ? 'border-b-2 border-accent text-ink' : 'text-muted hover:text-ink'"
          @click="photoView = 'albums'"
        >
          {{ t('gallery.photos.albums') }}
        </button>
        <button
          class="pb-2 transition-colors"
          :class="photoView === 'timeline' ? 'border-b-2 border-accent text-ink' : 'text-muted hover:text-ink'"
          @click="photoView = 'timeline'"
        >
          {{ t('gallery.photos.timeline') }}
        </button>
      </div>

      <div class="ml-auto flex items-center gap-2 text-[12px] text-muted">
        <span v-if="truncated" class="rounded bg-amber-500/15 px-2 py-0.5 fg-warn">
          {{ t('gallery.photos.truncated') }}
        </span>
        <button
          v-if="tab === 'cinema' || tab === 'music'"
          class="flex items-center gap-1 rounded-full border px-2 py-1 transition-colors"
          :class="onlyFavorites ? 'border-accent/50 text-accent' : 'border-line text-muted hover:text-ink'"
          :title="t('gallery.toolbar.onlyFavorites')"
          @click="onlyFavorites = !onlyFavorites"
        >
          <AppIcon name="heart" :size="12" />
          {{ t('gallery.toolbar.favorites') }}
        </button>
        <button
          v-if="tab === 'cinema'"
          class="rounded-full border px-2 py-1 transition-colors"
          :class="nativePlayer ? 'border-accent/50 text-accent' : 'border-line text-muted hover:text-ink'"
          :title="nativePlayer ? t('gallery.cinema.nativeOn') : t('gallery.cinema.nativeOff')"
          @click="setNativePlayer(!nativePlayer)"
        >
          {{ t('gallery.cinema.native') }}
        </button>
        <span class="hidden truncate md:inline">{{ t('gallery.toolbar.indexed') }}</span>
        <button
          class="grid size-7 place-items-center rounded-md transition-colors hover:bg-elevated hover:text-ink"
          :title="t('gallery.toolbar.rescan')"
          @click="load"
        >
          <AppIcon name="refresh" :size="14" />
        </button>
      </div>
    </header>

    <!-- 加载 / 错误 -->
    <div v-if="loading" class="min-h-0 flex-1 p-4">
      <div class="grid grid-cols-[repeat(auto-fill,minmax(160px,1fr))] gap-2">
        <div v-for="i in 12" :key="i" class="skeleton aspect-square rounded-xl" />
      </div>
    </div>

    <div v-else-if="error" class="grid min-h-0 flex-1 place-items-center px-6 text-center">
      <div>
        <span class="mb-3 inline-grid size-12 place-items-center rounded-full bg-elevated text-muted">
          <AppIcon name="alert" :size="22" />
        </span>
        <p class="text-sm text-ink">{{ t('gallery.toolbar.loadFailed') }}</p>
        <p class="mt-1 text-xs text-muted">{{ error }}</p>
      </div>
    </div>

    <!-- 照片 -->
    <div v-else-if="tab === 'photos'" class="min-h-0 flex-1">
      <div v-if="photoItems.length === 0" class="grid h-full place-items-center text-center">
        <div>
          <span class="mb-3 inline-grid size-12 place-items-center rounded-full bg-elevated text-muted">
            <AppIcon name="gallery" :size="22" />
          </span>
          <p class="text-sm text-ink">{{ t('gallery.photos.empty') }}</p>
          <p class="mt-1 text-xs text-muted">{{ t('gallery.photos.emptyHint') }}</p>
        </div>
      </div>

      <!-- 相册详情：点进去看这个目录下的全部照片 -->
      <div v-else-if="activeAlbum" class="h-full overflow-y-auto">
        <div
          class="sticky top-0 z-10 flex items-center gap-3 border-b border-line bg-base/85 px-4 py-2 backdrop-blur md:px-6"
        >
          <button
            class="flex shrink-0 items-center gap-1 rounded-md px-2 py-1 text-xs text-muted transition-colors hover:bg-elevated hover:text-ink"
            @click="activeAlbum = null"
          >
            <AppIcon name="arrowLeft" :size="13" />
            {{ t('gallery.photos.backToAlbums') }}
          </button>
          <div class="min-w-0 flex-1">
            <p class="truncate text-sm font-semibold text-ink">{{ activeAlbum.name }}</p>
            <p class="truncate text-[12px] text-muted">
              {{
                t('gallery.photos.albumMeta', {
                  count: activeAlbum.items.length,
                  folder: relativeFolder(activeAlbum.folder),
                })
              }}
            </p>
          </div>
          <button
            class="shrink-0 rounded-md px-2 py-1 text-xs text-muted transition-colors hover:bg-elevated hover:text-ink"
            @click="openViewer(activeAlbum.items, 0)"
          >
            {{ t('gallery.photos.slideshow') }}
          </button>
        </div>

        <div
          class="grid grid-cols-[repeat(auto-fill,minmax(110px,1fr))] gap-2 p-3 md:grid-cols-[repeat(auto-fill,minmax(170px,1fr))] md:gap-3 md:p-5"
        >
          <button
            v-for="(photo, index) in activeAlbum.items"
            :key="photo.path"
            class="group relative aspect-square overflow-hidden rounded-lg border border-transparent transition-colors hover:border-accent/40"
            :style="{ backgroundColor: placeholderFor(photo) }"
            :title="photo.name"
            @click="openViewer(activeAlbum.items, index)"
          >
            <img
              :src="thumbUrl(photo.path, 400)"
              class="size-full object-cover transition-transform duration-300 group-hover:scale-[1.04]"
              loading="lazy"
              alt=""
            />
          </button>
        </div>
      </div>

      <!-- 相册列表 -->
      <div v-else-if="photoView === 'albums'" class="h-full overflow-y-auto px-4 py-4 md:px-6">
        <div class="grid grid-cols-[repeat(auto-fill,minmax(150px,1fr))] gap-4 md:grid-cols-[repeat(auto-fill,minmax(200px,1fr))]">
          <button
            v-for="album in albums"
            :key="album.folder"
            class="group text-left"
            @click="openAlbum(album)"
          >
            <div
              class="relative aspect-[4/3] overflow-hidden rounded-xl border border-line"
              :style="{ backgroundColor: placeholderFor(album.cover) }"
            >
              <img
                :src="thumbUrl(album.cover.path, 480)"
                class="size-full object-cover transition-transform duration-300 group-hover:scale-[1.03]"
                loading="lazy"
                alt=""
              />
              <span
                class="absolute right-1.5 bottom-1.5 rounded-full bg-black/55 px-2 py-0.5 text-[11px] text-white backdrop-blur"
              >
                {{ t('gallery.photos.albumItems', { count: album.count }) }}
              </span>
            </div>
            <p class="mt-2 truncate text-sm font-medium text-ink">{{ album.name }}</p>
            <p class="truncate text-[12px] text-muted">
              {{
                t('gallery.photos.albumMeta', {
                  count: album.count,
                  folder: relativeFolder(album.folder),
                })
              }}
            </p>
          </button>
        </div>
      </div>

      <!-- 时间线：单滚轴 + content-visibility 跳过离屏渲染（万级条目也不卡） -->
      <div v-else class="h-full overflow-y-auto">
        <div v-for="group in timelineSorted" :key="group.label" class="mb-6">
          <div
            class="sticky top-0 z-10 border-b border-line/60 bg-base/85 px-4 py-2 text-sm font-semibold text-ink backdrop-blur md:px-6"
          >
            {{ group.label }}
            <span class="ml-2 text-[12px] font-normal text-muted">
              {{ t('gallery.photos.albumItems', { count: group.items.length }) }}
            </span>
          </div>
          <div
            class="grid grid-cols-[repeat(auto-fill,minmax(112px,1fr))] gap-1.5 p-2 md:grid-cols-[repeat(auto-fill,minmax(150px,1fr))] md:gap-2 md:p-3"
          >
            <button
              v-for="(item, index) in group.items"
              :key="item.path"
              class="group relative aspect-square overflow-hidden rounded-lg border border-transparent transition-colors hover:border-line"
              style="content-visibility: auto; contain-intrinsic-size: 150px"
              :style="{ backgroundColor: placeholderFor(item) }"
              @click="openViewer(group.items, index)"
            >
              <!-- 视频同样取后端首帧当封面（v2.8），失败才回落图标 -->
              <img
                v-if="!failedThumbs.has(item.path) && (item.kind === 'image' || item.kind === 'video')"
                :src="thumbUrl(item.path, 400)"
                class="size-full object-cover transition-transform duration-300 group-hover:scale-[1.03]"
                loading="lazy"
                alt=""
                @error="markThumbFailed(item.path)"
              />
              <span v-else class="grid size-full place-items-center text-white/70" :title="item.name">
                <AppIcon :name="item.kind === 'video' ? 'video' : 'gallery'" :size="26" />
              </span>
              <span
                v-if="item.kind === 'video'"
                class="absolute right-1 bottom-1 rounded bg-black/60 px-1.5 py-0.5 text-[11px] text-white backdrop-blur"
              >
                {{ t('gallery.photos.videoBadge') }}
              </span>
            </button>
          </div>
        </div>
      </div>
    </div>

    <!-- 影视库 -->
    <div v-else-if="tab === 'cinema'" class="min-h-0 flex-1 overflow-y-auto px-4 py-4 md:px-6 md:py-6">
      <div class="mb-4 flex items-center justify-between">
        <h1 class="text-xl font-bold text-ink md:text-2xl">{{ t('gallery.cinema.title') }}</h1>
        <span class="hidden text-sm text-muted md:block">
          {{ t('gallery.cinema.count', { count: videos.length }) }}
        </span>
      </div>
      <div v-if="videos.length === 0" class="grid h-full place-items-center text-center">
        <div>
          <span class="mb-3 inline-grid size-12 place-items-center rounded-full bg-elevated text-muted">
            <AppIcon name="video" :size="22" />
          </span>
          <p class="text-sm text-ink">{{ t('gallery.cinema.empty') }}</p>
          <p class="mt-1 text-xs text-muted">{{ t('gallery.cinema.emptyHint') }}</p>
        </div>
      </div>
      <div
        v-else
        class="grid grid-cols-[repeat(auto-fill,minmax(200px,1fr))] gap-4 md:grid-cols-[repeat(auto-fill,minmax(260px,1fr))]"
      >
        <button
          v-for="video in videos"
          :key="video.path"
          class="group text-left"
          @click="playing = video"
        >
          <div
            class="relative aspect-video overflow-hidden rounded-xl border border-line bg-black"
            :style="{ backgroundColor: placeholderFor(video) }"
          >
            <div class="absolute inset-0 grid place-items-center">
              <span
                class="grid size-11 place-items-center rounded-full bg-black/50 text-white backdrop-blur transition-transform group-hover:scale-110"
              >
                <AppIcon name="video" :size="18" />
              </span>
            </div>
            <span
              v-if="resumeAt(video.path) > 10"
              class="absolute left-1.5 bottom-1.5 rounded bg-black/70 px-1.5 py-0.5 text-[11px] text-white backdrop-blur"
            >
              {{ t('gallery.cinema.resume', { time: fmtSeconds(resumeAt(video.path)) }) }}
            </span>
            <span class="absolute right-1.5 bottom-1.5 rounded bg-black/70 px-1.5 py-0.5 text-[11px] text-white">
              {{ formatSize(video.size) }}
            </span>
            <button
              class="absolute right-1.5 top-1.5 grid size-7 place-items-center rounded-full bg-black/45 text-white/80 backdrop-blur transition-colors hover:text-white"
              :class="isFavorite(video.path) ? 'fg-danger' : ''"
              :title="isFavorite(video.path) ? t('gallery.favorite.remove') : t('gallery.favorite.add')"
              @click.stop="toggleFavorite(video)"
            >
              <AppIcon name="heart" :size="13" :class="isFavorite(video.path) ? 'fill-current' : ''" />
            </button>
          </div>
          <p class="mt-2 truncate text-sm font-medium text-ink">{{ video.name }}</p>
          <p class="truncate text-[12px] text-muted">{{ video.folder }}</p>
        </button>
      </div>
    </div>

    <!-- 音乐库（原型版式：封面头 + 播放全部 + 曲目表） -->
    <div v-else class="min-h-0 flex-1 overflow-y-auto px-4 py-4 pb-24 md:px-6 md:py-6">
      <div v-if="audios.length === 0" class="grid h-full place-items-center text-center">
        <div>
          <span class="mb-3 inline-grid size-12 place-items-center rounded-full bg-elevated text-muted">
            <AppIcon name="audio" :size="22" />
          </span>
          <p class="text-sm text-ink">{{ t('gallery.music.empty') }}</p>
          <p class="mt-1 text-xs text-muted">{{ t('gallery.music.emptyHint') }}</p>
        </div>
      </div>

      <div v-else class="mx-auto max-w-4xl">
        <!-- 库头部 -->
        <div class="mb-6 flex flex-col items-center gap-4 md:mb-8 md:flex-row md:items-end md:gap-8">
          <div
            class="grid size-32 shrink-0 place-items-center rounded-xl bg-gradient-to-br from-indigo-500 to-purple-600 text-white shadow-xl md:size-40"
          >
            <AppIcon name="audio" :size="44" class="opacity-60" />
          </div>
          <div class="text-center md:text-left">
            <p class="mb-1 text-xs font-bold tracking-wide text-muted uppercase">
              {{ t('gallery.music.title') }}
            </p>
            <h1 class="mb-2 text-2xl font-bold text-ink md:mb-4 md:text-3xl">
              {{ t('gallery.music.count', { count: audios.length }) }}
            </h1>
            <button
              class="inline-flex items-center rounded-full bg-accent px-5 py-2 text-sm font-medium text-white transition-opacity hover:opacity-90"
              @click="player.play(audios, 0)"
            >
              <AppIcon name="play" :size="14" class="mr-2" />
              {{ t('gallery.music.playAll') }}
            </button>
          </div>
        </div>

        <!-- 曲目表 -->
        <table class="w-full text-left text-sm">
          <thead class="border-b border-line text-xs text-muted">
            <tr>
              <th class="w-10 px-2 py-2 font-normal md:px-3">
                {{ t('gallery.music.columnIndex') }}
              </th>
              <th class="px-2 py-2 font-normal">{{ t('gallery.music.columnTitle') }}</th>
              <th class="hidden w-28 px-3 py-2 text-right font-normal md:table-cell">
                {{ t('gallery.music.columnSize') }}
              </th>
              <th class="w-14 px-2 py-2 text-right font-normal">
                {{ t('gallery.music.columnFavorite') }}
              </th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="(track, index) in audios"
              :key="track.path"
              class="group cursor-pointer transition-colors hover:bg-elevated/50"
              @click="player.play(audios, index)"
            >
              <td class="px-2 py-3 md:px-3">
                <span class="group-hover:hidden" :class="player.isCurrent(track.path) ? 'text-accent' : ''">
                  {{ index + 1 }}
                </span>
                <AppIcon
                  v-if="player.isCurrent(track.path) && player.state.playing"
                  name="pause"
                  :size="14"
                  class="hidden text-accent group-hover:block"
                />
                <AppIcon v-else name="play" :size="14" class="hidden text-accent group-hover:block" />
              </td>
              <td class="px-2 py-3">
                <div class="flex items-center gap-3">
                  <span
                    class="relative grid size-10 shrink-0 place-items-center overflow-hidden rounded text-white/80"
                    :style="{ backgroundColor: placeholderColor(track.path) }"
                  >
                    <AppIcon v-if="!showCover(track)" name="audio" :size="16" />
                    <img
                      v-else
                      :src="coverUrl(track.path)"
                      alt=""
                      loading="lazy"
                      class="size-full object-cover"
                      @error="markCoverFailed(track.path)"
                    />
                  </span>
                  <span class="min-w-0">
                    <span
                      class="block truncate font-medium"
                      :class="player.isCurrent(track.path) ? 'text-accent' : 'text-ink'"
                    >
                      {{ trackTitle(track) }}
                      <span v-if="player.isCurrent(track.path) && player.state.playing" class="playing-wave">
                        <span /><span /><span />
                      </span>
                    </span>
                    <span class="block truncate text-xs text-muted">
                      {{ trackSubtitle(track) }}
                    </span>
                  </span>
                </div>
              </td>
              <td class="hidden px-3 py-3 text-right text-xs text-muted md:table-cell">
                {{ formatSize(track.size) }}
              </td>
              <td class="px-2 py-3 text-right">
                <button
                  class="rounded-md p-1.5 transition-colors"
                  :class="isFavorite(track.path) ? 'fg-danger' : 'text-muted hover:text-ink'"
                  :title="isFavorite(track.path) ? t('gallery.favorite.remove') : t('gallery.favorite.add')"
                  @click.stop="toggleFavorite(track)"
                >
                  <AppIcon name="heart" :size="14" :class="isFavorite(track.path) ? 'fill-current' : ''" />
                </button>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>

    <!-- 播放器（影视库） -->
    <div
      v-if="playing"
      class="fixed inset-0 z-50 flex items-center justify-center bg-black/95 p-4 md:p-10"
      @click.self="playing = null"
    >
      <div class="w-full max-w-5xl">
        <p class="mb-2 truncate text-center text-sm text-white/80">{{ playing.name }}</p>
        <template v-if="nativePlayer">
          <!-- 降级开关：浏览器原生 <video>（设计文档 3.4 播放器选择） -->
          <video
            :key="playing.path"
            :src="rawUrl(playing.path)"
            controls
            autoplay
            playsinline
            class="max-h-[78vh] w-full rounded-xl bg-black"
            @loadedmetadata="seekNative"
            @error="onNativeError"
            @timeupdate="reportProgress(($event.target as HTMLVideoElement).currentTime)"
            @ended="onPlayEnded(playing.path)"
          />
          <p v-if="nativeError" class="mt-2 text-center text-xs fg-warn">
            {{ nativeError }}
            <button
              class="ml-2 rounded border border-amber-300/40 px-2 py-0.5 text-[12px] fg-warn transition-colors hover:bg-amber-300/10"
              @click="downloadPlaying"
            >
              {{ t('gallery.video.tryDownload') }}
            </button>
          </p>
        </template>
        <MediaPlayer
          v-else
          :key="playing.path"
          :src="rawUrl(playing.path)"
          :subtitle="playing.subtitle"
          :start-at="resumeAt(playing.path)"
          @progress="reportProgress"
          @close="playing = null"
          @ended="onPlayEnded(playing.path)"
        />
      </div>
    </div>

    <!-- 轻提示（坏文件 / 不支持格式） -->
    <Transition name="fade">
      <div
        v-if="toast"
        class="pointer-events-none fixed bottom-20 left-1/2 z-[60] -translate-x-1/2 rounded-full border border-line bg-panel px-4 py-2 text-xs text-ink shadow-lg"
      >
        {{ toast }}
      </div>
    </Transition>
  </section>
</template>
