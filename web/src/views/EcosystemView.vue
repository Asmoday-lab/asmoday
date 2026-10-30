<script setup lang="ts">
/**
 * 生态推荐（设计文档 10）：只读清单，独立页面（v4.4.11 从设置页迁出）。
 * 纯前端常量 + i18n 文案，断网可用；外链统一 target=_blank + rel=noopener。
 */
import { useI18n } from 'vue-i18n'
import AppIcon from '../components/AppIcon.vue'
import { ECOSYSTEM } from '../constants/ecosystem'

const { t } = useI18n()
</script>

<template>
  <section class="mx-auto w-full max-w-5xl px-4 py-6 md:px-8 md:py-10">
    <div class="mb-6">
      <h1 class="text-lg font-semibold text-ink">{{ t('ecosystem.title') }}</h1>
      <p class="mt-1 text-[13px] leading-relaxed text-muted">{{ t('ecosystem.intro') }}</p>
    </div>

    <div class="space-y-6">
      <div v-for="category in ECOSYSTEM" :key="category.id">
        <p class="mb-2 text-[12px] font-semibold tracking-wider text-muted uppercase">
          {{ t(`ecosystem.categories.${category.id}`) }}
        </p>
        <div class="grid gap-2 sm:grid-cols-2">
          <a
            v-for="item in category.items"
            :key="item.id"
            :href="item.url"
            target="_blank"
            rel="noopener noreferrer"
            class="glass-card flex items-start gap-3 rounded-xl px-4 py-3"
          >
            <span class="mt-0.5 grid size-9 shrink-0 place-items-center rounded-lg bg-accent-soft text-accent">
              <AppIcon :name="item.icon" :size="16" />
            </span>
            <span class="min-w-0 flex-1">
              <span class="block truncate text-sm font-medium text-ink">{{ item.name }}</span>
              <span class="mt-0.5 block text-[12px] leading-relaxed text-muted">
                {{ t(`ecosystem.items.${item.id}`) }}
              </span>
            </span>
            <AppIcon name="arrowRight" :size="14" class="mt-1 shrink-0 text-muted" />
          </a>
        </div>
      </div>
    </div>

    <p class="mt-6 rounded-xl border border-line px-4 py-3 text-[12px] leading-relaxed text-muted">
      {{ t('ecosystem.footer') }}
    </p>
  </section>
</template>
