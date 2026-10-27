/**
 * 全局灯箱状态（设计文档 3.2 / 8.4.9）。
 *
 * 单例原则：整个应用只有 App.vue 里的一个 `MediaLightbox` 实例（Teleport 到 body），
 * 文件管理、相册、影视库共用它，严禁在列表循环里各写一套 `<img>` / `<video>` 弹窗。
 *
 * 与项目其余 store 一致，用 `reactive` 的模块级单例（不额外引入 Pinia 依赖），
 * 语义就是「全局媒体播放状态」。
 */
import { computed, reactive } from 'vue'
import { libraryApi } from '../api/media'

export interface LightboxItem {
  path: string
  name: string
  kind: 'image' | 'video' | 'audio'
  /** 视频外挂字幕（同目录同名 .srt/.vtt） */
  subtitle?: string | null
}

interface LightboxState {
  open: boolean
  items: LightboxItem[]
  index: number
}

const state = reactive<LightboxState>({
  open: false,
  items: [],
  index: 0,
})

/** 进度写回回调：画廊传自己的 savePosition 以同步本地续播表，其余场景直接落库 */
let progressSink: ((path: string, seconds: number) => void) | null = null
let lastReport = 0

export const lightboxState = state

export const currentLightboxItem = computed<LightboxItem | null>(
  () => state.items[state.index] ?? null,
)

export const lightboxTotal = computed(() => state.items.length)

export interface OpenOptions {
  /** 需要同步本地播放进度时传（画廊的续播表） */
  onProgress?: (path: string, seconds: number) => void
}

export function openLightbox(items: LightboxItem[], index = 0, options: OpenOptions = {}): void {
  if (items.length === 0) return
  progressSink = options.onProgress ?? null
  lastReport = 0
  state.items = items.slice()
  state.index = Math.min(Math.max(0, index), items.length - 1)
  state.open = true
}

export function closeLightbox(): void {
  state.open = false
  // 清空引用，避免整份媒体列表被灯箱长期持有
  state.items = []
  state.index = 0
  progressSink = null
}

export function stepLightbox(delta: number): void {
  if (state.items.length === 0) return
  const length = state.items.length
  state.index = (state.index + delta + length) % length
}

export function jumpLightbox(index: number): void {
  if (index < 0 || index >= state.items.length) return
  state.index = index
}

/**
 * 上报播放进度：>5 秒才有续播价值，5 秒节流一次（与画廊原逻辑等价）。
 * 由灯箱内视频的 `progress` 事件调用。
 */
export function reportLightboxProgress(path: string, seconds: number): void {
  if (!Number.isFinite(seconds) || seconds < 5) return
  const now = Date.now()
  if (now - lastReport < 5000) return
  lastReport = now
  if (progressSink) progressSink(path, seconds)
  else void libraryApi.saveProgress(path, seconds).catch(() => {})
}

/** 播完清零，下次从头看（避免「续播」直接跳到片尾） */
export function finishLightboxPlayback(path: string): void {
  lastReport = Date.now()
  if (progressSink) progressSink(path, 0)
  else void libraryApi.saveProgress(path, 0).catch(() => {})
}
