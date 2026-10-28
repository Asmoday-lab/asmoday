/**
 * 文件管理 API 客户端。所有路径一律使用后端返回的 POSIX 风格绝对路径。
 *
 * 变更类接口（设计文档 6.2）统一返回逐项结果 + summary，前端按 `summary` 汇总，
 * 按 `error_code` 逐项翻译，绝不把后端的英文 message 直接甩给用户。
 */
import { ApiError, readApiError } from '../lib/errors'

export { ApiError }

export type FileKind = 'dir' | 'image' | 'video' | 'audio' | 'text' | 'archive' | 'other'

/** 挂载点（`GET /api/files/mounts`，设计文档 6.1） */
export interface MountPoint {
  id: string
  name: string
  path: string
  readonly: boolean
  online: boolean
  implicit: boolean
}

/** 兼容旧接口形状（roots） */
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
  is_symlink?: boolean
  size: number
  modified: number | null
  kind: FileKind
  ext: string
  editable: boolean
  /** 单条 stat 失败（权限 / 断链），前端显示占位而不是整页报错 */
  stat_error?: boolean
}

export interface Listing {
  path: string
  parent: string | null
  page: number
  page_size: number
  total: number
  has_more: boolean
  /** 目录条目超过后端扫描上限被截断（设计文档 3.2.4） */
  truncated?: boolean
  /** 所在挂载点只读：全部变更入口置灰 */
  readonly?: boolean
  entries: EntryInfo[]
}

/** 覆盖策略：后端不接受 ask，由弹窗在客户端解析成具体值 */
export type OnConflict = 'overwrite' | 'rename' | 'skip'

export interface ItemResult {
  path: string
  source?: string
  ok: boolean
  new_path?: string
  error_code?: string
  message?: string
  skipped?: boolean
}

export interface BatchResponse {
  results: ItemResult[]
  summary: { ok: number; failed: number; skipped: number }
}

export interface FileStat {
  path: string
  name: string
  is_dir: boolean
  is_symlink: boolean
  symlink_target: string | null
  size: number
  mtime: number | null
  mime: string
  real_path: string | null
  readonly: boolean
  mode: number | null
  inode: number | null
  owner: string | null
}

async function getJson<T>(url: string): Promise<T> {
  const res = await fetch(url, { headers: { Accept: 'application/json' } })
  if (!res.ok) throw new ApiError(res.status, await readApiError(res))
  return (await res.json()) as T
}

async function postJson<T>(url: string, body: unknown): Promise<T> {
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
    body: JSON.stringify(body),
  })
  if (!res.ok) throw new ApiError(res.status, await readApiError(res))
  return (await res.json()) as T
}

const encodePath = (path: string) => encodeURIComponent(path)

export const filesApi = {
  /** 挂载点清单：每次进入文件管理都要重新拉，禁止缓存（设计文档 3.2.1） */
  mounts: () => getJson<{ mounts: MountPoint[]; implicit: boolean }>('/api/files/mounts'),

  roots: () => getJson<RootsResponse>('/api/files/roots'),

  /** `showHidden` 对应后端 `show_hidden`（设计文档 3.2 / 6.1） */
  list: (path: string, page = 1, showHidden = false, sort = 'name', order = 'asc') =>
    getJson<Listing>(
      `/api/files/list?path=${encodePath(path)}&page=${page}&show_hidden=${showHidden}&sort=${sort}&order=${order}`,
    ),

  /** 直接给 <img>/<video> 用；同样走挂载点校验 */
  rawUrl: (path: string) => `/api/files/raw?path=${encodePath(path)}`,

  async readText(path: string): Promise<string> {
    const res = await fetch(filesApi.rawUrl(path))
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    return res.text()
  },

  /**
   * 只读前缀（CSV 大文件截断预览用）：走 HTTP Range，避免把几百 MB 全塞进内存。
   * 服务端返回 200（不支持或忽略 Range）时也按 limit 截断。
   */
  async readTextPrefix(path: string, limit: number): Promise<string> {
    const res = await fetch(filesApi.rawUrl(path), {
      headers: { Range: `bytes=0-${Math.max(0, limit - 1)}` },
    })
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    const buffer = await res.arrayBuffer()
    return new TextDecoder('utf-8', { fatal: false }).decode(buffer.slice(0, limit))
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

  // -------------------------------------------------------------------------
  // 变更操作（设计文档 6.2）
  // -------------------------------------------------------------------------

  mkdir: (parent: string, name: string) =>
    postJson<{ ok: boolean; path: string }>('/api/files/mkdir', { path: parent, name }),

  touch: (parent: string, name: string) =>
    postJson<{ ok: boolean; path: string }>('/api/files/touch', { path: parent, name }),

  rename: (path: string, newName: string) =>
    postJson<{ ok: boolean; path: string; new_path: string }>('/api/files/rename', {
      path,
      new_name: newName,
    }),

  move: (paths: string[], destPath: string, onConflict: OnConflict) =>
    postJson<BatchResponse>('/api/files/move', {
      paths,
      dest_path: destPath,
      on_conflict: onConflict,
    }),

  copy: (paths: string[], destPath: string, onConflict: OnConflict) =>
    postJson<BatchResponse>('/api/files/copy', {
      paths,
      dest_path: destPath,
      on_conflict: onConflict,
    }),

  /** 批量删除必须先取一次性令牌（60 秒、用后即删） */
  deleteToken: (paths: string[]) =>
    postJson<{ token: string; expires_in: number }>('/api/files/delete-token', { paths }),

  async remove(paths: string[], confirmName?: string): Promise<BatchResponse> {
    const body: Record<string, unknown> = { paths }
    if (paths.length > 1) {
      body.confirm_token = (await filesApi.deleteToken(paths)).token
    } else if (confirmName) {
      body.confirm_name = confirmName
    }
    return postJson<BatchResponse>('/api/files/delete', body)
  },

  /**
   * 删除前统计：项数 / 总字节 / 是否触达上限（设计文档 3.2.6.5）。
   * 只读接口，不产生任何变更，因此不消耗 delete-token。
   */
  measure: (paths: string[]) =>
    postJson<{ items: number; bytes: number; truncated: boolean }>('/api/files/measure', {
      paths,
    }),

  stat: (path: string) =>
    getJson<FileStat>(`/api/files/stat?path=${encodePath(path)}`),

  hash: (path: string) =>
    postJson<{ path: string; algorithm: string; hash: string; size: number }>(
      '/api/files/hash',
      { path },
    ),
}

/**
 * 一次变更操作的结果汇总：成功/失败/跳过。
 * 失败项带上后端 error_code，由调用方翻译成用户可读文案。
 */
export function summarizeResults(response: BatchResponse) {
  const failed = response.results.filter((item) => !item.ok && !item.skipped)
  return {
    ok: response.summary.ok,
    failed: response.summary.failed,
    skipped: response.summary.skipped,
    firstError: failed[0]?.error_code ?? null,
    failures: failed,
  }
}
