<script setup lang="ts">
/**
 * 登录 / 首次初始化。整页独立布局，不套侧边栏。
 */
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useRoute, useRouter } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import { authApi } from '../api/auth'
import { useTheme } from '../composables/useTheme'
import { SUPPORTED_LOCALES, type Locale } from '../locales'
import { applyLocale } from '../lib/i18n'

const { isDark, toggle } = useTheme()
const { t, locale } = useI18n()
const router = useRouter()
const route = useRoute()

const username = ref('')
const password = ref('')
const confirm = ref('')
const totp = ref('')
const showTotp = ref(false)
const error = ref('')
const submitting = ref(false)

const isSetup = computed(() => !authApi.initialized.value)

const canSubmit = computed(() => {
  if (isSetup.value) {
    return (
      username.value.trim().length >= 2 &&
      password.value.length >= 8 &&
      password.value === confirm.value &&
      !submitting.value
    )
  }
  return username.value.trim().length > 0 && password.value.length > 0 && !submitting.value
})

async function submit() {
  if (!canSubmit.value) return
  error.value = ''
  submitting.value = true
  try {
    if (isSetup.value) {
      await authApi.setup(username.value.trim(), password.value)
    } else {
      await authApi.login(username.value.trim(), password.value, totp.value.trim())
    }
    const target = typeof route.query.redirect === 'string' ? route.query.redirect : '/'
    await router.replace(target)
  } catch (err) {
    error.value = err instanceof Error ? err.message : t('auth.login.failed')
  } finally {
    submitting.value = false
  }
}

onMounted(async () => {
  await authApi.ensureAuth()
  if (!isSetup.value) username.value = 'admin'
})

/** 语言切换：登录前也能改（首次运行可能整站是英文环境） */
function changeLocale(event: Event) {
  const value = (event.target as HTMLSelectElement).value as Locale
  applyLocale(value)
}
</script>

<template>
  <div class="relative flex min-h-dvh items-center justify-center bg-canvas px-4 py-10">
    <div
      class="pointer-events-none absolute top-0 left-1/2 size-80 -translate-x-1/2 rounded-full bg-accent/20 blur-[120px]"
    />

    <div class="absolute top-4 right-4 flex items-center gap-2">
      <select
        :value="locale"
        class="rounded-lg border border-line bg-elevated px-2 py-1.5 text-[12px] text-muted outline-none focus:border-accent"
        :aria-label="t('common.language.label')"
        @change="changeLocale"
      >
        <option v-for="item in SUPPORTED_LOCALES" :key="item" :value="item">
          {{ t(`common.language.${item}`) }}
        </option>
      </select>
      <button
        class="grid size-9 place-items-center rounded-lg border border-line text-muted transition-colors hover:text-ink"
        :title="isDark ? t('common.theme.toLight') : t('common.theme.toDark')"
        @click="toggle"
      >
        <AppIcon :name="isDark ? 'moon' : 'sun'" :size="15" />
      </button>
    </div>

    <div class="relative w-full max-w-sm">
      <div class="mb-6 flex flex-col items-center text-center">
        <span
          class="mb-3 grid size-12 place-items-center rounded-2xl bg-accent text-white shadow-[0_0_24px_var(--accent)]"
        >
          <AppIcon name="sparkle" :size="22" :stroke-width="1.8" />
        </span>
        <h1 class="text-xl font-semibold text-ink">Asmoday</h1>
        <p class="mt-1 text-xs text-muted">
          {{ isSetup ? t('auth.login.setupTagline') : t('auth.login.tagline') }}
        </p>
      </div>

      <form class="glass-card space-y-3 rounded-2xl p-5" @submit.prevent="submit">
        <label class="block">
          <span class="mb-1 block text-[12px] text-muted">{{ t('auth.login.username') }}</span>
          <input
            v-model="username"
            type="text"
            autocomplete="username"
            class="w-full rounded-lg border border-line bg-elevated px-3 py-2 text-sm text-ink outline-none focus:border-accent"
            placeholder="admin"
          />
        </label>

        <label class="block">
          <span class="mb-1 block text-[12px] text-muted">{{ t('auth.login.password') }}</span>
          <input
            v-model="password"
            type="password"
            :autocomplete="isSetup ? 'new-password' : 'current-password'"
            class="w-full rounded-lg border border-line bg-elevated px-3 py-2 text-sm text-ink outline-none focus:border-accent"
            :placeholder="isSetup ? t('auth.login.passwordPlaceholder') : '••••••••'"
          />
        </label>

        <label v-if="isSetup" class="block">
          <span class="mb-1 block text-[12px] text-muted">{{ t('auth.login.confirmPassword') }}</span>
          <input
            v-model="confirm"
            type="password"
            autocomplete="new-password"
            class="w-full rounded-lg border border-line bg-elevated px-3 py-2 text-sm text-ink outline-none focus:border-accent"
            :placeholder="t('auth.login.confirmPlaceholder')"
          />
        </label>

        <template v-if="!isSetup">
          <button
            type="button"
            class="text-[12px] text-muted transition-colors hover:text-ink"
            @click="showTotp = !showTotp"
          >
            {{ showTotp ? t('auth.login.totpHide') : t('auth.login.totpToggle') }}
          </button>
          <label v-if="showTotp" class="block">
            <span class="mb-1 block text-[12px] text-muted">{{ t('auth.login.totpLabel') }}</span>
            <input
              v-model="totp"
              type="text"
              autocomplete="one-time-code"
              class="w-full rounded-lg border border-line bg-elevated px-3 py-2 text-sm tracking-[0.2em] text-ink outline-none focus:border-accent"
              placeholder="000000 / XXXXX-XXXXX"
            />
          </label>
        </template>

        <p
          v-if="error"
          class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs fg-danger"
        >
          {{ error }}
        </p>

        <button
          type="submit"
          :disabled="!canSubmit"
          class="w-full rounded-lg bg-accent py-2.5 text-sm font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-40"
        >
          {{ submitting ? t('auth.login.submitting') : isSetup ? t('auth.login.submitSetup') : t('auth.login.submit') }}
        </button>

        <p class="pt-1 text-center text-[12px] leading-relaxed text-muted">
          {{ isSetup ? t('auth.login.footerSetup') : t('auth.login.footerLogin') }}
        </p>
      </form>
    </div>
  </div>
</template>
