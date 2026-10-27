/**
 * 全局音乐播放器状态：底部悬浮播放条跨页面常驻（设计文档 3.4 音乐库）。
 * 播放模式：列表循环 / 单曲循环 / 随机。
 */
import { computed, reactive } from 'vue'
import type { MediaItem } from '../api/media'
import { lrcUrl } from '../api/media'
import { i18n } from '../lib/i18n'

export type PlayMode = 'loop' | 'one' | 'shuffle'

export interface LyricLine {
  time: number
  text: string
}

interface PlayerState {
  queue: MediaItem[]
  index: number
  playing: boolean
  mode: PlayMode
  currentTime: number
  duration: number
  /** 展开歌词面板 */
  expanded: boolean
  lyrics: LyricLine[] | null
  lyricError: string
}

const state = reactive<PlayerState>({
  queue: [],
  index: -1,
  playing: false,
  mode: (localStorage.getItem('asmoday.player.mode') as PlayMode | null) ?? 'loop',
  currentTime: 0,
  duration: 0,
  expanded: false,
  lyrics: null,
  lyricError: '',
})

/** 面板展示用：当前曲目（优先 ID3 标题） */
export const currentTrack = computed(() => state.queue[state.index] ?? null)

export const player = {
  state,
  currentTrack,

  /** 播放 tracks 中的第 index 首 */
  play(tracks: MediaItem[], index: number) {
    state.queue = [...tracks]
    state.index = index
    state.playing = true
    void loadLyrics()
  },

  toggle() {
    if (state.index < 0 && state.queue.length > 0) {
      state.index = 0
      state.playing = true
      void loadLyrics()
      return
    }
    state.playing = !state.playing
  },

  next(manual = true) {
    if (state.queue.length === 0) return
    if (state.mode === 'one' && !manual) return // 单曲循环：换源交给 <audio loop>
    if (state.mode === 'shuffle' && state.queue.length > 1) {
      let next = state.index
      while (next === state.index) {
        next = Math.floor(Math.random() * state.queue.length)
      }
      state.index = next
    } else {
      state.index = (state.index + 1) % state.queue.length
    }
    state.playing = true
    void loadLyrics()
  },

  prev() {
    if (state.queue.length === 0) return
    state.index = (state.index - 1 + state.queue.length) % state.queue.length
    state.playing = true
    void loadLyrics()
  },

  /** 播放列表抽屉里点某一首：直接跳到它，不改动队列 */
  jump(index: number) {
    if (index < 0 || index >= state.queue.length) return
    if (index === state.index) {
      state.playing = !state.playing
      return
    }
    state.index = index
    state.currentTime = 0
    state.playing = true
    void loadLyrics()
  },

  /** 清空播放队列（播放条随之收起） */
  clearQueue() {
    state.queue = []
    state.index = -1
    state.playing = false
    state.currentTime = 0
    state.duration = 0
    state.lyrics = null
    state.lyricError = ''
    state.expanded = false
  },

  setMode(mode: PlayMode) {
    state.mode = mode
    localStorage.setItem('asmoday.player.mode', mode)
  },

  seek(time: number) {
    state.currentTime = time
    return time
  },

  stop() {
    state.playing = false
    state.expanded = false
  },

  /** 是否正在播放某个路径（收藏高亮等场景不用，播放列表高亮用） */
  isCurrent(path: string) {
    return state.queue[state.index]?.path === path
  },
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

async function loadLyrics() {
  state.lyrics = null
  state.lyricError = ''
  const track = currentTrack.value
  if (!track) return
  try {
    const res = await fetch(lrcUrl(track.path))
    if (!res.ok) {
      state.lyricError = i18n.global.t('gallery.player.noLyrics')
      return
    }
    const text = await res.text()
    const lines = parseLrc(text)
    if (lines.length === 0) {
      state.lyricError = i18n.global.t('gallery.player.noTimeline')
      return
    }
    state.lyrics = lines
  } catch {
    state.lyricError = i18n.global.t('gallery.player.lyricsFailed')
  }
}
