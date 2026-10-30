<script setup lang="ts">
/**
 * 底部全局悬浮音乐播放条（设计文档 3.4）：
 * 列表循环 / 单曲 / 随机、进度拖拽、可展开的 .lrc 歌词面板、
 * PC 端音量滑块 + 静音（移动端交给物理键）、播放列表抽屉（8.4.7）。
 * 挂在 App.vue 认证布局下，跨页面不中断播放。
 */
import { computed, onBeforeUnmount, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import type { IconName } from './icons'
import { player, currentTrack, type PlayMode } from '../stores/player'
import { rawUrl, coverUrl, placeholderColor } from '../api/media'
import { volume, bindVolume, applyVolume } from '../lib/volume'

const { t } = useI18n()
const audio = ref<HTMLAudioElement | null>(null)
/** 歌词面板 */
const expanded = ref(false)
/** 播放列表抽屉 */
const listOpen = ref(false)
const lyricBox = ref<HTMLElement | null>(null)
const seeking = ref(false)
/** 内嵌封面加载失败时退回图标占位（老文件常常标注了 APIC 但数据是坏的） */
const coverFailed = ref(false)

const modes: { key: PlayMode; icon: IconName; label: string }[] = [
  { key: 'loop', icon: 'refresh', label: 'gallery.player.modeLoop' },
  { key: 'one', icon: 'repeat', label: 'gallery.player.modeOne' },
  { key: 'shuffle', icon: 'shuffle', label: 'gallery.player.modeShuffle' },
]

const modeLabel = computed(() =>
  t(modes.find((mode) => mode.key === player.state.mode)?.label ?? 'gallery.player.modeLoop'),
)

const modeIcon = computed<IconName>(
  () => modes.find((mode) => mode.key === player.state.mode)?.icon ?? 'refresh',
)

const playIcon = computed<IconName>(() => (player.state.playing ? 'pause' : 'play'))

/** 移动端条顶那根 2px 进度线：没有横向拖动条的位置，也不该没有进度感 */
const progressPercent = computed(() => {
  const total = player.state.duration
  if (!Number.isFinite(total) || total <= 0) return '0%'
  const ratio = Math.min(1, Math.max(0, player.state.currentTime / total))
  return `${(ratio * 100).toFixed(2)}%`
})

const volumeIcon = computed<IconName>(() => (volume.muted ? 'volumeMute' : 'volume'))
const volumeTitle = computed(() =>
  volume.muted ? t('gallery.player.unmute') : t('gallery.player.mute'),
)

function cycleMode() {
  const index = modes.findIndex((mode) => mode.key === player.state.mode)
  player.setMode(modes[(index + 1) % modes.length].key)
}

function closePlayer() {
  listOpen.value = false
  expanded.value = false
  player.clearQueue()
}

/** 歌词面板与播放列表互斥：两个都从底部弹，叠在一起会互相遮挡 */
function toggleLyrics() {
  expanded.value = !expanded.value
  if (expanded.value) listOpen.value = false
}

function toggleList() {
  listOpen.value = !listOpen.value
  if (listOpen.value) expanded.value = false
}

function onVolumeInput(event: Event) {
  volume.set(Number((event.target as HTMLInputElement).value))
}

const displayTitle = computed(() => {
  const track = currentTrack.value
  if (!track) return ''
  return track.title ?? track.name
})

const displayArtist = computed(() => {
  const track = currentTrack.value
  if (!track) return ''
  const folder = track.folder.split('/').filter(Boolean).pop() ?? ''
  return track.artist ?? folder
})

/** 当前曲目是否有可展示的内嵌封面 */
const showCover = computed(() => Boolean(currentTrack.value?.cover) && !coverFailed.value)

function trackTitle(path: string, title?: string | null, name = '') {
  return title ?? (name || path.split('/').pop() || path)
}

function fmt(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return '0:00'
  const total = Math.floor(seconds)
  const minutes = Math.floor(total / 60)
  return `${minutes}:${String(total % 60).padStart(2, '0')}`
}

function onTimeUpdate() {
  if (!seeking.value && audio.value) player.state.currentTime = audio.value.currentTime
}

function onLoaded() {
  if (audio.value) player.state.duration = audio.value.duration || 0
}

function onEnded() {
  if (player.state.mode === 'one') return // loop 属性会自动重放
  player.next(false)
}

function onSeekInput(event: Event) {
  const target = event.target as HTMLInputElement
  seeking.value = true
  player.state.currentTime = Number(target.value)
}

function onSeekCommit() {
  if (audio.value) audio.value.currentTime = player.state.currentTime
  seeking.value = false
}

function onError() {
  player.state.lyricError = ''
}

/** 当前应高亮的歌词行 */
const activeLyric = computed(() => {
  const lines = player.state.lyrics
  if (!lines || lines.length === 0) return -1
  const time = player.state.currentTime + 0.2
  let index = -1
  for (let i = 0; i < lines.length; i += 1) {
    if (lines[i].time <= time) index = i
    else break
  }
  return index
})

watch(activeLyric, () => {
  const box = lyricBox.value
  if (!box || activeLyric.value < 0) return
  const line = box.querySelector<HTMLElement>(`[data-line="${activeLyric.value}"]`)
  if (line) {
    box.scrollTo({ top: line.offsetTop - box.clientHeight / 2 + line.clientHeight / 2, behavior: 'smooth' })
  }
})

// 音量绑定：换元素（首次渲染/切歌重建）时重新挂，卸载时解绑
let detachVolume: (() => void) | null = null
watch(audio, (element) => {
  detachVolume?.()
  detachVolume = element ? bindVolume(element) : null
})

// 在别处（视频播放器、设置）调过音量时，音乐条这边也要跟上
watch(
  () => [volume.state.volume, volume.state.muted],
  () => applyVolume(audio.value),
)

onBeforeUnmount(() => {
  detachVolume?.()
})

watch(
  () => player.state.playing,
  (playing) => {
    const element = audio.value
    if (!element) return
    if (playing) void element.play().catch(() => (player.state.playing = false))
    else element.pause()
  },
)

// 换曲时 <audio :src> 变化后若处于播放态需要显式 play
watch(
  () => currentTrack.value?.path,
  () => {
    player.state.currentTime = 0
    player.state.duration = 0
    coverFailed.value = false
    const element = audio.value
    if (element && player.state.playing) void element.play().catch(() => (player.state.playing = false))
  },
)
</script>

<template>
  <!--
    歌词面板与播放列表：fixed 兄弟节点，各自淡入淡出。
    注意 v-if 必须带上 currentTrack —— 过渡只能感知「自己子节点」的 v-if，
    若改用外层 <template v-if> 包住整棵树，收歌时整棵子树会被瞬间卸载，动画全程不触发。
  -->
  <Transition name="fade">
      <div
        v-if="currentTrack && expanded"
        class="fixed inset-x-0 bottom-[calc(80px+env(safe-area-inset-bottom,0px))] z-40 mx-auto flex h-64 w-full max-w-md flex-col overflow-hidden rounded-2xl border border-line bg-panel/90 shadow-2xl backdrop-blur md:bottom-[calc(104px+env(safe-area-inset-bottom,0px))] md:max-w-lg"
      >
        <div class="border-b border-line/60 px-4 py-2 text-center">
          <p class="truncate text-sm font-medium text-ink">{{ displayTitle }}</p>
          <p class="truncate text-[12px] text-muted">{{ displayArtist }}</p>
        </div>
        <div v-if="player.state.lyrics" ref="lyricBox" class="min-h-0 flex-1 overflow-y-auto px-4 py-3">
          <p
            v-for="(line, index) in player.state.lyrics"
            :key="index"
            :data-line="index"
            class="py-1.5 text-center text-[14px] transition-colors"
            :class="index === activeLyric ? 'font-medium text-accent' : 'text-muted'"
          >
            {{ line.text || '···' }}
          </p>
        </div>
        <p v-else class="grid flex-1 place-items-center px-6 text-center text-xs text-muted">
          {{ player.state.lyricError || t('gallery.player.lyricsLoading') }}
        </p>
      </div>
    </Transition>

    <!--
      播放列表抽屉（8.4.7）：PC 贴底栏右上方，移动端占下半屏。
      z-30 低于控制条的 z-40，且不加遮罩 —— 展开时控制条仍可点。
    -->
    <Transition name="fade">
      <div
        v-if="currentTrack && listOpen"
        class="fixed inset-x-0 bottom-[calc(64px+env(safe-area-inset-bottom,0px))] z-30 flex h-[50vh] flex-col overflow-hidden rounded-t-2xl border border-line bg-panel/95 shadow-2xl backdrop-blur md:inset-x-auto md:right-4 md:bottom-[calc(96px+env(safe-area-inset-bottom,0px))] md:h-auto md:max-h-[60vh] md:w-80 md:rounded-2xl"
      >
        <div class="flex shrink-0 items-center gap-2 border-b border-line/60 px-4 py-2.5">
          <p class="min-w-0 flex-1 truncate text-sm font-medium text-ink">
            {{ t('gallery.player.playlist') }}
          </p>
          <span class="shrink-0 text-[12px] text-muted">
            {{ t('gallery.player.playlistCount', { count: player.state.queue.length }) }}
          </span>
          <button
            v-if="player.state.queue.length > 0"
            class="shrink-0 rounded px-1.5 py-1 text-[12px] text-muted transition-colors hover:text-ink"
            @click="player.clearQueue(); listOpen = false"
          >
            {{ t('gallery.player.playlistClear') }}
          </button>
        </div>

        <div class="min-h-0 flex-1 overflow-y-auto py-1">
          <p
            v-if="player.state.queue.length === 0"
            class="px-4 py-8 text-center text-xs text-muted"
          >
            {{ t('gallery.player.playlistEmpty') }}
          </p>
          <button
            v-for="(track, index) in player.state.queue"
            :key="`${track.path}-${index}`"
            class="flex w-full items-center gap-2.5 px-3 py-2 text-left transition-colors hover:bg-elevated/60"
            :class="index === player.state.index ? 'bg-elevated/40' : ''"
            @click="player.jump(index)"
          >
            <span
              class="w-5 shrink-0 text-center text-[12px] tabular-nums"
              :class="index === player.state.index ? 'text-accent' : 'text-muted'"
            >
              <AppIcon
                v-if="index === player.state.index && player.state.playing"
                name="volume"
                :size="12"
              />
              <template v-else>{{ index + 1 }}</template>
            </span>
            <span class="min-w-0 flex-1">
              <span
                class="block truncate text-[14px]"
                :class="index === player.state.index ? 'font-medium text-accent' : 'text-ink'"
              >
                {{ trackTitle(track.path, track.title, track.name) }}
              </span>
              <span class="block truncate text-[12px] text-muted">
                {{ track.artist ?? track.folder }}
              </span>
            </span>
          </button>
        </div>
      </div>
    </Transition>

    <!--
      播放条（v4.4.8 布局 / v4.4.11 动画）：
      - 桌面：居中浮动条，宽 70%（随窗口自适应，min/max 兜住极端窗口），
        三栏布局 —— 左曲目、中控件、右工具，播放键真正落在条的几何中心；
      - 移动端：铺满底部（不浮动、不带圆角），单行 + 顶部一根 2px 进度线，
        不额外占高度；
      - slide-up 过渡必须直接包住这个**有真实高度**的 fixed 容器：
        以前包在最外层零高度 div 上，translateY(100%) 对 0 高元素不产生位移，
        收起时只剩 opacity 骤变，观感抽搐。
    -->
    <Transition name="slide-up">
      <div
        v-if="currentTrack"
        class="pointer-events-none fixed inset-x-0 bottom-0 z-40 flex justify-center"
        style="padding-bottom: env(safe-area-inset-bottom, 0px)"
      >
      <div
        class="pointer-events-auto w-full border-t border-line bg-panel/95 shadow-2xl backdrop-blur md:mb-3 md:w-[70%] md:min-w-[660px] md:max-w-[1180px] md:rounded-2xl md:border"
        data-testid="music-bar"
      >
        <!-- 移动端：单行 -->
        <div class="relative flex h-16 items-center gap-2.5 px-3 md:hidden">
          <span class="absolute inset-x-0 top-0 h-[2px] bg-line/70" aria-hidden="true">
            <span class="block h-full bg-accent" :style="{ width: progressPercent }" />
          </span>
          <span
            class="relative grid size-9 shrink-0 place-items-center overflow-hidden rounded-lg text-accent"
            :style="{ backgroundColor: placeholderColor(currentTrack.path) }"
          >
            <AppIcon v-if="!showCover" name="audio" :size="16" />
            <img
              v-else
              :src="coverUrl(currentTrack.path)"
              alt=""
              class="size-full object-cover"
              @error="coverFailed = true"
            />
          </span>
          <div class="min-w-0 flex-1">
            <button
              class="block w-full truncate text-left text-[13px] font-medium text-ink"
              :title="displayTitle"
              @click="toggleLyrics"
            >
              {{ displayTitle }}
            </button>
            <p class="truncate text-[11px] text-muted">{{ displayArtist }}</p>
          </div>
          <button
            class="grid size-8 shrink-0 place-items-center rounded-md text-ink transition-colors hover:bg-elevated"
            :title="t('gallery.player.prev')"
            @click="player.prev()"
          >
            <AppIcon name="prev" :size="16" />
          </button>
          <button
            class="grid size-10 shrink-0 place-items-center rounded-full bg-accent text-white transition-transform active:scale-95"
            :title="player.state.playing ? t('gallery.player.pause') : t('gallery.player.play')"
            @click="player.toggle()"
          >
            <AppIcon :name="playIcon" :size="17" />
          </button>
          <button
            class="grid size-8 shrink-0 place-items-center rounded-md text-ink transition-colors hover:bg-elevated"
            :title="t('gallery.player.next')"
            @click="player.next()"
          >
            <AppIcon name="next" :size="16" />
          </button>
          <button
            class="grid size-8 shrink-0 place-items-center rounded-md text-muted transition-colors hover:text-ink"
            :title="t('gallery.player.close')"
            @click="closePlayer"
          >
            <AppIcon name="close" :size="15" />
          </button>
        </div>

        <!-- 桌面：左曲目 / 中控件 / 右工具 -->
        <div class="hidden h-[76px] grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)] items-center gap-4 px-4 md:grid">
          <div class="flex min-w-0 items-center gap-2.5">
          <span
            class="relative grid size-10 shrink-0 place-items-center overflow-hidden rounded-lg text-accent"
            :style="{ backgroundColor: placeholderColor(currentTrack.path) }"
          >
            <AppIcon v-if="!showCover" name="audio" :size="16" />
            <img
              v-else
              :src="coverUrl(currentTrack.path)"
              alt=""
              class="size-full object-cover"
              @error="coverFailed = true"
            />
          </span>
            <div class="min-w-0">
              <button
                class="block w-full truncate text-left text-[13px] font-medium text-ink hover:text-accent"
                :title="displayTitle"
                @click="toggleLyrics"
              >
                {{ displayTitle }}
              </button>
              <p class="truncate text-[11px] text-muted">{{ displayArtist }}</p>
            </div>
          </div>

          <div class="flex w-[min(34vw,400px)] min-w-[280px] flex-col items-center gap-1">
            <div class="flex items-center gap-4">
              <button
                class="grid size-8 place-items-center rounded-full text-ink transition-colors hover:bg-elevated"
                :title="t('gallery.player.prev')"
                @click="player.prev()"
              >
                <AppIcon name="prev" :size="17" />
              </button>
              <button
                class="grid size-10 place-items-center rounded-full bg-accent text-white transition-transform hover:scale-105 active:scale-95"
                :title="player.state.playing ? t('gallery.player.pause') : t('gallery.player.play')"
                data-testid="music-bar-play"
                @click="player.toggle()"
              >
                <AppIcon :name="playIcon" :size="17" />
              </button>
              <button
                class="grid size-8 place-items-center rounded-full text-ink transition-colors hover:bg-elevated"
                :title="t('gallery.player.next')"
                @click="player.next()"
              >
                <AppIcon name="next" :size="17" />
              </button>
            </div>
            <div class="flex w-full items-center gap-2">
              <span class="shrink-0 text-[11px] text-muted tabular-nums">{{ fmt(player.state.currentTime) }}</span>
              <input
                type="range"
                min="0"
                :max="Math.max(player.state.duration, 0.1)"
                step="0.1"
                :value="player.state.currentTime"
                class="h-1 min-w-0 flex-1 cursor-pointer accent-[var(--accent)]"
                @input="onSeekInput"
                @change="onSeekCommit"
                @pointerdown="seeking = true"
              />
              <span class="shrink-0 text-[11px] text-muted tabular-nums">{{ fmt(player.state.duration) }}</span>
            </div>
          </div>

          <div class="flex items-center justify-end gap-1">
            <button
              class="grid size-8 place-items-center rounded-md text-muted transition-colors hover:bg-elevated hover:text-ink"
              :title="modeLabel"
              @click="cycleMode"
            >
              <AppIcon :name="modeIcon" :size="15" />
            </button>
            <div class="hidden items-center gap-1 lg:flex">
              <button
                class="grid size-8 place-items-center rounded-md text-muted transition-colors hover:bg-elevated hover:text-ink"
                :title="volumeTitle"
                @click="volume.toggle()"
              >
                <AppIcon :name="volumeIcon" :size="15" />
              </button>
              <input
                type="range"
                min="0"
                max="1"
                step="0.01"
                :value="volume.sliderValue"
                :title="t('gallery.player.volume')"
                class="h-1 w-20 cursor-pointer accent-[var(--accent)]"
                @input="onVolumeInput"
              />
            </div>
            <button
              class="grid size-8 place-items-center rounded-md transition-colors"
              :class="listOpen ? 'text-accent' : 'text-muted hover:text-ink'"
              :title="t('gallery.player.playlist')"
              @click="toggleList"
            >
              <AppIcon name="list" :size="16" />
            </button>
            <button
              class="grid size-8 place-items-center rounded-md transition-colors"
              :class="expanded ? 'text-accent' : 'text-muted hover:text-ink'"
              :title="t('gallery.player.lyrics')"
              @click="toggleLyrics"
            >
              <AppIcon name="fileText" :size="15" />
            </button>
            <button
              class="grid size-8 place-items-center rounded-md text-muted transition-colors hover:text-ink"
              :title="t('gallery.player.close')"
              @click="closePlayer"
            >
              <AppIcon name="close" :size="15" />
            </button>
          </div>
        </div>
      </div>
      </div>
    </Transition>

    <audio
      v-if="currentTrack"
      ref="audio"
      data-testid="music-bar-audio"
      :src="rawUrl(currentTrack.path)"
      :loop="player.state.mode === 'one'"
      :autoplay="player.state.playing"
      preload="metadata"
      @timeupdate="onTimeUpdate"
      @loadedmetadata="onLoaded"
      @ended="onEnded"
      @error="onError"
    />
</template>
