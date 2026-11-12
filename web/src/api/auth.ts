/** 认证 API 与全局登录态 */

import { computed, ref } from 'vue'
import { ApiError, readApiError } from '../lib/errors'

export interface AuthStatus {
  initialized: boolean
  authenticated: boolean
  totp_enabled: boolean
  /** 一次性密码（--init / --reset）登录后必须改密 */
  must_change_password?: boolean
  version?: string
}

const status = ref<AuthStatus>({
  initialized: true,
  authenticated: false,
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
      status.value = { initialized: true, authenticated: false, totp_enabled: false }
    }
  } catch {
    status.value = { initialized: true, authenticated: false, totp_enabled: false }
  } finally {
    ready.value = true
  }
}

/** 首次导航时只请求一次，避免并发重复 */
export function ensureAuth(): Promise<void> {
  if (!inflight) inflight = refresh()
  return inflight
}

async function post(url: string, body?: unknown, headers?: Record<string, string>): Promise<void> {
  busy.value = true
  try {
    const res = await fetch(url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', ...headers },
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
  initialized: computed(() => status.value.initialized),
  ensureAuth,
  refresh,
  /**
   * 登录。单用户后没有用户名输入框：username 由后端按唯一账号解析（留空即可）。
   *
   * 两种调用形态（v4.9.48）：
   * - 密码模式：`login(password)`，只带 password
   * - 动态码模式：`login('', code)`，只带 totp（页面上是一枚 6 位码框）
   *
   * 两个都给（`login(password, code)`）= 严格双因子，脚本 / 第三方客户端用。
   */
  async login(credential: string, totp?: string) {
    await post('/api/auth/login', { password: credential, totp: totp || undefined })
  },
  async logout() {
    await post('/api/auth/logout')
  },
  /**
   * 改密。正式密码会话必须带 current；一次性密码会话（must_change_password=1）
   * 传空即可——登录时已验过一次性密码，后端不再重复校验（v4.9.33）。
   */
  async changePassword(current: string, next: string) {
    await post('/api/auth/password', { current, next })
  },
  /**
   * 生成 TOTP 密钥。
   * `qr_svg` 是服务端渲染好的二维码（otpauth 链接的 SVG），直接 v-html 塞进页面，
   * 前端不引 QR 库；服务端异常时会缺这个字段，此时退回只显示明文密钥。
   */
  async totpSetup(): Promise<{ secret: string; uri: string; qr_svg?: string | null }> {
    const res = await fetch('/api/auth/totp/setup')
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    return (await res.json()) as { secret: string; uri: string; qr_svg?: string | null }
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
