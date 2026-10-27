/** 系统状态 API（设计文档 6.2）：FFmpeg 探测 / 测试连通性 */

import { ApiError, readApiError } from '../lib/errors'

export type FfmpegStatus = 'ok' | 'missing'

/** 路径来源，与后端 `core::ffmpeg::FfmpegSource` 一一对应 */
export type FfmpegSource = 'config' | 'env' | 'path' | 'none'

export interface FfmpegInfo {
  status: FfmpegStatus
  version: string | null
  path: string | null
  source: FfmpegSource
}

export const systemApi = {
  /** 按「设置页 > 环境变量 > PATH」三层优先级探测当前生效的 FFmpeg */
  async ffmpegStatus(): Promise<FfmpegInfo> {
    const res = await fetch('/api/system/ffmpeg-status')
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    return (await res.json()) as FfmpegInfo
  },

  /** 只测指定路径；留空则等价于探测当前生效的那个 */
  async testFfmpeg(path: string): Promise<FfmpegInfo> {
    const res = await fetch('/api/system/test-ffmpeg', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path }),
    })
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    return (await res.json()) as FfmpegInfo
  },
}
