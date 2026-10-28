<script setup lang="ts">
import AppIcon from './AppIcon.vue'
import type { StagePoint } from './stage'

withDefaults(
  defineProps<{
    stage: string
    title: string
    description: string
    points?: StagePoint[]
  }>(),
  { points: () => [] },
)
</script>

<template>
  <section class="mx-auto w-full max-w-3xl px-4 py-10 md:px-8 md:py-14">
    <div class="glass-card rounded-2xl p-6 md:p-8">
      <div class="mb-5 flex items-start gap-4">
        <span
          class="grid size-11 shrink-0 place-items-center rounded-xl bg-accent-soft text-accent"
        >
          <AppIcon name="chip" :size="20" />
        </span>
        <div class="min-w-0">
          <p
            class="mb-1 inline-flex items-center rounded-full border border-line px-2 py-0.5 text-[11px] font-medium tracking-wider text-muted uppercase"
          >
            {{ stage }}
          </p>
          <h1 class="text-xl font-semibold tracking-tight text-ink md:text-2xl">{{ title }}</h1>
          <p class="mt-1 text-sm text-muted">{{ description }}</p>
        </div>
      </div>

      <div v-if="points.length" class="grid gap-2.5 sm:grid-cols-2">
        <div
          v-for="point in points"
          :key="point.title"
          class="rounded-xl border border-line bg-elevated/40 p-3.5"
        >
          <p class="mb-1 text-[14px] font-medium text-ink">{{ point.title }}</p>
          <p class="text-xs leading-relaxed text-muted">{{ point.detail }}</p>
        </div>
      </div>

      <div class="mt-6 flex items-center gap-2 text-xs text-muted">
        <span class="skeleton h-1.5 w-24 rounded-full" />
        <span>界面骨架已就绪，业务逻辑按阶段接入</span>
      </div>
    </div>
  </section>
</template>
