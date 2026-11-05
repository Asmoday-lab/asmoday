/** 用户管理 API（管理员，v4.9.24 多用户） */

import { ApiError, readApiError } from '../lib/errors'

export interface ManagedUser {
  id: number
  username: string
  role: 'admin' | 'user'
  is_active: boolean
  totp_enabled: boolean
  must_change_password: boolean
  /** 权限位掩码（v4.9.25） */
  permissions: number
  /** 挂载点收权：'*' 或 JSON 字符串数组 */
  mount_scope: string
}

async function request<T>(url: string, method: string, body?: unknown): Promise<T> {
  const res = await fetch(url, {
    method,
    headers: body === undefined ? { Accept: 'application/json' } : { 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  if (!res.ok) throw new ApiError(res.status, await readApiError(res))
  return (await res.json()) as T
}

export const usersApi = {
  async list(): Promise<ManagedUser[]> {
    const data = await request<{ items: ManagedUser[] }>('/api/users', 'GET')
    return data.items
  },
  async create(username: string, password: string, role: 'admin' | 'user'): Promise<ManagedUser> {
    const data = await request<{ user: ManagedUser }>('/api/users', 'POST', {
      username,
      password,
      role,
    })
    return data.user
  },
  async update(
    id: number,
    patch: {
      username?: string
      password?: string
      role?: 'admin' | 'user'
      is_active?: boolean
      permissions?: number
      mount_scope?: string
    },
  ): Promise<ManagedUser> {
    const data = await request<{ user: ManagedUser }>(`/api/users/${id}`, 'PATCH', patch)
    return data.user
  },
  async remove(id: number): Promise<void> {
    await request(`/api/users/${id}`, 'DELETE')
  },
}
