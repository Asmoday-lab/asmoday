<script setup lang="ts">
/**
 * 全局分享弹窗（设计文档 3.2.5 / 8.4.13）。
 *
 * 单例：只在 App.vue 挂一次，文件右键、照片灯箱、影视库、音乐库都通过
 * `stores/share.ts` 的 `openShare()` 唤起，避免每个页面各写一套分享表单。
 */
import { computed, reactive, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { sharesApi } from '../api/shares'
import { closeShare, shareTarget } from '../stores/share'
import { useScrollLock } from '../composables/useScrollLock'

const { t } = useI18n()

const busy = ref(false)
const error = ref('')
const result = ref<{ url: string; token: string } | null>(null)
const copied = ref(false)

const form = reactive({ password: '', days: 7, allowDownload: true })

const isOpen = computed(() => shareTarget.value !== null)
useScrollLock(isOpen)

// 每次重新打开都是干净表单（默认 7 天、允许下载）
watch(shareTarget, (target) => {
  if (!target) return
  form.password = ''
  form.days = 7
  form.allowDownload = true
  result.value = null
  error.value = ''
  copied.value = false
  busy.value = false
})

async function submit() {
  const target = shareTarget.value
  if (!target || busy.value) return
  busy.value = true
  error.value = ''
  try {
    const created = await sharesApi.create(target.path, {
      password: form.password || undefined,
      days: form.days,
      allow_download: form.allowDownload,
    })
    result.value = { url: `${location.origin}${created.url}`, token: created.token }
  } catch (err) {
    error.value = err instanceof Error ? err.message : t('files.share.failed')
  } finally {
    busy.value = false
  }
}

async function copyLink() {
  if (!result.value) return
  try {
    await navigator.clipboard.writeText(result.value.url)
    copied.value = true
  } catch {
    error.value = t('common.clipboard.denied')
  }
}
</script>

<template>
  <Teleport to="body">
    <div
      v-if="shareTarget"
      class="fixed inset-0 z-[85] grid place-items-center bg-black/50 p-4"
      data-testid="share-dialog"
      @click.self="closeShare()"
    >
      <div class="w-full max-w-sm rounded-2xl border border-line bg-panel p-5 shadow-xl">
        <template v-if="!result">
          <p class="text-sm font-medium text-ink">{{ t('files.share.create') }}</p>
          <p class="mt-0.5 truncate text-[12px] text-muted">{{ shareTarget.name }}</p>

          <label class="mt-4 block text-[12px] text-muted">
            {{ t('files.share.password') }}
            <input
              v-model="form.password"
              type="text"
              :placeholder="t('files.share.passwordPlaceholder')"
              class="mt-1 w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
            />
          </label>

          <label class="mt-3 block text-[12px] text-muted">
            {{ t('files.share.expiry') }}
            <select
              v-model.number="form.days"
              class="mt-1 w-full rounded-lg border border-line bg-elevated px-2 py-2 text-xs text-ink outline-none focus:border-accent"
            >
              <option :value="1">{{ t('files.share.days1') }}</option>
              <option :value="7">{{ t('files.share.days7') }}</option>
              <option :value="30">{{ t('files.share.days30') }}</option>
              <option :value="0">{{ t('files.share.permanent') }}</option>
            </select>
          </label>

          <label class="mt-3 flex items-center gap-2 text-xs text-ink">
            <input v-model="form.allowDownload" type="checkbox" class="accent-[var(--accent)]" />
            {{ t('files.share.allowDownload') }}
          </label>

          <p v-if="error" class="mt-3 text-xs fg-danger">{{ error }}</p>

          <div class="mt-5 flex justify-end gap-2">
            <button
              class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
              @click="closeShare()"
            >
              {{ t('common.action.cancel') }}
            </button>
            <button
              class="rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50"
              :disabled="busy"
              data-testid="share-submit"
              @click="submit"
            >
              {{ busy ? t('common.action.creating') : t('files.share.submit') }}
            </button>
          </div>
        </template>

        <template v-else>
          <p class="text-sm font-medium text-ink">{{ t('files.share.created') }}</p>
          <div
            class="mt-3 break-all rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink"
          >
            {{ result.url }}
          </div>
          <p class="mt-2 text-[12px] text-muted">{{ t('files.share.hint') }}</p>
          <div class="mt-4 flex justify-end gap-2">
            <button
              class="rounded-lg border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink"
              @click="closeShare()"
            >
              {{ t('common.action.close') }}
            </button>
            <button
              class="rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90"
              @click="copyLink"
            >
              {{ copied ? t('common.action.copied') : t('files.share.copyLink') }}
            </button>
          </div>
        </template>
      </div>
    </div>
  </Teleport>
</template>
