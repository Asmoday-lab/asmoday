/**
 * FLIP 重排动画（v4.9.0）。
 *
 * 排序一变，同一批卡片只是换了位置 —— 这时候最忌讳「瞬间跳过去」。FLIP 的做法：
 *
 * 1. **F**irst：换数据之前把每个卡片的盒子量下来（相对滚动容器，不受滚动影响）；
 * 2. **L**ast：换完数据、等 DOM 更新完，再量一次；
 * 3. **I**nvert：先给位移量一个反向 `transform`，让元素看起来还在旧位置；
 * 4. **P**lay：把 `transform` 归零，用 Web Animations 过渡回去。
 *
 * 用 `Element.animate` 而不是手写 inline `transition`：不用强制回流、不用在
 * `transitionend` 里清理样式，连续快速切换时也只要 `cancel()` 上一个动画即可。
 *
 * 两点刻意的取舍：
 * - 只处理**视口附近**的卡片。几千张时逐个 `getBoundingClientRect()` 会强制整页
 *   重排，为了看不见的卡片卡住主线程不划算。
 * - 新出现的卡片（排序后第一页内容本来就会变）不做位移，只淡入 —— 否则它们会
 *   从「旧列表里某个完全无关的位置」飞进来。
 */
import { nextTick, type Ref } from 'vue'

interface Box {
  x: number
  y: number
  w: number
  h: number
}

export interface FlipPlayOptions {
  duration?: number
  easing?: string
  /** 视口上下各放宽多少像素仍参与动画 */
  margin?: number
}

export interface FlipController {
  /** 换数据**之前**调用 */
  capture: () => void
  /** 换完数据、DOM 更新后调用（内部会自己等一次 nextTick） */
  play: (options?: FlipPlayOptions) => Promise<void>
  /** 丢掉快照（例如整块视图被替换） */
  reset: () => void
}

export function useFlip(
  container: Ref<HTMLElement | null>,
  keyAttribute = 'data-flip-key',
): FlipController {
  let before = new Map<string, Box>()
  const running = new Map<HTMLElement, Animation>()

  function measure(margin: number): Map<string, Box> {
    const boxes = new Map<string, Box>()
    const root = container.value
    if (!root) return boxes
    const rootRect = root.getBoundingClientRect()
    const top = -margin
    const bottom = window.innerHeight + margin
    const nodes = root.querySelectorAll<HTMLElement>(`[${keyAttribute}]`)
    for (const node of nodes) {
      const key = node.getAttribute(keyAttribute)
      if (!key) continue
      const rect = node.getBoundingClientRect()
      if (rect.bottom < top || rect.top > bottom) continue
      boxes.set(key, {
        x: rect.left - rootRect.left + root.scrollLeft,
        y: rect.top - rootRect.top + root.scrollTop,
        w: rect.width,
        h: rect.height,
      })
    }
    return boxes
  }

  function capture() {
    before = measure(0)
  }

  async function play(options: FlipPlayOptions = {}) {
    if (before.size === 0) return
    const duration = options.duration ?? 220
    const easing = options.easing ?? 'cubic-bezier(0.2, 0, 0, 1)'
    const margin = options.margin ?? 200
    await nextTick()
    const root = container.value
    if (!root) {
      before.clear()
      return
    }
    const rootRect = root.getBoundingClientRect()
    const animations: Animation[] = []
    for (const node of root.querySelectorAll<HTMLElement>(`[${keyAttribute}]`)) {
      const key = node.getAttribute(keyAttribute)
      if (!key) continue
      const rect = node.getBoundingClientRect()
      if (rect.bottom < -margin || rect.top > window.innerHeight + margin) continue
      const prev = before.get(key)
      running.get(node)?.cancel()
      if (!prev) {
        // 新卡片：淡入，别从别处飞过来
        const fade = node.animate([{ opacity: 0 }, { opacity: 1 }], {
          duration: Math.round(duration * 0.8),
          easing,
        })
        running.set(node, fade)
        animations.push(fade)
        continue
      }
      const dx = prev.x - (rect.left - rootRect.left + root.scrollLeft)
      const dy = prev.y - (rect.top - rootRect.top + root.scrollTop)
      if (Math.abs(dx) < 0.5 && Math.abs(dy) < 0.5) continue
      // 尺寸也变了（换列数）时补一点缩放，元素不会「一跳一跳」地对不齐
      const sx = prev.w > 0 && rect.width > 0 ? prev.w / rect.width : 1
      const sy = prev.h > 0 && rect.height > 0 ? prev.h / rect.height : 1
      const scale = Math.abs(sx - 1) < 0.02 && Math.abs(sy - 1) < 0.02 ? 1 : Math.min(sx, sy)
      const from =
        scale === 1
          ? `translate(${dx}px, ${dy}px)`
          : `translate(${dx}px, ${dy}px) scale(${scale})`
      const anim = node.animate([{ transform: from }, { transform: 'none' }], {
        duration,
        easing,
      })
      running.set(node, anim)
      animations.push(anim)
    }
    before.clear()
    if (animations.length === 0) return
    await Promise.all(
      animations.map((anim) => anim.finished.catch(() => undefined)),
    )
    for (const [node, anim] of running) {
      if (anim.playState !== 'running') running.delete(node)
    }
  }

  function reset() {
    before.clear()
  }

  return { capture, play, reset }
}
