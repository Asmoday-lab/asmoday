/**
 * 缩略图装载调度（v4.6.22）。
 *
 * 为什么不用 `<img loading="lazy">`：浏览器给每个 img 独立发请求，谁先回来谁先亮。
 * 服务端缩略图是**按需生成**的（解码有并发上限，超时打回 503），所以首屏几十个
 * 并行请求一拥而上时，完成顺序基本随机 —— 观感就是「东一块西一块亮起来」。
 *
 * 这里的策略：
 * 1. **只等快到视口才请求**（IntersectionObserver，提前 600px 预热），不预载整页；
 * 2. **限量并发 + 按渲染顺序排队**（DOM 顺序 = 行序），前排先出图；
 * 3. **失败重试**：服务端拥塞回 503，退避后再排一次队，用尽才落图标；
 * 4. **会话级缓存（v4.6.22）**：加载成功过的 URL 记进模块级集合，组件重挂
 *    （切视图、进出相册、分页续排、view-cache 恢复上一帧）直接同步点亮，
 *    不再排队重新请求 —— 以前每次挂载都从零排队，观感就是「毫无缓存」。
 *    HTTP 缓存（immutable）管跨会话，这里管会话内的重复挂载。
 * 5. **强引用防中止（v4.6.22）**：预载的 `new Image()` 进 in-flight 集合持引用，
 *    完成后才释放 —— 以前加载中的 Image 没有任何引用，引擎回收时会连请求一起
 *    掐掉（DevTools 里一排「已终止 0 B」、控制台刷 net::ERR_ABORTED 就是它），
 *    被掐的响应进不了缓存，下回还得从头再来。
 */

import { onBeforeUnmount, onMounted, ref, type Ref } from 'vue'

/** 同时进行的缩略图请求数：服务端解码吃 CPU，开太大只会互相拖慢 */
const CONCURRENCY = 6
/** 预热距离：滚到还有一屏时就开始取，滚动时不至于看白块 */
const NEAR_MARGIN = '600px 0px'
/** 重试退避（毫秒）：依次等这么久再排队，用尽即认定真的取不到 */
const RETRY_DELAYS = [250, 800, 1800]

/** 本会话加载成功过的缩略图 URL：重挂时直接命中，不走队列 */
const loadedUrls = new Set<string>()
/** 加载中的 Image 元素：持强引用，防止被 GC 连请求一起掐掉 */
const inflight = new Set<HTMLImageElement>()

interface Queued {
  order: number
  start: () => void
}

const queue: Queued[] = []
let running = 0
let nextOrder = 0

function pump() {
  while (running < CONCURRENCY && queue.length > 0) {
    const job = queue.shift()
    if (!job) return
    running += 1
    job.start()
  }
}

function release() {
  running -= 1
  pump()
}

/** 按渲染顺序插队：DOM 在前的先加载，滚动追加的排在后面 */
function push(order: number, start: () => void) {
  queue.push({ order, start })
  queue.sort((a, b) => a.order - b.order)
  pump()
}

function fetchOnce(url: string, attempt: number, order: number, done: (ok: boolean) => void) {
  const image = new Image()
  image.decoding = 'async'
  inflight.add(image)
  image.onload = () => {
    inflight.delete(image)
    loadedUrls.add(url)
    release()
    done(true)
  }
  image.onerror = () => {
    inflight.delete(image)
    release()
    if (attempt >= RETRY_DELAYS.length) {
      done(false)
      return
    }
    // 退避后重新排队（走同一个并发闸门，重试也不会把服务端打满）
    window.setTimeout(() => push(order, () => fetchOnce(url, attempt + 1, order, done)), RETRY_DELAYS[attempt])
  }
  image.src = url
}

/** 共享一个 IntersectionObserver：一千个格子就一千个实例太贵 */
const nearCallbacks = new WeakMap<Element, () => void>()
let nearObserver: IntersectionObserver | null = null

function ensureObserver(): IntersectionObserver | null {
  if (typeof IntersectionObserver === 'undefined') return null
  if (!nearObserver) {
    nearObserver = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (!entry.isIntersecting) continue
          const callback = nearCallbacks.get(entry.target)
          if (!callback) continue
          nearCallbacks.delete(entry.target)
          nearObserver?.unobserve(entry.target)
          callback()
        }
      },
      { rootMargin: NEAR_MARGIN },
    )
  }
  return nearObserver
}

/**
 * 给一个格子挂上「滚到附近才取图」的逻辑。
 *
 * `el` 是观测用的根节点，`url` 返回要取的缩略图地址。
 * 返回 `{ src, failed }`：`src` 为空时保持 ThumbHash 模糊打底，
 * 加载成功后才赋给 `<img>`，失败（含重试用尽）由 `failed` 切图标。
 * URL 本会话加载成功过 → 跳过观测与排队，挂载即点亮。
 */
export function useThumbSrc(el: Ref<HTMLElement | null>, url: () => string) {
  const src = ref('')
  const failed = ref(false)
  const order = ++nextOrder
  let alive = true
  let stopWatching: (() => void) | null = null

  function start() {
    const target = url()
    if (!target) return
    if (loadedUrls.has(target)) {
      // 会话内已经拿过这张图：同步赋 src（浏览器内存缓存瞬时出图）
      src.value = target
      return
    }
    push(order, () => {
      if (!alive) {
        release()
        return
      }
      fetchOnce(target, 0, order, (ok) => {
        if (!alive) return
        if (ok) src.value = target
        else failed.value = true
      })
    })
  }

  onMounted(() => {
    const node = el.value
    const observer = node ? ensureObserver() : null
    if (!node || !observer) {
      // 老浏览器 / 没拿到节点：退化成「立刻加载」，不因为优化把图卡住
      start()
      return
    }
    // 先查会话缓存再决定要不要观测：命中就不用等 IntersectionObserver 那一拍
    const target = url()
    if (target && loadedUrls.has(target)) {
      src.value = target
      return
    }
    nearCallbacks.set(node, start)
    observer.observe(node)
    stopWatching = () => {
      nearCallbacks.delete(node)
      observer.unobserve(node)
    }
  })

  onBeforeUnmount(() => {
    alive = false
    stopWatching?.()
  })

  return { src, failed }
}
