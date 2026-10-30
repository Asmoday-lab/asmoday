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
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useStorage } from '@vueuse/core'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import type { IconName } from './icons'
import { player, currentTrack, type BarStyle, type PlayMode } from '../stores/player'
import { rawUrl, coverUrl } from '../api/media'
import { volume, bindVolume, applyVolume } from '../lib/volume'

const { t } = useI18n()
const audio = ref<HTMLAudioElement | null>(null)
/** 歌词面板 */
const expanded = ref(false)
/** 播放列表抽屉 */
const listOpen = ref(false)
/** 极简唱片在触屏上点开后保持展开（桌面靠 hover） */
const miniOpen = ref(false)
/** 音量竖排滑条弹层（参考稿 .vol-popup，窄屏与触屏都靠它调音量） */
const volOpen = ref(false)
/** 锁定常驻：锁上后关闭键置灰，避免误关（参考稿的 .window-btn.pinned） */
const pinned = useStorage('asmoday.player.pinned', false)
const lyricBox = ref<HTMLElement | null>(null)
const seeking = ref(false)
/** 内嵌封面加载失败时退回图标占位（老文件常常标注了 APIC 但数据是坏的） */
const coverFailed = ref(false)

const modes: { key: PlayMode; icon: IconName; label: string }[] = [
  { key: 'loop', icon: 'repeat', label: 'gallery.player.modeLoop' },
  { key: 'one', icon: 'repeatOne', label: 'gallery.player.modeOne' },
  { key: 'shuffle', icon: 'shuffle', label: 'gallery.player.modeShuffle' },
]

const modeLabel = computed(() =>
  t(modes.find((mode) => mode.key === player.state.mode)?.label ?? 'gallery.player.modeLoop'),
)

const modeIcon = computed<IconName>(
  () => modes.find((mode) => mode.key === player.state.mode)?.icon ?? 'refresh',
)

const playIcon = computed<IconName>(() => (player.state.playing ? 'pauseSolid' : 'playSolid'))

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

const volumeIcon = computed<IconName>(() => (volume.muted ? 'volumeMute' : 'volume'))

/** 播放列表抽屉的贴底距离：极简形态下要落在胶囊上方，别压住唱片 */
const drawerOffset = computed(() =>
  player.state.barStyle === 'minimal'
    ? 'bottom-[calc(96px+env(safe-area-inset-bottom,0px))]'
    : 'bottom-[calc(84px+env(safe-area-inset-bottom,0px))] lg:bottom-[calc(130px+env(safe-area-inset-bottom,0px))]',
)

function cycleMode() {
  const index = modes.findIndex((mode) => mode.key === player.state.mode)
  player.setMode(modes[(index + 1) % modes.length].key)
}

function closePlayer() {
  if (pinned.value) return // 锁定常驻时关闭键不可用，先解锁
  listOpen.value = false
  expanded.value = false
  miniOpen.value = false
  volOpen.value = false
  player.clearQueue()
}

/** 切换形态：顺手收起浮层面板与极简展开态，避免悬在旧位置 */
function switchStyle(style: BarStyle) {
  miniOpen.value = false
  player.setBarStyle(style)
}

/** 唱片单击/双击的合流计时器（参考稿 240ms 判定，触屏同样有效） */
let discTapTimer: number | null = null

/**
 * 唱片点击（v4.5.2）：参考稿里唱片是**唯一**的还原入口，极简面板内不再放还原键 ——
 * 单击 = 播放/暂停（触屏 = 展开/收起面板），双击 = 还原标准条。
 * 浏览器先派 click 再派 dblclick，所以这里自己计时合并：
 * 否则双击会先切换一次播放状态，手感很脏；触屏也压根不派 dblclick。
 */
function onDiscClick() {
  if (discTapTimer !== null) {
    window.clearTimeout(discTapTimer)
    discTapTimer = null
    switchStyle('classic')
    return
  }
  discTapTimer = window.setTimeout(() => {
    discTapTimer = null
    if (window.matchMedia('(hover: none)').matches) miniOpen.value = !miniOpen.value
    else player.toggle()
  }, 240)
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

/** 声音键：静音/取消静音 + 开合竖排滑条（参考稿 volBtn 同款双动作） */
function toggleVolume() {
  volume.toggle()
  // lg 起条内就有横向滑轨，不再叠一个竖排弹层；窄屏/触屏只能靠弹层调音量
  volOpen.value = window.matchMedia('(min-width: 1024px)').matches ? false : !volOpen.value
}

/** 点到弹层外面就收起（与参考稿的 document click 一致） */
function onDocumentClick(event: MouseEvent) {
  if (!volOpen.value) return
  const target = event.target as HTMLElement | null
  if (!target?.closest('[data-testid="music-volume-wrap"]')) volOpen.value = false
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
  return track.artist?.trim() || track.album?.trim() || t('gallery.player.unknownArtist')
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

const displaySubtitle = computed(() => currentLyric.value || displayArtist.value)

/** 内联歌词区的文案：没有 LRC 时给出「无歌词」占位，不让那块空白着 */
const inlineLyric = computed(() => {
  const lines = player.state.lyrics
  if (!lines || lines.length === 0) return t('gallery.player.noLyricText')
  return currentLyric.value || lines[0]?.text?.trim() || t('gallery.player.noLyricText')
})

/** 占位文案（无歌词 / 还没开始唱）用 muted 色，真歌词才用 accent */
const inlineLyricIsPlaceholder = computed(() => {
  const lines = player.state.lyrics
  return !lines || lines.length === 0 || !currentLyric.value
})

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

onMounted(() => {
  document.addEventListener('click', onDocumentClick)
})

onBeforeUnmount(() => {
  detachVolume?.()
  document.removeEventListener('click', onDocumentClick)
  if (discTapTimer !== null) window.clearTimeout(discTapTimer)
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
      class="fixed inset-x-0 bottom-[calc(84px+env(safe-area-inset-bottom,0px))] z-40 mx-auto flex h-64 w-full max-w-md flex-col overflow-hidden rounded-2xl border border-line bg-panel/95 shadow-2xl backdrop-blur-xl lg:bottom-[calc(130px+env(safe-area-inset-bottom,0px))] lg:max-w-lg"
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
      <p class="grid flex-1 place-items-center px-6 text-center text-xs text-muted" v-else>
        {{ player.state.lyricError || t('gallery.player.noLyricText') }}
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
      :class="drawerOffset"
      class="fixed inset-x-0 z-30 flex h-[50vh] flex-col overflow-hidden rounded-t-2xl border border-line bg-panel/95 shadow-2xl backdrop-blur lg:inset-x-auto lg:right-4 lg:h-auto lg:max-h-[60vh] lg:w-80 lg:rounded-2xl"
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
    播放条（v4.5.4）：整体结构对齐参考稿 —— 进度独占顶行，下面**一行连排**：
    曲目（封面 + 歌名 + 歌手）｜ 内联歌词（sm 起有竖线分隔）｜ 控件。
    断点也跟参考稿一致：<lg 铺满底部（单行 + 浮在条外右上角的窗口键），
    lg+ 才浮起来占 60% 宽。参考稿在 <900 时也是这套「一行 + 窗口键」结构。
  -->
  <Transition name="slide-up">
    <div
      v-if="currentTrack && player.state.barStyle === 'classic'"
      class="pointer-events-none fixed inset-x-0 bottom-0 z-40 flex justify-center"
      style="padding-bottom: env(safe-area-inset-bottom, 0px)"
    >
      <div
        class="group/bar pointer-events-auto relative w-full border-t border-line bg-panel/95 backdrop-blur-2xl backdrop-saturate-150 lg:mb-6 lg:w-[60%] lg:min-w-[820px] lg:rounded-[20px] lg:border lg:shadow-[0_10px_30px_-8px_rgba(0,0,0,0.45)]"
        data-testid="music-bar"
      >
        <!--
          右上角外侧窗口键（参考稿 .window-ctrl，**两个键**）：锁定常驻 / 关闭。
          浮在条外 -14px，hover 整条、键盘聚焦或触屏（无 hover）时常显。
          锁定后关闭键置灰不可点，避免误关；条内不再挤这两个灰按钮。
        -->
        <div
          class="absolute -top-3.5 right-3 z-20 flex gap-1.5 opacity-0 transition-opacity duration-200 group-hover/bar:opacity-100 focus-within:opacity-100 lg:right-5 [@media(hover:none)]:opacity-100"
        >
          <button
            class="grid size-6 place-items-center rounded-full border bg-panel shadow-sm transition-all hover:scale-110"
            :class="
              pinned
                ? 'border-accent bg-accent/10 text-accent'
                : 'border-line text-muted hover:border-transparent hover:bg-accent hover:text-white'
            "
            :title="pinned ? t('gallery.player.unpin') : t('gallery.player.pin')"
            data-testid="music-bar-pin"
            @click="pinned = !pinned"
          >
            <AppIcon :name="pinned ? 'lock' : 'unlock'" :size="12" />
          </button>
          <button
            class="grid size-6 place-items-center rounded-full border border-line bg-panel text-muted shadow-sm transition-all"
            :class="
              pinned
                ? 'cursor-not-allowed opacity-40'
                : 'hover:scale-110 hover:border-transparent hover:bg-red-500/85 hover:text-white'
            "
            :title="pinned ? t('gallery.player.pinnedHint') : t('gallery.player.close')"
            :disabled="pinned"
            data-testid="music-bar-close"
            @click="closePlayer"
          >
            <AppIcon name="close" :size="12" />
          </button>
        </div>

        <div class="px-3 pb-2 pt-2 lg:px-6 lg:py-3.5">
          <!-- ① 全宽进度行：时间 – 细滑条 – 时长 -->
          <div class="flex items-center gap-2.5 lg:gap-3">
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

          <!-- ② 单行：曲目 ｜ 内联歌词 ｜ 控件 -->
          <div class="mt-1.5 flex items-center gap-2 lg:mt-2.5 lg:gap-3">
            <div class="flex min-w-0 flex-1 items-center gap-2 sm:flex-none lg:gap-2.5">
              <span
                class="relative grid size-8 shrink-0 place-items-center overflow-hidden rounded-lg bg-accent lg:size-10 lg:rounded-xl lg:shadow-[0_4px_12px_rgba(0,0,0,0.15)]"
              >
                <span v-if="!showCover" class="size-1.5 rounded-full bg-white/90 lg:size-2" />
                <img
                  v-else
                  :src="coverUrl(currentTrack.path)"
                  alt=""
                  class="size-full object-cover"
                  @error="coverFailed = true"
                />
              </span>
              <div class="min-w-0 flex-1 sm:max-w-[190px] sm:flex-none lg:max-w-[180px]">
                <button
                  class="block w-full truncate text-left text-[12px] font-semibold leading-[1.25] text-ink transition-colors hover:text-accent lg:text-[13px]"
                  :title="displayTitle"
                  @click="toggleLyrics"
                >
                  {{ displayTitle }}
                </button>
                <!-- 歌手：永远占一行，没有 ID3 就写「未知歌手」，不让这块空掉 -->
                <p
                  class="truncate text-[10px] leading-[1.25] text-muted sm:mt-0.5 sm:text-[11px]"
                  :title="displayArtist"
                >
                  {{ displayArtist }}
                </p>
                <!-- 窄屏没有内联歌词区，第三行补上歌词 / 「无歌词」占位 -->
                <p
                  class="truncate text-[10px] leading-[1.25] sm:hidden"
                  :class="inlineLyricIsPlaceholder ? 'text-muted' : 'text-accent/80'"
                  :title="inlineLyric"
                >
                  {{ inlineLyric }}
                </p>
              </div>
            </div>

            <!-- 内联歌词（参考稿 .lyrics-inline）：sm 起有竖线分隔，剩下的宽度全给它 -->
            <div
              class="hidden h-6 min-w-0 flex-1 items-center overflow-hidden border-l border-line pl-3 sm:flex"
              data-testid="music-bar-lyric"
            >
              <p
                class="w-full truncate text-[12px] font-medium leading-none lg:text-[13px]"
                :class="inlineLyricIsPlaceholder ? 'text-muted' : 'text-accent'"
                :title="inlineLyric"
              >
                {{ inlineLyric }}
              </p>
            </div>

            <div class="flex shrink-0 items-center gap-0.5 sm:gap-1 lg:gap-2">
              <!--
                音量（参考稿 .vol-popup）：声音键 + 上方竖排滑条。
                桌面上同时保留参考稿那根横向细轨（lg 起），窄屏只有这颗键。
              -->
              <div class="relative" data-testid="music-volume-wrap" @click.stop>
                <button
                  class="grid size-[26px] shrink-0 place-items-center rounded-full transition-colors hover:bg-elevated lg:size-6"
                  :class="volume.muted ? 'text-accent' : 'text-muted hover:text-ink'"
                  :title="volumeTitle"
                  data-testid="music-bar-volume"
                  @click="toggleVolume"
                >
                  <AppIcon :name="volumeIcon" :size="15" />
                </button>
                <Transition name="fade">
                  <div
                    v-if="volOpen"
                    class="absolute bottom-full left-1/2 mb-1 h-24 w-9 -translate-x-1/2 rounded-[18px] border border-line bg-panel shadow-2xl"
                    data-testid="music-volume-popup"
                  >
                    <div class="grid h-full w-full place-items-center">
                      <input
                        type="range"
                        min="0"
                        max="1"
                        step="0.01"
                        :value="volume.sliderValue"
                        :title="t('gallery.player.volume')"
                        class="music-range is-volume vol-vertical w-[72px]"
                        :style="{ backgroundImage: volumeBackground }"
                        data-testid="music-volume-slider"
                        @input="onVolumeInput"
                      />
                    </div>
                  </div>
                </Transition>
              </div>

              <!-- 桌面横向音量细轨（参考稿 .vol-horizontal，lg 起才有位置放） -->
              <input
                type="range"
                min="0"
                max="1"
                step="0.01"
                :value="volume.sliderValue"
                :title="volumeTitle"
                class="music-range is-volume hidden w-16 min-w-0 shrink-0 cursor-pointer lg:block"
                :style="{ backgroundImage: volumeBackground }"
                @input="onVolumeInput"
              />

              <!-- 模式键：桌面 32px 方角描边（参考稿 .mode-btn），非列表循环时 accent 高亮 -->
              <button
                class="grid size-[26px] shrink-0 place-items-center rounded-full transition-colors hover:bg-elevated lg:size-8 lg:rounded-lg lg:border lg:active:scale-90"
                :class="
                  player.state.mode === 'loop'
                    ? 'text-muted hover:text-ink lg:border-line lg:hover:border-muted/40'
                    : 'text-accent lg:border-accent lg:bg-accent/10'
                "
                :title="modeLabel"
                data-testid="music-bar-mode"
                @click="cycleMode"
              >
                <AppIcon :name="modeIcon" :size="15" />
              </button>

              <button
                class="grid size-[26px] shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink active:scale-90 lg:size-9"
                :title="t('gallery.player.prev')"
                @click="player.prev()"
              >
                <AppIcon name="prevSolid" :size="16" filled />
              </button>
              <button
                class="grid size-8 shrink-0 place-items-center rounded-full bg-accent text-white play-glow transition-all hover:scale-105 hover:brightness-110 active:scale-95 lg:size-11"
                :title="player.state.playing ? t('gallery.player.pause') : t('gallery.player.play')"
                data-testid="music-bar-play"
                @click="player.toggle()"
              >
                <AppIcon :name="playIcon" :size="15" filled />
              </button>
              <button
                class="grid size-[26px] shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink active:scale-90 lg:size-9"
                :title="t('gallery.player.next')"
                @click="player.next()"
              >
                <AppIcon name="nextSolid" :size="16" filled />
              </button>

              <!-- 歌单：参考稿没有这个入口，本项目保留（否则桌面端打不开播放列表） -->
              <button
                class="grid size-[26px] shrink-0 place-items-center rounded-full transition-colors hover:bg-elevated lg:size-8 lg:rounded-lg lg:border lg:active:scale-90"
                :class="
                  listOpen
                    ? 'text-accent lg:border-accent lg:bg-accent/10'
                    : 'text-muted hover:text-ink lg:border-line lg:hover:border-muted/40'
                "
                :title="t('gallery.player.playlist')"
                data-testid="music-bar-playlist"
                @click="toggleList"
              >
                <AppIcon name="list" :size="15" />
              </button>
              <button
                class="grid size-7 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink lg:size-8 lg:rounded-lg lg:border lg:border-line lg:hover:border-muted/40"
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
        class="pointer-events-auto absolute -top-3.5 right-2 z-20 grid size-6 place-items-center rounded-full border border-line bg-panel text-muted opacity-0 shadow-sm transition-all focus-visible:opacity-100 group-hover/mini:opacity-100 [@media(hover:none)]:opacity-100"
        :class="pinned ? 'cursor-not-allowed opacity-40' : 'hover:scale-110 hover:border-transparent hover:bg-red-500/85 hover:text-white'"
        :title="pinned ? t('gallery.player.pinnedHint') : t('gallery.player.close')"
        :disabled="pinned"
        data-testid="music-bar-close"
        @click="closePlayer"
      >
        <AppIcon name="close" :size="12" />
      </button>

      <div
        class="pointer-events-auto relative flex h-16 items-center overflow-hidden rounded-full border border-line bg-panel/95 shadow-[0_10px_30px_-10px_rgba(0,0,0,0.55)] backdrop-blur-2xl backdrop-saturate-150 transition-[width] duration-[400ms] ease-[cubic-bezier(0.22,1,0.36,1)]"
        :class="
          miniOpen
            ? 'w-[min(340px,calc(100vw-2.5rem))]'
            : 'w-16 group-hover/mini:w-[min(340px,calc(100vw-2.5rem))]'
        "
      >
        <!--
          展开面板：右侧留 64px 给唱片；按钮顺序对齐参考稿 ——
          音量 / 模式 / 上一首 / 播放 / 下一首，末尾只多留一个本项目的歌单入口。
        -->
        <div
          class="absolute inset-y-0 left-0 right-16 flex items-center pl-4 opacity-0 transition-opacity duration-200"
          :class="miniOpen ? 'opacity-100' : 'group-hover/mini:opacity-100'"
        >
          <div class="mr-1.5 min-w-0 flex-1">
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
          <div class="flex shrink-0 items-center">
            <!-- 音量：点一下静音 / 取消静音（极简面板放不下滑轨，参考稿那根竖向滑条不搬） -->
            <button
              class="grid size-6 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink"
              :title="volumeTitle"
              data-testid="music-bar-mini-volume"
              @click="volume.toggle()"
            >
              <AppIcon :name="volumeIcon" :size="14" />
            </button>
            <!-- 模式：列表 / 单曲 / 随机，非列表循环时高亮 -->
            <button
              class="grid size-6 shrink-0 place-items-center rounded-full transition-colors hover:bg-elevated"
              :class="player.state.mode === 'loop' ? 'text-muted hover:text-ink' : 'text-accent'"
              :title="modeLabel"
              data-testid="music-bar-mini-mode"
              @click="cycleMode"
            >
              <AppIcon :name="modeIcon" :size="14" />
            </button>
            <button
              class="grid size-6 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink"
              :title="t('gallery.player.prev')"
              data-testid="music-bar-mini-prev"
              @click="player.prev()"
            >
              <AppIcon name="prevSolid" :size="15" filled />
            </button>
            <button
              class="mx-0.5 grid size-7 shrink-0 place-items-center rounded-full bg-accent text-white play-glow transition-transform active:scale-95"
              :title="player.state.playing ? t('gallery.player.pause') : t('gallery.player.play')"
              data-testid="music-bar-mini-play"
              @click="player.toggle()"
            >
              <AppIcon :name="playIcon" :size="14" filled />
            </button>
            <button
              class="grid size-6 shrink-0 place-items-center rounded-full text-muted transition-colors hover:bg-elevated hover:text-ink"
              :title="t('gallery.player.next')"
              data-testid="music-bar-mini-next"
              @click="player.next()"
            >
              <AppIcon name="nextSolid" :size="15" filled />
            </button>
            <!-- 歌单：参考稿没有这个入口，本项目保留（否则极简形态下打不开列表） -->
            <button
              class="grid size-6 shrink-0 place-items-center rounded-full transition-colors hover:bg-elevated"
              :class="listOpen ? 'text-accent' : 'text-muted hover:text-ink'"
              :title="t('gallery.player.playlist')"
              data-testid="music-bar-mini-playlist"
              @click="toggleList"
            >
              <AppIcon name="list" :size="14" />
            </button>
          </div>
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
