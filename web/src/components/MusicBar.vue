<script setup lang="ts">
/**
 * 底部全局悬浮音乐播放条（设计文档 3.4）：
 * 列表循环 / 单曲 / 随机、进度拖拽、可展开的 .lrc 歌词面板、
 * PC 端音量滑块 + 静音（移动端交给物理键）、播放列表抽屉（8.4.7）。
 * 挂在 App.vue 认证布局下，跨页面不中断播放。
 *
 * v4.5.0 视觉重做（参考用户给的「沉浸与常驻」稿，只取样式、交互按我们的来）：
 * - 进度条独占顶行、贯穿整条（4px 细轨 + accent 填充，拇指 hover 才浮出）；
 * - 播放键 44px、accent 底 + 柔和外发光，前后切歌 36px；
 * - 模式键改成描边胶囊，非列表循环时高亮；
 * - 极简形态改为右下角黑胶唱片：播放时旋转，hover 展开成胶囊面板。
 */
import { computed, onBeforeUnmount, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import type { IconName } from './icons'
import { player, currentTrack, type BarStyle, type PlayMode } from '../stores/player'
import { rawUrl, coverUrl, placeholderColor } from '../api/media'
import { volume, bindVolume, applyVolume } from '../lib/volume'

const { t } = useI18n()
const audio = ref<HTMLAudioElement | null>(null)
/** 歌词面板 */
const expanded = ref(false)
/** 播放列表抽屉 */
const listOpen = ref(false)
/** 极简唱片在触屏上点开后保持展开（桌面靠 hover） */
const miniOpen = ref(false)
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

/** 进度比例（0~100 的数值，用来画滑条填充） */
const progressRatio = computed(() => {
  const total = player.state.duration
  if (!Number.isFinite(total) || total <= 0) return 0
  return Math.min(100, Math.max(0, (player.state.currentTime / total) * 100))
})

/** 滑条填充：accent 走到当前比例，其余是轨道色 */
const rangeBackground = computed(() => {
  const p = `${progressRatio.value.toFixed(2)}%`
  return `linear-gradient(to right, var(--accent) 0 ${p}, var(--border-color) ${p} 100%)`
})

/** 音量条填充：与进度条同一套细轨样式 */
const volumeBackground = computed(() => {
  const v = `${(volume.sliderValue * 100).toFixed(1)}%`
  return `linear-gradient(to right, var(--accent) 0 ${v}, var(--border-color) ${v} 100%)`
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
  miniOpen.value = false
  player.clearQueue()
}

/** 切换形态：顺手收起浮层面板与极简展开态，避免悬在旧位置 */
function switchStyle(style: BarStyle) {
  miniOpen.value = false
  player.setBarStyle(style)
}

/** 触点设备没有 hover：点唱片 = 展开面板；桌面点唱片 = 播放/暂停 */
function onDiscClick() {
  if (window.matchMedia('(hover: none)').matches) {
    miniOpen.value = !miniOpen.value
    return
  }
  player.toggle()
}

/** 桌面双击唱片还原标准条（参考稿的双击还原手感） */
function onDiscDblClick() {
  if (window.matchMedia('(hover: none)').matches) return
  switchStyle('classic')
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

/**
 * 歌名下方的副行（v4.4.12）：优先当前歌词行 —— 条上直接滚动显示 karaoke 字，
 * 点歌名仍可展开完整歌词面板；没有歌词退回 ID3 歌手；两者都没有就留空，
 * 不再回退到目录名（用户反馈「文件夹这个不需要」）。
 */
const currentLyric = computed(() => {
  const lines = player.state.lyrics
  if (!lines || activeLyric.value < 0) return ''
  return lines[activeLyric.value]?.text?.trim() ?? ''
})

const displaySubtitle = computed(
  () => currentLyric.value || currentTrack.value?.artist?.trim() || '',
)

/** 副行是不是歌词（决定着色：歌词行用 accent 弱化色，歌手用 muted） */
const subtitleIsLyric = computed(() => currentLyric.value.length > 0)

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
      class="fixed inset-x-0 bottom-[calc(84px+env(safe-area-inset-bottom,0px))] z-40 mx-auto flex h-64 w-full max-w-md flex-col overflow-hidden rounded-2xl border border-line bg-panel/95 shadow-2xl backdrop-blur-xl md:bottom-[calc(124px+env(safe-area-inset-bottom,0px))] md:max-w-lg"
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
      class="fixed inset-x-0 bottom-[calc(76px+env(safe-area-inset-bottom,0px))] z-30 flex h-[50vh] flex-col overflow-hidden rounded-t-2xl border border-line bg-panel/95 shadow-2xl backdrop-blur md:inset-x-auto md:right-4 md:bottom-[calc(120px+env(safe-area-inset-bottom,0px))] md:h-auto md:max-h-[60vh] md:w-80 md:rounded-2xl"
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
        <p v-if="player.state.queue.length === 0" class="px-4 py-8 text-center text-xs text-muted">
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
            <AppIcon v-if="index === player.state.index && player.state.playing" name="volume" :size="12" />
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
    播放条（v4.4.8 布局 / v4.4.11 动画 / v4.5.0 视觉）：
    - 桌面：居中浮动条，宽 70%，顶部一整行细进度条，下面三栏 —— 左曲目、中控件、右工具；
    - 移动端：铺满底部，进度条同样独占一行（终于能拖了），下面单行；
    - slide-up 过渡必须直接包住这个**有真实高度**的 fixed 容器：
      以前包在最外层零高度 div 上，translateY(100%) 对 0 高元素不产生位移，
      收起时只剩 opacity 骤变，观感抽搐。
  -->
  <Transition name="slide-up">
    <div
      v-if="currentTrack && player.state.barStyle === 'classic'"
      class="pointer-events-none fixed inset-x-0 bottom-0 z-40 flex justify-center"
      style="padding-bottom: env(safe-area-inset-bottom, 0px)"
    >
      <div
        class="pointer-events-auto w-full overflow-hidden border-t border-line bg-panel/95 backdrop-blur-xl md:mb-3 md:w-[70%] md:min-w-[720px] md:max-w-[1180px] md:rounded-2xl md:border md:shadow-[0_18px_50px_-28px_rgba(0,0,0,0.75)]"
        data-testid="music-bar"
      >
        <!-- ① 全宽进度行：时间 – 细滑条 – 时长 -->
        <div class="flex items-center gap-2.5 px-3 pt-2 md:gap-3 md:px-5 md:pt-3">
          <span class="w-8 shrink-0 text-right text-[11px] tabular-nums text-muted">
            {{ fmt(player.state.currentTime) }}
          </span>
          <input
            type="range"
            min="0"
            :max="Math.max(player.state.duration, 0.1)"
            step="0.1"
            :value="player.state.currentTime"
            class="music-range min-w-0 flex-1"
            :style="{ backgroundImage: rangeBackground }"
            :title="t('gallery.player.progress')"
            @input="onSeekInput"
            @change="onSeekCommit"
            @pointerdown="seeking = true"
          />
          <span class="w-8 shrink-0 text-[11px] tabular-nums text-muted">
            {{ fmt(player.state.duration) }}
          </span>
        </div>

        <!-- ② 移动端单行 -->
        <div class="flex items-center gap-2 px-3 pb-2 pt-1.5 md:hidden">
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
            <p
              class="truncate text-[11px]"
              :class="subtitleIsLyric ? 'text-accent/80' : 'text-muted'"
            >
              {{ displaySubtitle }}
            </p>
          </div>
          <button
            class="grid size-7 shrink-0 place-items-center rounded-full text-ink transition-colors hover:bg-elevated"
            :title="t('gallery.player.prev')"
            @click="player.prev()"
          >
            <AppIcon name="prev" :size="15" />
          </button>
          <button
            class="grid size-10 shrink-0 place-items-center rounded-full bg-accent text-white play-glow transition-transform active:scale-95"
            :title="player.state.playing ? t('gallery.player.pause') : t('gallery.player.play')"
            data-testid="music-bar-play"
            @click="player.toggle()"
          >
            <AppIcon :name="playIcon" :size="17" />
          </button>
          <button
            class="grid size-7 shrink-0 place-items-center rounded-full text-ink transition-colors hover:bg-elevated"
            :title="t('gallery.player.next')"
            @click="player.next()"
          >
            <AppIcon name="next" :size="15" />
          </button>
          <button
            class="grid size-8 shrink-0 place-items-center rounded-md text-muted transition-colors hover:text-ink"
            :title="t('gallery.player.styleMinimal')"
            data-testid="music-bar-minimize"
            @click="switchStyle('minimal')"
          >
            <AppIcon name="chevronDown" :size="16" />
          </button>
          <button
            class="grid size-8 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-red-500/85 hover:text-white"
            :title="t('gallery.player.close')"
            @click="closePlayer"
          >
            <AppIcon name="close" :size="15" />
          </button>
        </div>

        <!-- ③ 桌面：左曲目 / 中控件 / 右工具 -->
        <div
          class="hidden h-[64px] grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)] items-center gap-4 px-5 pb-3 md:grid"
        >
          <div class="flex min-w-0 items-center gap-2.5">
            <span
              class="relative grid size-10 shrink-0 place-items-center overflow-hidden rounded-xl text-accent"
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
                class="block w-full truncate text-left text-[13px] font-medium text-ink transition-colors hover:text-accent"
                :title="displayTitle"
                @click="toggleLyrics"
              >
                {{ displayTitle }}
              </button>
              <p
                class="truncate text-[11px]"
                :class="subtitleIsLyric ? 'text-accent/80' : 'text-muted'"
                :title="displaySubtitle"
              >
                {{ displaySubtitle }}
              </p>
            </div>
          </div>

          <div class="flex items-center gap-3">
            <button
              class="grid size-9 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink"
              :title="t('gallery.player.prev')"
              @click="player.prev()"
            >
              <AppIcon name="prev" :size="17" />
            </button>
            <!-- 播放键：44px + accent 外发光（参考稿 .btn.play） -->
            <button
              class="grid size-11 shrink-0 place-items-center rounded-full bg-accent text-white play-glow transition-all hover:scale-105 hover:brightness-110 active:scale-95"
              :title="player.state.playing ? t('gallery.player.pause') : t('gallery.player.play')"
              data-testid="music-bar-play"
              @click="player.toggle()"
            >
              <AppIcon :name="playIcon" :size="18" />
            </button>
            <button
              class="grid size-9 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink"
              :title="t('gallery.player.next')"
              @click="player.next()"
            >
              <AppIcon name="next" :size="17" />
            </button>
          </div>

          <!-- 右工具：按钮 size-9 + shrink-0（右列是 1fr，不加会被压成 24px 宽） -->
          <div class="flex items-center justify-end gap-1.5">
            <!-- 模式键：描边胶囊，非列表循环时 accent 高亮 -->
            <button
              class="grid size-9 shrink-0 place-items-center rounded-lg border transition-colors"
              :class="
                player.state.mode === 'loop'
                  ? 'border-line text-muted hover:bg-elevated hover:text-ink'
                  : 'border-accent/60 bg-accent/10 text-accent'
              "
              :title="modeLabel"
              @click="cycleMode"
            >
              <AppIcon :name="modeIcon" :size="16" />
            </button>
            <div class="hidden min-w-0 items-center gap-1.5 lg:flex">
              <button
                class="grid size-9 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink"
                :title="volumeTitle"
                @click="volume.toggle()"
              >
                <AppIcon :name="volumeIcon" :size="16" />
              </button>
              <input
                type="range"
                min="0"
                max="1"
                step="0.01"
                :value="volume.sliderValue"
                :title="t('gallery.player.volume')"
                class="music-range w-16 min-w-0 shrink-0 cursor-pointer xl:w-20"
                :style="{ backgroundImage: volumeBackground }"
                @input="onVolumeInput"
              />
            </div>
            <button
              class="grid size-9 shrink-0 place-items-center rounded-full transition-colors"
              :class="listOpen ? 'text-accent' : 'text-muted hover:bg-elevated hover:text-ink'"
              :title="t('gallery.player.playlist')"
              @click="toggleList"
            >
              <AppIcon name="list" :size="17" />
            </button>
            <button
              class="grid size-9 shrink-0 place-items-center rounded-full transition-colors"
              :class="expanded ? 'text-accent' : 'text-muted hover:bg-elevated hover:text-ink'"
              :title="t('gallery.player.lyrics')"
              @click="toggleLyrics"
            >
              <AppIcon name="fileText" :size="17" />
            </button>
            <button
              class="grid size-9 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink"
              :title="t('gallery.player.styleMinimal')"
              data-testid="music-bar-minimize"
              @click="switchStyle('minimal')"
            >
              <AppIcon name="chevronDown" :size="17" />
            </button>
            <button
              class="grid size-9 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-red-500/85 hover:text-white"
              :title="t('gallery.player.close')"
              @click="closePlayer"
            >
              <AppIcon name="close" :size="17" />
            </button>
          </div>
        </div>
      </div>
    </div>
  </Transition>

  <!--
    极简形态（v4.5.0）：右下角黑胶唱片。播放时旋转；桌面 hover 向左展开成胶囊面板，
    触屏点唱片展开/收起；双击唱片（桌面）或面板里的还原键回到标准条。
  -->
  <Transition name="slide-up">
    <div
      v-if="currentTrack && player.state.barStyle === 'minimal'"
      class="group/mini pointer-events-none fixed z-40 right-4"
      style="bottom: calc(1rem + env(safe-area-inset-bottom, 0px))"
      data-testid="music-bar-minimal"
    >
      <div
        class="pointer-events-auto relative flex h-16 items-center overflow-hidden rounded-full border border-line bg-panel/95 shadow-2xl backdrop-blur-xl transition-[width] duration-300 ease-out"
        :class="miniOpen ? 'w-[min(320px,calc(100vw-2rem))]' : 'w-16 group-hover/mini:w-[min(320px,calc(100vw-2rem))]'"
      >
        <!-- 展开面板：标题 + 歌词 + 播放/下一首/还原 -->
        <div
          class="absolute inset-y-0 left-0 right-16 flex items-center gap-1 pl-4 opacity-0 transition-opacity duration-200"
          :class="miniOpen ? 'opacity-100' : 'group-hover/mini:opacity-100'"
        >
          <div class="mr-auto min-w-0">
            <p class="truncate text-[13px] font-medium leading-tight text-ink">{{ displayTitle }}</p>
            <p
              class="truncate text-[11px] leading-tight"
              :class="subtitleIsLyric ? 'text-accent/80' : 'text-muted'"
            >
              {{ displaySubtitle }}
            </p>
          </div>
          <button
            class="grid size-9 shrink-0 place-items-center rounded-full bg-accent text-white play-glow transition-transform active:scale-95"
            :title="player.state.playing ? t('gallery.player.pause') : t('gallery.player.play')"
            @click="player.toggle()"
          >
            <AppIcon :name="playIcon" :size="16" />
          </button>
          <button
            class="grid size-8 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink"
            :title="t('gallery.player.next')"
            @click="player.next()"
          >
            <AppIcon name="next" :size="15" />
          </button>
          <button
            class="grid size-8 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink"
            :title="t('gallery.player.styleClassic')"
            data-testid="music-bar-restore"
            @click="switchStyle('classic')"
          >
            <AppIcon name="expand" :size="15" />
          </button>
        </div>

        <!-- 唱片本体：纹路 + 封面 + 中心标 -->
        <button
          type="button"
          class="absolute right-0 top-0 grid size-16 place-items-center"
          :title="t('gallery.player.discHint')"
          @click="onDiscClick"
          @dblclick="onDiscDblClick"
        >
          <span
            class="music-disc relative size-14 overflow-hidden rounded-full ring-1 ring-white/10"
            :class="{ 'is-playing': player.state.playing }"
            :style="{ backgroundColor: placeholderColor(currentTrack.path) }"
          >
            <img
              v-if="showCover"
              :src="coverUrl(currentTrack.path)"
              alt=""
              class="size-full object-cover"
              @error="coverFailed = true"
            />
            <span
              class="pointer-events-none absolute inset-0"
              style="background: repeating-radial-gradient(circle at 50%, rgba(255, 255, 255, 0.12) 0 1px, transparent 1px 3px)"
            />
            <!-- 中心标：黑胶的「标贴」，比封面更像唱片 -->
            <span
              class="pointer-events-none absolute inset-[36%] rounded-full bg-accent ring-2 ring-black/40"
            />
          </span>
          <span
            class="pointer-events-none absolute inset-0 grid place-items-center rounded-full bg-black/35 opacity-0 transition-opacity group-hover/mini:opacity-100"
          >
            <AppIcon :name="playIcon" :size="15" class="text-white" />
          </span>
        </button>
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
