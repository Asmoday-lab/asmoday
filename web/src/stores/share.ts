/**
 * 全局分享弹窗状态（设计文档 3.2.5 / 8.4.13）。
 *
 * 文件右键、照片灯箱、影视库海报墙、音乐列表都要能「分享」，但分享表单
 * （密码 / 有效期 / 是否允许下载 / 结果链接）只应该存在一份 —— 因此把目标
 * 放进模块级单例，由 App.vue 里的唯一一个 `ShareDialog` 消费。
 */
import { ref } from 'vue'

export interface ShareTarget {
  path: string
  name: string
}

export const shareTarget = ref<ShareTarget | null>(null)

export function openShare(path: string, name: string): void {
  shareTarget.value = { path, name }
}

export function closeShare(): void {
  shareTarget.value = null
}
