<script setup lang="ts">
/**
 * 视频播放器（设计文档 3.4）：Video.js 10 —— HTML 自定义元素 + neutral 皮肤。
 *
 * 按官方 Vue 指南接入（https://videojs.org/docs/guides/installation/vue）：
 * - 元素由静态 import 注册，随本组件的异步 chunk 下发，只有点开视频才下载内核；
 * - 播放状态与事件直接监听内部原生 <video>（官方推荐用法，skin 只负责 UI）；
 * - 兜底：自定义元素未升级（浏览器过旧 / 模块被拦截）时退回原生 <video controls>，
 *   保证「点了就一定能播」；
 * - 续播：loadedmetadata 后跳到 startAt（> 10s 才跳，避免误跳到片尾）；
 * - 外挂字幕（设计文档 3.4）：同目录同名 .vtt/.srt 交给原生 <track>，
 *   后端已把 srt 转成 WebVTT，这里只负责挂上去并默认打开；
 * - 宽高比：容器按视频真实比例定尺寸（宽 = min(可用宽, 78vh × 比例)），
 *   竖屏视频不会被裁成上半截；<video> 显式撑满容器，不依赖皮肤内部的 ::slotted 规则。
 */
import '@videojs/html/video/player'
import '@videojs/html/video/neutral-skin'
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from './AppIcon.vue'
import { subtitleUrl } from '../api/media'
import { volume, bindVolume, applyVolume } from '../lib/volume'

const props = defineProps<{
  src: string
  poster?: string
  autoplay?: boolean
  /** 续播起点（秒） */
  startAt?: number
  /** 外挂字幕文件路径（.vtt/.srt），空则不加字幕轨 */
  subtitle?: string | null
}>()

const emit = defineEmits<{
  (e: 'close'): void
  (e: 'progress', seconds: number): void
  (e: 'ended'): void
}>()


const { t } = useI18n()
const nativeFallback = ref(false)
const errorMsg = ref('')
/** 容器比例，随视频元数据更新，避免竖屏视频被强行拉成 16:9 */
const ratio = ref(16 / 9)
/** 播放器最大高度占总视口的百分比（下方还要留说明文字） */
const MAX_VH = 78
/** 最后已知播放位置（卸载时补报一次，防止丢进度） */
let lastTime = 0

/**
 * 容器尺寸：宽度同时受「可用宽度」与「最大高度 × 视频比例」约束，高度由 aspect-ratio 推出。
 * 不能写成 width:100% + max-height —— 那样宽度不跟着收，容器比例失真，
 * 竖屏视频（如 9:16）会被 overflow 裁掉上下半截。
 */
const boxStyle = computed(() => ({
  aspectRatio: ratio.value.toFixed(5),
  width: `min(100%, calc(${MAX_VH}vh * ${ratio.value.toFixed(5)}))`,
}))

/** 常见但浏览器不支持的容器格式，用于给出可读提示而不是一片黑 */
const RISKY_EXT = ['mkv', 'avi', 'wmv', 'flv', 'rmvb', 'rm', 'ts', 'm2ts', 'vob', 'iso']

let lastEmit = 0

function extensionOf(src: string): string {
  const name = src.split('?')[0].split('/').pop() ?? ''
  return name.includes('.') ? name.split('.').pop()!.toLowerCase() : ''
}

function describeError(): string {
  const ext = extensionOf(props.src)
  if (RISKY_EXT.includes(ext)) return t('gallery.video.playerUnsupported', { ext })
  return t('gallery.video.playerLoadFailed')
}

function onLoadedMetadata(event: Event) {
  const media = event.currentTarget as HTMLVideoElement
  // 音量记忆（3.4）：元数据就绪时立刻套用全局音量，避免第一声是 100%
  attachVolume(media)
  const w = media.videoWidth
  const h = media.videoHeight
  if (w && h) {
    // 上下限只防异常值：0.25 ≈ 9:36 的超长竖屏，3 ≈ 32:9 的超宽，正常素材都在区间内
    ratio.value = Math.min(3, Math.max(0.25, w / h))
  }
  const startAt = props.startAt ?? 0
  if (startAt > 10 && Number.isFinite(media.duration) && startAt < media.duration - 5) {
    if (Math.abs(media.currentTime - startAt) > 1) media.currentTime = startAt
    emit('progress', startAt)
  }
}

function onTimeUpdate(event: Event) {
  const media = event.currentTarget as HTMLVideoElement
  if (Number.isFinite(media.currentTime)) lastTime = media.currentTime
  const now = Date.now()
  // 原生 timeupdate 约 4 次/秒，节流到 5 秒一次即可满足续播精度
  if (now - lastEmit < 5000) return
  lastEmit = now
  emit('progress', media.currentTime)
}

function onPause(event: Event) {
  const media = event.currentTarget as HTMLVideoElement
  lastEmit = Date.now()
  if (Number.isFinite(media.currentTime)) {
    lastTime = media.currentTime
    emit('progress', media.currentTime)
  }
}

/** 播完：只通知「结束」，由父层决定清零进度（避免卸载时误报 0 覆盖续播位置） */
function onEnded() {
  emit('ended')
}

/** 播放器里真正的 <video>（自定义元素内部），用于绑定全局音量 */
const root = ref<HTMLElement | null>(null)
const mediaEl = ref<HTMLVideoElement | null>(null)
let detachVolume: (() => void) | null = null

function attachVolume(element: HTMLVideoElement) {
  mediaEl.value = element
  detachVolume?.()
  detachVolume = bindVolume(element)
}

// 音量在别处被调整（音乐条/设置）时同步到当前视频
watch(
  () => [volume.state.volume, volume.state.muted],
  () => applyVolume(mediaEl.value),
)

function onError() {
  errorMsg.value = describeError()
}

onMounted(() => {
  // 自定义元素没升级（极旧浏览器 / 模块加载被拦）时，皮肤不会渲染任何控件
  if (!('customElements' in window) || !customElements.get('video-neutral-skin')) {
    nativeFallback.value = true
  }
  // 音量要在播放前就位：`<video>` 虽是自定义元素的 light DOM 子节点，
  // 但升级完成前也能直接查到，先挂一次，避免自动播放的第一声是 100%
  const element = root.value?.querySelector('video')
  if (element) attachVolume(element)
})

onBeforeUnmount(() => {
  detachVolume?.()
  // 彻底销毁：只卸载 DOM 不够，浏览器可能继续缓冲下载（设计文档 8.4.9）
  const element = mediaEl.value ?? root.value?.querySelector('video')
  if (element) {
    element.pause()
    element.removeAttribute('src')
    element.load()
  }
  // 关闭播放器时把最后已知位置交出去（>5s 才有意义，父层会过滤小值）
  if (lastTime > 5) emit('progress', lastTime)
})
</script>

<template>
  <div ref="root" class="relative w-full">
    <div
      class="relative mx-auto overflow-hidden rounded-2xl bg-black"
      :style="boxStyle"
    >
      <!-- 原生兜底：不经过皮肤，直接给浏览器控件 -->
      <video
        v-if="nativeFallback"
        :src="src"
        :poster="poster"
        controls
        playsinline
        class="size-full object-contain"
        @loadedmetadata="onLoadedMetadata"
        @timeupdate="onTimeUpdate"
        @pause="onPause"
        @ended="onEnded"
        @error="onError"
      >
        <track
          v-if="subtitle"
          kind="subtitles"
          :src="subtitleUrl(subtitle)"
          srclang="zh"
          :label="t('gallery.video.subtitleLabel')"
          default
        />
      </video>
      <video-player v-else class="size-full">
        <video-neutral-skin class="size-full">
          <video
            :src="src"
            :poster="poster"
            :autoplay="autoplay ?? true"
            playsinline
            preload="metadata"
            class="size-full object-contain"
            @loadedmetadata="onLoadedMetadata"
            @timeupdate="onTimeUpdate"
            @pause="onPause"
            @ended="onEnded"
            @error="onError"
          >
            <track
              v-if="subtitle"
              kind="subtitles"
              :src="subtitleUrl(subtitle)"
              srclang="zh"
              :label="t('gallery.video.subtitleLabel')"
              default
            />
            <a :href="src" target="_blank" rel="noopener">
              {{ t('gallery.video.fallbackLink') }}
            </a>
          </video>
        </video-neutral-skin>
      </video-player>

      <button
        class="absolute top-2 right-2 z-10 grid size-8 place-items-center rounded-full bg-black/55 text-white backdrop-blur transition-colors hover:bg-black/80"
        :title="t('gallery.video.close')"
        @click="emit('close')"
      >
        <AppIcon name="close" :size="16" />
      </button>
    </div>

    <div class="mt-2 flex flex-wrap items-center gap-x-4 gap-y-1 text-xs">
      <p v-if="errorMsg" class="text-amber-300">{{ errorMsg }}</p>
      <a
        :href="src"
        target="_blank"
        rel="noopener"
        class="text-white/60 underline transition-colors hover:text-white"
      >
        {{ t('gallery.video.openOriginal') }}
      </a>
      <span class="ml-auto text-white/35">Video.js 10 · neutral</span>
    </div>
  </div>
</template>
