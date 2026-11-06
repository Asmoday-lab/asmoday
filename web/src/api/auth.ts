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
  /** 唯一账号还停在控制台一次性密码上：/login 应切出初始化形态（v4.9.33） */
  needs_setup?: boolean
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
  username: computed(() => status.value.username),
  /**
   * 是否「设备主人」。
   *
   * v4.9.28 回归单用户后这件事等价于「已登录」——唯一账号就是管理员，
   * 没有角色与权限位可判。保留这个名字是因为设置页的 tab 过滤还在用它。
   */
  isAdmin: computed(() => status.value.authenticated),
  initialized: computed(() => status.value.initialized),
  ensureAuth,
  refresh,
  /**
   * 登录。单用户后没有用户名输入框：username 由后端按唯一账号解析（留空即可）。
   */
  async login(password: string, totp?: string) {
    await post('/api/auth/login', { password, totp: totp || undefined })
  },
  /**
   * 初始化：输入控制台首启打印的一次性密码 + 新密码，一步建权并登录。
   * 仅在唯一账号还停在一次性密码上时可用；完成后该入口永久关闭。
   */
  async setup(oneTimePassword: string, password: string) {
    await post('/api/auth/setup', { one_time_password: oneTimePassword, password })
  },
  async logout() {
    await post('/api/auth/logout')
  },
  async changePassword(current: string, next: string) {
    await post('/api/auth/password', { current, next })
  },
  /** 改自己的用户名（改名后其他设备全部登出，当前浏览器自动续期） */
  async changeUsername(current: string, username: string) {
    await post('/api/auth/username', { current, username })
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
