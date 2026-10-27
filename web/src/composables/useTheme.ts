/**
 * 主题切换。
 * 直接操作原生 <html data-theme> 属性 + color-scheme，绕开框架渲染延迟；
 * 状态在模块级共享，任意组件调用 useTheme() 都拿到同一份。
 */

import { computed, ref } from 'vue'

export type ThemeMode = 'dark' | 'light'

const STORAGE_KEY = 'asmoday.theme'

function readInitial(): ThemeMode {
  if (typeof document === 'undefined') return 'dark'
  if (document.documentElement.getAttribute('data-theme') === 'light') return 'light'
  try {
    return localStorage.getItem(STORAGE_KEY) === 'light' ? 'light' : 'dark'
  } catch {
    return 'dark'
  }
}

const mode = ref<ThemeMode>(readInitial())

function apply(next: ThemeMode) {
  const root = document.documentElement
  if (next === 'light') {
    root.setAttribute('data-theme', 'light')
  } else {
    root.removeAttribute('data-theme')
  }
  root.style.colorScheme = next
  try {
    localStorage.setItem(STORAGE_KEY, next)
  } catch {
    /* 隐私模式下忽略 */
  }
}

export function useTheme() {
  const toggle = () => {
    mode.value = mode.value === 'dark' ? 'light' : 'dark'
    apply(mode.value)
  }

  const setTheme = (next: ThemeMode) => {
    mode.value = next
    apply(next)
  }

  return {
    mode,
    isDark: computed(() => mode.value === 'dark'),
    toggle,
    setTheme,
  }
}
