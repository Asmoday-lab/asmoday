/**
 * 后端错误本地化（设计文档 3.8 第 2 条）。
 *
 * 后端统一返回 `{ code, message }`（message 为英文）；前端**只按 code** 查
 * i18n 的 `errors.<CODE>`，不把英文 message 直接甩给用户。
 * 兼容旧接口：没有 code 时退回 `{ error }` 文本，仍拿不到就给通用提示。
 */
import { i18n } from './i18n'

export interface ApiErrorBody {
  code?: string
  message?: string
  /** 兼容字段：旧版后端与旧缓存前端 */
  error?: string
}

/** code → 当前语言文案；未收录的 code 返回 null，交给调用方兜底 */
export function localizedCode(code?: string | null): string | null {
  if (!code) return null
  const key = `errors.${code}`
  if (!i18n.global.te(key)) return null
  return i18n.global.t(key)
}

/** 从已解析的响应体 + 状态码生成用户可读文案 */
export function messageFromBody(body: ApiErrorBody | null, status: number): string {
  const byCode = localizedCode(body?.code)
  if (byCode) return byCode
  const legacy = body?.message ?? body?.error
  if (legacy) return legacy
  return `${i18n.global.t('errors.UNKNOWN')} (HTTP ${status})`
}

/** 读取失败响应并本地化（各 api 客户端共用） */
export async function readApiError(res: Response): Promise<string> {
  let body: ApiErrorBody | null = null
  try {
    body = (await res.json()) as ApiErrorBody
  } catch {
    body = null
  }
  return messageFromBody(body, res.status)
}

/** 业务层抛出的错误对象：保留 code 便于特殊分支判断（如分享口令） */
export class ApiError extends Error {
  readonly status: number
  readonly code: string | null

  constructor(status: number, message: string, code: string | null = null) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
  }

  hasCode(code: string): boolean {
    return this.code === code
  }
}
