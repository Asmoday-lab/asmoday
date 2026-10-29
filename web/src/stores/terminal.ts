import { useStorage } from '@vueuse/core'
import { defineStore } from 'pinia'
import { computed, ref } from 'vue'

export type TerminalStatus = 'connecting' | 'ready' | 'closed' | 'error'

export interface TerminalTab {
  id: string
  title: string
  cwd: string
  /** 后端会话 id，重连时带上即可原地恢复 */
  sessionId: string | null
  status: TerminalStatus
  /** 纯净模式：跳过 profile/rc（设计文档 3.3.7，救急用） */
  noProfile: boolean
  /** 服务端平台（windows/unix），决定是否显示「修复乱码」入口 */
  platform: string
}

/** 纯净模式的默认值跨会话记住：用户一旦为了救急打开，下次还按这个来 */
const SAFE_MODE_KEY = 'asmoday.terminal.noProfile'

/**
 * 纯净模式默认值。
 *
 * 历史值是字符串 `'1'` / `'0'`（早期直接读写 localStorage），
 * 这里用自定义 serializer 保持兼容——改成布尔序列化会把老用户的设置读丢。
 */
const safeModeDefault = useStorage(SAFE_MODE_KEY, false, undefined, {
  serializer: {
    read: (raw: string) => raw === '1',
    write: (value: boolean) => (value ? '1' : '0'),
  },
})

export function saveSafeModeDefault(value: boolean) {
  safeModeDefault.value = value
}

function basename(path: string): string {
  if (!path) return '~'
  const parts = path.split('/').filter(Boolean)
  return parts[parts.length - 1] ?? path
}

export const useTerminalStore = defineStore('terminal', () => {
  const tabs = ref<TerminalTab[]>([])
  const activeId = ref('')
  let seq = 0

  const active = computed(() => tabs.value.find((tab) => tab.id === activeId.value) ?? null)

  function openTab(cwd: string, title?: string) {
    const id = `tab-${++seq}`
    tabs.value.push({
      id,
      title: title ?? basename(cwd),
      cwd,
      sessionId: null,
      status: 'connecting',
      noProfile: safeModeDefault.value,
      platform: '',
    })
    activeId.value = id
    return id
  }

  function closeTab(id: string) {
    const index = tabs.value.findIndex((tab) => tab.id === id)
    if (index === -1) return
    tabs.value.splice(index, 1)
    if (activeId.value === id) {
      const next = tabs.value[index] ?? tabs.value[index - 1] ?? tabs.value[0]
      activeId.value = next?.id ?? ''
    }
  }

  function setActive(id: string) {
    activeId.value = id
  }

  function updateTab(id: string, patch: Partial<TerminalTab>) {
    const tab = tabs.value.find((item) => item.id === id)
    if (tab) Object.assign(tab, patch)
  }

  return { tabs, activeId, active, openTab, closeTab, setActive, updateTab }
})
