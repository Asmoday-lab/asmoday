/**
 * 触屏长按（设计文档 8.6：500ms 阈值、位移 >10px 判为滚动并取消）。
 *
 * 为什么单独抽出来：相册、音乐列表、影视库都没有「右键」，长按是它们唯一的
 * 操作入口；三处各写一遍计时器就会出现三套阈值（FileBrowser 的 500/10 是基准）。
 *
 * 用法（模板里显式绑四个事件，payload 由调用处给）：
 * ```html
 * <tr @touchstart.passive="press.start($event, track)"
 *     @touchmove.passive="press.move"
 *     @touchend="press.cancel"
 *     @touchcancel="press.cancel"
 *     @click="if (!press.takeFired()) play(track)">
 * ```
 * 长按之后浏览器还会补一次 click：菜单在这时已经挂着全屏遮罩，那一下会落在遮罩上
 * 直接把菜单关掉（看起来像「长按没反应」）。所以触发后在**捕获阶段**吃掉紧随其后的
 * 一次 click；`takeFired()` 是给元素自身 click 处理器的第二道保险。
 */
import { onScopeDispose } from 'vue'

/** 长按判定阈值（毫秒） */
export const LONG_PRESS_MS = 500
/** 判定为滚动的位移（像素） */
export const LONG_PRESS_SLOP = 10

export function useLongPress<T>(
  fire: (payload: T) => void,
  options: { ms?: number; slop?: number } = {},
) {
  const ms = options.ms ?? LONG_PRESS_MS
  const slop = options.slop ?? LONG_PRESS_SLOP
  let timer: number | null = null
  let fired = false
  let origin = { x: 0, y: 0 }
  /** 吃掉 click 的截止时间（长按后浏览器补的那一下通常 <100ms 就到） */
  let swallowUntil = 0

  /**
   * 捕获阶段拦截：遮罩在长按弹层里是全屏的，click 目标是遮罩而不是卡片，
   * 冒泡阶段拦已经晚了（遮罩的 close 在冒泡里执行）。
   */
  function interceptClick(event: MouseEvent) {
    if (!swallowUntil) return
    if (Date.now() > swallowUntil) {
      swallowUntil = 0
      return
    }
    swallowUntil = 0
    event.stopPropagation()
    event.preventDefault()
  }
  document.addEventListener('click', interceptClick, true)

  function cancel() {
    if (timer === null) return
    window.clearTimeout(timer)
    timer = null
  }

  function start(event: TouchEvent, payload: T) {
    const touch = event.touches[0]
    if (!touch) return
    fired = false
    origin = { x: touch.clientX, y: touch.clientY }
    cancel()
    timer = window.setTimeout(() => {
      timer = null
      fired = true
      swallowUntil = Date.now() + 700
      navigator.vibrate?.(50)
      fire(payload)
    }, ms)
  }

  function move(event: TouchEvent) {
    if (timer === null) return
    const touch = event.touches[0]
    if (!touch) return
    if (
      Math.abs(touch.clientX - origin.x) > slop ||
      Math.abs(touch.clientY - origin.y) > slop
    ) {
      cancel()
    }
  }

  /** 取出「刚刚长按过」的标记：click 处理器据此跳过本次点击 */
  function takeFired() {
    const value = fired
    fired = false
    return value
  }

  onScopeDispose(() => {
    cancel()
    document.removeEventListener('click', interceptClick, true)
  })

  return { start, move, cancel, takeFired }
}
