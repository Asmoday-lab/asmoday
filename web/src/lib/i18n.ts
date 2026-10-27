import { createI18n } from 'vue-i18n'
import messages, { FALLBACK_LOCALE, SUPPORTED_LOCALES, type Locale } from '../locales'

export { SUPPORTED_LOCALES, FALLBACK_LOCALE }
export type { Locale }

/** 语言选择的持久化键（设计文档 3.8 第 3 条） */
const LOCALE_KEY = 'asmoday.locale'

export function isLocale(value: unknown): value is Locale {
  return typeof value === 'string' && (SUPPORTED_LOCALES as readonly string[]).includes(value)
}

/**
 * 语言检测顺序：本地存储 → 浏览器语言 → 英文兜底。
 * 浏览器语言逐个匹配（navigator.languages 可能有多个），只认支持的前缀。
 */
export function detectLocale(): Locale {
  try {
    const saved = localStorage.getItem(LOCALE_KEY)
    if (isLocale(saved)) return saved
  } catch {
    /* 隐私模式下 localStorage 可能不可用 */
  }

  const candidates = navigator.languages?.length ? navigator.languages : [navigator.language]
  for (const raw of candidates) {
    const language = (raw ?? '').toLowerCase()
    if (language.startsWith('zh')) return 'zh-CN'
    if (language.startsWith('en')) return 'en-US'
  }
  return FALLBACK_LOCALE
}

export const i18n = createI18n({
  legacy: false,
  globalInjection: true,
  locale: detectLocale(),
  fallbackLocale: FALLBACK_LOCALE,
  messages,
})

export function currentLocale(): Locale {
  const value = i18n.global.locale.value
  return isLocale(value) ? value : FALLBACK_LOCALE
}

/** 切换语言：即时生效（无刷新）+ 持久化 + 同步 <html lang> */
export function applyLocale(locale: Locale): void {
  i18n.global.locale.value = locale
  try {
    localStorage.setItem(LOCALE_KEY, locale)
  } catch {
    /* 忽略写入失败 */
  }
  if (typeof document !== 'undefined') document.documentElement.lang = locale
}

// 首次加载就把 <html lang> 对齐，避免读屏器与浏览器翻译提示用语种不符
if (typeof document !== 'undefined') document.documentElement.lang = currentLocale()

export default i18n
