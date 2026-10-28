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

let seq = 0

const tabs = ref<TerminalTab[]>([])
const activeId = ref('')

/** 纯净模式的默认值跨会话记住：用户一旦为了救急打开，下次还按这个来 */
const SAFE_MODE_KEY = 'asmoday.terminal.noProfile'

function loadSafeMode(): boolean {
  try {
    return window.localStorage.getItem(SAFE_MODE_KEY) === '1'
  } catch {
    return false
  }
}

export function saveSafeModeDefault(value: boolean) {
  try {
    window.localStorage.setItem(SAFE_MODE_KEY, value ? '1' : '0')
  } catch {
    /* 隐私模式下写不进去，忽略 */
  }
}

function basename(path: string): string {
  if (!path) return '~'
  const parts = path.split('/').filter(Boolean)
  return parts[parts.length - 1] ?? path
}

export function useTerminalStore() {
  const active = computed(() => tabs.value.find((tab) => tab.id === activeId.value) ?? null)

  function openTab(cwd: string, title?: string) {
    const id = `tab-${++seq}`
    tabs.value.push({
      id,
      title: title ?? basename(cwd),
      cwd,
      sessionId: null,
      status: 'connecting',
      noProfile: loadSafeMode(),
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
}
