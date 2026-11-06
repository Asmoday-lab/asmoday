<script setup lang="ts">
/**
 * 登录 / 初始化一体页。整页独立布局，不套侧边栏。
 *
 * 单用户后没有用户名可言（v4.9.32）：
 * - 待初始化（needs_setup）：这里就是初始化页——输入控制台首启打印的
 *   一次性密码 + 新密码，一步建权并登录（v4.9.33）；
 * - 已初始化：普通登录页，只剩密码框（2FA 可选展开）。
 */
import { onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useRoute, useRouter } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import PasswordInput from '../components/PasswordInput.vue'
import { authApi } from '../api/auth'
import { useTheme } from '../composables/useTheme'
import { SUPPORTED_LOCALES, type Locale } from '../locales'
import { applyLocale } from '../lib/i18n'

const { isDark, toggle } = useTheme()
const { t, locale } = useI18n()
const router = useRouter()
const route = useRoute()

const mode = ref<'loading' | 'setup' | 'login'>('loading')
const oneTimePassword = ref('')
const password = ref('')
const totp = ref('')
const showTotp = ref(false)
const error = ref('')
const submitting = ref(false)

async function submit() {
  if (submitting.value) return
  error.value = ''
  if (mode.value === 'setup') {
    if (!oneTimePassword.value) {
      error.value = t('auth.setup.oneTimeRequired')
      return
    }
    if (password.value.length < 8) {
      error.value = t('auth.setup.tooShort')
      return
    }
  } else if (!password.value) {
    error.value = t('auth.login.required')
    return
  }
  submitting.value = true
  try {
    if (mode.value === 'setup') {
      await authApi.setup(oneTimePassword.value, password.value)
    } else {
      await authApi.login(password.value, totp.value.trim())
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
  mode.value = authApi.status.value.needs_setup ? 'setup' : 'login'
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
          class="mb-3 grid size-12 place-items-center rounded-2xl bg-[linear-gradient(135deg,#818cf8,#4f46e5)] text-white shadow-md"
        >
          <AppIcon name="sparkle" :size="48" :stroke-width="1.75" />
        </span>
        <h1 class="text-xl font-semibold text-ink">Asmoday</h1>
        <p class="mt-1 text-xs text-muted">
          {{ mode === 'setup' ? t('auth.setup.tagline') : t('auth.login.tagline') }}
        </p>
      </div>

      <form class="glass-card space-y-3 rounded-2xl p-5" @submit.prevent="submit">
        <p v-if="mode === 'setup'" class="text-[12px] leading-relaxed text-muted">
          {{ t('auth.setup.desc') }}
        </p>

        <label v-if="mode === 'setup'" class="block">
          <span class="mb-1 block text-[12px] text-muted">{{ t('auth.setup.oneTime') }}</span>
          <input
            v-model="oneTimePassword"
            type="text"
            autocomplete="one-time-code"
            class="w-full rounded-lg border border-line bg-elevated px-3 py-2 text-sm text-ink outline-none focus:border-accent"
            :placeholder="t('auth.setup.oneTimePlaceholder')"
          />
        </label>

        <label class="block">
          <span class="mb-1 block text-[12px] text-muted">
            {{ mode === 'setup' ? t('auth.setup.password') : t('auth.login.password') }}
          </span>
          <PasswordInput
            v-model="password"
            :autocomplete="mode === 'setup' ? 'new-password' : 'current-password'"
            class="w-full rounded-lg border border-line bg-elevated px-3 py-2 text-sm text-ink outline-none focus:border-accent"
            placeholder="••••••••"
          />
        </label>

        <button
          v-if="mode === 'login'"
          type="button"
          class="text-[12px] text-muted transition-colors hover:text-ink"
          @click="showTotp = !showTotp"
        >
          {{ showTotp ? t('auth.login.totpHide') : t('auth.login.totpToggle') }}
        </button>
        <label v-if="mode === 'login' && showTotp" class="block">
          <span class="mb-1 block text-[12px] text-muted">{{ t('auth.login.totpLabel') }}</span>
          <input
            v-model="totp"
            type="text"
            autocomplete="one-time-code"
            class="w-full rounded-lg border border-line bg-elevated px-3 py-2 text-sm tracking-[0.2em] text-ink outline-none focus:border-accent"
            placeholder="000000 / XXXXX-XXXXX"
          />
        </label>

        <p
          v-if="error"
          class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs fg-danger"
        >
          {{ error }}
        </p>

        <button
          type="submit"
          :disabled="submitting"
          class="w-full rounded-lg bg-accent py-2.5 text-sm font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-40"
        >
          {{
            submitting
              ? t('auth.login.submitting')
              : mode === 'setup'
                ? t('auth.setup.submit')
                : t('auth.login.submit')
          }}
        </button>

        <p class="pt-1 text-center text-[12px] leading-relaxed text-muted">
          {{ mode === 'setup' ? t('auth.setup.footer') : t('auth.login.footerLogin') }}
        </p>
      </form>
    </div>
  </div>
</template>
