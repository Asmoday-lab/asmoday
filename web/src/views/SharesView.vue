<script setup lang="ts">
/**
 * 分享与图床：文件右键「分享…」/「复制图床直链」创建的链接集中管理。
 * 撤销后链接立即失效；防盗链在 config.toml 的 [shares].referer_allowlist。
 */
import { onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import AppIcon from '../components/AppIcon.vue'
import { readApiError } from '../lib/errors'

interface ShareRow {
  token: string
  path: string
  is_folder: boolean
  has_password: boolean
  allow_download: boolean
  expires_at: number | null
  created_at: number | null
}

const { t } = useI18n()
const shares = ref<ShareRow[]>([])
const sharesError = ref('')
const toast = ref('')

function showToast(message: string) {
  toast.value = message
  window.setTimeout(() => {
    if (toast.value === message) toast.value = ''
  }, 2400)
}

function formatExpiry(value: number | null) {
  if (!value) return t('common.state.permanent')
  return new Date(value * 1000).toLocaleDateString()
}

async function loadShares() {
  sharesError.value = ''
  try {
    const res = await fetch('/api/shares')
    if (!res.ok) throw new Error(await readApiError(res))
    shares.value = ((await res.json()) as { items: ShareRow[] }).items
  } catch (err) {
    sharesError.value = err instanceof Error ? err.message : t('shares.list.loadFailed')
  }
}

async function revokeShare(token: string) {
  try {
    const res = await fetch(`/api/shares/${encodeURIComponent(token)}`, { method: 'DELETE' })
    if (!res.ok) throw new Error(await readApiError(res))
    await loadShares()
    showToast(t('shares.list.revoked'))
  } catch (err) {
    sharesError.value = err instanceof Error ? err.message : t('shares.list.revokeFailed')
  }
}

function shareUrl(token: string): string {
  return `${window.location.origin}/s/${token}`
}

async function copyShareLink(token: string) {
  try {
    await navigator.clipboard.writeText(shareUrl(token))
    showToast(t('common.clipboard.linkCopied'))
  } catch {
    showToast(t('common.clipboard.denied'))
  }
}

onMounted(loadShares)
</script>

<template>
  <section class="mx-auto w-full max-w-4xl px-4 py-6 md:px-8 md:py-10">
    <div class="rounded-xl border border-line p-4 text-[12px] leading-relaxed text-muted">
      {{ t('shares.list.intro') }}
    </div>

    <div class="mt-4 overflow-hidden rounded-xl border border-line">
      <div
        v-for="(share, index) in shares"
        :key="share.token"
        class="flex items-center gap-3 px-4 py-2.5 text-xs"
        :class="index % 2 === 1 ? 'bg-elevated/40' : ''"
      >
        <span class="shrink-0 text-muted">
          <AppIcon :name="share.is_folder ? 'folder' : 'file'" :size="14" />
        </span>
        <span class="min-w-0 flex-1 truncate text-ink" :title="share.path">{{ share.path }}</span>
        <span v-if="share.has_password" class="shrink-0 rounded bg-elevated px-1.5 py-0.5 text-[11px] text-muted">
          {{ t('shares.list.password') }}
        </span>
        <span v-if="!share.allow_download && !share.is_folder" class="shrink-0 rounded bg-elevated px-1.5 py-0.5 text-[11px] text-muted">
          {{ t('shares.list.noDownload') }}
        </span>
        <span class="hidden w-20 shrink-0 text-right text-[12px] text-muted sm:block">
          {{ formatExpiry(share.expires_at) }}
        </span>
        <button
          class="shrink-0 rounded-md p-1.5 text-muted transition-colors hover:bg-elevated hover:text-ink"
          :title="t('shares.list.copy')"
          @click="copyShareLink(share.token)"
        >
          <AppIcon name="link" :size="13" />
        </button>
        <button class="shrink-0 text-muted hover-danger"  :title="t('shares.list.revoke')" @click="revokeShare(share.token)">
          <AppIcon name="close" :size="14" />
        </button>
      </div>
      <p v-if="!shares.length" class="px-4 py-3 text-[12px] text-muted">
        {{ t('shares.list.empty') }}
      </p>
    </div>

    <p
      v-if="sharesError"
      class="mt-4 rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs fg-danger"
    >
      {{ sharesError }}
    </p>

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
