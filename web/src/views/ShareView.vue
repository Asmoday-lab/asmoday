<script setup lang="ts">
/**
 * 公开分享页（/s/:token，无需登录）：口令校验、文件内联预览、
 * 目录浏览与子文件下载。走 /api/public/share/* 直链接口。
 */
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useRoute } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import { sharesApi, type PublicShareInfo, type ShareEntry } from '../api/shares'
import { ApiError } from '../lib/errors'

const route = useRoute()
const { t } = useI18n()
const token = computed(() => String(route.params.token ?? ''))

const info = ref<PublicShareInfo | null>(null)
const password = ref('')
const needPassword = ref(false)
const errorMsg = ref('')
const loading = ref(true)

// 目录浏览
const currentSub = ref('')
const entries = ref<ShareEntry[]>([])
const entriesLoading = ref(false)
const browsingName = ref('')

// 文件预览
const previewToken = ref(0)

function withPassword(extra: Record<string, string> = {}): string {
  const params = new URLSearchParams(extra)
  if (password.value) params.set('password', password.value)
  return params.toString()
}

function rawUrlOf(sub?: string, download = false): string {
  const suffix = download ? 'download' : 'raw'
  const query = withPassword(sub ? { sub } : {})
  return `/api/public/share/${encodeURIComponent(token.value)}/${suffix}${query ? `?${query}` : ''}`
}

const previewUrl = computed(() => (info.value && !info.value.is_folder ? rawUrlOf() : ''))
const previewKind = computed(() => {
  const name = info.value?.name ?? ''
  const ext = name.split('.').pop()?.toLowerCase() ?? ''
  if (['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp', 'svg'].includes(ext)) return 'image'
  if (['mp4', 'webm', 'mov', 'mkv', 'm4v'].includes(ext)) return 'video'
  if (['mp3', 'flac', 'm4a', 'wav', 'ogg', 'aac'].includes(ext)) return 'audio'
  if (['mp3', 'flac'].includes(ext)) return 'audio'
  return 'file'
})

async function boot(withPw?: string) {
  loading.value = true
  errorMsg.value = ''
  try {
    if (withPw !== undefined) password.value = withPw
    info.value = await sharesApi.info(token.value, password.value || undefined)
    needPassword.value = false
    if (info.value.is_folder) await openFolder('')
  } catch (err) {
    // 口令保护走错误码判定（设计文档 3.8），不再靠中文文案匹配
    const needsPassword =
      (err instanceof ApiError && err.code === 'SHARE_PASSWORD_REQUIRED') ||
      (err instanceof ApiError && err.code === 'SHARE_PASSWORD_WRONG')
    if (needsPassword) {
      needPassword.value = true
      errorMsg.value =
        err instanceof ApiError && err.code === 'SHARE_PASSWORD_WRONG'
          ? t('errors.SHARE_PASSWORD_WRONG')
          : ''
    } else {
      errorMsg.value = err instanceof Error ? err.message : t('shares.public.loadFailed')
      info.value = null
    }
  } finally {
    loading.value = false
  }
}

async function openFolder(sub: string) {
  entriesLoading.value = true
  try {
    const listing = await sharesApi.listEntries(token.value, sub, password.value || undefined)
    entries.value = [...listing.entries].sort((a, b) =>
      a.is_dir === b.is_dir ? a.name.localeCompare(b.name) : a.is_dir ? -1 : 1,
    )
    currentSub.value = listing.sub
    const segments = listing.sub ? listing.sub.split('/').filter(Boolean) : []
    browsingName.value = segments.length
      ? segments[segments.length - 1]
      : (info.value?.name ?? t('shares.public.shareDir'))
  } catch (err) {
    errorMsg.value = err instanceof Error ? err.message : t('shares.public.dirFailed')
  } finally {
    entriesLoading.value = false
  }
}

function openEntry(entry: ShareEntry) {
  if (entry.is_dir) void openFolder(entry.sub)
}

function parentSub(): string {
  const segments = currentSub.value.split('/').filter(Boolean)
  segments.pop()
  return segments.join('/')
}

function fmtSize(size: number): string {
  if (size >= 1024 * 1024 * 1024) return `${(size / 1024 / 1024 / 1024).toFixed(1)} GB`
  if (size >= 1024 * 1024) return `${(size / 1024 / 1024).toFixed(1)} MB`
  if (size >= 1024) return `${(size / 1024).toFixed(1)} KB`
  return `${size} B`
}

function fmtExpiry(timestamp: number | null): string {
  if (!timestamp) return t('common.time.permanent')
  return t('common.time.expiresAt', { time: new Date(timestamp * 1000).toLocaleString() })
}

function iconFor(entry: ShareEntry) {
  if (entry.is_dir) return 'folder'
  if (entry.kind === 'image') return 'gallery'
  if (entry.kind === 'video') return 'video'
  if (entry.kind === 'audio') return 'audio'
  return 'file'
}

onMounted(() => void boot())
</script>

<template>
  <div class="grid min-h-dvh place-items-center bg-canvas px-4 py-10 text-ink">
    <div class="w-full max-w-lg">
      <!-- 加载中 -->
      <div v-if="loading" class="space-y-3 rounded-2xl border border-line bg-panel p-6">
        <div class="skeleton h-5 w-1/2 rounded" />
        <div class="skeleton h-3 w-1/3 rounded" />
        <div class="skeleton h-10 w-full rounded-lg" />
      </div>

      <!-- 错误 / 过期 / 需要口令 -->
      <div v-else-if="!info" class="rounded-2xl border border-line bg-panel p-8 text-center">
        <span class="mx-auto mb-3 grid size-12 place-items-center rounded-full bg-elevated text-muted">
          <AppIcon :name="needPassword ? 'lock' : 'alert'" :size="22" />
        </span>
        <p class="text-sm font-medium">
          {{ needPassword ? t('shares.public.protected') : t('shares.public.missing') }}
        </p>
        <p v-if="errorMsg" class="mt-1 text-xs text-muted">{{ errorMsg }}</p>

        <form
          v-if="needPassword"
          class="mx-auto mt-4 flex max-w-xs items-center gap-2"
          @submit.prevent="boot(password)"
        >
          <input
            v-model="password"
            type="password"
            :placeholder="t('shares.public.passwordPlaceholder')"
            autofocus
            class="min-w-0 flex-1 rounded-lg border border-line bg-elevated px-3 py-2 text-xs outline-none focus:border-accent"
          />
          <button
            type="submit"
            class="shrink-0 rounded-lg bg-accent px-3 py-2 text-xs font-medium text-white hover:opacity-90"
          >
            {{ t('shares.public.unlock') }}
          </button>
        </form>
      </div>

      <!-- 分享内容 -->
      <div v-else class="overflow-hidden rounded-2xl border border-line bg-panel">
        <header class="border-b border-line/60 px-5 py-4">
          <div class="flex items-center gap-2.5">
            <span class="grid size-9 shrink-0 place-items-center rounded-lg bg-accent-soft text-accent">
              <AppIcon :name="info.is_folder ? 'folderOpen' : 'file'" :size="17" />
            </span>
            <div class="min-w-0">
              <p class="truncate text-sm font-medium">{{ info.name }}</p>
              <p class="text-[11px] text-muted">
                {{ fmtExpiry(info.expires_at) }}
                <span v-if="info.has_password"> · {{ t('shares.public.passwordProtected') }}</span>
                <span v-if="!info.allow_download && !info.is_folder">
                  · {{ t('shares.public.noDownload') }}
                </span>
              </p>
            </div>
          </div>
        </header>

        <!-- 单文件：内联预览 + 下载 -->
        <div v-if="!info.is_folder" class="p-5">
          <div
            v-if="previewKind !== 'file'"
            class="mb-4 overflow-hidden rounded-xl border border-line bg-black"
          >
            <img
              v-if="previewKind === 'image'"
              :key="previewToken"
              :src="previewUrl"
              class="max-h-[60vh] w-full object-contain"
              alt=""
            />
            <video
              v-else-if="previewKind === 'video'"
              :key="previewToken"
              :src="previewUrl"
              controls
              playsinline
              class="max-h-[60vh] w-full"
            />
            <audio v-else :key="previewToken" :src="previewUrl" controls class="w-full p-3" />
          </div>
          <a
            v-if="info.allow_download"
            :href="rawUrlOf(undefined, true)"
            class="flex items-center justify-center gap-2 rounded-xl bg-accent px-4 py-2.5 text-sm font-medium text-white transition-opacity hover:opacity-90"
          >
            <AppIcon name="archive" :size="15" />
            {{ t('shares.public.download') }}
          </a>
          <p v-else class="text-center text-xs text-muted">{{ t('shares.public.previewOnly') }}</p>
        </div>

        <!-- 目录：列表浏览 -->
        <div v-else>
          <div class="flex items-center gap-2 border-b border-line/60 px-4 py-2.5 text-xs">
            <button
              v-if="currentSub"
              class="flex items-center gap-1 rounded-md px-2 py-1 text-muted transition-colors hover:bg-elevated hover:text-ink"
              @click="openFolder(parentSub())"
            >
              <AppIcon name="arrowLeft" :size="13" />
              {{ t('shares.public.parent') }}
            </button>
            <span class="min-w-0 flex-1 truncate text-muted" :title="currentSub || info.name">
              {{ browsingName }}
            </span>
            <span class="shrink-0 text-muted">
              {{ t('shares.public.count', { count: entries.length }) }}
            </span>
          </div>

          <div class="max-h-[55vh] overflow-y-auto">
            <p v-if="entriesLoading" class="px-5 py-6 text-center text-xs text-muted">
              {{ t('common.action.loading') }}
            </p>
            <p v-else-if="entries.length === 0" class="px-5 py-6 text-center text-xs text-muted">
              {{ t('shares.public.emptyDir') }}
            </p>
            <ul v-else class="divide-y divide-line/50">
              <li
                v-for="entry in entries"
                :key="entry.sub"
                class="flex items-center gap-2.5 px-4 py-2.5"
                :class="entry.is_dir ? 'cursor-pointer hover:bg-elevated/60' : ''"
                @click="openEntry(entry)"
              >
                <span class="shrink-0 text-muted">
                  <AppIcon :name="iconFor(entry)" :size="16" />
                </span>
                <span class="min-w-0 flex-1 truncate text-[13px]">{{ entry.name }}</span>
                <span class="shrink-0 text-[11px] text-muted">
                  {{ entry.is_dir ? '' : fmtSize(entry.size) }}
                </span>
                <a
                  v-if="!entry.is_dir && info.allow_download"
                  :href="rawUrlOf(entry.sub, true)"
                  class="shrink-0 rounded-md p-1.5 text-muted transition-colors hover:bg-elevated hover:text-ink"
                  :title="t('common.action.download')"
                  :data-sub="entry.sub"
                  @click.stop
                >
                  <AppIcon name="archive" :size="14" />
                </a>
              </li>
            </ul>
          </div>
        </div>

        <footer class="border-t border-line/60 px-5 py-3 text-center text-[10px] text-muted">
          {{ t('shares.public.footer') }}
        </footer>
      </div>
    </div>
  </div>
</template>
