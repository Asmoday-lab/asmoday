/**
 * 子路径部署（`[server.proxy].base_url`）支持。
 *
 * 后端返回 index.html 时会注入 `window.__ASMODAY_BASE__`（如 `/asmoday`）。
 * 前端所有「绝对路径」都要过一遍 `withBase()`；`fetch` 由 main.ts 统一打补丁，
 * 业务代码里继续写 `/api/...` 即可。
 */
const injected = (window as unknown as { __ASMODAY_BASE__?: string }).__ASMODAY_BASE__

/** 规范化后的前缀，根路径部署时为空串 */
export const APP_BASE: string = (() => {
  const raw = typeof injected === 'string' ? injected : ''
  const trimmed = raw.trim().replace(/\/+$/, '')
  return trimmed.startsWith('/') ? trimmed : trimmed ? `/${trimmed}` : ''
})()

/** 给以 `/` 开头的站内绝对路径加上部署前缀（外部 URL 原样返回） */
export function withBase(path: string): string {
  if (!APP_BASE || !path.startsWith('/')) return path
  if (path === APP_BASE || path.startsWith(`${APP_BASE}/`)) return path
  return `${APP_BASE}${path}`
}

/** 补丁：把 fetch / WebSocket 的站内绝对路径自动加前缀 */
const API_PREFIXES = ['/api/', '/proxy/', '/dav']

function needsBase(input: string): boolean {
  if (!APP_BASE) return false
  return API_PREFIXES.some((prefix) => input === prefix.slice(0, -1) || input.startsWith(prefix))
}

export function installBaseUrlShim() {
  if (!APP_BASE) return

  const originalFetch = window.fetch.bind(window)
  window.fetch = (input: RequestInfo | URL, init?: RequestInit) => {
    if (typeof input === 'string' && needsBase(input)) {
      return originalFetch(withBase(input), init)
    }
    return originalFetch(input as RequestInfo, init)
  }
}
