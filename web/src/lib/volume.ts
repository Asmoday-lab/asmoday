/**
 * 全局音量记忆（设计文档 3.4「全局音量记忆机制（防爆音）」）。
 *
 * - 初始音量强制 35%，绝不用浏览器默认的 100% 导致深夜爆音；
 * - 用户每次调整都写进 localStorage（`asm_volume`），之后播放任何媒体都先读它；
 * - 静音只记标记，实际音量不动；取消静音恢复「静音前」的数值；
 * - 滑块拖到 0 视作静音（图标自动切换），再点静音键恢复最近一次非 0 音量。
 *
 * 音乐条、视频播放器、灯箱里的原生 `<audio>` 共用这一份状态。
 */
import { reactive } from 'vue'

/** 未经用户调整时的安全默认值（35%） */
export const DEFAULT_VOLUME = 0.35
/** 设计文档指定的持久化键 */
export const VOLUME_KEY = 'asm_volume'
const MUTED_KEY = 'asm_muted'
const LAST_AUDIBLE_KEY = 'asm_volume_last'

function clamp(value: number): number {
  if (!Number.isFinite(value)) return DEFAULT_VOLUME
  return Math.min(1, Math.max(0, value))
}

function readNumber(key: string, fallback: number): number {
  try {
    const raw = localStorage.getItem(key)
    if (raw === null) return fallback
    const parsed = Number(raw)
    return Number.isFinite(parsed) ? clamp(parsed) : fallback
  } catch {
    // 隐私模式下 localStorage 可能直接抛异常
    return fallback
  }
}

function readFlag(key: string): boolean {
  try {
    return localStorage.getItem(key) === '1'
  } catch {
    return false
  }
}

function write(key: string, value: string) {
  try {
    localStorage.setItem(key, value)
  } catch {
    /* 写不进去就只在本会话生效，不影响播放 */
  }
}

const state = reactive({
  volume: readNumber(VOLUME_KEY, DEFAULT_VOLUME),
  muted: readFlag(MUTED_KEY),
  /** 最近一次非 0 音量，取消静音时恢复它 */
  lastAudible: readNumber(LAST_AUDIBLE_KEY, DEFAULT_VOLUME),
})

// 上次退出时若正好拖到 0，`lastAudible` 可能也是 0，恢复时要有个兜底
if (state.lastAudible <= 0) state.lastAudible = DEFAULT_VOLUME

function persist() {
  write(VOLUME_KEY, String(state.volume))
  write(MUTED_KEY, state.muted ? '1' : '0')
  write(LAST_AUDIBLE_KEY, String(state.lastAudible))
}

export const volume = {
  state,

  /** 界面是否表现为静音：点了静音键，或滑块被拖到 0 */
  get muted(): boolean {
    return state.muted || state.volume === 0
  },

  /** 滑块位置：静音时仍显示静音前的刻度，方便一眼看出恢复后会多大声 */
  get sliderValue(): number {
    return state.volume
  },

  /** 滑块调整（0..1） */
  set(value: number) {
    const next = clamp(value)
    state.volume = next
    if (next > 0) {
      state.lastAudible = next
      state.muted = false
    } else {
      state.muted = true
    }
    persist()
  },

  /** 静音 / 取消静音 */
  toggle() {
    if (state.muted || state.volume === 0) {
      state.volume = state.lastAudible > 0 ? state.lastAudible : DEFAULT_VOLUME
      state.muted = false
    } else {
      state.lastAudible = state.volume
      state.muted = true
    }
    persist()
  },
}

/** 把当前音量写进某个 `<audio>` / `<video>` */
export function applyVolume(element: HTMLMediaElement | null | undefined) {
  if (!element) return
  const shouldMute = state.muted || state.volume === 0
  if (Math.abs(element.volume - state.volume) > 0.001) element.volume = state.volume
  if (element.muted !== shouldMute) element.muted = shouldMute
}

/** 已经绑定过的元素，避免重复挂监听（多个地方都会调 bindVolume） */
const bound = new WeakSet<HTMLMediaElement>()

/**
 * 绑定媒体元素：先写入当前音量，再把元素自身的 `volumechange`
 * （原生控件 / Video.js 皮肤 / 手机物理键）反向同步回全局，
 * 保证「从哪儿调都会被记住」。返回解绑函数；重复调用是幂等的。
 */
export function bindVolume(element: HTMLMediaElement): () => void {
  applyVolume(element)
  if (bound.has(element)) return () => {}

  const onChange = () => {
    if (element.muted) {
      if (!state.muted) {
        if (element.volume > 0) state.lastAudible = element.volume
        state.muted = true
        persist()
      }
      return
    }

    const next = clamp(element.volume)
    if (next === 0) {
      if (!state.muted || state.volume !== 0) {
        state.volume = 0
        state.muted = true
        persist()
      }
      return
    }

    if (state.muted || Math.abs(next - state.volume) > 0.005) {
      state.volume = next
      state.lastAudible = next
      state.muted = false
      persist()
    }
  }

  bound.add(element)
  element.addEventListener('volumechange', onChange)
  return () => {
    element.removeEventListener('volumechange', onChange)
    bound.delete(element)
  }
}
