<script setup lang="ts">
/**
 * 系统设置（设计文档 3.5）：按功能分 Tab —— 通用 / 媒体 / 安全 / 高级 / 生态推荐。
 *
 * 三条硬约束：
 * 1. `[[system.mounts]]` 与 `[[services]]` 属于高级配置，**绝不**被「恢复当前面板默认值」覆盖，
 *    只能在这里手工编辑或走源码模式；
 * 2. 「恢复默认」是分区块的，且**永远不碰** SQLite（用户、会话、分享都保留）；
 * 3. 界面文本一律走 `$t()`（设计文档 1.4 / 3.8）。
 */
import { computed, defineAsyncComponent, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from '../components/AppIcon.vue'
import { authApi } from '../api/auth'
import { cacheApi, type CacheStats } from '../api/media'
import { systemApi, type FfmpegInfo, type FfmpegSource } from '../api/system'
import { ApiError, readApiError } from '../lib/errors'
import { formatSize } from '../lib/format'
import { useTheme, type ThemeMode } from '../composables/useTheme'
import { applyLocale, detectLocale, SUPPORTED_LOCALES, type Locale } from '../lib/i18n'
import { ECOSYSTEM } from '../constants/ecosystem'

const CodeEditor = defineAsyncComponent(() => import('../components/CodeEditor.vue'))

const { t, locale } = useI18n()
const { mode: themeMode, setTheme, isDark } = useTheme()

interface Mount {
  name: string
  path: string
}

interface Service {
  id: string
  name: string
  icon?: string
  type: string
  target_url: string
  strip_prefix: boolean
  rewrite_html: boolean
  insecure_skip_verify: boolean
  inject_headers: Record<string, string>
}

interface MediaSettings {
  thumb_cache_max_size_mb: number
  thumb_cache_max_age_days: number
  ffmpeg_path: string
}

interface AppConfig {
  server: {
    port: number
    ssl: { enabled: boolean; cert_path: string; key_path: string }
    proxy: { trust_proxy_headers: boolean; trusted_ips: string[]; base_url: string }
  }
  auth: {
    local_enabled: boolean
    proxy_trust: { enabled: boolean; user_header: string }
  }
  system: { mounts: Mount[] }
  services: Service[]
  media: MediaSettings
}

/** 媒体面板默认值，与后端 `MediaConfig::default()` 保持一致 */
const MEDIA_DEFAULTS: MediaSettings = {
  thumb_cache_max_size_mb: 1024,
  thumb_cache_max_age_days: 90,
  ffmpeg_path: '',
}

type Tab = 'general' | 'media' | 'security' | 'advanced' | 'ecosystem'
const tabs: { key: Tab; label: string }[] = [
  { key: 'general', label: 'settings.tabs.general' },
  { key: 'media', label: 'settings.tabs.media' },
  { key: 'security', label: 'settings.tabs.security' },
  { key: 'advanced', label: 'settings.tabs.advanced' },
  { key: 'ecosystem', label: 'settings.tabs.ecosystem' },
]
const tab = ref<Tab>('general')

// ---------------- 通用：主题 / 语言 ----------------
const themes: { key: ThemeMode; label: string }[] = [
  { key: 'dark', label: 'settings.general.themeDark' },
  { key: 'light', label: 'settings.general.themeLight' },
  { key: 'system', label: 'settings.general.themeSystem' },
]

function pickLocale(next: Locale) {
  applyLocale(next)
}

/** 通用面板复位：主题回「跟随系统」（8.2 默认值）、语言回到浏览器识别结果 */
function resetGeneral() {
  setTheme('system')
  applyLocale(detectLocale())
  showToast(t('settings.general.resetDone'))
}

// ---------------- 配置（表单 + 源码双模） ----------------
const config = ref<AppConfig | null>(null)
const draft = ref<AppConfig | null>(null)
const loading = ref(true)
const error = ref('')
const toast = ref('')

const saving = ref(false)
const configMode = ref<'form' | 'source'>('form')
const rawText = ref('')
const rawLoading = ref(false)
const rawDirty = ref(false)

// ---------------- 媒体：缓存 + FFmpeg ----------------
const cache = ref<CacheStats | null>(null)
const cacheBusy = ref(false)
const ffmpeg = ref<FfmpegInfo | null>(null)
const ffmpegTesting = ref(false)

const ffmpegBadge = computed(() => {
  const info = ffmpeg.value
  if (!info) return { text: '…', ok: false }
  return info.status === 'ok'
    ? { text: t('settings.media.ffmpegDetected', { version: info.version ?? '?' }), ok: true }
    : { text: t('settings.media.ffmpegMissing'), ok: false }
})

const ffmpegSourceLabel = computed(() => {
  const source = (ffmpeg.value?.source ?? 'none') as FfmpegSource
  const map: Record<FfmpegSource, string> = {
    config: t('settings.media.sourceConfig'),
    env: t('settings.media.sourceEnv'),
    path: t('settings.media.sourcePath'),
    none: t('settings.media.sourceNone'),
  }
  return map[source]
})

async function loadCache() {
  try {
    cache.value = await cacheApi.stats()
  } catch {
    cache.value = null
  }
}

async function loadFfmpeg() {
  try {
    ffmpeg.value = await systemApi.ffmpegStatus()
  } catch {
    ffmpeg.value = null
  }
}

async function clearCache() {
  if (cacheBusy.value) return
  cacheBusy.value = true
  try {
    const result = await cacheApi.clear()
    showToast(
      t('settings.media.cleared', { files: result.files, size: formatSize(result.bytes) }),
    )
    await loadCache()
  } catch (err) {
    showToast(err instanceof Error ? err.message : t('settings.media.clearFailed'))
  } finally {
    cacheBusy.value = false
  }
}

/** 测试连通性：只测输入框里这一条，不回落（否则「该路径不可用」就不准了） */
async function testFfmpeg() {
  const path = draft.value?.media.ffmpeg_path.trim() ?? ''
  if (!path) {
    showToast(t('settings.media.testEmpty'))
    return
  }
  ffmpegTesting.value = true
  try {
    const info = await systemApi.testFfmpeg(path)
    if (info.status === 'ok') {
      showToast(t('settings.media.testOk', { version: info.version ?? '?' }))
    } else {
      showToast(t('settings.media.testFailed'))
    }
  } catch (err) {
    showToast(err instanceof Error ? err.message : t('settings.media.testFailed'))
  } finally {
    ffmpegTesting.value = false
  }
}

/** 媒体面板复位：只动 `[media]`，挂载点与服务原样保留 */
async function resetMedia() {
  if (!draft.value) return
  draft.value.media = { ...MEDIA_DEFAULTS }
  await saveForm(t('settings.media.resetDone'))
  await loadFfmpeg()
}

// ---------------- 安全 ----------------
const currentPassword = ref('')
const nextPassword = ref('')
const confirmPassword = ref('')
const securityError = ref('')
const securityBusy = ref(false)
const totpSecret = ref('')
const totpUri = ref('')
const totpCode = ref('')
const totpEnabled = computed(() => authApi.status.value.totp_enabled)
const disablePassword = ref('')

// ---------------- WebDAV 令牌 ----------------
interface AccessToken {
  id: number
  name: string
  readonly: boolean
  expires_at: number | null
}

const tokens = ref<AccessToken[]>([])
const tokenName = ref('')
const tokenReadonly = ref(false)
const tokenDays = ref(0)
const newToken = ref('')
const tokenError = ref('')

const davUrl = `${window.location.origin}/dav`

function showToast(message: string) {
  toast.value = message
  window.setTimeout(() => {
    if (toast.value === message) toast.value = ''
  }, 2400)
}

async function loadTokens() {
  try {
    const res = await fetch('/api/tokens')
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    tokens.value = ((await res.json()) as { items: AccessToken[] }).items
  } catch (err) {
    tokenError.value = err instanceof Error ? err.message : t('settings.security.tokenLoadFailed')
  }
}

async function createToken() {
  tokenError.value = ''
  if (!tokenName.value.trim()) {
    tokenError.value = t('settings.security.tokenNameRequired')
    return
  }
  try {
    const res = await fetch('/api/tokens', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        name: tokenName.value.trim(),
        readonly: tokenReadonly.value,
        days: tokenDays.value,
      }),
    })
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    const data = (await res.json()) as { token: string }
    newToken.value = data.token
    tokenName.value = ''
    await loadTokens()
  } catch (err) {
    tokenError.value = err instanceof Error ? err.message : t('settings.security.tokenCreateFailed')
  }
}

async function deleteToken(id: number) {
  try {
    const res = await fetch(`/api/tokens/${id}`, { method: 'DELETE' })
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    await loadTokens()
  } catch (err) {
    tokenError.value = err instanceof Error ? err.message : t('settings.security.tokenDeleteFailed')
  }
}

function formatExpiry(value: number | null) {
  if (!value) return t('common.time.permanent')
  return new Date(value * 1000).toLocaleDateString()
}

async function loadConfig() {
  loading.value = true
  error.value = ''
  try {
    const res = await fetch('/api/config')
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    const data = (await res.json()) as AppConfig
    config.value = data
    draft.value = structuredClone(data)
  } catch (err) {
    error.value = err instanceof Error ? err.message : t('settings.advanced.loadFailed')
  } finally {
    loading.value = false
  }
}

async function loadRaw() {
  rawLoading.value = true
  try {
    const res = await fetch('/api/config/raw')
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    rawText.value = await res.text()
    rawDirty.value = false
  } catch (err) {
    error.value = err instanceof Error ? err.message : t('settings.advanced.rawLoadFailed')
  } finally {
    rawLoading.value = false
  }
}

async function switchMode(mode: 'form' | 'source') {
  if (mode === configMode.value) return
  if (mode === 'source' && !rawText.value) await loadRaw()
  configMode.value = mode
}

function onRawChange(value: string) {
  rawText.value = value
  rawDirty.value = true
}

/** 保存整份配置（表单模式）。媒体面板的复位也走这里，保证只改 `[media]`。 */
async function saveForm(successMessage?: string) {
  if (!draft.value) return
  saving.value = true
  error.value = ''
  try {
    const res = await fetch('/api/config', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(draft.value),
    })
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    showToast(successMessage ?? t('settings.advanced.saved'))
    await loadConfig()
    rawText.value = ''
  } catch (err) {
    error.value = err instanceof Error ? err.message : t('settings.advanced.saveFailed')
  } finally {
    saving.value = false
  }
}

async function saveRaw() {
  saving.value = true
  error.value = ''
  try {
    const res = await fetch('/api/config/raw', {
      method: 'POST',
      headers: { 'Content-Type': 'text/plain; charset=utf-8' },
      body: rawText.value,
    })
    if (!res.ok) throw new ApiError(res.status, await readApiError(res))
    rawDirty.value = false
    showToast(t('settings.advanced.rawSaved'))
    await loadConfig()
    await loadFfmpeg()
  } catch (err) {
    error.value = err instanceof Error ? err.message : t('settings.advanced.saveFailed')
  } finally {
    saving.value = false
  }
}

function addMount() {
  draft.value?.system.mounts.push({ name: t('settings.advanced.newMount'), path: '' })
}

function removeMount(index: number) {
  draft.value?.system.mounts.splice(index, 1)
}

function addService() {
  draft.value?.services.push({
    id: `service-${Date.now().toString(36)}`,
    name: t('settings.advanced.newService'),
    icon: 'grid',
    type: 'external',
    target_url: '',
    strip_prefix: true,
    rewrite_html: false,
    insecure_skip_verify: false,
    inject_headers: {},
  })
}

function removeService(index: number) {
  draft.value?.services.splice(index, 1)
}

const injectKey = (service: Service) => Object.keys(service.inject_headers)[0] ?? ''
const injectValue = (service: Service) => Object.values(service.inject_headers)[0] ?? ''

function setInject(service: Service, key: string, value: string) {
  service.inject_headers = key.trim() ? { [key.trim()]: value } : {}
}

async function changePassword() {
  securityError.value = ''
  if (nextPassword.value !== confirmPassword.value) {
    securityError.value = t('settings.security.mismatch')
    return
  }
  securityBusy.value = true
  try {
    await authApi.changePassword(currentPassword.value, nextPassword.value)
    currentPassword.value = ''
    nextPassword.value = ''
    confirmPassword.value = ''
    showToast(t('settings.security.changed'))
  } catch (err) {
    securityError.value = err instanceof Error ? err.message : t('settings.security.failed')
  } finally {
    securityBusy.value = false
  }
}

async function startTotp() {
  securityError.value = ''
  try {
    const data = await authApi.totpSetup()
    totpSecret.value = data.secret
    totpUri.value = data.uri
  } catch (err) {
    securityError.value = err instanceof Error ? err.message : t('settings.security.totpGenerateFailed')
  }
}

async function enableTotp() {
  securityError.value = ''
  try {
    await authApi.totpEnable(totpCode.value.trim())
    totpCode.value = ''
    totpSecret.value = ''
    showToast(t('settings.security.totpEnabled'))
  } catch (err) {
    securityError.value = err instanceof Error ? err.message : t('settings.security.totpVerifyFailed')
  }
}

async function disableTotp() {
  securityError.value = ''
  try {
    await authApi.totpDisable(disablePassword.value)
    disablePassword.value = ''
    showToast(t('settings.security.totpDisabled'))
  } catch (err) {
    securityError.value = err instanceof Error ? err.message : t('settings.security.totpDisableFailed')
  }
}

/** 运行概览：只读快照，不承载任何编辑入口（编辑在「高级」） */
const summary = computed(() => {
  const cfg = config.value
  if (!cfg) return []
  return [
    { label: t('settings.general.port'), value: String(cfg.server.port) },
    {
      label: t('settings.general.baseUrl'),
      value: cfg.server.proxy.base_url || t('settings.general.rootPath'),
    },
    {
      label: t('settings.general.trustProxy'),
      value: cfg.server.proxy.trust_proxy_headers ? t('common.state.yes') : t('common.state.no'),
    },
    {
      label: t('settings.general.localAuth'),
      value: cfg.auth.local_enabled ? t('common.state.enabled') : t('common.state.disabled'),
    },
    {
      label: t('settings.general.proxyAuth'),
      value: cfg.auth.proxy_trust.enabled ? t('common.state.enabled') : t('common.state.disabled'),
    },
    { label: t('settings.general.mounts'), value: t('settings.general.count', { count: cfg.system.mounts.length }) },
    {
      label: t('settings.general.services'),
      value: t('settings.general.count', { count: cfg.services.length }),
    },
  ]
})

watch(tab, (value) => {
  if (value === 'security' && tokens.value.length === 0) void loadTokens()
  if (value === 'media') void loadFfmpeg()
})

// ---------------- 危险配置自检警告条（设计文档 3.5 / 9.3） ----------------
interface SafetyIssue {
  level: string
  code: string
}
const safetyIssues = ref<SafetyIssue[]>([])

async function loadSafety() {
  try {
    const res = await fetch('/api/config/safety')
    if (!res.ok) return
    const data = (await res.json()) as { issues: SafetyIssue[] }
    safetyIssues.value = data.issues
  } catch {
    // 警告条拉取失败不打扰用户
  }
}

// ---------------- WebDAV 地址复制（8.1.2：只读框 + 复制按钮） ----------------
const davCopied = ref(false)
let davCopyTimer: number | null = null

async function copyDavUrl() {
  try {
    await navigator.clipboard.writeText(davUrl)
    davCopied.value = true
    if (davCopyTimer !== null) window.clearTimeout(davCopyTimer)
    davCopyTimer = window.setTimeout(() => {
      davCopied.value = false
    }, 2000)
  } catch {
    // 剪贴板不可用（非安全上下文）
  }
}

onMounted(() => {
  void loadConfig()
  void loadCache()
  void loadFfmpeg()
  void loadSafety()
})
</script>

<template>
  <section class="mx-auto w-full max-w-5xl px-4 py-6 md:px-8 md:py-10">
    <!-- 分区切换 -->
    <nav class="mb-6 flex flex-wrap items-center gap-4 border-b border-line pb-2 text-sm">
      <button
        v-for="item in tabs"
        :key="item.key"
        class="transition-colors"
        :class="
          tab === item.key
            ? 'border-b-2 border-accent pb-2 font-medium text-ink'
            : 'pb-2 text-muted hover:text-ink'
        "
        @click="tab = item.key"
      >
        {{ t(item.label) }}
      </button>
      <span class="ml-auto text-[12px] text-muted">{{ authApi.username.value }}</span>
    </nav>

    <p
      v-if="error"
      class="mb-4 rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs fg-danger"
    >
      {{ error }}
    </p>

    <!-- 危险配置自检警告条（设计文档 3.5 / 9.3） -->
    <p
      v-for="issue in safetyIssues"
      :key="issue.code"
      class="mb-4 rounded-lg px-3 py-2 text-xs"
      :class="
        issue.level === 'error'
          ? 'border border-red-500/30 bg-red-500/10 fg-danger'
          : 'border border-amber-500/30 bg-amber-500/10 fg-warn'
      "
      :data-safety="issue.code"
    >
      {{ t(`settings.safety.${issue.code}`) }}
    </p>

    <!-- ================= 通用 ================= -->
    <div v-if="tab === 'general'" class="space-y-6">
      <div class="rounded-xl border border-line p-6">
        <p class="mb-3 text-xs font-semibold tracking-wider text-muted uppercase">
          {{ t('settings.general.appearance') }}
        </p>
        <div class="grid gap-3 sm:grid-cols-2">
          <div>
            <span class="mb-1 block text-[12px] text-muted">{{ t('settings.general.theme') }}</span>
            <select
              :value="themeMode"
              class="h-10 w-full rounded-lg border border-line bg-elevated px-3 text-sm text-ink outline-none focus:border-accent"
              data-testid="theme-select"
              @change="setTheme(($event.target as HTMLSelectElement).value as ThemeMode)"
            >
              <option v-for="item in themes" :key="item.key" :value="item.key">
                {{ t(item.label) }}
              </option>
            </select>
          </div>
          <div>
            <span class="mb-1 block text-[12px] text-muted">{{
              t('settings.general.language')
            }}</span>
            <select
              :value="locale"
              class="w-full rounded-lg h-10 border border-line bg-elevated px-3 text-sm text-ink outline-none focus:border-accent"
              @change="pickLocale(($event.target as HTMLSelectElement).value as Locale)"
            >
              <option v-for="item in SUPPORTED_LOCALES" :key="item" :value="item">
                {{ t(`common.language.${item}`) }}
              </option>
            </select>
          </div>
        </div>
        <p class="mt-2 text-[12px] text-muted">{{ t('settings.general.languageHint') }}</p>
      </div>

      <div class="rounded-xl border border-line p-6">
        <div class="mb-3 flex items-center justify-between">
          <p class="text-xs font-semibold tracking-wider text-muted uppercase">
            {{ t('settings.general.overview') }}
          </p>
          <button class="text-[12px] text-accent hover:underline" @click="tab = 'advanced'">
            {{ t('settings.advanced.form') }} →
          </button>
        </div>
        <div v-if="loading" class="space-y-2">
          <div v-for="i in 5" :key="i" class="skeleton h-8 rounded-lg" />
        </div>
        <div v-else class="overflow-hidden rounded-lg border border-line/60">
          <div
            v-for="(row, index) in summary"
            :key="row.label"
            class="flex items-center justify-between gap-4 px-3 py-2 text-sm"
            :class="index % 2 === 1 ? 'bg-elevated/40' : ''"
          >
            <span class="text-muted">{{ row.label }}</span>
            <span class="truncate font-mono text-[14px] text-ink">{{ row.value }}</span>
          </div>
        </div>
      </div>

      <!-- 生态推荐入口 -->
      <button
        class="glass-card flex w-full items-center gap-3 rounded-xl px-4 py-3 text-left"
        @click="tab = 'ecosystem'"
      >
        <span class="grid size-9 shrink-0 place-items-center rounded-lg bg-accent-soft text-accent">
          <AppIcon name="sparkle" :size="16" />
        </span>
        <span class="min-w-0 flex-1">
          <span class="block text-sm font-medium text-ink">{{ t('ecosystem.title') }}</span>
          <span class="mt-0.5 block truncate text-[12px] text-muted">{{ t('ecosystem.intro') }}</span>
        </span>
        <AppIcon name="arrowRight" :size="14" class="shrink-0 text-muted" />
      </button>

      <div class="flex flex-wrap items-center justify-between gap-3 rounded-xl border border-line p-6">
        <div class="min-w-0">
          <p class="text-[12px] text-muted">{{ t('settings.general.resetHint') }}</p>
        </div>
        <button
          class="shrink-0 rounded-lg border border-line px-3 py-1.5 text-xs text-ink transition-colors hover:border-accent/50"
          @click="resetGeneral"
        >
          {{ t('settings.general.reset') }}
        </button>
      </div>
    </div>

    <!-- ================= 媒体 ================= -->
    <div v-else-if="tab === 'media'" class="space-y-6">
      <div class="rounded-xl border border-line p-6">
        <div class="mb-3 flex items-center justify-between gap-3">
          <p class="text-xs font-semibold tracking-wider text-muted uppercase">
            {{ t('settings.media.cache') }}
          </p>
          <button
            class="shrink-0 rounded-lg border border-line px-3 py-1.5 text-xs text-ink transition-colors hover:border-accent/50 disabled:opacity-50"
            :disabled="cacheBusy"
            @click="clearCache"
          >
            {{ cacheBusy ? t('settings.media.clearing') : t('settings.media.clear') }}
          </button>
        </div>
        <p class="text-xs text-muted">
          {{
            cache
              ? t('settings.media.cacheUsage', { size: formatSize(cache.bytes), files: cache.files })
              : '…'
          }}
          <span v-if="cache">
            · {{ t('settings.media.cacheLimit', { mb: cache.limit_mb, days: cache.max_age_days }) }}
          </span>
        </p>
        <p class="mt-1 text-[12px] text-muted/70">{{ t('settings.media.cacheHint') }}</p>

        <div v-if="draft" class="mt-3 grid gap-3 sm:grid-cols-2">
          <label class="block">
            <span class="mb-1 block text-[12px] text-muted">{{ t('settings.media.thumbSize') }}</span>
            <input
              v-model.number="draft.media.thumb_cache_max_size_mb"
              type="number"
              min="16"
              class="w-full rounded-lg h-10 border border-line bg-elevated px-3 text-sm text-ink outline-none focus:border-accent"
            />
          </label>
          <label class="block">
            <span class="mb-1 block text-[12px] text-muted">{{ t('settings.media.thumbAge') }}</span>
            <input
              v-model.number="draft.media.thumb_cache_max_age_days"
              type="number"
              min="1"
              class="w-full rounded-lg h-10 border border-line bg-elevated px-3 text-sm text-ink outline-none focus:border-accent"
            />
          </label>
        </div>
      </div>

      <!-- FFmpeg：三层优先级 + 状态检测 + 测试连通性 -->
      <div class="rounded-xl border border-line p-6">
        <div class="mb-3 flex flex-wrap items-center gap-2">
          <p class="text-xs font-semibold tracking-wider text-muted uppercase">
            {{ t('settings.media.ffmpeg') }}
          </p>
          <span
            class="rounded-full px-2 py-0.5 text-[11px]"
            :class="ffmpegBadge.ok ? 'bg-emerald-500/15 fg-ok' : 'bg-elevated text-muted'"
          >
            {{ ffmpegBadge.text }}
          </span>
          <span v-if="ffmpeg?.path" class="truncate font-mono text-[11px] text-muted">
            {{ ffmpeg.path }}
          </span>
          <span class="text-[11px] text-muted/70">
            {{ t('settings.media.ffmpegSource', { source: ffmpegSourceLabel }) }}
          </span>
          <!-- libwebp 支持探测（设计文档 3.5）：决定缩略图能否输出 WebP -->
          <span
            v-if="ffmpeg?.status === 'ok'"
            class="rounded-full px-2 py-0.5 text-[11px]"
            :class="ffmpeg.webp_support ? 'bg-sky-500/15 fg-info' : 'bg-elevated text-muted'"
            data-testid="webp-badge"
          >
            {{ ffmpeg.webp_support ? t('settings.media.webpOn') : t('settings.media.webpOff') }}
          </span>
        </div>

        <label class="block">
          <span class="mb-1 block text-[12px] text-muted">{{ t('settings.media.ffmpegPath') }}</span>
          <div class="flex gap-2">
            <input
              v-if="draft"
              v-model="draft.media.ffmpeg_path"
              placeholder="/usr/bin/ffmpeg 或 C:/AimerFiles/ffmpeg-n9.0/bin"
              class="min-w-0 flex-1 rounded-lg h-10 border border-line bg-elevated px-3 font-mono text-xs text-ink outline-none focus:border-accent"
            />
            <button
              class="shrink-0 rounded-lg border border-line px-3 py-1.5 text-xs text-ink transition-colors hover:border-accent/50 disabled:opacity-50"
              :disabled="ffmpegTesting"
              @click="testFfmpeg"
            >
              {{ ffmpegTesting ? t('settings.media.testing') : t('settings.media.test') }}
            </button>
          </div>
        </label>
        <p class="mt-2 text-[12px] text-muted">{{ t('settings.media.ffmpegPathHint') }}</p>
        <p class="mt-1 text-[12px] fg-warn/80">{{ t('settings.media.note') }}</p>
      </div>

      <div class="flex flex-wrap items-center justify-between gap-3 rounded-xl border border-line p-6">
        <p class="text-[12px] text-muted">{{ t('settings.media.resetHint') }}</p>
        <div class="flex shrink-0 items-center gap-2">
          <button
            class="rounded-lg border border-line px-3 py-1.5 text-xs text-ink transition-colors hover:border-accent/50"
            @click="resetMedia"
          >
            {{ t('settings.media.reset') }}
          </button>
          <button
            class="h-10 rounded-md bg-accent px-4 text-xs text-white disabled:opacity-50"
            :disabled="saving"
            @click="saveForm()"
          >
            {{ t('common.action.save') }}
          </button>
        </div>
      </div>
    </div>

    <!-- ================= 安全 ================= -->
    <div v-else-if="tab === 'security'" class="space-y-6">
      <div class="rounded-xl border border-line p-6">
        <p class="mb-3 text-xs font-semibold tracking-wider text-muted uppercase">
          {{ t('settings.security.password') }}
        </p>
        <div class="grid gap-4 md:grid-cols-3">
          <input
            v-model="currentPassword"
            type="password"
            :placeholder="t('settings.security.current')"
            class="rounded-lg h-10 border border-line bg-elevated px-3 text-sm text-ink outline-none focus:border-accent"
          />
          <input
            v-model="nextPassword"
            type="password"
            :placeholder="t('settings.security.next')"
            class="rounded-lg h-10 border border-line bg-elevated px-3 text-sm text-ink outline-none focus:border-accent"
          />
          <input
            v-model="confirmPassword"
            type="password"
            :placeholder="t('settings.security.confirm')"
            class="rounded-lg h-10 border border-line bg-elevated px-3 text-sm text-ink outline-none focus:border-accent"
          />
        </div>
        <div class="mt-3 flex items-center gap-3">
          <button
            class="h-10 rounded-md bg-accent px-4 text-xs text-white disabled:opacity-50"
            :disabled="securityBusy"
            @click="changePassword"
          >
            {{ t('settings.security.submit') }}
          </button>
          <span class="text-[12px] text-muted">{{ t('settings.security.hint') }}</span>
        </div>
      </div>

      <div class="rounded-xl border border-line p-6">
        <div class="mb-3 flex items-center gap-2">
          <p class="text-xs font-semibold tracking-wider text-muted uppercase">
            {{ t('settings.security.totp') }}
          </p>
          <span
            class="rounded-full px-2 py-0.5 text-[11px]"
            :class="totpEnabled ? 'bg-emerald-500/15 fg-ok' : 'bg-elevated text-muted'"
          >
            {{ totpEnabled ? t('settings.security.totpOn') : t('settings.security.totpOff') }}
          </span>
        </div>

        <template v-if="!totpEnabled">
          <p class="mb-2 text-[12px] text-muted">{{ t('settings.security.totpHint') }}</p>
          <button class="rounded-md border border-line px-3 py-1.5 text-xs text-ink" @click="startTotp">
            {{ t('settings.security.totpGenerate') }}
          </button>
          <div v-if="totpSecret" class="mt-3 space-y-2">
            <p class="font-mono text-xs break-all text-ink">{{ totpSecret }}</p>
            <p class="font-mono text-[11px] break-all text-muted">{{ totpUri }}</p>
            <div class="flex gap-2">
              <input
                v-model="totpCode"
                maxlength="6"
                :placeholder="t('settings.security.totpCode')"
                class="w-32 rounded-lg border border-line bg-elevated px-2 py-1.5 text-center text-sm tracking-[0.25em] text-ink outline-none focus:border-accent"
              />
              <button class="h-10 rounded-md bg-accent px-4 text-xs text-white" @click="enableTotp">
                {{ t('settings.security.totpEnable') }}
              </button>
            </div>
          </div>
        </template>

        <template v-else>
          <div class="flex flex-wrap items-center gap-2">
            <input
              v-model="disablePassword"
              type="password"
              :placeholder="t('settings.security.totpDisablePlaceholder')"
              class="w-48 rounded-lg border border-line bg-elevated px-2 py-1.5 text-xs text-ink outline-none focus:border-accent"
            />
            <button
              class="rounded-md border border-red-500/40 px-3 py-1.5 text-xs fg-danger"
              @click="disableTotp"
            >
              {{ t('settings.security.totpDisable') }}
            </button>
          </div>
        </template>
      </div>

      <p
        v-if="securityError"
        class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs fg-danger"
      >
        {{ securityError }}
      </p>

      <!-- WebDAV 令牌 -->
      <div class="rounded-xl border border-line p-6">
        <p class="mb-1 text-xs font-semibold tracking-wider text-muted uppercase">
          {{ t('settings.security.davUrl') }}
        </p>
        <div class="mb-2 flex items-center gap-2">
          <code
            class="min-w-0 flex-1 truncate rounded-md bg-elevated px-3 py-2 font-mono text-xs text-ink"
            :title="davUrl"
          >{{ davUrl }}</code>
          <button
            class="grid h-10 w-10 shrink-0 place-items-center rounded-md border transition-colors"
            :class="
              davCopied
                ? 'border-emerald-500/40 fg-ok'
                : 'border-line text-muted hover:text-ink'
            "
            :title="davCopied ? t('settings.security.davCopied') : t('settings.security.copyDav')"
            data-testid="copy-dav-url"
            @click="copyDavUrl"
          >
            <AppIcon :name="davCopied ? 'check' : 'copy'" :size="14" />
          </button>
        </div>
        <p class="text-[12px] leading-relaxed text-muted">{{ t('settings.security.davHint') }}</p>
      </div>

      <div class="rounded-xl border border-line p-6">
        <p class="mb-3 text-xs font-semibold tracking-wider text-muted uppercase">
          {{ t('settings.security.createToken') }}
        </p>
        <div class="flex flex-wrap items-center gap-4">
          <input
            v-model="tokenName"
            :placeholder="t('settings.security.tokenName')"
            class="h-10 w-40 rounded-lg border border-line bg-elevated px-3 text-xs text-ink outline-none focus:border-accent"
          />
          <label class="flex items-center gap-1 text-[12px] text-muted">
            <input v-model="tokenReadonly" type="checkbox" /> {{ t('settings.security.readonly') }}
          </label>
          <label class="flex items-center gap-1 text-[12px] text-muted">
            {{ t('settings.security.tokenDays') }}
            <input
              v-model.number="tokenDays"
              type="number"
              min="0"
              class="h-10 w-16 rounded border border-line bg-elevated px-2 text-xs text-ink outline-none focus:border-accent"
            />
            {{ t('settings.security.tokenDaysUnit') }}
          </label>
          <button class="h-10 rounded-md bg-accent px-4 text-xs text-white" @click="createToken">
            {{ t('settings.security.generate') }}
          </button>
        </div>

        <div v-if="newToken" class="mt-3 rounded-lg border border-amber-500/40 bg-amber-500/10 p-3">
          <p class="mb-1 text-[12px] font-medium fg-warn">
            {{ t('settings.security.tokenOnce') }}
          </p>
          <p class="font-mono text-xs break-all text-ink select-all">{{ newToken }}</p>
        </div>
      </div>

      <div class="overflow-hidden rounded-xl border border-line">
        <!-- 表头与数据列严格对齐，行高统一 h-12（8.4.2） -->
        <div
          class="flex h-10 items-center gap-4 border-b border-line bg-elevated/60 px-4 text-[12px] font-medium text-muted"
        >
          <span class="min-w-0 flex-1">{{ t('settings.security.colName') }}</span>
          <span class="w-14 shrink-0 text-center">{{ t('settings.security.colPerm') }}</span>
          <span class="w-24 shrink-0 text-right">{{ t('settings.security.colExpiry') }}</span>
          <span class="w-6 shrink-0"></span>
        </div>
        <div
          v-for="(token, index) in tokens"
          :key="token.id"
          class="flex h-12 items-center gap-4 px-4 text-xs"
          :class="index % 2 === 1 ? 'bg-elevated/40' : ''"
        >
          <span class="min-w-0 flex-1 truncate text-ink">{{ token.name }}</span>
          <span class="w-14 shrink-0 rounded bg-elevated px-1.5 py-0.5 text-center text-[11px] text-muted">
            {{ token.readonly ? t('settings.security.readonly') : t('settings.security.readwrite') }}
          </span>
          <span class="w-24 shrink-0 text-right text-[12px] text-muted">{{
            formatExpiry(token.expires_at)
          }}</span>
          <!-- 危险操作固定在行末，hover 变红（8.4.2） -->
          <button
            class="flex w-6 shrink-0 justify-end text-muted transition-colors hover-danger"
            :title="t('settings.security.deleteToken')"
            @click="deleteToken(token.id)"
          >
            <AppIcon name="close" :size="14" />
          </button>
        </div>
        <p v-if="!tokens.length" class="px-4 py-3 text-[12px] text-muted">
          {{ t('settings.security.tokenEmpty') }}
        </p>
      </div>

      <p
        v-if="tokenError"
        class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs fg-danger"
      >
        {{ tokenError }}
      </p>

      <div class="rounded-xl border border-line p-6 text-[12px] leading-relaxed text-muted">
        <p class="mb-1 font-medium text-ink">{{ t('settings.security.forgotTitle') }}</p>
        {{ t('settings.security.forgotBody') }}
        <code class="rounded bg-elevated px-1 py-0.5">asmoday --reset-password</code>
        {{ t('settings.security.forgotBody2') }}
      </div>
    </div>

    <!-- ================= 高级 ================= -->
    <div v-else-if="tab === 'advanced'" class="space-y-6">
      <div class="flex flex-wrap items-center gap-2">
        <div class="flex items-center rounded-lg border border-line p-0.5 text-xs">
          <button
            class="rounded px-2.5 py-1 transition-colors"
            :class="configMode === 'form' ? 'bg-accent-soft text-accent' : 'text-muted hover:text-ink'"
            @click="switchMode('form')"
          >
            {{ t('settings.advanced.form') }}
          </button>
          <button
            class="rounded px-2.5 py-1 transition-colors"
            :class="configMode === 'source' ? 'bg-accent-soft text-accent' : 'text-muted hover:text-ink'"
            @click="switchMode('source')"
          >
            {{ t('settings.advanced.source') }}
          </button>
        </div>
        <span class="text-[12px] text-muted">
          {{
            configMode === 'form'
              ? t('settings.advanced.formHint')
              : t('settings.advanced.sourceHint')
          }}
        </span>

        <button
          v-if="configMode === 'form'"
          class="ml-auto flex items-center gap-1.5 h-10 rounded-md bg-accent px-4 text-xs text-white disabled:opacity-50"
          :disabled="saving"
          @click="saveForm()"
        >
          <AppIcon name="save" :size="13" />
          {{ t('settings.advanced.save') }}
        </button>
        <button
          v-else
          class="ml-auto flex items-center gap-1.5 h-10 rounded-md bg-accent px-4 text-xs text-white disabled:opacity-50"
          :disabled="saving || !rawDirty"
          @click="saveRaw"
        >
          <AppIcon name="save" :size="13" />
          {{ t('settings.advanced.write') }}
        </button>
      </div>

      <!-- 表单模式 -->
      <div v-if="configMode === 'form' && draft" class="space-y-6">
        <div class="rounded-xl border border-line p-6">
          <p class="mb-3 text-xs font-semibold tracking-wider text-muted uppercase">
            {{ t('settings.advanced.server') }}
          </p>
          <div class="grid gap-3 sm:grid-cols-2">
            <label class="block">
              <span class="mb-1 block text-[12px] text-muted">{{ t('settings.advanced.port') }}</span>
              <input
                v-model.number="draft.server.port"
                type="number"
                min="1"
                max="65535"
                class="w-full rounded-lg h-10 border border-line bg-elevated px-3 text-sm text-ink outline-none focus:border-accent"
              />
            </label>
            <label class="block">
              <span class="mb-1 block text-[12px] text-muted">{{ t('settings.advanced.baseUrl') }}</span>
              <input
                v-model="draft.server.proxy.base_url"
                class="w-full rounded-lg h-10 border border-line bg-elevated px-3 text-sm text-ink outline-none focus:border-accent"
                :placeholder="t('settings.advanced.baseUrlPlaceholder')"
              />
            </label>
          </div>
          <label class="mt-3 flex items-center gap-2 text-xs text-muted">
            <input v-model="draft.server.proxy.trust_proxy_headers" type="checkbox" />
            {{ t('settings.advanced.trustProxy') }}
          </label>
          <p class="mt-2 text-[12px] text-muted">{{ t('settings.advanced.portHint') }}</p>
        </div>

        <p class="rounded-lg border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-[12px] fg-warn">
          {{ t('settings.advanced.protected') }}
        </p>

        <div class="rounded-xl border border-line p-6">
          <div class="mb-3 flex items-center justify-between">
            <p class="text-xs font-semibold tracking-wider text-muted uppercase">
              {{ t('settings.advanced.mounts') }}
            </p>
            <button class="text-[12px] text-accent hover:underline" @click="addMount">
              + {{ t('settings.advanced.add') }}
            </button>
          </div>
          <p v-if="!draft.system.mounts.length" class="text-[12px] text-muted">
            {{ t('settings.advanced.mountsHint') }}
          </p>
          <div v-for="(mount, index) in draft.system.mounts" :key="index" class="mb-2 flex gap-2">
            <input
              v-model="mount.name"
              :placeholder="t('settings.advanced.mountName')"
              class="w-32 rounded-lg border border-line bg-elevated px-2 py-1.5 text-xs text-ink outline-none focus:border-accent"
            />
            <input
              v-model="mount.path"
              :placeholder="t('settings.advanced.mountPath')"
              class="min-w-0 flex-1 rounded-lg border border-line bg-elevated px-2 py-1.5 font-mono text-xs text-ink outline-none focus:border-accent"
            />
            <button class="text-muted hover-danger" @click="removeMount(index)">
              <AppIcon name="close" :size="14" />
            </button>
          </div>
        </div>

        <div class="rounded-xl border border-line p-6">
          <div class="mb-3 flex items-center justify-between">
            <p class="text-xs font-semibold tracking-wider text-muted uppercase">
              {{ t('settings.advanced.services') }}
            </p>
            <button class="text-[12px] text-accent hover:underline" @click="addService">
              + {{ t('settings.advanced.add') }}
            </button>
          </div>
          <p v-if="!draft.services.length" class="text-[12px] text-muted">
            {{ t('settings.advanced.servicesHint') }}
          </p>
          <div
            v-for="(service, index) in draft.services"
            :key="index"
            class="mb-3 space-y-2 rounded-lg border border-line/60 p-3"
          >
            <div class="flex gap-2">
              <input
                v-model="service.id"
                :placeholder="t('settings.advanced.serviceId')"
                class="w-28 rounded-lg border border-line bg-elevated px-2 py-1.5 font-mono text-xs text-ink outline-none focus:border-accent"
              />
              <input
                v-model="service.name"
                :placeholder="t('settings.advanced.serviceName')"
                class="w-32 rounded-lg border border-line bg-elevated px-2 py-1.5 text-xs text-ink outline-none focus:border-accent"
              />
              <input
                v-model="service.target_url"
                placeholder="http://192.168.1.50:3000"
                class="min-w-0 flex-1 rounded-lg border border-line bg-elevated px-2 py-1.5 font-mono text-xs text-ink outline-none focus:border-accent"
              />
              <button class="text-muted hover-danger" @click="removeService(index)">
                <AppIcon name="close" :size="14" />
              </button>
            </div>
            <div class="flex flex-wrap items-center gap-3 text-[12px] text-muted">
              <label class="flex items-center gap-1">
                <input v-model="service.strip_prefix" type="checkbox" />
                {{ t('settings.advanced.stripPrefix') }}
              </label>
              <label class="flex items-center gap-1">
                <input v-model="service.rewrite_html" type="checkbox" />
                {{ t('settings.advanced.rewriteHtml') }}
              </label>
              <label class="flex items-center gap-1">
                <input v-model="service.insecure_skip_verify" type="checkbox" />
                {{ t('settings.advanced.insecure') }}
              </label>
              <span class="mx-1 opacity-50">|</span>
              <input
                :value="injectKey(service)"
                :placeholder="t('settings.advanced.injectKey')"
                class="w-36 rounded border border-line bg-elevated px-2 py-1 font-mono text-[12px] text-ink outline-none focus:border-accent"
                @input="setInject(service, ($event.target as HTMLInputElement).value, injectValue(service))"
              />
              <input
                :value="injectValue(service)"
                :placeholder="t('settings.advanced.injectValue')"
                class="w-32 rounded border border-line bg-elevated px-2 py-1 font-mono text-[12px] text-ink outline-none focus:border-accent"
                @input="setInject(service, injectKey(service), ($event.target as HTMLInputElement).value)"
              />
            </div>
          </div>
        </div>

        <p class="text-[12px] text-muted">{{ t('settings.advanced.resetHint') }}</p>
      </div>

      <!-- 源码模式：按视口高度自适应，避免 420px 固定高在桌面端只剩一条缝（8.4.1） -->
      <div v-else class="overflow-hidden rounded-xl border border-line">
        <div
          v-if="rawLoading"
          class="grid h-[min(72vh,760px)] place-items-center text-sm text-muted"
        >
          {{ t('settings.advanced.loadingRaw') }}
        </div>
        <div v-else class="flex h-[min(72vh,760px)] min-h-[420px] flex-col">
          <CodeEditor
            :content="rawText"
            language="toml"
            :dark="isDark"
            class="min-h-0 flex-1"
            @change="onRawChange"
          />
        </div>
      </div>
    </div>

    <!-- ================= 生态推荐（只读清单，设计文档 10） ================= -->
    <div v-else class="space-y-5">
      <p class="text-xs text-muted">{{ t('ecosystem.intro') }}</p>

      <div v-for="category in ECOSYSTEM" :key="category.id" class="space-y-2">
        <p class="text-[12px] font-semibold tracking-wider text-muted uppercase">
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

      <p class="rounded-xl border border-line px-4 py-3 text-[12px] leading-relaxed text-muted">
        {{ t('ecosystem.footer') }}
      </p>
    </div>

    <Transition name="fade">
      <div
        v-if="toast"
        class="pointer-events-none fixed bottom-6 left-1/2 z-50 -translate-x-1/2 rounded-full border border-line bg-panel px-4 py-2 text-xs text-ink shadow-lg"
      >
        {{ toast }}
      </div>
    </Transition>
  </section>
</template>
