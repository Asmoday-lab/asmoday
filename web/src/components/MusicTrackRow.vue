<script setup lang="ts">
/**
 * 音乐库的一行曲目（v4.9.5）。
 *
 * 以前是 `<table>` 里的一行：序号、标题、**文件大小**、心。大小对听歌的人没有意义，
 * 这里换成时长；行号在悬停时变成播放键，当前曲目用强调色 + 跳动条 —— 这几处
 * 是 Navidrome / Spotify 那类界面里最省事的「我现在在听哪首」的交代。
 */
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import { coverUrl, placeholderColor, type MediaItem } from '../api/media'
import { formatDuration } from '../lib/format'

const { t } = useI18n()

const props = defineProps<{
  item: MediaItem
  index: number
  /** 是不是当前播放的这首 */
  active: boolean
  /** 当前这首是否正在放（暂停时行号显示播放键） */
  playing: boolean
  favorite: boolean
  coverFailed: boolean
}>()

const emit = defineEmits<{
  (e: 'play'): void
  (e: 'menu', event: MouseEvent): void
  (e: 'favorite'): void
  (e: 'cover-error'): void
}>()

const hasCover = computed(() => Boolean(props.item.cover) && !props.coverFailed)

/** 副标题：`艺术家 · 专辑`，缺哪边就不补分隔符 */
const subtitle = computed(() => {
  const artist = props.item.artist?.trim() ?? ''
  const album = props.item.album?.trim() ?? ''
  if (artist && album && artist !== album) return `${artist} · ${album}`
  return artist || album
})

const title = computed(() => props.item.title?.trim() || props.item.name.replace(/\.[^.]+$/, ''))
const duration = computed(() => formatDuration(props.item.duration_ms))
</script>

<template>
  <div
    class="group flex h-14 items-center gap-3 rounded-md px-2 transition-colors hover:bg-elevated/60"
    :class="active ? 'bg-elevated/60' : ''"
    :data-track="item.path"
    :title="title"
    @click="emit('play')"
    @contextmenu.prevent="emit('menu', $event)"
  >
    <!-- 行号 / 播放键 / 跳动条三态：悬停或正在播时都可点，不用去顶部找按钮 -->
    <button
      type="button"
      class="grid w-6 shrink-0 place-items-center text-muted"
      :title="t('gallery.music.play')"
      @click.stop="emit('play')"
    >
      <span v-if="active && playing" class="playing-wave">
        <span /><span /><span />
      </span>
      <AppIcon
        v-else-if="active"
        name="playSolid"
        :size="12"
        class="text-accent"
      />
      <span v-else class="text-[12px] tabular-nums group-hover:hidden">{{ index + 1 }}</span>
      <AppIcon v-if="!active" name="playSolid" :size="12" class="hidden text-ink group-hover:block" />
    </button>

    <span
      class="relative grid size-9 shrink-0 place-items-center overflow-hidden rounded text-white/80 sm:size-10"
      :style="{ backgroundColor: placeholderColor(item.path) }"
    >
      <AppIcon v-if="!hasCover" name="audio" :size="16" />
      <img
        v-else
        :src="coverUrl(item.path)"
        alt=""
        loading="lazy"
        class="size-full object-cover"
        @error="emit('cover-error')"
      />
    </span>

    <span class="min-w-0 flex-1">
      <span
        class="block truncate text-[13px]"
        :class="active ? 'font-medium text-accent' : 'text-ink'"
      >
        {{ title }}
      </span>
      <span v-if="subtitle" class="mt-0.5 block truncate text-[11px] text-muted">
        {{ subtitle }}
      </span>
    </span>

    <span class="w-14 shrink-0 text-right text-[11px] tabular-nums text-muted">{{ duration }}</span>

    <button
      type="button"
      class="grid size-7 shrink-0 place-items-center rounded transition-colors hover:bg-panel"
      :class="favorite ? 'text-accent' : 'text-muted/60 hover:text-ink'"
      :title="favorite ? t('gallery.favorite.remove') : t('gallery.favorite.add')"
      @click.stop="emit('favorite')"
    >
      <AppIcon name="heart" :size="14" :filled="favorite" />
    </button>

    <!-- 更多：右键也能开。窄屏没有 hover，按钮必须常驻可见（v4.9.68）——
         以前 `hidden sm:grid` + group-hover 恰好把最需要它的触屏用户挡在门外。 -->
    <button
      type="button"
      class="grid size-7 shrink-0 place-items-center rounded text-muted opacity-70 transition-colors hover:bg-panel hover:text-ink focus-visible:bg-panel focus-visible:text-ink sm:opacity-40 sm:group-hover:opacity-100"
      :title="t('gallery.toolbar.more')"
      :aria-label="t('gallery.toolbar.more')"
      data-testid="track-more"
      @click.stop="emit('menu', $event)"
    >
      <AppIcon name="more" :size="14" />
    </button>
  </div>
</template>
