/**
 * 全局音乐播放器状态：底部悬浮播放条跨页面常驻（设计文档 3.4 音乐库）。
 * 播放模式：列表循环 / 单曲循环 / 随机。
 *
 * 状态放在 Pinia store 里；播放模式用 `useStorage` 直接与 localStorage 双向绑定，
 * 不再手写「读一次 + 每次 setMode 手动写回」。
 */
import { useStorage } from '@vueuse/core'
import { defineStore } from 'pinia'
import { computed, ref } from 'vue'

import type { MediaItem } from '../api/media'
import { lrcUrl } from '../api/media'
import { i18n } from '../lib/i18n'
import { pinia } from './pinia'

export type PlayMode = 'loop' | 'one' | 'shuffle'

export interface LyricLine {
  time: number
  text: string
}

export const usePlayerStore = defineStore('player', () => {
  const queue = ref<MediaItem[]>([])
  const index = ref(-1)
  const playing = ref(false)
  /** 播放模式跨会话保留（旧值就是这三个字符串，迁移无缝） */
  const mode = useStorage<PlayMode>('asmoday.player.mode', 'loop')
  const currentTime = ref(0)
  const duration = ref(0)
  /** 展开歌词面板 */
  const expanded = ref(false)
  const lyrics = ref<LyricLine[] | null>(null)
  const lyricError = ref('')

  /** 面板展示用：当前曲目（优先 ID3 标题） */
  const currentTrack = computed(() => queue.value[index.value] ?? null)

  async function loadLyrics() {
    lyrics.value = null
    lyricError.value = ''
    const track = currentTrack.value
    if (!track) return
    try {
      const res = await fetch(lrcUrl(track.path))
      if (!res.ok) {
        lyricError.value = i18n.global.t('gallery.player.noLyrics')
        return
      }
      const text = await res.text()
      const lines = parseLrc(text)
      if (lines.length === 0) {
        lyricError.value = i18n.global.t('gallery.player.noTimeline')
        return
      }
      lyrics.value = lines
    } catch {
      lyricError.value = i18n.global.t('gallery.player.lyricsFailed')
    }
  }

  /** 播放 tracks 中的第 index 首 */
  function play(tracks: MediaItem[], start: number) {
    queue.value = [...tracks]
    index.value = start
    playing.value = true
    void loadLyrics()
  }

  function toggle() {
    if (index.value < 0 && queue.value.length > 0) {
      index.value = 0
      playing.value = true
      void loadLyrics()
      return
    }
    playing.value = !playing.value
  }

  function next(manual = true) {
    if (queue.value.length === 0) return
    if (mode.value === 'one' && !manual) return // 单曲循环：换源交给 <audio loop>
    if (mode.value === 'shuffle' && queue.value.length > 1) {
      let pick = index.value
      while (pick === index.value) {
        pick = Math.floor(Math.random() * queue.value.length)
      }
      index.value = pick
    } else {
      index.value = (index.value + 1) % queue.value.length
    }
    playing.value = true
    void loadLyrics()
  }

  function prev() {
    if (queue.value.length === 0) return
    index.value = (index.value - 1 + queue.value.length) % queue.value.length
    playing.value = true
    void loadLyrics()
  }

  /** 播放列表抽屉里点某一首：直接跳到它，不改动队列 */
  function jump(target: number) {
    if (target < 0 || target >= queue.value.length) return
    if (target === index.value) {
      playing.value = !playing.value
      return
    }
    index.value = target
    currentTime.value = 0
    playing.value = true
    void loadLyrics()
  }

  /** 清空播放队列（播放条随之收起） */
  function clearQueue() {
    queue.value = []
    index.value = -1
    playing.value = false
    currentTime.value = 0
    duration.value = 0
    lyrics.value = null
    lyricError.value = ''
    expanded.value = false
  }

  function setMode(value: PlayMode) {
    // useStorage 会自动写回 localStorage，不再手动 setItem
    mode.value = value
  }

  function seek(time: number) {
    currentTime.value = time
    return time
  }

  function stop() {
    playing.value = false
    expanded.value = false
  }

  /** 是否正在播放某个路径（播放列表高亮用） */
  function isCurrent(path: string) {
    return queue.value[index.value]?.path === path
  }

  return {
    queue,
    index,
    playing,
    mode,
    currentTime,
    duration,
    expanded,
    lyrics,
    lyricError,
    currentTrack,
    play,
    toggle,
    next,
    prev,
    jump,
    clearQueue,
    setMode,
    seek,
    stop,
    isCurrent,
  }
})

/**
 * 组件外使用的门面。
 *
 * `state` 指向 store 本身——Pinia store 是 reactive 代理，所以老的
 * `player.state.playing` 写法语义不变；其余方法逐一转发。
 */
const store = usePlayerStore(pinia)

export const currentTrack = computed(() => store.currentTrack)

export const player = {
  state: store,
  currentTrack,
  play: store.play,
  toggle: store.toggle,
  next: store.next,
  prev: store.prev,
  jump: store.jump,
  clearQueue: store.clearQueue,
  setMode: store.setMode,
  seek: store.seek,
  stop: store.stop,
  isCurrent: store.isCurrent,
}

/** [mm:ss.xx] 多时间戳 LRC 解析 */
export function parseLrc(text: string): LyricLine[] {
  const lines: LyricLine[] = []
  for (const raw of text.split(/\r?\n/)) {
    const stamps = [...raw.matchAll(/\[(\d{1,3}):(\d{1,2})(?:[.:](\d{1,3}))?\]/g)]
    if (stamps.length === 0) continue
    const content = raw.replace(/\[[^\]]*\]/g, '').trim()
    for (const stamp of stamps) {
      const minutes = Number(stamp[1])
      const seconds = Number(stamp[2])
      const fraction = stamp[3] ? Number(stamp[3].padEnd(3, '0').slice(0, 3)) / 1000 : 0
      lines.push({ time: minutes * 60 + seconds + fraction, text: content })
    }
  }
  return lines.sort((a, b) => a.time - b.time)
}
