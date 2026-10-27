<script setup lang="ts">
/**
 * 聚合主页：核心入口 + 由 config.toml 的 [[services]] 驱动的外部服务卡片。
 * 服务卡片走内置反代 /proxy/{id}/，iframe 内嵌（后端已剥离 X-Frame-Options）。
 */
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { RouterLink } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import type { IconName } from '../components/icons'
import { authApi } from '../api/auth'

interface Entry {
  to: string
  /** i18n key 前缀（如 portal.entries.files） */
  key: string
  icon: IconName
  tone: string
  /** 标题与箭头 hover 时的颜色（完整类名，Tailwind 需静态可扫描） */
  hoverTone: string
}

interface ServiceCard {
  id: string
  name: string
  icon?: string
  target_url: string
}

const entries: Entry[] = [
  {
    to: '/files',
    key: 'portal.entries.files',
    icon: 'folder',
    tone: 'bg-accent-soft text-accent',
    hoverTone: 'group-hover:text-accent',
  },
  {
    to: '/photos',
    key: 'portal.entries.photos',
    icon: 'gallery',
    tone: 'bg-blue-500/15 text-blue-500',
    hoverTone: 'group-hover:text-blue-500',
  },
  {
    to: '/terminal',
    key: 'portal.entries.terminal',
    icon: 'terminal',
    tone: 'bg-emerald-500/15 text-emerald-500',
    hoverTone: 'group-hover:text-emerald-500',
  },
]

const { t } = useI18n()
const services = ref<ServiceCard[]>([])
const active = ref<ServiceCard | null>(null)
const frameKey = ref(0)

const activeUrl = computed(() => (active.value ? `/proxy/${active.value.id}/` : ''))

function openService(service: ServiceCard) {
  frameKey.value += 1
  active.value = service
}

onMounted(async () => {
  try {
    const res = await fetch('/api/config')
    if (!res.ok) return
    const config = (await res.json()) as { services?: ServiceCard[] }
    services.value = config.services ?? []
  } catch {
    /* 后端未就绪时静默降级 */
  }
})
</script>

<template>
  <section class="relative mx-auto w-full max-w-3xl px-4 py-10 md:px-8 md:py-16">
    <div
      class="pointer-events-none absolute top-0 left-1/3 size-72 rounded-full bg-accent/20 blur-[110px]"
    />

    <div class="relative">
      <p class="mb-2 text-xs font-medium tracking-[0.2em] text-accent uppercase">Asmoday</p>
      <h1 class="text-2xl font-semibold tracking-tight text-ink md:text-4xl">
        {{ t('portal.greeting', { name: authApi.username.value ?? t('portal.guest') }) }}
      </h1>
      <p class="mt-2 text-sm text-muted md:text-base">
        {{ t('portal.status') }}
      </p>

      <div class="mt-8 space-y-3">
        <RouterLink
          v-for="entry in entries"
          :key="entry.to"
          :to="entry.to"
          class="glass-card group flex items-center gap-4 rounded-2xl px-4 py-4 md:px-5"
        >
          <span
            class="grid size-11 shrink-0 place-items-center rounded-xl md:size-12"
            :class="entry.tone"
          >
            <AppIcon :name="entry.icon" :size="21" />
          </span>
          <span class="min-w-0 flex-1">
            <span
              class="block text-sm font-semibold text-ink transition-colors md:text-base"
              :class="entry.hoverTone"
            >
              {{ t(`${entry.key}.title`) }}
            </span>
            <span class="block truncate text-xs text-muted md:text-sm">{{ t(`${entry.key}.desc`) }}</span>
          </span>
          <span
            class="hidden text-muted transition-colors md:block"
            :class="entry.hoverTone"
          >
            <AppIcon name="arrowRight" :size="16" />
          </span>
        </RouterLink>
      </div>

      <!-- 外部服务 -->
      <div class="mt-10">
        <div class="mb-3 flex items-center justify-between">
          <h2 class="text-xs font-semibold tracking-[0.16em] text-muted uppercase">
            {{ t('portal.services.title') }}
          </h2>
          <span class="text-[11px] text-muted">
            {{ t('portal.services.count', { count: services.length }) }}
          </span>
        </div>

        <div v-if="services.length === 0" class="glass-card rounded-2xl px-4 py-5 text-xs text-muted">
          {{ t('portal.services.empty') }}
        </div>

        <div v-else class="grid gap-3 sm:grid-cols-2">
          <button
            v-for="service in services"
            :key="service.id"
            class="glass-card group flex items-center gap-3 rounded-2xl px-4 py-3.5 text-left"
            @click="openService(service)"
          >
            <span
              class="grid size-10 shrink-0 place-items-center rounded-xl bg-elevated text-muted group-hover:text-accent"
            >
              <AppIcon name="grid" :size="18" />
            </span>
            <span class="min-w-0 flex-1">
              <span class="block truncate text-sm font-medium text-ink">{{ service.name }}</span>
              <span class="block truncate text-[11px] text-muted">{{ service.target_url }}</span>
            </span>
            <span class="text-muted opacity-0 transition-opacity group-hover:opacity-100">
              <AppIcon name="arrowRight" :size="15" />
            </span>
          </button>
        </div>
      </div>

      <p class="mt-8 flex items-center gap-2 text-xs text-muted">
        <AppIcon name="sparkle" :size="14" />
        {{ t('portal.services.note') }}
      </p>
    </div>

    <!-- 服务内嵌窗口 -->
    <div
      v-if="active"
      class="fixed inset-0 z-50 flex flex-col bg-black/90 p-2 md:p-6"
      @click.self="active = null"
    >
      <div class="mb-2 flex items-center gap-3 px-1 text-white">
        <span class="text-sm font-medium">{{ active.name }}</span>
        <span class="truncate text-[11px] text-white/50">{{ activeUrl }}</span>
        <a
          :href="activeUrl"
          target="_blank"
          rel="noopener"
          class="ml-auto rounded bg-white/10 px-2 py-1 text-[11px] hover:bg-white/20"
        >
          {{ t('portal.services.openNewWindow') }}
        </a>
        <button class="rounded bg-white/10 px-2 py-1 text-[11px] hover:bg-white/20" @click="active = null">
          {{ t('portal.services.close') }}
        </button>
      </div>
      <iframe
        :key="frameKey"
        :src="activeUrl"
        class="min-h-0 w-full flex-1 rounded-lg border border-white/10 bg-white"
        referrerpolicy="no-referrer"
        sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-downloads"
      />
    </div>
  </section>
</template>
