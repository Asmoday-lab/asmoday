/**
 * 敏感操作二次验证（设计文档 3.1.3 的补强）。
 *
 * 会话最长 30 天，但「开系统 shell / 建令牌 / 改配置」这三类操作必须证明
 * 是本人坐在电脑前：后端要求最近 5 分钟内输过密码。这里负责在需要时
 * 弹一次密码框，并把结果返回给调用方（true = 已通过，可以继续）。
 */
import { ref } from 'vue'

/** 弹窗是否可见 */
export const reauthOpen = ref(false)
/** 弹窗内的错误提示（密码错误 / 网络失败） */
export const reauthError = ref('')
/** 弹窗正在提交 */
export const reauthBusy = ref(false)

let resolver: ((ok: boolean) => void) | null = null

/** 服务器是否还要求二次验证（GET 不受时间闸门限制） */
async function serverRequires(): Promise<boolean> {
  try {
    const res = await fetch('/api/auth/reauth', {
      headers: { Accept: 'application/json' },
      credentials: 'same-origin',
    })
    if (!res.ok) return true // 保守：读不到状态就先要求验证
    const data = (await res.json()) as { required?: boolean }
    return data.required !== false
  } catch {
    return true
  }
}

/**
 * 确保当前会话已通过二次验证。
 *
 * - 已经从最近验证过的会话：直接 resolve true，不打扰用户；
 * - 否则弹窗，用户取消 → false，调用方应当直接中止本次操作。
 */
export async function ensureVerified(): Promise<boolean> {
  if (reauthOpen.value) {
    // 已有弹窗在等用户输入：共用同一个 promise，避免叠两层弹窗
    return new Promise<boolean>((resolve) => {
      const previous = resolver
      resolver = (ok) => {
        previous?.(ok)
        resolve(ok)
      }
    })
  }
  if (!(await serverRequires())) return true
  return new Promise<boolean>((resolve) => {
    resolver = resolve
    reauthError.value = ''
    reauthBusy.value = false
    reauthOpen.value = true
  })
}

/** 弹窗里点「确认」 */
export async function submitReauth(password: string): Promise<void> {
  if (reauthBusy.value) return
  reauthBusy.value = true
  reauthError.value = ''
  try {
    const res = await fetch('/api/auth/verify', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      credentials: 'same-origin',
      body: JSON.stringify({ password }),
    })
    if (!res.ok) {
      const body = (await res.json().catch(() => null)) as { code?: string } | null
      reauthError.value = body?.code ?? 'E_REAUTH_FAILED'
      return
    }
    finish(true)
  } catch {
    reauthError.value = 'E_INTERNAL'
  } finally {
    reauthBusy.value = false
  }
}

/** 取消 / 关闭 */
export function cancelReauth(): void {
  finish(false)
}

function finish(ok: boolean): void {
  reauthOpen.value = false
  reauthError.value = ''
  const resolve = resolver
  resolver = null
  resolve?.(ok)
}
