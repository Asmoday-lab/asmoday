/**
 * 终端会话管理（设计文档 3.3.6）。
 *
 * 并发上限 10 个，打满后前端必须能列出「谁在占着」并强制关闭，
 * 否则用户只会看到「已达上限」却无从下手。
 */
import { readApiError } from '../lib/errors'

export interface TerminalSession {
  id: string
  cwd: string
  shell: string
  no_profile: boolean
  cols: number
  rows: number
  /** 当前挂着的 WebSocket 数，0 = 已断线正在后台保活 */
  clients: number
  age_secs: number
}

export interface TerminalSessionsResponse {
  count: number
  limit: number
  sessions: TerminalSession[]
}

export async function listTerminalSessions(): Promise<TerminalSessionsResponse> {
  const res = await fetch('/api/terminal/sessions', { credentials: 'same-origin' })
  if (!res.ok) throw new Error(await readApiError(res))
  return (await res.json()) as TerminalSessionsResponse
}

export async function killTerminalSession(id: string): Promise<void> {
  const res = await fetch(`/api/terminal/sessions/${encodeURIComponent(id)}`, {
    method: 'DELETE',
    credentials: 'same-origin',
  })
  // 404 视为已完成：会话自己先结束了，用户的目标同样达成
  if (!res.ok && res.status !== 404) throw new Error(await readApiError(res))
}
