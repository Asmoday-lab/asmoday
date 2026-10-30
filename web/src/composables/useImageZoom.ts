/**
 * 图片缩放 / 平移 / 触屏手势（设计文档 3.2 / 8.4.9）。
 *
 * 灯箱的图片预览原来只是一张 `object-contain` 的静态图，看不清细节；这里补齐
 * FileBrowser `ExtendedImage` 与 Telegram 都有的那套基础能力：
 *
 * - **滚轮缩放**：以光标位置为锚点（放大时光标下的那一点不动），1.12 倍步进，
 *   范围 1–4；`1` 是「适合窗口」，再往下缩没有意义（会露出比原图更小的缩略效果）；
 * - **拖动平移**：仅放大后生效，位移按媒体**布局尺寸**夹取，不会把图拖到只剩一角；
 * - **双击**：1 ↔ 2 倍切换；**双指**：捏合缩放（Pointer Events，触屏与手写笔通吃）；
 * - **滑动**：未放大时左右滑切图、下滑关闭，位移跟手，松手按阈值决定执行还是回弹。
 *
 * 变换写成 `translate3d(x, y, 0) scale(s)`：translate 在缩放**之前**作用，
 * 所以 x/y 是屏幕像素，夹取范围就是 `尺寸 × (s - 1) / 2`；
 * 锚点换算用「内容坐标在缩放前后不变」推出来，不能拿 `getBoundingClientRect()`
 * 当参照——那个矩形本身带着当前位移，越缩放越偏。
 */
import { computed, ref } from 'vue'

export interface UseImageZoomOptions {
  /** 舞台元素：锚点与滑动的参照中心，尺寸恒等于视口，不随变换漂移 */
  container: () => HTMLElement | null
  /** 媒体元素：只用它的**布局尺寸**（transform 不改布局），用于夹取平移范围 */
  media: () => HTMLElement | null
  min?: number
  max?: number
  /** 当前是否允许左右滑切图（只有图片且多于一张） */
  canSwipe?: () => boolean
  /** 当前是否允许下滑关闭 */
  canDismiss?: () => boolean
  onSwipe?: (delta: 1 | -1) => void
  onDismiss?: () => void
  /** 轻点（没有拖动、没有缩放）：灯箱用它切换 chrome 显隐 */
  onTap?: () => void
}

/** 滑动判定的方向死区：小于这个位移算「点了一下」，不算滑 */
const TOUCH_SLOP = 8
/** 左右切图的位移阈值 */
const SWIPE_X = 60
/** 下滑关闭的位移阈值 */
const SWIPE_Y = 110
/** 单指按下的最长时间，超过就当长按（不触发 tap 切换 chrome） */
const TAP_MS = 350

export function useImageZoom(options: UseImageZoomOptions) {
  const min = options.min ?? 1
  const max = options.max ?? 4

  const scale = ref(1)
  const x = ref(0)
  const y = ref(0)
  /** 滑动位移：与 x/y 分开存，松手判定后归零，不影响已经建立的平移状态 */
  const dragX = ref(0)
  const dragY = ref(0)
  /** 拖动中关掉过渡，松手才让回弹/复位走缓动 */
  const moving = ref(false)

  const percent = computed(() => Math.round(scale.value * 100))
  const zoomed = computed(() => scale.value > min + 0.001)

  const style = computed(() => ({
    transform: `translate3d(${x.value + dragX.value}px, ${y.value + dragY.value}px, 0) scale(${scale.value})`,
    transition: moving.value ? 'none' : 'transform 220ms cubic-bezier(0.22, 0.61, 0.36, 1)',
    willChange: 'transform',
  }))

  const pointers = new Map<number, { x: number; y: number }>()
  let startX = 0
  let startY = 0
  let startTX = 0
  let startTY = 0
  let startScale = 1
  let pinchDistance = 0
  let axis: 'x' | 'y' | null = null
  let moved = false
  let downAt = 0

  function clampScale(value: number): number {
    return Math.min(max, Math.max(min, value))
  }

  /** 平移夹取：媒体布局尺寸 × (缩放 - 1) / 2，两个方向都留满 */
  function clampTranslate() {
    const el = options.media()
    const width = el?.offsetWidth ?? 0
    const height = el?.offsetHeight ?? 0
    const limitX = Math.max(0, (width * (scale.value - 1)) / 2)
    const limitY = Math.max(0, (height * (scale.value - 1)) / 2)
    x.value = Math.min(limitX, Math.max(-limitX, x.value))
    y.value = Math.min(limitY, Math.max(-limitY, y.value))
  }

  /**
   * 复位。`instant` 用于「切换条目」：不要让旧画面当着新图的面做一次 220ms 回中动画
   * （视觉上就是一闪），先把过渡关掉等新 transform 上屏再恢复。
   */
  function reset(instant = false) {
    scale.value = min
    x.value = 0
    y.value = 0
    dragX.value = 0
    dragY.value = 0
    pointers.clear()
    axis = null
    if (instant) {
      moving.value = true
      requestAnimationFrame(() => requestAnimationFrame(() => (moving.value = false)))
    } else {
      moving.value = false
    }
  }

  /** 以舞台中心为参照，把 (clientX, clientY) 对应的那一点固定在原地完成缩放 */
  function zoomAt(clientX: number, clientY: number, factor: number) {
    const container = options.container()
    if (!container) return
    const next = clampScale(scale.value * factor)
    if (Math.abs(next - scale.value) < 0.001) return

    const rect = container.getBoundingClientRect()
    const dx = clientX - (rect.left + rect.width / 2)
    const dy = clientY - (rect.top + rect.height / 2)
    const ratio = next / scale.value

    // 内容坐标 c = (d - t) / s，缩放后要求 d 不变 → t' = d - c · s'
    x.value = dx - (dx - x.value) * ratio
    y.value = dy - (dy - y.value) * ratio
    scale.value = next
    if (next <= min + 0.001) {
      x.value = 0
      y.value = 0
    }
    clampTranslate()
  }

  function zoomBy(factor: number) {
    const container = options.container()
    if (!container) return
    const rect = container.getBoundingClientRect()
    zoomAt(rect.left + rect.width / 2, rect.top + rect.height / 2, factor)
  }

  function onWheel(event: WheelEvent) {
    // 没有可滚动的父层（页面已锁滚动），纵向滚轮一律当缩放
    event.preventDefault()
    const factor = event.deltaY < 0 ? 1.12 : 1 / 1.12
    zoomAt(event.clientX, event.clientY, factor)
  }

  function onPointerDown(event: PointerEvent) {
    const container = options.container()
    if (!container) return
    pointers.set(event.pointerId, { x: event.clientX, y: event.clientY })

    if (pointers.size === 1) {
      startX = event.clientX
      startY = event.clientY
      startTX = x.value
      startTY = y.value
      startScale = scale.value
      axis = null
      moved = false
      downAt = Date.now()
      moving.value = true
    } else if (pointers.size === 2) {
      // 双指：记录初始间距与当时的缩放，后续按比例推
      const [a, b] = [...pointers.values()]
      pinchDistance = Math.hypot(a.x - b.x, a.y - b.y)
      startScale = scale.value
      axis = 'x' // 捏合期间不再参与滑动判定
      dragX.value = 0
      dragY.value = 0
    }
  }

  function onPointerMove(event: PointerEvent) {
    if (!pointers.has(event.pointerId)) return
    pointers.set(event.pointerId, { x: event.clientX, y: event.clientY })

    if (pointers.size >= 2) {
      const [a, b] = [...pointers.values()]
      const distance = Math.hypot(a.x - b.x, a.y - b.y)
      if (pinchDistance > 0 && distance > 0) {
        const next = clampScale(startScale * (distance / pinchDistance))
        scale.value = next
        if (next <= min + 0.001) {
          x.value = 0
          y.value = 0
        }
      }
      moved = true
      return
    }

    const dx = event.clientX - startX
    const dy = event.clientY - startY
    if (!moved && Math.hypot(dx, dy) > TOUCH_SLOP) moved = true

    // 放大后拖动 = 平移，优先于滑动
    if (scale.value > min + 0.001) {
      x.value = startTX + dx
      y.value = startTY + dy
      dragX.value = 0
      dragY.value = 0
      clampTranslate()
      return
    }

    if (!moved) return
    if (!axis) axis = Math.abs(dx) > Math.abs(dy) ? 'x' : 'y'
    if (axis === 'x') {
      if (options.canSwipe?.() === false) return
      dragX.value = dx
    } else {
      if (options.canDismiss?.() === false) return
      dragY.value = dy
    }
  }

  function finishPointer(event: PointerEvent) {
    if (!pointers.has(event.pointerId)) return
    pointers.delete(event.pointerId)

    // 捏合松开一根手指：把当前缩放当新基准，避免剩下那根手指把图弹回去
    if (pointers.size === 1) {
      const [rest] = [...pointers.values()]
      startX = rest.x
      startY = rest.y
      startTX = x.value
      startTY = y.value
      startScale = scale.value
      axis = 'x'
      moving.value = true
      return
    }
    if (pointers.size > 1) return

    moving.value = false

    if (!moved && Date.now() - downAt < TAP_MS) {
      options.onTap?.()
      return
    }

    if (axis === 'x' && Math.abs(dragX.value) > SWIPE_X) {
      const delta: 1 | -1 = dragX.value < 0 ? 1 : -1
      dragX.value = 0
      dragY.value = 0
      options.onSwipe?.(delta)
      return
    }
    if (axis === 'y' && dragY.value > SWIPE_Y) {
      dragX.value = 0
      dragY.value = 0
      options.onDismiss?.()
      return
    }

    // 没到阈值：回弹
    dragX.value = 0
    dragY.value = 0
  }

  return {
    scale,
    percent,
    zoomed,
    style,
    reset,
    zoomBy,
    zoomAt,
    onWheel,
    onPointerDown,
    onPointerMove,
    onPointerUp: finishPointer,
    onPointerCancel: finishPointer,
  }
}
