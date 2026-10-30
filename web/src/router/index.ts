import { createRouter, createWebHistory, type RouteRecordRaw } from 'vue-router'
import type { IconName } from '../components/icons'
import { authApi, ensureAuth } from '../api/auth'
import { APP_BASE } from '../lib/base'
import { i18n } from '../lib/i18n'

declare module 'vue-router' {
  interface RouteMeta {
    /** 侧边栏 / 顶栏显示名（i18n key，如 common.nav.files） */
    title: string
    /** 侧边栏图标 */
    icon?: IconName
    /** 顶栏副标题（i18n key） */
    subtitle?: string
    /** 无需登录即可访问 */
    public?: boolean
    /** GalleryView 复用时的媒体分区 */
    mediaTab?: 'photos' | 'cinema' | 'music'
  }
}

const routes: RouteRecordRaw[] = [
  {
    path: '/login',
    name: 'login',
    component: () => import('../views/LoginView.vue'),
    meta: { title: 'common.nav.login', public: true },
  },
  {
    path: '/s/:token',
    name: 'share',
    component: () => import('../views/ShareView.vue'),
    meta: { title: 'common.nav.share', public: true },
  },
  {
    path: '/',
    name: 'portal',
    component: () => import('../views/PortalView.vue'),
    meta: { title: 'common.nav.portal', icon: 'home', subtitle: 'common.subtitle.portal' },
  },
  {
    path: '/files',
    name: 'files',
    component: () => import('../views/FilesView.vue'),
    meta: { title: 'common.nav.files', icon: 'folder', subtitle: 'common.subtitle.files' },
  },
  {
    // 单挂载点必须 router.replace 到这里（设计文档 3.2.1），深链接形如 /files/<id>/a/b
    path: '/files/:mountId/:pathMatch(.*)*',
    name: 'files-mount',
    component: () => import('../views/FilesView.vue'),
    meta: { title: 'common.nav.files', icon: 'folder', subtitle: 'common.subtitle.files' },
  },
  {
    path: '/photos',
    name: 'photos',
    component: () => import('../views/GalleryView.vue'),
    meta: { title: 'common.nav.photos', icon: 'gallery', mediaTab: 'photos', subtitle: 'common.subtitle.photos' },
  },
  {
    path: '/cinema',
    name: 'cinema',
    component: () => import('../views/GalleryView.vue'),
    meta: { title: 'common.nav.cinema', icon: 'film', mediaTab: 'cinema', subtitle: 'common.subtitle.cinema' },
  },
  {
    path: '/music',
    name: 'music',
    component: () => import('../views/GalleryView.vue'),
    meta: { title: 'common.nav.music', icon: 'audio', mediaTab: 'music', subtitle: 'common.subtitle.music' },
  },
  { path: '/gallery', redirect: '/photos' },
  {
    path: '/terminal',
    name: 'terminal',
    component: () => import('../views/TerminalView.vue'),
    meta: { title: 'common.nav.terminal', icon: 'terminal', subtitle: 'common.subtitle.terminal' },
  },
  {
    path: '/shares',
    name: 'shares',
    component: () => import('../views/SharesView.vue'),
    meta: { title: 'common.nav.shares', icon: 'share', subtitle: 'common.subtitle.shares' },
  },
  {
    path: '/settings',
    name: 'settings',
    component: () => import('../views/SettingsView.vue'),
    meta: { title: 'common.nav.settings', icon: 'gear', subtitle: 'common.subtitle.settings' },
  },
  {
    // 生态推荐（v4.4.11 从设置页迁出，入口在侧边栏左下角）
    path: '/ecosystem',
    name: 'ecosystem',
    component: () => import('../views/EcosystemView.vue'),
    meta: { title: 'common.nav.ecosystem', icon: 'sparkle' },
  },
  { path: '/:pathMatch(.*)*', redirect: '/' },
]

export const router = createRouter({
  // History 模式：后端对非 /api、非 /proxy 路径回落到 index.html；
  // 子路径部署时用注入的前缀（如 /asmoday），保证刷新/深链不丢前缀
  history: createWebHistory(APP_BASE || '/'),
  routes,
  scrollBehavior: () => ({ top: 0 }),
})

// 全局登录守卫：未认证一律回登录页；已认证访问 /login 直接进主页
router.beforeEach(async (to) => {
  await ensureAuth()

  const authenticated = authApi.status.value.authenticated

  if (to.meta.public) {
    if (authenticated && to.path === '/login') return '/'
    return true
  }

  if (!authenticated) {
    return {
      path: '/login',
      query: to.fullPath && to.fullPath !== '/' ? { redirect: to.fullPath } : {},
    }
  }

  return true
})

router.afterEach((to) => {
  const title = to.meta.title ? i18n.global.t(to.meta.title) : ''
  document.title = title ? `${title} · Asmoday` : 'Asmoday'
})

export default router
