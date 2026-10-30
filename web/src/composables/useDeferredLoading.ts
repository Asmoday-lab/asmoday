import { onScopeDispose, ref, watch, type Ref } from 'vue'

/**
 * 把「加载中」延后一小段再置为 true。
 *
 * 接口 100~200ms 就返回时，直接渲染骨架会闪一下又消失——比一直空白更刺眼。
 * 约定：延迟窗口内出结果就完全不显示骨架；超过窗口才显示，并且此后不再闪回。
 */
export function useDeferredLoading(source: Ref<boolean>, delay = 180) {
  const deferred = ref(false)
  let timer: number | null = null

  const clear = () => {
    if (timer !== null) {
      clearTimeout(timer)
      timer = null
    }
  }

  watch(
    source,
    (value) => {
      if (!value) {
        clear()
        deferred.value = false
        return
      }
      if (deferred.value) return
      clear()
      timer = window.setTimeout(() => {
        timer = null
        deferred.value = true
      }, delay)
    },
    { immediate: true },
  )

  onScopeDispose(clear)
  return deferred
}
