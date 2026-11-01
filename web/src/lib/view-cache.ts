/**
 * 会话级「上一帧」缓存（v4.6.8）。
 *
 * 只解决一件事：再次进入文件管理 / 媒体库时，先把上次画出来的列表贴回屏幕，
 * 然后再用后端返回的数据覆盖。没有它，每次进页面都要先闪一下骨架屏 ——
 * 即使接口只要 200ms，那一闪也很刺眼；而目录扫描一旦慢下来，用户盯着的
 * 就是一整屏灰块。
 *
 * 三条纪律：
 * 1. **绝不参与正确性判断**。每个页面进来照旧全量重新拉取（设计文档 3.2.1：
 *    进入必须重新拉取，禁止拿旧数据当答案）。缓存只决定「第一帧画什么」。
 * 2. **只活在当前标签页内存里**。刷新、关标签页、切换会话都会自然清空，
 *    不存在「看到的是昨天目录」的隐患。
 * 3. **只存列表快照**，不存选中态、搜索态、弹窗态 —— 那些必须每次重新建立。
 */

const store = new Map<string, unknown>()

/** 上限只防内存无节制增长（一个深层目录换一个键），淘汰最旧的 */
const MAX_ENTRIES = 48

export function readCache<T>(key: string): T | undefined {
  return store.get(key) as T | undefined
}

export function writeCache(key: string, value: unknown): void {
  // 重新插入到队尾，让「最近用过的」活得更久
  store.delete(key)
  store.set(key, value)
  while (store.size > MAX_ENTRIES) {
    const oldest = store.keys().next().value
    if (oldest === undefined) break
    store.delete(oldest)
  }
}
