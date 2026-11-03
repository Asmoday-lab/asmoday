/**
 * 年份刻度尺（v4.6.19）。
 *
 * Immich 时间线右边缘那条竖排年份：滚动时高亮「当前所在年份」，点一下跳过去。
 * 照片库与影视库是两条独立的滚动流，逻辑一模一样，所以抽成一个组合式函数，
 * 各自给一个容器 ref 与分组 computed 即可。
 *
 * 依赖约定：分组本身必须是「新 → 旧」顺序，且按时间分组时带 `year`；
 * 按文件夹分组没有年份，`years` 自然为空，刻度尺自己隐藏。
 */
import { computed, onBeforeUnmount, ref, type ComputedRef, type Ref } from 'vue'

export interface RailGroup {
  key: string
  year: number | null
}

export function useYearRail<G extends RailGroup>(
  container: Ref<HTMLElement | null>,
  groups: ComputedRef<G[]>,
) {
  /** 去重后的年份（降序，与分组顺序一致） */
  const years = computed(() => {
    const seen: number[] = []
    for (const group of groups.value) {
      if (group.year && !seen.includes(group.year)) seen.push(group.year)
    }
    return seen
  })

  const active = ref<number | null>(null)
  const elements = new Map<string, HTMLElement>()
  let raf = 0

  function setEl(key: string, el: unknown) {
    if (el instanceof HTMLElement) elements.set(key, el)
    else elements.delete(key)
  }

  /** 当前年份 = 最后一个「标题已滚进吸顶带」的分组所属年份 */
  function update() {
    const root = container.value
    if (!root || years.value.length === 0) return
    const rootTop = root.getBoundingClientRect().top
    let current: number | null = years.value[0] ?? null
    for (const group of groups.value) {
      if (!group.year) continue
      const el = elements.get(group.key)
      if (!el) continue
      if (el.getBoundingClientRect().top - rootTop <= 56) current = group.year
    }
    active.value = current
  }

  function onScroll() {
    if (raf) return
    raf = requestAnimationFrame(() => {
      raf = 0
      update()
    })
  }

  function jump(year: number) {
    const root = container.value
    if (!root) return
    const target = groups.value.find((group) => group.year === year)
    const el = target ? elements.get(target.key) : null
    if (!el) return
    // 44px ≈ 吸顶标题的高度，跳过去后标题正好贴在顶边而不是被压在下面
    const top = el.getBoundingClientRect().top - root.getBoundingClientRect().top + root.scrollTop - 44
    root.scrollTo({ top: Math.max(0, top), behavior: 'smooth' })
    active.value = year
  }

  onBeforeUnmount(() => {
    if (raf) cancelAnimationFrame(raf)
  })

  return { years, active, setEl, onScroll, jump, update }
}
