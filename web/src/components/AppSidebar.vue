<script setup lang="ts">
/**
 * 全局侧边栏：PC 端固定 240px，可折叠为图标栏（w-60 ↔ w-16），移动端为抽屉。
 * 分组对齐原型：工作区 / 媒体库（照片·影视·音乐）/ 系统（分享与图床·设置）。
 * 主题切换入口**不在**这里（设计文档 8.2：禁止侧边栏底部孤立图标），统一收在顶栏账号菜单。
 */
import { computed, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { RouterLink, useRoute, useRouter } from 'vue-router'
import AppIcon from './AppIcon.vue'
import type { IconName } from './icons'
import { authApi } from '../api/auth'

const props = defineProps<{
  open: boolean
}>()

const emit = defineEmits<{ (e: 'close'): void }>()

const { t } = useI18n()
const router = useRouter()

interface NavItem {
  to: string
  /** i18n key（设计文档 1.4：界面文本一律走 $t） */
  label: string
  icon: IconName
}

interface NavGroup {
  title: string
  items: NavItem[]
}

const groups: NavGroup[] = [
  {
    title: 'common.group.workspace',
    items: [
      { to: '/', label: 'common.nav.portal', icon: 'home' },
      { to: '/files', label: 'common.nav.files', icon: 'folder' },
      { to: '/terminal', label: 'common.nav.terminal', icon: 'terminal' },
    ],
  },
  {
    title: 'common.group.media',
    items: [
      { to: '/photos', label: 'common.nav.photos', icon: 'gallery' },
      { to: '/cinema', label: 'common.nav.cinema', icon: 'film' },
      { to: '/music', label: 'common.nav.music', icon: 'audio' },
    ],
  },
  {
    title: 'common.group.system',
    items: [
      { to: '/shares', label: 'common.nav.shares', icon: 'share' },
      { to: '/settings', label: 'common.nav.settings', icon: 'gear' },
    ],
  },
]

const route = useRoute()
const isActive = (to: string) => (to === '/' ? route.path === '/' : route.path.startsWith(to))

// 桌面折叠（移动端抽屉不受影响）
const COLLAPSE_KEY = 'asmoday.sidebar.collapsed'
const collapsed = ref(localStorage.getItem(COLLAPSE_KEY) === '1')
watch(collapsed, (value) => {
  localStorage.setItem(COLLAPSE_KEY, value ? '1' : '0')
})

const asideClass = computed(() => [
  'fixed inset-y-0 left-0 z-50 flex shrink-0 flex-col',
  'border-r border-line bg-panel',
  'transition-[transform,width] duration-300 ease-out will-change-transform',
  'md:static md:z-auto md:translate-x-0',
  collapsed.value ? 'md:w-16' : 'md:w-60',
  props.open ? 'translate-x-0 w-64 shadow-2xl' : '-translate-x-full w-64',
])

const itemClass = (to: string) => [
  'flex items-center rounded-lg px-3 py-2.5 text-sm transition-colors',
  collapsed.value ? 'md:justify-center md:px-0' : '',
  isActive(to)
    ? 'bg-accent-soft font-medium text-accent'
    : 'text-muted hover:bg-elevated hover:text-ink',
]
</script>

<template>
  <aside :class="asideClass">
    <!-- 品牌 + 折叠按钮 -->
    <div
      class="flex h-16 shrink-0 cursor-pointer items-center border-b border-line px-4"
      :class="collapsed ? 'md:justify-center md:px-0' : ''"
      @click="router.push('/')"
    >
      <span
        class="grid size-8 shrink-0 place-items-center rounded-lg bg-[linear-gradient(135deg,#818cf8,#4f46e5)] text-white shadow-md"
      >
        <AppIcon name="sparkle" :size="32" :stroke-width="1.75" />
      </span>
      <span v-if="!collapsed" class="ml-3 truncate text-lg font-bold tracking-wide text-ink">
        Asmoday
      </span>

      <button
        v-if="!collapsed"
        class="ml-auto hidden text-muted transition-colors hover:text-ink md:block"
        :title="t('common.sidebar.collapse')"
        @click.stop="collapsed = true"
      >
        <AppIcon name="collapse" :size="15" />
      </button>
      <button
        class="ml-auto text-muted transition-colors hover:text-ink md:hidden"
        :aria-label="t('common.sidebar.closeMenu')"
        @click.stop="emit('close')"
      >
        <AppIcon name="close" :size="18" />
      </button>
    </div>

    <!-- 折叠态的展开按钮（桌面） -->
    <button
      v-if="collapsed"
      class="absolute top-[68px] right-0 z-10 hidden translate-x-1/2 rounded-full border border-line bg-panel p-1 text-muted shadow transition-colors hover:text-ink md:block"
      :title="t('common.sidebar.expand')"
      @click="collapsed = false"
    >
      <AppIcon name="expand" :size="13" />
    </button>

    <!-- 导航 -->
    <nav
      class="flex-1 space-y-1 overflow-y-auto px-2 py-4"
      :class="collapsed ? 'md:px-1.5' : ''"
    >
      <template v-for="(group, gi) in groups" :key="group.title">
        <p
          v-if="!collapsed"
          class="px-2 pb-1.5 text-xs font-bold text-muted"
          :class="gi === 0 ? '' : 'mt-4'"
        >
          {{ t(group.title) }}
        </p>
        <div v-else class="mx-2 my-3 border-t border-line" />

        <RouterLink
          v-for="item in group.items"
          :key="item.to"
          :to="item.to"
          :class="itemClass(item.to)"
          :title="collapsed ? t(item.label) : undefined"
          @click="emit('close')"
        >
          <AppIcon :name="item.icon" :size="17" class="w-6 shrink-0 text-center" />
          <span v-if="!collapsed" class="ml-2 truncate">{{ t(item.label) }}</span>
        </RouterLink>
      </template>
    </nav>

    <!-- 左下角：生态推荐入口 + 版本 + 署名（v4.4.11，从设置页迁出） -->
    <div
      class="shrink-0 border-t border-line px-4 py-3"
      :class="collapsed ? 'md:grid md:place-items-center md:px-0' : ''"
    >
      <RouterLink
        to="/ecosystem"
        class="flex items-center gap-1.5 rounded text-[12px] transition-colors"
        :class="route.path === '/ecosystem' ? 'text-accent' : 'text-muted hover:text-accent'"
        :title="collapsed ? t('common.nav.ecosystem') : undefined"
        @click="emit('close')"
      >
        <AppIcon name="sparkle" :size="12" />
        <span v-if="!collapsed">{{ t('common.nav.ecosystem') }}</span>
      </RouterLink>
      <p v-if="!collapsed" class="mt-1 text-[11px] text-muted/70">
        v{{ authApi.status.value.version ?? 'dev' }} · {{ t('common.sidebar.poweredBy') }}
      </p>
    </div>
  </aside>
</template>
