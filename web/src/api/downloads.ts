/**
 * 下载页 API（v4.9.31，设计文档 3.10）。
 *
 * 走 Cookie 会话的 `/api/downloads*`；`/jsonrpc` 那条 aria2 通道仍是给外部
 * 客户端用的，浏览器不碰（密钥不进前端存储）。
 */
import { readApiError } from '../lib/errors'

/** aria2 `tellStatus` 的任务形状（后端直出，字段名保持一致，缺数据填 0/空） */
export interface TaskFile {
  index: number
  path: string
  length: number
  completedLength: number
  selected: boolean
}

export interface DownloadTask {
  gid: string
  /** 列表展示名：BT 用种子名，HTTP 用落盘文件名（magnet 解析前为空） */
  name: string
  kind: 'http' | 'bt'
  status: 'active' | 'waiting' | 'paused' | 'error' | 'complete' | 'removed'
  totalLength: number
  completedLength: number
  uploadLength: number
  downloadSpeed: number
  uploadSpeed: number
  connections: number
  errorCode: number
  errorMessage: string
  dir: string
  files: TaskFile[]
  seeder?: boolean
  numPieces?: number
  pieceLength?: number
  bittorrent?: { info?: { name?: string } }
}

export interface GlobalStat {
  downloadSpeed: number
  uploadSpeed: number
  numActive: number
  numWaiting: number
  numStopped: number
  numStoppedTotal: number
}

export interface Snapshot {
  enabled: boolean
  bt: boolean
  global?: GlobalStat
  tasks?: {
    active: DownloadTask[]
    waiting: DownloadTask[]
    stopped: DownloadTask[]
  }
}

export interface EngineInfo {
  enabled: boolean
  bt: boolean
  /** 入站 peer 端口；未监听为 null（只在出站模式下） */
  bt_port: number | null
  segments: number
  max_concurrent: number
  roots: string[]
  rpc_path: string
  /** 仅管理端返回：给用户填 AriaNg */
  rpc_secret: string
}

async function request<T>(input: string, init?: RequestInit): Promise<T> {
  const res = await fetch(input, init)
  if (!res.ok) throw new Error(await readApiError(res))
  return (await res.json()) as T
}

export const downloadsApi = {
  snapshot: () => request<Snapshot>('/api/downloads'),

  engine: () => request<EngineInfo>('/api/downloads/engine'),

  /** 刷新 RPC 密钥：写库 + 换内存，旧密钥下一次请求即 401（v4.9.36） */
  rotateSecret: () =>
    request<{ status: string; rpc_secret: string }>('/api/downloads/secret', { method: 'POST' }),

  addUri: (payload: { uri: string; dir?: string }) =>
    request<{ gid: string }>('/api/downloads', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    }),

  /** `.torrent` 走 base64 JSON：省掉 multipart，与 RPC 的 addTorrent 同一入口 */
  addTorrent: async (file: File, dir?: string) =>
    request<{ gid: string }>('/api/downloads/torrents', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ torrent: await fileToBase64(file), dir }),
    }),

  pause: (gid: string) =>
    request<{ gid: string }>(`/api/downloads/${encodeURIComponent(gid)}/pause`, { method: 'POST' }),

  unpause: (gid: string) =>
    request<{ gid: string }>(`/api/downloads/${encodeURIComponent(gid)}/unpause`, {
      method: 'POST',
    }),

  remove: (gid: string) =>
    request<{ gid: string }>(`/api/downloads/${encodeURIComponent(gid)}`, { method: 'DELETE' }),

  purge: () => request<{ removed: number }>('/api/downloads/stopped', { method: 'DELETE' }),
}

/**
 * File → base64（去掉 `data:...;base64,` 前缀）。
 *
 * 用 FileReader 而不是 `arrayBuffer()` + 手写编码：后者要自己处理 3 字节分组，
 * 种子文件虽然不大，但没必要为它多写一段容易越界的代码。
 */
function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onerror = () => reject(new Error('读取种子文件失败'))
    reader.onload = () => {
      const result = String(reader.result ?? '')
      const comma = result.indexOf(',')
      resolve(comma >= 0 ? result.slice(comma + 1) : result)
    }
    reader.readAsDataURL(file)
  })
}
