/**
 * 文件管理 API 客户端。所有路径一律使用后端返回的 POSIX 风格绝对路径。
 */
import { ApiError, readApiError } from '../lib/errors'

export { ApiError }

export type FileKind = 'dir' | 'image' | 'video' | 'audio' | 'text' | 'archive' | 'other'

export interface RootEntry {
  name: string
  path: string
  exists: boolean
  implicit: boolean
}

export interface RootsResponse {
  roots: RootEntry[]
  implicit: boolean
  default: string | null
}

export interface EntryInfo {
  name: string
  path: string
  is_dir: boolean
  size: number
  modified: number | null
  kind: FileKind
  ext: string
  editable: boolean
}

export interface Listing {
  path: string
  parent: string | null
  page: number
  page_size: number
  total: number
  has_more: boolean
  entries: EntryInfo[]
}

async function getJson<T>(url: string): Promise<T> {
  const res = await fetch(url, { headers: { Accept: 'application/json' } })
  if (!res.ok) throw new ApiError(res.status, await readApiError(res))
  return (await res.json()) as T
}

const encodePath = (path: string) => encodeURIComponent(path)

export const filesApi = {
  roots: () => getJson<RootsResponse>('/api/files/roots'),

  list: (path: string, page = 1) =>
    getJson<Listing>(`/api/files/list?path=${encodePath(path)}&page=${page}`),

  /** 直接给 <img>/<video> 用；同样走挂载点校验 */
  rawUrl: (path: string) => `/api/files/raw?path=${encodePath(path)}`,

  async readText(path: string): Promise<string> {
    const res = await fetch(filesApi.rawUrl(path))
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    return res.text()
  },

  async saveText(path: string, content: string): Promise<{ status: string; size: number }> {
    const res = await fetch(`/api/files/raw?path=${encodePath(path)}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'text/plain; charset=utf-8' },
      body: content,
    })
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    return (await res.json()) as { status: string; size: number }
  },

  /** 删除文件或目录（递归），挂载点根不允许删 */
  async remove(path: string): Promise<void> {
    const res = await fetch(`/api/files/raw?path=${encodePath(path)}`, { method: 'DELETE' })
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
  },
}
