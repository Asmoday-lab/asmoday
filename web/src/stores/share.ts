/**
 * 全局分享弹窗状态（设计文档 3.2.5 / 8.4.13）。
 *
 * 文件右键、照片灯箱、影视库海报墙、音乐列表都要能「分享」，但分享表单
 * （密码 / 有效期 / 是否允许下载 / 结果链接）只应该存在一份 —— 因此把目标
 * 放进全局 store，由 App.vue 里的唯一一个 `ShareDialog` 消费。
 */
import { defineStore } from 'pinia'
import { ref, toRef } from 'vue'

import { pinia } from './pinia'

export interface ShareTarget {
  path: string
  name: string
}

export const useShareStore = defineStore('share', () => {
  const target = ref<ShareTarget | null>(null)

  function open(path: string, name: string) {
    target.value = { path, name }
  }

  function close() {
    target.value = null
  }

  return { target, open, close }
})

// 组件外直接可用的具名导出（显式传实例，不依赖 activePinia）
const share = useShareStore(pinia)

export const shareTarget = toRef(share, 'target')
export const openShare = share.open
export const closeShare = share.close
