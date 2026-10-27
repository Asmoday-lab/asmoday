/**
 * 语言包汇总（设计文档 3.8）。
 *
 * 按功能模块分文件存放：locales/<locale>/<module>.json。
 * 新增模块只要在这里加一行 import + 一个 key，中英两份都要补齐。
 */
import commonZh from './zh-CN/common.json'
import authZh from './zh-CN/auth.json'
import portalZh from './zh-CN/portal.json'
import filesZh from './zh-CN/files.json'
import galleryZh from './zh-CN/gallery.json'
import terminalZh from './zh-CN/terminal.json'
import sharesZh from './zh-CN/shares.json'
import settingsZh from './zh-CN/settings.json'
import ecosystemZh from './zh-CN/ecosystem.json'
import errorsZh from './zh-CN/errors.json'

import commonEn from './en-US/common.json'
import authEn from './en-US/auth.json'
import portalEn from './en-US/portal.json'
import filesEn from './en-US/files.json'
import galleryEn from './en-US/gallery.json'
import terminalEn from './en-US/terminal.json'
import sharesEn from './en-US/shares.json'
import settingsEn from './en-US/settings.json'
import ecosystemEn from './en-US/ecosystem.json'
import errorsEn from './en-US/errors.json'

export const SUPPORTED_LOCALES = ['zh-CN', 'en-US'] as const
export type Locale = (typeof SUPPORTED_LOCALES)[number]

/** 浏览器语言不在列表时按设计文档 3.8 回退到英文 */
export const FALLBACK_LOCALE: Locale = 'en-US'

export const messages = {
  'zh-CN': {
    common: commonZh,
    auth: authZh,
    portal: portalZh,
    files: filesZh,
    gallery: galleryZh,
    terminal: terminalZh,
    shares: sharesZh,
    settings: settingsZh,
    ecosystem: ecosystemZh,
    errors: errorsZh,
  },
  'en-US': {
    common: commonEn,
    auth: authEn,
    portal: portalEn,
    files: filesEn,
    gallery: galleryEn,
    terminal: terminalEn,
    shares: sharesEn,
    settings: settingsEn,
    ecosystem: ecosystemEn,
    errors: errorsEn,
  },
} as const

export default messages
