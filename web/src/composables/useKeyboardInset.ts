import { onBeforeUnmount, onMounted, ref, type Ref } from 'vue'

/**
 * 软键盘遮挡高度（px，0 = 没被遮挡）。
 *
 * 移动端软键盘只缩 `visualViewport`：布局视口、`100dvh`、父容器高度全都不变，
 * 因此贴着容器底边的元素（终端快捷键条、状态栏）会被键盘盖掉半截。
 * 这里持续测量「元素底边超出可视区下沿多少像素」，调用方转成 `padding-bottom`
 * 把整块内容顶到键盘上方（padding 在 border-box 里，内容盒随之变矮，
 * 子元素的 `flex-1` 会自动让位）。
 *
 * @param target 需要跟键盘一起让位的元素
 * @param threshold 低于该值视为「Safari 地址栏收放」而不是键盘（软键盘通常 ≥ 200px）
 */
export function useKeyboardInset(target: Ref<HTMLElement | null>, threshold = 100) {
  const inset = ref(0)
  let frame = 0
  let observer: ResizeObserver | null = null

  function measure() {
    frame = 0
    const el = target.value
    const viewport = window.visualViewport
    if (!el || !viewport) {
      inset.value = 0
      return
    }
    // 双指缩放同样会缩 visualViewport，但那不是键盘：不动，避免整页跟着乱跳
    if (Math.abs(viewport.scale - 1) > 0.02) {
      inset.value = 0
      return
    }
    const rect = el.getBoundingClientRect()
    const hidden = Math.round(rect.bottom - (viewport.offsetTop + viewport.height))
    inset.value = hidden > threshold ? hidden : 0
  }

  function schedule() {
    if (frame) return
    frame = requestAnimationFrame(measure)
  }

  onMounted(() => {
    const viewport = window.visualViewport
    // 键盘弹出/收起走 resize；iOS 还会同时平移可视区，必须一起听 scroll
    viewport?.addEventListener('resize', schedule)
    viewport?.addEventListener('scroll', schedule)
    window.addEventListener('resize', schedule)
    window.addEventListener('orientationchange', schedule)
    if (target.value) {
      observer = new ResizeObserver(schedule)
      observer.observe(target.value)
    }
    schedule()
  })

  onBeforeUnmount(() => {
    if (frame) cancelAnimationFrame(frame)
    frame = 0
    const viewport = window.visualViewport
    viewport?.removeEventListener('resize', schedule)
    viewport?.removeEventListener('scroll', schedule)
    window.removeEventListener('resize', schedule)
    window.removeEventListener('orientationchange', schedule)
    observer?.disconnect()
    observer = null
  })

  return inset
}
