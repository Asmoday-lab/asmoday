<script setup lang="ts">
/**
 * 二次验证弹窗（全局单例，挂在 App.vue）。
 *
 * 触发点：终端连接、创建/删除令牌、保存配置。用户输入密码后写入会话的
 * `verified_at`，5 分钟内不再重复询问。
 */
import { ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { useScrollLock } from '../composables/useScrollLock'
import { cancelReauth, reauthBusy, reauthError, reauthOpen, submitReauth } from '../stores/reauth'
import { localizedCode } from '../lib/errors'

const { t } = useI18n()
const password = ref('')

useScrollLock(reauthOpen)

watch(reauthOpen, (open) => {
  if (open) password.value = ''
})

function submit() {
  if (!password.value) return
  void submitReauth(password.value)
}

const errorText = () => (reauthError.value ? localizedCode(reauthError.value) ?? t('common.reauth.failed') : '')
</script>

<template>
  <Teleport to="body">
    <div
      v-if="reauthOpen"
      class="fixed inset-0 z-[90] grid place-items-center bg-black/50 p-4"
      data-testid="reauth-dialog"
      @click.self="cancelReauth()"
    >
      <div class="w-full max-w-sm rounded-2xl border border-line bg-panel p-5 shadow-xl">
        <p class="text-sm font-medium text-ink">{{ t('common.reauth.title') }}</p>
        <p class="mt-1 text-[12px] leading-relaxed text-muted">{{ t('common.reauth.hint') }}</p>

        <input
          v-model="password"
          type="password"
          autocomplete="current-password"
          :placeholder="t('common.reauth.placeholder')"
          class="mt-4 w-full rounded-lg border border-line bg-elevated px-3 py-2 text-xs text-ink outline-none focus:border-accent"
          @keyup.enter="submit"
        />

        <p v-if="reauthError" class="mt-3 text-xs fg-danger">{{ errorText() }}</p>

        <div class="mt-5 flex justify-end gap-2">
          <button
            class="h-9 rounded-md border border-line px-4 text-xs text-muted hover:text-ink"
            @click="cancelReauth()"
          >
            {{ t('common.action.cancel') }}
          </button>
          <button
            class="h-9 rounded-md bg-accent px-4 text-xs text-white disabled:opacity-50"
            :disabled="reauthBusy || !password"
            @click="submit"
          >
            {{ reauthBusy ? t('common.reauth.checking') : t('common.action.confirm') }}
          </button>
        </div>
      </div>
    </div>
  </Teleport>
</template>
