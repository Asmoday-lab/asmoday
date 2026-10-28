/** 认证 API 与全局登录态 */

import { computed, ref } from 'vue'
import { ApiError, readApiError } from '../lib/errors'

export interface AuthStatus {
  initialized: boolean
  authenticated: boolean
  username: string | null
  totp_enabled: boolean
  /** 一次性密码（--init / --reset）登录后必须改密 */
  must_change_password?: boolean
  version?: string
}

const status = ref<AuthStatus>({
  initialized: true,
  authenticated: false,
  username: null,
  totp_enabled: false,
})
const ready = ref(false)
const busy = ref(false)

let inflight: Promise<void> | null = null

async function refresh(): Promise<void> {
  try {
    const res = await fetch('/api/auth/status', { headers: { Accept: 'application/json' } })
    if (res.ok) {
      status.value = (await res.json()) as AuthStatus
    } else {
      status.value = { initialized: true, authenticated: false, username: null, totp_enabled: false }
    }
  } catch {
    status.value = { initialized: true, authenticated: false, username: null, totp_enabled: false }
  } finally {
    ready.value = true
  }
}

/** 首次导航时只请求一次，避免并发重复 */
export function ensureAuth(): Promise<void> {
  if (!inflight) inflight = refresh()
  return inflight
}

async function post(url: string, body?: unknown): Promise<void> {
  busy.value = true
  try {
    const res = await fetch(url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: body === undefined ? '{}' : JSON.stringify(body),
    })
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    await refresh()
    inflight = Promise.resolve()
  } finally {
    busy.value = false
  }
}

export const authApi = {
  status,
  ready,
  busy,
  isAuthenticated: computed(() => status.value.authenticated),
  username: computed(() => status.value.username),
  initialized: computed(() => status.value.initialized),
  ensureAuth,
  refresh,
  async login(username: string, password: string, totp?: string) {
    await post('/api/auth/login', { username, password, totp: totp || undefined })
  },
  async setup(username: string, password: string) {
    await post('/api/auth/setup', { username, password })
  },
  async logout() {
    await post('/api/auth/logout')
  },
  async changePassword(current: string, next: string) {
    await post('/api/auth/password', { current, next })
  },
  async totpSetup(): Promise<{ secret: string; uri: string }> {
    const res = await fetch('/api/auth/totp/setup')
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    return (await res.json()) as { secret: string; uri: string }
  },
  /** 开启 2FA：返回一次性恢复码（只在这里出现一次，设计文档 3.1.4） */
  async totpEnable(code: string): Promise<string[]> {
    busy.value = true
    try {
      const res = await fetch('/api/auth/totp/enable', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ code }),
      })
      if (!res.ok) throw new ApiError(res.status, await readApiError(res))
      const data = (await res.json()) as { recovery_codes?: string[] }
      await refresh()
      inflight = Promise.resolve()
      return data.recovery_codes ?? []
    } finally {
      busy.value = false
    }
  },
  /** 关闭 2FA：必须同时给出密码与「TOTP 或恢复码」 */
  async totpDisable(password: string, code: string) {
    await post('/api/auth/totp/disable', { password, code })
  },
}
