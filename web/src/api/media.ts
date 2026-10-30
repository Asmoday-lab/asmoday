/** 媒体库 API 客户端 */

import { withBase } from '../lib/base'
import { ApiError, readApiError } from '../lib/errors'

export type MediaKind = 'image' | 'video' | 'audio'

export interface MediaItem {
  name: string
  path: string
  size: number
  modified: number | null
  kind: MediaKind
  ext: string
  folder: string
  /** 音频内嵌 ID3 标题 / 艺术家 / 专辑（其它类型缺省） */
  title?: string | null
  artist?: string | null
  album?: string | null
  /** 音频是否有内嵌封面；true 时才值得去拉 coverUrl() */
  cover?: boolean
  /** ThumbHash（base64，25 字节左右），缩略图生成过才有 */
  hash?: string | null
  /** 由 ThumbHash 解出的平均色，用作加载占位底色 */
  color?: string | null
  /** 同目录同名外挂字幕（.vtt / .srt，视频才有） */
  subtitle?: string | null
}

/** 各类型总数：切换分区时不必等全部翻完就知道库里到底有多少 */
export interface KindCounts {
  image: number
  video: number
  audio: number
}

export interface BrowseResult {
  root: string
  items: MediaItem[]
  /** 本次 readdir 到的目录项数（诊断用） */
  scanned: number
  /** 命中 kind 过滤的总条数（不是本页条数） */
  total: number
  counts: KindCounts
  has_more: boolean
  next_cursor?: string | null
}

export interface PageOptions {
  limit?: number
  cursor?: string | null
  /** 强制穿透后端目录快照重扫（「重新扫描」按钮用；默认走缓存） */
  refresh?: boolean
}

/**
 * 媒体列表：游标分页（设计文档 3.4）。
 * 传上一页的 `next_cursor` 取下一页；后端不再有「扫到上限就截断」这回事。
 */
export async function browseMedia(
  path: string | undefined,
  kind: 'media' | MediaKind = 'media',
  depth = 2,
  options: PageOptions = {},
): Promise<BrowseResult> {
  const params = new URLSearchParams({ kind, depth: String(depth) })
  if (path) params.set('path', path)
  if (options.limit) params.set('limit', String(options.limit))
  if (options.cursor) params.set('cursor', options.cursor)
  if (options.refresh) params.set('refresh', '1')
  const res = await fetch(`/api/media/browse?${params.toString()}`)
  if (!res.ok) throw new ApiError(res.status, await readApiError(res))
  return (await res.json()) as BrowseResult
}

/**
 * 相册（照片库「相册视图」的卡片，设计文档 3.4.3）。
 * 后端已剔除内部没有任何图片的目录，`cover` 一定是相册里最新的一张图。
 */
export interface Album {
  name: string
  path: string
  /** 该目录第一层的图片数量（与点进去看到的条数一致） */
  count: number
  /** 是否就是扫描起点（挂载目录本身也算一本相册，v2.8 口径） */
  is_root: boolean
  cover: string | null
  /** 封面的 ThumbHash 与平均色 */
  cover_hash: string | null
  cover_color: string | null
  modified: number | null
}

export interface AlbumsResult {
  root: string
  items: Album[]
  total: number
  has_more: boolean
  next_cursor?: string | null
}

/** 相册清单：回「直接含该类媒体的目录」（含挂载目录），空目录后端已过滤。
 *  `kind` 与照片相册同一套口径，影视库传 `video` 就得到「影视文件夹」清单。 */
export async function browseAlbums(
  path?: string,
  depth = 3,
  options: PageOptions & { kind?: 'image' | 'video' | 'audio' } = {},
): Promise<AlbumsResult> {
  const params = new URLSearchParams({ depth: String(depth) })
  if (path) params.set('path', path)
  if (options.kind) params.set('kind', options.kind)
  if (options.limit) params.set('limit', String(options.limit))
  if (options.cursor) params.set('cursor', options.cursor)
  if (options.refresh) params.set('refresh', '1')
  const res = await fetch(`/api/media/albums?${params.toString()}`)
  if (!res.ok) throw new ApiError(res.status, await readApiError(res))
  return (await res.json()) as AlbumsResult
}

/**
 * 缩略图渲染版本：**生成规则一变就 +1**（与服务端 `Thumbnailer::CACHE_VERSION` 对齐）。
 *
 * 它只是 URL 上的盐。浏览器缓存按 URL 认人，光改服务端缓存键没用——旧 URL 仍然
 * 一分钟命中一次旧字节。改规则时同步改这里，所有缩略图 URL 一起换，旧缓存自然作废。
 */
export const THUMB_REV = 'v3'

/**
 * 缩略图（后端信号量限流，拥塞时返回占位图）。
 *
 * `version` 传源文件的 `modified`（秒）。URL 里带它之后，服务端才敢给
 * `immutable` 长缓存：文件一变 URL 就变，不可能拿到过期图。不传也能用，只是
 * 每次都要回源校验（304，便宜但多一趟）。
 */
export function thumbUrl(path: string, width = 320, version?: number | null): string {
  const params = new URLSearchParams({ path, w: String(width), r: THUMB_REV })
  if (version !== undefined && version !== null) params.set('v', String(version))
  return withBase(`/api/files/thumb?${params.toString()}`)
}

/** 原图/原视频。`download` 为真时带 attachment，浏览器直接存盘 */
export function rawUrl(path: string, options: { download?: boolean } = {}): string {
  const params = new URLSearchParams({ path })
  if (options.download) params.set('download', '1')
  return withBase(`/api/files/raw?${params.toString()}`)
}

/** 目录打包下载（store 模式流式 zip） */
export function archiveUrl(path: string): string {
  return withBase(`/api/files/archive?path=${encodeURIComponent(path)}`)
}

/** 音频内嵌封面（ID3 APIC）；无封面时后端回 404，调用方需做兜底 */
export function coverUrl(path: string): string {
  return withBase(`/api/media/cover?path=${encodeURIComponent(path)}`)
}

/** 优先用后端算好的 ThumbHash 平均色，退化时用路径派生色 */
export function placeholderFor(item: MediaItem): string {
  return item.color ?? placeholderColor(item.path)
}

/** 由路径派生一个稳定的占位色（没有 ThumbHash 时的兜底） */
export function placeholderColor(seed: string): string {
  let hash = 0
  for (let i = 0; i < seed.length; i += 1) {
    hash = (hash * 31 + seed.charCodeAt(i)) % 360
  }
  return `hsl(${hash} 28% 22%)`
}

/** 同目录同名 .lrc 歌词文件 */
export function lrcUrl(path: string): string {
  return rawUrl(path.replace(/\.[^./]+$/, '.lrc'))
}

/**
 * 外挂字幕（设计文档 3.4）：后端统一转成 WebVTT 再吐出来，
 * 所以 .srt 也能被浏览器原生 <track> 识别。
 */
export function subtitleUrl(path: string): string {
  return withBase(`/api/files/subtitle?path=${encodeURIComponent(path)}`)
}

// ---------------------------------------------------------------------------
// 播放进度 / 收藏
// ---------------------------------------------------------------------------

export interface PlayHistoryItem {
  path: string
  last_played_at: number | null
  progress: number | null
}

export interface FavoriteItem {
  path: string
  created_at: number | null
}

async function postJson(url: string, body: unknown): Promise<void> {
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })
  if (!res.ok) throw new ApiError(res.status, await readApiError(res))
}

export const libraryApi = {
  async saveProgress(path: string, progress: number): Promise<void> {
    await postJson('/api/media/progress', { path, progress: Math.round(progress) })
  },

  async history(): Promise<PlayHistoryItem[]> {
    const res = await fetch('/api/media/history')
    if (!res.ok) return []
    const data = (await res.json()) as { items: PlayHistoryItem[] }
    return data.items
  },

  async setFavorite(path: string, favorite: boolean): Promise<void> {
    await postJson('/api/media/favorite', { path, favorite })
  },

  async favorites(): Promise<string[]> {
    const res = await fetch('/api/media/favorites')
    if (!res.ok) return []
    const data = (await res.json()) as { items: FavoriteItem[] }
    return data.items.map((item) => item.path)
  },
}

// ---------------------------------------------------------------------------
// 缩略图缓存管理（设计文档 3.4）
// ---------------------------------------------------------------------------

export interface CacheStats {
  files: number
  bytes: number
  limit_mb: number
  max_age_days: number
  dir: string
}

export const cacheApi = {
  async stats(): Promise<CacheStats> {
    const res = await fetch('/api/media/cache')
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    return (await res.json()) as CacheStats
  },
  async clear(): Promise<{ files: number; bytes: number }> {
    const res = await fetch('/api/media/cache/clear', { method: 'POST' })
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    return (await res.json()) as { files: number; bytes: number }
  },
}
