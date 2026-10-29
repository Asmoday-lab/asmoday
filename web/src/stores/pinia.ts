/**
 * Pinia 实例（设计文档 8.x：全局状态统一收口）。
 *
 * 单独一个文件是因为 store 在**组件外**也要能用：路由守卫、API 层、`index.html`
 * 之后的首屏逻辑都会读写状态。Pinia 的 `useStore()` 默认依赖「当前活跃实例」，
 * 组件外的调用时机不确定；显式传实例（`usePlayerStore(pinia)`）就没有这个问题，
 * 也不需要 `setActivePinia` 这种全局副作用。
 *
 * 约定：
 * - 新状态一律写成 `defineStore('名字', () => { ... })`（setup 语法）；
 * - 组件里正常 `const store = useXxxStore()`；
 * - 需要在模块顶层直接用（例如给 `App.vue` 提供具名导出）时，在 store 文件末尾
 *   `useXxxStore(pinia)` 取一次实例，再用 `toRef` 把字段转回 ref 导出。
 */
import { createPinia } from 'pinia'

export const pinia = createPinia()
