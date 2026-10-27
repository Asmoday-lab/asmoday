/** 分享与图床 API 客户端（设计文档 3.1 / 6） */

import { ApiError, readApiError } from '../lib/errors'

export interface ShareInfo {
  token: string
  path: string
  is_folder: boolean
  has_password: boolean
  allow_download: boolean
  expires_at: number | null
  created_at: number | null
}

export interface CreateShareResult {
  status: string
  token: string
  url: string
  is_folder: boolean
  has_password: boolean
  expires_at: number | null
}

export interface PublicShareInfo {
  name: string
  path: string
  is_folder: boolean
  has_password: boolean
  allow_download: boolean
  expires_at: number | null
}

export interface ShareEntry {
  name: string
  is_dir: boolean
  size: number
  modified: number | null
  kind: string
  ext: string
  /** 相对分享根的子路径（拼 raw / download 链接用） */
  sub: string
}

export interface ShareListing {
  path: string
  sub: string
  total: number
  has_more: boolean
  entries: ShareEntry[]
}

export const sharesApi = {
  async create(
    path: string,
    options: { password?: string; days?: number; allow_download?: boolean } = {},
  ): Promise<CreateShareResult> {
    const res = await fetch('/api/shares', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        path,
        password: options.password || undefined,
        days: options.days ?? 0,
        allow_download: options.allow_download ?? true,
      }),
    })
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    return (await res.json()) as CreateShareResult
  },

  async list(): Promise<ShareInfo[]> {
    const res = await fetch('/api/shares')
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    const data = (await res.json()) as { items: ShareInfo[] }
    return data.items
  },

  async remove(token: string): Promise<void> {
    const res = await fetch(`/api/shares/${encodeURIComponent(token)}`, { method: 'DELETE' })
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
  },

  /** 公开元信息（无登录态） */
  async info(token: string, password?: string): Promise<PublicShareInfo> {
    const params = new URLSearchParams()
    if (password) params.set('password', password)
    const res = await fetch(`/api/public/share/${encodeURIComponent(token)}?${params.toString()}`)
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    return (await res.json()) as PublicShareInfo
  },

  /** 目录分享条目（无登录态） */
  async listEntries(token: string, sub: string, password?: string): Promise<ShareListing> {
    const params = new URLSearchParams()
    if (sub) params.set('sub', sub)
    if (password) params.set('password', password)
    const res = await fetch(`/api/public/share/${encodeURIComponent(token)}/list?${params.toString()}`)
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    return (await res.json()) as ShareListing
  },

  /** 内联预览地址（图床直链本体） */
  rawUrl(token: string, options: { sub?: string; password?: string; download?: boolean } = {}): string {
    const params = new URLSearchParams()
    if (options.sub) params.set('sub', options.sub)
    if (options.password) params.set('password', options.password)
    const suffix = options.download ? 'download' : 'raw'
    const query = params.toString()
    return `/api/public/share/${encodeURIComponent(token)}/${suffix}${query ? `?${query}` : ''}`
  },
}
