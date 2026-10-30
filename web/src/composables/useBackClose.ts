/**
 * 系统返回键接管（Android 物理返回 / 浏览器后退 / 移动端边缘滑动返回）。
 *
 * 背景：编辑器、灯箱、预览面板、各类确认弹窗都不在路由里，旧实现下按系统返回
 * 等于「直接离开当前页面」——未保存的编辑内容没有任何确认机会，用户也会因为
 * 想关掉一个弹窗而丢掉整个目录位置。
 *
 * 做法：只要还有一层浮层开着，就在历史里压一条**同 URL** 的哨兵记录（只压一条，
 * 不随层数增长）。收到 popstate 就先关掉最上层浮层；浮层由 UI 自己关掉时，若哨兵
 * 还停在当前位置就 history.back() 把它退掉，让历史深度始终等于「用户感知的层数」，
 * 不留「按了没反应」的空步。
 *
 * 为什么不用 `history.pushState` 改 URL：文件管理把目录同步进了 URL（设计文档 3.2.1），
 * 给浮层染色会污染深链接、刷新语义和 sitemap 之外的一切判断。
 */
import { nextTick, onScopeDispose, toValue, watch, type MaybeRefOrGetter } from 'vue'

/** 哨兵记录在 history.state 里的标记（与 vue-router 自己的 position/scroll 并存） */
const MARK = '__asmodayBack'

interface BackLayer {
  close: () => void
}

/** 打开中的浮层，栈顶 = 视觉最上层 = 返回键应该先关掉的那层 */
const layers: BackLayer[] = []
/** 历史里当前是否压着一条我们自己塞的哨兵记录 */
let trapArmed = false
let listening = false

function isTrapEntry(): boolean {
  const current = history.state as Record<string, unknown> | null
  return Boolean(current && typeof current === 'object' && current[MARK])
}

/** 压哨兵：复制当前 state 再打标记，保留 vue-router 的 position/scroll 字段 */
function arm() {
  if (trapArmed) return
  trapArmed = true
  const current = history.state as Record<string, unknown> | null
  const state = current && typeof current === 'object' ? { ...current } : {}
  state[MARK] = true
  history.pushState(state, '')
}

/**
 * 退哨兵。只在「哨兵仍停在当前历史位置」时才退：
 * 已经被返回键消费掉的不需要再退一次，否则会连累上一层页面。
 */
function disarm() {
  if (!trapArmed) return
  trapArmed = false
  if (isTrapEntry()) history.back()
}

function onPopState() {
  // disarm() 自己触发的 popstate：此时 trapArmed 已置 false，直接忽略
  if (!trapArmed) return
  trapArmed = false
  if (!layers.length) return

  layers[layers.length - 1].close()

  // close() 可能被用户取消（例如「有未保存的修改，确定关闭？」点取消），
  // 这时浮层仍然开着，必须等一轮响应式更新后重新支起哨兵，
  // 否则下一次返回就直接漏到路由层，把整页退出去了
  void nextTick(() => {
    if (!trapArmed && layers.length) arm()
  })
}

function ensureListener() {
  if (listening) return
  listening = true
  window.addEventListener('popstate', onPopState)
}

/**
 * 手动登记一层浮层（拿不到响应式开关时用）。
 *
 * @returns 释放函数：关闭浮层后必须调用，否则哨兵会永远留在历史里
 */
export function registerBackLayer(close: () => void): () => void {
  ensureListener()
  const layer: BackLayer = { close }
  layers.push(layer)
  arm()
  return () => {
    const index = layers.indexOf(layer)
    if (index === -1) return
    layers.splice(index, 1)
    // 还有别的浮层开着就保持哨兵；全部关掉才把哨兵退掉
    if (!layers.length) disarm()
  }
}

/**
 * 声明式写法：`isOpen` 为真期间接管系统返回，返回动作交给 `onBack`。
 *
 * 多层浮层可以并存（相册详情 → 灯箱），返回键按「后开先关」逐层退出。
 */
export function useBackClose(isOpen: MaybeRefOrGetter<boolean>, onBack: () => void): void {
  let release: (() => void) | null = null

  watch(
    () => toValue(isOpen),
    (open) => {
      if (open && !release) {
        release = registerBackLayer(onBack)
        return
      }
      if (!open && release) {
        release()
        release = null
      }
    },
    { immediate: true },
  )

  // 组件卸载（路由已切走）时不能留下无人认领的哨兵
  onScopeDispose(() => {
    if (!release) return
    release()
    release = null
  })
}
