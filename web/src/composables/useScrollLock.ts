/**
 * 弹窗/灯箱/抽屉打开时锁定底层滚动（设计文档 8.4 第 5 条）。
 *
 * 本项目的滚动容器不是 `body`，而是布局里的 `#app-scroll`，
 * 所以这里直接锁那个元素；用计数保证多层弹窗叠开时不会提前解锁。
 */
import { onBeforeUnmount, watch, type Ref } from 'vue'

let locks = 0

function container(): HTMLElement | null {
  return document.getElementById('app-scroll')
}

function apply() {
  const el = container()
  const body = document.body
  if (el) el.style.overflow = locks > 0 ? 'hidden' : ''
  body.style.overscrollBehavior = locks > 0 ? 'contain' : ''
}

export function useScrollLock(active: Ref<boolean>) {
  let held = false

  const sync = (value: boolean) => {
    if (value === held) return
    held = value
    locks = Math.max(0, locks + (value ? 1 : -1))
    apply()
  }

  watch(active, sync, { immediate: true })
  onBeforeUnmount(() => sync(false))
}
