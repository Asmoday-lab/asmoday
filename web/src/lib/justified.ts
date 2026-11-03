/**
 * 等高行布局（justified rows，v4.6.19）。
 *
 * 灵感来自 Immich 的时间线：每个日期一行一行铺，同一行里每张图按**自己的宽高比**
 * 占宽度，行高由「这一行排满了容器宽度」反推，于是横图竖图都完整、不裁不拉伸，
 * 也不像方格网格那样把竖图裁成方图。
 *
 * 算法就是最朴素的贪心装行：
 * 1. 先按目标行高把图按比例换算成宽度，累加；
 * 2. 累加到「如果按容器宽度等比缩放，行高 <= 目标行高」时，这一行就满了；
 * 3. 整行按容器宽度精确缩放（最后一像素的舍入误差摊给最宽的格子）；
 * 4. 末行装不满就不拉伸——保持目标行高、左对齐，否则一两张图会被放成巨幅。
 *
 * 纯函数、不碰 DOM，宽度由调用方测量（ResizeObserver）后传进来。
 */

export interface JustifiedCell<T> {
  item: T
  /** 格子宽度（px） */
  width: number
  /** 格子高度（px），同一行内相等 */
  height: number
}

export interface JustifiedRow<T> {
  cells: JustifiedCell<T>[]
  height: number
}

export interface JustifiedOptions {
  /** 目标行高；实际行高会在它附近浮动以保证行宽刚好等于容器宽 */
  targetHeight?: number
  /** 格子间距（px） */
  gap?: number
}

/** 宽高比的有效区间：极端全景/长图不参与「拉宽」，避免一行只剩一张 */
const MIN_RATIO = 0.35
const MAX_RATIO = 3.4

/** 把任意输入收敛成一个可用的宽高比（拿不到尺寸时按 4:3 铺） */
export function normalizeRatio(value: number | null | undefined): number {
  if (!value || !Number.isFinite(value) || value <= 0) return 4 / 3
  return Math.min(MAX_RATIO, Math.max(MIN_RATIO, value))
}

export function justifiedRows<T>(
  items: readonly T[],
  ratioOf: (item: T) => number,
  containerWidth: number,
  options: JustifiedOptions = {},
): JustifiedRow<T>[] {
  const targetHeight = options.targetHeight ?? 190
  const gap = options.gap ?? 3
  if (!Number.isFinite(containerWidth) || containerWidth <= 0 || items.length === 0) return []

  const rows: JustifiedRow<T>[] = []
  let pending: Array<{ item: T; ratio: number }> = []
  let ratioSum = 0

  const flush = (full: boolean) => {
    if (pending.length === 0) return
    const available = Math.max(1, containerWidth - gap * (pending.length - 1))
    // 装满的行按容器宽度精确铺满；末行不放大，保持目标行高
    const height = full ? available / ratioSum : Math.min(targetHeight, available / ratioSum)
    const widths = pending.map((cell) => cell.ratio * height)
    // 浮点累加会差出零点几像素，把误差补给最宽的格子，行右边缘才不会留缝
    if (full) {
      const total = widths.reduce((sum, width) => sum + width, 0)
      const widest = widths.indexOf(Math.max(...widths))
      if (widest >= 0) widths[widest] += available - total
    }
    rows.push({
      cells: pending.map((cell, index) => ({
        item: cell.item,
        width: widths[index] ?? 1,
        height,
      })),
      height,
    })
    pending = []
    ratioSum = 0
  }

  for (const item of items) {
    const ratio = normalizeRatio(ratioOf(item))
    pending.push({ item, ratio })
    ratioSum += ratio
    const available = Math.max(1, containerWidth - gap * (pending.length - 1))
    if (available / ratioSum <= targetHeight) flush(true)
  }
  flush(false)
  return rows
}
