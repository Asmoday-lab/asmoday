/** 展示层小工具：体积与时间格式化。 */

/**
 * 毫秒 → `3:42` / `1:02:05`（音乐库、影视时长共用）。
 *
 * 算不出来（后端没解析出来、或文件坏）时给 `--:--`：空字符串会让整列错位，
 * 而 `0:00` 会被当成「真是一首零秒的歌」。
 */
export function formatDuration(ms: number | null | undefined): string {
  if (!ms || !Number.isFinite(ms) || ms <= 0) return '--:--'
  const total = Math.round(ms / 1000)
  const hours = Math.floor(total / 3600)
  const minutes = Math.floor((total % 3600) / 60)
  const seconds = total % 60
  const pad = (value: number) => String(value).padStart(2, '0')
  return hours > 0 ? `${hours}:${pad(minutes)}:${pad(seconds)}` : `${minutes}:${pad(seconds)}`
}

/** 总时长的人话（`6 小时 24 分`），Hero 统计用 */
export function formatDurationLong(ms: number | null | undefined): string {
  if (!ms || !Number.isFinite(ms) || ms <= 0) return ''
  const minutes = Math.round(ms / 60000)
  const hours = Math.floor(minutes / 60)
  const rest = minutes % 60
  if (hours === 0) return `${rest} 分钟`
  return rest === 0 ? `${hours} 小时` : `${hours} 小时 ${rest} 分`
}

export function formatSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '—'
  if (bytes < 1024) return `${bytes} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let value = bytes / 1024
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return `${value >= 10 ? value.toFixed(0) : value.toFixed(1)} ${units[unit]}`
}

export function formatTime(unixSeconds: number | null): string {
  if (!unixSeconds) return '—'
  const date = new Date(unixSeconds * 1000)
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`
}
