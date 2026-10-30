<script setup lang="ts">
/**
 * 媒体画廊：照片（相册 / 时间线）+ 影视库 + 音乐库。
 *
 * 视频播放器由 defineAsyncComponent 按需加载（可在「原生播放器」降级开关切换）；
 * 照片网格用虚拟列表渲染；音乐走全局悬浮播放条（跨页面不中断）。
 */
import { useEventListener } from '@vueuse/core'
import { computed, defineAsyncComponent, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { useRoute } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import ContextMenu, { type ContextMenuItem } from '../components/ContextMenu.vue'
import PhotoTile from '../components/PhotoTile.vue'
import VideoCard from '../components/VideoCard.vue'
import {
  browseAlbums,
  browseMedia,
  coverUrl,
  libraryApi,
  placeholderColor,
  rawUrl,
  thumbUrl,
  type Album,
  type KindCounts,
  type MediaItem,
} from '../api/media'
import { sharesApi } from '../api/shares'
import { formatSize } from '../lib/format'
import { triggerDownload } from '../lib/download'
import { useScrollLock } from '../composables/useScrollLock'
import { useBackClose } from '../composables/useBackClose'
import { player } from '../stores/player'
import { openLightbox } from '../stores/lightbox'
import { openShare } from '../stores/share'
import { thumbHashToDataUrl } from '../lib/thumbhash'

const MediaPlayer = defineAsyncComponent(() => import('../components/MediaPlayer.vue'))
const { t } = useI18n()

type MediaTab = 'photos' | 'cinema' | 'music'

// 媒体分区由路由驱动（/photos /cinema /music 共用本组件，见 router 的 meta.mediaTab）
const route = useRoute()
const tab = ref<MediaTab>(route.meta.mediaTab ?? 'photos')
watch(
  () => route.meta.mediaTab,
  (value) => {
    if (value === 'photos' || value === 'cinema' || value === 'music') {
      tab.value = value
      // 组件在三个路由间复用，挂载时只 load 过一次：影视库的文件夹卡片按需补拉
      if (value === 'cinema' && cinemaAlbums.value.length === 0 && !cinemaAlbumsLoading.value) {
        void loadCinemaAlbums()
      }
    }
  },
)

const items = ref<MediaItem[]>([])
const root = ref('')
const loading = ref(true)
const error = ref('')

// ---------------- 游标分页（设计文档 3.4） ----------------
//
// 媒体库和文件管理同一口径：**没有扫描上限**，翻到最后一页为止。一页 300 条
// （首屏 JSON 从「整个库」降到几百 KB），触底续接下一页，游标记的是
// 「上一页最后一条的 (mtime, 路径)」，扫描期间新增/删除文件也不会重复或漏项。
const PAGE_SIZE = 300
const nextCursor = ref<string | null>(null)
const hasMore = ref(false)
const loadingMore = ref(false)
const total = ref(0)
/** 各类型总数：不受「已经翻了几页」影响，切分区时数字必须是对的 */
const counts = ref<KindCounts>({ image: 0, video: 0, audio: 0 })
/** 已进列表的路径：续接时去重，防止边界上（同一 mtime）出现重复卡片 */
const loadedPaths = new Set<string>()

/** 追加一页，按路径去重并保持后端给的新→旧顺序 */
function appendItems(list: MediaItem[]) {
  const fresh = list.filter((item) => {
    if (loadedPaths.has(item.path)) return false
    loadedPaths.add(item.path)
    return true
  })
  if (fresh.length) items.value = [...items.value, ...fresh]
}

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
  observer?.disconnect()
  observer = null
})

// 影视库播放器是整屏遮罩，锁住底层滚动（设计文档 8.4）；灯箱自己管自己的锁
useScrollLock(computed(() => Boolean(playing.value)))

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
  triggerDownload(rawUrl(item.path, { download: true }))
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

/**
 * 分页进度（头部那枚小标签）：按**当前分区**取分子分母。
 *
 * 媒体流是按 mtime 混排的一条，所以影视库的「已加载」必须只数视频——
 * 否则会出现「影视库显示已加载 300 / 共 1040」这种把图片算进来的怪数字。
 */
const progress = computed(() => {
  if (tab.value === 'cinema') return { loaded: videos.value.length, total: counts.value.video }
  if (tab.value === 'music') return { loaded: audios.value.length, total: counts.value.audio }
  return { loaded: photoItems.value.length, total: counts.value.image + counts.value.video }
})

/** 相册视图看的是卡片，不是媒体流，这枚进度标签没有意义 */
const showProgress = computed(() => {
  if (tab.value === 'photos' && photoView.value === 'albums') return false
  // 影视库现在是「文件夹卡片 → 册内影片」两层，分子分母都跟媒体流对不上（v4.4.8）
  if (tab.value === 'cinema') return false
  return progress.value.loaded < progress.value.total
})

/** 音乐副标题：歌手 · 专辑；没有标签就留空——目录位置不上桌面（v3.5.5） */
function trackSubtitle(item: MediaItem): string {
  const parts = [item.artist, item.album].filter(Boolean) as string[]
  return parts.join(' · ')
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

/** 按所在目录分组（影视库/音乐库的「按文件夹」分区，v3.5.5） */
interface FolderGroup {
  key: string
  label: string
  items: MediaItem[]
}
function groupByFolder(list: MediaItem[]): FolderGroup[] {
  const map = new Map<string, FolderGroup>()
  for (const item of list) {
    const key = item.folder || '/'
    let group = map.get(key)
    if (!group) {
      group = { key, label: relativeFolder(key), items: [] }
      map.set(key, group)
    }
    group.items.push(item)
  }
  // 根目录排最前，其余按目录名排序
  const rootLabel = t('gallery.photos.rootFolder')
  return [...map.values()].sort((a, b) => {
    if (a.label === rootLabel) return -1
    if (b.label === rootLabel) return 1
    return a.label.localeCompare(b.label, 'zh-Hans-CN')
  })
}
const audioGroups = computed(() => groupByFolder(audios.value))
/** 曲目在完整列表里的行号（分组渲染后仍按全局序号播放） */
const audioRowIndex = computed(() => {
  const map = new Map<string, number>()
  audios.value.forEach((track, index) => map.set(track.path, index))
  return map
})

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

// ---------------- 照片库双视图（设计文档 3.4.3） ----------------
//
// 相册视图 = 子文件夹卡片（封面取册内最新一张图，空册由后端剔除）；
// 时间线视图 = 忽略目录层级、按月吸顶的全部照片。两个视图平级，用户显式切换，
// 绝不允许以「优化」为由只留一种（设计文档 1.4 第 9 条）。
type PhotoView = 'albums' | 'timeline'
const PHOTO_VIEW_KEY = 'asmoday.photos.view'
const photoView = ref<PhotoView>(
  localStorage.getItem(PHOTO_VIEW_KEY) === 'timeline' ? 'timeline' : 'albums',
)

const albums = ref<Album[]>([])
const albumsLoading = ref(false)
const albumsError = ref('')
/** 相册卡片同样分页（一个媒体根下几千本相册是常态） */
const albumsCursor = ref<string | null>(null)
const albumsHasMore = ref(false)
const albumsTotal = ref(0)
/** 已经点进去的那本相册；非空时相册视图展示册内图片 */
const activeAlbum = ref<Album | null>(null)
const albumItems = ref<MediaItem[]>([])
const albumLoading = ref(false)
const albumCursor = ref<string | null>(null)
const albumHasMore = ref(false)
/** 相册封面加载失败（坏图）时回退纯色，不再重复请求 */
const albumCoverFailed = ref(new Set<string>())

async function loadAlbums(refresh = false) {
  if (albumsLoading.value) return
  albumsLoading.value = true
  albumsError.value = ''
  try {
    const result = await browseAlbums(undefined, 3, { limit: PAGE_SIZE, refresh })
    albums.value = result.items
    albumsTotal.value = result.total
    albumsCursor.value = result.next_cursor ?? null
    albumsHasMore.value = result.has_more && Boolean(result.next_cursor)
  } catch (err) {
    albumsError.value = err instanceof Error ? err.message : t('gallery.toolbar.loadFailed')
  } finally {
    albumsLoading.value = false
  }
}

async function loadMoreAlbums() {
  const cursor = albumsCursor.value
  if (!albumsHasMore.value || !cursor || albumsLoading.value || loadingMore.value) return
  loadingMore.value = true
  try {
    const result = await browseAlbums(undefined, 3, { limit: PAGE_SIZE, cursor })
    const seen = new Set(albums.value.map((album) => album.path))
    albums.value = [...albums.value, ...result.items.filter((album) => !seen.has(album.path))]
    albumsTotal.value = result.total
    albumsCursor.value = result.next_cursor ?? null
    albumsHasMore.value = result.has_more && Boolean(result.next_cursor)
  } catch (err) {
    showToast(err instanceof Error ? err.message : t('gallery.toolbar.loadFailed'))
  } finally {
    loadingMore.value = false
  }
}

function setPhotoView(view: PhotoView) {
  if (photoView.value === view) return
  photoView.value = view
  localStorage.setItem(PHOTO_VIEW_KEY, view)
  closeAlbum()
  if (view === 'albums' && albums.value.length === 0) void loadAlbums()
}

async function openAlbum(album: Album) {
  activeAlbum.value = album
  albumLoading.value = true
  albumItems.value = []
  albumCursor.value = null
  albumHasMore.value = false
  try {
    // 只列册内第一层（depth=0 = 不进子目录，与后端统计 count 的口径一致，
    // v2.8 相册视图同款；递归会把子目录的图也算进来，条数就对不上了）
    const result = await browseMedia(album.path, 'image', 0, { limit: PAGE_SIZE })
    albumItems.value = result.items
    albumCursor.value = result.next_cursor ?? null
    albumHasMore.value = result.has_more && Boolean(result.next_cursor)
  } catch (err) {
    showToast(err instanceof Error ? err.message : t('gallery.toolbar.loadFailed'))
  } finally {
    albumLoading.value = false
  }
}

async function loadMoreAlbumItems() {
  const album = activeAlbum.value
  const cursor = albumCursor.value
  if (!album || !albumHasMore.value || !cursor || loadingMore.value) return
  loadingMore.value = true
  try {
    const result = await browseMedia(album.path, 'image', 0, { limit: PAGE_SIZE, cursor })
    const seen = new Set(albumItems.value.map((item) => item.path))
    albumItems.value = [...albumItems.value, ...result.items.filter((item) => !seen.has(item.path))]
    albumCursor.value = result.next_cursor ?? null
    albumHasMore.value = result.has_more && Boolean(result.next_cursor)
  } catch (err) {
    showToast(err instanceof Error ? err.message : t('gallery.toolbar.loadFailed'))
  } finally {
    loadingMore.value = false
  }
}

function closeAlbum() {
  activeAlbum.value = null
  albumItems.value = []
  albumLoading.value = false
  albumCursor.value = null
  albumHasMore.value = false
}

// ---------------- 影视库文件夹（v4.4.8：与照片相册同构） ----------------
//
// 照片库的相册卡片来自 `/api/media/albums?kind=image`——后端统一算「直接含这类媒体的
// 目录」，count 只数第一层。影视库此前是在前端拿「已加载那几页里每个视频的 folder」
// 现场分区，两边口径完全不一致：同一棵树，图片是一张卡片，视频却按完整相对路径各占
// 一区，深目录还会把同一部剧拆成好几段。现在同样走 albums 接口（kind=video），
// 层级、计数、卡片样式都与照片库对齐。
const cinemaAlbums = ref<Album[]>([])
const cinemaAlbumsLoading = ref(false)
const cinemaAlbumsError = ref('')
const cinemaAlbumsCursor = ref<string | null>(null)
const cinemaAlbumsHasMore = ref(false)
const cinemaAlbumsTotal = ref(0)
const activeCinemaAlbum = ref<Album | null>(null)
const cinemaItems = ref<MediaItem[]>([])
const cinemaLoading = ref(false)
const cinemaCursor = ref<string | null>(null)
const cinemaHasMore = ref(false)
const cinemaCoverFailed = ref(new Set<string>())

function markCinemaCoverFailed(path: string) {
  cinemaCoverFailed.value = new Set(cinemaCoverFailed.value).add(path)
}

async function loadCinemaAlbums(refresh = false) {
  if (cinemaAlbumsLoading.value) return
  cinemaAlbumsLoading.value = true
  cinemaAlbumsError.value = ''
  try {
    const result = await browseAlbums(undefined, 3, { kind: 'video', limit: PAGE_SIZE, refresh })
    cinemaAlbums.value = result.items
    cinemaAlbumsTotal.value = result.total
    cinemaAlbumsCursor.value = result.next_cursor ?? null
    cinemaAlbumsHasMore.value = result.has_more && Boolean(result.next_cursor)
  } catch (err) {
    cinemaAlbumsError.value = err instanceof Error ? err.message : t('gallery.toolbar.loadFailed')
  } finally {
    cinemaAlbumsLoading.value = false
  }
}

async function loadMoreCinemaAlbums() {
  const cursor = cinemaAlbumsCursor.value
  if (!cinemaAlbumsHasMore.value || !cursor || cinemaAlbumsLoading.value || loadingMore.value) return
  loadingMore.value = true
  try {
    const result = await browseAlbums(undefined, 3, { kind: 'video', limit: PAGE_SIZE, cursor })
    const seen = new Set(cinemaAlbums.value.map((album) => album.path))
    cinemaAlbums.value = [
      ...cinemaAlbums.value,
      ...result.items.filter((album) => !seen.has(album.path)),
    ]
    cinemaAlbumsTotal.value = result.total
    cinemaAlbumsCursor.value = result.next_cursor ?? null
    cinemaAlbumsHasMore.value = result.has_more && Boolean(result.next_cursor)
  } catch (err) {
    showToast(err instanceof Error ? err.message : t('gallery.toolbar.loadFailed'))
  } finally {
    loadingMore.value = false
  }
}

async function openCinemaAlbum(album: Album) {
  activeCinemaAlbum.value = album
  cinemaLoading.value = true
  cinemaItems.value = []
  cinemaCursor.value = null
  cinemaHasMore.value = false
  try {
    // depth=0：只列这一层，与卡片上的 count 口径一致（同照片相册）
    const result = await browseMedia(album.path, 'video', 0, { limit: PAGE_SIZE })
    cinemaItems.value = result.items
    cinemaCursor.value = result.next_cursor ?? null
    cinemaHasMore.value = result.has_more && Boolean(result.next_cursor)
  } catch (err) {
    showToast(err instanceof Error ? err.message : t('gallery.toolbar.loadFailed'))
  } finally {
    cinemaLoading.value = false
  }
}

async function loadMoreCinemaItems() {
  const album = activeCinemaAlbum.value
  const cursor = cinemaCursor.value
  if (!album || !cinemaHasMore.value || !cursor || loadingMore.value) return
  loadingMore.value = true
  try {
    const result = await browseMedia(album.path, 'video', 0, { limit: PAGE_SIZE, cursor })
    const seen = new Set(cinemaItems.value.map((item) => item.path))
    cinemaItems.value = [...cinemaItems.value, ...result.items.filter((item) => !seen.has(item.path))]
    cinemaCursor.value = result.next_cursor ?? null
    cinemaHasMore.value = result.has_more && Boolean(result.next_cursor)
  } catch (err) {
    showToast(err instanceof Error ? err.message : t('gallery.toolbar.loadFailed'))
  } finally {
    loadingMore.value = false
  }
}

function closeCinemaAlbum() {
  activeCinemaAlbum.value = null
  cinemaItems.value = []
  cinemaLoading.value = false
  cinemaCursor.value = null
  cinemaHasMore.value = false
}

/** 影视文件夹标题：根目录那本显示「根目录」，其余用目录名（与照片相册同口径） */
function cinemaAlbumLabel(album: Album): string {
  return album.is_root ? t('gallery.photos.rootFolder') : album.name
}

function cinemaAlbumSubtitle(album: Album): string {
  return t('gallery.cinema.albumItems', { count: album.count })
}

function cinemaAlbumColor(album: Album): string {
  return album.cover_color ?? placeholderColor(album.path)
}

function cinemaAlbumHash(album: Album): string | null {
  return thumbHashToDataUrl(album.cover_hash)
}

// 相册详情与视频全屏在真机上都是一层历史：返回键先退这一层，
// 而不是把整个媒体库都退掉（此前按返回直接离开页面，返回过后还要重新载入列表）
useBackClose(computed(() => Boolean(activeAlbum.value)), closeAlbum)
useBackClose(computed(() => Boolean(activeCinemaAlbum.value)), closeCinemaAlbum)
useBackClose(computed(() => Boolean(playing.value)), () => {
  playing.value = null
})

/** 当前正在浏览的照片列表：相册详情与时间线各自独立 */
function currentPhotoList(): MediaItem[] {
  return photoView.value === 'albums' && activeAlbum.value ? albumItems.value : photoItems.value
}

function albumColor(album: Album): string {
  return album.cover_color ?? placeholderColor(album.path)
}

/** 相册封面也没有缩略图时（首次浏览），用封面 ThumbHash 打底 */
function albumHash(album: Album): string | null {
  return thumbHashToDataUrl(album.cover_hash)
}

/**
 * 相册标题（v2.8 口径）：挂载目录本身那本册显示「根目录」，其余用目录名。
 */
function albumLabel(album: Album): string {
  return album.is_root ? t('gallery.photos.rootFolder') : album.name
}

/**
 * 相册副标题（对齐 v2.9 的「N 张 · 路径」）。
 * 根目录相册与册名重复时只显示数量，避免「根目录 · 根目录」。
 */
function albumSubtitle(album: Album): string {
  if (album.is_root) return t('gallery.photos.albumItems', { count: album.count })
  const folder = relativeFolder(album.path)
  return folder === album.name
    ? t('gallery.photos.albumItems', { count: album.count })
    : t('gallery.photos.albumMeta', { count: album.count, folder })
}

function markAlbumCoverFailed(path: string) {
  albumCoverFailed.value = new Set(albumCoverFailed.value).add(path)
}

/** 点开图片/视频：交给全局灯箱（设计文档 3.2：严禁重复造轮子） */
function openViewer(list: MediaItem[], index: number) {
  openLightbox(
    list.map((item) => ({
      path: item.path,
      name: item.name,
      kind: item.kind,
      subtitle: item.subtitle ?? null,
      size: item.size,
      modified: item.modified,
    })),
    index,
    { onProgress: savePosition },
  )
}

// ---------------- 媒体库右键菜单（设计文档 3.2.5 / 8.4.13） ----------------
//
// 相册时间线、影视海报墙、音乐列表都复用全局 ContextMenu —— 媒体库不允许
// 变成「只能看不能操作」的死水，下载 / 分享 / 复制直链 / 收藏必须在原地可达。
const mediaMenu = ref<{ x: number; y: number; item: MediaItem; context: MediaTab } | null>(null)

function openMediaMenu(event: MouseEvent, item: MediaItem, context: MediaTab) {
  mediaMenu.value = { x: event.clientX, y: event.clientY, item, context }
}

const mediaMenuItems = computed<ContextMenuItem[]>(() => {
  const current = mediaMenu.value
  if (!current) return []
  const item = current.item
  const items: ContextMenuItem[] = []
  items.push({
    id: 'open',
    label:
      current.context === 'photos' ? t('files.mediaMenu.open') : t('files.mediaMenu.play'),
    icon: current.context === 'photos' ? 'eye' : 'play',
  })
  items.push({ id: 'download', label: t('files.mediaMenu.download'), icon: 'download' })
  items.push({ id: 'share', label: t('files.mediaMenu.share'), icon: 'share' })
  if (item.kind === 'image') {
    items.push({ id: 'directLink', label: t('files.mediaMenu.directLink'), icon: 'link' })
  }
  items.push({
    id: 'favorite',
    label: isFavorite(item.path) ? t('gallery.favorite.remove') : t('gallery.favorite.add'),
    icon: 'heart',
    divider: true,
  })
  return items
})

async function onMediaMenuSelect(id: string) {
  const current = mediaMenu.value
  mediaMenu.value = null
  if (!current) return
  const item = current.item
  switch (id) {
    case 'open': {
      if (current.context === 'photos') {
        const list = currentPhotoList()
        const index = list.findIndex((entry) => entry.path === item.path)
        openViewer(list, index > 0 ? index : 0)
      } else if (current.context === 'cinema') {
        playing.value = item
      } else {
        const index = audios.value.findIndex((entry) => entry.path === item.path)
        player.play(audios.value, index > 0 ? index : 0)
      }
      break
    }
    case 'download':
      window.open(rawUrl(item.path), '_blank', 'noopener')
      break
    case 'share':
      openShare(item.path, item.name)
      break
    case 'directLink':
      await copyDirectLink(item)
      break
    case 'favorite':
      await toggleFavorite(item)
      break
    default:
      break
  }
}

/** 图床直链：永久、无口令的 raw 分享地址（与文件管理里的同名动作一致） */
async function copyDirectLink(item: MediaItem) {
  try {
    const created = await sharesApi.create(item.path, { days: 0, allow_download: true })
    await navigator.clipboard.writeText(`${location.origin}${sharesApi.rawUrl(created.token)}`)
    showToast(t('files.menu.directLinkCopied'))
  } catch (err) {
    showToast(err instanceof Error ? err.message : t('files.menu.directLinkFailed'))
  }
}

function onKey(event: KeyboardEvent) {
  const target = event.target as HTMLElement | null
  // 输入框里不抢键
  if (target && /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName)) return

  if (playing.value) {
    if (event.key === 'Escape') playing.value = null
    return
  }
  // 相册详情里按 Esc 退回相册列表（对齐 v2.9）；灯箱会先消费掉 Esc 并 preventDefault，
  // 这里判 defaultPrevented，避免一次 Esc 连退两层
  if (event.key === 'Escape' && !event.defaultPrevented && activeAlbum.value) {
    closeAlbum()
  }
  if (event.key === 'Escape' && !event.defaultPrevented && activeCinemaAlbum.value) {
    closeCinemaAlbum()
  }
}

/**
 * 首屏加载。`refresh` 只由工具栏的「重新扫描」按钮传 true：
 * 后端其余时候都读目录快照（毫秒级），只有用户明确要求时才整树重扫。
 */
async function load(options: { refresh?: boolean } = {}) {
  loading.value = true
  error.value = ''
  try {
    const result = await browseMedia(undefined, 'media', 2, {
      limit: PAGE_SIZE,
      refresh: options.refresh,
    })
    items.value = []
    loadedPaths.clear()
    appendItems(result.items)
    root.value = result.root
    total.value = result.total
    counts.value = result.counts
    nextCursor.value = result.next_cursor ?? null
    hasMore.value = result.has_more && Boolean(result.next_cursor)
    // 照片库同时刷新相册清单（后端会剔除空相册）
    if (tab.value === 'photos') {
      closeAlbum()
      void loadAlbums(options.refresh)
    }
    // 影视库同构：进页面就把文件夹卡片拉出来
    if (tab.value === 'cinema') {
      closeCinemaAlbum()
      void loadCinemaAlbums(options.refresh)
    }
  } catch (err) {
    error.value = err instanceof Error ? err.message : t('gallery.toolbar.loadFailed')
  } finally {
    loading.value = false
  }
}

async function loadMoreMedia() {
  const cursor = nextCursor.value
  if (!hasMore.value || !cursor || loading.value || loadingMore.value) return
  loadingMore.value = true
  try {
    const result = await browseMedia(undefined, 'media', 2, { limit: PAGE_SIZE, cursor })
    appendItems(result.items)
    total.value = result.total
    counts.value = result.counts
    nextCursor.value = result.next_cursor ?? null
    hasMore.value = result.has_more && Boolean(result.next_cursor)
  } catch (err) {
    showToast(err instanceof Error ? err.message : t('gallery.toolbar.loadFailed'))
  } finally {
    loadingMore.value = false
  }
}

/** 触底续接：按当前视图决定续哪条流（媒体列表 / 相册卡片 / 册内图片） */
async function loadMore() {
  if (loading.value || loadingMore.value) return
  if (tab.value === 'photos' && photoView.value === 'albums') {
    if (activeAlbum.value) await loadMoreAlbumItems()
    else await loadMoreAlbums()
    return
  }
  if (tab.value === 'cinema') {
    if (activeCinemaAlbum.value) await loadMoreCinemaItems()
    else await loadMoreCinemaAlbums()
    return
  }
  await loadMoreMedia()
}

// ---------------- 触底观察器 ----------------
//
// 每个可滚动列表末尾放一个 1px 的 sentinel；只 observe 当前挂载的那一个
// （v-if 分支互斥，模板 ref 同时只有一个实例）。
const sentinel = ref<HTMLElement | null>(null)
let observer: IntersectionObserver | null = null

function observeSentinel(el: HTMLElement | null) {
  observer?.disconnect()
  if (el) observer?.observe(el)
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
  observer = new IntersectionObserver(
    (entries) => {
      if (entries.some((entry) => entry.isIntersecting)) void loadMore()
    },
    // 提前一屏预取：等真的滚到底才请求，用户会看到明显的空白等待
    { rootMargin: '800px 0px' },
  )
  observeSentinel(sentinel.value)
  void load()
  void loadLibraryState()
})

// 切换视图会换掉 sentinel 元素（v-if 分支互斥），这里跟着重新 observe
watch(sentinel, (el) => observeSentinel(el), { flush: 'post' })

// 快捷键：挂载/卸载由 useEventListener 成对处理
useEventListener(window, 'keydown', onKey)
</script>

<template>
  <section class="flex h-full min-h-0 flex-col">
    <!-- 顶部：照片子视图 + 控制区 -->
    <header class="flex shrink-0 flex-wrap items-center gap-4 border-b border-line px-4 py-2 md:px-6">
      <!-- 照片库双视图（设计文档 3.4.3）：[相册] 与 [时间线] 是两个平级视图，缺一不可 -->
      <div
        v-if="tab === 'photos'"
        class="flex items-center gap-0.5 rounded-lg border border-line p-0.5 text-xs"
        data-testid="photo-view-switch"
      >
        <button
          class="rounded px-2.5 py-1 transition-colors"
          :class="
            photoView === 'albums'
              ? 'bg-accent-soft text-accent'
              : 'text-muted hover:text-ink'
          "
          @click="setPhotoView('albums')"
        >
          {{ t('gallery.photos.albums') }}
        </button>
        <button
          class="rounded px-2.5 py-1 transition-colors"
          :class="
            photoView === 'timeline'
              ? 'bg-accent-soft text-accent'
              : 'text-muted hover:text-ink'
          "
          @click="setPhotoView('timeline')"
        >
          {{ t('gallery.photos.timeline') }}
        </button>
      </div>

      <div class="ml-auto flex items-center gap-2 text-[12px] text-muted">
        <!-- 分页进度：翻到一半时也能看出库里还有多少（分母是后端给的真实总数） -->
        <span
          v-if="showProgress"
          class="hidden rounded bg-accent-soft px-2 py-0.5 text-accent md:inline"
        >
          {{ t('gallery.toolbar.loadedOf', { loaded: progress.loaded, total: progress.total }) }}
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
          @click="load({ refresh: true })"
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

    <!-- 照片：相册 / 时间线 两个平级视图（设计文档 3.4.3） -->
    <div v-else-if="tab === 'photos'" class="min-h-0 flex-1">
      <!-- 相册详情：只看当前这本相册里的图片 -->
      <div v-if="photoView === 'albums' && activeAlbum" class="flex h-full flex-col">
        <!-- 相册详情头（对齐 v2.9 交互）：返回 + 册名/册内路径 + 幻灯片，滚动时始终可见 -->
        <header
          class="sticky top-0 z-10 flex shrink-0 items-center gap-3 border-b border-line bg-base/85 px-4 py-2 backdrop-blur md:px-6"
        >
          <button
            class="flex shrink-0 items-center gap-0.5 rounded-md px-2 py-1 text-xs text-muted transition-colors hover:bg-elevated hover:text-ink"
            data-testid="album-back"
            @click="closeAlbum"
          >
            <AppIcon name="chevronLeft" :size="13" />
            {{ t('gallery.photos.backToAlbums') }}
          </button>
          <div class="min-w-0 flex-1">
            <p class="truncate text-sm font-semibold text-ink">{{ albumLabel(activeAlbum) }}</p>
            <p class="truncate text-[12px] text-muted">{{ albumSubtitle(activeAlbum) }}</p>
          </div>
          <button
            class="shrink-0 rounded-md px-2 py-1 text-xs text-muted transition-colors hover:bg-elevated hover:text-ink disabled:cursor-not-allowed disabled:opacity-40"
            :disabled="albumLoading || albumItems.length === 0"
            data-testid="album-slideshow"
            @click="openViewer(albumItems, 0)"
          >
            {{ t('gallery.photos.slideshow') }}
          </button>
        </header>

        <div class="min-h-0 flex-1 overflow-y-auto p-2 md:p-3">
          <div
            v-if="albumLoading"
            class="grid grid-cols-[repeat(auto-fill,minmax(112px,1fr))] gap-1.5 md:grid-cols-[repeat(auto-fill,minmax(150px,1fr))] md:gap-2"
          >
            <div v-for="i in 12" :key="i" class="skeleton aspect-square rounded-lg" />
          </div>
          <div
            v-else-if="albumItems.length === 0"
            class="grid h-full place-items-center text-center"
          >
            <p class="text-sm text-muted">{{ t('gallery.photos.empty') }}</p>
          </div>
          <div
            v-else
            class="grid grid-cols-[repeat(auto-fill,minmax(112px,1fr))] gap-1.5 md:grid-cols-[repeat(auto-fill,minmax(150px,1fr))] md:gap-2"
            data-testid="album-photos"
          >
            <PhotoTile
              v-for="(item, index) in albumItems"
              :key="item.path"
              :item="item"
              @click="openViewer(albumItems, index)"
              @contextmenu.prevent="openMediaMenu($event, item, 'photos')"
            />
          </div>
          <!-- 触底续接（游标分页）：滚到这里自动取下一页 -->
          <div v-if="loadingMore" class="flex items-center justify-center gap-2 py-4 text-xs text-muted">
            <span class="size-3 animate-spin rounded-full border-2 border-line border-t-accent" />
            {{ t('gallery.toolbar.loadingMore') }}
          </div>
          <div v-if="albumHasMore" ref="sentinel" class="h-px w-full" aria-hidden="true" />
          <p v-else-if="albumItems.length > 0 && !loadingMore" class="py-4 text-center text-[11px] text-muted">
            {{ t('gallery.toolbar.allLoaded') }}
          </p>
        </div>
      </div>

      <!-- 相册卡片：封面取册内最新一张图，空相册后端已剔除 -->
      <div v-else-if="photoView === 'albums'" class="h-full overflow-y-auto p-2 md:p-3" data-testid="photo-albums">
        <div
          v-if="albumsLoading"
          class="grid grid-cols-[repeat(auto-fill,minmax(140px,1fr))] gap-3 md:grid-cols-[repeat(auto-fill,minmax(190px,1fr))]"
        >
          <div v-for="i in 8" :key="i" class="skeleton aspect-[4/3] rounded-xl" />
        </div>

        <div v-else-if="albumsError" class="grid h-full place-items-center text-center">
          <div>
            <span class="mb-3 inline-grid size-12 place-items-center rounded-full bg-elevated text-muted">
              <AppIcon name="alert" :size="22" />
            </span>
            <p class="text-sm text-ink">{{ t('gallery.toolbar.loadFailed') }}</p>
            <p class="mt-1 text-xs text-muted">{{ albumsError }}</p>
          </div>
        </div>

        <div v-else-if="albums.length === 0" class="grid h-full place-items-center text-center">
          <div>
            <span class="mb-3 inline-grid size-12 place-items-center rounded-full bg-elevated text-muted">
              <AppIcon name="gallery" :size="22" />
            </span>
            <p class="text-sm text-ink">{{ t('gallery.photos.albumEmpty') }}</p>
            <p class="mt-1 text-xs text-muted">{{ t('gallery.photos.albumEmptyHint') }}</p>
          </div>
        </div>

        <div
          v-else
          class="grid grid-cols-[repeat(auto-fill,minmax(140px,1fr))] gap-3 md:grid-cols-[repeat(auto-fill,minmax(190px,1fr))]"
        >
          <button
            v-for="album in albums"
            :key="album.path"
            type="button"
            class="group text-left"
            :data-album="album.name"
            :title="albumLabel(album)"
            @click="openAlbum(album)"
          >
            <div
              class="relative aspect-[4/3] overflow-hidden rounded-xl border border-line"
              :style="{ backgroundColor: albumColor(album) }"
            >
              <!-- ThumbHash 打底：封面还没加载完也不是一块死色 -->
              <img
                v-if="albumHash(album)"
                :src="albumHash(album)!"
                class="absolute inset-0 size-full scale-110 object-cover blur-md"
                aria-hidden="true"
                alt=""
              />
              <img
                v-if="album.cover && !albumCoverFailed.has(album.path)"
                :src="thumbUrl(album.cover, 480)"
                class="relative size-full object-cover transition-transform duration-300 group-hover:scale-[1.03]"
                loading="lazy"
                alt=""
                @error="markAlbumCoverFailed(album.path)"
              />
              <span v-else class="relative grid size-full place-items-center text-white/60">
                <AppIcon name="gallery" :size="26" />
              </span>
              <span
                class="absolute right-1.5 bottom-1.5 rounded bg-black/60 px-1.5 py-0.5 text-[11px] text-white backdrop-blur"
              >
                {{ t('gallery.photos.albumItems', { count: album.count }) }}
              </span>
            </div>
            <p class="mt-2 truncate text-sm font-medium text-ink">{{ albumLabel(album) }}</p>
            <!-- 副标题（对齐 v2.9）：数量 + 册内路径，卡片不只剩一个封面和名字 -->
            <p class="truncate text-[12px] text-muted">{{ albumSubtitle(album) }}</p>
          </button>
        </div>
          <!-- 触底续接（游标分页）：滚到这里自动取下一页 -->
          <div v-if="loadingMore" class="flex items-center justify-center gap-2 py-4 text-xs text-muted">
            <span class="size-3 animate-spin rounded-full border-2 border-line border-t-accent" />
            {{ t('gallery.toolbar.loadingMore') }}
          </div>
          <div v-if="albumsHasMore" ref="sentinel" class="h-px w-full" aria-hidden="true" />
          <p v-else-if="albums.length > 0 && !loadingMore" class="py-4 text-center text-[11px] text-muted">
            {{ t('gallery.toolbar.allLoaded') }}
          </p>
      </div>

      <!-- 时间线：忽略目录层级、按月吸顶（设计文档 3.4.3 视图二） -->
      <div v-else class="h-full overflow-y-auto" data-testid="photo-timeline">
        <div v-if="photoItems.length === 0" class="grid h-full place-items-center text-center">
          <div>
            <span class="mb-3 inline-grid size-12 place-items-center rounded-full bg-elevated text-muted">
              <AppIcon name="gallery" :size="22" />
            </span>
            <p class="text-sm text-ink">{{ t('gallery.photos.empty') }}</p>
            <p class="mt-1 text-xs text-muted">{{ t('gallery.photos.emptyHint') }}</p>
          </div>
        </div>

        <template v-else>
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
              <PhotoTile
                v-for="(item, index) in group.items"
                :key="item.path"
                :item="item"
                @click="openViewer(group.items, index)"
                @contextmenu.prevent="openMediaMenu($event, item, 'photos')"
              />
            </div>
          </div>
        </template>
          <!-- 触底续接（游标分页）：滚到这里自动取下一页 -->
          <div v-if="loadingMore" class="flex items-center justify-center gap-2 py-4 text-xs text-muted">
            <span class="size-3 animate-spin rounded-full border-2 border-line border-t-accent" />
            {{ t('gallery.toolbar.loadingMore') }}
          </div>
          <div v-if="hasMore" ref="sentinel" class="h-px w-full" aria-hidden="true" />
          <p v-else-if="photoItems.length > 0 && !loadingMore" class="py-4 text-center text-[11px] text-muted">
            {{ t('gallery.toolbar.allLoaded') }}
          </p>
      </div>
    </div>

    <!-- 影视库（v4.4.8：与照片库同构，先文件夹卡片、点进去看影片） -->
    <div v-else-if="tab === 'cinema'" class="flex min-h-0 min-w-0 flex-1 flex-col">
      <!-- 文件夹详情 -->
      <div v-if="activeCinemaAlbum" class="flex h-full flex-col" data-testid="cinema-album">
        <header
          class="sticky top-0 z-10 flex shrink-0 items-center gap-3 border-b border-line bg-base/85 px-4 py-2 backdrop-blur md:px-6"
        >
          <button
            class="flex shrink-0 items-center gap-0.5 rounded-md px-2 py-1 text-xs text-muted transition-colors hover:bg-elevated hover:text-ink"
            data-testid="cinema-album-back"
            @click="closeCinemaAlbum"
          >
            <AppIcon name="chevronLeft" :size="13" />
            {{ t('gallery.cinema.backToFolders') }}
          </button>
          <div class="min-w-0 flex-1">
            <p class="truncate text-sm font-semibold text-ink">{{ cinemaAlbumLabel(activeCinemaAlbum) }}</p>
            <p class="truncate text-[12px] text-muted">{{ cinemaAlbumSubtitle(activeCinemaAlbum) }}</p>
          </div>
        </header>

        <div class="min-h-0 flex-1 overflow-y-auto px-4 py-4 md:px-6 md:py-6">
          <div
            v-if="cinemaLoading"
            class="grid grid-cols-[repeat(auto-fill,minmax(200px,1fr))] gap-4 md:grid-cols-[repeat(auto-fill,minmax(260px,1fr))]"
          >
            <div v-for="i in 8" :key="i" class="skeleton aspect-video rounded-xl" />
          </div>
          <div v-else-if="cinemaItems.length === 0" class="grid h-full place-items-center text-center">
            <p class="text-sm text-muted">{{ t('gallery.cinema.empty') }}</p>
          </div>
          <div
            v-else
            class="grid grid-cols-[repeat(auto-fill,minmax(200px,1fr))] gap-4 md:grid-cols-[repeat(auto-fill,minmax(260px,1fr))]"
            data-testid="cinema-videos"
          >
            <VideoCard
              v-for="video in cinemaItems"
              :key="video.path"
              :video="video"
              :favorite="isFavorite(video.path)"
              :resume-at="resumeAt(video.path)"
              @play="playing = video"
              @menu="openMediaMenu($event, video, 'cinema')"
              @favorite="toggleFavorite(video)"
            />
          </div>
          <div v-if="loadingMore" class="flex items-center justify-center gap-2 py-4 text-xs text-muted">
            <span class="size-3 animate-spin rounded-full border-2 border-line border-t-accent" />
            {{ t('gallery.toolbar.loadingMore') }}
          </div>
          <div v-if="cinemaHasMore" ref="sentinel" class="h-px w-full" aria-hidden="true" />
          <p
            v-else-if="cinemaItems.length > 0 && !loadingMore"
            class="py-4 text-center text-[11px] text-muted"
          >
            {{ t('gallery.toolbar.allLoaded') }}
          </p>
        </div>
      </div>

      <!-- 文件夹卡片：封面取文件夹里最新的视频首帧，空文件夹后端已剔除 -->
      <div v-else class="h-full overflow-y-auto px-4 py-4 md:px-6 md:py-6" data-testid="cinema-albums">
        <div class="mb-4 flex items-center justify-between">
          <h1 class="text-xl font-bold text-ink md:text-2xl">{{ t('gallery.cinema.title') }}</h1>
          <span class="hidden text-sm text-muted md:block">
            {{ t('gallery.cinema.count', { count: cinemaAlbumsTotal || counts.video }) }}
          </span>
        </div>

        <div
          v-if="cinemaAlbumsLoading"
          class="grid grid-cols-[repeat(auto-fill,minmax(200px,1fr))] gap-4 md:grid-cols-[repeat(auto-fill,minmax(260px,1fr))]"
        >
          <div v-for="i in 8" :key="i" class="skeleton aspect-video rounded-xl" />
        </div>

        <div v-else-if="cinemaAlbumsError" class="grid h-full place-items-center text-center">
          <div>
            <span class="mb-3 inline-grid size-12 place-items-center rounded-full bg-elevated text-muted">
              <AppIcon name="alert" :size="22" />
            </span>
            <p class="text-sm text-ink">{{ t('gallery.toolbar.loadFailed') }}</p>
            <p class="mt-1 text-xs text-muted">{{ cinemaAlbumsError }}</p>
          </div>
        </div>

        <div v-else-if="cinemaAlbums.length === 0" class="grid h-full place-items-center text-center">
          <div>
            <span class="mb-3 inline-grid size-12 place-items-center rounded-full bg-elevated text-muted">
              <AppIcon name="video" :size="22" />
            </span>
            <p class="text-sm text-ink">{{ t('gallery.cinema.folderEmpty') }}</p>
            <p class="mt-1 text-xs text-muted">{{ t('gallery.cinema.folderEmptyHint') }}</p>
          </div>
        </div>

        <div
          v-else
          class="grid grid-cols-[repeat(auto-fill,minmax(200px,1fr))] gap-4 md:grid-cols-[repeat(auto-fill,minmax(260px,1fr))]"
        >
          <button
            v-for="album in cinemaAlbums"
            :key="album.path"
            type="button"
            class="group text-left"
            :data-cinema-album="album.name"
            :title="cinemaAlbumLabel(album)"
            @click="openCinemaAlbum(album)"
          >
            <div
              class="relative aspect-video overflow-hidden rounded-xl border border-line"
              :style="{ backgroundColor: cinemaAlbumColor(album) }"
            >
              <img
                v-if="cinemaAlbumHash(album)"
                :src="cinemaAlbumHash(album)!"
                class="absolute inset-0 size-full scale-110 object-cover blur-md"
                aria-hidden="true"
                alt=""
              />
              <img
                v-if="album.cover && !cinemaCoverFailed.has(album.path)"
                :src="thumbUrl(album.cover, 480)"
                class="relative size-full object-cover transition-transform duration-300 group-hover:scale-[1.03]"
                loading="lazy"
                alt=""
                @error="markCinemaCoverFailed(album.path)"
              />
              <span v-else class="relative grid size-full place-items-center text-white/60">
                <AppIcon name="video" :size="26" />
              </span>
              <span class="absolute inset-0 grid place-items-center">
                <span
                  class="grid size-9 place-items-center rounded-full bg-black/50 text-white backdrop-blur transition-transform group-hover:scale-110"
                >
                  <AppIcon name="play" :size="15" />
                </span>
              </span>
              <span
                class="absolute right-1.5 bottom-1.5 rounded bg-black/60 px-1.5 py-0.5 text-[11px] text-white backdrop-blur"
              >
                {{ t('gallery.cinema.albumItems', { count: album.count }) }}
              </span>
            </div>
            <p class="mt-2 truncate text-sm font-medium text-ink">{{ cinemaAlbumLabel(album) }}</p>
          </button>
        </div>

        <div v-if="loadingMore" class="flex items-center justify-center gap-2 py-4 text-xs text-muted">
          <span class="size-3 animate-spin rounded-full border-2 border-line border-t-accent" />
          {{ t('gallery.toolbar.loadingMore') }}
        </div>
        <div v-if="cinemaAlbumsHasMore" ref="sentinel" class="h-px w-full" aria-hidden="true" />
        <p
          v-else-if="cinemaAlbums.length > 0 && !loadingMore"
          class="py-4 text-center text-[11px] text-muted"
        >
          {{ t('gallery.toolbar.allLoaded') }}
        </p>
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
              {{ t('gallery.music.count', { count: counts.audio }) }}
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
            <template v-for="group in audioGroups" :key="group.key">
              <!-- 分组标题：多个文件夹时按文件夹分区（v3.5.5） -->
              <tr v-if="audioGroups.length > 1">
                <td colspan="4" class="border-b border-line/60 px-2 pb-1.5 pt-4 md:px-3">
                  <span class="text-sm font-semibold text-ink">{{ group.label }}</span>
                  <span class="ml-2 text-xs text-muted">{{ group.items.length }}</span>
                </td>
              </tr>
              <tr
                v-for="track in group.items"
                :key="track.path"
                class="group cursor-pointer transition-colors hover:bg-elevated/50"
                :data-track="track.path"
                @click="player.play(audios, audioRowIndex.get(track.path) ?? 0)"
                @contextmenu.prevent="openMediaMenu($event, track, 'music')"
              >
              <td class="px-2 py-3 md:px-3">
                <span class="group-hover:hidden" :class="player.isCurrent(track.path) ? 'text-accent' : ''">
                  {{ (audioRowIndex.get(track.path) ?? 0) + 1 }}
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
                    <span v-if="trackSubtitle(track)" class="block truncate text-xs text-muted">
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
            </template>
          </tbody>
        </table>
      </div>
      <!-- 触底续接（游标分页）：滚到这里自动取下一页 -->
      <div v-if="loadingMore" class="flex items-center justify-center gap-2 py-4 text-xs text-muted">
        <span class="size-3 animate-spin rounded-full border-2 border-line border-t-accent" />
        {{ t('gallery.toolbar.loadingMore') }}
      </div>
      <div v-if="hasMore" ref="sentinel" class="h-px w-full" aria-hidden="true" />
      <p v-else-if="audios.length > 0 && !loadingMore" class="py-4 text-center text-[11px] text-muted">
        {{ t('gallery.toolbar.allLoaded') }}
      </p>
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

    <!-- 全局右键菜单（设计文档 8.4.13：媒体库与文件管理共用同一组件） -->
    <ContextMenu
      :open="!!mediaMenu"
      :x="mediaMenu?.x ?? 0"
      :y="mediaMenu?.y ?? 0"
      :items="mediaMenuItems"
      @select="onMediaMenuSelect"
      @close="mediaMenu = null"
    />

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
