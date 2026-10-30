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
  return `linear-gradient(to right, var(--accent) 0 ${p}, var(--range-track) ${p} 100%)`
})

/** 音量条填充：与进度条同一套细轨样式 */
const volumeBackground = computed(() => {
  const v = `${(volume.sliderValue * 100).toFixed(1)}%`
  return `linear-gradient(to right, var(--accent) 0 ${v}, var(--range-track) ${v} 100%)`
})

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

/**
 * 桌面副行（v4.5.1 对齐参考稿）：只放 ID3 歌手 / 专辑，不再回退目录名
 * （用户反馈「文件夹这个不需要」）。歌词改由右侧的内联歌词区承担。
 */
const displayArtist = computed(() => {
  const track = currentTrack.value
  if (!track) return ''
  return track.artist?.trim() || track.album?.trim() || ''
})

/**
 * 移动端与极简面板的副行（v4.4.12）：优先当前歌词行 —— 这两种形态没有
 * 内联歌词区，副行是唯一能看到歌词滚动的地方；没有歌词退回歌手。
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
      data-testid="music-lyrics-panel"
      class="fixed inset-x-0 bottom-[calc(80px+env(safe-area-inset-bottom,0px))] z-40 mx-auto flex h-64 w-full max-w-md flex-col overflow-hidden rounded-2xl border border-line bg-panel/95 shadow-2xl backdrop-blur-xl md:bottom-[calc(130px+env(safe-area-inset-bottom,0px))] md:max-w-lg"
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
      data-testid="music-playlist"
      class="fixed inset-x-0 bottom-[calc(78px+env(safe-area-inset-bottom,0px))] z-30 flex h-[50vh] flex-col overflow-hidden rounded-t-2xl border border-line bg-panel/95 shadow-2xl backdrop-blur md:inset-x-auto md:right-4 md:bottom-[calc(130px+env(safe-area-inset-bottom,0px))] md:h-auto md:max-h-[60vh] md:w-80 md:rounded-2xl"
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
        class="group/bar pointer-events-auto relative w-full border-t border-line bg-panel/95 backdrop-blur-2xl backdrop-saturate-150 md:mb-6 md:w-[clamp(760px,85%,1200px)] md:rounded-[20px] md:border md:shadow-[0_10px_30px_-8px_rgba(0,0,0,0.45)]"
        data-testid="music-bar"
      >
        <!--
          右上角外侧关闭键（参考稿 .window-ctrl）：浮在条外 -14px，
          hover 整条或键盘聚焦才浮现、平时完全隐形 —— 条内不必再挤一个灰按钮。
        -->
        <div
          class="absolute -top-3.5 right-5 z-20 hidden opacity-0 transition-opacity duration-200 group-hover/bar:opacity-100 focus-within:opacity-100 md:block"
        >
          <button
            class="grid size-6 place-items-center rounded-full border border-line bg-panel text-muted shadow-sm transition-all hover:scale-110 hover:border-transparent hover:bg-red-500/85 hover:text-white"
            :title="t('gallery.player.close')"
            data-testid="music-bar-close"
            @click="closePlayer"
          >
            <AppIcon name="close" :size="12" />
          </button>
        </div>

        <div class="px-3 pb-2 pt-2 md:px-6 md:py-3">
          <!-- ① 全宽进度行：时间 – 细滑条 – 时长 -->
          <div class="flex items-center gap-3">
            <span class="w-[34px] shrink-0 text-center text-[11px] tabular-nums text-muted">
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
            <span class="w-[34px] shrink-0 text-center text-[11px] tabular-nums text-muted">
              {{ fmt(player.state.duration) }}
            </span>
          </div>

          <!-- ② 移动端：单行，尺寸对齐参考稿窄屏断点（封面 32 / 播放 34 / 其它 28~32） -->
          <div class="mt-1.5 flex items-center gap-1.5 md:hidden">
            <span
              class="relative grid size-8 shrink-0 place-items-center overflow-hidden rounded-lg text-accent"
              :style="{ backgroundColor: placeholderColor(currentTrack.path) }"
            >
              <AppIcon v-if="!showCover" name="audio" :size="14" />
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
                class="block w-full truncate text-left text-[12px] font-semibold leading-tight text-ink"
                :title="displayTitle"
                @click="toggleLyrics"
              >
                {{ displayTitle }}
              </button>
              <p
                class="truncate text-[10px] leading-tight"
                :class="subtitleIsLyric ? 'text-accent/80' : 'text-muted'"
              >
                {{ displaySubtitle }}
              </p>
            </div>
            <button
              class="grid size-7 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink active:scale-90"
              :title="t('gallery.player.prev')"
              @click="player.prev()"
            >
              <AppIcon name="prev" :size="14" />
            </button>
            <button
              class="grid size-[34px] shrink-0 place-items-center rounded-full bg-accent text-white play-glow transition-transform active:scale-95"
              :title="player.state.playing ? t('gallery.player.pause') : t('gallery.player.play')"
              data-testid="music-bar-play"
              @click="player.toggle()"
            >
              <AppIcon :name="playIcon" :size="15" />
            </button>
            <button
              class="grid size-7 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink active:scale-90"
              :title="t('gallery.player.next')"
              @click="player.next()"
            >
              <AppIcon name="next" :size="14" />
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
              data-testid="music-bar-close"
              @click="closePlayer"
            >
              <AppIcon name="close" :size="15" />
            </button>
          </div>

          <!--
            ③ 桌面：曲目 | 内联歌词（竖线分隔、吃满中段）| 控件。
            参考稿是一行连续排布（没有三栏 grid），中段留给歌词滚动，
            控件按「音量 → 模式 → 上一首 → 播放 → 下一首 ｜ 列表 / 歌词 / 极简」排。
          -->
          <div class="mt-2.5 hidden items-center gap-3 md:flex">
            <div class="flex min-w-0 max-w-[248px] shrink-0 items-center gap-2.5">
              <span
                class="relative grid size-10 shrink-0 place-items-center overflow-hidden rounded-xl text-accent shadow-[0_4px_12px_rgba(0,0,0,0.15)]"
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
                  class="block w-full truncate text-left text-[13px] font-semibold leading-[1.3] text-ink transition-colors hover:text-accent"
                  :title="displayTitle"
                  @click="toggleLyrics"
                >
                  {{ displayTitle }}
                </button>
                <p class="mt-0.5 truncate text-[11px] leading-[1.3] text-muted" :title="displayArtist">
                  {{ displayArtist }}
                </p>
              </div>
            </div>

            <!-- 内联歌词：一条竖线隔开曲目区，剩下的宽度全给当前歌词行 -->
            <div class="flex h-6 min-w-0 flex-1 items-center overflow-hidden border-l border-line px-3">
              <p class="w-full truncate text-[13px] font-medium leading-none text-accent">
                {{ currentLyric }}
              </p>
            </div>

            <div class="flex shrink-0 items-center gap-2">
              <!-- 音量：只有一根细轨（参考稿如此），拖到 0 即静音，窄屏收起走物理键 -->
              <input
                type="range"
                min="0"
                max="1"
                step="0.01"
                :value="volume.sliderValue"
                :title="volumeTitle"
                class="music-range hidden w-16 min-w-0 shrink-0 cursor-pointer lg:block"
                :style="{ backgroundImage: volumeBackground }"
                @input="onVolumeInput"
              />

              <!-- 模式键：32px 方角描边（参考稿 .mode-btn），非列表循环时 accent 高亮 -->
              <button
                class="grid size-8 shrink-0 place-items-center rounded-lg border transition-colors active:scale-90"
                :class="
                  player.state.mode === 'loop'
                    ? 'border-line text-muted hover:border-muted/40 hover:bg-elevated hover:text-ink'
                    : 'border-accent bg-accent/10 text-accent'
                "
                :title="modeLabel"
                @click="cycleMode"
              >
                <AppIcon :name="modeIcon" :size="16" />
              </button>

              <button
                class="grid size-9 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink active:scale-90"
                :title="t('gallery.player.prev')"
                @click="player.prev()"
              >
                <AppIcon name="prev" :size="17" />
              </button>
              <button
                class="grid size-11 shrink-0 place-items-center rounded-full bg-accent text-white play-glow transition-all hover:scale-105 hover:brightness-110 active:scale-95"
                :title="player.state.playing ? t('gallery.player.pause') : t('gallery.player.play')"
                data-testid="music-bar-play"
                @click="player.toggle()"
              >
                <AppIcon :name="playIcon" :size="18" />
              </button>
              <button
                class="grid size-9 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink active:scale-90"
                :title="t('gallery.player.next')"
                @click="player.next()"
              >
                <AppIcon name="next" :size="17" />
              </button>

              <button
                class="grid size-8 shrink-0 place-items-center rounded-lg transition-colors hover:bg-elevated"
                :class="listOpen ? 'bg-accent/10 text-accent' : 'text-muted hover:text-ink'"
                :title="t('gallery.player.playlist')"
                @click="toggleList"
              >
                <AppIcon name="list" :size="16" />
              </button>
              <button
                class="grid size-8 shrink-0 place-items-center rounded-lg text-muted transition-colors hover:bg-elevated hover:text-ink"
                :title="t('gallery.player.styleMinimal')"
                data-testid="music-bar-minimize"
                @click="switchStyle('minimal')"
              >
                <AppIcon name="disc" :size="16" />
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  </Transition>

  <!--
    极简形态（v4.5.1 对齐参考稿）：右下角 64px 黑胶胶囊。
    唱片 48px 停在右侧（right/top 各 8px），桌面 hover 向左展开成 300px 面板；
    触屏点唱片展开/收起，双击还原标准条。关闭键浮在胶囊右上方外侧、hover 才出现。
  -->
  <Transition name="slide-up">
    <div
      v-if="currentTrack && player.state.barStyle === 'minimal'"
      class="group/mini pointer-events-none fixed right-5 z-40"
      style="bottom: calc(20px + env(safe-area-inset-bottom, 0px))"
      data-testid="music-bar-minimal"
    >
      <button
        class="pointer-events-auto absolute -top-3.5 right-2 z-20 grid size-6 place-items-center rounded-full border border-line bg-panel text-muted opacity-0 shadow-sm transition-all hover:scale-110 hover:border-transparent hover:bg-red-500/85 hover:text-white focus-visible:opacity-100 group-hover/mini:opacity-100 [@media(hover:none)]:opacity-100 [@media(hover:none)]:opacity-100"
        :title="t('gallery.player.close')"
        data-testid="music-bar-close"
        @click="closePlayer"
      >
        <AppIcon name="close" :size="12" />
      </button>

      <div
        class="pointer-events-auto relative flex h-16 items-center overflow-hidden rounded-full border border-line bg-panel/95 shadow-[0_10px_30px_-10px_rgba(0,0,0,0.55)] backdrop-blur-2xl backdrop-saturate-150 transition-[width] duration-[400ms] ease-[cubic-bezier(0.22,1,0.36,1)]"
        :class="
          miniOpen
            ? 'w-[min(300px,calc(100vw-2.5rem))]'
            : 'w-16 group-hover/mini:w-[min(300px,calc(100vw-2.5rem))]'
        "
      >
        <!-- 展开面板：右侧留 64px 给唱片 -->
        <div
          class="absolute inset-y-0 left-0 right-16 flex items-center gap-0.5 pl-4 opacity-0 transition-opacity duration-200"
          :class="miniOpen ? 'opacity-100' : 'group-hover/mini:opacity-100'"
        >
          <div class="mr-auto min-w-0">
            <p class="truncate text-[13px] font-semibold leading-tight text-ink">
              {{ displayTitle }}
            </p>
            <p
              class="truncate text-[11px] leading-tight"
              :class="subtitleIsLyric ? 'text-accent/80' : 'text-muted'"
            >
              {{ displaySubtitle }}
            </p>
          </div>
          <button
            class="grid size-7 shrink-0 place-items-center rounded-full bg-accent text-white play-glow transition-transform active:scale-95"
            :title="player.state.playing ? t('gallery.player.pause') : t('gallery.player.play')"
            @click="player.toggle()"
          >
            <AppIcon :name="playIcon" :size="13" />
          </button>
          <button
            class="grid size-6 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink"
            :title="t('gallery.player.next')"
            @click="player.next()"
          >
            <AppIcon name="next" :size="13" />
          </button>
          <button
            class="grid size-6 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink"
            :title="t('gallery.player.styleClassic')"
            data-testid="music-bar-restore"
            @click="switchStyle('classic')"
          >
            <AppIcon name="expand" :size="13" />
          </button>
        </div>

        <!--
          唱片本体（参考稿 .record）：48px 黑胶 —— 深色同心纹路 + conic 高光，
          中心标贴换成封面（保留信息量），正中一枚小轴孔；播放时整盘旋转。
        -->
        <button
          type="button"
          class="absolute right-2 top-2 grid size-12 place-items-center rounded-full transition-transform duration-200 hover:scale-105 active:scale-95"
          :title="t('gallery.player.discHint')"
          @click="onDiscClick"
          @dblclick="onDiscDblClick"
        >
          <span
            class="music-disc relative block size-12 rounded-full"
            :class="{ 'is-playing': player.state.playing }"
            style="box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.07), 0 5px 14px rgba(0, 0, 0, 0.45)"
          >
            <span
              class="absolute inset-0 rounded-full"
              style="background: repeating-radial-gradient(circle at 50%, #2a2a34 0 1.5px, #14141a 1.5px 3px)"
            />
            <span
              class="absolute inset-0 rounded-full"
              style="background: conic-gradient(from 0deg, rgba(255, 255, 255, 0.14), transparent 22%, transparent 78%, rgba(255, 255, 255, 0.06))"
            />
            <span
              class="absolute inset-[30%] overflow-hidden rounded-full bg-accent ring-1 ring-black/50"
            >
              <img
                v-if="showCover"
                :src="coverUrl(currentTrack.path)"
                alt=""
                class="size-full object-cover"
                @error="coverFailed = true"
              />
            </span>
            <span class="absolute inset-[46%] rounded-full bg-[#0b0b12]" />
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
