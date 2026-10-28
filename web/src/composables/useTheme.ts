/**
 * 主题切换（设计文档 8.2）。
 *
 * 三态：`light` / `dark` / `system`（默认 `system`）。直接操作原生
 * `<html data-theme>` + `color-scheme`，绕开框架渲染延迟；状态在模块级共享，
 * 任意组件调用 useTheme() 都拿到同一份。
 *
 * - 偏好存 localStorage 的 `asmoday.theme`（存三态原值，不存解析后的色）；
 * - `system` 时用 `matchMedia('(prefers-color-scheme: dark)')` 的 `change` 事件
 *   实时跟随，无需刷新（8.2「监听系统变化」）；
 * - 首屏防白屏由 index.html 里的内联脚本负责，这里只处理运行期。
 */

import { computed, ref } from 'vue'

export type ThemeMode = 'light' | 'dark' | 'system'
/** 实际生效的两态 */
export type ResolvedTheme = 'light' | 'dark'

const STORAGE_KEY = 'asmoday.theme'

const mediaQuery =
  typeof window !== 'undefined' && typeof window.matchMedia === 'function'
    ? window.matchMedia('(prefers-color-scheme: dark)')
    : null

function isMode(value: unknown): value is ThemeMode {
  return value === 'light' || value === 'dark' || value === 'system'
}

function readMode(): ThemeMode {
  try {
    const stored = localStorage.getItem(STORAGE_KEY)
    if (isMode(stored)) return stored
  } catch {
    /* 隐私模式：读不到就按 system */
  }
  // 没有偏好时看首屏脚本已经写上的 data-theme，避免状态与画面打架
  if (typeof document !== 'undefined') {
    const attr = document.documentElement.getAttribute('data-theme')
    if (attr === 'light' || attr === 'dark') return attr
  }
  return 'system'
}

const mode = ref<ThemeMode>(readMode())
const systemDark = ref<boolean>(mediaQuery?.matches ?? true)

function resolve(target: ThemeMode): ResolvedTheme {
  if (target === 'system') return systemDark.value ? 'dark' : 'light'
  return target
}

function paint(next: ResolvedTheme) {
  const root = document.documentElement
  if (next === 'light') root.setAttribute('data-theme', 'light')
  else root.removeAttribute('data-theme')
  root.style.colorScheme = next
}

const resolved = ref<ResolvedTheme>(resolve(mode.value))

function apply(next: ThemeMode) {
  mode.value = next
  resolved.value = resolve(next)
  paint(resolved.value)
  try {
    localStorage.setItem(STORAGE_KEY, next)
  } catch {
    /* 隐私模式下忽略 */
  }
}

// 系统深浅色变化：只有偏好是 system 时才跟着变（8.2）
mediaQuery?.addEventListener('change', (event) => {
  systemDark.value = event.matches
  if (mode.value === 'system') {
    resolved.value = resolve('system')
    paint(resolved.value)
  }
})

/** 与首屏脚本对齐一次（幂等） */
if (typeof document !== 'undefined') paint(resolved.value)

/** 循环顺序：light → dark → system → light（8.2 侧边栏按钮） */
const CYCLE: ThemeMode[] = ['light', 'dark', 'system']

export function useTheme() {
  const toggle = () => {
    apply(resolved.value === 'dark' ? 'light' : 'dark')
  }

  const cycle = () => {
    const index = CYCLE.indexOf(mode.value)
    apply(CYCLE[(index + 1) % CYCLE.length])
  }

  const setTheme = (next: ThemeMode) => apply(next)

  return {
    mode,
    resolved,
    isDark: computed(() => resolved.value === 'dark'),
    toggle,
    cycle,
    setTheme,
  }
}
