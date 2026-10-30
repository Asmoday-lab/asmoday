/**
 * 全局灯箱状态（设计文档 3.2 / 8.4.9）。
 *
 * 单例原则：整个应用只有 App.vue 里的一个 `MediaLightbox` 实例（Teleport 到 body），
 * 文件管理、相册、影视库共用它，严禁在列表循环里各写一套 `<img>` / `<video>` 弹窗。
 *
 * 状态本体在 Pinia store 里（`useLightboxStore`），文件末尾再导出几个具名符号，
 * 让组件层保持「`lightboxState.open` / `openLightbox(...)`」这种简洁写法。
 */
import { defineStore } from 'pinia'
import { computed, ref } from 'vue'

import { libraryApi } from '../api/media'
import { pinia } from './pinia'

export interface LightboxItem {
  path: string
  name: string
  kind: 'image' | 'video' | 'audio'
  /** 视频外挂字幕（同目录同名 .srt/.vtt） */
  subtitle?: string | null
  /** 字节数（信息层展示；调用方有就带上，缺省则整行隐藏） */
  size?: number
  /** 修改时间（unix 秒），同上 */
  modified?: number | null
}

export interface OpenOptions {
  /** 需要同步本地播放进度时传（画廊的续播表） */
  onProgress?: (path: string, seconds: number) => void
}

/** 进度上报节流：>5 秒才有续播价值，5 秒最多写一次 */
const REPORT_INTERVAL = 5000

export const useLightboxStore = defineStore('lightbox', () => {
  const open = ref(false)
  const items = ref<LightboxItem[]>([])
  const index = ref(0)

  /** 进度写回回调：画廊传自己的 savePosition 以同步本地续播表，其余场景直接落库 */
  let progressSink: ((path: string, seconds: number) => void) | null = null
  let lastReport = 0

  const current = computed<LightboxItem | null>(() => items.value[index.value] ?? null)
  const total = computed(() => items.value.length)

  function show(list: LightboxItem[], start = 0, options: OpenOptions = {}) {
    if (list.length === 0) return
    progressSink = options.onProgress ?? null
    lastReport = 0
    items.value = list.slice()
    index.value = Math.min(Math.max(0, start), list.length - 1)
    open.value = true
  }

  function close() {
    open.value = false
    // 清空引用，避免整份媒体列表被灯箱长期持有
    items.value = []
    index.value = 0
    progressSink = null
  }

  function step(delta: number) {
    if (items.value.length === 0) return
    const length = items.value.length
    index.value = (index.value + delta + length) % length
  }

  function jump(target: number) {
    if (target < 0 || target >= items.value.length) return
    index.value = target
  }

  /** 由灯箱内视频的 `progress` 事件调用 */
  function reportProgress(path: string, seconds: number) {
    if (!Number.isFinite(seconds) || seconds < 5) return
    const now = Date.now()
    if (now - lastReport < REPORT_INTERVAL) return
    lastReport = now
    if (progressSink) progressSink(path, seconds)
    else void libraryApi.saveProgress(path, seconds).catch(() => {})
  }

  /** 播完清零，下次从头看（避免「续播」直接跳到片尾） */
  function finishPlayback(path: string) {
    lastReport = Date.now()
    if (progressSink) progressSink(path, 0)
    else void libraryApi.saveProgress(path, 0).catch(() => {})
  }

  return {
    open,
    items,
    index,
    current,
    total,
    show,
    close,
    step,
    jump,
    reportProgress,
    finishPlayback,
  }
})

// 组件外直接可用的具名导出（显式传实例，不依赖 activePinia）。
// `lightboxState` 就是 store 本身：Pinia store 是 reactive 代理，
// `lightboxState.open` / `.items` / `.index` 的读写语义与原来的 reactive 单例一致。
const lightbox = useLightboxStore(pinia)

export const lightboxState = lightbox
export const currentLightboxItem = computed<LightboxItem | null>(() => lightbox.current)
export const lightboxTotal = computed(() => lightbox.total)

export const openLightbox = (
  items: LightboxItem[],
  index = 0,
  options: OpenOptions = {},
): void => lightbox.show(items, index, options)
export const closeLightbox = (): void => lightbox.close()
export const stepLightbox = (delta: number): void => lightbox.step(delta)
export const jumpLightbox = (index: number): void => lightbox.jump(index)
export const reportLightboxProgress = (path: string, seconds: number): void =>
  lightbox.reportProgress(path, seconds)
export const finishLightboxPlayback = (path: string): void => lightbox.finishPlayback(path)
